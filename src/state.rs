//! In-memory session state shared between the MCP tools and the capture thread.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, RwLock};

use image::{ExtendedColorType, codecs::jpeg::JpegEncoder};

use crate::games::GameProfile;

/// Live game metrics tracked by the MCP session.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct GameMetrics {
    pub player_hp: i32,
    pub stamina: i32,
    pub q_ready: bool,
    pub r_ready: bool,
    pub f_ready: bool,
    // pub action_availability: std::collections::HashMap<String, bool>,
    pub location: String,
    pub in_combat: bool,
}

/// A single logged action with the metrics snapshot taken at that moment.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionEvent {
    pub timestamp: u64,
    pub action_taken: String,
    pub state_snapshot: GameMetrics,
}

/// Full session state: current metrics plus the event history.
#[derive(Clone)]
pub struct SessionState {
    pub current_metrics: GameMetrics,
    pub active_game: Arc<dyn GameProfile>,
    pub last_frame_hash: Option<u64>,
    pub event_history: VecDeque<SessionEvent>,
}

/// Maximum number of events retained in the session history.
/// Older events are dropped first, so the log cannot grow without bound.
pub const MAX_EVENT_HISTORY: usize = 512;

impl SessionState {
    /// Appends an event, snapshotting the current metrics, and evicts the oldest
    /// entry once [`MAX_EVENT_HISTORY`] is reached.
    ///
    /// `VecDeque` gives O(1) pop-front, so truncation never shifts the remaining
    /// entries the way `Vec::remove(0)` did.
    pub fn record_event(&mut self, action_taken: impl Into<String>) {
        if self.event_history.len() >= MAX_EVENT_HISTORY {
            self.event_history.pop_front();
        }

        self.event_history.push_back(SessionEvent {
            timestamp: unix_timestamp(),
            action_taken: action_taken.into(),
            state_snapshot: self.current_metrics.clone(),
        });
    }
}

/// Thread-safe pointer for state management (async readers/writers).
pub type SharedSession = Arc<RwLock<SessionState>>;

/// Latest captured frame shared between the capture thread and the MCP tools.
///
/// The preview pixels travel with their hash so the tools can read telemetry straight out
/// of the frame the capture thread already produced once, instead of re-encoding or
/// re-decoding anything on every call. JPEG encoding is deferred to the consumer: the
/// capture thread never pays for it while no MCP client is querying frames.
#[derive(Default, Debug)]
pub struct FramePayload {
    /// Tightly packed RGB8 pixels of the preview.
    pub rgb: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Native pixel size of the captured source (monitor or window) before the preview
    /// downscale. Absolute image-space mouse coordinates are scaled up with these
    /// dimensions; using the source rather than the display keeps the mapping correct
    /// when a single window is captured.
    pub source_width: u32,
    pub source_height: u32,
    /// Average hash of the preview, computed by the capture thread at publish time.
    pub hash: u64,
}

impl FramePayload {
    /// Borrows the published preview pixels as a zero-copy [`RgbView`].
    ///
    /// Telemetry only samples a sparse subset of pixels, so it reads straight out of the
    /// shared buffer instead of materializing a full `DynamicImage` copy (which cost a
    /// fresh ~1.7 MB allocation per call at the default 1024x576 preview size).
    pub fn as_rgb_view(&self) -> Option<RgbView<'_>> {
        RgbView::new(&self.rgb, self.width, self.height)
    }

    /// Encodes the preview pixels into a freshly allocated JPEG buffer.
    ///
    /// Called only when a consumer actually requests an image, so the per-call cost is
    /// paid once per served capture instead of continuously at the capture frame rate.
    pub fn encode_jpeg(&self, quality: u8) -> Result<Vec<u8>, image::ImageError> {
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, quality.clamp(1, 100)).encode(
            &self.rgb,
            self.width,
            self.height,
            ExtendedColorType::Rgb8,
        )?;
        Ok(jpeg)
    }
}

/// A borrowed, tightly packed RGB8 pixel buffer with `GenericImageView`-style access.
///
/// Exists so consumers can read pixels out of a [`FramePayload`] without copying them into
/// an owned `DynamicImage` first.
#[derive(Debug, Clone, Copy)]
pub struct RgbView<'a> {
    rgb: &'a [u8],
    width: u32,
    height: u32,
}

/// An RGB pixel sampled out of an [`RgbView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbPixel {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbView<'_> {
    fn new(rgb: &[u8], width: u32, height: u32) -> Option<RgbView<'_>> {
        let expected = width as usize * height as usize * 3;
        if width == 0 || height == 0 || rgb.len() < expected {
            return None;
        }
        Some(RgbView { rgb, width, height })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Samples the pixel at `(x, y)`, clamped to the frame bounds.
    ///
    /// Out-of-bounds reads return black, matching the graceful degradation the telemetry
    /// parser already applies to bars authored for larger layouts.
    pub fn get_pixel(&self, x: u32, y: u32) -> RgbPixel {
        if x >= self.width || y >= self.height {
            return RgbPixel { r: 0, g: 0, b: 0 };
        }
        let offset = (y as usize * self.width as usize + x as usize) * 3;
        RgbPixel {
            r: self.rgb[offset],
            g: self.rgb[offset + 1],
            b: self.rgb[offset + 2],
        }
    }
}

/// Thread-safe pointer to the latest captured frame.
pub type SharedFrameBuffer = Arc<Mutex<Option<FramePayload>>>;

/// Event signal fired by the capture thread after every published frame.
///
/// Consumers awaiting a visible screen change park on this instead of re-polling the
/// frame buffer on an interval, so a new frame wakes them the moment it lands.
pub type SharedFrameNotify = Arc<Notify>;

/// Current UNIX timestamp in seconds (0 if the clock is before the epoch).
pub fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
