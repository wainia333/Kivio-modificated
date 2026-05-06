//! 翻译 / 截图翻译 / OCR 提示词模板与构建器。
//!
//! 所有提示词在 OpenAI 兼容 API 调用前由调用方拼接好（`api.rs` 不直接构建 prompt），
//! 这样 prompt 模板的演进与 HTTP 客户端解耦，前端 Settings 也能 reuse 同一组默认值
//! （通过 `get_default_prompt_templates` 命令暴露给前端）。

/// 默认翻译提示词模板
pub const DEFAULT_TRANSLATION_TEMPLATE: &str =
  "Translate the following text to {lang}. Output only the translation.\n\nRules:\n- Preserve existing LaTeX formulas exactly (keep $...$ and $$...$$).\n- If formula-like plain text appears, normalize it to proper LaTeX when needed.\n- Keep the original line breaks and list structure when possible.\n- Do not add explanations.\n\n{text}";

/// 默认截图翻译提示词模板
pub const DEFAULT_SCREENSHOT_TRANSLATION_TEMPLATE: &str =
  "Translate the OCR text below to {lang}. Output only the translation.\n\nRules:\n- Preserve existing LaTeX formulas exactly (keep $...$ and $$...$$).\n- If formula-like plain text appears, normalize it to proper LaTeX when needed.\n- Keep paragraph and line-break structure from OCR text when possible.\n- Correct only obvious OCR character mistakes; do not invent missing content.\n- Do not add explanations.\n\n{text}";

/// AI OCR 专用提示词：只让视觉模型做识别，不夹带翻译。
pub const DEFAULT_SCREENSHOT_OCR_PROMPT: &str =
  "Read all text in this screenshot and output only the recognized text.\n\nRules:\n- Do not translate or explain.\n- Reconstruct natural paragraphs: merge visual line wraps inside the same paragraph.\n- Separate real paragraphs with one blank line.\n- Preserve intentional line breaks for lists, tables, code, math formulas, and UI labels.\n- Preserve punctuation and symbols exactly.\n- Do not wrap the result in Markdown code fences.\n- If no text is visible, output an empty string.";

/// 使用模板构建提示词
/// 支持 {text} 和 {lang} 占位符；如果自定义模板为空或不含 {text}，则追加文本内容
pub fn build_prompt_with_template(
    text: &str,
    lang_name: &str,
    template: Option<&str>,
    default_template: &str,
) -> String {
    let default_prompt = default_template
        .replace("{lang}", lang_name)
        .replace("{text}", text);

    let Some(template) = template else {
        return default_prompt;
    };
    let trimmed = template.trim();
    if trimmed.is_empty() {
        return default_prompt;
    }

    let mut prompt = trimmed.replace("{text}", text).replace("{lang}", lang_name);
    if !trimmed.contains("{text}") {
        prompt = format!("{prompt}\n\n{text}");
    }
    prompt
}

/// 构建普通翻译提示词
pub fn build_translation_prompt(text: &str, lang_name: &str, template: Option<&str>) -> String {
    build_prompt_with_template(text, lang_name, template, DEFAULT_TRANSLATION_TEMPLATE)
}

/// 构建截图翻译提示词
pub fn build_screenshot_translation_prompt(
    text: &str,
    lang_name: &str,
    template: Option<&str>,
) -> String {
    build_prompt_with_template(
        text,
        lang_name,
        template,
        DEFAULT_SCREENSHOT_TRANSLATION_TEMPLATE,
    )
}
