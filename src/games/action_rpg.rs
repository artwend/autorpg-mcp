// src/games/new_world.rs

use super::GameProfile;
use crate::state::GameMetrics;
use image::{DynamicImage, GenericImage, GenericImageView};
use std::io::Cursor;

pub struct ActionRPG;

impl ActionRPG {
    /// Internal pixel helper to evaluate relative luminance (perceived human brightness).
    fn calculate_luminance(r: f32, g: f32, b: f32) -> f32 {
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    /// Internal pixel helper to scan horizontal bar segments.
    fn scan_horizontal_bar<F>(img: &DynamicImage, start_x: u32, end_x: u32, y: u32, color_match: F) -> i32
    where
        F: Fn(u8, u8, u8) -> bool,
    {
        if img.width() < 1920 || img.height() < 1080 {
            return 100; // Safe default fallback
        }

        let mut matched_pixels = 0;
        let total_pixels = end_x - start_x;

        for x in start_x..end_x {
            let pixel = img.get_pixel(x, y);
            if color_match(pixel[0], pixel[1], pixel[2]) {
                matched_pixels += 1;
            }
        }

        let percentage = (matched_pixels * 100) / total_pixels;
        percentage.clamp(0, 100) as i32
    }
}

impl GameProfile for ActionRPG {
    fn id(&self) -> &'static str {
        "action_rpg"
    }

    fn parse_telemetry(&self, img: &DynamicImage) -> GameMetrics {
        // 1. Scan Health Bar (Bottom center, deep saturated red)
        let hp_percent = Self::scan_horizontal_bar(img, 760, 1160, 960, |r, g, b| {
            r > 150 && g < 60 && b < 60
        });

        // 2. Scan Stamina Bar (Directly above HP bar, bright white/light-cyan hue)
        let stamina_percent = Self::scan_horizontal_bar(img, 760, 1160, 952, |r, g, b| {
            r > 200 && g > 200 && b > 200
        });

        // 3. Scan Skill Cooldown Pixels (Bottom-right weapon ability icons)
        let is_skill_ready = |x: u32, y: u32| -> bool {
            if img.width() < 1920 || img.height() < 1080 { return true; }
            let pixel = img.get_pixel(x, y);
            let lum = Self::calculate_luminance(pixel[0] as f32, pixel[1] as f32, pixel[2] as f32);
            lum > 65.0 // Glow indicator filter threshold
        };

        let q_ready = is_skill_ready(1685, 960);
        let r_ready = is_skill_ready(1745, 960);
        let f_ready = is_skill_ready(1805, 960);

        GameMetrics {
            player_hp: hp_percent,
            stamina: stamina_percent,
            q_ready,
            r_ready,
            f_ready,
            location: "Bullrush Wash".to_string(), // Can hook to custom sub-OCR matrix if needed
        }
    }

    fn get_system_instructions(&self) -> String {
        r#"# GAME RULESET: ACTION RPG
- VIEWPORT LAYOUT: The image is a stitched composite. TOP half is the Center Combat view. BOTTOM half is the Right Quest Log.
- UI TELEMETRY: HP and Stamina are handled by the server. Do not hunt for status bars in the image.
- COMBAT ROTATION: Prioritize Weapon Abilities [Q], [R], and [F] if server telemetry flags them as 'READY'. Use '1' for potions if HP <= 30%.
- INPUT METHOD: Keyboard strokes simulate actions instantly. Standard attacks utilize left mouse clicks."#.to_string()
    }
}
