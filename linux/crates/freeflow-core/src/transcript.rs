//! Transcription response parsing and LLM output sanitizing.
//!
//! Ported from `Sources/TranscriptTextCore.swift`. The hallucination phrase
//! list, the 0.1 `no_speech_prob` threshold, the `EMPTY` sentinel handling and
//! the instruction-execution guard are all reproduced exactly; changing any of
//! them changes user-visible behaviour.

use std::collections::HashSet;

use regex::Regex;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TranscriptionResponseParsingError {
    #[error("transcription provider returned an unusable response")]
    InvalidResponse,
}

/// Whisper can emit these stock phrases for silence or background noise.
/// Only suppress them when segment metadata independently reports a high
/// probability of no speech, which protects genuine short dictations.
const HALLUCINATION_PHRASES: &[&str] = &[
    "thank you",
    "thank you for watching",
    "thank you very much",
    "thank you so much",
    "thanks for watching",
    "please subscribe",
    "like and subscribe",
    "subtitles by",
    "subtitles by the amara.org community",
    "you",
];

/// Tuned conservatively against roughly 500 quiet, noisy, and real-speech
/// samples to minimize the chance of filtering genuine short dictations.
const HALLUCINATION_NO_SPEECH_THRESHOLD: f64 = 0.1;

/// Parses a transcription response body into transcript text.
///
/// A body that is not valid JSON is rejected outright. Valid JSON that is not
/// an object carrying a `text` string falls back to treating the whole body as
/// UTF-8 text, which is how non-OpenAI-shaped providers are tolerated.
pub fn parse_transcription_response(
    data: &[u8],
) -> Result<String, TranscriptionResponseParsingError> {
    let value: Value = serde_json::from_slice(data)
        .map_err(|_| TranscriptionResponseParsingError::InvalidResponse)?;

    if let Some(text) = value.get("text").and_then(Value::as_str) {
        if is_hallucination(text, &value) {
            return Ok(String::new());
        }
        return Ok(text.to_string());
    }

    let plain_text = std::str::from_utf8(data).unwrap_or_default();
    let text = plain_text
        .split([
            '\n', '\r', '\u{0b}', '\u{0c}', '\u{85}', '\u{2028}', '\u{2029}',
        ])
        .collect::<Vec<_>>()
        .join(" ");
    let text = text.trim();
    if text.is_empty() {
        return Err(TranscriptionResponseParsingError::InvalidResponse);
    }
    Ok(text.to_string())
}

fn is_hallucination(text: &str, json: &Value) -> bool {
    // Swift trims `CharacterSet.punctuationCharacters` (Unicode category P)
    // unioned with whitespace. ASCII punctuation plus whitespace covers every
    // phrase in the list and every form Whisper emits in practice.
    let normalized = text
        .to_lowercase()
        .trim_matches(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .to_string();

    if !HALLUCINATION_PHRASES.contains(&normalized.as_str()) {
        return false;
    }

    // Without segment metadata we cannot tell silence from a genuine short
    // dictation, so the phrase is kept.
    let Some(no_speech_prob) = json
        .get("segments")
        .and_then(Value::as_array)
        .and_then(|segments| segments.first())
        .and_then(|segment| segment.get("no_speech_prob"))
        .and_then(Value::as_f64)
    else {
        return false;
    };

    no_speech_prob >= HALLUCINATION_NO_SPEECH_THRESHOLD
}

fn strip_wrapping_quotes(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() > 1 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        return trimmed[1..trimmed.len() - 1].trim().to_string();
    }
    trimmed.to_string()
}

/// Verbatim translation deliberately preserves the cleanup prompt's `EMPTY`
/// sentinel because "empty" can be legitimate translated speech here.
pub fn sanitize_verbatim_translation(value: &str) -> String {
    if value.trim().is_empty() {
        return String::new();
    }
    strip_wrapping_quotes(value)
}

pub fn sanitize_post_processed_transcript(value: &str) -> String {
    if value.trim().is_empty() {
        return String::new();
    }
    let result = strip_wrapping_quotes(value);
    if result == "EMPTY" {
        return String::new();
    }
    result
}

pub fn sanitize_command_mode_transcript(value: &str) -> String {
    value.trim().to_string()
}

