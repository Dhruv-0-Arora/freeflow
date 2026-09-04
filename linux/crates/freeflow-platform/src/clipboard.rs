//! Clipboard and primary-selection access.
//!
//! Replaces the `NSPasteboard` handling in `Sources/AppState.swift`, including
//! the `PreservedPasteboardSnapshot` machinery that snapshotted every
//! pasteboard item and type so it could be restored after a synthetic paste.
//!
//! Two things make this much smaller than the macOS original. Text is normally
//! typed directly with `wtype`, so the clipboard is not involved at all in the
//! default path. And Wayland's PRIMARY selection carries highlighted text
//! without the user copying anything, which is what Edit Mode needs and what
//! macOS had to go through the Accessibility API for.
//!
//! `wl-copy`/`wl-paste` use `wlr-data-control`, which this compositor exposes,
//! so a background daemon can read and write selections without holding focus.

use std::process::{Command, Stdio};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("{tool} is not installed")]
    MissingTool { tool: &'static str },
    #[error("{tool} failed: {reason}")]
    Failed { tool: &'static str, reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// The ordinary clipboard, written by an explicit copy.
    Clipboard,
    /// The PRIMARY selection, which most toolkits populate on highlight.
    Primary,
}

fn run(tool: &'static str, args: &[&str]) -> Result<std::process::Output, ClipboardError> {
    Command::new(tool)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ClipboardError::MissingTool { tool }
            } else {
                ClipboardError::Failed {
                    tool,
                    reason: e.to_string(),
                }
            }
        })
}

/// Reads a selection as text.
///
/// An empty selection is `Ok("")`, not an error: `wl-paste` exits non-zero
/// when the selection is empty, which is a normal state rather than a fault.
pub fn read(selection: Selection) -> Result<String, ClipboardError> {
    let mut args = vec!["--no-newline", "--type", "text/plain;charset=utf-8"];
    if selection == Selection::Primary {
        args.push("--primary");
    }
    let output = run("wl-paste", &args)?;
    if !output.status.success() {
        return Ok(String::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Reads the text the user currently has highlighted.
///
/// Falls back from PRIMARY to CLIPBOARD because a few clients (notably some
/// Electron apps) never populate PRIMARY, and in those the user's habit is to
/// press Ctrl-C first.
pub fn read_selected_text() -> String {
    match read(Selection::Primary) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => read(Selection::Clipboard).unwrap_or_default(),
    }
}

/// Writes text to a selection.
pub fn write(selection: Selection, text: &str) -> Result<(), ClipboardError> {
    use std::io::Write;

    let mut args = vec!["--type", "text/plain;charset=utf-8"];
    if selection == Selection::Primary {
        args.push("--primary");
    }
    // `wl-copy` must keep running to serve the selection, so it daemonizes
    // itself; we only wait for it to take ownership.
    let mut child = Command::new("wl-copy")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ClipboardError::MissingTool { tool: "wl-copy" }
            } else {
                ClipboardError::Failed {
                    tool: "wl-copy",
                    reason: e.to_string(),
                }
            }
        })?;

    child
        .stdin
        .take()
        .ok_or(ClipboardError::Failed {
            tool: "wl-copy",
            reason: "stdin was not piped".into(),
        })?
        .write_all(text.as_bytes())
        .map_err(|e| ClipboardError::Failed {
            tool: "wl-copy",
            reason: e.to_string(),
        })?;

    let status = child.wait().map_err(|e| ClipboardError::Failed {
        tool: "wl-copy",
        reason: e.to_string(),
    })?;
    if !status.success() {
        return Err(ClipboardError::Failed {
            tool: "wl-copy",
            reason: format!("exited with {status}"),
        });
    }
    Ok(())
}

/// The clipboard contents captured before a paste, so they can be put back.
///
/// Far simpler than the macOS `PreservedPasteboardSnapshot`, which had to
/// preserve every representation of every item and use `changeCount`
/// heuristics. Only the default injection path avoids the clipboard entirely;
/// this exists for the fallback path.
pub struct PreservedClipboard {
    previous: String,
}

impl PreservedClipboard {
    pub fn capture() -> Self {
        Self {
            previous: read(Selection::Clipboard).unwrap_or_default(),
        }
    }

    /// Restores what was there before.
    ///
    /// Deliberately does not restore an empty clipboard: taking ownership just
    /// to publish nothing would clear whatever another application set in the
    /// meantime.
    pub fn restore(self) {
        if self.previous.is_empty() {
            return;
        }
        if let Err(e) = write(Selection::Clipboard, &self.previous) {
            tracing::warn!(error = %e, "could not restore the previous clipboard");
        }
    }
}
