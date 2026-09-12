// src/games/action_rpg.rs

use super::{GameProfile, Resolution};
use crate::capture::PREVIEW_EDGE;
use crate::state::{GameMetrics, RgbPixel, RgbView};

/// Telemetry is always read from the capture engine's preview, whose longest edge never
/// exceeds [`PREVIEW_EDGE`], so the UI layout is authored against that preview box instead
/// of the desktop resolution it was captured from. Frames below the preview ceiling are
/// still scaled proportionally by the [`Resolution`] handed to the parser.
const REFERENCE_WIDTH: f32 = PREVIEW_EDGE as f32;
const REFERENCE_HEIGHT: f32 = REFERENCE_WIDTH * 9.0 / 16.0;

/// Health/stamina bar horizontal extent, rescaled from the 1920-wide layout it was measured on.
const BAR_LEFT: f32 = REFERENCE_WIDTH * (760.0 / 1920.0);
const BAR_RIGHT: f32 = REFERENCE_WIDTH * (1160.0 / 1920.0);

/// Vertical center of the health and stamina bars.
const HEALTH_BAR_Y: f32 = REFERENCE_HEIGHT * (960.0 / 1080.0);
const STAMINA_BAR_Y: f32 = REFERENCE_HEIGHT * (952.0 / 1080.0);

/// Weapon ability icons along the bottom right.
const Q_ICON_X: f32 = REFERENCE_WIDTH * (1685.0 / 1920.0);
const R_ICON_X: f32 = REFERENCE_WIDTH * (1745.0 / 1920.0);
const F_ICON_X: f32 = REFERENCE_WIDTH * (1805.0 / 1920.0);

/// Relative luminance above which a weapon-ability icon counts as off cooldown.
const ABILITY_READY_LUMINANCE: f32 = 65.0;

/// Telemetry returned when a bar cannot be measured (no readable pixels).
const UNKNOWN_PERCENT: i32 = 100;

pub struct ActionRPG;

impl ActionRPG {
    /// Internal pixel helper to evaluate relative luminance (perceived human brightness).
    fn calculate_luminance(r: f32, g: f32, b: f32) -> f32 {
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    /// Maps an x coordinate authored at [`REFERENCE_WIDTH`] onto `resolution`.
    fn scale_x(design_x: f32, resolution: Resolution) -> u32 {
        (design_x * resolution.width as f32 / REFERENCE_WIDTH).round() as u32
    }

    /// Maps a y coordinate authored at [`REFERENCE_HEIGHT`] onto `resolution`.
    fn scale_y(design_y: f32, resolution: Resolution) -> u32 {
        (design_y * resolution.height as f32 / REFERENCE_HEIGHT).round() as u32
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
            Self::scale_x(BAR_LEFT, resolution),
            Self::scale_x(BAR_RIGHT, resolution),
            Self::scale_y(HEALTH_BAR_Y, resolution),
            |p| p.r > 150 && p.g < 60 && p.b < 60,
        );

        // 2. Scan Stamina Bar (Directly above HP bar, bright white/light-cyan hue)
        let stamina_percent = Self::scan_horizontal_bar(
            pixels,
            Self::scale_x(BAR_LEFT, resolution),
            Self::scale_x(BAR_RIGHT, resolution),
            Self::scale_y(STAMINA_BAR_Y, resolution),
            |p| p.r > 200 && p.g > 200 && p.b > 200,
        );

        // 3. Scan Skill Cooldown Pixels (Bottom-right weapon ability icons)
        let ability_y = Self::scale_y(HEALTH_BAR_Y, resolution);
        let q_ready = Self::is_ability_ready(pixels, Self::scale_x(Q_ICON_X, resolution), ability_y);
        let r_ready = Self::is_ability_ready(pixels, Self::scale_x(R_ICON_X, resolution), ability_y);
        let f_ready = Self::is_ability_ready(pixels, Self::scale_x(F_ICON_X, resolution), ability_y);

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
