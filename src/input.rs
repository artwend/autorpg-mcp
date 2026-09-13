//! Input simulation engine: WASD movement, keyboard and mouse control.
//!
//! Two interchangeable backends are supported behind the `input-simulator`
//! Cargo feature; both implement the `enigo` `Keyboard`/`Mouse` traits, so the
//! generic [`crate::server::GameServer`] works with either.

#[cfg(not(feature = "input-simulator"))]
use enigo::{Enigo, Settings as EnigoSettings};
#[cfg(feature = "input-simulator")]
use input_simulator::{InputSimulator, Settings as InputSimSettings};

/// Maps a movement direction name to a physical key scancode.
/// Scancodes are used (instead of characters) so movement works
/// regardless of the active keyboard layout.
pub fn direction_scancode(direction: &str) -> Option<u16> {
    match direction.to_lowercase().as_str() {
        "forward" | "up" | "w" => Some(0x11),             // W
        "left" | "a" => Some(0x1E),                       // A
        "back" | "backward" | "down" | "s" => Some(0x1F), // S
        "right" | "d" => Some(0x20),                      // D
        _ => None,
    }
}

/// Maps a key name to a physical key scancode (PS/2 Set 1).
///
/// Accepts a single character (e.g. "e", "1", " ") or a named key
/// (e.g. "space", "enter", "escape", "tab", "f1"). Scancodes are used so the
/// press reaches DirectInput/RawInput game engines regardless of the active
/// keyboard layout.
///
/// Navigation keys ("up", "down", "left", "right", "insert", "delete", "home",
/// "end", "pageup", "pagedown") are extended (E0-prefixed) keys. Their Set 1
/// scancodes are shared with the numpad, so they are only distinguishable with
/// the extended-key flag: the enigo backend sets it automatically when
/// translating the scancode back to a virtual key, and the input-simulator
/// backend receives a converted virtual key ([`scancode_to_vk`]) instead.
pub fn key_scancode(key: &str) -> Option<u16> {
    let lower = key.to_lowercase();
    match lower.as_str() {
        "space" => Some(0x39),
        "enter" | "return" => Some(0x1C),
        "escape" | "esc" => Some(0x01),
        "tab" => Some(0x0F),
        "backspace" => Some(0x0E),
        "delete" | "del" => Some(0x53),
        "insert" | "ins" => Some(0x52),
        "home" => Some(0x47),
        "end" => Some(0x4F),
        "pageup" | "pgup" => Some(0x49),
        "pagedown" | "pgdn" => Some(0x51),
        "up" => Some(0x48),
        "down" => Some(0x50),
        "left" => Some(0x4B),
        "right" => Some(0x4D),
        "shift" => Some(0x2A),
        "ctrl" | "control" => Some(0x1D),
        "alt" => Some(0x38),
        "capslock" | "caps" => Some(0x3A),
        "scrolllock" => Some(0x46),
        "numlock" => Some(0x45),
        // NOTE: "pause" (E1 14 45, no usable plain scancode) and "printscreen"
        // (E0 37) are extended-sequence keys with no distinct non-extended Set 1
        // scancode; mapping them to 0x45/0x37 would send NumLock/NumPad-*
        // instead, so they are deliberately not mapped.
        "minus" => Some(0x0C),
        "equals" | "equal" => Some(0x0D),
        "leftbracket" | "lbracket" => Some(0x1A),
        "rightbracket" | "rbracket" => Some(0x1B),
        "backslash" => Some(0x2B),
        "semicolon" => Some(0x27),
        "apostrophe" | "quote" => Some(0x28),
        "backquote" | "grave" => Some(0x29),
        "comma" => Some(0x33),
        "period" | "dot" => Some(0x34),
        "slash" => Some(0x35),
        _ => {
            // Function keys F1-F12. F1-F10 are sequential Set 1 scancodes
            // 0x3B..0x44, but F11 (0x57) and F12 (0x58) sit in a separate block:
            // 0x3A + 11/12 would collide with Pause (0x45) and ScrollLock (0x46).
            if let Some(n) = lower.strip_prefix('f').and_then(|s| s.parse::<u8>().ok()) {
                match n {
                    1..=10 => return Some((0x3A + n) as u16),
                    11 => return Some(0x57),
                    12 => return Some(0x58),
                    _ => return None,
                }
            }
            // Single character.
            let mut chars = key.chars();
            let ch = chars.next()?;
            if chars.next().is_none() {
                char_scancode(ch)
            } else {
                None
            }
        }
    }
}

