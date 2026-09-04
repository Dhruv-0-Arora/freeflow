//! Foreground-window context from the compositor.
//!
//! Replaces `Sources/AppContextService.swift`, which drove the macOS
//! Accessibility API (`AXUIElementCreateApplication`, `kAXFocusedUIElement`,
//! `kAXSelectedText`) and needed the user to grant Accessibility permission.
//!
//! Hyprland exposes the same information over its IPC socket with no
//! permission prompt at all, so this is both simpler and less invasive than
//! the original. Screenshot-based context is deliberately not ported: it
//! required a vision model that does not fit in 8 GB of VRAM, and window title
//! plus selected text carries most of the value.
//!
//! We shell out to `hyprctl` rather than using a Hyprland client crate: the
//! published crates lag the compositor by a long way, and a mismatched
//! protocol version would break the build for no benefit. Only a handful of
//! fields are deserialized, so compositor updates cannot break parsing.

use serde::Deserialize;

/// What the cleanup prompt is told about where the text is going.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowContext {
    /// Window class, e.g. `foot`, `firefox`. Used for per-app injection
    /// overrides as well as for the prompt.
    pub class: Option<String>,
    pub title: Option<String>,
    /// Hyprland window address, so the daemon can re-focus the original window
    /// before injecting if focus moved during transcription.
    pub address: Option<String>,
    /// XWayland clients are the ones most likely to mishandle synthetic keys.
    pub xwayland: bool,
}

#[derive(Debug, Deserialize)]
struct ActiveWindow {
    #[serde(default)]
    class: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    address: String,
    #[serde(default)]
    xwayland: bool,
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Parses `hyprctl activewindow -j` output.
///
/// Split out from the command invocation so the shape handling is testable
/// without a compositor.
pub fn parse_active_window(json: &str) -> WindowContext {
    // Hyprland prints `{}` when nothing is focused, which is not an error.
    let Ok(window) = serde_json::from_str::<ActiveWindow>(json) else {
        return WindowContext::default();
    };
    WindowContext {
        class: non_empty(window.class),
        title: non_empty(window.title),
        address: non_empty(window.address),
        xwayland: window.xwayland,
    }
}

/// Reads the currently focused window.
///
/// Never fails: context is an enhancement to the prompt, and losing it must
/// not cost the user their dictation.
pub fn active_window() -> WindowContext {
    match std::process::Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
    {
        Ok(output) if output.status.success() => {
            parse_active_window(&String::from_utf8_lossy(&output.stdout))
        }
        Ok(output) => {
            tracing::debug!(status = ?output.status, "hyprctl activewindow failed");
            WindowContext::default()
        }
        Err(e) => {
            tracing::debug!(error = %e, "hyprctl is not available");
            WindowContext::default()
        }
    }
}

/// Focuses a window by Hyprland address.
///
/// Used before injection when focus moved while the transcript was being
/// prepared, so the text lands where the user was actually typing.
pub fn focus_window(address: &str) -> bool {
    std::process::Command::new("hyprctl")
        .args(["dispatch", "focuswindow", &format!("address:{address}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

impl WindowContext {
    /// One-line summary for the `CONTEXT:` field of the prompt.
    ///
    /// Kept terse on purpose. The prompt uses context only as a formatting
    /// hint and a spelling reference, so more detail buys nothing and costs
    /// tokens on every dictation.
    pub fn summary(&self) -> String {
        match (&self.class, &self.title) {
            (Some(class), Some(title)) => format!("App: {class}; Window: {title}"),
            (Some(class), None) => format!("App: {class}"),
            (None, Some(title)) => format!("Window: {title}"),
            (None, None) => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_hyprctl_payload() {
        // Trimmed from actual `hyprctl activewindow -j` output; unknown fields
        // must be ignored so a compositor update cannot break dictation.
        let json = r#"{
            "address": "0x619f015b4510",
            "mapped": true,
            "at": [14, 14],
            "size": [3412, 1412],
            "workspace": { "id": -1342, "name": "i" },
            "floating": false,
            "monitor": 0,
            "class": "foot",
            "title": "default | synthetic title",
            "pid": 15885,
            "xwayland": false
        }"#;
        let ctx = parse_active_window(json);
        assert_eq!(ctx.class.as_deref(), Some("foot"));
        assert_eq!(ctx.title.as_deref(), Some("default | synthetic title"));
        assert_eq!(ctx.address.as_deref(), Some("0x619f015b4510"));
        assert!(!ctx.xwayland);
        assert_eq!(
            ctx.summary(),
            "App: foot; Window: default | synthetic title"
        );
    }

    #[test]
    fn an_empty_or_malformed_payload_is_not_an_error() {
        // Hyprland prints `{}` when no window has focus.
        assert_eq!(parse_active_window("{}"), WindowContext::default());
        assert_eq!(parse_active_window(""), WindowContext::default());
        assert_eq!(parse_active_window("not json"), WindowContext::default());
        assert_eq!(WindowContext::default().summary(), "");
    }

    #[test]
    fn blank_fields_are_treated_as_absent() {
        let ctx = parse_active_window(r#"{"class":"","title":"   ","address":"0x1"}"#);
        assert_eq!(ctx.class, None);
        assert_eq!(ctx.title, None);
        assert_eq!(ctx.address.as_deref(), Some("0x1"));
        assert_eq!(ctx.summary(), "");
    }

    #[test]
    fn xwayland_clients_are_flagged_for_injection_decisions() {
        let ctx = parse_active_window(r#"{"class":"Xterm","title":"t","xwayland":true}"#);
        assert!(ctx.xwayland);
    }
}
