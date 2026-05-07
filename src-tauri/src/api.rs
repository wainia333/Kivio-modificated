//! HTTP 客户端、provider 凭据解析、retry / failover、OpenAI / OpenAI 兼容模型调用与 SSE 流。
//!
//! 本模块对外暴露：
//! - `ProviderConnectionInput` / `resolve_provider_credentials` —— 来自前端的 provider 临时配置或 settings.json 的解析。
//! - `build_http_client` —— 60s 超时的 reqwest Client 构造。
//! - `effective_retry_attempts` —— 把 settings.retry_enabled + retry_attempts 折成实际尝试次数。
//! - `extract_status_code` / `is_failover_error` —— failover 判定（仅 401/402/403/429）。
//! - `send_with_retry` —— 网络抖动 / 5xx / 429 退避重试。
//! - `send_with_failover` —— 在 api_keys 列表上轮换。
//! - `call_openai_text` / `call_openai_ocr` / `call_vision_api` —— 文本、OCR、视觉三类调用。
//! - `call_baidu_ocr` / `call_chaoxing_ocr` / `call_baidu_translate` —— OCR 与翻译接口调用。
//! - `call_google_translate` / `call_bing_translate` / `call_bing2_translate`
//!   / `call_yandex_translate` / `call_microsoft_translate` —— 无密钥在线翻译接口调用。
//! - `call_tencent_translate` / `call_caiyun2_translate` —— 腾讯云与彩云小译密钥接口调用。
//! - `stream_chat_call` / `stream_vision_response` —— SSE 流解析。

use std::{
    collections::HashSet,
    fs,
    future::Future,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use base64::{engine::general_purpose, Engine as _};
use chrono::{TimeZone, Utc};
use hmac::{Hmac, Mac};
use reqwest::{
    header::{HeaderMap, ACCEPT, ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_TYPE},
    Client, RequestBuilder, StatusCode,
};
use serde::Deserialize;
use sha2::Sha256;
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;

use crate::apple_intelligence::APPLE_INTELLIGENCE_BASE_URL;
use crate::resolve_explain_image_path;
use crate::settings::{
    self, default_system_prompt, no_think_instruction, ExplainMessage, Settings,
};
use crate::state::AppState;

// ===== Provider 凭据 =====

/// 供应商连接输入参数，用于测试连接或获取模型列表时临时传入
/// api_keys 优先；api_key 为兼容旧前端发的单 key 字段（v2.3.x 时的 ProviderConnectionInput）
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConnectionInput {
    pub id: Option<String>,
    pub base_url: String,
    #[serde(default)]
    pub api_keys: Vec<String>,
    #[serde(default)]
    pub api_key: Option<String>,
}

impl ProviderConnectionInput {
    /// 整理出非空 key 列表：优先 api_keys，回退到 api_key。
    pub fn merged_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .api_keys
            .iter()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect();
        if keys.is_empty() {
            if let Some(legacy) = self.api_key.as_deref() {
                let trimmed = legacy.trim().to_string();
                if !trimmed.is_empty() {
                    keys.push(trimmed);
                }
            }
        }
        keys
    }
}

/// 解析供应商的凭据信息（base_url + 多 key 列表）
/// 优先使用传入的 ProviderConnectionInput（如测试连接时），否则从 settings 中查找对应的供应商
pub fn resolve_provider_credentials(
    settings: &Settings,
    provider_id: &str,
    provider: Option<ProviderConnectionInput>,
) -> Result<(String, Vec<String>), String> {
    if let Some(input) = provider {
        let id_matches = input
            .id
            .as_ref()
            .map(|id| id.is_empty() || id == provider_id)
            .unwrap_or(true);

        if id_matches {
            return Ok((input.base_url.clone(), input.merged_keys()));
        }
    }

    let provider = settings
        .get_provider(provider_id)
        .ok_or_else(|| "Provider not found".to_string())?;
    Ok((provider.base_url.clone(), provider.api_keys.clone()))
}

/// 构建 HTTP 客户端，设置 60 秒超时
pub fn build_http_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_else(|err| {
            eprintln!("Failed to build HTTP client: {err}");
            Client::new()
        })
}

// ===== Retry / Failover =====

/// 重试延迟基础值（毫秒）
const RETRY_BASE_DELAY_MS: u64 = 500;
/// 重试延迟最大值（毫秒）
const RETRY_MAX_DELAY_MS: u64 = 10_000;
/// 流式模型调用允许更长时间。Responses + reasoning(high) + web_search 可能在首段最终文本前等待很久。
const STREAM_REQUEST_TIMEOUT_SECS: u64 = 1800;
const STREAM_CANCEL_POLL_MS: u64 = 200;
const STREAM_FALLBACK_CHUNK_CHARS: usize = 24;
const STREAM_FALLBACK_CHUNK_DELAY_MS: u64 = 12;

fn configure_sse_request(request: RequestBuilder) -> RequestBuilder {
    request
        .header(ACCEPT, "text/event-stream")
        .header(ACCEPT_ENCODING, "identity")
        .header(CACHE_CONTROL, "no-cache")
        .timeout(Duration::from_secs(STREAM_REQUEST_TIMEOUT_SECS))
}

fn reqwest_error_details(error: &reqwest::Error) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    let mut depth = 0;

    while let Some(err) = source {
        let detail = err.to_string();
        if !detail.is_empty() && !message.contains(&detail) {
            message.push_str(": ");
            message.push_str(&detail);
        }
        source = err.source();
        depth += 1;
        if depth >= 4 {
            break;
        }
    }

    message
}

/// 获取实际的重试次数
/// 如果重试功能被禁用，则返回 1（即只尝试一次）
pub fn effective_retry_attempts(settings: &Settings) -> usize {
    if settings.retry_enabled {
        settings.retry_attempts as usize
    } else {
        1
    }
}

/// 从响应头中解析 Retry-After 值（秒），转换为毫秒延迟
fn parse_retry_after(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

/// 判断 HTTP 状态码是否可重试
/// 包括 429（限流）和所有服务器错误（5xx）
fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// 判断请求错误是否可重试
/// 包括超时和连接错误
fn is_retryable_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect()
}

/// 计算重试延迟
/// 优先使用服务器返回的 Retry-After 头；否则使用指数退避策略
fn retry_delay_ms(attempt: usize, retry_after: Option<u64>) -> u64 {
    if let Some(seconds) = retry_after {
        return seconds.saturating_mul(1000);
    }

    let delay = RETRY_BASE_DELAY_MS.saturating_mul(2u64.saturating_pow((attempt - 1) as u32));
    delay.min(RETRY_MAX_DELAY_MS)
}

fn parse_leading_status_code(value: &str) -> Option<u16> {
    let end = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    if end == 0 {
        return None;
    }
    value[..end].parse().ok()
}

/// 从 HTTP 错误信息中提取状态码
/// 格式约定：`"{label} Error: {status} - {body}"`，
/// status 形如 `"429 Too Many Requests"`，第一段数字即可
/// 兼容少数防御性分支使用的 `"{label} HTTP {status}: {body}"`。
/// 网络错误（reqwest::Error 路径）格式为 `"{label} Error: <reqwest msg>"`，无前导数字 → 返回 None
pub fn extract_status_code(err_msg: &str) -> Option<u16> {
    if let Some(idx) = err_msg.find(" Error: ") {
        let rest = &err_msg[idx + " Error: ".len()..];
        if let Some(code) = parse_leading_status_code(rest) {
            return Some(code);
        }
    }

    if let Some(idx) = err_msg.find(" HTTP ") {
        let rest = &err_msg[idx + " HTTP ".len()..];
        return parse_leading_status_code(rest);
    }

    None
}

/// 判断错误信息是否触发 key failover
/// 严格按 HTTP 状态码：401/402/403/429 才换 key —— 与 key 直接相关的错误：
/// - 401 鉴权失败（key 被吊销 / 错误）
/// - 402 需要付费（账户欠费）
/// - 403 权限不足 / 被封禁
/// - 429 限流（key 维度配额耗尽）
/// 其它 4xx（如 400 malformed body）属于请求本身问题，换 key 也无济于事 → 不触发
/// 5xx 由 send_with_retry 内部退避重试，不会到这里
/// 网络错误（timeout / connect 失败）非 key 问题，extract_status_code 返回 None → 不触发
pub fn is_failover_error(err_msg: &str) -> bool {
    matches!(extract_status_code(err_msg), Some(401 | 402 | 403 | 429))
}

/// 多 key failover 包装：在 api_keys 列表上依次尝试，遇到 failover-eligible 错误自动切下一 key
/// 内层每次尝试仍走 send_with_retry（处理网络抖动 / 服务端 5xx 等通用重试）
pub async fn send_with_failover<F, Fut>(
    state: &AppState,
    label: &str,
    attempts: usize,
    provider_id: &str,
    api_keys: &[String],
    send: F,
) -> Result<reqwest::Response, String>
where
    F: Fn(&str) -> Fut,
    Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    let total = api_keys.len();
    if total == 0 {
        return Err(format!("{} Error: No API key configured", label));
    }

    let mut tried: HashSet<usize> = HashSet::new();
    let mut last_err: Option<String> = None;

    while tried.len() < total {
        let idx = match state.pick_active_key(provider_id, total, &tried) {
            Some(i) => i,
            None => break,
        };
        tried.insert(idx);
        let key = api_keys[idx].as_str();

        match send_with_retry(label, attempts, || send(key)).await {
            Ok(resp) => {
                state.mark_key_ok(provider_id, idx);
                return Ok(resp);
            }
            Err(err_msg) => {
                if is_failover_error(&err_msg) && tried.len() < total {
                    state.mark_key_failed(provider_id, idx);
                    eprintln!(
                        "[failover] {} key #{}/{} failed, switching to next: {}",
                        label,
                        idx + 1,
                        total,
                        err_msg
                    );
                    last_err = Some(err_msg);
                    continue;
                }
                // 非 failover 错误（或已穷举所有 key）→ 直接返回
                if is_failover_error(&err_msg) {
                    state.mark_key_failed(provider_id, idx);
                }
                return Err(err_msg);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| format!("{} Error: all {} keys exhausted", label, total)))
}

/// 带重试机制的 HTTP 发送函数
/// 对可重试的错误（限流、服务器错误、超时、连接失败）进行指数退避重试
pub async fn send_with_retry<F, Fut>(
    label: &str,
    attempts: usize,
    mut send: F,
) -> Result<reqwest::Response, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    let attempts = attempts.max(1);
    let mut last_error: Option<String> = None;

    for attempt in 1..=attempts {
        match send().await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return Ok(response);
                }

                let retry_after = parse_retry_after(response.headers());
                let text = response.text().await.unwrap_or_default();
                let err_msg = format!("{} Error: {} - {}", label, status, text);

                if is_retryable_status(status) && attempt < attempts {
                    last_error = Some(err_msg);
                    let delay = retry_delay_ms(attempt, retry_after);
                    eprintln!(
                        "{} retrying in {}ms (attempt {}/{})",
                        label, delay, attempt, attempts
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    continue;
                }

                return Err(format!("{} (attempt {}/{})", err_msg, attempt, attempts));
            }
            Err(err) => {
                let err_msg = format!("{} Error: {}", label, err);
                if is_retryable_error(&err) && attempt < attempts {
                    last_error = Some(err_msg);
                    let delay = retry_delay_ms(attempt, None);
                    eprintln!(
                        "{} retrying in {}ms (attempt {}/{})",
                        label, delay, attempt, attempts
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    continue;
                }
                return Err(format!("{} (attempt {}/{})", err_msg, attempt, attempts));
            }
        }
    }

    Err(last_error
        .map(|msg| format!("{} (attempt {}/{})", msg, attempts, attempts))
        .unwrap_or_else(|| format!("{} Error: exceeded retry attempts ({})", label, attempts)))
}

// ===== OpenAI / Chat completion 调用 =====

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelEndpointKind {
    ChatCompletions,
    Responses,
    LegacyBase,
}

fn normalized_provider_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn provider_endpoint_kind(url: &str) -> ModelEndpointKind {
    let normalized = normalized_provider_url(url).to_ascii_lowercase();
    if normalized.ends_with("/responses") {
        ModelEndpointKind::Responses
    } else if normalized.ends_with("/chat/completions") {
        ModelEndpointKind::ChatCompletions
    } else {
        ModelEndpointKind::LegacyBase
    }
}

fn chat_completions_url(url: &str) -> String {
    let normalized = normalized_provider_url(url);
    if provider_endpoint_kind(&normalized) == ModelEndpointKind::ChatCompletions {
        normalized
    } else {
        format!("{normalized}/chat/completions")
    }
}