/// Maps a single character to its physical key scancode.
fn char_scancode(ch: char) -> Option<u16> {
    match ch {
        'a' | 'A' => Some(0x1E),
        'b' | 'B' => Some(0x30),
        'c' | 'C' => Some(0x2E),
        'd' | 'D' => Some(0x20),
        'e' | 'E' => Some(0x12),
        'f' | 'F' => Some(0x21),
        'g' | 'G' => Some(0x22),
        'h' | 'H' => Some(0x23),
        'i' | 'I' => Some(0x17),
        'j' | 'J' => Some(0x24),
        'k' | 'K' => Some(0x25),
        'l' | 'L' => Some(0x26),
        'm' | 'M' => Some(0x32),
        'n' | 'N' => Some(0x31),
        'o' | 'O' => Some(0x18),
        'p' | 'P' => Some(0x19),
        'q' | 'Q' => Some(0x10),
        'r' | 'R' => Some(0x13),
        's' | 'S' => Some(0x1F),
        't' | 'T' => Some(0x14),
        'u' | 'U' => Some(0x16),
        'v' | 'V' => Some(0x2F),
        'w' | 'W' => Some(0x11),
        'x' | 'X' => Some(0x2D),
        'y' | 'Y' => Some(0x15),
        'z' | 'Z' => Some(0x2C),
        '1' => Some(0x02),
        '2' => Some(0x03),
        '3' => Some(0x04),
        '4' => Some(0x05),
        '5' => Some(0x06),
        '6' => Some(0x07),
        '7' => Some(0x08),
        '8' => Some(0x09),
        '9' => Some(0x0A),
        '0' => Some(0x0B),
        ' ' => Some(0x39),
        '-' => Some(0x0C),
        '=' => Some(0x0D),
        '[' => Some(0x1A),
        ']' => Some(0x1B),
        '\\' => Some(0x2B),
        ';' => Some(0x27),
        '\'' => Some(0x28),
        '`' => Some(0x29),
        ',' => Some(0x33),
        '.' => Some(0x34),
        '/' => Some(0x35),
        _ => None,
    }
}

