//! Putting text where the cursor is.
//!
//! Replaces `pasteAtCursor` in `Sources/AppState.swift`, which wrote to
//! `NSPasteboard` and posted a synthetic Cmd-V `CGEvent`, then restored the
//! clipboard.
//!
//! The default here does not touch the clipboard at all. `wtype` types the
//! text through `zwp_virtual_keyboard_manager_v1`, which this compositor
//! exposes and which needs no elevated permission - unlike `ydotool`, whose
//! `/dev/uinput` is root-only on this machine. That removes the entire
//! snapshot-and-restore problem along with its races.
//!
//! The clipboard path is kept for clients that mishandle synthetic key events,
//! which in practice means some Electron, Java, and XWayland applications.

use std::process::{Command, Stdio};

use freeflow_config::InjectionMethod;
use thiserror::Error;

use crate::clipboard::{self, PreservedClipboard, Selection};

#[derive(Debug, Error)]
pub enum InjectError {
    #[error("wtype is not installed; it is required to type text into the focused window")]
    MissingWtype,
    #[error("wtype failed: {0}")]
    Wtype(String),
    #[error("clipboard error: {0}")]
    Clipboard(#[from] clipboard::ClipboardError),
}

fn run_wtype(args: &[&str]) -> Result<(), InjectError> {
    let output = Command::new("wtype")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                InjectError::MissingWtype
            } else {
                InjectError::Wtype(e.to_string())
            }
        })?;
    if !output.status.success() {
        return Err(InjectError::Wtype(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

/// Builds the `wtype` argument list for typing literal text.
///
/// The `--` matters: without it, a transcript that happens to begin with a
/// dash would be parsed as flags. Dictating "dash dash fix" is a documented
/// behaviour of the cleanup prompt, so this is a real case rather than a
/// theoretical one.
fn type_args(text: &str, delay_ms: u64) -> Vec<String> {
    let mut args = Vec::new();
    if delay_ms > 0 {
        args.push("-d".to_string());
        args.push(delay_ms.to_string());
    }
    args.push("--".to_string());
    args.push(text.to_string());
    args
}

/// Types text into whatever currently has keyboard focus.
pub fn type_text(text: &str, delay_ms: u64) -> Result<(), InjectError> {
    if text.is_empty() {
        return Ok(());
    }
    let args = type_args(text, delay_ms);
    run_wtype(&args.iter().map(String::as_str).collect::<Vec<_>>())
}

/// Copies the text and sends Ctrl-V, then restores the previous clipboard.
pub fn paste_text(text: &str) -> Result<(), InjectError> {
    if text.is_empty() {
        return Ok(());
    }
    let preserved = PreservedClipboard::capture();
    clipboard::write(Selection::Clipboard, text)?;

    let result = run_wtype(&["-M", "ctrl", "-k", "v", "-m", "ctrl"]);

    // Give the target a moment to read the selection before taking it back;
    // restoring immediately can win the race and paste the old contents.
    std::thread::sleep(std::time::Duration::from_millis(120));
    preserved.restore();
    result
}

/// Presses Return, for the spoken "press enter" command.
pub fn press_enter() -> Result<(), InjectError> {
    run_wtype(&["-k", "Return"])
}

/// Injects `text` using the configured method.
pub fn inject(text: &str, method: InjectionMethod, delay_ms: u64) -> Result<(), InjectError> {
    match method {
        InjectionMethod::Wtype => type_text(text, delay_ms),
        InjectionMethod::Clipboard => paste_text(text),
    }
}

/// Whether `wtype` can be found. Surfaced by `freeflow doctor`.
pub fn wtype_available() -> bool {
    Command::new("wtype")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_text_is_separated_from_flags() {
        // The cleanup prompt is documented to turn "dash dash fix" into
        // "--fix", so a transcript starting with dashes is expected input.
        let args = type_args("--fix the build", 0);
        assert_eq!(args, ["--", "--fix the build"]);
    }

    #[test]
    fn a_keystroke_delay_is_passed_through_when_set() {
        assert_eq!(type_args("hi", 12), ["-d", "12", "--", "hi"]);
        assert_eq!(type_args("hi", 0), ["--", "hi"]);
    }

    #[test]
    fn multiline_and_unicode_text_is_passed_as_one_argument() {
        let text = "line one\nline two — ünïcode";
        let args = type_args(text, 0);
        assert_eq!(args.len(), 2);
        assert_eq!(args[1], text);
    }
}
