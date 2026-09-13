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
///
/// Measured off the 1366x768 screenshots (bars span roughly x 590..800 there): the
/// left edge sits past the crossed-swords combat icon so its red pixels never leak
/// into the bar scans.
const BAR_LEFT: f32 = 830.0;
const BAR_RIGHT: f32 = 1124.0;

/// Vertical center of the stamina bar (upper, yellow), in 1920x1080 design pixels.
const STAMINA_BAR_Y: f32 = 966.0;

/// Scan row for the health bar (lower), in 1920x1080 design pixels.
///
/// Sits on the top edge of the bar, above the centered "8,105/12,123" HP text, so
/// white digit glyphs cannot pollute the fill scan.
const HEALTH_BAR_Y: f32 = 982.0;

/// Vertical center of the weapon ability icons, in 1920x1080 design pixels.
const ABILITY_BAR_Y: f32 = 984.0;

/// Weapon ability icons along the bottom right, in 1920x1080 design pixels.
///
/// The row drifts ~40 design px between sessions (x 1335 in one capture, x 1296 in
/// another), so the centers below sit between the two observed layouts and the
/// sampling strips are wide enough to cover either offset.
const Q_ICON_X: f32 = 1316.0;
const R_ICON_X: f32 = 1386.0;
const F_ICON_X: f32 = 1457.0;
const G_ICON_X: f32 = 1539.0;

/// Crossed-swords combat indicator left of the health bar, in 1920x1080 design pixels.
/// Lit red while in combat; absent or dimmed outside combat.
const COMBAT_ICON_X: f32 = 812.0;
const COMBAT_ICON_Y: f32 = 990.0;

/// Relative luminance above which a pixel counts as part of a lit ability icon.
///
/// Sits above the amber cooldown squares (luminance ~160) so an on-cooldown skill
/// never reads as ready even when its overlay is bright.
const ABILITY_READY_LUMINANCE: f32 = 170.0;

/// Bright pixels within an icon strip required to call the ability off cooldown.
const ABILITY_READY_PIXELS: u32 = 4;

/// Red pixels within the combat-icon strip required to flag in-combat state.
const COMBAT_ICON_PIXELS: u32 = 3;

/// Half width of the strip sampled across an ability icon, in reference pixels.
/// Wide enough to straddle the ~40 px row drift observed between captures.
const ICON_STRIP_HALF_WIDTH: u32 = 30;

/// Half width of the strip sampled across the combat icon, in reference pixels.
/// Ends short of `BAR_LEFT` so health-bar fill never leaks into the count.
const COMBAT_STRIP_HALF_WIDTH: u32 = 14;

