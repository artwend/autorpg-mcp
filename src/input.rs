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

/// Maps a key name to a physical key scancode.
///
/// Accepts a single character (e.g. "e", "1", " ") or a named key
/// (e.g. "space", "enter", "escape", "tab", "f1"). Scancodes are used so the
/// press reaches DirectInput/RawInput game engines regardless of the active
/// keyboard layout.
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
        "printscreen" | "prtsc" => Some(0x37),
        "scrolllock" => Some(0x46),
        "pause" => Some(0x45),
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
            // Function keys F1-F12.
            if let Some(n) = lower.strip_prefix('f').and_then(|s| s.parse::<u8>().ok())
                && (1..=12).contains(&n)
            {
                return Some((0x3A + n) as u16);
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

    /// Presses a raw scancode, waits, then releases it (blocking; call from a blocking thread).
    pub fn hold_scancode<I: Keyboard + ?Sized>(
        input: &mut I,
        scancode: u16,
        duration: Duration,
    ) -> Result<(), InputError> {
        input.raw(scancode, Direction::Press)?;
        std::thread::sleep(duration);
        input.raw(scancode, Direction::Release)
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
