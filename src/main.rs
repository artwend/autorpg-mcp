//! autorpg-mcp: an MCP server exposing screen capture, input simulation and
//! game-state tracking tools for game automation.

mod capture;
mod error;
mod input;
mod server;
mod state;

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

use capture::{CaptureReceiver, OS_UPDATE_HINT};
use server::GameServer;
use state::{GameMetrics, SessionState, SharedFrameBuffer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize in-memory session parameters
    let session_state = RwLock::new(SessionState {
        current_metrics: GameMetrics {
            player_hp: 100,
            stamina: 100,
            q_ready: true,
            r_ready: true,
            f_ready: true,
            location: "Starter Village".to_string(),
        },
        event_history: Vec::new(),
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
        MinimumUpdateIntervalSettings::Custom(OS_UPDATE_HINT)
    } else {
        MinimumUpdateIntervalSettings::Default
    };

    let primary_monitor = Monitor::primary()?;
    let settings = Settings::new(
        primary_monitor,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        minimum_update_interval,
        DirtyRegionSettings::Default,
        ColorFormat::Rgba8,
        frame_buffer.clone(),
    );

    // Keep the capture control alive for the lifetime of the process
    let _capture_control = CaptureReceiver::start_free_threaded(settings)?;

    // Build the configured input backend (enigo by default, input-simulator
    // when the `input-simulator` feature is enabled)
    let input = input::create_input()?;

    // Serve the MCP server over standard I/O (JSON-RPC via stdin/stdout)
    let server = GameServer::new(session_state, frame_buffer, input);
    let service = server.serve(rmcp::transport::stdio()).await?;

    // Block until the client disconnects
    service.waiting().await?;
    Ok(())
}
