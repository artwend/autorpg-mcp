// src/games/action_rpg.rs

use super::{GameProfile, Resolution};
use crate::state::{GameMetrics, RgbPixel, RgbView};

/// Reference box defaults matching the pre-configuration build: a 1024-edge preview with
/// a 16:9 layout.
const DEFAULT_PREVIEW_EDGE: u32 = 1024;
const DEFAULT_REFERENCE_ASPECT_RATIO: f32 = 16.0 / 9.0;

/// Size of the 1920x1080 UI the layout constants below were measured on.
const DESIGN_WIDTH: f32 = 1920.0;
const DESIGN_HEIGHT: f32 = 1080.0;

/// Health/stamina bar horizontal extent, in 1920x1080 design pixels.
const BAR_LEFT: f32 = 760.0;
const BAR_RIGHT: f32 = 1160.0;

/// Vertical center of the health and stamina bars, in 1920x1080 design pixels.
const HEALTH_BAR_Y: f32 = 960.0;
const STAMINA_BAR_Y: f32 = 952.0;

/// Weapon ability icons along the bottom right, in 1920x1080 design pixels.
const Q_ICON_X: f32 = 1685.0;
const R_ICON_X: f32 = 1745.0;
const F_ICON_X: f32 = 1805.0;

/// Relative luminance above which a weapon-ability icon counts as off cooldown.
const ABILITY_READY_LUMINANCE: f32 = 65.0;

/// Telemetry returned when a bar cannot be measured (no readable pixels).
const UNKNOWN_PERCENT: i32 = 100;

/// Action RPG telemetry profile.
///
/// Telemetry is always read from the capture engine's preview, whose longest edge is the
/// configured `preview_edge`, so the UI layout is authored against that preview box
/// instead of the desktop resolution it was captured from. The box's aspect ratio comes
/// from `[game] reference_aspect_ratio`; both are injected at construction and the layout
/// below is rescaled from the 1920x1080 UI it was measured on into that reference box.
/// Frames below the preview ceiling are still scaled proportionally by the [`Resolution`]
/// handed to the parser.
pub struct ActionRPG {
    /// Reference box the layout coordinates are mapped through.
    reference: Resolution,
    /// Layout positions rescaled into the reference box.
    bar_left: u32,
    bar_right: u32,
    health_bar_y: u32,
    stamina_bar_y: u32,
    q_icon_x: u32,
    r_icon_x: u32,
    f_icon_x: u32,
}

impl Default for ActionRPG {
    fn default() -> Self {
        Self::new(DEFAULT_PREVIEW_EDGE, DEFAULT_REFERENCE_ASPECT_RATIO)
    }
}

impl ActionRPG {
    /// Builds a profile whose reference box is `preview_edge` pixels wide with
    /// `reference_aspect_ratio` (width / height) aspect. Degenerate values fall back to
    /// the defaults above.
    pub fn new(preview_edge: u32, reference_aspect_ratio: f32) -> Self {
        let aspect = if reference_aspect_ratio.is_finite() && reference_aspect_ratio > 0.0 {
            reference_aspect_ratio
        } else {
            DEFAULT_REFERENCE_ASPECT_RATIO
        };
        let width = preview_edge.max(1);
        let height = ((width as f32 / aspect).round() as u32).max(1);
        let reference = Resolution::new(width, height);

        // Rescale the 1920x1080-measured layout into the reference box.
        let fx = |x: f32| (x * width as f32 / DESIGN_WIDTH).round() as u32;
        let fy = |y: f32| (y * height as f32 / DESIGN_HEIGHT).round() as u32;
        Self {
            reference,
            bar_left: fx(BAR_LEFT),
            bar_right: fx(BAR_RIGHT),
            health_bar_y: fy(HEALTH_BAR_Y),
            stamina_bar_y: fy(STAMINA_BAR_Y),
            q_icon_x: fx(Q_ICON_X),
            r_icon_x: fx(R_ICON_X),
            f_icon_x: fx(F_ICON_X),
        }
    }

