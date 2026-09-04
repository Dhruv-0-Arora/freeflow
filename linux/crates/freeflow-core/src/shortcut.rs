//! Dictation shortcut session state machine.
//!
//! Ported from `Sources/ShortcutCore/DictationShortcutSessionController.swift`
//! and the event/mode models in `Sources/ShortcutCore/ShortcutModels.swift`.
//! Behaviour is intended to be identical; only the language changed.
//!
//! Deliberately **not** ported: `ShortcutMatcher` and `ShortcutBinding` from the
//! same directory. Those reduce raw macOS virtual keycodes and `NSEvent`
//! modifier masks into the [`ShortcutEvent`]s consumed here. On Wayland the
//! compositor owns key binding, so `freeflow-cli` emits [`ShortcutEvent`]
//! directly from Hyprland `bind`/`bindr` dispatches and there is no keycode
//! layer to reduce. Reviving them would only make sense alongside an evdev
//! input backend.

use serde::{Deserialize, Serialize};

/// Whether a dictation session stops on key release or on a second press.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordingTriggerMode {
    /// Push-to-talk: recording lasts as long as the shortcut is held.
    Hold,
    /// Tap to start, tap again to stop.
    Toggle,
}

/// An edge-triggered shortcut signal.
///
/// On Linux these arrive from the compositor via `freeflow-cli`: a Hyprland
/// `bind` produces an `*Activated` event and the matching `bindr` produces the
/// `*Deactivated` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShortcutEvent {
    HoldActivated,
    HoldDeactivated,
    ToggleActivated,
    ToggleDeactivated,
    CopyAgainTriggered,
}

/// What the daemon should do in response to a [`ShortcutEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictationShortcutAction {
    Start(RecordingTriggerMode),
    Stop,
    /// A hold session was latched into toggle mode; recording continues.
    SwitchedToToggle,
}

/// Tracks whether a dictation session is active and how it will end.
#[derive(Debug, Default)]
pub struct DictationShortcutSessionController {
    active_mode: Option<RecordingTriggerMode>,
    toggle_stop_armed: bool,
}

