//! The dictation pipeline decision tree.
//!
//! Ported from `processTranscript` at `Sources/AppState.swift:2459`, plus the
//! voice-macro matching at `:2413` and the trailing "press enter" parsing at
//! `:2358`.
//!
//! The macOS version interleaves the decisions with the network calls that
//! act on them. Here the two are split: [`plan`] is a pure function deciding
//! *what* to do, and [`resolve`] folds the result back into a final transcript
//! and an [`Outcome`]. Everything in between - the actual provider call - is
//! the daemon's job. That split is what makes the whole tree unit testable
//! without a network.

use crate::prompt::{
    build_cleanup_prompt, build_edit_mode_prompt, build_verbatim_translation_prompt, Prompt,
};

/// A phrase that expands to fixed text instead of being transcribed normally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceMacro {
    pub command: String,
    pub payload: String,
}

/// What the user is doing with this dictation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionIntent {
    /// Ordinary dictation: speak, get cleaned text.
    Dictation,
    /// Edit mode: the spoken words are an instruction for transforming
    /// `selected_text`.
    EditMode { selected_text: String },
}

/// Everything [`plan`] needs, gathered by the daemon before it runs.
#[derive(Debug, Clone)]
pub struct PipelineInput<'a> {
    pub raw_transcript: &'a str,
    pub intent: SessionIntent,
    pub context_summary: &'a str,
    pub vocabulary: &'a [String],
    pub custom_system_prompt: &'a str,
    pub output_language: &'a str,
    pub preserve_exact_wording: bool,
    pub macros: &'a [VoiceMacro],
}

/// The chosen branch of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing was said; do not call a provider and do not paste.
    SkipEmpty,
    /// A macro matched; its payload is used verbatim, no provider call.
    Macro {
        command: String,
        payload: String,
    },
    /// Paste the raw transcript unchanged, no provider call.
    UseRawTranscript,
    /// Call the provider with this prompt; each variant fails differently.
    Cleanup(Prompt),
    EditMode(Prompt),
    VerbatimTranslation(Prompt),
}

impl Decision {
    /// The prompt to send, when this decision needs a provider at all.
    pub fn prompt(&self) -> Option<&Prompt> {
        match self {
            Decision::Cleanup(p) | Decision::EditMode(p) | Decision::VerbatimTranslation(p) => {
                Some(p)
            }
            _ => None,
        }
    }
}

/// What actually happened, for the status line and history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    SkippedEmptyRawTranscript,
    VoiceMacro { command: String },
    PostProcessingSucceeded,
    PostProcessingFailedFallback,
    PreservedExactWording,
    PreservedExactWordingTranslated,
    PreservedExactWordingTranslationFailedFallback,
    EditModeSucceeded,
    EditModeFailedFallback,
}

impl Outcome {
    /// Status text shown in the overlay, matching the macOS wording.
    pub fn status_message(&self, is_retry: bool) -> String {
        match self {
            Outcome::SkippedEmptyRawTranscript => {
                "Skipped macros and post-processing for empty raw transcript".into()
            }
            Outcome::VoiceMacro { command } => format!("Voice macro used: {command}"),
            Outcome::PostProcessingSucceeded => if is_retry {
                "Post-processing succeeded (retried)"
            } else {
                "Post-processing succeeded"
            }
            .into(),
            Outcome::PostProcessingFailedFallback => if is_retry {
                "Post-processing failed on retry, using raw transcript"
            } else {
                "Post-processing failed, using raw transcript"
            }
            .into(),
            Outcome::PreservedExactWording => {
                "Preserved exact wording, skipped post-processing".into()
            }
            Outcome::PreservedExactWordingTranslated => {
                "Preserved exact wording, translated to output language".into()
            }
            Outcome::PreservedExactWordingTranslationFailedFallback => {
                "Verbatim translation failed, using untranslated raw transcript".into()
            }
            Outcome::EditModeSucceeded => "Edit mode succeeded".into(),
            Outcome::EditModeFailedFallback => "Edit mode failed, using selected text".into(),
        }
    }