fn responses_api_url(url: &str) -> String {
    let normalized = normalized_provider_url(url);
    if provider_endpoint_kind(&normalized) == ModelEndpointKind::Responses {
        normalized
    } else {
        format!("{normalized}/responses")
    }
}

pub fn models_url_from_provider_url(url: &str) -> String {
    let normalized = normalized_provider_url(url);
    let lower = normalized.to_ascii_lowercase();
    let base = if lower.ends_with("/chat/completions") {
        &normalized[..normalized.len() - "/chat/completions".len()]
    } else if lower.ends_with("/responses") {
        &normalized[..normalized.len() - "/responses".len()]
    } else {
        normalized.as_str()
    };
    format!("{base}/models")
}

fn responses_role(role: &str) -> &'static str {
    match role {
        "system" => "developer",
        "assistant" => "assistant",
        "developer" => "developer",
        _ => "user",
    }
}

fn responses_input_text(text: impl Into<String>) -> serde_json::Value {
    serde_json::json!({ "type": "input_text", "text": text.into() })
}

fn responses_output_text(text: impl Into<String>) -> serde_json::Value {
    serde_json::json!({ "type": "output_text", "text": text.into() })
}

fn responses_input_image(url: impl Into<String>) -> serde_json::Value {
    serde_json::json!({ "type": "input_image", "image_url": url.into() })
}

fn responses_text_message(role: &str, text: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
      "role": responses_role(role),
      "content": [responses_input_text(text)]
    })
}

fn responses_tools(web_search: bool) -> serde_json::Value {
    if web_search {
        serde_json::json!([{ "type": "web_search" }])
    } else {
        serde_json::json!([])
    }
}

fn apply_openai_web_search(body: &mut serde_json::Value, web_search: bool) {
    if web_search {
        body["tools"] = responses_tools(true);
        body["tool_choice"] = serde_json::json!("auto");
    }
}

fn normalize_thinking_effort(effort: &str) -> &'static str {
    match effort.trim().to_ascii_lowercase().as_str() {
        "low" => "low",
        "high" => "high",
        "xhigh" => "xhigh",
        _ => "medium",
    }
}

fn thinking_budget_for_effort(effort: &str) -> u64 {
    match normalize_thinking_effort(effort) {
        "low" => 2_000,
        "medium" => 20_000,
        "high" => 64_000,
        "xhigh" => 128_000,
        _ => 20_000,
    }
}

fn remove_body_keys(body: &mut serde_json::Value, keys: &[&str]) {
    if let Some(obj) = body.as_object_mut() {
        for key in keys {
            obj.remove(*key);
        }
    }
}

fn provider_reasoning_key(provider: &settings::ModelProvider, model: &str) -> String {
    format!("{} {} {}", provider.id, provider.base_url, model).to_ascii_lowercase()
}

fn apply_responses_reasoning(
    body: &mut serde_json::Value,
    provider: &settings::ModelProvider,
    model: &str,
    thinking_enabled: bool,
    thinking_effort: &str,
) {
    remove_body_keys(
        body,
        &[
            "reasoning",
            "reasoning_effort",
            "thinking",
            "enable_thinking",
            "thinking_budget",
            "thinking_mode",
        ],
    );
    if !thinking_enabled {
        return;
    }

    let key = provider_reasoning_key(provider, model);
    if key.contains("dashscope") || key.contains("aliyun") {
        body["enable_thinking"] = serde_json::json!(true);
        return;
    }

    let effort = normalize_thinking_effort(thinking_effort);
    body["reasoning"] = serde_json::json!({
      "summary": "auto",
      "effort": effort
    });
}

fn apply_chat_reasoning(
    body: &mut serde_json::Value,
    provider: &settings::ModelProvider,
    model: &str,
    thinking_enabled: bool,
    thinking_effort: &str,
) {
    remove_body_keys(
        body,
        &[
            "reasoning",
            "reasoning_effort",
            "thinking",
            "enable_thinking",
            "thinking_budget",
            "thinking_mode",
        ],
    );

    let key = provider_reasoning_key(provider, model);
    let effort = normalize_thinking_effort(thinking_effort);
    let budget = thinking_budget_for_effort(effort);

    if key.contains("openrouter.ai") {
        body["reasoning"] = if thinking_enabled {
            serde_json::json!({ "enabled": true, "max_tokens": budget })
        } else {
            serde_json::json!({ "enabled": false })
        };
        return;
    }

    if key.contains("dashscope") || key.contains("aliyun") {
        body["enable_thinking"] = serde_json::json!(thinking_enabled);
        if thinking_enabled {
            body["thinking_budget"] = serde_json::json!(budget);
        }
        return;
    }

    if key.contains("siliconflow") {
        if thinking_enabled {
            body["thinking_budget"] = serde_json::json!(budget);
        } else {
            body["enable_thinking"] = serde_json::json!(false);
        }
        return;
    }

    if key.contains("intern-ai") || key.contains("intern") || key.contains("chat.intern-ai.org.cn")
    {
        body["thinking_mode"] = serde_json::json!(thinking_enabled);
        return;
    }

    if key.contains("open.bigmodel.cn")
        || key.contains("bigmodel")
        || key.contains("xiaomimimo")
        || key.contains("mimo-")
        || key.contains("ark.cn-beijing.volces.com")
        || key.contains("volc")
        || key.contains("ark")
        || key.contains("deepseek")
        || key.contains("kimi-k2-thinking")
        || key.contains("kimi-k2.5")
    {
        body["thinking"] = serde_json::json!({
          "type": if thinking_enabled { "enabled" } else { "disabled" }
        });
        return;
    }

    if thinking_enabled {
        body["reasoning_effort"] = serde_json::json!(effort);
    } else {
        body["thinking"] = serde_json::json!({ "type": "disabled" });
    }
}

fn responses_text_content_for_role(role: &str, text: impl Into<String>) -> serde_json::Value {
    if role == "assistant" {
        responses_output_text(text)
    } else {
        responses_input_text(text)
    }
}

