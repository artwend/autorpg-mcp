//! autorpg-mcp: an MCP server exposing screen capture, input simulation and
//! game-state tracking tools for game automation.

mod capture;
mod config;
mod error;
mod games;
mod input;
mod server;
mod state;

use std::sync::Arc;

use log::info;
use rmcp::ServiceExt;
use tokio::sync::RwLock;
use windows_capture::{
    capture::GraphicsCaptureApiHandler,
    graphics_capture_api::GraphicsCaptureApi,
    monitor::Monitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
};

use capture::{CaptureFlags, CaptureReceiver};
use config::{Config, CONFIG_PATH_ENV, DEFAULT_CONFIG_PATH};
use server::GameServer;
use state::{GameMetrics, SessionState, SharedFrameBuffer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load the configuration file. An explicit CLI argument or the AUTORPG_MCP_CONFIG
    // environment variable must point at an existing file; the default path is optional
    // and simply falls back to the built-in defaults when absent.
    let config_path = config::resolve_path(std::env::args().nth(1).as_deref());
    let config = match Config::load(&config_path) {
        Ok(config) => {
            info!("loaded configuration from {}", config_path.display());
            config
        }
        Err(error) if config_path == std::path::Path::new(DEFAULT_CONFIG_PATH) => {
            info!(
                "no configuration file at {} ({}), using defaults",
                config_path.display(),
                error
            );
            Config::default()
        }
        Err(error) => {
            return Err(format!(
                "failed to load configuration from {} (set via CLI argument or {CONFIG_PATH_ENV}): {error}",
                config_path.display()
            )
            .into());
        }
    };

    // Prompt templates are resolved relative to the configuration file's directory, so a
    // config in another folder keeps pointing at its own templates.
    let instructions_path = {
        let path = std::path::PathBuf::from(&config.prompts.instructions_path);
        if path.is_absolute() {
            path
        } else {
            config_path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join(path)
        }
    };

    // Initialize in-memory session parameters
    let session_state = RwLock::new(SessionState {
        current_metrics: GameMetrics {
            player_hp: config.session.initial_hp,
            stamina: config.session.initial_stamina,
            q_ready: true,
            r_ready: true,
            f_ready: true,
            location: config.session.initial_location.clone(),
            in_combat: false,
        },
        active_game: Arc::new(games::action_rpg::ActionRPG),
        last_frame_hash: None,
        event_history: std::collections::VecDeque::new(),
    })
    .into();

    // Shared buffer for the latest compressed screenshot
    let frame_buffer: SharedFrameBuffer = Default::default();

    // Start the Windows Graphics Capture session on a dedicated background thread.
    //
    // Ask Windows to stop producing compositor updates faster than the receiver consumes them,
    // so we never pay for callbacks whose frames get dropped anyway. The setting is advisory
    // and unsupported on older builds, hence the capability check; `CaptureReceiver` also paces
    // itself, so falling back to the default interval only costs a little idle CPU.
    let minimum_update_interval = if GraphicsCaptureApi::is_minimum_update_interval_supported()
        .unwrap_or(false)
    {
        MinimumUpdateIntervalSettings::Custom(config.capture.os_update_hint())
    } else {
        MinimumUpdateIntervalSettings::Default
    };

    let primary_monitor = Monitor::primary()?;
    let cursor_settings = if config.capture.with_cursor {
        CursorCaptureSettings::WithCursor
    } else {
        CursorCaptureSettings::WithoutCursor
    };
    let settings = Settings::new(
        primary_monitor,
        cursor_settings,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        minimum_update_interval,
        DirtyRegionSettings::Default,
        ColorFormat::Rgba8,
        CaptureFlags {
            frame_buffer: frame_buffer.clone(),
            config: config.capture,
        },
    );

    // Keep the capture control alive for the lifetime of the process
    let _capture_control = CaptureReceiver::start_free_threaded(settings)?;

    // Build the configured input backend (enigo by default, input-simulator
    // when the `input-simulator` feature is enabled)
    let input = input::create_input()?;

    // Serve the MCP server over standard I/O (JSON-RPC via stdin/stdout)
    let server = GameServer::new(
        session_state,
        frame_buffer,
        input,
        config.server,
        instructions_path,
    );
    let service = server.serve(rmcp::transport::stdio()).await?;

    // Block until the client disconnects
    service.waiting().await?;
    Ok(())
}
