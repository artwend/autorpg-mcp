//! MCP server: application context, tool argument schemas and tool implementations.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use enigo::{Button, Coordinate, Direction, Keyboard, Mouse};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, PromptMessage, Role, ServerCapabilities, ServerInfo},
    prompt, prompt_handler, prompt_router, schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::capture::PREVIEW_EDGE;
use crate::config::ServerConfig;
use crate::error::{input_error, internal_error, invalid_params};
use crate::games::Resolution;
use crate::input::{direction_scancode, key_scancode, ops};
use crate::state::{SharedFrameBuffer, SharedSession};
/// Application context shared by all MCP tools.
#[derive(Clone)]
pub struct GameServer<Input: Keyboard + Mouse + Send + 'static> {
    session: SharedSession,
    frame_buffer: SharedFrameBuffer,
    input: Arc<Mutex<Input>>,
    /// Tool limits loaded from the configuration file.
    limits: ServerConfig,
    /// Path of the prompt instructions template, resolved against the configuration
    /// file's directory. Re-read on every prompt call so edits apply without a restart.
    instructions_path: std::path::PathBuf,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct UpdateMetricsArgs {
    /// Current player HP evaluation
    hp: i32,
    /// Current zone location identifier
    location: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct MovePlayerArgs {
    /// Movement direction: "forward" (W), "back" (S), "left" (A) or "right" (D)
    direction: String,
    /// How long to hold the movement key in milliseconds (default 500, max 10000)
    duration_ms: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PressKeyArgs {
    /// The key to press: a single character (e.g. "e", "1", " ") or a named key (e.g. "space", "enter", "escape", "tab", "f1")
    key: String,
    /// How long to hold the key in milliseconds (default 50, max 10000)
    duration_ms: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct InputTextArgs {
    /// The text to type into the focused input field (chat, search, etc.)
    text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct MoveMouseArgs {
    /// Target X coordinate in captured-frame (image) pixels; scaled to native display pixels
    x: i32,
    /// Target Y coordinate in captured-frame (image) pixels; scaled to native display pixels
    y: i32,
    /// If true, the coordinates are relative to the current cursor position
    relative: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ClickMouseArgs {
    /// Mouse button to click: "left" (default), "right" or "middle"
    button: Option<String>,
    /// If true, perform a double click
    double: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ScrollMouseArgs {
    /// Scroll amount in wheel clicks; positive scrolls up, negative scrolls down
    amount: i32,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct StartFarmArgs {
    /// How long to farm in minutes (default 10)
    duration_minutes: Option<u32>,
    /// Mob type to focus on (e.g. "boar", "wolf"); empty means any nearby mob
    target: Option<String>,
    /// HP percentage at which to drink a potion (default 30)
    potion_threshold: Option<i32>,
}

#[tool_router]
impl<Input: Keyboard + Mouse + Send + 'static> GameServer<Input> {
    pub fn new(
        session: SharedSession,
        frame_buffer: SharedFrameBuffer,
        input: Input,
        limits: ServerConfig,
        instructions_path: std::path::PathBuf,
    ) -> Self {
        Self {
            session,
            frame_buffer,
            input: Arc::new(Mutex::new(input)),
            limits,
            instructions_path,
        }
    }

    /// Grabs a highly optimized frame of the primary monitor.
    ///
    /// If the screen has not visibly changed since the last capture, this blocks until it does
    /// (or until the configured wait timeout elapses), so a static screen never produces a
    /// redundant frame or a redundant model round trip.
    #[tool(description = "Grabs a highly optimized frame of the primary monitor.")]
    async fn capture_screen(&self) -> Result<CallToolResult, McpError> {
        // Block until the screen changes; this is the server-side replacement for a "wait" tool.
        let timed_out = self.wait_for_screen_change().await?;

        // Session lock first, frame lock second: the frame lock is a `std` mutex and must never
        // be held across the await below.
        let mut session = self.session.write().await;

        // Everything read out of the frame happens under one short critical section: sparse
        // telemetry pixel samples and a copy of the (small) JPEG bytes. No image construction
        // or base64 encoding runs under the lock, so the capture thread's `on_frame_arrived`
        // is never blocked behind per-call image work.
        let (telemetry, jpeg) = {
            let guard = match self.frame_buffer.lock() {
                Ok(lock) => lock,
                Err(poisoned) => poisoned.into_inner(),
            };
            let Some(frame) = guard.as_ref() else {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "No active display buffer detected yet. Try again.",
                )]));
            };

            let Some(pixels) = frame.as_rgb_view() else {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "Published display frame is malformed. Try again.",
                )]));
            };
            let resolution = Resolution::new(frame.width, frame.height);
            let mut metrics = session.active_game.parse_telemetry(&pixels, resolution);
            // Zone is MCP-managed via `update_game_metrics`; pixel parsing cannot read it,
            // so carry the session's location forward instead of dropping it.
            metrics.location = session.current_metrics.location.clone();
            // Persist parsed telemetry so `get_game_metrics` and `record_event` snapshots
            // reflect the live frame instead of stale initialization data.
            session.current_metrics = metrics.clone();
            session.last_frame_hash = Some(frame.hash);

            // The JPEG is copied out so the lock can be dropped before the (potentially
            // slow) base64 encode below.
            (metrics, frame.jpeg.clone())
        };

        let metrics = telemetry;
        if timed_out {
            return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "Screen unchanged for the wait window. Telemetry: HP = {}%, Stamina = {}%. \
                No visible change detected; wait longer or take a different action.",
                metrics.player_hp, metrics.stamina
            ))]));
        }

        let img_base64 = base64::engine::general_purpose::STANDARD.encode(jpeg);
        Ok(CallToolResult::success(vec![
            ContentBlock::text(format!(
                "Display frame captured. Server-side Telemetry: HP = {}%, Stamina = {}%.",
                metrics.player_hp, metrics.stamina
            )),
            ContentBlock::image(img_base64, "image/jpeg"),
        ]))
    }

    /// Mutates variables and logs a state transition context record.
    #[tool(description = "Mutates variables and logs a state transition context record.")]
    async fn update_game_metrics(
        &self,
        Parameters(UpdateMetricsArgs { hp, location }): Parameters<UpdateMetricsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut session = self.session.write().await;
        session.current_metrics.player_hp = hp;
        session.current_metrics.location = location.clone();
        session.record_event(format!("State synchronized. Zone: {}", location));

        Ok(CallToolResult::success(vec![ContentBlock::text(
            "In-memory metrics synced successfully.",
        )]))
    }

    /// Returns the current in-memory game metrics as JSON.
    #[tool(
        description = "Returns the current in-memory game metrics snapshot (HP, stamina, ability readiness, location, combat state) as JSON."
    )]
    async fn get_game_metrics(&self) -> Result<CallToolResult, McpError> {
        let session = self.session.read().await;
        let json =
            serde_json::to_string_pretty(&session.current_metrics).map_err(internal_error)?;

        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    /// Moves the player by holding a WASD movement key for the given duration.
    #[tool(
        description = "Moves the player by holding a WASD movement key (layout-independent scancode)."
    )]
    async fn move_player(
        &self,
        Parameters(MovePlayerArgs {
            direction,
            duration_ms,
        }): Parameters<MovePlayerArgs>,
    ) -> Result<CallToolResult, McpError> {
        let scancode = direction_scancode(&direction).ok_or_else(|| {
            invalid_params(format!(
                "Invalid direction mapping: {}. Use forward/back/left/right.",
                direction
            ))
        })?;
        let duration = Duration::from_millis(duration_ms.unwrap_or(500).min(self.limits.max_hold_ms));

        let input = Arc::clone(&self.input);
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut simulator = input.lock().map_err(|e| e.to_string())?;
            ops::hold_scancode(&mut *simulator, scancode, duration).map_err(|e| e.to_string())
        })
        .await
        .map_err(input_error)?
        .map_err(input_error)?;

        let message = format!(
            "Moved {} for {} ms.",
            direction.to_lowercase(),
            duration.as_millis()
        );
        self.session.write().await.record_event(message.clone());

        Ok(CallToolResult::success(vec![ContentBlock::text(message)]))
    }

    /// Presses (and optionally holds) a keyboard key.
    ///
    /// Accepts a single character (e.g. "e", "1", " ") or a named key
    /// (e.g. "space", "enter", "escape", "tab", "f1"). The key is dispatched as a
    /// physical scancode so DirectInput/RawInput game engines receive it
    /// regardless of the active keyboard layout. For typing text into input
    /// fields, use `input_text` instead.
    #[tool(
        description = "Presses a keyboard key (single character or named key like 'space', 'enter', 'escape', 'tab', 'f1') as a physical scancode, optionally holding it."
    )]
    async fn press_key(
        &self,
        Parameters(PressKeyArgs { key, duration_ms }): Parameters<PressKeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let scancode = key_scancode(&key).ok_or_else(|| {
            invalid_params(format!(
                "Invalid key: {:?}. Use a single character (e.g. \"e\", \"1\", \" \") or a named key (e.g. \"space\", \"enter\", \"escape\", \"tab\", \"f1\").",
                key
            ))
        })?;
        let duration = Duration::from_millis(duration_ms.unwrap_or(50).min(self.limits.max_hold_ms));

        let input = Arc::clone(&self.input);
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut simulator = input.lock().map_err(|e| e.to_string())?;
            ops::hold_scancode(&mut *simulator, scancode, duration).map_err(|e| e.to_string())
        })
        .await
        .map_err(input_error)?
        .map_err(input_error)?;

        let message = format!("Key '{}' pressed for {} ms.", key, duration.as_millis());
        self.session.write().await.record_event(message.clone());

        Ok(CallToolResult::success(vec![ContentBlock::text(message)]))
    }

    /// Types text into the focused input field.
    ///
    /// Dispatches each character as a Unicode text event (`KEYEVENTF_UNICODE`),
    /// which is what text input fields consume. Game engines ignore these events,
    /// so use `press_key` for gameplay input.
    #[tool(
        description = "Types text into the focused input field (chat, search, etc.) using Unicode text events."
    )]
    async fn input_text(
        &self,
        Parameters(InputTextArgs { text }): Parameters<InputTextArgs>,
    ) -> Result<CallToolResult, McpError> {
        if text.is_empty() {
            return Err(invalid_params("Text must not be empty."));
        }

        let message = format!("Typed text: {:?}", text);

        let input = Arc::clone(&self.input);
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut simulator = input.lock().map_err(|e| e.to_string())?;
            ops::type_text(&mut *simulator, &text).map_err(|e| e.to_string())
        })
        .await
        .map_err(input_error)?
        .map_err(input_error)?;

        self.session.write().await.record_event(message.clone());

        Ok(CallToolResult::success(vec![ContentBlock::text(message)]))
    }

    /// Moves the mouse cursor to absolute (or relative) screen coordinates.
    ///
    /// Coordinates are in the captured frame's image space (the preview scaled to
    /// [`PREVIEW_EDGE`] on its longest edge) and are scaled up to native display pixels before
    /// the move, so a position picked off the image lands on the same physical spot.
    #[tool(
        description = "Moves the mouse cursor to absolute image-space coordinates (auto-scaled to native display pixels), or relative to its current position."
    )]
    async fn move_mouse(
        &self,
        Parameters(MoveMouseArgs { x, y, relative }): Parameters<MoveMouseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let coordinate = if relative.unwrap_or(false) {
            Coordinate::Rel
        } else {
            Coordinate::Abs
        };
        let (image_x, image_y) = (x, y);

        let input = Arc::clone(&self.input);
        let (x, y) = tokio::task::spawn_blocking(move || -> Result<(i32, i32), String> {
            let mut simulator = input.lock().map_err(|e| e.to_string())?;

            // The capture pipeline publishes a preview scaled to PREVIEW_EDGE on its longest
            // edge (never upscaled), so image-space coordinates must be scaled up to native
            // display pixels before the move.
            let (native_w, native_h) = simulator.main_display().map_err(|e| e.to_string())?;
            let longest = native_w.max(native_h);
            let scale = if longest > PREVIEW_EDGE as i32 {
                longest as f64 / PREVIEW_EDGE as f64
            } else {
                1.0
            };
            let target_x = (x as f64 * scale).round() as i32;
            let target_y = (y as f64 * scale).round() as i32;

            simulator
                .move_mouse(target_x, target_y, coordinate)
                .map_err(|e| e.to_string())?;
            simulator.location().map_err(|e| e.to_string())
        })
        .await
        .map_err(input_error)?
        .map_err(input_error)?;

        let message = format!(
            "Mouse moved to image ({}, {}) -> native ({}, {}).",
            image_x, image_y, x, y
        );
        self.session.write().await.record_event(message.clone());

        Ok(CallToolResult::success(vec![ContentBlock::text(message)]))
    }

    /// Clicks a mouse button (left/right/middle, optional double click).
    #[tool(
        description = "Clicks a mouse button: left (default), right or middle; supports double click."
    )]
    async fn click_mouse(
        &self,
        Parameters(ClickMouseArgs { button, double }): Parameters<ClickMouseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let button = match button
            .unwrap_or_else(|| "left".to_string())
            .to_lowercase()
            .as_str()
        {
            "left" => Button::Left,
            "right" => Button::Right,
            "middle" => Button::Middle,
            other => {
                return Err(invalid_params(format!(
                    "Invalid button: {}. Use left/right/middle.",
                    other
                )));
            }
        };
        let double = double.unwrap_or(false);

        let input = Arc::clone(&self.input);
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut simulator = input.lock().map_err(|e| e.to_string())?;
            simulator
                .button(button, Direction::Click)
                .map_err(|e| e.to_string())?;
            if double {
                std::thread::sleep(Duration::from_millis(50));
                simulator
                    .button(button, Direction::Click)
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        })
        .await
        .map_err(input_error)?
        .map_err(input_error)?;

        let message = format!(
            "{} {:?} mouse button clicked.",
            if double { "Double" } else { "Single" },
            button
        );
        self.session.write().await.record_event(message.clone());

        Ok(CallToolResult::success(vec![ContentBlock::text(message)]))
    }

    /// Scrolls the mouse wheel vertically.
    #[tool(
        description = "Scrolls the mouse wheel; positive amounts scroll up, negative scroll down (Windows WHEEL_DELTA convention)."
    )]
    async fn scroll_mouse(
        &self,
        Parameters(ScrollMouseArgs { amount }): Parameters<ScrollMouseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let input = Arc::clone(&self.input);
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut simulator = input.lock().map_err(|e| e.to_string())?;
            ops::scroll_vertical(&mut *simulator, amount).map_err(|e| e.to_string())
        })
        .await
        .map_err(input_error)?
        .map_err(input_error)?;

        let message = format!(
            "Scrolled {} clicks {}.",
            amount.abs(),
            if amount >= 0 { "up" } else { "down" }
        );
        self.session.write().await.record_event(message.clone());

        Ok(CallToolResult::success(vec![ContentBlock::text(message)]))
    }
}