fn chat_content_to_responses_content(
    role: &str,
    content: &serde_json::Value,
) -> Vec<serde_json::Value> {
    if let Some(text) = content.as_str() {
        return vec![responses_text_content_for_role(role, text)];
    }

    let mut items = Vec::new();
    if let Some(array) = content.as_array() {
        for item in array {
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match item_type {
                "text" | "input_text" | "output_text" => {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        items.push(responses_text_content_for_role(role, text));
                    }
                }
                "image_url" | "input_image" => {
                    if role == "assistant" {
                        continue;
                    }
                    let url = item
                        .get("image_url")
                        .and_then(|v| {
                            v.as_str()
                                .or_else(|| v.get("url").and_then(|url| url.as_str()))
                        })
                        .or_else(|| item.get("url").and_then(|v| v.as_str()));
                    if let Some(url) = url {
                        items.push(responses_input_image(url));
                    }
                }
                "refusal" => {
                    if role == "assistant" {
                        if let Some(refusal) = item.get("refusal").and_then(|v| v.as_str()) {
                            items.push(serde_json::json!({
                              "type": "refusal",
                              "refusal": refusal
                            }));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    items
}

fn chat_messages_to_responses_input(messages: &serde_json::Value) -> serde_json::Value {
    let input = messages
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|message| {
                    let role = message
                        .get("role")
                        .and_then(|v| v.as_str())
                        .map(responses_role)
                        .unwrap_or("user");
                    let content = chat_content_to_responses_content(
                        role,
                        message.get("content").unwrap_or(&serde_json::Value::Null),
                    );
                    if content.is_empty() {
                        None
                    } else {
                        Some(serde_json::json!({ "role": role, "content": content }))
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    serde_json::Value::Array(input)
}

fn response_max_output_tokens_from_body(body: &serde_json::Value, default_value: u64) -> u64 {
    body.get("max_output_tokens")
        .or_else(|| body.get("max_tokens"))
        .and_then(|v| v.as_u64())
        .filter(|v| *v > 0)
        .unwrap_or(default_value)
}

fn responses_max_output_tokens(
    default_value: u64,
    thinking_enabled: bool,
    thinking_effort: &str,
) -> u64 {
    if !thinking_enabled {
        return default_value;
    }

    let minimum = match normalize_thinking_effort(thinking_effort) {
        "low" => 8_000,
        "medium" => 16_000,
        "high" => 25_000,
        "xhigh" => 32_000,
        _ => 16_000,
    };
    default_value.max(minimum)
}

async fn read_json_response(
    response: reqwest::Response,
    label: &str,
) -> Result<(String, serde_json::Value), String> {
    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        let snippet: String = body_text.chars().take(500).collect();
        return Err(format!("{label} HTTP {}: {}", status.as_u16(), snippet));
    }

    let raw = response
        .text()
        .await
        .map_err(|e| format!("{label} read body: {e}"))?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "{label} parse JSON: {} (body: {})",
            e,
            raw.chars().take(500).collect::<String>()
        )
    })?;

    Ok((raw, value))
}

fn response_incomplete_reason(value: &serde_json::Value) -> Option<String> {
    let response = value.get("response").unwrap_or(value);
    let status = response.get("status").and_then(|v| v.as_str())?;
    if status != "incomplete" {
        return None;
    }

    Some(
        response
            .get("incomplete_details")
            .and_then(|v| v.get("reason"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
    )
}

fn response_stream_delta_text(value: &serde_json::Value) -> Option<String> {
    value
        .get("delta")
        .and_then(|v| {
            v.as_str().map(str::to_string).or_else(|| {
                v.get("text")
                    .and_then(|text| text.as_str())
                    .map(str::to_string)
            })
        })
        .or_else(|| {
            value
                .get("text")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
}

fn response_output_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.get("output_text").and_then(|v| v.as_str()) {
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }

    let mut parts = Vec::new();
    if let Some(output) = value.get("output").and_then(|v| v.as_array()) {
        for item in output {
            if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
                for part in content {
                    let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if matches!(part_type, "output_text" | "text") {
                        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                            parts.push(text);
                        }
                    }
                }
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(""))
    }
}

fn response_stream_done_text(value: &serde_json::Value) -> Option<String> {
    response_stream_delta_text(value)
        .or_else(|| value.get("part").and_then(response_output_text))
        .or_else(|| value.get("item").and_then(response_output_text))
        .or_else(|| value.get("response").and_then(response_output_text))
}

fn parse_response_output_text(
    raw: &str,
    value: &serde_json::Value,
    label: &str,
) -> Result<String, String> {
    if let Some(text) = response_output_text(value)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
    {
        return Ok(text);
    }

    if let Some(reason) = response_incomplete_reason(value) {
        return Err(format!(
            "{label} incomplete: {reason}. 当前思考/搜索可能耗尽了输出 token，请降低思考强度或稍后重试。"
        ));
    }

    Err(format!(
        "Invalid {label} response: {}",
        raw.chars().take(500).collect::<String>()
    ))
}

fn responses_stream_error(value: &serde_json::Value) -> String {
    value
        .get("error")
        .or_else(|| value.get("response").and_then(|v| v.get("error")))
        .map(|v| v.to_string())
        .unwrap_or_else(|| value.to_string())
        .chars()
        .take(500)
        .collect()
}

/// 调用 OpenAI 兼容的文本聊天接口
/// 发送单轮 user 消息，temperature 设为 0.2,返回模型生成的文本内容
pub async fn call_openai_text(
    state: &State<'_, AppState>,
    config: &settings::ModelProvider,
    model: &str,
    prompt: String,
    retry_attempts: usize,
    thinking_enabled: bool,
    thinking_effort: &str,
) -> Result<String, String> {
    // Apple Intelligence(端上)路由：跳过 HTTP，直接调 sidecar。model/retry/thinking 三个参数全部忽略。
    if config.base_url == APPLE_INTELLIGENCE_BASE_URL {
        let _ = (model, retry_attempts, thinking_enabled, thinking_effort);
        return state.apple_intelligence.call_text(&prompt).await;
    }

    if provider_endpoint_kind(&config.base_url) == ModelEndpointKind::Responses {
        let url = responses_api_url(&config.base_url);
        let mut body = serde_json::json!({
          "model": model,
          "input": [responses_text_message("user", prompt)],
          "max_output_tokens": responses_max_output_tokens(2000, thinking_enabled, thinking_effort)
        });
        apply_openai_web_search(&mut body, true);
        apply_responses_reasoning(&mut body, config, model, thinking_enabled, thinking_effort);

        let response = send_with_failover(
            state,
            "OpenAI Responses",
            retry_attempts,
            &config.id,
            &config.api_keys,
            |key| {
                state
                    .http
                    .post(url.clone())
                    .bearer_auth(key)
                    .json(&body)
                    .send()
            },
        )
        .await?;

        let (raw, value) = read_json_response(response, "OpenAI Responses").await?;
        return parse_response_output_text(&raw, &value, "OpenAI Responses");
    }

    let url = chat_completions_url(&config.base_url);
    let mut body = serde_json::json!({
      "model": model,
      "messages": [{ "role": "user", "content": prompt }],
      "temperature": 0.2
    });
    apply_chat_reasoning(&mut body, config, model, thinking_enabled, thinking_effort);

    let response = send_with_failover(
        state,
        "OpenAI API",
        retry_attempts,
        &config.id,
        &config.api_keys,
        |key| {
            state
                .http
                .post(url.clone())
                .bearer_auth(key)
                .json(&body)
                .send()
        },
    )
    .await?;

    let value: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    let content = value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .ok_or_else(|| "Invalid response".to_string())?;

    Ok(content.trim().to_string())
}

/// 调用 OpenAI 兼容的 OCR/视觉接口
/// 将图片转为 Base64 后作为 image_url 类型内容发送，temperature 设为 0 以提高识别稳定性
pub async fn call_openai_ocr(
    state: &State<'_, AppState>,
    config: &settings::ModelProvider,
    model: &str,
    image_path: &Path,
    prompt: &str,
    retry_attempts: usize,
    thinking_enabled: bool,
    thinking_effort: &str,
) -> Result<String, String> {
    if config.base_url == APPLE_INTELLIGENCE_BASE_URL {
        let _ = (
            state,
            model,
            image_path,
            prompt,
            retry_attempts,
            thinking_enabled,
            thinking_effort,
        );
        return Err(
            "Apple Intelligence 暂不支持图像输入,请为截图/视觉功能配置云端 provider".into(),
        );
    }
    let bytes = fs::read(image_path).map_err(|e| e.to_string())?;
    let base64 = general_purpose::STANDARD.encode(bytes);

    if provider_endpoint_kind(&config.base_url) == ModelEndpointKind::Responses {
        let url = responses_api_url(&config.base_url);
        let mut body = serde_json::json!({
          "model": model,
          "input": [
            {
              "role": "user",
              "content": [
                responses_input_image(format!("data:image/png;base64,{base64}")),
                responses_input_text(prompt)
              ]
            }
          ],
          "max_output_tokens": responses_max_output_tokens(2000, thinking_enabled, thinking_effort)
        });
        apply_responses_reasoning(&mut body, config, model, thinking_enabled, thinking_effort);

        let response = send_with_failover(
            state,
            "OpenAI OCR",
            retry_attempts,
            &config.id,
            &config.api_keys,
            |key| {
                state
                    .http
                    .post(url.clone())
                    .bearer_auth(key)
                    .json(&body)
                    .send()
            },
        )
        .await?;

        let (raw, value) = read_json_response(response, "OCR").await?;
        return parse_response_output_text(&raw, &value, "OCR");
    }

    let url = chat_completions_url(&config.base_url);

    // 与 lens 的 vision body 对齐：image 在 text 前、显式 max_tokens。
    // thinking 按调用方传入：截图翻译默认 false（节省时间），lens 默认 true。
    let mut body = serde_json::json!({
      "model": model,
      "messages": [
        {
          "role": "user",
          "content": [
            {
              "type": "image_url",
              "image_url": { "url": format!("data:image/png;base64,{base64}") }
            },
            {
              "type": "text",
              "text": prompt
            }
          ]
        }
      ],
      "temperature": 0.2,
      "max_tokens": 2000
    });
    apply_chat_reasoning(&mut body, config, model, thinking_enabled, thinking_effort);

    let response = send_with_failover(
        state,
        "OpenAI OCR",
        retry_attempts,
        &config.id,
        &config.api_keys,
        |key| {
            state
                .http
                .post(url.clone())
                .bearer_auth(key)
                .json(&body)
                .send()
        },
    )
    .await?;

    // 显式检查 HTTP 状态：非 2xx 把原始 body 文本带回，避免后续 .json() 抛出含糊的 "error decoding response body"
    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        let snippet: String = body_text.chars().take(500).collect();
        return Err(format!("OCR HTTP {}: {}", status.as_u16(), snippet));
    }

    let raw = response
        .text()
        .await
        .map_err(|e| format!("OCR read body: {}", e))?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "OCR parse JSON: {} (body: {})",
            e,
            raw.chars().take(500).collect::<String>()
        )
    })?;
    let content = value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .ok_or_else(|| {
            format!(
                "Invalid OCR response: {}",
                raw.chars().take(500).collect::<String>()
            )
        })?;

    Ok(content.trim().to_string())
}

fn baidu_response_error(value: &serde_json::Value) -> Option<String> {
    let code = value
        .get("error_code")
        .and_then(|v| {
            v.as_i64()
                .map(|n| n.to_string())
                .or_else(|| v.as_str().map(str::to_string))
        })
        .or_else(|| {
            value
                .get("error")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });

    code.map(|code| {
        let msg = value
            .get("error_msg")
            .or_else(|| value.get("error_description"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        format!("Baidu API {code}: {msg}")
    })
}

async fn baidu_ocr_access_token(
    state: &State<'_, AppState>,
    config: &settings::BaiduOcrConfig,
    retry_attempts: usize,
) -> Result<String, String> {
    let api_key = config.api_key.trim();
    let secret_key = config.secret_key.trim();
    if api_key.is_empty() || secret_key.is_empty() {
        return Err("Missing Baidu OCR API Key or Secret Key".to_string());
    }

    let cache_key = format!("{:x}", md5::compute(format!("{api_key}:{secret_key}")));
    {
        let tokens = state
            .baidu_ocr_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((token, expires_at)) = tokens.get(&cache_key) {
            if *expires_at > Instant::now() {
                return Ok(token.clone());
            }
        }
    }

    let form = vec![
        ("grant_type", "client_credentials".to_string()),
        ("client_id", api_key.to_string()),
        ("client_secret", secret_key.to_string()),
    ];
    let response = send_with_retry("Baidu OCR token", retry_attempts, || {
        state
            .http
            .post("https://aip.baidubce.com/oauth/2.0/token")
            .form(&form)
            .send()
    })
    .await?;
    let raw = response
        .text()
        .await
        .map_err(|e| format!("Baidu OCR token read body: {e}"))?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Baidu OCR token parse JSON: {e} (body: {})",
            raw.chars().take(500).collect::<String>()
        )
    })?;
    if let Some(err) = baidu_response_error(&value) {
        return Err(err);
    }
    let token = value
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "Invalid Baidu OCR token response: {}",
                raw.chars().take(500).collect::<String>()
            )
        })?
        .to_string();
    let expires_in = value
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(2_592_000);
    let valid_for = expires_in.saturating_sub(300).max(60);
    {
        let mut tokens = state
            .baidu_ocr_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        tokens.insert(
            cache_key,
            (
                token.clone(),
                Instant::now() + Duration::from_secs(valid_for),
            ),
        );
    }
    Ok(token)
}

/// 调用百度智能云文字识别接口。
pub async fn call_baidu_ocr(
    state: &State<'_, AppState>,
    config: &settings::BaiduOcrConfig,
    image_path: &Path,
    retry_attempts: usize,
) -> Result<String, String> {
    let token = baidu_ocr_access_token(state, config, retry_attempts).await?;
    let bytes = fs::read(image_path).map_err(|e| e.to_string())?;
    let image_base64 = general_purpose::STANDARD.encode(bytes);
    let language_type = config.language_type.trim();
    let language_type = if language_type.is_empty() {
        "CHN_ENG"
    } else {
        language_type
    };
    let endpoint = if config.accurate {
        "https://aip.baidubce.com/rest/2.0/ocr/v1/accurate_basic"
    } else {
        "https://aip.baidubce.com/rest/2.0/ocr/v1/general_basic"
    };
    let form = vec![
        ("image", image_base64),
        ("language_type", language_type.to_string()),
        ("detect_direction", "true".to_string()),
        ("paragraph", "true".to_string()),
    ];
    let response = send_with_retry("Baidu OCR", retry_attempts, || {
        state
            .http
            .post(endpoint)
            .query(&[("access_token", token.as_str())])
            .form(&form)
            .send()
    })
    .await?;
    let raw = response
        .text()
        .await
        .map_err(|e| format!("Baidu OCR read body: {e}"))?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Baidu OCR parse JSON: {e} (body: {})",
            raw.chars().take(500).collect::<String>()
        )
    })?;
    if let Some(err) = baidu_response_error(&value) {
        return Err(err);
    }

    let lines = value
        .get("words_result")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            format!(
                "Invalid Baidu OCR response: {}",
                raw.chars().take(500).collect::<String>()
            )
        })?
        .iter()
        .filter_map(|item| item.get("words").and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();

    if let Some(paragraphs) = value.get("paragraphs_result").and_then(|v| v.as_array()) {
        let grouped = paragraphs
            .iter()
            .filter_map(|paragraph| {
                let text = paragraph
                    .get("words_result_idx")
                    .and_then(|v| v.as_array())
                    .map(|indices| {
                        indices
                            .iter()
                            .filter_map(|idx| idx.as_u64())
                            .filter_map(|idx| lines.get(idx as usize))
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
            .collect::<Vec<_>>();
        if !grouped.is_empty() {
            return Ok(grouped.join("\n\n"));
        }
    }

    Ok(lines.join("\n"))
}

fn collect_chaoxing_text(value: &serde_json::Value, lines: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(|v| v.as_str()) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    lines.push(trimmed.to_string());
                }
            }
            for child in map.values() {
                collect_chaoxing_text(child, lines);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_chaoxing_text(item, lines);
            }
        }
        _ => {}
    }
}

fn chaoxing_ocr_lines(value: &serde_json::Value) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(data) = value.get("data") {
        collect_chaoxing_text(data, &mut lines);
    }
    lines
}

/// 调用学习通 OCR 接口。
pub async fn call_chaoxing_ocr(
    state: &State<'_, AppState>,
    image_path: &Path,
    retry_attempts: usize,
) -> Result<String, String> {
    const ENDPOINT: &str = "http://ai.chaoxing.com/api/v1/ocr/common/sync";
    const SECRET_ID: &str = "Inner_40731a6efece4c2e992c0d670222e6da";
    const SIGN_SALT: &str = "43e7a66431b14c8f856a8e889070c19b";
    const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

    let bytes = fs::read(image_path).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("Chaoxing OCR Error: image size cannot exceed 5MB".to_string());
    }

    let image_base64 = general_purpose::STANDARD.encode(bytes);
    let now_ms = Utc::now().timestamp_millis();
    let nonce = (now_ms.unsigned_abs() % 100_000) as u32;
    let body_value = serde_json::json!({
        "images": [
            {
                "data": image_base64,
                "dataId": "1",
                "type": 2
            }
        ],
        "nonce": nonce,
        "secretId": SECRET_ID,
        "timestamp": now_ms
    });
    let body =
        serde_json::to_string(&body_value).map_err(|e| format!("Chaoxing OCR build body: {e}"))?;
    let signature = format!(
        "{:x}",
        md5::compute(format!("{body}{SIGN_SALT}").as_bytes())
    );

    let response = send_with_retry("Chaoxing OCR", retry_attempts, || {
        state
            .http
            .post(ENDPOINT)
            .header(CONTENT_TYPE, "application/json;charset=utf-8")
            .header("CX-Signature", signature.clone())
            .body(body.clone())
            .send()
    })
    .await?;
    let raw = response
        .text()
        .await
        .map_err(|e| format!("Chaoxing OCR read body: {e}"))?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Chaoxing OCR parse JSON: {e} (body: {})",
            raw.chars().take(500).collect::<String>()
        )
    })?;
    let lines = chaoxing_ocr_lines(&value);
    if lines.is_empty() {
        let msg = value
            .get("msg")
            .or_else(|| value.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("empty OCR result");
        return Err(format!("Chaoxing OCR Error: {msg}"));
    }

    Ok(lines.join("\n"))
}

fn baidu_translate_lang(code: &str) -> &'static str {
    match code {
        "zh" | "zh-Hans" => "zh",
        "zh-Hant" | "zh-TW" | "zh_TW" => "cht",
        "en" => "en",
        "ja" | "jp" => "jp",
        "ko" | "kor" => "kor",
        "fr" | "fra" => "fra",
        "de" => "de",
        _ => "zh",
    }
}

