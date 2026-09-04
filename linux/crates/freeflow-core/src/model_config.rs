//! Per-model request tuning for OpenAI-compatible providers.
//!
//! Ported from `Sources/ModelConfiguration.swift`. The Swift original spells
//! out every known model in a long if/else chain, but only three of them
//! differ from the default; the rest return the same all-empty config as an
//! unknown model. That chain is collapsed here into the three real cases plus
//! a default, which is behaviourally identical and covered by the ported
//! tests.

/// Request knobs that vary by model. `None` means "omit the field entirely",
/// which is not the same as sending a default value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelConfig {
    pub max_completion_tokens: Option<u32>,
    pub reasoning_effort: Option<&'static str>,
    pub include_reasoning: Option<bool>,
    /// Some reasoning models emit `<think>...</think>` in the content field.
    pub should_strip_think_tags: bool,
}

pub const LLM_MODELS: &[&str] = &[
    "openai/gpt-oss-20b",
    "openai/gpt-oss-120b",
    "openai/gpt-oss-safeguard-20b",
    "qwen/qwen3.6-27b",
    "groq/compound",
    "groq/compound-mini",
];

/// Models that accept image input. Retained for provider compatibility even
/// though the Linux build does not capture screenshots.
pub const VISION_MODELS: &[&str] = &["qwen/qwen3.6-27b"];

pub const TRANSCRIPTION_MODELS: &[&str] = &["whisper-large-v3", "whisper-large-v3-turbo"];

/// Expands providerless aliases so `gpt-oss-20b` and `openai/gpt-oss-20b`
/// resolve to the same configuration.
fn canonical_model_name(model: &str) -> String {
    let clean = model.trim().to_lowercase();
    match clean.as_str() {
        "qwen3-32b" => "qwen/qwen3-32b".to_string(),
        "qwen3.6-27b" => "qwen/qwen3.6-27b".to_string(),
        "gpt-oss-20b" => "openai/gpt-oss-20b".to_string(),
        "gpt-oss-120b" => "openai/gpt-oss-120b".to_string(),
        "gpt-oss-safeguard-20b" => "openai/gpt-oss-safeguard-20b".to_string(),
        _ => clean,
    }
}

pub fn config_for(model: &str) -> ModelConfig {
    match canonical_model_name(model).as_str() {
        "openai/gpt-oss-20b" => ModelConfig {
            max_completion_tokens: Some(4096),
            reasoning_effort: Some("low"),
            include_reasoning: Some(false),
            should_strip_think_tags: false,
        },
        "qwen/qwen3-32b" => ModelConfig {
            should_strip_think_tags: true,
            ..Default::default()
        },
        "qwen/qwen3.6-27b" => ModelConfig {
            max_completion_tokens: None,
            reasoning_effort: Some("none"),
            include_reasoning: Some(false),
            should_strip_think_tags: true,
        },
        _ => ModelConfig::default(),
    }
}

/// Removes leading `<think>...</think>` blocks, including an unclosed one left
/// behind when the model runs out of completion tokens mid-thought.
///
/// Both patterns are anchored at the start deliberately: a `<think>` appearing
/// later in the text is ordinary content the user dictated, not reasoning.
pub fn strip_think_tags(text: &str) -> String {
    use std::sync::LazyLock;

    use regex::Regex;

    static CLOSED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)^(?:\s*<think>.*?</think>)+").expect("valid pattern"));
    static UNCLOSED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)^\s*<think>.*$").expect("valid pattern"));

    let cleaned = CLOSED.replace(text, "");
    let cleaned = UNCLOSED.replace(&cleaned, "");
    cleaned.trim().to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn assert_same_config(alias: &str, canonical: &str) {
        assert_eq!(
            config_for(alias),
            config_for(canonical),
            "{alias} should resolve to {canonical}"
        );
    }

    // Ported from Tests/ModelConfigurationTests.swift:testProviderlessAliasesMatchCanonicalModels
    #[test]
    fn providerless_aliases_match_canonical_models() {
        assert_same_config(" GPT-OSS-20B ", "openai/gpt-oss-20b");
        assert_same_config("gpt-oss-120b", "openai/gpt-oss-120b");
        assert_same_config("gpt-oss-safeguard-20b", "openai/gpt-oss-safeguard-20b");
        assert_same_config("qwen3-32b", "qwen/qwen3-32b");
        assert_same_config(" QWEN3.6-27B ", "qwen/qwen3.6-27b");
    }

    // Ported from Tests/ModelConfigurationTests.swift:testKnownModelSettingsRemainStable
    #[test]
    fn known_model_settings_remain_stable() {
        let gpt_oss = config_for("openai/gpt-oss-20b");
        assert_eq!(gpt_oss.max_completion_tokens, Some(4096));
        assert_eq!(gpt_oss.reasoning_effort, Some("low"));
        assert_eq!(gpt_oss.include_reasoning, Some(false));
        assert!(!gpt_oss.should_strip_think_tags);

        let qwen = config_for("qwen/qwen3.6-27b");
        assert_eq!(qwen.reasoning_effort, Some("none"));
        assert_eq!(qwen.include_reasoning, Some(false));
        assert!(qwen.should_strip_think_tags);

        let unknown = config_for("example/unknown-model");
        assert_eq!(unknown.max_completion_tokens, None);
        assert_eq!(unknown.reasoning_effort, None);
        assert_eq!(unknown.include_reasoning, None);
        assert!(!unknown.should_strip_think_tags);
    }

    // Ported from Tests/ModelConfigurationTests.swift:testModelListsAreConsistent
    #[test]
    fn model_lists_are_consistent() {
        for list in [LLM_MODELS, VISION_MODELS, TRANSCRIPTION_MODELS] {
            let unique: HashSet<_> = list.iter().collect();
            assert_eq!(unique.len(), list.len(), "model list has duplicates");
        }
        let llm: HashSet<_> = LLM_MODELS.iter().collect();
        assert!(
            VISION_MODELS.iter().all(|m| llm.contains(m)),
            "every vision model must also be selectable as an LLM"
        );
    }

    // Ported from Tests/ModelConfigurationTests.swift:testThinkTagStripping
    #[test]
    fn think_tag_stripping() {
        assert_eq!(
            strip_think_tags("<think>hidden</think> Visible output"),
            "Visible output"
        );
        assert_eq!(
            strip_think_tags("<think>one</think>\n<think>two</think>\nResult"),
            "Result"
        );
        assert_eq!(strip_think_tags("<think>unfinished"), "");
        assert_eq!(
            strip_think_tags("Ordinary output with a later <think> marker"),
            "Ordinary output with a later <think> marker"
        );
    }
}
