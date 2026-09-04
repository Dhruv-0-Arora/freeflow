//! Hyprland key names to Linux evdev key codes.
//!
//! The daemon watches `/dev/input` for the release of the key that started a
//! dictation session. It cannot ask the compositor: a Hyprland release bind
//! (`bindr`) silently consumes the bound key's *press*, and the `i` flag
//! needed to catch a release after the modifier is lifted makes that
//! consumption apply to every modifier state - which is how `bindri` binds on
//! D and T once swallowed all ordinary typing of `d` and `t`.
//!
//! Names resolve to their positions in the standard US layout, the same
//! positions the compositor matched the bind against. A non-QWERTY xkb layout
//! that moves a letter will move the watched key with it only via `code:`.

/// Resolves a key name as written in a Hyprland bind to its evdev key code.
///
/// Accepts the spellings Hyprland does: letters, digits, function keys, the
/// common named keys, and `code:N` for a raw X keycode (X keycodes are evdev
/// codes offset by 8). Returns `None` for anything unrecognised.
pub fn keycode(name: &str) -> Option<u16> {
    let name = name.trim();
    if let Some(code) = name.strip_prefix("code:") {
        return code.trim().parse::<u16>().ok()?.checked_sub(8);
    }
    let lower = name.to_ascii_lowercase();
    let code = match lower.as_str() {
        "escape" => 1,
        "1" => 2,
        "2" => 3,
        "3" => 4,
        "4" => 5,
        "5" => 6,
        "6" => 7,
        "7" => 8,
        "8" => 9,
        "9" => 10,
        "0" => 11,
        "minus" => 12,
        "equal" => 13,
        "backspace" => 14,
        "tab" => 15,
        "q" => 16,
        "w" => 17,
        "e" => 18,
        "r" => 19,
        "t" => 20,
        "y" => 21,
        "u" => 22,
        "i" => 23,
        "o" => 24,
        "p" => 25,
        "bracketleft" => 26,
        "bracketright" => 27,
        "return" | "enter" => 28,
        "a" => 30,
        "s" => 31,
        "d" => 32,
        "f" => 33,
        "g" => 34,
        "h" => 35,
        "j" => 36,
        "k" => 37,
        "l" => 38,
        "semicolon" => 39,
        "apostrophe" => 40,
        "grave" => 41,
        "backslash" => 43,
        "z" => 44,
        "x" => 45,
        "c" => 46,
        "v" => 47,
        "b" => 48,
        "n" => 49,
        "m" => 50,
        "comma" => 51,
        "period" => 52,
        "slash" => 53,
        "space" => 57,
        "f1" => 59,
        "f2" => 60,
        "f3" => 61,
        "f4" => 62,
        "f5" => 63,
        "f6" => 64,
        "f7" => 65,
        "f8" => 66,
        "f9" => 67,
        "f10" => 68,
        "f11" => 87,
        "f12" => 88,
        // F13-F24 exist on no physical board, which makes them ideal
        // firmware-remap targets (e.g. a Wooting layer key sending F20).
        "f13" => 183,
        "f14" => 184,
        "f15" => 185,
        "f16" => 186,
        "f17" => 187,
        "f18" => 188,
        "f19" => 189,
        "f20" => 190,
        "f21" => 191,
        "f22" => 192,
        "f23" => 193,
        "f24" => 194,
        _ => return None,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_bind_keys_resolve() {
        // KEY_D and KEY_T, the hold and toggle defaults.
        assert_eq!(keycode("D"), Some(32));
        assert_eq!(keycode("T"), Some(20));
        assert_eq!(keycode("Escape"), Some(1));
    }

    #[test]
    fn names_match_however_hyprland_spells_them() {
        assert_eq!(keycode("d"), keycode("D"));
        assert_eq!(keycode(" t "), keycode("T"));
        assert_eq!(keycode("RETURN"), keycode("enter"));
    }

    #[test]
    fn high_function_keys_resolve_for_firmware_remaps() {
        assert_eq!(keycode("F20"), Some(190));
        assert_eq!(keycode("f13"), Some(183));
        assert_eq!(keycode("F24"), Some(194));
        assert_eq!(keycode("F25"), None);
    }

    #[test]
    fn raw_x_keycodes_pass_through_with_the_offset() {
        // X keycode 40 is evdev 32, i.e. KEY_D.
        assert_eq!(keycode("code:40"), Some(32));
        assert_eq!(keycode("code:4"), None, "below the X offset");
    }

    #[test]
    fn unknown_names_resolve_to_nothing() {
        assert_eq!(keycode("XF86AudioMute"), None);
        assert_eq!(keycode(""), None);
    }
}