fn microsoft_translate_lang(code: &str) -> &'static str {
    match code {
        "zh" | "zh-Hans" => "zh-Hans",
        "zh-Hant" | "zh-TW" | "zh_TW" => "zh-Hant",
        "en" => "en",
        "ja" | "jp" => "ja",
        "ko" | "kor" => "ko",
        "fr" | "fra" => "fr",
        "de" => "de",
        _ => "zh-Hans",
    }
}

fn google_translate_lang(code: &str) -> &'static str {
    match code {
        "zh" | "zh-Hans" => "zh-CN",
        "zh-Hant" | "zh-TW" | "zh_TW" => "zh-TW",
        "en" => "en",
        "ja" | "jp" => "ja",
        "ko" | "kor" => "ko",
        "fr" | "fra" => "fr",
        "de" => "de",
        _ => "zh-CN",
    }
}

fn google_hot_patch(language_code: &str) -> &str {
    match language_code {
        "mni" => "mni-Mtei",
        "prs" => "fa-FA",
        "nqo" => "bm-Nkoo",
        "ndc" => "ndc-ZW",
        "sat" => "sat-Latn",
        _ => language_code,
    }
}

fn google_token_work(mut num: i64, seed: &str) -> i64 {
    let chars = seed.as_bytes();
    let mut i = 0usize;
    while i + 2 < chars.len() {
        let mut d = chars[i + 2] as i64;
        if d >= b'a' as i64 {
            d -= b'W' as i64;
        }
        let shift = d as u32;
        if chars[i + 1] == b'+' {
            num = num.wrapping_add(num >> shift) & u32::MAX as i64;
        } else {
            num ^= num.wrapping_shl(shift);
        }
        i += 3;
    }
    num
}

fn google_translate_token_for_hour(text: &str, hour: i64) -> String {
    let mut a = hour;
    let b = a;

    // GTranslate 的 C# 实现按 UTF-16 char 遍历；这里用 encode_utf16 保持同一语义。
    for unit in text.encode_utf16() {
        a = google_token_work(a + i64::from(unit), "+-a^+6");
    }

    a = google_token_work(a, "+-3^+b+-f");
    if a < 0 {
        a = (a & i32::MAX as i64) + i32::MAX as i64 + 1;
    }
    a %= 1_000_000;

    format!("{a}.{}", a ^ b)
}

fn google_translate_token(text: &str) -> String {
    google_translate_token_for_hour(text, Utc::now().timestamp() / 3600)
}

#[derive(Debug, Deserialize)]
struct GoogleTranslateSentence {
    #[serde(default, rename = "trans")]
    translation: String,
}

#[derive(Debug, Deserialize)]
struct GoogleTranslateResponse {
    #[serde(default)]
    sentences: Vec<GoogleTranslateSentence>,
}

fn google_translate_response_text(raw: &str) -> Result<String, String> {
    let value: GoogleTranslateResponse = serde_json::from_str(raw).map_err(|e| {
        format!(
            "Google Translate parse JSON: {e} (body: {})",
            raw.chars().take(500).collect::<String>()
        )
    })?;
    let translated = value
        .sentences
        .into_iter()
        .map(|sentence| sentence.translation)
        .collect::<String>()
        .trim()
        .to_string();
    if translated.is_empty() {
        return Err(format!(
            "Invalid Google Translate response: {}",
            raw.chars().take(500).collect::<String>()
        ));
    }
    Ok(translated)
}

fn microsoft_signature_url_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if keep {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn microsoft_translate_signature(url: &str) -> String {
    type HmacSha256 = Hmac<Sha256>;

    const APP_ID: &str = "MSTranslatorAndroidApp";
    const PRIVATE_KEY: [u8; 64] = [
        0xa2, 0x29, 0x3a, 0x3d, 0xd0, 0xdd, 0x32, 0x73, 0x97, 0x7a, 0x64, 0xdb, 0xc2, 0xf3, 0x27,
        0xf5, 0xd7, 0xbf, 0x87, 0xd9, 0x45, 0x9d, 0xf0, 0x5a, 0x09, 0x66, 0xc6, 0x30, 0xc6, 0x6a,
        0xaa, 0x84, 0x9a, 0x41, 0xaa, 0x94, 0x3a, 0xa8, 0xd5, 0x1a, 0x6e, 0x4d, 0xaa, 0xc9, 0xa3,
        0x70, 0x12, 0x35, 0xc7, 0xeb, 0x12, 0xf6, 0xe8, 0x23, 0x07, 0x9e, 0x47, 0x10, 0x95, 0x91,
        0x88, 0x55, 0xd8, 0x17,
    ];

    let escaped_url = microsoft_signature_url_encode(url);
    let timestamp = Utc::now().format("%a, %d %b %Y %H:%M:%SGMT");
    let uuid = Uuid::new_v4().simple().to_string();
    let value = format!("{APP_ID}{escaped_url}{timestamp}{uuid}").to_lowercase();
    let mut mac = HmacSha256::new_from_slice(&PRIVATE_KEY)
        .expect("Microsoft Translator private key length is fixed");
    mac.update(value.as_bytes());
    let signature = general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    format!("{APP_ID}::{signature}::{timestamp}::{uuid}")
}

fn microsoft_translate_chunks(text: &str) -> Vec<String> {
    const MAX_CHARS: usize = 900;

    fn push_long_segment(chunks: &mut Vec<String>, value: &str) {
        let mut current = String::new();
        let mut current_len = 0usize;
        for ch in value.chars() {
            if current_len >= MAX_CHARS {
                chunks.push(current.trim().to_string());
                current = String::new();
                current_len = 0;
            }
            current.push(ch);
            current_len += 1;
        }
        if !current.trim().is_empty() {
            chunks.push(current.trim().to_string());
        }
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let line_len = line.chars().count();
        if line_len > MAX_CHARS {
            if !current.trim().is_empty() {
                chunks.push(current.trim().to_string());
                current.clear();
                current_len = 0;
            }
            push_long_segment(&mut chunks, line);
            continue;
        }

        let separator_len = if current.is_empty() { 0 } else { 1 };
        if current_len + separator_len + line_len > MAX_CHARS {
            if !current.trim().is_empty() {
                chunks.push(current.trim().to_string());
            }
            current = line.to_string();
            current_len = line_len;
        } else {
            if !current.is_empty() {
                current.push('\n');
                current_len += 1;
            }
            current.push_str(line);
            current_len += line_len;
        }
    }

    if !current.trim().is_empty() {
        chunks.push(current.trim().to_string());
    }

    chunks
}

fn tencent_translate_lang(code: &str) -> &'static str {
    match code {
        "zh" | "zh-Hans" | "zh-Hant" | "zh-TW" | "zh_TW" => "zh",
        "en" => "en",
        "ja" | "jp" => "ja",
        "ko" | "kor" => "ko",
        "fr" | "fra" => "fr",
        "de" => "de",
        _ => "zh",
    }
}

fn bing_translate_lang(code: &str) -> &'static str {
    match code {
        "zh" | "zh-Hans" => "zh-Hans",
        "zh-Hant" | "zh-TW" | "zh_TW" => "zh-Hant",
        "en" => "en",
        "ja" | "jp" => "ja",
        "ko" | "kor" => "ko",
        "fr" | "fra" => "fr",
        "de" => "de",
        _ => "zh-Hans",
    }
}

fn yandex_translate_lang(code: &str) -> &'static str {
    match code {
        "zh" | "zh-Hans" => "zh",
        "zh-Hant" | "zh-TW" | "zh_TW" => "zh",
        "en" => "en",
        "ja" | "jp" => "ja",
        "ko" | "kor" => "ko",
        "fr" | "fra" => "fr",
        "de" => "de",
        _ => "zh",
    }
}

fn caiyun_translate_lang(code: &str) -> &'static str {
    match code {
        "zh" | "zh-Hans" => "zh",
        "zh-Hant" | "zh-TW" | "zh_TW" => "zh-Hant",
        "en" => "en",
        "ja" | "jp" => "ja",
        "ko" | "kor" => "ko",
        "fr" | "fra" => "fr",
        "de" => "de",
        _ => "zh",
    }
}

fn split_text_by_newlines(text: &str, max_len: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for (idx, line) in text.split('\n').enumerate() {
        let mut line_with_newline = line.to_string();
        if idx < text.split('\n').count().saturating_sub(1) {
            line_with_newline.push('\n');
        }
        if !current.is_empty()
            && current.chars().count() + line_with_newline.chars().count() > max_len
        {
            chunks.push(current);
            current = String::new();
        }
        if line_with_newline.chars().count() > max_len {
            let mut part = String::new();
            for ch in line_with_newline.chars() {
                if part.chars().count() >= max_len {
                    chunks.push(part);
                    part = String::new();
                }
                part.push(ch);
            }
            if !part.is_empty() {
                current.push_str(&part);
            }
        } else {
            current.push_str(&line_with_newline);
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(text.to_string());
    }
    chunks
}

fn find_between<'a>(value: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let start_pos = value.find(start)? + start.len();
    let rest = &value[start_pos..];
    let end_pos = rest.find(end)?;
    Some(&rest[..end_pos])
}

fn parse_bing_credentials(html: &str) -> Result<(String, String, String), String> {
    let ig = find_between(html, "IG:\"", "\"")
        .ok_or_else(|| "Unable to find Bing IG value".to_string())?
        .to_string();
    let marker = "var params_AbusePreventionHelper";
    let marker_pos = html
        .find(marker)
        .ok_or_else(|| "Unable to find Bing credentials marker".to_string())?;
    let after_marker = &html[marker_pos..];
    let open = after_marker
        .find('[')
        .ok_or_else(|| "Unable to find Bing credentials start".to_string())?;
    let close = after_marker[open + 1..]
        .find(']')
        .ok_or_else(|| "Unable to find Bing credentials end".to_string())?
        + open
        + 1;
    let inner = &after_marker[open + 1..close];
    let mut parts = inner.splitn(2, ',');
    let key = parts
        .next()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "Unable to find Bing key".to_string())?
        .to_string();
    let rest = parts
        .next()
        .ok_or_else(|| "Unable to find Bing token".to_string())?;
    let token = find_between(rest, "\"", "\"")
        .ok_or_else(|| "Unable to find Bing token".to_string())?
        .to_string();
    Ok((key, token, ig))
}

fn bing_market_from_lang(lang: &str) -> &'static str {
    match lang.to_ascii_lowercase().as_str() {
        "zh-cn" | "zh-hans" => "zh-CN",
        "zh-tw" | "zh-hant" => "zh-TW",
        "en" | "en-us" => "en-US",
        "ja" | "ja-jp" => "ja-JP",
        "ko" | "ko-kr" => "ko-KR",
        "fr" | "fr-fr" => "fr-FR",
        "de" | "de-de" => "de-DE",
        _ => "en-US",
    }
}

fn parse_set_cookie_name_value(value: &str) -> Option<(String, String)> {
    let first = value.split(';').next()?.trim();
    let (name, val) = first.split_once('=')?;
    Some((name.trim().to_string(), val.trim().to_string()))
}

