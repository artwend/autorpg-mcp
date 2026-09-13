//! In-memory session state shared between the MCP tools and the capture thread.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, RwLock};

use enigo::Button;
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
    pub g_ready: bool,
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
///
/// Deliberately not `Clone`: it lives behind an `Arc<RwLock<...>>` and copying it would
/// deep-copy the whole event history for no caller.
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

/// Mouse buttons currently held down through `hold_mouse`.
///
/// Tracked for two reasons: a second press of an already-held button is rejected instead
/// of silently stacking, and whatever is still held when the session ends is released
/// rather than left stuck down in the OS input state.
///
/// Uses a `std::sync::Mutex` rather than the async session lock so the release path can
/// also run from a synchronous `Drop`, where no async context is available.
#[derive(Default)]
pub struct HeldButtons(Mutex<Vec<Button>>);

impl HeldButtons {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Button>> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records `button` as held, returning `false` when it already was.
    ///
    /// The check and the insert share one lock acquisition, so two concurrent presses of
    /// the same button cannot both succeed.
    pub fn press(&self, button: Button) -> bool {
        let mut held = self.lock();
        if held.contains(&button) {
            return false;
        }
        held.push(button);
        true
    }

    /// Clears `button` from the held set, returning whether it had been recorded.
    pub fn release(&self, button: Button) -> bool {
        let mut held = self.lock();
        match held.iter().position(|&held| held == button) {
            Some(index) => {
                held.remove(index);
                true
            }
            None => false,
        }
    }

    /// Removes and returns every held button, so the caller can release them.
    pub fn take_all(&self) -> Vec<Button> {
        std::mem::take(&mut self.lock())
    }
}

/// Thread-safe pointer to the set of mouse buttons held by `hold_mouse`.
pub type SharedHeldButtons = Arc<HeldButtons>;

/// Latest captured frame shared between the capture thread and the MCP tools.
///
/// The preview pixels travel with their hash so the tools can read telemetry straight out
/// of the frame the capture thread already produced once, instead of re-encoding or
/// re-decoding anything on every call. JPEG encoding is deferred to the consumer: the
/// capture thread never pays for it while no MCP client is querying frames.
#[derive(Default)]
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

/// Outcome of a non-blocking attempt to read the latest published frame.
enum FrameLock {
    /// The lock was acquired and held a frame.
    Frame(Arc<FramePayload>),
    /// The lock was acquired and no frame has been published yet.
    Empty,
    /// Another thread (the capture thread) held the lock.
    Contended,
}

impl FrameLock {
    fn from_option(frame: Option<&Arc<FramePayload>>) -> Self {
        match frame {
            Some(frame) => Self::Frame(Arc::clone(frame)),
            None => Self::Empty,
        }
    }
}

/// Shared buffer holding the latest captured frame.
///
/// The payload sits behind its own `Arc` so a consumer can clone the handle and release
/// the mutex before doing per-call image work (telemetry parsing, JPEG encoding), instead
/// of blocking the capture thread's next publish behind it. The wrapper hides the lock
/// (including poisoned-lock recovery) from every call site.
#[derive(Default)]
pub struct FrameBuffer {
    latest: Mutex<Option<Arc<FramePayload>>>,
    /// Set once the capture session has ended. Distinguishes "the capture stopped" from
    /// "the capture has not produced a frame yet", which are otherwise both an empty slot.
    closed: AtomicBool,
}

impl FrameBuffer {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Arc<FramePayload>>> {
        self.latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Clones the handle to the latest published frame, if any.
    ///
    /// Returns `None` while the capture thread has not produced a frame yet, or after the
    /// capture session has closed. Blocking; call from a blocking thread or use
    /// [`FrameBuffer::latest_async`].
    pub fn latest(&self) -> Option<Arc<FramePayload>> {
        self.lock().as_ref().map(Arc::clone)
    }

    /// Async-friendly clone of the latest published frame handle.
    ///
    /// A blocking `std` mutex taken directly in an async fn parks the executor thread when
    /// the capture thread happens to be mid-publish. Both sides hold the lock only for a
    /// pointer swap or an `Arc` clone, so the non-blocking fast path virtually always
    /// succeeds; the rare contended case is offloaded to a blocking thread instead of
    /// stalling the runtime.
    pub async fn latest_async(self: &Arc<Self>) -> Option<Arc<FramePayload>> {
        match self.try_latest() {
            FrameLock::Frame(frame) => Some(frame),
            FrameLock::Empty => None,
            FrameLock::Contended => {
                let buffer = Arc::clone(self);
                tokio::task::spawn_blocking(move || buffer.latest())
                    .await
                    .ok()
                    .flatten()
            }
        }
    }

    /// Non-blocking [`FrameBuffer::latest`].
    ///
    /// Kept as a separate synchronous helper (rather than inlined into
    /// [`FrameBuffer::latest_async`]) so no `MutexGuard` is ever alive across an await
    /// point, which would make the async fn's future `!Send`.
    fn try_latest(&self) -> FrameLock {
        match self.latest.try_lock() {
            Ok(guard) => FrameLock::from_option(guard.as_ref()),
            Err(TryLockError::Poisoned(poisoned)) => {
                FrameLock::from_option(poisoned.into_inner().as_ref())
            }
            Err(TryLockError::WouldBlock) => FrameLock::Contended,
        }
    }

    /// Publishes a new frame, returning the previous one.
    ///
    /// The publisher can reclaim the previous frame's pixel allocation via
    /// [`Arc::try_unwrap`] once no consumer still holds a clone.
    pub fn publish(&self, payload: Arc<FramePayload>) -> Option<Arc<FramePayload>> {
        let previous = self.lock().replace(payload);
        // Only the capture thread calls this, immediately after `new`, so a publish can
        // never race a close in practice; clearing it here keeps the state coherent if the
        // capture is ever restarted in-process.
        self.closed.store(false, Ordering::Release);
        previous
    }

    /// Marks the capture session as ended, dropping the published frame.
    ///
    /// Called from the capture handler's `on_closed`. Clearing the frame means consumers
    /// report "no active display buffer" instead of repeatedly serving a frozen image from
    /// a capture that is no longer running, and [`FrameBuffer::is_closed`] lets a waiter
    /// stop blocking on a notification that can never fire again.
    pub fn close(&self) {
        self.lock().take();
        self.closed.store(true, Ordering::Release);
    }

    /// Whether the capture session has ended without being restarted.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// Thread-safe pointer to the latest captured frame.
pub type SharedFrameBuffer = Arc<FrameBuffer>;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn press_is_rejected_while_the_button_is_held() {
        let held = HeldButtons::default();
        assert!(held.press(Button::Right));
        assert!(!held.press(Button::Right), "double press must be rejected");
        assert!(held.press(Button::Left), "other buttons stay independent");
    }

    #[test]
    fn release_clears_only_the_named_button() {
        let held = HeldButtons::default();
        held.press(Button::Left);
        held.press(Button::Middle);

        assert!(held.release(Button::Left));
        assert!(!held.release(Button::Left), "a second release is a no-op");
        assert_eq!(held.take_all(), vec![Button::Middle]);
    }

    #[test]
    fn take_all_drains_the_set() {
        let held = HeldButtons::default();
        held.press(Button::Left);
        held.press(Button::Right);

        let mut taken = held.take_all();
        taken.sort_by_key(|button| format!("{button:?}"));
        assert_eq!(taken, vec![Button::Left, Button::Right]);
        assert!(
            held.take_all().is_empty(),
            "a second drain must release nothing twice"
        );
    }
}