    /// Whether the pipeline fell back rather than doing what was asked. Used
    /// to decide whether to show a warning indicator.
    pub fn is_fallback(&self) -> bool {
        matches!(
            self,
            Outcome::PostProcessingFailedFallback
                | Outcome::PreservedExactWordingTranslationFailedFallback
                | Outcome::EditModeFailedFallback
        )
    }
}

/// Lowercases and strips punctuation so macro matching is forgiving about how
/// the transcriber punctuated the phrase.
fn normalize_for_macro(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| !c.is_ascii_punctuation())
        .collect::<String>()
        .trim()
        .to_string()
}

fn find_matching_macro<'a>(transcript: &str, macros: &'a [VoiceMacro]) -> Option<&'a VoiceMacro> {
    let normalized = normalize_for_macro(transcript);
    if normalized.is_empty() {
        return None;
    }
    macros
        .iter()
        .find(|m| normalize_for_macro(&m.command) == normalized)
}

/// Chooses which branch of the pipeline to run.
///
/// Order is significant and matches macOS: edit mode wins over everything,
/// then macros, then preserve-exact-wording, then ordinary cleanup.
pub fn plan(input: &PipelineInput<'_>) -> Decision {
    let transcript = input.raw_transcript.trim();
    if transcript.is_empty() {
        return Decision::SkipEmpty;
    }

    if let SessionIntent::EditMode { selected_text } = &input.intent {
        return Decision::EditMode(build_edit_mode_prompt(
            selected_text,
            input.raw_transcript,
            input.context_summary,
            input.vocabulary,
            input.output_language,
        ));
    }

    if let Some(matched) = find_matching_macro(transcript, input.macros) {
        return Decision::Macro {
            command: matched.command.clone(),
            payload: matched.payload.clone(),
        };
    }

    // Preserve-exact-wording has two sub-cases so translation stays honored.
    // The user asked for another language; silently dropping the translation
    // would defeat their settings, so route through a translate-only prompt
    // that still preserves filler and informal wording 1:1.
    if input.preserve_exact_wording {
        let language = input.output_language.trim();
        if language.is_empty() {
            return Decision::UseRawTranscript;
        }
        return Decision::VerbatimTranslation(build_verbatim_translation_prompt(
            transcript, language,
        ));
    }

    Decision::Cleanup(build_cleanup_prompt(
        transcript,
        input.context_summary,
        input.vocabulary,
        input.custom_system_prompt,
        input.output_language,
    ))
}

/// Folds a provider result into the final text and an [`Outcome`].
///
/// Every failure path yields usable text rather than an error: a failed
/// cleanup falls back to the raw transcript and a failed edit-mode
/// transformation falls back to the untouched selection. The user's words are
/// never lost because a provider misbehaved.
pub fn resolve(
    decision: &Decision,
    input: &PipelineInput<'_>,
    provider_output: Option<String>,
) -> (String, Outcome) {
    let transcript = input.raw_transcript.trim().to_string();

    match decision {
        Decision::SkipEmpty => (String::new(), Outcome::SkippedEmptyRawTranscript),

        Decision::Macro { command, payload } => (
            payload.clone(),
            Outcome::VoiceMacro {
                command: command.clone(),
            },
        ),

        Decision::UseRawTranscript => (transcript, Outcome::PreservedExactWording),

        Decision::EditMode(_) => {
            let selected = match &input.intent {
                SessionIntent::EditMode { selected_text } => selected_text.clone(),
                SessionIntent::Dictation => String::new(),
            };
            match provider_output {
                Some(text) => (text, Outcome::EditModeSucceeded),
                None => (selected, Outcome::EditModeFailedFallback),
            }
        }

        Decision::VerbatimTranslation(_) => match provider_output {
            Some(text) => (text, Outcome::PreservedExactWordingTranslated),
            None => (
                transcript,
                Outcome::PreservedExactWordingTranslationFailedFallback,
            ),
        },

        Decision::Cleanup(_) => match provider_output {
            Some(text) => (text, Outcome::PostProcessingSucceeded),
            None => (transcript, Outcome::PostProcessingFailedFallback),
        },
    }
}