fn bing_cookie_header(response: &reqwest::Response, market: &str) -> String {
    let mut cookies = Vec::<(String, String)>::new();
    for value in response.headers().get_all("set-cookie").iter() {
        let Ok(raw) = value.to_str() else {
            continue;
        };
        if let Some((name, val)) = parse_set_cookie_name_value(raw) {
            if name == "_EDGE_S" {
                let sid = find_between(&val, "SID=", "&")
                    .map(str::to_string)
                    .or_else(|| {
                        let with_suffix = format!("{val};");
                        find_between(&with_suffix, "SID=", ";").map(str::to_string)
                    });
                if let Some(sid) = sid {
                    cookies.push((name, format!("SID={sid}&mkt={market}")));
                }
            } else {
                cookies.push((name, val));
            }
        }
    }

    if !cookies.iter().any(|(name, _)| name == "_EDGE_S") {
        cookies.push((
            "_EDGE_S".to_string(),
            format!(
                "SID={}&mkt={market}",
                Uuid::new_v4().simple().to_string().to_uppercase()
            ),
        ));
    }

    cookies
        .into_iter()
        .map(|(name, val)| format!("{name}={val}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn response_origin(response: &reqwest::Response) -> String {
    let url = response.url();
    let host = url.host_str().unwrap_or("www.bing.com");
    match url.port() {
        Some(port) => format!("{}://{}:{}", url.scheme(), host, port),
        None => format!("{}://{}", url.scheme(), host),
    }
}

fn parse_bing_translation(raw: &str, label: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| {
        format!(
            "{label} parse JSON: {e} (body: {})",
            raw.chars().take(500).collect::<String>()
        )
    })?;
    let translated = value
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item.get("translations"))
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if translated.is_empty() {
        return Err(format!(
            "Invalid {label} response: {}",
            raw.chars().take(500).collect::<String>()
        ));
    }
    Ok(translated)
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_hex(value: &str) -> String {
    use sha2::Digest;
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex_lower(&hasher.finalize())
}

fn hmac_sha256(key: &[u8], value: &str) -> Vec<u8> {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts arbitrary key length");
    mac.update(value.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn tencent_translate_signature(
    secret_id: &str,
    secret_key: &str,
    timestamp: i64,
    payload: &str,
) -> String {
    let date = Utc
        .timestamp_opt(timestamp, 0)
        .single()
        .unwrap_or_else(Utc::now)
        .format("%Y-%m-%d")
        .to_string();
    let canonical_request = format!(
        "POST\n/\n\ncontent-type:application/json; charset=utf-8\nhost:tmt.tencentcloudapi.com\n\ncontent-type;host\n{}",
        sha256_hex(payload)
    );
    let credential_scope = format!("{date}/tmt/tc3_request");
    let string_to_sign = format!(
        "TC3-HMAC-SHA256\n{timestamp}\n{credential_scope}\n{}",
        sha256_hex(&canonical_request)
    );
    let secret_date = hmac_sha256(format!("TC3{secret_key}").as_bytes(), &date);
    let secret_service = hmac_sha256(&secret_date, "tmt");
    let secret_signing = hmac_sha256(&secret_service, "tc3_request");
    let signature = hex_lower(&hmac_sha256(&secret_signing, &string_to_sign));
    format!(
        "TC3-HMAC-SHA256 Credential={secret_id}/{credential_scope}, SignedHeaders=content-type;host, Signature={signature}"
    )
}

fn caiyun_trans_type(target_lang: &str) -> Result<String, String> {
    let to = caiyun_translate_lang(target_lang);
    let trans_type = format!("auto2{to}");
    let supported = matches!(
        trans_type.as_str(),
        "auto2zh" | "auto2zh-Hant" | "auto2en" | "auto2ja" | "auto2ko"
    );
    if supported {
        Ok(trans_type)
    } else {
        Err(format!("Caiyun2 does not support auto -> {to}"))
    }
}

fn is_cjk_text(text: &str) -> bool {
    text.chars().any(|ch| {
        matches!(
            ch as u32,
            0x3400..=0x9fff | 0xf900..=0xfaff | 0x3040..=0x30ff | 0xac00..=0xd7af
        )
    })
}

fn baidu_tts_lang(text: &str) -> &'static str {
    if is_cjk_text(text) {
        "zh"
    } else {
        "en"
    }
}

fn baidu_tts_url_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if keep {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// 调用百度翻译网页端 TTS，参数与 TianruoOCR 朗读功能保持同样的默认语速/音量取向。
/// 不复用 TianruoOCR 中硬编码的百度开放平台密钥，避免把第三方密钥写入本项目。
pub async fn call_baidu_tts_data_url(
    state: &State<'_, AppState>,
    text: &str,
    retry_attempts: usize,
) -> Result<String, String> {
    let cleaned = text.replace("***", "");
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return Err("No text to speak".to_string());
    }

    let lang = baidu_tts_lang(trimmed);
    let encoded_text = baidu_tts_url_encode(trimmed);
    let url =
        format!("https://fanyi.baidu.com/gettts?lan={lang}&text={encoded_text}&spd=5&source=web");

    let response = send_with_retry("Baidu TTS", retry_attempts, || {
        state
            .http
            .get(url.clone())
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36",
            )
            .header("Referer", "https://fanyi.baidu.com/")
            .send()
    })
    .await?;
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("audio/mpeg")
        .to_string();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("Baidu TTS read body: {e}"))?;

    if bytes.is_empty() {
        return Err("Baidu TTS returned empty audio".to_string());
    }
    if content_type.contains("json") || bytes.first() == Some(&b'{') {
        let body = String::from_utf8_lossy(&bytes);
        return Err(format!("Baidu TTS error: {body}"));
    }

    Ok(format!(
        "data:audio/mpeg;base64,{}",
        general_purpose::STANDARD.encode(bytes)
    ))
}

/// 调用百度翻译开放平台。
pub async fn call_baidu_translate(
    state: &State<'_, AppState>,
    config: &settings::BaiduTranslateConfig,
    text: &str,
    target_lang: &str,
    retry_attempts: usize,
) -> Result<String, String> {
    let app_id = config.app_id.trim();
    let app_key = config.app_key.trim();
    if app_id.is_empty() || app_key.is_empty() {
        return Err("Missing Baidu Translate APP ID or APP Key".to_string());
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }

    let source_lang = config.source_lang.trim();
    let from = if source_lang.is_empty() || source_lang == "auto" {
        "auto".to_string()
    } else {
        baidu_translate_lang(source_lang).to_string()
    };
    let to = baidu_translate_lang(target_lang).to_string();
    let salt = Uuid::new_v4().to_string();
    let sign = format!(
        "{:x}",
        md5::compute(format!("{app_id}{trimmed}{salt}{app_key}"))
    );
    let form = vec![
        ("q", trimmed.to_string()),
        ("from", from),
        ("to", to),
        ("appid", app_id.to_string()),
        ("salt", salt),
        ("sign", sign),
    ];

    let response = send_with_retry("Baidu Translate", retry_attempts, || {
        state
            .http
            .post("https://fanyi-api.baidu.com/api/trans/vip/translate")
            .form(&form)
            .send()
    })
    .await?;
    let raw = response
        .text()
        .await
        .map_err(|e| format!("Baidu Translate read body: {e}"))?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Baidu Translate parse JSON: {e} (body: {})",
            raw.chars().take(500).collect::<String>()
        )
    })?;
    if let Some(err) = baidu_response_error(&value) {
        return Err(err);
    }
    let translated = value
        .get("trans_result")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            format!(
                "Invalid Baidu Translate response: {}",
                raw.chars().take(500).collect::<String>()
            )
        })?
        .iter()
        .filter_map(|item| item.get("dst").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(translated.trim().to_string())
}

/// 调用 Google Translate 旧版 gtx 接口。
/// 参考 TianruoOCR 使用的 GTranslate.GoogleTranslator：translate.googleapis.com/translate_a/single，
/// 无密钥，source 固定 auto；是否可用取决于用户当前网络环境。
pub async fn call_google_translate(
    state: &State<'_, AppState>,
    text: &str,
    target_lang: &str,
    retry_attempts: usize,
) -> Result<String, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }

    let source = google_hot_patch("auto");
    let target = google_hot_patch(google_translate_lang(target_lang));
    let token = google_translate_token(trimmed);
    let url = format!(
        "https://translate.googleapis.com/translate_a/single?client=gtx&sl={source}&tl={target}&dt=t&dt=bd&dj=1&source=input&tk={token}"
    );
    let form = [("q", trimmed.to_string())];

    let response = send_with_retry("Google Translate", retry_attempts, || {
        state
            .http
            .post(url.clone())
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36",
            )
            .form(&form)
            .send()
    })
    .await?;
    let raw = response
        .text()
        .await
        .map_err(|e| format!("Google Translate read body: {e}"))?;

    google_translate_response_text(&raw)
}

/// 调用 TianruoOCR 的 Bing 翻译路径：先访问 bing.com/translator 抓取 IG/key/token，
/// 再调用 ttranslatev3。
pub async fn call_bing_translate(
    state: &State<'_, AppState>,
    text: &str,
    target_lang: &str,
    retry_attempts: usize,
) -> Result<String, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }

    let target = bing_translate_lang(target_lang);
    let market = bing_market_from_lang(target);
    let response = send_with_retry("Bing Translate bootstrap", retry_attempts, || {
        state
            .http
            .get("https://www.bing.com/translator")
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
            )
            .header("Referer", "https://www.bing.com/translator")
            .send()
    })
    .await?;
    let base = response_origin(&response);
    let cookie = bing_cookie_header(&response, market);
    let html = response
        .text()
        .await
        .map_err(|e| format!("Bing Translate bootstrap read body: {e}"))?;
    let (key, token, ig) = parse_bing_credentials(&html)?;
    let request_url = format!("{base}/ttranslatev3?isVertical=1&IG={ig}&IID=translator.5028.1");

    let mut translated = String::new();
    for chunk in split_text_by_newlines(trimmed, 1000) {
        let form = vec![
            ("fromLang", "auto-detect".to_string()),
            ("text", chunk),
            ("to", target.to_string()),
            ("tryFetchingGenderDebiasedTranslations", "true".to_string()),
            ("token", token.clone()),
            ("key", key.clone()),
        ];
        let response = send_with_retry("Bing Translate", retry_attempts, || {
            state
                .http
                .post(request_url.clone())
                .header("Cookie", cookie.clone())
                .header("Referer", "https://www.bing.com/translator")
                .form(&form)
                .send()
        })
        .await?;
        let raw = response
            .text()
            .await
            .map_err(|e| format!("Bing Translate read body: {e}"))?;
        translated.push_str(&parse_bing_translation(&raw, "Bing Translate")?);
    }

    Ok(translated.trim().to_string())
}

/// 调用 TianruoOCR 的 Bing2 路径：Edge translate auth + api-edge.cognitive.microsofttranslator.com。
pub async fn call_bing2_translate(
    state: &State<'_, AppState>,
    text: &str,
    target_lang: &str,
    retry_attempts: usize,
) -> Result<String, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }

    let token_response = send_with_retry("Bing2 Translate auth", retry_attempts, || {
        state
            .http
            .get("https://edge.microsoft.com/translate/auth")
            .header("Accept", "*/*")
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/113.0.0.0 Safari/537.36 Edg/113.0.1774.42",
            )
            .send()
    })
    .await?;
    let token = token_response
        .text()
        .await
        .map_err(|e| format!("Bing2 Translate auth read body: {e}"))?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err("Bing2 Translate auth token is empty".to_string());
    }

    let to = bing_translate_lang(target_lang);
    let url = format!(
        "https://api-edge.cognitive.microsofttranslator.com/translate?api-version=3.0&from=&to={to}&includeSentenceLength=true"
    );
    let body = serde_json::json!([{ "Text": trimmed }]);
    let response = send_with_retry("Bing2 Translate", retry_attempts, || {
        state
            .http
            .post(url.clone())
            .header("Accept", "*/*")
            .header("Authorization", format!("Bearer {token}"))
            .header("Cache-Control", "no-cache")
            .header("Pragma", "no-cache")
            .header("Referer", "https://appsumo.com/")
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/113.0.0.0 Safari/537.36 Edg/113.0.1774.42",
            )
            .json(&body)
            .send()
    })
    .await?;
    let raw = response
        .text()
        .await
        .map_err(|e| format!("Bing2 Translate read body: {e}"))?;
    parse_bing_translation(&raw, "Bing2 Translate")
}

/// 调用腾讯云机器翻译 TextTranslate。
pub async fn call_tencent_translate(
    state: &State<'_, AppState>,
    config: &settings::TencentTranslateConfig,
    text: &str,
    target_lang: &str,
    retry_attempts: usize,
) -> Result<String, String> {
    let secret_id = config.secret_id.trim();
    let secret_key = config.secret_key.trim();
    if secret_id.is_empty() || secret_key.is_empty() {
        return Err("Missing Tencent Translate SecretId or SecretKey".to_string());
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }

    let payload = serde_json::json!({
        "SourceText": trimmed,
        "Source": "auto",
        "Target": tencent_translate_lang(target_lang),
        "ProjectId": 0,
    })
    .to_string();
    let timestamp = Utc::now().timestamp();
    let authorization = tencent_translate_signature(secret_id, secret_key, timestamp, &payload);
    let response = send_with_retry("Tencent Translate", retry_attempts, || {
        state
            .http
            .post("https://tmt.tencentcloudapi.com/")
            .header("Authorization", authorization.clone())
            .header("Content-Type", "application/json; charset=utf-8")
            .header("Host", "tmt.tencentcloudapi.com")
            .header("X-TC-Action", "TextTranslate")
            .header("X-TC-Version", "2018-03-21")
            .header("X-TC-Timestamp", timestamp.to_string())
            .header("X-TC-Region", "ap-guangzhou")
            .body(payload.clone())
            .send()
    })
    .await?;
    let raw = response
        .text()
        .await
        .map_err(|e| format!("Tencent Translate read body: {e}"))?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Tencent Translate parse JSON: {e} (body: {})",
            raw.chars().take(500).collect::<String>()
        )
    })?;
    if let Some(error) = value.get("Response").and_then(|v| v.get("Error")) {
        let code = error
            .get("Code")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let message = error
            .get("Message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(format!("Tencent Translate {code}: {message}"));
    }
    value
        .get("Response")
        .and_then(|v| v.get("TargetText"))
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            format!(
                "Invalid Tencent Translate response: {}",
                raw.chars().take(500).collect::<String>()
            )
        })
}

