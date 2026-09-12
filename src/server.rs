//! MCP server: application context, tool argument schemas and tool implementations.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use enigo::{Button, Coordinate, Direction, Keyboard, Mouse};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, PromptMessage, Role, ServerCapabilities, ServerInfo},
    prompt, prompt_handler, prompt_router, schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::config::ServerConfig;
use crate::error::{input_error, internal_error, invalid_params};
use crate::games::Resolution;
use crate::input::{direction_scancode, key_scancode, ops};
use crate::state::{SharedFrameBuffer, SharedFrameNotify, SharedSession};
/// Application context shared by all MCP tools.
#[derive(Clone)]
pub struct GameServer<Input: Keyboard + Mouse + Send + 'static> {
    session: SharedSession,
    frame_buffer: SharedFrameBuffer,
    /// Signalled by the capture thread after every published frame; lets the
    /// wait-for-change loop sleep until a frame actually lands instead of polling.
    frame_notify: SharedFrameNotify,
    input: Arc<Mutex<Input>>,
    /// Tool limits loaded from the configuration file.
    limits: ServerConfig,
    /// Longest edge of the published preview, from the capture configuration; image-space
    /// mouse coordinates are scaled up to native display pixels with it.
    preview_edge: u32,
    /// Quality of the on-demand JPEG encode, from the capture configuration.
    jpeg_quality: u8,
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
    /// Target X coordinate in captured-frame (image) pixels; scaled to native display pixels.
    /// Ignored for relative moves (raw delta in image pixels).
    x: i32,
    /// Target Y coordinate in captured-frame (image) pixels; scaled to native display pixels.
    /// Ignored for relative moves (raw delta in image pixels).
    y: i32,
    /// If true, the coordinates are a raw delta applied to the current cursor position
    /// (no scaling; for camera look, aiming and other mickey-based camera control)
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
pub struct HoldMouseArgs {
    /// Mouse button to act on: "left" (default), "right" or "middle"
    button: Option<String>,
    /// What to do: "hold" (default: press, wait `duration_ms`, release), "press"
    /// (keep the button held down) or "release" (release a held button)
    action: Option<String>,
    /// How long to hold the button in milliseconds for the "hold" action (default 50, max 10000)
    duration_ms: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ScrollMouseArgs {
    /// Scroll amount in wheel clicks; positive scrolls up, negative scrolls down
    amount: i32,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WaitArgs {
    /// How long to wait in milliseconds (default 500, max from the server's max_wait_ms limit)
    duration_ms: Option<u64>,
    /// Optional note about what this delay is for (e.g. "loading screen", "death respawn"); recorded in the session log
    reason: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CaptureScreenArgs {
    /// When true, skip the wait-for-change window and return the latest frame immediately,
    /// even if the screen has not visibly changed. Use this to inspect static screens
    /// (menus, dialogue, inventory) that would otherwise time out without an image.
    force: Option<bool>,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: SharedSession,
        frame_buffer: SharedFrameBuffer,
        frame_notify: SharedFrameNotify,
        input: Input,
        limits: ServerConfig,
        preview_edge: u32,
        jpeg_quality: u8,
        instructions_path: std::path::PathBuf,
    ) -> Self {
        Self {
            session,
            frame_buffer,
            frame_notify,
            input: Arc::new(Mutex::new(input)),
            limits,
            preview_edge,
            jpeg_quality,
            instructions_path,
        }
    }

    /// Grabs a highly optimized frame of the primary monitor.
    ///
    /// If the screen has not visibly changed since the last capture, this blocks until it does
    /// (or until the configured wait timeout elapses), so a static screen never produces a
    /// redundant frame or a redundant model round trip. Pass `force: true` to skip the wait
    /// and always receive the latest frame as an image, which is the only way to inspect a
    /// screen that stays visually static (menus, dialogue, inventory screens).
    #[tool(description = "Grabs a highly optimized frame of the primary monitor.")]
    async fn capture_screen(
        &self,
        Parameters(CaptureScreenArgs { force }): Parameters<CaptureScreenArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Block until the screen changes; this is the server-side replacement for a "wait" tool.
        // A forced capture skips the wait entirely and always returns an image.
        let force = force.unwrap_or(false);
        let timed_out = !force && self.wait_for_screen_change().await?;

        // Session lock first, frame lock second: the frame lock is a `std` mutex and must never
        // be held across the await below.
        let mut session = self.session.write().await;

        // Everything read out of the frame happens under one short critical section: sparse
        // telemetry pixel samples and the on-demand JPEG encode. No base64 encoding runs
        // under the lock, so the capture thread's `on_frame_arrived` is never blocked behind
        // per-call image work.
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
            // Only record the frame hash when the screen actually changed: overwriting it
            // during a timed-out (static) check would make every subsequent call re-poll
            // against the identical hash and time out forever until something else moves
            // the display.
            if !timed_out {
                session.last_frame_hash = Some(frame.hash);
            }

            // The JPEG is encoded on demand (the capture thread no longer pre-encodes) and
            // handed out so the lock can be dropped before the base64 encode below.
            let jpeg = frame
                .encode_jpeg(self.jpeg_quality)
                .map_err(|error| internal_error(format!("failed to encode preview: {error}")))?;
            (metrics, jpeg)
        };

        let metrics = telemetry;
        if timed_out {
            return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "Screen unchanged for the wait window. Telemetry: HP = {}%, Stamina = {}%, \
                Abilities: Q = {}, R = {}, F = {}. \
                No visible change detected; wait longer, take a different action, or call \
                again with force=true to receive the current frame as an image.",
                metrics.player_hp,
                metrics.stamina,
                Self::format_ability(metrics.q_ready),
                Self::format_ability(metrics.r_ready),
                Self::format_ability(metrics.f_ready)
            ))]));
        }

        let img_base64 = base64::engine::general_purpose::STANDARD.encode(jpeg);
        Ok(CallToolResult::success(vec![
            ContentBlock::text(format!(
                "Display frame captured. Server-side Telemetry: HP = {}%, Stamina = {}%, \
                Abilities: Q = {}, R = {}, F = {}.",
                metrics.player_hp,
                metrics.stamina,
                Self::format_ability(metrics.q_ready),
                Self::format_ability(metrics.r_ready),
                Self::format_ability(metrics.f_ready)
            )),
            ContentBlock::image(img_base64, "image/jpeg"),
        ]))
    }

    /// Formats an ability readiness flag as READY or COOLDOWN for telemetry text.
    fn format_ability(ready: bool) -> &'static str {
        if ready { "READY" } else { "COOLDOWN" }
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
        let json = serde_json::to_string(&session.current_metrics).map_err(internal_error)?;

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
        let duration =
            Duration::from_millis(duration_ms.unwrap_or(500).min(self.limits.max_hold_ms));

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
        let duration =
            Duration::from_millis(duration_ms.unwrap_or(50).min(self.limits.max_hold_ms));

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
    /// Absolute coordinates are in the captured frame's image space (the preview scaled to
    /// the configured preview edge on its longest edge) and are scaled up to native display
    /// pixels before the move, so a position picked off the image lands on the same
    /// physical spot. The process runs with per-monitor DPI awareness, so the display
    /// dimensions used for that scaling are physical pixels, matching what
    /// windows-capture captures. Relative coordinates are raw mickey deltas passed
    /// through unscaled: camera look operates on deltas, not image offsets.
    #[tool(
        description = "Moves the mouse cursor to absolute image-space coordinates (auto-scaled to native display pixels), or by a raw unscaled delta relative to its current position."
    )]
    async fn move_mouse(
        &self,
        Parameters(MoveMouseArgs { x, y, relative }): Parameters<MoveMouseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let relative = relative.unwrap_or(false);
        let coordinate = if relative {
            Coordinate::Rel
        } else {
            Coordinate::Abs
        };
        let (image_x, image_y) = (x, y);

        let preview_edge = self.preview_edge as f64;
        let input = Arc::clone(&self.input);
        let (target, native, final_position) =
            tokio::task::spawn_blocking(move || -> MouseMoveResult {
                    let mut simulator = input.lock().map_err(|e| e.to_string())?;

                // Absolute: image-space coordinates must be scaled up to native display
                // pixels before the move. Relative: pass the raw delta through unscaled,
                // since game cameras consume mickey deltas, not image-space offsets.
                let (target_x, target_y, native) = if relative {
                    (x, y, (0, 0))
                } else {
                    // The capture pipeline publishes a preview scaled to the configured
                    // preview edge on its longest edge (never upscaled), so image-space
                    // coordinates must be scaled up to native display pixels before the
                    // move. With DPI awareness set the display dimensions are physical
                    // pixels, matching the captured frame.
                    let (native_w, native_h) = simulator.main_display().map_err(|e| e.to_string())?;
                    let longest = native_w.max(native_h);
                    let scale = if longest as f64 > preview_edge {
                        longest as f64 / preview_edge
                    } else {
                        1.0
                    };
                    (
                        (x as f64 * scale).round() as i32,
                        (y as f64 * scale).round() as i32,
                        (native_w, native_h),
                    )
                };

                simulator
                    .move_mouse(target_x, target_y, coordinate)
                    .map_err(|e| e.to_string())?;
                let final_position = simulator.location().map_err(|e| e.to_string())?;
                Ok(((target_x, target_y), native, final_position))
            })
            .await
            .map_err(input_error)?
            .map_err(input_error)?;

        let message = if relative {
            format!(
                "Mouse moved by relative delta ({}, {}); now at cursor ({}, {}).",
                image_x, image_y, final_position.0, final_position.1
            )
        } else {
            format!(
                "Mouse moved to image ({}, {}) -> native ({}, {}) on a {}x{} display; \
                 now at cursor ({}, {}).",
                image_x,
                image_y,
                target.0,
                target.1,
                native.0,
                native.1,
                final_position.0,
                final_position.1
            )
        };
        self.session.write().await.record_event(message.clone());

        Ok(CallToolResult::success(vec![ContentBlock::text(message)]))
    }

    /// Clicks a mouse button (left/right/middle, optional double click).
    ///
    /// For mechanics that need the button held (charging abilities, camera orbit
    /// drag, inventory drag-and-drop), use `hold_mouse` instead.
    #[tool(
        description = "Clicks a mouse button: left (default), right or middle; supports double click. For button holds and drags use hold_mouse."
    )]
    async fn click_mouse(
        &self,
        Parameters(ClickMouseArgs { button, double }): Parameters<ClickMouseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let button = parse_button(button.as_deref())?;
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

    /// Holds, presses or releases a mouse button.
    ///
    /// "hold" keeps the button down for `duration_ms` (charge-and-release).
    /// "press"/"release" bracket a drag: press, `move_mouse` while held, release.
    #[tool(
        description = "Mouse button press-and-hold: action 'hold' (default) presses, waits duration_ms and releases; 'press' keeps the button down and 'release' releases it, so press + move_mouse + release performs a drag or camera orbit. Buttons: left (default), right or middle."
    )]
    async fn hold_mouse(
        &self,
        Parameters(HoldMouseArgs {
            button,
            action,
            duration_ms,
        }): Parameters<HoldMouseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let button = parse_button(button.as_deref())?;
        let action = action
            .unwrap_or_else(|| "hold".to_string())
            .to_lowercase();
        let duration =
            Duration::from_millis(duration_ms.unwrap_or(50).min(self.limits.max_hold_ms));

        let input = Arc::clone(&self.input);
        let action_task = action.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut simulator = input.lock().map_err(|e| e.to_string())?;
            match action_task.as_str() {
                "hold" => {
                    simulator
                        .button(button, Direction::Press)
                        .map_err(|e| e.to_string())?;
                    std::thread::sleep(duration);
                    simulator
                        .button(button, Direction::Release)
                        .map_err(|e| e.to_string())
                }
                "press" => simulator.button(button, Direction::Press).map_err(|e| e.to_string()),
                "release" => {
                    simulator
                        .button(button, Direction::Release)
                        .map_err(|e| e.to_string())
                }
                _ => Err(format!(
                    "invalid action: {}. Use hold/press/release.",
                    action_task
                )),
            }
        })
        .await
        .map_err(input_error)?
        .map_err(input_error)?;

        let message = match action.as_str() {
            "hold" => format!(
                "Held {:?} mouse button for {} ms, then released.",
                button,
                duration.as_millis()
            ),
            other => format!("Mouse button {:?} {}ed.", button, other),
        };
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

    /// Waits for a fixed delay so the game state can settle.
    ///
    /// Useful after loading screens, respawns, teleports, cutscenes or any action
    /// whose result arrives later than the next frame. The server caps the duration
    /// at `max_wait_ms`; when a longer pause is needed, call `wait` repeatedly.
    #[tool(
        description = "Waits for a fixed delay (e.g. after a loading screen, respawn or teleport) before acting again."
    )]
    async fn wait(
        &self,
        Parameters(WaitArgs { duration_ms, reason }): Parameters<WaitArgs>,
    ) -> Result<CallToolResult, McpError> {
        let duration = Duration::from_millis(duration_ms.unwrap_or(500).min(self.limits.max_wait_ms));

        tokio::time::sleep(duration).await;

        let message = match reason.as_deref() {
            Some(reason) if !reason.is_empty() => {
                format!("Waited {} ms: {}.", duration.as_millis(), reason)
            }
            _ => format!("Waited {} ms.", duration.as_millis()),
        };
        self.session.write().await.record_event(message.clone());

        Ok(CallToolResult::success(vec![ContentBlock::text(message)]))
    }
}

