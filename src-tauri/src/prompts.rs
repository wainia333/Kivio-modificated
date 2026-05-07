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
  "Read all text in this screenshot and output only the recognized content as copy-ready Markdown.\n\nRules:\n- Do not translate, summarize, explain, or add content that is not visible.\n- Reconstruct natural paragraphs: merge visual line wraps inside the same paragraph.\n- Separate real paragraphs with one blank line.\n- Preserve document structure as Markdown when visible: headings, ordered lists, unordered lists, nested lists, block quotes, tables, code blocks, inline code, links, emphasis, bold, italic, strikethrough, and UI labels.\n- Preserve intentional line breaks for lists, tables, code, mathematical formulas, captions, and UI labels.\n- Output mathematical formulas in LaTeX: use $...$ for inline formulas and $$...$$ for standalone/display formulas.\n- Normalize fractions, superscripts, subscripts, roots, integrals, sums, matrices, Greek letters, and other mathematical symbols to proper LaTeX when they appear in formulas.\n- Preserve non-formula punctuation and symbols exactly.\n- Do not invent Markdown styling when the visual evidence is unclear.\n- Do not wrap the whole result in Markdown code fences; use code fences only for visible code blocks.\n- If no text is visible, output an empty string.";

pub fn default_prompt_optimizer_system_prompt(language: &str) -> String {
    if language.starts_with("zh") {
        "你是一个严谨、务实的提示词优化专家。你的目标不是把提示词写得更长，而是让模型更容易稳定地产出符合用户意图的结果。保留用户原始意图、关键约束、语气和必要变量；删除含糊、重复、相互冲突或不可执行的表达；补足角色、任务、上下文、输出格式、质量标准和边界条件。不要编造业务事实。".to_string()
    } else {
        "You are a pragmatic prompt optimization expert. Your goal is not to make prompts longer, but to make them clearer, more controllable, and easier for a model to follow. Preserve the user's intent, constraints, tone, and variables; remove ambiguity, repetition, conflicts, and unenforceable wording; add role, task, context, output format, quality criteria, and boundaries when useful. Do not invent domain facts.".to_string()
    }
}

pub fn default_prompt_optimizer_template(language: &str) -> String {
    if language.starts_with("zh") {
        "请优化下面的原始提示词，并使用 {lang} 输出。\n\n优化原则：\n- 先判断任务类型和目标用户，不盲目套模板。\n- 保留原始意图、硬性约束、变量占位符、输入输出字段和语气。\n- 将含糊要求改写为可执行的步骤、判断标准和输出格式。\n- 补足必要上下文、角色边界、禁止事项、异常处理和质量检查。\n- 如果原提示词已经足够清晰，只做轻量整理。\n- 不要添加与原任务无关的能力、工具、背景或事实。\n\n输出格式：\n## 优化后的提示词\n给出可直接复制使用的完整提示词。\n\n## 调整要点\n用 3-6 条短要点说明主要改动。\n\n原始提示词：\n{text}".to_string()
    } else {
        "Optimize the raw prompt below and respond in {lang}.\n\nOptimization principles:\n- Identify the task type and target user first; do not force a generic template.\n- Preserve intent, hard constraints, variables, input/output fields, and tone.\n- Rewrite vague requirements into executable steps, decision criteria, and output format.\n- Add only useful context, role boundaries, prohibitions, edge-case handling, and quality checks.\n- If the prompt is already clear, keep changes light.\n- Do not add unrelated capabilities, tools, background, or facts.\n\nOutput format:\n## Optimized Prompt\nProvide the complete copy-ready prompt.\n\n## Changes Made\nExplain the main changes in 3-6 concise bullets.\n\nRaw prompt:\n{text}".to_string()
    }
}

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

pub fn build_prompt_optimizer_prompt(
    text: &str,
    lang_name: &str,
    language: &str,
    system_prompt: Option<&str>,
    optimize_prompt: Option<&str>,
) -> String {
    let system = system_prompt
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| default_prompt_optimizer_system_prompt(language));
    let template = default_prompt_optimizer_template(language);
    let user_prompt = build_prompt_with_template(text, lang_name, optimize_prompt, &template);
    format!("{system}\n\n{user_prompt}")
}
