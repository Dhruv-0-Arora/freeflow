//! Watching `/dev/input` for a hotkey's release.
//!
//! The compositor cannot deliver release edges safely: a Hyprland release
//! bind consumes the bound key's press, and with the ignore-modifiers flag
//! that consumption applies to every unmodified press of the key - which is
//! how release binds on D and T once swallowed all ordinary typing of `d`
//! and `t`. So release detection happens below the compositor instead.
//!
//! The watcher polls key state (`EVIOCGKEY`) rather than reading event
//! streams: it is a couple of ioctls every few milliseconds, it needs no
//! non-blocking setup, and it can never interfere with input the way a grab
//! could. It runs only while a dictation session is active - the daemon arms
//! it on the session-starting press and drops it when the session ends - so
//! outside those moments nothing reads input devices at all.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use evdev::KeyCode;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KeyWatchError {
    #[error(
        "no readable /dev/input device carries key code {0}; \
         is this user in the `input` group?"
    )]
    NoDevice(u16),
    #[error("could not start the key watcher: {0}")]
    Io(#[from] std::io::Error),
}

const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A running watch on one key. Dropping it stops the watch; the poll thread
/// notices within one interval.
pub struct KeyWatcher {
    stop: Arc<AtomicBool>,
}

impl Drop for KeyWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Calls `on_release` once, as soon as no input device reports `keycode`
/// held down.
///
/// Every device advertising the key is polled - all keyboards, each event
/// node of a multi-interface keyboard, and remapper virtual devices alike -
/// so the release is seen no matter which device the press came from. If the
/// key is already up when the watch starts (a tap faster than the daemon's
/// round-trip), `on_release` fires on the first poll.
pub fn watch_release(
    keycode: u16,
    on_release: impl FnOnce() + Send + 'static,
) -> Result<KeyWatcher, KeyWatchError> {
    let key = KeyCode::new(keycode);
    let devices: Vec<evdev::Device> = evdev::enumerate()
        .filter(|(_, device)| {
            device
                .supported_keys()
                .is_some_and(|keys| keys.contains(key))
        })
        .map(|(_, device)| device)
        .collect();
    if devices.is_empty() {
        return Err(KeyWatchError::NoDevice(keycode));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    std::thread::Builder::new()
        .name("freeflow-keywatch".into())
        .spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                let held = devices.iter().any(|device| {
                    device
                        .get_key_state()
                        .is_ok_and(|state| state.contains(key))
                });
                if !held {
                    on_release();
                    return;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        })?;

    Ok(KeyWatcher { stop })
}

/// How many readable input devices carry the given key. Surfaced by
/// `freeflow doctor`: zero usually means the user is not in the `input`
/// group.
pub fn watchable_device_count(keycode: u16) -> usize {
    let key = KeyCode::new(keycode);
    evdev::enumerate()
        .filter(|(_, device)| {
            device
                .supported_keys()
                .is_some_and(|keys| keys.contains(key))
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Works both on a real desktop and in a build sandbox with no
    // /dev/input at all: nothing anywhere advertises a key beyond KEY_MAX's
    // usual range.
    #[test]
    fn a_key_no_device_carries_is_an_error_not_a_hang() {
        match watch_release(0x2fe, || {}) {
            Err(KeyWatchError::NoDevice(0x2fe)) => {}
            Err(other) => panic!("wrong error: {other}"),
            Ok(_) => panic!("some device claims to carry key 0x2fe"),
        }
    }
}
