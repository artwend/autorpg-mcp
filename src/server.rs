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
use crate::windmouse;
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
    /// Quality of the on-demand JPEG encode, from the capture configuration.
    jpeg_quality: u8,
    /// Path of the prompt instructions template, resolved against the configuration
    /// file's directory. Re-read on every prompt call so edits apply without a restart.
    instructions_path: std::path::PathBuf,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SetZoneArgs {
    /// Name of the new zone or area (read from compass, minimap, or screen banner)
    pub location: String,
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
    /// If true (the default), an absolute move travels along a human-like WindMouse
    /// path (curved, accelerated and settled) instead of jumping straight to the
    /// target. Set to false for a single instantaneous move.
    human_like: Option<bool>,
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
        jpeg_quality: u8,
        instructions_path: std::path::PathBuf,
    ) -> Self {
        Self {
            session,
            frame_buffer,
            frame_notify,
            input: Arc::new(Mutex::new(input)),
            limits,
            jpeg_quality,
            instructions_path,
        }
    }

    /// Grabs a highly optimized frame of the captured screen (monitor or window).
    ///
    /// If the screen has not visibly changed since the last capture, this blocks until it does
    /// (or until the configured wait timeout elapses), so a static screen never produces a
    /// redundant frame or a redundant model round trip. Pass `force: true` to skip the wait
    /// and always receive the latest frame as an image, which is the only way to inspect a
    /// screen that stays visually static (menus, dialogue, inventory screens).
    #[tool(description = "Grabs a highly optimized frame of the captured screen.")]
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
            // handed out so the lock can be dropped before the base64 encode below. A
            // timed-out (static) capture returns no image, so skip the encode entirely
            // instead of paying for it under the locks only to discard the result.
            let jpeg = if timed_out {
                None
            } else {
                Some(
                    frame
                        .encode_jpeg(self.jpeg_quality)
                        .map_err(|error| internal_error(format!("failed to encode preview: {error}")))?,
                )
            };
            (metrics, jpeg)
        };

        let metrics = telemetry;
        let telemetry_text = format!(
            "[HP: {} | Stamina: {} | Q: {} | R: {} | F: {} | Zone: {}]",
            Self::format_percent(metrics.player_hp),
            Self::format_percent(metrics.stamina),
            Self::format_ability(metrics.q_ready),
            Self::format_ability(metrics.r_ready),
            Self::format_ability(metrics.f_ready),
            if metrics.location.is_empty() {
                "Unknown"
            } else {
                &metrics.location
            }
        );

        if timed_out {
            return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "Screen unchanged. Telemetry: {telemetry_text}. \
                Wait longer, take an action, or call with force=true for an image."
            ))]));
        }

        // `timed_out` returned above, so a JPEG is always present here.
        let jpeg = jpeg.ok_or_else(|| internal_error("missing preview for a captured frame"))?;
        let img_base64 = base64::engine::general_purpose::STANDARD.encode(jpeg);
        Ok(CallToolResult::success(vec![
            ContentBlock::text(format!("Frame captured. Telemetry: {telemetry_text}")),
            ContentBlock::image(img_base64, "image/jpeg"),
        ]))
    }

    /// Formats an ability readiness flag as READY or COOLDOWN for telemetry text.
    fn format_ability(ready: bool) -> &'static str {
        if ready { "READY" } else { "COOLDOWN" }
    }

    /// Formats a bar percentage for telemetry text. A negative value marks a bar the
    /// parser could not measure (`UNKNOWN_PERCENT`); rendering it as `?` keeps the model
    /// from mistaking a failed scan for a full bar.
    fn format_percent(value: i32) -> String {
        if value < 0 {
            "?".to_string()
        } else {
            format!("{value}%")
        }
    }

    /// Updates the active zone identifier when entering a new area.
    #[tool(description = "Updates the active zone/area name when entering a new region.")]
    async fn set_zone(
        &self,
        Parameters(SetZoneArgs { location }): Parameters<SetZoneArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut session = self.session.write().await;
        session.current_metrics.location = location.clone();
        session.record_event(format!("Zone updated: {location}"));

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Active zone set to: {location}."
        ))]))
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
        ensure_scancode_supported(scancode)?;
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
        ensure_scancode_supported(scancode)?;
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
    /// Absolute coordinates are in the captured frame's image space (the captured source
    /// scaled to the configured preview edge on its longest edge) and are scaled up by the
    /// source's own pixel dimensions — read out of the latest published frame — before the
    /// move, so a position picked off the image lands on the same physical spot. Using the
    /// source (monitor or window) rather than the display keeps the mapping correct when
    /// `capture.target = "window"`, where the captured area is smaller than the monitor.
    /// The process runs with per-monitor DPI awareness, so source pixels are physical
    /// pixels, matching what windows-capture captures.
    ///
    /// Absolute moves are sent as `Coordinate::Abs` because `MOUSEEVENTF_ABSOLUTE` (which
    /// enigo emits for it) reports the true mouse position, whereas this backend's other
    /// path is not a plain `MOUSEEVENTF_MOVE` relative event: with
    /// `windows_subject_to_mouse_speed_and_acceleration_level = false` (the crate default)
    /// enigo resolves `Coordinate::Rel` by reading the cursor and recursing back into the
    /// absolute path, which would make a relative move carry absolute-position noise into
    /// a game's camera. `Coordinate::Abs` is also free of the OS pointer-acceleration
    /// curve.
    ///
    /// Absolute moves are human-like by default: the path is interpolated with the
    /// WindMouse algorithm ([`crate::windmouse`]) so aiming looks like a person moving a
    /// mouse instead of a cursor landing on the target in one event. Pass
    /// `human_like: false` for a single instantaneous move, and keep relative moves
    /// (camera look, drag-orbit) at their raw mickey deltas in one event.
    #[tool(
        description = "Moves the mouse cursor to absolute image-space coordinates (auto-scaled to native display pixels), or by a raw unscaled delta relative to its current position. Absolute moves follow a human-like WindMouse path by default; pass human_like: false for an instant move."
    )]
    async fn move_mouse(
        &self,
        Parameters(MoveMouseArgs {
            x,
            y,
            relative,
            human_like,
        }): Parameters<MoveMouseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let relative = relative.unwrap_or(false);
        let human_like = human_like.unwrap_or(true);

        // Absolute: image-space coordinates are scaled up to the captured source's pixel
        // dimensions. The scale is taken from the latest published frame (native source
        // size over published preview size), which stays correct for window capture and
        // for sources below the preview ceiling (never upscaled); with no frame yet there
        // is nothing to aim at, so the call fails instead of guessing. Relative: pass the
        // raw delta through unscaled, since game cameras consume mickey deltas, not
        // image-space offsets.
        let scale = if relative {
            1.0
        } else {
            // Frame lock only, never held across an await: the closure below runs on a
            // blocking thread.
            let guard = match self.frame_buffer.lock() {
                Ok(lock) => lock,
                Err(poisoned) => poisoned.into_inner(),
            };
            let frame = guard.as_ref().ok_or_else(|| {
                invalid_params("No frame captured yet; call capture_screen first.")
            })?;
            let preview_longest = frame.width.max(frame.height) as f64;
            let source_longest = frame.source_width.max(frame.source_height) as f64;
            if preview_longest > 0.0 {
                source_longest / preview_longest
            } else {
                1.0
            }
        };

        let input = Arc::clone(&self.input);
        let (target, final_position, steps) =
            tokio::task::spawn_blocking(move || -> MouseMoveResult {
                let mut simulator = input.lock().map_err(|e| e.to_string())?;
                let (target_x, target_y) = if relative {
                    (x, y)
                } else {
                    (
                        (x as f64 * scale).round() as i32,
                        (y as f64 * scale).round() as i32,
                    )
                };

                let steps = if relative {
                    // Camera look consumes raw mickey deltas; interpolating them would
                    // turn one look into a swing, so a relative move stays one event.
                    simulator
                        .move_mouse(target_x, target_y, Coordinate::Rel)
                        .map_err(|e| e.to_string())?;
                    1
                } else if human_like {
                    windmouse::move_to(&mut *simulator, windmouse::Point::new(target_x, target_y))
                        .map_err(|e| e.to_string())?
                } else {
                    simulator
                        .move_mouse(target_x, target_y, Coordinate::Abs)
                        .map_err(|e| e.to_string())?;
                    1
                };

                let final_position = simulator.location().map_err(|e| e.to_string())?;
                Ok(((target_x, target_y), final_position, steps))
            })
            .await
            .map_err(input_error)?
            .map_err(input_error)?;

        let message = if relative {
            format!(
                "Mouse moved by relative delta ({}, {}); now at cursor ({}, {}).",
                x, y, final_position.0, final_position.1
            )
        } else if human_like {
            format!(
                "Mouse moved along a {steps}-step WindMouse path to image ({}, {}) -> \
                 native ({}, {}) (scale {:.3}); now at cursor ({}, {}).",
                x, y, target.0, target.1, scale, final_position.0, final_position.1
            )
        } else {
            format!(
                "Mouse moved to image ({}, {}) -> native ({}, {}) (scale {:.3}); \
                 now at cursor ({}, {}).",
                x, y, target.0, target.1, scale, final_position.0, final_position.1
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
        let action = parse_hold_action(action.as_deref())?;
        let duration =
            Duration::from_millis(duration_ms.unwrap_or(50).min(self.limits.max_hold_ms));

        let input = Arc::clone(&self.input);
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut simulator = input.lock().map_err(|e| e.to_string())?;
            match action {
                HoldAction::Hold => {
                    simulator
                        .button(button, Direction::Press)
                        .map_err(|e| e.to_string())?;
                    std::thread::sleep(duration);
                    simulator
                        .button(button, Direction::Release)
                        .map_err(|e| e.to_string())
                }
                HoldAction::Press => simulator
                    .button(button, Direction::Press)
                    .map_err(|e| e.to_string()),
                HoldAction::Release => simulator
                    .button(button, Direction::Release)
                    .map_err(|e| e.to_string()),
            }
        })
        .await
        .map_err(input_error)?
        .map_err(input_error)?;

        let message = match action {
            HoldAction::Hold => format!(
                "Held {:?} mouse button for {} ms, then released.",
                button,
                duration.as_millis()
            ),
            HoldAction::Press => format!("Mouse button {button:?} pressed (held down)."),
            HoldAction::Release => format!("Mouse button {button:?} released."),
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
        Parameters(WaitArgs {
            duration_ms,
            reason,
        }): Parameters<WaitArgs>,
    ) -> Result<CallToolResult, McpError> {
        let duration = duration_ms
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_millis(500))
            .min(self.limits.max_wait());

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

/// Rejects a scancode the active input backend cannot dispatch.
///
/// The enigo backend accepts every Set 1 scancode, but the input-simulator backend
/// works in virtual-key space, so a scancode without a virtual-key equivalent is an
/// invalid argument rather than an internal failure.
fn ensure_scancode_supported(scancode: u16) -> Result<(), McpError> {
    if ops::scancode_supported(scancode) {
        Ok(())
    } else {
        Err(invalid_params(format!(
            "Scancode 0x{scancode:X} cannot be sent by the active input backend."
        )))
    }
}

/// Action requested by `hold_mouse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoldAction {
    /// Press, wait `duration_ms`, release.
    Hold,
    /// Keep the button held down.
    Press,
    /// Release a held button.
    Release,
}

/// Parses a tool's hold action argument into a [`HoldAction`].
///
/// Validated before the blocking task is spawned so a bad argument is an
/// `INVALID_PARAMS` error rather than an internal one surfaced from the worker.
fn parse_hold_action(action: Option<&str>) -> Result<HoldAction, McpError> {
    match action.unwrap_or("hold").to_lowercase().as_str() {
        "hold" => Ok(HoldAction::Hold),
        "press" => Ok(HoldAction::Press),
        "release" => Ok(HoldAction::Release),
        other => Err(invalid_params(format!(
            "Invalid action: {}. Use hold/press/release.",
            other
        ))),
    }
}

/// Spawn-blocking result of a mouse move: `(target in native source pixels, cursor
/// position after the move, number of interpolation steps sent)`.
type MouseMoveResult = Result<((i32, i32), (i32, i32), usize), String>;

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
            if tokio::time::timeout_at(deadline, notified.as_mut())
                .await
                .is_err()
            {
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
