//! Windows Graphics Capture engine: streams the primary monitor into a shared JPEG buffer.

use std::time::{Duration, Instant};

use fast_image_resize::{
    FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
    images::{Image, ImageRef},
};
use image::{ExtendedColorType, codecs::jpeg::JpegEncoder};
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    settings::ColorFormat,
};

use crate::state::SharedFrameBuffer;

/// Longest edge of the published preview, in pixels.
const PREVIEW_EDGE: u32 = 1024;

/// Quality of the published JPEG. Lower quality shrinks the payload and, more importantly,
/// shortens the encode step.
const JPEG_QUALITY: u8 = 70;

/// Minimum time between two frames that are actually converted.
///
/// Frame delivery is asynchronous: Windows hands the handler every compositor update (60-240
/// per second) while consumers only ever read the *latest* frame. Converting every one of them
/// is wasted work, so anything arriving sooner than this is dropped before it is touched.
pub(crate) const FRAME_INTERVAL: Duration = Duration::from_millis(200);

/// Interval handed to Windows as a hint so the capture pipeline stops producing frames we
/// would only discard.
///
/// Deliberately shorter than [`FRAME_INTERVAL`]: the hint is advisory and jittery, and it must
/// never leave the receiver without a fresh frame to publish.
pub(crate) const OS_UPDATE_HINT: Duration = Duration::from_millis(150);

/// Resampling algorithm used for the downsample.
///
/// [`FilterType::Lanczos3`] is the crate default and the highest quality option; its SIMD
/// convolution is cheap enough at the handful of frames per second published here. Swap in
/// [`FilterType::Hamming`] (downscaling quality close to bicubic at bilinear speed) or
/// [`FilterType::Bilinear`] to trade quality for speed.
const RESIZE_ALGORITHM: ResizeAlg = ResizeAlg::Convolution(FilterType::Lanczos3);

/// Receives captured frames, downscales them and stores the JPEG bytes
/// in the shared runtime buffer.
///
/// The receiver is paced by [`FRAME_INTERVAL`] and reuses every one of its working buffers, so
/// in a steady state it allocates nothing and reads the frame exactly once.
pub struct CaptureReceiver {
    frame_buffer: SharedFrameBuffer,
    /// Scratch space used to strip row padding out of the mapped GPU texture.
    depad_scratch: Vec<u8>,
    /// Destination of the downsample, still in the source's RGBA layout.
    preview_rgba: Vec<u8>,
    /// Scratch space for the packed RGB preview handed to the JPEG encoder.
    rgb_scratch: Vec<u8>,
    /// Scratch space for the encoded JPEG that is published to `frame_buffer`.
    jpeg_scratch: Vec<u8>,
    /// Kept across frames so its internal working buffers are allocated once.
    resizer: Resizer,
    /// Resize configuration, built once from [`RESIZE_ALGORITHM`].
    resize_options: ResizeOptions,
    /// When the last frame was converted, used to enforce [`FRAME_INTERVAL`].
    last_processed: Option<Instant>,
}

impl GraphicsCaptureApiHandler for CaptureReceiver {
    // The shared frame buffer is passed to the handler through the settings flags
    type Flags = SharedFrameBuffer;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            frame_buffer: ctx.flags,
            depad_scratch: Vec::new(),
            preview_rgba: Vec::new(),
            rgb_scratch: Vec::new(),
            jpeg_scratch: Vec::new(),
            resizer: Resizer::new(),
            // Alpha is always opaque in a monitor capture and is discarded when packing to
            // RGB, so skipping the resampler's premultiply/unpremultiply pass is pure win.
            resize_options: ResizeOptions::new()
                .resize_alg(RESIZE_ALGORITHM)
                .use_alpha(false),
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
            .is_some_and(|last| now.duration_since(last) < FRAME_INTERVAL)
        {
            return Ok(());
        }
        self.last_processed = Some(now);

        let source_width = frame.width();
        let source_height = frame.height();
        let swap_red_blue = matches!(frame.color_format(), ColorFormat::Bgra8);
        let expected_len = source_width as usize * source_height as usize * 4;
        let (preview_width, preview_height) = preview_dimensions(source_width, source_height);
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
        } // The mapped staging texture is released here, before the encode below.

        // `JpegEncoder` appends to its writer, so the reused buffer has to be emptied first.
        self.jpeg_scratch.clear();
        JpegEncoder::new_with_quality(&mut self.jpeg_scratch, JPEG_QUALITY).encode(
            &self.rgb_scratch,
            preview_width,
            preview_height,
            ExtendedColorType::Rgb8,
        )?;

        // Publish the new payload while handing the previous one back for reuse: the shared
        // buffer settles on a single capacity and the lock is held for a pointer swap only.
        let mut published = match self.frame_buffer.lock() {
            Ok(lock) => lock,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut previous = published.take().unwrap_or_default();
        std::mem::swap(&mut previous, &mut self.jpeg_scratch);
        *published = Some(previous);

        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Destination size for a `width` x `height` frame: scaled down to fit [`PREVIEW_EDGE`] on the
/// longest edge while preserving the aspect ratio, never upscaled.
fn preview_dimensions(width: u32, height: u32) -> (u32, u32) {
    if width <= PREVIEW_EDGE && height <= PREVIEW_EDGE {
        return (width, height);
    }

    let longest = u64::from(width.max(height));
    let shortest = u64::from(width.min(height));
    let scaled = (shortest * u64::from(PREVIEW_EDGE) / longest).max(1) as u32;

    if width >= height {
        (PREVIEW_EDGE, scaled)
    } else {
        (scaled, PREVIEW_EDGE)
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
