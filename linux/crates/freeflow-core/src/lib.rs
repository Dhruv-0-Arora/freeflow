//! Pure dictation-pipeline logic shared by the FreeFlow Linux binaries.
//!
//! Everything in this crate is deterministic and I/O-free so it can be unit
//! tested without a compositor, a microphone, or a network. Platform access
//! lives in `freeflow-platform`; providers live in `freeflow-llm` and
//! `freeflow-asr`.

pub mod cooldown;
pub mod model_config;
pub mod prompt;
pub mod prompt_text;
pub mod shortcut;
pub mod transcript;

pub use prompt::{
    apply_output_language, build_cleanup_prompt, build_edit_mode_prompt,
    build_verbatim_translation_prompt, merge_vocabulary_terms, Prompt,
};

pub use cooldown::{rate_limit_cooldown, LlmCooldownManager, RateLimitCooldown};
pub use model_config::{config_for, strip_think_tags, ModelConfig};

pub use transcript::{
    appears_to_have_executed_instruction, parse_transcription_response,
    sanitize_command_mode_transcript, sanitize_post_processed_transcript,
    sanitize_verbatim_translation, TranscriptionResponseParsingError,
};

pub use shortcut::{
    DictationShortcutAction, DictationShortcutSessionController, RecordingTriggerMode,
    ShortcutEvent,
};