impl<Input: Keyboard + Mouse + Send + 'static> GameServer<Input> {
    /// Polls the shared frame buffer until the screen visibly changes or the wait times out.
    ///
    /// Returns `true` if the wait timed out with the screen still static.
    ///
    /// Lock discipline: session lock first, frame lock second, and neither is ever held
    /// across an await.
    async fn wait_for_screen_change(&self) -> Result<bool, McpError> {
        let deadline = Instant::now() + self.limits.wait_for_change_timeout();
        loop {
            let changed = {
                let session = self.session.read().await;
                let last_hash = session.last_frame_hash;
                let guard = match self.frame_buffer.lock() {
                    Ok(lock) => lock,
                    Err(poisoned) => poisoned.into_inner(),
                };
                match guard.as_ref() {
                    // No buffer published yet; keep polling.
                    None => false,
                    Some(frame) => last_hash.is_none_or(|last| {
                        calculate_hamming_distance(frame.hash, last)
                            > self.limits.stale_hash_distance
                    }),
                }
            };
            if changed {
                return Ok(false);
            }
            if Instant::now() >= deadline {
                return Ok(true);
            }
            tokio::time::sleep(self.limits.wait_poll_interval()).await;
        }
    }
}

/// Computes the Hamming Distance (number of differing bits) between two hashes.
/// Returns a value between 0 (identical) and 64 (completely different).
fn calculate_hamming_distance(hash1: u64, hash2: u64) -> u32 {
    // XOR finds differing bits, count_ones counts them
    (hash1 ^ hash2).count_ones()
}