/// Telemetry returned when a bar cannot be measured (no readable pixels).
///
/// Deliberately negative: a failed scan must never read as 100%, or the farming loop
/// would keep fighting with a full health bar it never re-checks. The server renders
/// negative values as `?` in the telemetry text.
const UNKNOWN_PERCENT: i32 = -1;

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
    ability_icon_y: u32,
    q_icon_x: u32,
    r_icon_x: u32,
    f_icon_x: u32,
    g_icon_x: u32,
    combat_icon_x: u32,
    combat_icon_y: u32,
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
            ability_icon_y: fy(ABILITY_BAR_Y),
            q_icon_x: fx(Q_ICON_X),
            r_icon_x: fx(R_ICON_X),
            f_icon_x: fx(F_ICON_X),
            g_icon_x: fx(G_ICON_X),
            combat_icon_x: fx(COMBAT_ICON_X),
            combat_icon_y: fy(COMBAT_ICON_Y),
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

    /// Counts pixels matching `color_match` along a horizontal strip, clamped to the
    /// frame. Used for icon sampling where a single pixel is too fragile.
    fn count_matching<F>(pixels: &RgbView, start_x: u32, end_x: u32, y: u32, color_match: F) -> u32
    where
        F: Fn(RgbPixel) -> bool,
    {
        let width = pixels.width();
        if width == 0 || y >= pixels.height() {
            return 0;
        }

        let start_x = start_x.min(width - 1);
        let end_x = end_x.min(width);
        if start_x >= end_x {
            return 0;
        }

        (start_x..end_x).filter(|&x| color_match(pixels.get_pixel(x, y))).count() as u32
    }

    /// Whether the ability icon centered at `(x, y)` is off cooldown (lit).
    ///
    /// Samples a strip across the icon and counts bright pixels, so strand gaps in
    /// line-art icons (nets, claws) or an off-center single pixel cannot flip the read.
    fn is_ability_ready(pixels: &RgbView, x: u32, y: u32) -> bool {
        // Ready art renders white/bright; the amber cooldown overlay is bright but
        // heavily red-biased, so a pixel only counts as lit when its blue channel
        // keeps pace with red (white/bright art) on top of the luminance gate.
        let lit = |p: RgbPixel| {
            Self::calculate_luminance(p.r as f32, p.g as f32, p.b as f32)
                > ABILITY_READY_LUMINANCE
                && p.b > 100
                && p.b as u32 * 5 > p.r as u32 * 2
        };
        let matched = Self::count_matching(
            pixels,
            x.saturating_sub(ICON_STRIP_HALF_WIDTH),
            x + ICON_STRIP_HALF_WIDTH,
            y,
            lit,
        );
        matched >= ABILITY_READY_PIXELS
    }

    /// Whether the crossed-swords indicator beside the health bar is lit red.
    fn is_in_combat(pixels: &RgbView, x: u32, y: u32) -> bool {
        let red = |p: RgbPixel| p.r > 140 && p.g < 80 && p.b < 80;
        let matched = Self::count_matching(
            pixels,
            x.saturating_sub(COMBAT_STRIP_HALF_WIDTH),
            x + COMBAT_STRIP_HALF_WIDTH,
            y,
            red,
        );
        matched >= COMBAT_ICON_PIXELS
    }
}

impl GameProfile for ActionRPG {
    fn id(&self) -> &'static str {
        "action_rpg"
    }

    fn parse_telemetry(&self, pixels: &RgbView, resolution: Resolution) -> GameMetrics {
        // 1. Scan Health Bar (Bottom center; red fill when damaged, bright when full)
        let hp_percent = Self::scan_horizontal_bar(
            pixels,
            self.scale_x(self.bar_left, resolution),
            self.scale_x(self.bar_right, resolution),
            self.scale_y(self.health_bar_y, resolution),
            |p| {
                (p.r > 150 && p.g < 80 && p.b < 80)
                    || (p.r > 170 && p.g > 150 && p.b < 130)
                    || (p.r > 170 && p.g > 170 && p.b > 170)
            },
        );

        // 2. Scan Stamina Bar (Directly above HP bar, yellow fill)
        let stamina_percent = Self::scan_horizontal_bar(
            pixels,
            self.scale_x(self.bar_left, resolution),
            self.scale_x(self.bar_right, resolution),
            self.scale_y(self.stamina_bar_y, resolution),
            |p| p.r > 170 && p.g > 150 && p.b < 130,
        );

        // 3. Scan Skill Cooldown Pixels (Bottom-right weapon ability icons)
        let ability_y = self.scale_y(self.ability_icon_y, resolution);
        let q_ready = Self::is_ability_ready(pixels, self.scale_x(self.q_icon_x, resolution), ability_y);
        let r_ready = Self::is_ability_ready(pixels, self.scale_x(self.r_icon_x, resolution), ability_y);
        let f_ready = Self::is_ability_ready(pixels, self.scale_x(self.f_icon_x, resolution), ability_y);
        let g_ready = Self::is_ability_ready(pixels, self.scale_x(self.g_icon_x, resolution), ability_y);

        // 4. Crossed-swords combat indicator left of the health bar.
        let in_combat = Self::is_in_combat(
            pixels,
            self.scale_x(self.combat_icon_x, resolution),
            self.scale_y(self.combat_icon_y, resolution),
        );

        GameMetrics {
            player_hp: hp_percent,
            stamina: stamina_percent,
            q_ready,
            r_ready,
            f_ready,
            g_ready,
            // Zone is not pixel-readable here; the server merges the MCP-managed
            // location (`update_game_metrics`) into the returned metrics.
            location: String::new(),
            in_combat,
        }
    }
}
