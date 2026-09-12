//! TOML configuration file support.
//!
//! The server reads an optional `autorpg-mcp.toml` at startup. Every field has a
//! default matching the previous hard-coded constants, so an absent or partial file
//! behaves exactly like the pre-configuration build. Unknown keys are rejected so a
//! typo in the file fails loudly at startup instead of being silently ignored.

use std::time::Duration;

use serde::Deserialize;

/// File name looked up in the working directory when no explicit path is given.
pub const DEFAULT_CONFIG_PATH: &str = "autorpg-mcp.toml";

/// Environment variable overriding the default config path.
pub const CONFIG_PATH_ENV: &str = "AUTORPG_MCP_CONFIG";

/// Root of the configuration file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Capture pipeline tuning.
    pub capture: CaptureConfig,
    /// MCP tool limits.
    pub server: ServerConfig,
    /// Prompt templates.
    pub prompts: PromptsConfig,
    /// Initial session state.
    pub session: SessionConfig,
}

impl Config {
    /// Parses a configuration file's contents.
    pub fn from_str(toml: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml)
    }

    /// Reads and parses the configuration file at `path`.
    pub fn load(path: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
        let text = std::fs::read_to_string(path)?;
        Ok(Self::from_str(&text)?)
    }
}

/// Capture pipeline tuning.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    /// Minimum time between two frames that are actually converted, in milliseconds.
    /// Frame delivery is asynchronous and consumers only read the latest frame, so
    /// anything arriving sooner than this is dropped untouched.
    pub frame_interval_ms: u64,

    /// Update interval hint handed to Windows, in milliseconds. Advisory and jittery;
    /// deliberately shorter than `frame_interval_ms` so the receiver always has a fresh
    /// frame to publish.
    pub os_update_hint_ms: u64,

    /// Quality of the published JPEG (1-100). Lower quality shrinks the payload and
    /// shortens the encode step.
    pub jpeg_quality: u8,

    /// Whether the mouse cursor is drawn into captured frames.
    pub with_cursor: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            frame_interval_ms: 200,
            os_update_hint_ms: 150,
            jpeg_quality: 70,
            with_cursor: true,
        }
    }
}

impl CaptureConfig {
    pub fn frame_interval(&self) -> Duration {
        Duration::from_millis(self.frame_interval_ms.max(1))
    }

    pub fn os_update_hint(&self) -> Duration {
        Duration::from_millis(self.os_update_hint_ms.max(1))
    }

    /// JPEG quality clamped into the encoder's accepted 1-100 range.
    pub fn sanitized_jpeg_quality(&self) -> u8 {
        self.jpeg_quality.clamp(1, 100)
    }
}

/// MCP tool limits.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Maximum duration for hold-style input operations, in milliseconds.
    pub max_hold_ms: u64,

    /// Hamming distance between two frame hashes that still counts as "the screen did
    /// not move". Wide enough to absorb slight text changes, narrow enough to notice a
    /// step, a swing or a mob walking into view.
    pub stale_hash_distance: u32,

    /// How long `capture_screen` blocks waiting for the screen to change, in
    /// milliseconds, before giving up.
    pub wait_for_change_timeout_ms: u64,

    /// How often the blocking wait re-checks the shared frame buffer, in milliseconds.
    pub wait_poll_interval_ms: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_hold_ms: 10_000,
            stale_hash_distance: 2,
            wait_for_change_timeout_ms: 3_000,
            wait_poll_interval_ms: 100,
        }
    }
}

impl ServerConfig {
    pub fn wait_for_change_timeout(&self) -> Duration {
        Duration::from_millis(self.wait_for_change_timeout_ms)
    }

    pub fn wait_poll_interval(&self) -> Duration {
        Duration::from_millis(self.wait_poll_interval_ms.max(1))
    }
}

/// Prompt template settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptsConfig {
    /// Path to the prompt instructions template, relative to the configuration
    /// file's directory (or the working directory when the path is absolute).
    /// `{target}`, `{duration}` and `{potion_threshold}` placeholders are
    /// substituted per prompt call.
    pub instructions_path: String,
}

impl Default for PromptsConfig {
    fn default() -> Self {
        Self {
            instructions_path: "ai_instructions.md".to_string(),
        }
    }
}

/// 

/// Initial session state published before any tool call arrives.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// Initial player HP.
    pub initial_hp: i32,
    /// Initial stamina.
    pub initial_stamina: i32,
    /// Initial zone location identifier.
    pub initial_location: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            initial_hp: 100,
            initial_stamina: 100,
            initial_location: "Starter Village".to_string(),
        }
    }
}

/// Resolves the configuration path: explicit CLI argument, then the
/// [`CONFIG_PATH_ENV`] environment variable, then [`DEFAULT_CONFIG_PATH`].
pub fn resolve_path(explicit: Option<&str>) -> std::path::PathBuf {
    if let Some(path) = explicit {
        return std::path::PathBuf::from(path);
    }
    if let Ok(path) = std::env::var(CONFIG_PATH_ENV) {
        return std::path::PathBuf::from(path);
    }
    std::path::PathBuf::from(DEFAULT_CONFIG_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_yields_defaults() {
        let config = Config::from_str("").unwrap();
        assert_eq!(config.capture.frame_interval_ms, 200);
        assert_eq!(config.server.max_hold_ms, 10_000);
        assert_eq!(config.session.initial_location, "Starter Village");
        assert_eq!(config.prompts.instructions_path, "ai_instructions.md");
    }

    #[test]
    fn partial_file_fills_defaults() {
        let config = Config::from_str("[capture]\njpeg_quality = 40\n").unwrap();
        assert_eq!(config.capture.jpeg_quality, 40);
        assert_eq!(config.capture.frame_interval_ms, 200);
        assert_eq!(config.server.stale_hash_distance, 2);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(Config::from_str("[capture]\njpeg_qality = 40\n").is_err());
    }

    #[test]
    fn full_file_parses() {
        let config = Config::from_str(
            r#"
            [capture]
            frame_interval_ms = 100
            os_update_hint_ms = 80
            jpeg_quality = 85
            with_cursor = false

            [server]
            max_hold_ms = 5000
            stale_hash_distance = 4
            wait_for_change_timeout_ms = 1500
            wait_poll_interval_ms = 50

            [prompts]
            instructions_path = "prompts/farm.md"

            [session]
            initial_hp = 80
            initial_stamina = 90
            initial_location = "Dungeon"
            "#,
        )
        .unwrap();
        assert_eq!(config.capture.frame_interval(), Duration::from_millis(100));
        assert!(!config.capture.with_cursor);
        assert_eq!(config.server.wait_for_change_timeout(), Duration::from_millis(1500));
        assert_eq!(config.prompts.instructions_path, "prompts/farm.md");
        assert_eq!(config.session.initial_hp, 80);
    }
}