/// 调用 GTranslate.YandexTranslator 使用的 Yandex Android 端接口。
pub async fn call_yandex_translate(
    state: &State<'_, AppState>,
    text: &str,
    target_lang: &str,
    retry_attempts: usize,
) -> Result<String, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }

    let lang = yandex_translate_lang(target_lang);
    let ucid = Uuid::new_v4().simple().to_string();
    let url = format!(
        "https://translate.yandex.net/api/v1/tr.json/translate?ucid={ucid}&srv=android&format=text"
    );
    let form = vec![("text", trimmed.to_string()), ("lang", lang.to_string())];
    let response = send_with_retry("Yandex Translate", retry_attempts, || {
        state
            .http
            .post(url.clone())
            .header("User-Agent", "ru.yandex.translate/3.20.2024")
            .form(&form)
            .send()
    })
    .await?;
    let raw = response
        .text()
        .await
        .map_err(|e| format!("Yandex Translate read body: {e}"))?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Yandex Translate parse JSON: {e} (body: {})",
            raw.chars().take(500).collect::<String>()
        )
    })?;
    if value.get("code").and_then(|v| v.as_u64()) != Some(200) {
        let code = value
            .get("code")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let message = value
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(format!("Yandex Translate {code}: {message}"));
    }
    value
        .get("text")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            format!(
                "Invalid Yandex Translate response: {}",
                raw.chars().take(500).collect::<String>()
            )
        })
}

/// 调用彩云小译 2 密钥版接口。
pub async fn call_caiyun2_translate(
    state: &State<'_, AppState>,
    config: &settings::CaiyunTranslateConfig,
    text: &str,
    target_lang: &str,
    retry_attempts: usize,
) -> Result<String, String> {
    let token = config.token.trim();
    if token.is_empty() {
        return Err("Missing Caiyun Translate token".to_string());
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }

    let trans_type = caiyun_trans_type(target_lang)?;
    let body = serde_json::json!({
        "source": [trimmed],
        "trans_type": trans_type,
        "detect": true,
        "media": "text",
        "request_id": "kivio",
    });
    let response = send_with_retry("Caiyun2 Translate", retry_attempts, || {
        state
            .http
            .post("https://api.interpreter.caiyunai.com/v1/translator")
            .header("x-authorization", format!("token {token}"))
            .json(&body)
            .send()
    })
    .await?;
    let raw = response
        .text()
        .await
        .map_err(|e| format!("Caiyun2 Translate read body: {e}"))?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Caiyun2 Translate parse JSON: {e} (body: {})",
            raw.chars().take(500).collect::<String>()
        )
    })?;
    if let Some(target) = value.get("target") {
        if let Some(text) = target.as_str() {
            return Ok(text.trim().to_string());
        }
        if let Some(text) = target
            .as_array()
            .and_then(|items| items.first())
            .and_then(|v| v.as_str())
        {
            return Ok(text.trim().to_string());
        }
    }
    if let Some(message) = value.get("message").and_then(|v| v.as_str()) {
        return Err(format!("Caiyun2 Translate: {message}"));
    }
    Err(format!(
        "Invalid Caiyun2 Translate response: {}",
        raw.chars().take(500).collect::<String>()
    ))
}

/// 调用 Microsoft Translator 接口。
pub async fn call_microsoft_translate(
    state: &State<'_, AppState>,
    text: &str,
    target_lang: &str,
    retry_attempts: usize,
) -> Result<String, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }

    let to = microsoft_translate_lang(target_lang);
    let url_path =
        format!("api.cognitive.microsofttranslator.com/translate?api-version=3.0&to={to}");
    let url = format!("https://{url_path}");
    let chunks = microsoft_translate_chunks(trimmed);
    let body = chunks
        .iter()
        .map(|chunk| serde_json::json!({ "Text": chunk }))
        .collect::<Vec<_>>();
    let signature = microsoft_translate_signature(&url_path);
    let attempts = retry_attempts.max(1);
    let mut last_error: Option<String> = None;
    let response = {
        let mut response = None;
        for attempt in 1..=attempts {
            match state
                .http
                .post(url.clone())
                .header("X-MT-Signature", signature.clone())
                .header("User-Agent", "okhttp/4.5.0")
                .json(&body)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        response = Some(resp);
                        break;
                    }

                    let retry_after = parse_retry_after(resp.headers());
                    let text = resp.text().await.unwrap_or_default();
                    if status == StatusCode::TOO_MANY_REQUESTS {
                        eprintln!("Microsoft Translate rate limited: {text}");
                        return Err(
                            "Microsoft 翻译接口限流，请稍后重试或切换翻译接口。".to_string()
                        );
                    }

                    let err_msg = format!("Microsoft Translate Error: {} - {}", status, text);
                    if is_retryable_status(status) && attempt < attempts {
                        last_error = Some(err_msg);
                        let delay = retry_delay_ms(attempt, retry_after);
                        eprintln!(
                            "Microsoft Translate retrying in {}ms (attempt {}/{})",
                            delay, attempt, attempts
                        );
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                        continue;
                    }

                    return Err(format!("{} (attempt {}/{})", err_msg, attempt, attempts));
                }
                Err(err) => {
                    let err_msg = format!("Microsoft Translate Error: {}", err);
                    if is_retryable_error(&err) && attempt < attempts {
                        last_error = Some(err_msg);
                        let delay = retry_delay_ms(attempt, None);
                        eprintln!(
                            "Microsoft Translate retrying in {}ms (attempt {}/{})",
                            delay, attempt, attempts
                        );
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                        continue;
                    }
                    return Err(format!("{} (attempt {}/{})", err_msg, attempt, attempts));
                }
            }
        }
        response.ok_or_else(|| {
            last_error
                .map(|msg| format!("{} (attempt {}/{})", msg, attempts, attempts))
                .unwrap_or_else(|| {
                    format!("Microsoft Translate Error: exceeded retry attempts ({attempts})")
                })
        })?
    };
    let raw = response
        .text()
        .await
        .map_err(|e| format!("Microsoft Translate read body: {e}"))?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Microsoft Translate parse JSON: {e} (body: {})",
            raw.chars().take(500).collect::<String>()
        )
    })?;
    if let Some(error) = value.get("error") {
        let code = error
            .get("code")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        if code.trim_matches('"').starts_with("429") {
            return Err("Microsoft 翻译接口限流，请稍后重试或切换翻译接口。".to_string());
        }
        let message = error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(format!("Microsoft Translate {code}: {message}"));
    }

    let translated = value
        .as_array()
        .ok_or_else(|| {
            format!(
                "Invalid Microsoft Translate response: {}",
                raw.chars().take(500).collect::<String>()
            )
        })?
        .iter()
        .filter_map(|item| {
            item.get("translations")
                .and_then(|v| v.as_array())
                .and_then(|items| items.first())
                .and_then(|first| first.get("text"))
                .and_then(|v| v.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok(translated.trim().to_string())
}

/// 调用视觉 API（截图解释 / Lens 共用）
/// 支持流式输出：如果 stream 为 true，通过 stream_vision_response 逐段 emit `event_name` 事件。
/// `provider_id_override` 非空时使用指定 provider/model（用于 lens 选择独立模型）；空则走 explain 配置。
#[allow(clippy::too_many_arguments)]
pub async fn call_vision_api(
    app: &AppHandle,
    state: &State<'_, AppState>,
    image_id: &str,
    messages: Vec<ExplainMessage>,
    language: &str,
    retry_attempts: usize,
    stream: bool,
    stream_kind: &str,
    event_name: &str,
    provider_id_override: Option<&str>,
    model_override: Option<&str>,
    system_prompt_override: Option<&str>,
    thinking_enabled: bool,
    thinking_effort: &str,
    web_search_enabled: bool,
) -> Result<String, String> {
    let settings = state.settings_read().clone();
    let provider_id = provider_id_override
        .filter(|s| !s.is_empty())
        .unwrap_or(&settings.translator_provider_id);
    let provider = settings
        .get_provider(provider_id)
        .ok_or_else(|| "Vision provider not found".to_string())?;
    if provider.base_url == APPLE_INTELLIGENCE_BASE_URL {
        return Err(
            "Apple Intelligence 暂不支持图像输入,请为 Lens / 截图视觉功能配置云端 provider".into(),
        );
    }

    // image_id 为空 → 走纯文本对话路径（不附图）
    let has_image = !image_id.is_empty();

    let mut api_messages = Vec::new();
    // 优先用调用方传入的 system_prompt_override；否则用默认模板（区分有/无图片）
    // 关闭思考时在 system 末尾追加显式禁止指令，作为参数层不生效时的兜底
    let system_prompt_to_use: Option<String> = {
        let base = match system_prompt_override.filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None => default_system_prompt(language, has_image),
        };
        if !thinking_enabled {
            Some(format!("{}{}", base, no_think_instruction(language)))
        } else {
            Some(base)
        }
    };
    if let Some(sp) = system_prompt_to_use {
        api_messages.push(serde_json::json!({
          "role": "system",
          "content": sp
        }));
    }

    if has_image {
        let image_path = resolve_explain_image_path(app, state, image_id)?;
        let bytes = fs::read(image_path).map_err(|e| e.to_string())?;
        let base64 = general_purpose::STANDARD.encode(bytes);
        if let Some(first) = messages.first() {
            api_messages.push(serde_json::json!({
        "role": "user",
        "content": [
          { "type": "image_url", "image_url": { "url": format!("data:image/png;base64,{base64}") } },
          { "type": "text", "text": first.content }
        ]
      }));
            for message in messages.iter().skip(1) {
                api_messages.push(serde_json::json!({
                  "role": message.role,
                  "content": message.content
                }));
            }
        }
    } else {
        // 纯文本：每条 message 直接 push（无图）
        for message in messages.iter() {
            api_messages.push(serde_json::json!({
              "role": message.role,
              "content": message.content,
            }));
        }
    }

    let model = model_override
        .filter(|s| !s.is_empty())
        .unwrap_or(&settings.translator_model);

    if provider_endpoint_kind(&provider.base_url) == ModelEndpointKind::Responses {
        let url = responses_api_url(&provider.base_url);
        let mut body = serde_json::json!({
          "model": model,
          "input": chat_messages_to_responses_input(&serde_json::Value::Array(api_messages.clone())),
          "max_output_tokens": responses_max_output_tokens(2000, thinking_enabled, thinking_effort)
        });
        if stream {
            body["stream"] = serde_json::json!(true);
            body["stream_options"] = serde_json::json!({ "include_obfuscation": false });
        }
        apply_openai_web_search(&mut body, web_search_enabled);
        apply_responses_reasoning(
            &mut body,
            provider,
            model,
            thinking_enabled,
            thinking_effort,
        );

        let response = send_with_failover(
            state,
            "Vision Responses",
            retry_attempts,
            &provider.id,
            &provider.api_keys,
            |key| {
                let request = state.http.post(url.clone()).bearer_auth(key).json(&body);
                let request = if stream {
                    configure_sse_request(request)
                } else {
                    request
                };
                request.send()
            },
        )
        .await?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            let snippet = body_text.chars().take(500).collect::<String>();
            return Err(format!(
                "Vision Responses HTTP {}: {}",
                status.as_u16(),
                snippet
            ));
        }

        if stream {
            let generation = state
                .explain_stream_generation
                .fetch_add(1, Ordering::SeqCst)
                + 1;
            return stream_responses_response(
                app,
                response,
                image_id,
                stream_kind,
                event_name,
                &state.explain_stream_generation,
                generation,
            )
            .await;
        }

        let raw = response
            .text()
            .await
            .map_err(|e| format!("Vision Responses read body: {}", e))?;
        let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
            format!(
                "Vision Responses parse JSON: {} (body: {})",
                e,
                raw.chars().take(500).collect::<String>()
            )
        })?;
        return parse_response_output_text(&raw, &value, "vision");
    }

    let url = chat_completions_url(&provider.base_url);
    let mut body = serde_json::json!({
      "model": model,
      "messages": api_messages,
      "temperature": 0.7,
      "max_tokens": 2000
    });
    if stream {
        body["stream"] = serde_json::json!(true);
    }

    apply_chat_reasoning(
        &mut body,
        provider,
        model,
        thinking_enabled,
        thinking_effort,
    );

    let response = send_with_failover(
        state,
        "Vision API",
        retry_attempts,
        &provider.id,
        &provider.api_keys,
        |key| {
            let request = state.http.post(url.clone()).bearer_auth(key).json(&body);
            let request = if stream {
                configure_sse_request(request)
            } else {
                request
            };
            request.send()
        },
    )
    .await?;

    // 先检查 HTTP 状态：非 2xx 直接读出 body 文本作为错误，避免后续 .json() / chunk() 拿到非预期格式时抛出含糊的 "error decoding response body"。
    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        let snippet = body_text.chars().take(500).collect::<String>();
        return Err(format!("Vision API HTTP {}: {}", status.as_u16(), snippet));
    }

    if stream {
        // 启动新流：递增代号，存到本流持有的快照里；后续 chunk 循环只要发现全局代号 != 自己的快照就退出。
        let generation = state
            .explain_stream_generation
            .fetch_add(1, Ordering::SeqCst)
            + 1;
        return stream_vision_response(
            app,
            response,
            image_id,
            stream_kind,
            event_name,
            &state.explain_stream_generation,
            generation,
        )
        .await;
    }

    // 非流式：先读 raw text，再 parse JSON，把原始 body 作为错误信息便于诊断。
    let raw = response
        .text()
        .await
        .map_err(|e| format!("Vision API read body: {}", e))?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Vision API parse JSON: {} (body: {})",
            e,
            raw.chars().take(500).collect::<String>()
        )
    })?;
    let content = value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .ok_or_else(|| {
            format!(
                "Invalid vision response: {}",
                raw.chars().take(500).collect::<String>()
            )
        })?;

    Ok(content.trim().to_string())
}

