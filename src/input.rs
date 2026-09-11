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

    use enigo::{Direction, InputError, Key, Keyboard};

    /// Presses a key, waits, then releases it (blocking; call from a blocking thread).
    pub fn hold_key<I: Keyboard + ?Sized>(
        input: &mut I,
        key: Key,
        duration: Duration,
    ) -> Result<(), InputError> {
        input.key(key, Direction::Press)?;
        std::thread::sleep(duration);
        input.key(key, Direction::Release)
    }

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
}