#[prompt_router]
impl<Input: Keyboard + Mouse + Send + 'static> GameServer<Input> {
    /// Starts an automated mob-farming loop.
    #[prompt(
        description = "Starts an automated mob-farming loop that moves to nearby mobs, attacks them, and uses abilities/potions until the target duration elapses."
    )]
    async fn start_farm(
        &self,
        Parameters(args): Parameters<StartFarmArgs>,
    ) -> Result<Vec<PromptMessage>, McpError> {
        let duration = args.duration_minutes.unwrap_or(10);
        let target = args.target.unwrap_or_else(|| "any nearby mob".to_string());
        let potion_threshold = args.potion_threshold.unwrap_or(30).clamp(0, 100);

        // The template lives outside the binary so it can be edited without a rebuild;
        // `{target}`, `{duration}` and `{potion_threshold}` are substituted per call.
        let template = std::fs::read_to_string(&self.instructions_path).map_err(|error| {
            internal_error(format!(
                "failed to read prompt instructions from {}: {}",
                self.instructions_path.display(),
                error
            ))
        })?;
        let instructions = template
            .replace("{target}", &target)
            .replace("{duration}", &duration.to_string())
            .replace("{potion_threshold}", &potion_threshold.to_string());

        Ok(vec![PromptMessage::new_text(Role::User, instructions)])
    }
}

#[tool_handler]
#[prompt_handler]
impl<Input: Keyboard + Mouse + Send + 'static> ServerHandler for GameServer<Input> {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::from_build_env())
    }
}