// ===== SSE 流 =====

fn stream_text_suffix(full: &str, text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else if full.is_empty() {
        text.to_string()
    } else if text.starts_with(full) {
        text[full.len()..].to_string()
    } else {
        String::new()
    }
}

fn split_stream_chunks(text: &str) -> Vec<&str> {
    let char_count = text.chars().count();
    if char_count <= STREAM_FALLBACK_CHUNK_CHARS {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    let mut count = 0;
    for (idx, _) in text.char_indices() {
        if count >= STREAM_FALLBACK_CHUNK_CHARS {
            chunks.push(&text[start..idx]);
            start = idx;
            count = 0;
        }
        count += 1;
    }
    if start < text.len() {
        chunks.push(&text[start..]);
    }
    chunks
}

async fn emit_responses_text_delta(
    app: &AppHandle,
    event_name: &str,
    image_id: &str,
    kind: &str,
    full: &mut String,
    delta: &str,
) {
    if delta.is_empty() {
        return;
    }

    let chunks = split_stream_chunks(delta);
    let multi_chunk = chunks.len() > 1;
    for chunk in chunks {
        full.push_str(chunk);
        let _ = app.emit(
            event_name,
            serde_json::json!({
              "imageId": image_id,
              "kind": kind,
              "delta": chunk
            }),
        );
        if multi_chunk {
            tokio::time::sleep(Duration::from_millis(STREAM_FALLBACK_CHUNK_DELAY_MS)).await;
        } else {
            tokio::task::yield_now().await;
        }
    }
}

async fn emit_responses_reasoning_delta(
    app: &AppHandle,
    event_name: &str,
    image_id: &str,
    kind: &str,
    reasoning_full: &mut String,
    delta: &str,
) {
    if delta.is_empty() {
        return;
    }

    let chunks = split_stream_chunks(delta);
    let multi_chunk = chunks.len() > 1;
    for chunk in chunks {
        reasoning_full.push_str(chunk);
        let _ = app.emit(
            event_name,
            serde_json::json!({
              "imageId": image_id,
              "kind": kind,
              "delta": "",
              "reasoningDelta": chunk,
            }),
        );
        if multi_chunk {
            tokio::time::sleep(Duration::from_millis(STREAM_FALLBACK_CHUNK_DELAY_MS)).await;
        } else {
            tokio::task::yield_now().await;
        }
    }
}

/// 通用流式 chat 调用：发送 body（model 在外层注入）→ 解析 SSE → 通过 stream_vision_response emit。
/// 复用 explain_stream_generation 作取消代号（lens-stream / lens-translate-stream 都共用）。
#[allow(clippy::too_many_arguments)]
pub async fn stream_chat_call(
    app: &AppHandle,
    state: &State<'_, AppState>,
    provider: &settings::ModelProvider,
    model: &str,
    mut body: serde_json::Value,
    retry_attempts: usize,
    image_id: &str,
    kind: &str,
    event_name: &str,
    thinking_enabled: bool,
    thinking_effort: &str,
) -> Result<String, String> {
    if provider.base_url == APPLE_INTELLIGENCE_BASE_URL {
        let _ = (
            app,
            state,
            model,
            &mut body,
            retry_attempts,
            image_id,
            kind,
            event_name,
            thinking_enabled,
            thinking_effort,
        );
        return Err("Apple Intelligence 暂不支持图像输入,请为截图翻译配置云端 provider".into());
    }
    body["model"] = serde_json::json!(model);

    if provider_endpoint_kind(&provider.base_url) == ModelEndpointKind::Responses {
        let input = body.get("input").cloned().unwrap_or_else(|| {
            chat_messages_to_responses_input(
                body.get("messages").unwrap_or(&serde_json::Value::Null),
            )
        });
        let mut responses_body = serde_json::json!({
          "model": model,
          "input": input,
          "stream": true,
          "stream_options": { "include_obfuscation": false },
          "max_output_tokens": responses_max_output_tokens(
              response_max_output_tokens_from_body(&body, 2000),
              thinking_enabled,
              thinking_effort
          )
        });
        apply_openai_web_search(&mut responses_body, true);
        apply_responses_reasoning(
            &mut responses_body,
            provider,
            model,
            thinking_enabled,
            thinking_effort,
        );

        let url = responses_api_url(&provider.base_url);
        let response = send_with_failover(
            state,
            "Stream Responses",
            retry_attempts,
            &provider.id,
            &provider.api_keys,
            |key| {
                configure_sse_request(
                    state
                        .http
                        .post(url.clone())
                        .bearer_auth(key)
                        .json(&responses_body),
                )
                .send()
            },
        )
        .await?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            let snippet: String = body_text.chars().take(500).collect();
            return Err(format!(
                "Stream Responses HTTP {}: {}",
                status.as_u16(),
                snippet
            ));
        }

        let generation = state
            .explain_stream_generation
            .fetch_add(1, Ordering::SeqCst)
            + 1;
        return stream_responses_response(
            app,
            response,
            image_id,
            kind,
            event_name,
            &state.explain_stream_generation,
            generation,
        )
        .await;
    }

    let url = chat_completions_url(&provider.base_url);
    apply_chat_reasoning(
        &mut body,
        provider,
        model,
        thinking_enabled,
        thinking_effort,
    );

    let response = send_with_failover(
        state,
        "Stream chat",
        retry_attempts,
        &provider.id,
        &provider.api_keys,
        |key| {
            configure_sse_request(state.http.post(url.clone()).bearer_auth(key).json(&body)).send()
        },
    )
    .await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        let snippet: String = body_text.chars().take(500).collect();
        return Err(format!("Stream HTTP {}: {}", status.as_u16(), snippet));
    }

    let generation = state
        .explain_stream_generation
        .fetch_add(1, Ordering::SeqCst)
        + 1;
    stream_vision_response(
        app,
        response,
        image_id,
        kind,
        event_name,
        &state.explain_stream_generation,
        generation,
    )
    .await
}

