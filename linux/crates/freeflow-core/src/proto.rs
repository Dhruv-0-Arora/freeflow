//! Control protocol between `freeflow` (the CLI) and `freeflowd`.
//!
//! Newline-delimited JSON over a Unix socket in `$XDG_RUNTIME_DIR`. Chosen
//! over a bespoke binary format so the daemon can be driven and debugged with
//! `socat` when something goes wrong on a machine that has no debugger
//! attached.
//!
//! The compositor owns key binding on Wayland, so hotkeys arrive here as
//! already-resolved [`ShortcutEvent`]s dispatched by Hyprland binds rather
//! than as raw key codes.

use serde::{Deserialize, Serialize};

use crate::shortcut::ShortcutEvent;

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// A shortcut edge from a Hyprland `bind`.
    Shortcut {
        event: ShortcutEvent,
        /// The main key of the bind that fired, as Hyprland spells it
        /// (`"D"`, `"T"`, `"code:40"`). Lets the daemon watch `/dev/input`
        /// for the key's release, since a compositor release bind would
        /// consume the key's press for every modifier state. Optional so
        /// older clients and manual `socat` use keep working.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        /// Start this session in edit mode, applying the dictation to the
        /// PRIMARY selection. Explicit opt-in: on Wayland the selection
        /// persists from whatever was last highlighted anywhere, so sniffing
        /// it on every dictation turned stale selections into rewrites of
        /// the wrong text.
        #[serde(default, skip_serializing_if = "is_false")]
        edit: bool,
    },
    /// Re-inject the last result, for when a paste landed in the wrong window.
    CopyAgain,
    /// Abandon the current dictation without injecting anything.
    Cancel,
    Status,
    /// Environment checks, for `freeflow doctor`.
    Doctor,
    /// Run the pipeline against a WAV file instead of the microphone.
    ///
    /// Makes the transcribe/cleanup/inject path reproducible: the same audio
    /// gives comparable per-stage timings, which is how latency regressions
    /// get noticed rather than merely felt.
    Bench {
        wav_path: String,
        /// Type the result into the focused window, rather than only
        /// reporting it. Off by default so a benchmark cannot scribble into
        /// whatever the user happens to be looking at.
        #[serde(default)]
        inject: bool,
    },
    /// Read recent dictations.
    History {
        limit: usize,
    },
    /// Delete all stored dictations.
    ClearHistory,
    Shutdown,
}

/// One history row, as sent over the control socket.
///
/// This carries transcript text, unlike [`Response::message`]. That is
/// acceptable only because the socket lives in `$XDG_RUNTIME_DIR`, which is
/// `0700`, and because sending it is what the user explicitly asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryRow {
    pub created_at: i64,
    pub raw_transcript: String,
    pub final_text: String,
    pub outcome: String,
    pub app_class: Option<String>,
    pub transcription_ms: i64,
    pub cleanup_ms: i64,
}

/// What the daemon is doing. Drives the overlay and `freeflow status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonState {
    Idle,
    Recording,
    Transcribing,
    Cleaning,
    Injecting,
    Error,
}

impl DaemonState {
    pub fn label(self) -> &'static str {
        match self {
            DaemonState::Idle => "Ready",
            DaemonState::Recording => "Recording",
            DaemonState::Transcribing => "Transcribing",
            DaemonState::Cleaning => "Cleaning up",
            DaemonState::Injecting => "Pasting",
            DaemonState::Error => "Error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub state: DaemonState,
    /// Human-readable status. Never contains transcript text: the control
    /// socket has looser permissions than the history store, and dictated
    /// content must not leak into something a `socat` session can read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Input level 0.0..=1.0 while recording, for the overlay's meter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<f32>,
    /// Per-stage milliseconds, reported by `bench`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<StageTimings>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<DoctorCheck>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<HistoryRow>,
}

impl Response {
    pub fn ok(state: DaemonState) -> Self {
        Self {
            ok: true,
            state,
            message: None,
            level: None,
            timings: None,
            checks: Vec::new(),
            history: Vec::new(),
        }
    }

    pub fn error(state: DaemonState, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            state,
            message: Some(message.into()),
            level: None,
            timings: None,
            checks: Vec::new(),
            history: Vec::new(),
        }
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }
}

/// Per-stage latency, in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageTimings {
    pub transcription_ms: u64,
    pub cleanup_ms: u64,
    pub injection_ms: u64,
}

/// One environment check reported by `freeflow doctor`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Frames a value as one protocol line.
pub fn encode<T: Serialize>(value: &T) -> String {
    let mut line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_as_tagged_json() {
        let request = Request::Shortcut {
            event: ShortcutEvent::HoldActivated,
            key: None,
            edit: false,
        };
        let line = encode(&request);
        assert!(line.ends_with('\n'));
        // No `key` on the wire when it is absent, so the encoding is
        // unchanged for clients that do not send one.
        assert_eq!(line.trim(), r#"{"cmd":"shortcut","event":"holdActivated"}"#);
        assert_eq!(
            serde_json::from_str::<Request>(line.trim()).unwrap(),
            request
        );
    }

    // Binds dispatch `--key` so the daemon can watch /dev/input for the
    // release; a request without one (older client, manual socat) must still
    // parse.
    #[test]
    fn the_shortcut_key_and_edit_flag_are_optional_on_the_wire() {
        assert_eq!(
            serde_json::from_str::<Request>(
                r#"{"cmd":"shortcut","event":"holdActivated","key":"D"}"#
            )
            .unwrap(),
            Request::Shortcut {
                event: ShortcutEvent::HoldActivated,
                key: Some("D".into()),
                edit: false,
            }
        );
        assert_eq!(
            serde_json::from_str::<Request>(
                r#"{"cmd":"shortcut","event":"holdActivated","key":"E","edit":true}"#
            )
            .unwrap(),
            Request::Shortcut {
                event: ShortcutEvent::HoldActivated,
                key: Some("E".into()),
                edit: true,
            }
        );
    }

    #[test]
    fn unit_requests_are_a_bare_tag() {
        assert_eq!(encode(&Request::Status).trim(), r#"{"cmd":"status"}"#);
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"copy_again"}"#).unwrap(),
            Request::CopyAgain
        );
    }

    #[test]
    fn responses_omit_absent_fields() {
        assert_eq!(
            encode(&Response::ok(DaemonState::Idle)).trim(),
            r#"{"ok":true,"state":"idle"}"#
        );
    }

    #[test]
    fn an_unknown_command_is_rejected_rather_than_silently_ignored() {
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"launch_missiles"}"#).is_err());
    }
}
