//! Windows Graphics Capture engine: streams the primary monitor (or the window
//! named in the configuration) into a shared RGB preview buffer.

use std::time::{Duration, Instant};

use fast_image_resize::{
    FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
    images::{Image, ImageRef},
};
use log::warn;
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    settings::ColorFormat,
};

use crate::config::CaptureConfig;
use crate::state::{FramePayload, SharedFrameBuffer, SharedFrameNotify};

/// Everything the capture thread needs from the outside world, passed to the handler
/// through the `Flags` mechanism.
pub struct CaptureFlags {
    /// Shared buffer the converted frames are published into.
    pub frame_buffer: SharedFrameBuffer,
    /// Signalled after every published frame so waiting consumers wake immediately
    /// instead of polling the buffer.
    pub frame_notify: SharedFrameNotify,
    /// Pacing and encoding settings.
    pub config: CaptureConfig,
}

/// Resampling algorithm used for the downsample.
///
/// [`FilterType::Lanczos3`] is the crate default and the highest quality option; its SIMD
/// convolution is cheap enough at the handful of frames per second published here. Swap in
/// [`FilterType::Hamming`] (downscaling quality close to bicubic at bilinear speed) or
/// [`FilterType::Bilinear`] to trade quality for speed.
const RESIZE_ALGORITHM: ResizeAlg = ResizeAlg::Convolution(FilterType::Hamming);

/// Grid resolution of the published frame hash: an 8x8 grid yields its 64 bits.
const HASH_GRID: u32 = 8;

/// Samples taken per hash cell. Cell averages are what keep the hash stable against
/// compression artifacts and sub-pixel jitter, and 4x4 samples already measure them well
/// without walking the entire preview.
const HASH_SAMPLES: u32 = 4;

/// Receives captured frames, downscales them and stores the JPEG bytes
/// in the shared runtime buffer.
///
/// The receiver is paced by its configured frame interval and reuses every one of its
/// working buffers, so in a steady state it allocates nothing and reads the frame exactly
/// once.
pub struct CaptureReceiver {
    frame_buffer: SharedFrameBuffer,
    /// Signalled after every published frame; see [`CaptureFlags::frame_notify`].
    frame_notify: SharedFrameNotify,
    /// Scratch space used to strip row padding out of the mapped GPU texture.
    depad_scratch: Vec<u8>,
    /// Destination of the downsample, still in the source's RGBA layout.
    preview_rgba: Vec<u8>,
    /// Packed RGB preview pixels published to consumers. JPEG encoding is deferred to
    /// the consumer (`GameServer::capture_screen`), so no encode work happens here while
    /// no MCP client is querying frames.
    rgb_scratch: Vec<u8>,
    /// Kept across frames so its internal working buffers are allocated once.
    resizer: Resizer,
    /// Resize configuration, built once from [`RESIZE_ALGORITHM`].
    resize_options: ResizeOptions,
    /// Minimum time between two converted frames, from [`CaptureConfig`].
    frame_interval: Duration,
    /// Longest edge of the published preview, from [`CaptureConfig`].
    preview_edge: u32,
    /// When the last frame was converted, used to enforce `frame_interval`.
    last_processed: Option<Instant>,
}

impl GraphicsCaptureApiHandler for CaptureReceiver {
    // The shared frame buffer and the capture settings are passed to the handler
    // through the settings flags
    type Flags = CaptureFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            frame_buffer: ctx.flags.frame_buffer,
            frame_notify: ctx.flags.frame_notify,
            depad_scratch: Vec::new(),
            preview_rgba: Vec::new(),
            rgb_scratch: Vec::new(),
            resizer: Resizer::new(),
            // Alpha is always opaque in a monitor capture and is discarded when packing to
            // RGB, so skipping the resampler's premultiply/unpremultiply pass is pure win.
            resize_options: ResizeOptions::new()
                .resize_alg(RESIZE_ALGORITHM)
                .use_alpha(false),
            frame_interval: ctx.flags.config.frame_interval(),
            preview_edge: ctx.flags.config.sanitized_preview_edge(),
            last_processed: None,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // Throttle before touching the frame. `frame.buffer()` copies the entire GPU texture
        // into a CPU-readable staging texture, which costs more than everything else in this
        // callback combined, so the cheapest win is to not do it at all.
        let now = Instant::now();
        if self
            .last_processed
            .is_some_and(|last| now.duration_since(last) < self.frame_interval)
        {
            return Ok(());
        }
        self.last_processed = Some(now);