/// 流式解析 OpenAI Responses API 的 SSE 响应。
/// 透传最终文本 delta；Responses 推理摘要 / 工具状态作为 reasoningDelta 透给前端，避免长搜索时界面长时间无变化。
pub async fn stream_responses_response(
    app: &AppHandle,
    mut response: reqwest::Response,
    image_id: &str,
    kind: &str,
    event_name: &str,
    generation_atom: &AtomicU64,
    my_generation: u64,
) -> Result<String, String> {
    let mut buffer = String::new();
    let mut full = String::new();
    let mut reasoning_full = String::new();
    let mut web_search_notice_emitted = false;
    let mut sse_event_type = String::new();

    let emit_done = |reason: &str, full_text: &str| {
        let _ = app.emit(
            event_name,
            serde_json::json!({
              "imageId": image_id,
              "kind": kind,
              "delta": "",
              "done": true,
              "reason": reason,
              "full": full_text,
            }),
        );
    };

    loop {
        if generation_atom.load(Ordering::SeqCst) != my_generation {
            emit_done("cancelled", full.trim());
            return Ok(full.trim().to_string());
        }

        let chunk = loop {
            if generation_atom.load(Ordering::SeqCst) != my_generation {
                emit_done("cancelled", full.trim());
                return Ok(full.trim().to_string());
            }

            tokio::select! {
                result = response.chunk() => {
                    match result {
                        Ok(Some(c)) => break c,
                        Ok(None) => {
                            emit_done("done", full.trim());
                            return Ok(full.trim().to_string());
                        }
                        Err(e) => {
                            emit_done("error", full.trim());
                            return Err(format!(
                                "Responses stream read body: {}",
                                reqwest_error_details(&e)
                            ));
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(STREAM_CANCEL_POLL_MS)) => {}
            }
        };

        let text = String::from_utf8_lossy(&chunk);
        buffer.push_str(&text);

        while let Some(pos) = buffer.find('\n') {
            let line: String = buffer.drain(..=pos).collect();
            let line = line.trim();
            if line.is_empty() {
                sse_event_type.clear();
                continue;
            }
            if let Some(event) = line.strip_prefix("event:") {
                sse_event_type = event.trim().to_string();
                continue;
            }
            if !line.starts_with("data:") {
                continue;
            }
            let data = line.trim_start_matches("data:").trim();
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                emit_done("done", full.trim());
                return Ok(full.trim().to_string());
            }

            let value: serde_json::Value = match serde_json::from_str(data) {
                Ok(val) => val,
                Err(_) => continue,
            };
            let event_type_owned = value
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or(sse_event_type.as_str())
                .to_string();
            sse_event_type.clear();
            let event_type = event_type_owned.as_str();

            if !web_search_notice_emitted {
                let item_type = value
                    .get("item")
                    .and_then(|item| item.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if event_type.contains("web_search") || item_type.contains("web_search") {
                    web_search_notice_emitted = true;
                    emit_responses_reasoning_delta(
                        app,
                        event_name,
                        image_id,
                        kind,
                        &mut reasoning_full,
                        "正在联网搜索...\n",
                    )
                    .await;
                }
            }

            match event_type {
                event if event.contains("reasoning") && event.ends_with(".delta") => {
                    if let Some(delta) =
                        response_stream_delta_text(&value).filter(|s| !s.is_empty())
                    {
                        emit_responses_reasoning_delta(
                            app,
                            event_name,
                            image_id,
                            kind,
                            &mut reasoning_full,
                            &delta,
                        )
                        .await;
                    }
                }
                event if event.contains("reasoning") && event.ends_with(".done") => {
                    if let Some(text) = response_stream_done_text(&value) {
                        let delta = stream_text_suffix(&reasoning_full, &text);
                        emit_responses_reasoning_delta(
                            app,
                            event_name,
                            image_id,
                            kind,
                            &mut reasoning_full,
                            &delta,
                        )
                        .await;
                    }
                }
                "response.output_text.delta" | "response.text.delta" => {
                    if let Some(delta) =
                        response_stream_delta_text(&value).filter(|s| !s.is_empty())
                    {
                        emit_responses_text_delta(
                            app, event_name, image_id, kind, &mut full, &delta,
                        )
                        .await;
                    }
                }
                "response.output_text.done" | "response.text.done" => {
                    if let Some(text) = response_stream_done_text(&value) {
                        let delta = stream_text_suffix(&full, &text);
                        emit_responses_text_delta(
                            app, event_name, image_id, kind, &mut full, &delta,
                        )
                        .await;
                    }
                }
                "response.content_part.done" | "response.output_item.done" => {
                    if let Some(text) = response_stream_done_text(&value) {
                        let delta = stream_text_suffix(&full, &text);
                        emit_responses_text_delta(
                            app, event_name, image_id, kind, &mut full, &delta,
                        )
                        .await;
                    }
                }
                "response.completed" => {
                    if let Some(reason) = response_incomplete_reason(&value) {
                        emit_done("error", full.trim());
                        return Err(format!(
                            "Responses stream incomplete: {reason}. 当前思考/搜索可能耗尽了输出 token，请降低思考强度或稍后重试。"
                        ));
                    }
                    if full.trim().is_empty() {
                        if let Some(text) = value.get("response").and_then(response_output_text) {
                            emit_responses_text_delta(
                                app, event_name, image_id, kind, &mut full, &text,
                            )
                            .await;
                        }
                    }
                    emit_done("done", full.trim());
                    return Ok(full.trim().to_string());
                }
                "response.incomplete" => {
                    let reason =
                        response_incomplete_reason(&value).unwrap_or_else(|| "unknown".to_string());
                    emit_done("error", full.trim());
                    return Err(format!(
                        "Responses stream incomplete: {reason}. 当前思考/搜索可能耗尽了输出 token，请降低思考强度或稍后重试。"
                    ));
                }
                "response.failed" | "error" => {
                    let message = responses_stream_error(&value);
                    emit_done("error", full.trim());
                    return Err(format!("Responses stream error: {message}"));
                }
                _ => {}
            }
        }
    }
}

/// 流式解析视觉 API 的 SSE 响应
/// 逐 chunk 读取响应体，解析 "data:" 行，提取 delta 中的 content 并通过 `event_name` emit。
/// 支持取消：调用方持有 `my_generation`，全局代号 `generation_atom` 一旦变化即视为被新流或外部取消作废。
pub async fn stream_vision_response(
    app: &AppHandle,
    mut response: reqwest::Response,
    image_id: &str,
    kind: &str,
    event_name: &str,
    generation_atom: &AtomicU64,
    my_generation: u64,
) -> Result<String, String> {
    let mut buffer = String::new();
    let mut full = String::new();

    let emit_done = |reason: &str, full_text: &str| {
        let _ = app.emit(
            event_name,
            serde_json::json!({
              "imageId": image_id,
              "kind": kind,
              "delta": "",
              "done": true,
              "reason": reason,
              "full": full_text,
            }),
        );
    };

    loop {
        if generation_atom.load(Ordering::SeqCst) != my_generation {
            emit_done("cancelled", full.trim());
            return Ok(full.trim().to_string());
        }

        let chunk = match response.chunk().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => {
                emit_done("error", full.trim());
                return Err(format!("Stream read body: {}", reqwest_error_details(&e)));
            }
        };

        let text = String::from_utf8_lossy(&chunk);
        buffer.push_str(&text);

        while let Some(pos) = buffer.find('\n') {
            let line: String = buffer.drain(..=pos).collect();
            let line = line.trim();
            if !line.starts_with("data:") {
                continue;
            }
            let data = line.trim_start_matches("data:").trim();
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                emit_done("done", full.trim());
                return Ok(full.trim().to_string());
            }

            let value: serde_json::Value = match serde_json::from_str(data) {
                Ok(val) => val,
                Err(_) => continue,
            };

            let delta_obj = value
                .get("choices")
                .and_then(|choices| choices.get(0))
                .and_then(|choice| choice.get("delta"));

            // 推理模型（DeepSeek-R1 / Kimi 等）把链路放在 delta.reasoning_content
            // 部分实现用 delta.reasoning。两种字段都尝试取，只要有就 emit。
            let reasoning = delta_obj
                .and_then(|d| d.get("reasoning_content").or_else(|| d.get("reasoning")))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());

            if let Some(r) = reasoning {
                let _ = app.emit(
                    event_name,
                    serde_json::json!({
                      "imageId": image_id,
                      "kind": kind,
                      "delta": "",
                      "reasoningDelta": r,
                    }),
                );
            }

            let content = delta_obj
                .and_then(|d| d.get("content"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());

            if let Some(content) = content {
                full.push_str(content);
                let _ = app.emit(
                    event_name,
                    serde_json::json!({ "imageId": image_id, "kind": kind, "delta": content }),
                );
            }
        }
    }

    emit_done("done", full.trim());
    Ok(full.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_endpoint_kind_uses_explicit_endpoint_path() {
        assert_eq!(
            provider_endpoint_kind("https://api.example.com/v1/responses"),
            ModelEndpointKind::Responses
        );
        assert_eq!(
            provider_endpoint_kind("https://api.example.com/v1/chat/completions"),
            ModelEndpointKind::ChatCompletions
        );
        assert_eq!(
            provider_endpoint_kind("https://api.openai.com/v1"),
            ModelEndpointKind::LegacyBase
        );
    }

    #[test]
    fn provider_url_helpers_keep_full_endpoint_or_build_legacy_chat_url() {
        assert_eq!(
            responses_api_url("https://api.example.com/v1/responses"),
            "https://api.example.com/v1/responses"
        );
        assert_eq!(
            chat_completions_url("https://api.example.com/v1/chat/completions"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://api.example.com/v1"),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn models_url_from_provider_url_strips_known_model_endpoint_suffix() {
        assert_eq!(
            models_url_from_provider_url("https://api.example.com/v1/responses"),
            "https://api.example.com/v1/models"
        );
        assert_eq!(
            models_url_from_provider_url("https://api.example.com/v1/chat/completions"),
            "https://api.example.com/v1/models"
        );
        assert_eq!(
            models_url_from_provider_url("https://api.example.com/v1"),
            "https://api.example.com/v1/models"
        );
    }

    #[test]
    fn chat_messages_to_responses_input_maps_text_and_images() {
        let messages = serde_json::json!([
          { "role": "system", "content": "Be concise" },
          {
            "role": "user",
            "content": [
              { "type": "image_url", "image_url": { "url": "data:image/png;base64,abc" } },
              { "type": "text", "text": "Read this" }
            ]
          }
        ]);

        let input = chat_messages_to_responses_input(&messages);
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["content"][0]["type"], "input_image");
        assert_eq!(input[1]["content"][1]["type"], "input_text");
    }

    #[test]
    fn chat_messages_to_responses_input_maps_assistant_history_to_output_text() {
        let messages = serde_json::json!([
          { "role": "user", "content": "first question" },
          { "role": "assistant", "content": "first answer" },
          { "role": "user", "content": "follow up" }
        ]);

        let input = chat_messages_to_responses_input(&messages);
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["content"][0]["text"], "first answer");
        assert_eq!(input[2]["content"][0]["type"], "input_text");
    }

    #[test]
    fn response_output_text_collects_message_content() {
        let response = serde_json::json!({
          "output": [
            {
              "type": "message",
              "content": [
                { "type": "output_text", "text": "hello" },
                { "type": "output_text", "text": " world" }
              ]
            }
          ]
        });

        assert_eq!(
            response_output_text(&response),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn google_translate_helpers_match_gtx_response_shape() {
        assert_eq!(google_translate_lang("zh"), "zh-CN");
        assert_eq!(google_translate_lang("zh-Hant"), "zh-TW");
        assert_eq!(google_translate_lang("ja"), "ja");
        assert!(google_translate_token_for_hour("Hello", 493_000).contains('.'));

        let raw = r#"{
          "sentences": [
            { "trans": "你好", "orig": "Hello" },
            { "trans": "世界", "orig": " world" }
          ],
          "src": "en",
          "confidence": 1
        }"#;

        assert_eq!(
            google_translate_response_text(raw).unwrap(),
            "你好世界".to_string()
        );
    }

    #[test]
    fn chaoxing_ocr_lines_extract_nested_text() {
        let response = serde_json::json!({
            "data": [[
                { "text": "第一行" },
                { "text": " second line " },
                { "text": "" }
            ]]
        });

        assert_eq!(chaoxing_ocr_lines(&response), vec!["第一行", "second line"]);
    }

    #[test]
    fn bing_translate_helpers_parse_tianruo_response_shapes() {
        let html = r#"
          <script>
            IG:"0123456789abcdef0123456789abcdef";
            var params_AbusePreventionHelper = [123456,"token-value",999999];
          </script>
        "#;
        let (key, token, ig) = parse_bing_credentials(html).unwrap();
        assert_eq!(key, "123456");
        assert_eq!(token, "token-value");
        assert_eq!(ig, "0123456789abcdef0123456789abcdef");

        let raw = r#"[{"translations":[{"text":"你好世界"}]}]"#;
        assert_eq!(
            parse_bing_translation(raw, "Bing Translate").unwrap(),
            "你好世界"
        );
    }

    #[test]
    fn caiyun2_trans_type_matches_supported_auto_targets() {
        assert_eq!(caiyun_trans_type("zh").unwrap(), "auto2zh");
        assert_eq!(caiyun_trans_type("zh-Hant").unwrap(), "auto2zh-Hant");
        assert_eq!(caiyun_trans_type("en").unwrap(), "auto2en");
        assert_eq!(caiyun_trans_type("ja").unwrap(), "auto2ja");
        assert!(caiyun_trans_type("de").is_err());
    }

    #[test]
    fn baidu_tts_helpers_choose_language_and_encode_text() {
        assert_eq!(baidu_tts_lang("你好世界"), "zh");
        assert_eq!(baidu_tts_lang("hello world"), "en");
        assert_eq!(baidu_tts_url_encode("hello world"), "hello%20world");
        assert_eq!(
            baidu_tts_url_encode("你好"),
            "%E4%BD%A0%E5%A5%BD".to_string()
        );
    }

    // ===== extract_status_code =====

    #[test]
    fn extract_status_code_parses_typical_send_with_retry_format() {
        // send_with_retry 拼出来的标准格式
        let s = "OpenAI API Error: 429 Too Many Requests - {\"error\":\"rate_limit\"}";
        assert_eq!(extract_status_code(s), Some(429));
    }

    #[test]
    fn extract_status_code_handles_each_failover_status() {
        assert_eq!(
            extract_status_code("X Error: 401 Unauthorized - body"),
            Some(401)
        );
        assert_eq!(
            extract_status_code("X Error: 402 Payment Required - body"),
            Some(402)
        );
        assert_eq!(
            extract_status_code("X Error: 403 Forbidden - body"),
            Some(403)
        );
        assert_eq!(
            extract_status_code("X Error: 429 Too Many Requests - body"),
            Some(429)
        );
    }

    #[test]
    fn extract_status_code_handles_defensive_http_format() {
        assert_eq!(
            extract_status_code("Stream HTTP 429: rate limited"),
            Some(429)
        );
        assert_eq!(
            extract_status_code("Stream HTTP 401: unauthorized"),
            Some(401)
        );
        assert_eq!(
            extract_status_code("Vision API HTTP 403: forbidden"),
            Some(403)
        );
    }

    #[test]
    fn extract_status_code_handles_non_failover_status() {
        assert_eq!(
            extract_status_code("X Error: 400 Bad Request - body"),
            Some(400)
        );
        assert_eq!(
            extract_status_code("X Error: 500 Internal Server Error - body"),
            Some(500)
        );
    }

    #[test]
    fn extract_status_code_returns_none_for_network_error() {
        // reqwest::Error 路径无前导数字
        let s = "Stream chat Error: error sending request: connection refused (attempt 3/3)";
        assert_eq!(extract_status_code(s), None);
    }

    #[test]
    fn extract_status_code_returns_none_when_marker_missing() {
        assert_eq!(extract_status_code("just some message"), None);
        assert_eq!(extract_status_code(""), None);
    }

    // ===== is_failover_error =====

    #[test]
    fn is_failover_error_only_triggers_on_auth_quota_codes() {
        assert!(is_failover_error("X Error: 401 - body"));
        assert!(is_failover_error("X Error: 402 - body"));
        assert!(is_failover_error("X Error: 403 - body"));
        assert!(is_failover_error("X Error: 429 - body"));
        assert!(is_failover_error("Stream HTTP 429: rate limited"));
        assert!(is_failover_error("Stream HTTP 401: unauthorized"));
    }

    #[test]
    fn is_failover_error_does_not_trigger_on_400_or_5xx() {
        // 400 是请求 body 问题，不应换 key
        assert!(!is_failover_error("X Error: 400 Bad Request - body"));
        assert!(!is_failover_error("Stream HTTP 400: bad request"));
        // 500 由 send_with_retry 内部退避重试，不应到 failover 层
        assert!(!is_failover_error(
            "X Error: 500 Internal Server Error - body"
        ));
        assert!(!is_failover_error(
            "X Error: 503 Service Unavailable - body"
        ));
    }

    #[test]
    fn is_failover_error_does_not_trigger_on_network_failure() {
        // 网络问题不是 key 的锅
        assert!(!is_failover_error(
            "Stream Error: error sending request: timed out"
        ));
        assert!(!is_failover_error("X Error: connection closed"));
    }

    #[test]
    fn is_failover_error_does_not_trigger_on_body_keywords_alone() {
        // 旧版宽泛匹配 body 含 "billing" / "quota" 会误触发；现版严格按状态码
        assert!(!is_failover_error(
            "X Error: 400 - {\"message\":\"billing issue\"}"
        ));
        assert!(!is_failover_error(
            "X Error: 500 - {\"message\":\"quota exceeded\"}"
        ));
    }
}