/// A transcript with a trailing "press enter" command stripped off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTranscriptCommands {
    pub transcript: String,
    pub should_press_enter: bool,
}

/// Detects and removes a trailing spoken "press enter".
///
/// Ported from `parseTranscriptCommands` at `Sources/AppState.swift:2358`.
pub fn parse_transcript_commands(
    transcript: &str,
    press_enter_command_enabled: bool,
) -> ParsedTranscriptCommands {
    use std::sync::LazyLock;

    use regex::Regex;

    static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:^|[ \t\r\n,;:\-]+)press[ \t\r\n]+enter[\s[:punct:]]*$")
            .expect("valid pattern")
    });

    if !press_enter_command_enabled {
        return ParsedTranscriptCommands {
            transcript: transcript.trim().to_string(),
            should_press_enter: false,
        };
    }

    match PATTERN.find(transcript) {
        Some(m) => {
            let mut stripped = String::with_capacity(transcript.len());
            stripped.push_str(&transcript[..m.start()]);
            stripped.push_str(&transcript[m.end()..]);
            ParsedTranscriptCommands {
                transcript: stripped.trim().to_string(),
                should_press_enter: true,
            }
        }
        None => ParsedTranscriptCommands {
            transcript: transcript.trim().to_string(),
            should_press_enter: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base<'a>(raw: &'a str) -> PipelineInput<'a> {
        PipelineInput {
            raw_transcript: raw,
            intent: SessionIntent::Dictation,
            context_summary: "App: foot",
            vocabulary: &[],
            custom_system_prompt: "",
            output_language: "",
            preserve_exact_wording: false,
            macros: &[],
        }
    }

    #[test]
    fn empty_transcript_skips_everything() {
        let input = base("   \n  ");
        let decision = plan(&input);
        assert_eq!(decision, Decision::SkipEmpty);
        let (text, outcome) = resolve(&decision, &input, None);
        assert_eq!(text, "");
        assert_eq!(outcome, Outcome::SkippedEmptyRawTranscript);
    }

    #[test]
    fn edit_mode_wins_over_macros_and_cleanup() {
        let macros = [VoiceMacro {
            command: "make it shorter".into(),
            payload: "SHOULD NOT FIRE".into(),
        }];
        let mut input = base("make it shorter");
        input.intent = SessionIntent::EditMode {
            selected_text: "The quick brown fox jumped.".into(),
        };
        input.macros = &macros;

        let decision = plan(&input);
        assert!(matches!(decision, Decision::EditMode(_)));
        let (text, outcome) = resolve(&decision, &input, Some("The fox jumped.".into()));
        assert_eq!(text, "The fox jumped.");
        assert_eq!(outcome, Outcome::EditModeSucceeded);
    }

    #[test]
    fn failed_edit_mode_returns_the_selection_untouched() {
        let mut input = base("make it shorter");
        input.intent = SessionIntent::EditMode {
            selected_text: "Original selection.".into(),
        };
        let decision = plan(&input);
        let (text, outcome) = resolve(&decision, &input, None);
        assert_eq!(text, "Original selection.");
        assert_eq!(outcome, Outcome::EditModeFailedFallback);
        assert!(outcome.is_fallback());
    }

    #[test]
    fn macro_matching_ignores_case_and_punctuation() {
        let macros = [VoiceMacro {
            command: "insert my email".into(),
            payload: "synthetic@example.com".into(),
        }];
        for spoken in [
            "Insert my email.",
            "insert my email",
            "  INSERT MY EMAIL!  ",
        ] {
            let mut input = base(spoken);
            input.macros = &macros;
            let decision = plan(&input);
            let (text, outcome) = resolve(&decision, &input, None);
            assert_eq!(text, "synthetic@example.com", "failed for {spoken:?}");
            assert_eq!(
                outcome,
                Outcome::VoiceMacro {
                    command: "insert my email".into()
                }
            );
        }
    }

    #[test]
    fn near_miss_does_not_trigger_a_macro() {
        let macros = [VoiceMacro {
            command: "insert my email".into(),
            payload: "synthetic@example.com".into(),
        }];
        let mut input = base("insert my email address");
        input.macros = &macros;
        assert!(matches!(plan(&input), Decision::Cleanup(_)));
    }

    #[test]
    fn preserve_exact_wording_skips_the_provider_entirely() {
        let mut input = base("  um so like hello  ");
        input.preserve_exact_wording = true;
        let decision = plan(&input);
        assert_eq!(decision, Decision::UseRawTranscript);
        let (text, outcome) = resolve(&decision, &input, None);
        assert_eq!(text, "um so like hello");
        assert_eq!(outcome, Outcome::PreservedExactWording);
    }

    #[test]
    fn preserve_exact_wording_still_translates_when_a_language_is_set() {
        let mut input = base("um so like hello");
        input.preserve_exact_wording = true;
        input.output_language = "Spanish";
        let decision = plan(&input);
        assert!(matches!(decision, Decision::VerbatimTranslation(_)));

        let (text, outcome) = resolve(&decision, &input, Some("eh o sea hola".into()));
        assert_eq!(text, "eh o sea hola");
        assert_eq!(outcome, Outcome::PreservedExactWordingTranslated);

        let (text, outcome) = resolve(&decision, &input, None);
        assert_eq!(text, "um so like hello");
        assert_eq!(
            outcome,
            Outcome::PreservedExactWordingTranslationFailedFallback
        );
    }

    #[test]
    fn failed_cleanup_falls_back_to_the_raw_transcript() {
        let input = base("  hello there  ");
        let decision = plan(&input);
        assert!(matches!(decision, Decision::Cleanup(_)));
        let (text, outcome) = resolve(&decision, &input, None);
        assert_eq!(text, "hello there");
        assert_eq!(outcome, Outcome::PostProcessingFailedFallback);
    }

    // The core promise: no branch of the tree can lose what the user said.
    #[test]
    fn no_branch_loses_the_users_words_when_the_provider_fails() {
        let macros = [VoiceMacro {
            command: "sig".into(),
            payload: "Regards".into(),
        }];
        let variants: Vec<PipelineInput<'_>> = vec![
            base("hello there"),
            PipelineInput {
                preserve_exact_wording: true,
                ..base("hello there")
            },
            PipelineInput {
                preserve_exact_wording: true,
                output_language: "Spanish",
                ..base("hello there")
            },
            PipelineInput {
                macros: &macros,
                ..base("sig")
            },
            PipelineInput {
                intent: SessionIntent::EditMode {
                    selected_text: "kept selection".into(),
                },
                ..base("shorten it")
            },
        ];
        for input in &variants {
            let decision = plan(input);
            let (text, _) = resolve(&decision, input, None);
            assert!(
                !text.is_empty(),
                "provider failure produced empty text for {decision:?}"
            );
        }
    }

    #[test]
    fn press_enter_command_is_detected_and_stripped() {
        let cases = [
            ("send it press enter", "send it", true),
            ("send it, press enter.", "send it", true),
            ("Send it PRESS ENTER!!", "Send it", true),
            ("press enter", "", true),
        ];
        for (input, expected, flag) in cases {
            let parsed = parse_transcript_commands(input, true);
            assert_eq!(parsed.transcript, expected, "for {input:?}");
            assert_eq!(parsed.should_press_enter, flag, "for {input:?}");
        }
    }

    #[test]
    fn press_enter_is_left_alone_when_not_trailing_or_disabled() {
        // Mid-sentence mentions are ordinary dictation, not a command.
        let parsed = parse_transcript_commands("press enter to continue", true);
        assert_eq!(parsed.transcript, "press enter to continue");
        assert!(!parsed.should_press_enter);

        // And the whole feature can be switched off.
        let parsed = parse_transcript_commands("send it press enter", false);
        assert_eq!(parsed.transcript, "send it press enter");
        assert!(!parsed.should_press_enter);
    }
}