impl DictationShortcutSessionController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn active_mode(&self) -> Option<RecordingTriggerMode> {
        self.active_mode
    }

    pub fn toggle_stop_armed(&self) -> bool {
        self.toggle_stop_armed
    }

    pub fn handle(
        &mut self,
        event: ShortcutEvent,
        is_transcribing: bool,
    ) -> Option<DictationShortcutAction> {
        use DictationShortcutAction as Action;
        use RecordingTriggerMode as Mode;
        use ShortcutEvent as Event;

        // Paste Again is handled before this controller runs; if it ever
        // reaches here, treat as a no-op so dictation state is unaffected.
        if event == Event::CopyAgainTriggered {
            return None;
        }

        let Some(mode) = self.active_mode else {
            // No session yet. Ignore new starts while a previous transcript is
            // still being processed.
            if is_transcribing {
                return None;
            }
            return match event {
                Event::ToggleActivated => {
                    self.begin_manual(Mode::Toggle);
                    Some(Action::Start(Mode::Toggle))
                }
                Event::HoldActivated => {
                    self.begin_manual(Mode::Hold);
                    Some(Action::Start(Mode::Hold))
                }
                Event::HoldDeactivated | Event::ToggleDeactivated | Event::CopyAgainTriggered => {
                    None
                }
            };
        };

        match mode {
            Mode::Hold => match event {
                // Latch: adding the toggle shortcut mid-hold converts the
                // session so it survives releasing the hold key.
                Event::ToggleActivated => {
                    self.force_toggle_mode();
                    Some(Action::SwitchedToToggle)
                }
                Event::HoldDeactivated => {
                    self.reset();
                    Some(Action::Stop)
                }
                Event::HoldActivated | Event::ToggleDeactivated | Event::CopyAgainTriggered => None,
            },
            Mode::Toggle => match event {
                // Arm on release of the starting tap so the same physical tap
                // cannot immediately stop the session it just started.
                Event::ToggleDeactivated => {
                    self.toggle_stop_armed = true;
                    None
                }
                Event::ToggleActivated => {
                    if !self.toggle_stop_armed {
                        return None;
                    }
                    self.reset();
                    Some(Action::Stop)
                }
                Event::HoldActivated | Event::HoldDeactivated | Event::CopyAgainTriggered => None,
            },
        }
    }

    pub fn begin_manual(&mut self, mode: RecordingTriggerMode) {
        self.active_mode = Some(mode);
        self.toggle_stop_armed = false;
    }

    pub fn force_toggle_mode(&mut self) {
        self.active_mode = Some(RecordingTriggerMode::Toggle);
        self.toggle_stop_armed = false;
    }

    pub fn reset(&mut self) {
        self.active_mode = None;
        self.toggle_stop_armed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::DictationShortcutAction::*;
    use super::RecordingTriggerMode::*;
    use super::ShortcutEvent::*;
    use super::*;

    // Ported from Tests/ShortcutCoreTests.swift:testHoldSessionControllerLifecycle
    #[test]
    fn hold_session_lifecycle() {
        let mut c = DictationShortcutSessionController::new();
        assert_eq!(c.handle(HoldActivated, true), None);
        assert_eq!(c.handle(HoldActivated, false), Some(Start(Hold)));
        assert_eq!(c.handle(HoldDeactivated, false), Some(Stop));
        assert_eq!(c.active_mode(), None);
    }

    // Ported from Tests/ShortcutCoreTests.swift:testToggleSessionControllerLifecycle
    #[test]
    fn toggle_session_lifecycle() {
        let mut c = DictationShortcutSessionController::new();
        assert_eq!(c.handle(ToggleActivated, false), Some(Start(Toggle)));
        assert_eq!(c.handle(ToggleActivated, false), None);
        assert_eq!(c.handle(ToggleDeactivated, false), None);
        assert!(c.toggle_stop_armed());
        assert_eq!(c.handle(ToggleActivated, false), Some(Stop));
        assert_eq!(c.active_mode(), None);
    }

    // Ported from Tests/ShortcutCoreTests.swift:testHoldToToggleSessionControllerLifecycle
    #[test]
    fn hold_to_toggle_session_lifecycle() {
        let mut c = DictationShortcutSessionController::new();
        assert_eq!(c.handle(HoldActivated, false), Some(Start(Hold)));
        assert_eq!(c.handle(ToggleActivated, false), Some(SwitchedToToggle));
        assert_eq!(c.handle(HoldDeactivated, false), None);
        assert_eq!(c.active_mode(), Some(Toggle));
        assert_eq!(c.handle(CopyAgainTriggered, false), None);

        c.begin_manual(Hold);
        assert_eq!(c.active_mode(), Some(Hold));
        c.force_toggle_mode();
        assert_eq!(c.active_mode(), Some(Toggle));
        c.reset();
        assert_eq!(c.active_mode(), None);
        assert!(!c.toggle_stop_armed());
    }

    // New: a latched session must still stop on a toggle tap, which is the
    // whole point of latching. The Swift suite stops short of asserting this.
    #[test]
    fn latched_session_stops_on_toggle_tap() {
        let mut c = DictationShortcutSessionController::new();
        assert_eq!(c.handle(HoldActivated, false), Some(Start(Hold)));
        assert_eq!(c.handle(ToggleActivated, false), Some(SwitchedToToggle));
        assert_eq!(c.handle(HoldDeactivated, false), None);
        assert_eq!(c.handle(ToggleDeactivated, false), None);
        assert_eq!(c.handle(ToggleActivated, false), Some(Stop));
        assert_eq!(c.active_mode(), None);
    }

    // New: `is_transcribing` must only gate *starting* a session, never
    // stopping one, or a slow provider could strand the user in a live
    // recording they cannot end.
    #[test]
    fn transcribing_does_not_block_stopping() {
        let mut c = DictationShortcutSessionController::new();
        assert_eq!(c.handle(HoldActivated, false), Some(Start(Hold)));
        assert_eq!(c.handle(HoldDeactivated, true), Some(Stop));
        assert_eq!(c.active_mode(), None);
    }
}
