pub fn resolve_target_lang(target: &str, text: &str) -> String {
    if target == "auto" {
        if has_chinese(text) {
            "en".to_string()
        } else {
            "zh".to_string()
        }
    } else {
        target.to_string()
    }
}

pub fn has_chinese(text: &str) -> bool {
    text.chars().any(|c| ('\u{4e00}'..'\u{9fff}').contains(&c))
}

pub fn language_name(code: &str) -> &'static str {
    match code {
        "zh" | "zh-Hans" => "Simplified Chinese",
        "zh-Hant" => "Traditional Chinese",
        "en" => "English",
        "ja" => "Japanese",
        "ko" => "Korean",
        "fr" => "French",
        "de" => "German",
        _ => "English",
    }
}