/// Converts a PS/2 Set 1 scancode to its Windows virtual-key code.
///
/// The input-simulator backend's `Keyboard::raw` interprets its argument as a
/// virtual key (it maps straight onto the USB HID usage table), while the enigo
/// backend expects a raw Set 1 scancode (`KEYEVENTF_SCANCODE`). This table is
/// verified against `MapVirtualKeyW(code, MAPVK_VSC_TO_VK)` so the same
/// [`key_scancode`] value reaches both backends correctly; extended navigation
/// scancodes map to their navigation virtual keys (not their numpad aliases).
#[cfg_attr(not(feature = "input-simulator"), allow(dead_code))]
pub fn scancode_to_vk(scancode: u16) -> Option<u16> {
    let vk = match scancode {
        0x01 => 0x1B,                   // Escape
        0x02..=0x0A => scancode + 0x2F, // Digits 1-9 -> VK 0x31..0x39
        0x0B => 0x30,                   // 0
        0x0C => 0xBD,                   // -
        0x0D => 0xBB,                   // =
        0x0E => 0x08,                   // Backspace
        0x0F => 0x09,                   // Tab
        0x10 => 0x51,                   // Q
        0x11 => 0x57,                   // W
        0x12 => 0x45,                   // E
        0x13 => 0x52,                   // R
        0x14 => 0x54,                   // T
        0x15 => 0x59,                   // Y
        0x16 => 0x55,                   // U
        0x17 => 0x49,                   // I
        0x18 => 0x4F,                   // O
        0x19 => 0x50,                   // P
        0x1A => 0xDB,                   // [
        0x1B => 0xDD,                   // ]
        0x1C => 0x0D,                   // Enter
        0x1D => 0x11,                   // Left Ctrl
        0x1E => 0x41,                   // A
        0x1F => 0x53,                   // S
        0x20 => 0x44,                   // D
        0x21 => 0x46,                   // F
        0x22 => 0x47,                   // G
        0x23 => 0x48,                   // H
        0x24 => 0x4A,                   // J
        0x25 => 0x4B,                   // K
        0x26 => 0x4C,                   // L
        0x27 => 0xBA,                   // ;
        0x28 => 0xDE,                   // '
        0x29 => 0xC0,                   // `
        0x2A => 0x10,                   // Left Shift
        0x2B => 0xDC,                   // \
        0x2C => 0x5A,                   // Z
        0x2D => 0x58,                   // X
        0x2E => 0x43,                   // C
        0x2F => 0x56,                   // V
        0x30 => 0x42,                   // B
        0x31 => 0x4E,                   // N
        0x32 => 0x4D,                   // M
        0x33 => 0xBC,                   // ,
        0x34 => 0xBE,                   // .
        0x35 => 0xBF,                   // /
        0x36 => 0x10,                   // Right Shift
        0x38 => 0x12,                   // Left Alt
        0x39 => 0x20,                   // Space
        0x3A => 0x14,                   // CapsLock
        0x3B..=0x44 => scancode + 0x35, // F1-F10 -> VK_F1 0x70..VK_F10 0x79
        0x45 => 0x90,                   // NumLock
        0x46 => 0x91,                   // ScrollLock
        0x47 => 0x24,                   // Home (extended)
        0x48 => 0x26,                   // Up (extended; NOT numpad 8 / VK_NUMPAD8 0x68)
        0x49 => 0x21,                   // PageUp (extended)
        0x4B => 0x25,                   // Left (extended)
        0x4D => 0x27,                   // Right (extended)
        0x4F => 0x23,                   // End (extended)
        0x50 => 0x28,                   // Down (extended)
        0x51 => 0x22,                   // PageDown (extended)
        0x52 => 0x2D,                   // Insert (extended)
        0x53 => 0x2E,                   // Delete (extended)
        0x57 => 0x7A,                   // F11
        0x58 => 0x7B,                   // F12
        _ => return None,
    };
    Some(vk)
}

/// Builds the configured input backend (enigo by default, input-simulator
/// when the `input-simulator` feature is enabled).
#[cfg(not(feature = "input-simulator"))]
pub fn create_input() -> Result<Enigo, enigo::NewConError> {
    Enigo::new(&EnigoSettings::default())
}

#[cfg(feature = "input-simulator")]
pub fn create_input() -> Result<InputSimulator, enigo::NewConError> {
    InputSimulator::new(&InputSimSettings::default())
}

/// Convenience helpers shared by the tool handlers.
pub mod ops {
    use std::time::Duration;

    use enigo::{Axis, Direction, InputError, Keyboard, Mouse};

    #[cfg(feature = "input-simulator")]
    use crate::input::scancode_to_vk;

    /// Reports whether `scancode` can be dispatched by the active backend.
    ///
    /// The enigo backend accepts every Set 1 scancode, but the input-simulator
    /// backend works in virtual-key space, so a scancode with no virtual-key
    /// equivalent cannot be sent. Checking this from the async tool handler, before
    /// the blocking task is spawned, lets the failure surface as an
    /// `INVALID_PARAMS` error instead of an internal one.
    #[cfg(not(feature = "input-simulator"))]
    pub fn scancode_supported(_scancode: u16) -> bool {
        true
    }