    /// Internal pixel helper to evaluate relative luminance (perceived human brightness).
    fn calculate_luminance(r: f32, g: f32, b: f32) -> f32 {
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    /// Maps an x coordinate authored at `self.reference.width` onto `resolution`.
    fn scale_x(&self, reference_x: u32, resolution: Resolution) -> u32 {
        (reference_x as f32 * resolution.width as f32 / self.reference.width as f32).round() as u32
    }

    /// Maps a y coordinate authored at `self.reference.height` onto `resolution`.
    fn scale_y(&self, reference_y: u32, resolution: Resolution) -> u32 {
        (reference_y as f32 * resolution.height as f32 / self.reference.height as f32).round() as u32
    }

    /// Internal pixel helper to scan horizontal bar segments.
    ///
    /// Coordinates are clamped to the frame, so a bar authored for a larger layout degrades
    /// gracefully instead of panicking on small frames.
    fn scan_horizontal_bar<F>(pixels: &RgbView, start_x: u32, end_x: u32, y: u32, color_match: F) -> i32
    where
        F: Fn(RgbPixel) -> bool,
    {
        let width = pixels.width();
        if width == 0 || y >= pixels.height() {
            return UNKNOWN_PERCENT;
        }

        let start_x = start_x.min(width - 1);
        let end_x = end_x.min(width);
        if start_x >= end_x {
            return UNKNOWN_PERCENT;
        }

        let mut matched_pixels = 0;
        let total_pixels = end_x - start_x;

        for x in start_x..end_x {
            if color_match(pixels.get_pixel(x, y)) {
                matched_pixels += 1;
            }
        }

        let percentage = (matched_pixels * 100) / total_pixels;
        percentage.clamp(0, 100) as i32
    }

    /// Whether the ability icon at `(x, y)` is off cooldown (glowing).
    fn is_ability_ready(pixels: &RgbView, x: u32, y: u32) -> bool {
        let pixel = pixels.get_pixel(x, y);
        let luminance =
            Self::calculate_luminance(pixel.r as f32, pixel.g as f32, pixel.b as f32);
        luminance > ABILITY_READY_LUMINANCE
    }
}

impl GameProfile for ActionRPG {
    fn id(&self) -> &'static str {
        "action_rpg"
    }

    fn parse_telemetry(&self, pixels: &RgbView, resolution: Resolution) -> GameMetrics {
        // 1. Scan Health Bar (Bottom center, deep saturated red)
        let hp_percent = Self::scan_horizontal_bar(
            pixels,
            self.scale_x(self.bar_left, resolution),
            self.scale_x(self.bar_right, resolution),
            self.scale_y(self.health_bar_y, resolution),
            |p| p.r > 150 && p.g < 60 && p.b < 60,
        );

        // 2. Scan Stamina Bar (Directly above HP bar, bright white/light-cyan hue)
        let stamina_percent = Self::scan_horizontal_bar(
            pixels,
            self.scale_x(self.bar_left, resolution),
            self.scale_x(self.bar_right, resolution),
            self.scale_y(self.stamina_bar_y, resolution),
            |p| p.r > 200 && p.g > 200 && p.b > 200,
        );

        // 3. Scan Skill Cooldown Pixels (Bottom-right weapon ability icons)
        let ability_y = self.scale_y(self.health_bar_y, resolution);
        let q_ready = Self::is_ability_ready(pixels, self.scale_x(self.q_icon_x, resolution), ability_y);
        let r_ready = Self::is_ability_ready(pixels, self.scale_x(self.r_icon_x, resolution), ability_y);
        let f_ready = Self::is_ability_ready(pixels, self.scale_x(self.f_icon_x, resolution), ability_y);

        GameMetrics {
            player_hp: hp_percent,
            stamina: stamina_percent,
            q_ready,
            r_ready,
            f_ready,
            // Zone is not pixel-readable here; the server merges the MCP-managed
            // location (`update_game_metrics`) into the returned metrics.
            location: String::new(),
            in_combat: false, // Can be updated based on combat detection logic
        }
    }
}
