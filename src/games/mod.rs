use crate::state::{GameMetrics, RgbView};

pub mod action_rpg;

/// Pixel dimensions of the frame passed to [`GameProfile::parse_telemetry`].
///
/// Taken from the published preview itself, so a profile scales its UI coordinates against
/// the space the frame it is handed was actually decoded in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

pub trait GameProfile: Send + Sync {
    fn id(&self) -> &'static str;

    /// Reads live telemetry out of `pixels`, which is laid out for `resolution`.
    ///
    /// The view borrows the capture thread's published preview directly, so parsing never
    /// copies the frame.
    fn parse_telemetry(&self, pixels: &RgbView, resolution: Resolution) -> GameMetrics;
}