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
    capture::{CaptureControl, GraphicsCaptureApiHandler},
    graphics_capture_api::GraphicsCaptureApi,
    monitor::Monitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
    window::Window,
};

use capture::{CaptureFlags, CaptureReceiver};
use config::{Config, CONFIG_PATH_ENV, DEFAULT_CONFIG_PATH};
use server::GameServer;
use state::{GameMetrics, SessionState, SharedFrameBuffer, SharedFrameNotify};

// Per-monitor-v2 DPI awareness so `GetSystemMetrics` (and therefore the input
// backend's `main_display`) reports physical monitor pixels instead of
// DPI-virtualized ones: windows-capture always captures physical pixels, so
// mouse coordinate scaling in `move_mouse` would drift on displays with
// Windows scaling > 100% without this. Must run before any DPI-dependent API
// is used.
#[link(name = "user32")]
unsafe extern "system" {
    fn SetProcessDpiAwarenessContext(value: isize) -> i32;
}

/// `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` (documented as -4).
const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;

/// Declares per-monitor-v2 DPI awareness for this process. Idempotent: the
/// Windows call fails harmlessly when awareness was already set.
fn set_process_dpi_awareness() {
    // The return value is only an error when awareness was already set (e.g. by
    // a manifest), in which case nothing needs to change.
    let _ = unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
    };
}

/// Builds the capture settings for `source` and starts the capture thread.
///
/// Generic over the source because the target is either a [`Monitor`] or a
/// [`Window`], chosen at runtime. `GraphicsCaptureItemType` itself is not
/// `Send` (its HWND fallback variant holds a raw pointer), so the two source
/// types cannot be collapsed into it before `start_free_threaded`.
fn start_capture<Source>(
    source: Source,
    cursor_settings: CursorCaptureSettings,
    minimum_update_interval: MinimumUpdateIntervalSettings,
    flags: CaptureFlags,
) -> Result<
    CaptureControl<CaptureReceiver, Box<dyn std::error::Error + Send + Sync>>,
    Box<dyn std::error::Error>,
>
where
    Source: TryInto<GraphicsCaptureItemType> + Send + 'static,
{
    let settings = Settings::new(
        source,
        cursor_settings,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        minimum_update_interval,
        DirtyRegionSettings::Default,
        ColorFormat::Rgba8,
        flags,
    );
    Ok(CaptureReceiver::start_free_threaded(settings)?)
}

/// Locates the window to capture by title.
///
/// Exact titles win; otherwise the first window whose title contains the text
/// (case-insensitively) is used. Failures list the visible window titles so a
/// typo in the configuration can be fixed on the spot.
fn capture_window_by_title(title: &str) -> Result<Window, Box<dyn std::error::Error>> {
    let title = title.trim();
    if title.is_empty() {
        return Err("capture.target = \"window\" requires capture.window_name".into());
    }

    if let Ok(window) = Window::from_name(title) {
        return Ok(window);
    }

    let lowercase_title = title.to_lowercase();
    let mut matching: Option<Window> = None;
    let mut titles: Vec<String> = Vec::new();
    for window in Window::enumerate()? {
        if let Ok(name) = window.title() {
            if matching.is_none() && name.to_lowercase().contains(&lowercase_title) {
                matching = Some(window);
            }
            titles.push(name);
        }
    }

    matching.map_or_else(
        || {
            Err(format!(
                "no window matching {title:?} (open windows: {})",
                titles.join("; ")
            )
            .into())
        },
        Ok,
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    set_process_dpi_awareness();

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
        active_game: Arc::new(games::action_rpg::ActionRPG::new(
            config.capture.sanitized_preview_edge(),
            config.game.sanitized_reference_aspect_ratio(),
        )),
        last_frame_hash: None,
        event_history: std::collections::VecDeque::new(),
    })
    .into();

    // Shared buffer for the latest compressed screenshot, plus the signal the capture
    // thread fires after every published frame so waiters wake immediately.
    let frame_buffer: SharedFrameBuffer = Default::default();
    let frame_notify: SharedFrameNotify = Default::default();

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

    let cursor_settings = if config.capture.with_cursor {
        CursorCaptureSettings::WithCursor
    } else {
        CursorCaptureSettings::WithoutCursor
    };
    let capture_flags = CaptureFlags {
        frame_buffer: frame_buffer.clone(),
        frame_notify: frame_notify.clone(),
        // `CaptureConfig` is no longer `Copy` (it carries the window title), so it
        // is cloned out of `config` here while the rest of `main` keeps using it.
        config: config.capture.clone(),
    };

    // Resolve the capture target: the whole primary monitor by default, or the window
    // named in the configuration. `start_capture` is generic over the source, so a
    // single call site fits either choice.
    // Keep the capture control alive for the lifetime of the process.
    let _capture_control = match config.capture.window_title() {
        None => start_capture(
            Monitor::primary()?,
            cursor_settings,
            minimum_update_interval,
            capture_flags,
        )?,
        Some(title) => start_capture(
            capture_window_by_title(title)?,
            cursor_settings,
            minimum_update_interval,
            capture_flags,
        )?,
    };

    // Build the configured input backend (enigo by default, input-simulator
    // when the `input-simulator` feature is enabled)
    let input = input::create_input()?;

    // Serve the MCP server over standard I/O (JSON-RPC via stdin/stdout)
    let server = GameServer::new(
        session_state,
        frame_buffer,
        frame_notify,
        input,
        config.server,
        config.capture.sanitized_preview_edge(),
        config.capture.sanitized_jpeg_quality(),
        instructions_path,
    );
    let service = server.serve(rmcp::transport::stdio()).await?;

    // Block until the client disconnects
    service.waiting().await?;
    Ok(())
}