/// Parses a tool's button name argument into the enigo `Button`.
fn parse_button(button: Option<&str>) -> Result<Button, McpError> {
    match button.unwrap_or("left").to_lowercase().as_str() {
        "left" => Ok(Button::Left),
        "right" => Ok(Button::Right),
        "middle" => Ok(Button::Middle),
        other => Err(invalid_params(format!(
            "Invalid button: {}. Use left/right/middle.",
            other
        ))),
    }
}

/// Spawn-blocking result of a mouse move: `(target, native display size, cursor
/// position after the move)`.
type MouseMoveResult = Result<((i32, i32), (i32, i32), (i32, i32)), String>;

impl<Input: Keyboard + Mouse + Send + 'static> GameServer<Input> {
    /// Waits until the screen visibly changes or the wait times out.
    ///
    /// Returns `true` if the wait timed out with the screen still static.
    ///
    /// Event-driven: the capture thread signals [`Self::frame_notify`] after every
    /// published frame, so this parks on `Notified` instead of re-polling the buffer on
    /// an interval. Each iteration registers its `Notified` future *before* re-checking
    /// the buffer, which closes the lost-wakeup race between the check and the park.
    ///
    /// Lock discipline: session lock first, frame lock second, and neither is ever held
    /// across an await.
    async fn wait_for_screen_change(&self) -> Result<bool, McpError> {
        let deadline = tokio::time::Instant::now() + self.limits.wait_for_change_timeout();
        loop {
            // Register interest in the next frame signal before inspecting the buffer: a
            // frame published between the check and the await still completes the future.
            let notified = self.frame_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let changed = {
                let session = self.session.read().await;
                let last_hash = session.last_frame_hash;
                let guard = match self.frame_buffer.lock() {
                    Ok(lock) => lock,
                    Err(poisoned) => poisoned.into_inner(),
                };
                match guard.as_ref() {
                    // No buffer published yet; keep waiting.
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
            if tokio::time::Instant::now() >= deadline {
                return Ok(true);
            }
            // Sleep until the next frame signal or the deadline, whichever comes first.
            // Frames arriving while the screen stays static simply re-run the check.
            if tokio::time::timeout_at(deadline, notified.as_mut()).await.is_err() {
                return Ok(true);
            }
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