        // Returning an error from this callback makes `windows-capture` tear down the capture
        // thread permanently, so transient failures (DirectX surface loss, a busy staging
        // buffer, a momentary encode hiccup) are logged and skipped instead: the next frame
        // arrives within [`Self::frame_interval`] and the pipeline keeps running.
        if let Err(error) = self.process_frame(frame) {
            warn!("skipping captured frame: {error}");
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl CaptureReceiver {
    /// Converts one frame and publishes it. Errors are transient by design: the caller logs
    /// and skips them rather than letting them kill the capture thread.
    fn process_frame(
        &mut self,
        frame: &mut Frame,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let source_width = frame.width();
        let source_height = frame.height();
        let swap_red_blue = matches!(frame.color_format(), ColorFormat::Bgra8);
        let expected_len = source_width as usize * source_height as usize * 4;
        let (preview_width, preview_height) =
            preview_dimensions(source_width, source_height, self.preview_edge);
        let preview_len = preview_width as usize * preview_height as usize * 4;

        {
            // Map the GPU texture to CPU-accessible memory and drop any row padding.
            let buffer = frame.buffer()?;

            // `as_nopadding_buffer` borrows the staging texture when the rows are already
            // tightly packed, so it only fills `depad_scratch` when the GPU added padding.
            let pixels = buffer.as_nopadding_buffer(&mut self.depad_scratch);
            if pixels.len() < expected_len {
                return Ok(());
            }

            // Downsample with a SIMD resampler. It requires matching pixel types, so the result
            // stays RGBA and the alpha channel is dropped further down, on the small preview
            // rather than on the full-resolution frame.
            self.preview_rgba.resize(preview_len, 0);
            let source = ImageRef::new(
                source_width,
                source_height,
                &pixels[..expected_len],
                PixelType::U8x4,
            )?;
            let mut preview = Image::from_slice_u8(
                preview_width,
                preview_height,
                &mut self.preview_rgba,
                PixelType::U8x4,
            )?;
            self.resizer
                .resize(&source, &mut preview, &self.resize_options)?;

            // Pack the preview into RGB so the encoder can consume it directly.
            pack_rgba_to_rgb(preview.buffer(), &mut self.rgb_scratch, swap_red_blue);
        } // The mapped staging texture is released here.

        // Hash the packed preview here, right next to the pixels it describes: consumers then
        // detect a static screen without decoding the preview they are handed.
        let frame_hash = average_hash(&self.rgb_scratch, preview_width, preview_height);

        // Publish the new payload while reclaiming the previous frame's allocation. The
        // scratch buffer is handed to `payload` below, so taking the old frame's buffer
        // back is what keeps it on a single allocation.
        let payload = FramePayload {
            rgb: std::mem::take(&mut self.rgb_scratch),
            width: preview_width,
            height: preview_height,
            hash: frame_hash,
        };

        let mut published = match self.frame_buffer.lock() {
            Ok(lock) => lock,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(previous) = published.take() {
            self.rgb_scratch = previous.rgb;
        }
        *published = Some(payload);
        drop(published);

        // Wake any consumer blocked in `wait_for_screen_change` now that a fresh frame
        // is visible. `notify_waiters` pairs with the `Notified::enable` registration the
        // waiter performs before re-checking the buffer, so no wakeup is ever lost.
        self.frame_notify.notify_waiters();

        Ok(())
    }
}

/// Destination size for a `width` x `height` frame: scaled down to fit `preview_edge` on the
/// longest edge while preserving the aspect ratio, never upscaled.
fn preview_dimensions(width: u32, height: u32, preview_edge: u32) -> (u32, u32) {
    if width <= preview_edge && height <= preview_edge {
        return (width, height);
    }

    let longest = u64::from(width.max(height));
    let shortest = u64::from(width.min(height));
    let scaled = (shortest * u64::from(preview_edge) / longest).max(1) as u32;

    if width >= height {
        (preview_edge, scaled)
    } else {
        (scaled, preview_edge)
    }
}

/// Packs an RGBA8 buffer into RGB8 by dropping the alpha channel.
///
/// This runs on the downsampled preview rather than on the full-resolution frame: de-interleaving
/// roughly a megapixel is a rounding error next to touching four to eight, and the resampler is
/// left to work on the four-component data it reads most efficiently.
fn pack_rgba_to_rgb(rgba: &[u8], rgb: &mut Vec<u8>, swap_red_blue: bool) {
    rgb.clear();
    rgb.reserve(rgba.len() / 4 * 3);

    for px in rgba.chunks_exact(4) {
        let (red, blue) = if swap_red_blue {
            (px[2], px[0])
        } else {
            (px[0], px[2])
        };
        rgb.push(red);
        rgb.push(px[1]);
        rgb.push(blue);
    }
}

/// 64-bit average hash of a packed RGB preview: an 8x8 grid of cell-mean luminances compared
/// against the mean of the grid, MSB-first by cell index.
///
/// The capture thread is the only place that already holds the preview pixels, so the hash is
/// taken here and shipped with them; consumers compare hashes instead of decoding a JPEG just
/// to find out that nothing moved.
fn average_hash(rgb: &[u8], width: u32, height: u32) -> u64 {
    if width < HASH_GRID || height < HASH_GRID || rgb.len() < width as usize * height as usize * 3 {
        return 0;
    }

    let mut cells = [0u32; (HASH_GRID * HASH_GRID) as usize];

    for grid_y in 0..HASH_GRID {
        let y0 = grid_y * height / HASH_GRID;
        let y1 = (grid_y + 1) * height / HASH_GRID;

        for grid_x in 0..HASH_GRID {
            let x0 = grid_x * width / HASH_GRID;
            let x1 = (grid_x + 1) * width / HASH_GRID;

            let mut sum = 0u32;
            for sample_y in 0..HASH_SAMPLES {
                let y = y0 + (y1 - y0) * sample_y / HASH_SAMPLES;
                for sample_x in 0..HASH_SAMPLES {
                    let x = x0 + (x1 - x0) * sample_x / HASH_SAMPLES;
                    let offset = (y as usize * width as usize + x as usize) * 3;
                    let (r, g, b) = (
                        u32::from(rgb[offset]),
                        u32::from(rgb[offset + 1]),
                        u32::from(rgb[offset + 2]),
                    );
                    sum += (2126 * r + 7152 * g + 722 * b) / 10_000;
                }
            }

            cells[(grid_y * HASH_GRID + grid_x) as usize] = sum / (HASH_SAMPLES * HASH_SAMPLES);
        }
    }

    let mean = cells.iter().sum::<u32>() / cells.len() as u32;
    cells.iter().enumerate().fold(0u64, |hash, (bit, cell)| {
        hash | (u64::from(*cell >= mean) << bit)
    })
}