    /// Reports whether `scancode` can be dispatched by the active backend.
    #[cfg(feature = "input-simulator")]
    pub fn scancode_supported(scancode: u16) -> bool {
        scancode_to_vk(scancode).is_some()
    }

    /// Presses a raw key, waits, then releases it (blocking; call from a blocking thread).
    ///
    /// `scancode` is a PS/2 Set 1 scancode as produced by [`key_scancode`] and
    /// [`direction_scancode`]. Under the enigo backend it is passed through
    /// (enigo re-translates it to a virtual key and adds the extended-key flag
    /// for E0 keys); under the input-simulator backend it is converted to the
    /// virtual key that backend expects.
    pub fn hold_scancode<I: Keyboard + ?Sized>(
        input: &mut I,
        scancode: u16,
        duration: Duration,
    ) -> Result<(), InputError> {
        #[cfg(feature = "input-simulator")]
        let code = scancode_to_vk(scancode).ok_or_else(|| {
            InputError::Mapping(format!(
                "scancode 0x{scancode:X} has no virtual-key equivalent"
            ))
        })?;
        #[cfg(not(feature = "input-simulator"))]
        let code = scancode;

        input.raw(code, Direction::Press)?;
        std::thread::sleep(duration);
        input.raw(code, Direction::Release)
    }

    /// Types a string of text via Unicode text events (for text input fields).
    ///
    /// Game engines ignore `KEYEVENTF_UNICODE` events outside of text input
    /// fields, so use [`hold_scancode`] for gameplay input.
    pub fn type_text<I: Keyboard + ?Sized>(input: &mut I, text: &str) -> Result<(), InputError> {
        input.text(text)
    }

    /// Scrolls the mouse wheel vertically by `amount` wheel clicks.
    ///
    /// Positive amounts scroll up (wheel rotated forward, matching Windows
    /// `WHEEL_DELTA`); negative amounts scroll down.
    #[cfg(not(feature = "input-simulator"))]
    pub fn scroll_vertical<I: Mouse + ?Sized>(
        input: &mut I,
        amount: i32,
    ) -> Result<(), InputError> {
        // enigo's `scroll` treats a positive length as scrolling down, so negate
        // to expose the Windows `WHEEL_DELTA` convention (positive = up).
        input.scroll(-amount, Axis::Vertical)
    }