const INSTRUCTION_MARKERS: &[&str] = &[
    "ask",
    "answer",
    "compose",
    "create",
    "draft",
    "email",
    "generate",
    "make",
    "message",
    "prompt",
    "reply",
    "respond",
    "response",
    "summarize",
    "tell",
    "translate",
    "write",
    "claude",
    "chatgpt",
    "ai",
    "llm",
];

const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "can", "could", "for", "from", "had",
    "has", "have", "he", "her", "him", "his", "i", "if", "in", "into", "is", "it", "its", "just",
    "me", "my", "of", "on", "or", "our", "please", "she", "so", "that", "the", "their", "them",
    "then", "there", "this", "to", "um", "uh", "was", "we", "were", "what", "when", "where", "who",
    "with", "would", "you", "your",
];

const ASSISTANT_PREAMBLE_PATTERN: &str =
    r"(?i)^\s*(sure|certainly|absolutely|here('s| is)|i('d| would) be happy to|i can)\b";

fn significant_tokens(text: &str) -> HashSet<String> {
    let stop_words: HashSet<&str> = STOP_WORDS.iter().copied().collect();
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| token.chars().count() > 1 && !stop_words.contains(token))
        .map(str::to_string)
        .collect()
}

/// Detects the failure mode where the LLM *performed* the dictated instruction
/// instead of cleaning it up as text.
///
/// This guard matters more on Linux than it did on macOS: the default backend
/// is an agentic CLI harness, which is far more inclined to act on an
/// instruction than a bare chat completion is.
pub fn appears_to_have_executed_instruction(
    raw_transcript: &str,
    cleaned_transcript: &str,
    output_language: &str,
) -> bool {
    // A requested translation legitimately produces text that shares almost no
    // tokens with the source, so the overlap heuristic cannot apply.
    if !output_language.trim().is_empty() {
        return false;
    }

    let raw_tokens = significant_tokens(raw_transcript);
    let cleaned_tokens = significant_tokens(cleaned_transcript);
    if raw_tokens.is_empty() || cleaned_tokens.is_empty() {
        return false;
    }

    let markers: HashSet<String> = INSTRUCTION_MARKERS.iter().map(|m| m.to_string()).collect();
    let raw_markers: HashSet<&String> = raw_tokens.intersection(&markers).collect();
    if raw_markers.is_empty() {
        return false;
    }

    let preserved_markers = raw_markers
        .iter()
        .filter(|marker| cleaned_tokens.contains(**marker))
        .count();
    let overlap = raw_tokens.intersection(&cleaned_tokens).count();
    let overlap_ratio = overlap as f64 / raw_tokens.len().max(1) as f64;

    // Compiled once per call; the guard runs at most once per dictation.
    let preamble = Regex::new(ASSISTANT_PREAMBLE_PATTERN).expect("static pattern is valid");
    let cleaned_has_preamble = preamble.is_match(cleaned_transcript);
    let raw_has_preamble = preamble.is_match(raw_transcript);

    (cleaned_has_preamble && !raw_has_preamble) || (preserved_markers == 0 && overlap_ratio < 0.35)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(data: &[u8]) {
        assert_eq!(
            parse_transcription_response(data),
            Err(TranscriptionResponseParsingError::InvalidResponse),
            "expected invalid response for {data:?}"
        );
    }

    // Ported from Tests/TranscriptTextCoreTests.swift:testJSONTranscriptParsing
    #[test]
    fn json_transcript_parsing() {
        assert_eq!(
            parse_transcription_response(br#"{"text":"Synthetic transcript."}"#).unwrap(),
            "Synthetic transcript."
        );
        assert_eq!(parse_transcription_response(br#"{"text":""}"#).unwrap(), "");
    }

    // Ported from Tests/TranscriptTextCoreTests.swift:testNonTranscriptJSONFallsBackToUTF8
    #[test]
    fn non_transcript_json_falls_back_to_utf8() {
        let data = "{\n  \"synthetic\": \"fallback\"\n}".as_bytes();
        assert_eq!(
            parse_transcription_response(data).unwrap(),
            "{   \"synthetic\": \"fallback\" }"
        );
    }

    // Ported from Tests/TranscriptTextCoreTests.swift:testInvalidTranscriptResponses
    #[test]
    fn invalid_transcript_responses() {
        err(b"");
        err(b" \n\t ");
        err(b"{malformed synthetic JSON");
        err(&[0xFF, 0xFE]);
    }

    // Ported from Tests/TranscriptTextCoreTests.swift:testHighConfidenceHallucinationsAreSuppressed
    #[test]
    fn high_confidence_hallucinations_are_suppressed() {
        assert_eq!(
            parse_transcription_response(
                br#"{"text":"Thank you.","segments":[{"no_speech_prob":0.1}]}"#
            )
            .unwrap(),
            ""
        );
        assert_eq!(
            parse_transcription_response(
                br#"{"text":"  THANK YOU FOR WATCHING!!!  ","segments":[{"no_speech_prob":0.9}]}"#
            )
            .unwrap(),
            ""
        );
    }

    // Ported from Tests/TranscriptTextCoreTests.swift:testPossibleRealSpeechIsPreserved
    #[test]
    fn possible_real_speech_is_preserved() {
        assert_eq!(
            parse_transcription_response(
                br#"{"text":"Thank you.","segments":[{"no_speech_prob":0.099}]}"#
            )
            .unwrap(),
            "Thank you."
        );
        assert_eq!(
            parse_transcription_response(br#"{"text":"Thank you."}"#).unwrap(),
            "Thank you."
        );
        assert_eq!(
            parse_transcription_response(
                br#"{"text":"Thank you.","segments":[{"synthetic":true}]}"#
            )
            .unwrap(),
            "Thank you."
        );
        assert_eq!(
            parse_transcription_response(
                br#"{"text":"Synthetic project update.","segments":[{"no_speech_prob":0.95}]}"#
            )
            .unwrap(),
            "Synthetic project update."
        );
    }

    // Ported from Tests/TranscriptTextCoreTests.swift:testPostProcessedTranscriptSanitization
    #[test]
    fn post_processed_transcript_sanitization() {
        assert_eq!(
            sanitize_post_processed_transcript("  \"Synthetic output.\" \n"),
            "Synthetic output."
        );
        assert_eq!(sanitize_post_processed_transcript("EMPTY"), "");
        assert_eq!(sanitize_post_processed_transcript("\"EMPTY\""), "");
        assert_eq!(sanitize_post_processed_transcript("empty"), "empty");
        assert_eq!(sanitize_post_processed_transcript("  \n "), "");
    }

    // Ported from Tests/TranscriptTextCoreTests.swift:testModeSpecificSanitization
    #[test]
    fn mode_specific_sanitization() {
        assert_eq!(sanitize_verbatim_translation("  \"EMPTY\"  "), "EMPTY");
        assert_eq!(
            sanitize_verbatim_translation(" \"Literal synthetic text.\" "),
            "Literal synthetic text."
        );
        assert_eq!(
            sanitize_command_mode_transcript("  \"Keep command quotes\" \n"),
            "\"Keep command quotes\""
        );
    }

    // Ported from Tests/TranscriptTextCoreTests.swift:testInstructionExecutionGuard
    #[test]
    fn instruction_execution_guard() {
        assert!(
            appears_to_have_executed_instruction(
                "Write an email asking Alex for the synthetic report.",
                "Sure, here's a draft: Hello Alex, please send the report.",
                ""
            ),
            "assistant-style execution should be rejected"
        );
        assert!(
            appears_to_have_executed_instruction(
                "Write a haiku about synthetic rain.",
                "Soft drizzle taps windows.",
                ""
            ),
            "low-overlap instruction execution should be rejected"
        );
        assert!(
            !appears_to_have_executed_instruction(
                "Write an email asking Alex for the synthetic report.",
                "Write an email asking Alex for the synthetic report.",
                ""
            ),
            "faithful cleanup should preserve instruction wording"
        );
        assert!(
            !appears_to_have_executed_instruction(
                "The synthetic launch is Friday.",
                "Sure, the synthetic launch is Friday.",
                ""
            ),
            "ordinary speech without an instruction marker should not trigger the guard"
        );
        assert!(
            !appears_to_have_executed_instruction(
                "Translate the synthetic update.",
                "Aggiornamento sintetico.",
                "Italian"
            ),
            "explicit translation output should bypass the instruction guard"
        );
    }
}