    /// Scrolls the mouse wheel vertically by `amount` wheel clicks.
    ///
    /// Positive amounts scroll up (wheel rotated forward, matching Windows
    /// `WHEEL_DELTA`); negative amounts scroll down.
    #[cfg(feature = "input-simulator")]
    pub fn scroll_vertical<I: Mouse + ?Sized>(
        input: &mut I,
        amount: i32,
    ) -> Result<(), InputError> {
        // input-simulator's `scroll` already treats a positive length as scrolling up.
        input.scroll(amount, Axis::Vertical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_scancode_maps_wasd() {
        assert_eq!(direction_scancode("forward"), Some(0x11)); // W
        assert_eq!(direction_scancode("back"), Some(0x1F)); // S
        assert_eq!(direction_scancode("backward"), Some(0x1F));
        assert_eq!(direction_scancode("left"), Some(0x1E)); // A
        assert_eq!(direction_scancode("right"), Some(0x20)); // D
    }

    #[test]
    fn direction_scancode_accepts_aliases_and_case() {
        assert_eq!(direction_scancode("W"), Some(0x11));
        assert_eq!(direction_scancode("Up"), Some(0x11));
        assert_eq!(direction_scancode("DOWN"), Some(0x1F));
        assert_eq!(direction_scancode("a"), Some(0x1E));
        assert_eq!(direction_scancode("d"), Some(0x20));
    }

    #[test]
    fn direction_scancode_rejects_unknown() {
        assert_eq!(direction_scancode("diagonal"), None);
        assert_eq!(direction_scancode(""), None);
    }

    #[test]
    fn named_keys_map_to_expected_scancodes() {
        assert_eq!(key_scancode("space"), Some(0x39));
        assert_eq!(key_scancode("enter"), Some(0x1C));
        assert_eq!(key_scancode("return"), Some(0x1C));
        assert_eq!(key_scancode("escape"), Some(0x01));
        assert_eq!(key_scancode("esc"), Some(0x01));
        assert_eq!(key_scancode("tab"), Some(0x0F));
        assert_eq!(key_scancode("backspace"), Some(0x0E));
        assert_eq!(key_scancode("shift"), Some(0x2A));
        assert_eq!(key_scancode("ctrl"), Some(0x1D));
        assert_eq!(key_scancode("alt"), Some(0x38));
        assert_eq!(key_scancode("capslock"), Some(0x3A));
        assert_eq!(key_scancode("scrolllock"), Some(0x46));
        assert_eq!(key_scancode("numlock"), Some(0x45));
    }

    #[test]
    fn function_keys_use_their_real_scancodes() {
        // F1-F10 are sequential: 0x3B..=0x44.
        assert_eq!(key_scancode("f1"), Some(0x3B));
        assert_eq!(key_scancode("F5"), Some(0x3F));
        assert_eq!(key_scancode("f10"), Some(0x44));
        // F11/F12 live in the 0x57/0x58 block; 0x3A + 11/12 would be Pause
        // (0x45) and ScrollLock (0x46) instead.
        assert_eq!(key_scancode("f11"), Some(0x57));
        assert_eq!(key_scancode("F12"), Some(0x58));
        // Non-existent function keys must fail instead of wrapping around.
        assert_eq!(key_scancode("f0"), None);
        assert_eq!(key_scancode("f13"), None);
        assert_eq!(key_scancode("f99"), None);
    }

    #[test]
    fn navigation_keys_do_not_collide_with_numpad() {
        // Set 1 scancodes of the extended navigation keys. They are shared with
        // numpad scancodes, but each must convert to a distinct navigation
        // virtual key, never a numpad virtual key.
        assert_eq!(key_scancode("up"), Some(0x48));
        assert_eq!(key_scancode("down"), Some(0x50));
        assert_eq!(key_scancode("left"), Some(0x4B));
        assert_eq!(key_scancode("right"), Some(0x4D));
        assert_eq!(key_scancode("delete"), Some(0x53));
        assert_eq!(key_scancode("del"), Some(0x53));
        assert_eq!(key_scancode("insert"), Some(0x52));
        assert_eq!(key_scancode("ins"), Some(0x52));
        assert_eq!(key_scancode("home"), Some(0x47));
        assert_eq!(key_scancode("end"), Some(0x4F));
        assert_eq!(key_scancode("pageup"), Some(0x49));
        assert_eq!(key_scancode("pagedown"), Some(0x51));

        // Numpad virtual keys are 0x60..0x69 (VK_NUMPAD0..VK_NUMPAD9); none of
        // the navigation scancodes may map into that range, which is what a
        // plain Set 1 -> numpad aliasing bug would produce.
        let navigation = [
            "up", "down", "left", "right", "delete", "insert", "home", "end", "pageup", "pagedown",
        ];
        for key in navigation {
            let vk = scancode_to_vk(key_scancode(key).expect("mapped")).expect("vk");
            assert!(
                !(0x60..=0x69).contains(&vk),
                "{key} must not map to a numpad virtual key (got 0x{vk:X})"
            );
        }
        assert_eq!(scancode_to_vk(0x48), Some(0x26)); // VK_UP, not VK_NUMPAD8 0x68
        assert_eq!(scancode_to_vk(0x50), Some(0x28)); // VK_DOWN, not VK_NUMPAD2 0x62
        assert_eq!(scancode_to_vk(0x4B), Some(0x25)); // VK_LEFT, not VK_NUMPAD4 0x64
        assert_eq!(scancode_to_vk(0x4D), Some(0x27)); // VK_RIGHT, not VK_NUMPAD6 0x66
        assert_eq!(scancode_to_vk(0x53), Some(0x2E)); // VK_DELETE, not VK_DECIMAL 0x6E
        assert_eq!(scancode_to_vk(0x52), Some(0x2D)); // VK_INSERT, not VK_NUMPAD0 0x60
    }

    #[test]
    fn scancode_to_vk_matches_map_virtual_key_for_common_keys() {
        // Spot checks verified against MapVirtualKeyW(code, MAPVK_VSC_TO_VK).
        assert_eq!(scancode_to_vk(0x11), Some(0x57)); // W
        assert_eq!(scancode_to_vk(0x1E), Some(0x41)); // A
        assert_eq!(scancode_to_vk(0x1F), Some(0x53)); // S
        assert_eq!(scancode_to_vk(0x20), Some(0x44)); // D
        assert_eq!(scancode_to_vk(0x39), Some(0x20)); // Space
        assert_eq!(scancode_to_vk(0x1C), Some(0x0D)); // Enter
        assert_eq!(scancode_to_vk(0x01), Some(0x1B)); // Escape
        assert_eq!(scancode_to_vk(0x02), Some(0x31)); // 1
        assert_eq!(scancode_to_vk(0x0B), Some(0x30)); // 0
        assert_eq!(scancode_to_vk(0x3B), Some(0x70)); // F1
        assert_eq!(scancode_to_vk(0x44), Some(0x79)); // F10
        assert_eq!(scancode_to_vk(0x57), Some(0x7A)); // F11
        assert_eq!(scancode_to_vk(0x58), Some(0x7B)); // F12
        assert_eq!(scancode_to_vk(0x45), Some(0x90)); // NumLock
        assert_eq!(scancode_to_vk(0x2A), Some(0x10)); // Shift
        assert_eq!(scancode_to_vk(0x1D), Some(0x11)); // Ctrl
        assert_eq!(scancode_to_vk(0x38), Some(0x12)); // Alt
    }

    #[test]
    fn scancode_to_vk_rejects_unmapped_scancodes() {
        assert_eq!(scancode_to_vk(0x00), None);
        assert_eq!(scancode_to_vk(0x37), None); // PrintScreen (E0 37, no plain scancode)
        assert_eq!(scancode_to_vk(0xFF), None);
    }

    #[test]
    fn single_characters_map_to_scancodes() {
        assert_eq!(key_scancode("e"), Some(0x12));
        assert_eq!(key_scancode("E"), Some(0x12));
        assert_eq!(key_scancode("q"), Some(0x10));
        assert_eq!(key_scancode("z"), Some(0x2C));
        assert_eq!(key_scancode("1"), Some(0x02));
        assert_eq!(key_scancode("0"), Some(0x0B));
        assert_eq!(key_scancode(" "), Some(0x39));
        assert_eq!(key_scancode("-"), Some(0x0C));
        assert_eq!(key_scancode("="), Some(0x0D));
        assert_eq!(key_scancode("["), Some(0x1A));
        assert_eq!(key_scancode("]"), Some(0x1B));
        assert_eq!(key_scancode("\\"), Some(0x2B));
        assert_eq!(key_scancode(";"), Some(0x27));
        assert_eq!(key_scancode("'"), Some(0x28));
        assert_eq!(key_scancode("`"), Some(0x29));
        assert_eq!(key_scancode(","), Some(0x33));
        assert_eq!(key_scancode("."), Some(0x34));
        assert_eq!(key_scancode("/"), Some(0x35));
    }

    #[test]
    fn key_scancode_rejects_unknown_names_and_multichar_strings() {
        assert_eq!(key_scancode("ab"), None);
        assert_eq!(key_scancode("hello"), None);
        assert_eq!(key_scancode(""), None);
        // Non-US-ASCII characters have no physical key on a standard layout.
        assert_eq!(key_scancode("é"), None);
        assert_eq!(key_scancode("printscreen"), None);
        assert_eq!(key_scancode("pause"), None);
    }
}
