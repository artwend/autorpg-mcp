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
use crate::state::{SharedFrameBuffer, SharedFrameNotify, SharedHeldButtons, SharedSession};
use crate::windmouse;

/// Deserializes a number from either a JSON number or a numeric string.
///
/// MCP prompt arguments are transmitted as strings (the protocol's `PromptArgument`
/// carries no type), and some clients stringify tool arguments too, so a bare
/// `u32`/`i32`/`u64` field would otherwise reject `"10"` with
/// `invalid type: string "10", expected u32`. This accepts both forms and also works
/// for `Option<T>` fields (a JSON `null` or a missing field yields `None`).
fn deserialize_number<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    use serde::de::Error as _;

    let value = serde_json::Value::deserialize(deserializer)?;
    let coerced = match value {
        serde_json::Value::String(text) => {
            let text = text.trim();
            if let Ok(number) = text.parse::<i64>() {
                serde_json::Value::from(number)
            } else if let Ok(number) = text.parse::<u64>() {
                serde_json::Value::from(number)
            } else if let Ok(number) = text.parse::<f64>() {
                serde_json::Number::from_f64(number)
                    .map(serde_json::Value::Number)
                    .ok_or_else(|| D::Error::custom(format!("invalid number: {text:?}")))?
            } else {
                return Err(D::Error::custom(format!("invalid number: {text:?}")));
            }
        }
        other => other,
    };
    serde_json::from_value(coerced).map_err(D::Error::custom)
}

/// Deserializes a boolean from either a JSON boolean or a string ("true"/"false").
///
/// Same rationale as [`deserialize_number`]: prompt arguments arrive as strings.
fn deserialize_bool<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    use serde::de::Error as _;

    let value = serde_json::Value::deserialize(deserializer)?;
    let coerced = match value {
        serde_json::Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => serde_json::Value::Bool(true),
            "false" | "0" | "no" | "off" => serde_json::Value::Bool(false),
            _ => return Err(D::Error::custom(format!("invalid boolean: {text:?}"))),
        },
        other => other,
    };
    serde_json::from_value(coerced).map_err(D::Error::custom)
}

/// Application context shared by all MCP tools.
pub struct GameServer<Input: Keyboard + Mouse + Send + 'static> {
    session: SharedSession,
    frame_buffer: SharedFrameBuffer,
    /// Signalled by the capture thread after every published frame; lets the
    /// wait-for-change loop sleep until a frame actually lands instead of polling.
    frame_notify: SharedFrameNotify,
    input: Arc<Mutex<Input>>,
    /// Mouse buttons currently held down via `hold_mouse`.
    held_buttons: SharedHeldButtons,
    /// Tool limits loaded from the configuration file.
    limits: ServerConfig,
    /// Quality of the on-demand JPEG encode, from the capture configuration.
    jpeg_quality: u8,
    /// Speed multiplier for human-like mouse moves, from the configuration file.
    mouse_speed: f64,
    /// Base pause between cursor updates in a human-like mouse move.
    mouse_step_interval: Duration,
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
    #[serde(default, deserialize_with = "deserialize_number")]
    duration_ms: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PressKeyArgs {
    /// The key to press: a single character (e.g. "e", "1", " ") or a named key (e.g. "space", "enter", "escape", "tab", "f1")
    key: String,
    /// How long to hold the key in milliseconds (default 50, max 10000)
    #[serde(default, deserialize_with = "deserialize_number")]
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
    #[serde(deserialize_with = "deserialize_number")]
    x: i32,
    /// Target Y coordinate in captured-frame (image) pixels; scaled to native display pixels.
    /// Ignored for relative moves (raw delta in image pixels).
    #[serde(deserialize_with = "deserialize_number")]
    y: i32,
    /// If true, the coordinates are a raw delta applied to the current cursor position
    /// (no scaling; for camera look, aiming and other mickey-based camera control)
    #[serde(default, deserialize_with = "deserialize_bool")]
    relative: Option<bool>,
    /// If true (the default), a move travels along a human-like WindMouse path
    /// (curved, accelerated and settled) instead of jumping straight to the
    /// target. Applies to both absolute and relative moves. Set to false for a
    /// single instantaneous move.
    #[serde(default, deserialize_with = "deserialize_bool")]
    human_like: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ClickMouseArgs {
    /// Mouse button to click: "left" (default), "right" or "middle"
    button: Option<String>,
    /// If true, perform a double click
    #[serde(default, deserialize_with = "deserialize_bool")]
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
    #[serde(default, deserialize_with = "deserialize_number")]
    duration_ms: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ScrollMouseArgs {
    /// Scroll amount in wheel clicks; positive scrolls up, negative scrolls down
    #[serde(deserialize_with = "deserialize_number")]
    amount: i32,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WaitArgs {
    /// How long to wait in milliseconds (default 500, max from the server's max_wait_ms limit)
    #[serde(default, deserialize_with = "deserialize_number")]
    duration_ms: Option<u64>,
    /// Optional note about what this delay is for (e.g. "loading screen", "death respawn"); recorded in the session log
    reason: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CaptureScreenArgs {
    /// When true, skip the wait-for-change window and return the latest frame immediately,
    /// even if the screen has not visibly changed. Use this to inspect static screens
    /// (menus, dialogue, inventory) that would otherwise time out without an image.
    #[serde(default, deserialize_with = "deserialize_bool")]
    force: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct StartFarmArgs {
    /// How long to farm in minutes (default 10)
    #[serde(default, deserialize_with = "deserialize_number")]
    duration_minutes: Option<u32>,
    /// Mob type to focus on (e.g. "boar", "wolf"); empty means any nearby mob
    target: Option<String>,
    /// HP percentage at which to drink a potion (default 30)
    #[serde(default, deserialize_with = "deserialize_number")]
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
        held_buttons: SharedHeldButtons,
        limits: ServerConfig,
        jpeg_quality: u8,
        mouse_speed: f64,
        mouse_step_interval: Duration,
        instructions_path: std::path::PathBuf,
    ) -> Self {
        Self {
            session,
            frame_buffer,
            frame_notify,
            input: Arc::new(Mutex::new(input)),
            held_buttons,
            limits,
            jpeg_quality,
            mouse_speed,
            mouse_step_interval,
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

        // Take a shared handle to the latest frame and release the frame mutex immediately:
        // telemetry parsing and the on-demand JPEG encode below are per-call image work and
        // must not block the capture thread's next publish behind them. The read is
        // async-friendly so a contended lock never parks the executor thread.
        let Some(frame) = self.frame_buffer.latest_async().await else {
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

        // The JPEG is encoded on demand (the capture thread no longer pre-encodes). A
        // timed-out (static) capture returns no image, so skip the encode entirely
        // instead of paying for it only to discard the result.
        let jpeg =
            if timed_out {
                None
            } else {
                Some(frame.encode_jpeg(self.jpeg_quality).map_err(|error| {
                    internal_error(format!("failed to encode preview: {error}"))
                })?)
            };
        drop(session);

        let combat_text = if metrics.in_combat { "IN" } else { "OUT" };
        let telemetry_text = format!(
            "[HP: {} | Stamina: {} | Q: {} | R: {} | F: {} | G: {} | Combat: {} | Zone: {}]",
            Self::format_percent(metrics.player_hp),
            Self::format_percent(metrics.stamina),
            Self::format_ability(metrics.q_ready),
            Self::format_ability(metrics.r_ready),
            Self::format_ability(metrics.f_ready),
            Self::format_ability(metrics.g_ready),
            combat_text,
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
    /// mouse instead of a cursor landing on the target in one event. Relative moves
    /// (camera look, drag-orbit) are interpolated the same way: the delta is simulated
    /// as a curved path of raw mickey events, so a look sweeps like a person turning
    /// the camera instead of snapping. Pass `human_like: false` for a single
    /// instantaneous event in either mode.
    #[tool(
        description = "Moves the mouse cursor to absolute image-space coordinates (auto-scaled to native display pixels), or by a delta relative to its current position. Both modes follow a human-like WindMouse path by default; pass human_like: false for an instant move."
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
            // blocking thread. Only the dimensions are read, so the lock is released
            // before the move is dispatched. The read is async-friendly so a contended
            // lock never parks the executor thread.
            let frame = self.frame_buffer.latest_async().await.ok_or_else(|| {
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
        let mouse_speed = self.mouse_speed;
        let mouse_step_interval = self.mouse_step_interval;
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

                let steps = if human_like {
                    // Both modes are interpolated: absolute moves walk a curved path of
                    // absolute events, relative moves a curved path of raw mickey events
                    // (each step is a delta, so the simulation runs from the origin).
                    // Speed and step cadence come from the configuration file.
                    if relative {
                        windmouse::move_by(
                            &mut *simulator,
                            windmouse::Point::new(target_x, target_y),
                            mouse_speed,
                            mouse_step_interval,
                        )
                        .map_err(|e| e.to_string())?
                    } else {
                        windmouse::move_to(
                            &mut *simulator,
                            windmouse::Point::new(target_x, target_y),
                            mouse_speed,
                            mouse_step_interval,
                        )
                        .map_err(|e| e.to_string())?
                    }
                } else {
                    let mode = if relative {
                        Coordinate::Rel
                    } else {
                        Coordinate::Abs
                    };
                    simulator
                        .move_mouse(target_x, target_y, mode)
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
            if human_like {
                format!(
                    "Mouse moved along a {steps}-step WindMouse path by relative delta ({}, {}); \
                     now at cursor ({}, {}).",
                    x, y, final_position.0, final_position.1
                )
            } else {
                format!(
                    "Mouse moved by relative delta ({}, {}); now at cursor ({}, {}).",
                    x, y, final_position.0, final_position.1
                )
            }
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

        // The gap between the two clicks is waited on asynchronously, outside the input
        // mutex, so a double click does not block other input tools for the duration.
        self.click_once(button).await?;
        if double {
            tokio::time::sleep(DOUBLE_CLICK_GAP).await;
            self.click_once(button).await?;
        }

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
        // Only "hold" consumes a duration: it presses, waits, then releases. A bare press
        // or release is a single event, so `duration_ms` is ignored for those instead of
        // being clamped and carried through unused.
        let hold_duration = match action {
            HoldAction::Hold => Some(Duration::from_millis(
                duration_ms.unwrap_or(50).min(self.limits.max_hold_ms),
            )),
            HoldAction::Press | HoldAction::Release => None,
        };

        // Track what `press`/`hold` leaves down so a double press can be rejected and a
        // button still held when the session ends can be released.
        match action {
            HoldAction::Press => {
                if !self.held_buttons.press(button) {
                    return Err(invalid_params(format!(
                        "The {button:?} mouse button is already held down; release it before pressing again."
                    )));
                }
            }
            HoldAction::Release | HoldAction::Hold => {
                // `hold` releases itself below, so it must not leave itself marked as held
                // on failure; both are simple clears when nothing was recorded.
                self.held_buttons.release(button);
            }
        }

        let input = Arc::clone(&self.input);
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut simulator = input.lock().map_err(|e| e.to_string())?;
            match action {
                HoldAction::Hold => {
                    let duration = hold_duration.ok_or("hold action requires a duration")?;
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
        .map_err(input_error)
        .and_then(|result| result.map_err(input_error))
        .inspect_err(|_| {
            // A failed press must not leave the button marked as held, or the matching
            // release would be rejected as "not held" later.
            if action == HoldAction::Press {
                self.held_buttons.release(button);
            }
        })?;

        let message = match (action, hold_duration) {
            (HoldAction::Hold, Some(duration)) => format!(
                "Held {:?} mouse button for {} ms, then released.",
                button,
                duration.as_millis()
            ),
            (HoldAction::Press, _) => format!(
                "Mouse button {button:?} pressed (held down). Release it with hold_mouse action 'release'."
            ),
            (HoldAction::Release, _) => format!("Mouse button {button:?} released."),
            // `Hold` always carries a duration by construction above.
            (HoldAction::Hold, None) => unreachable!(),
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

        // `unsigned_abs` instead of `abs`, which panics on `i32::MIN` (the negation
        // overflows). Zero is neither up nor down, so it gets its own wording rather than
        // being reported as an upward scroll.
        let message = format!(
            "Scrolled {} clicks {}.",
            amount.unsigned_abs(),
            if amount == 0 {
                "(no movement)"
            } else if amount > 0 {
                "up"
            } else {
                "down"
            }
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

/// Gap inserted between the two clicks of a double click.
const DOUBLE_CLICK_GAP: Duration = Duration::from_millis(50);

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
    /// Dispatches a single press-and-release of `button`.
    ///
    /// Split out of `click_mouse` so the double-click gap is awaited in the async handler
    /// rather than slept through inside the blocking task, which would hold the input
    /// mutex for the whole gap.
    async fn click_once(&self, button: Button) -> Result<(), McpError> {
        let input = Arc::clone(&self.input);
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut simulator = input.lock().map_err(|e| e.to_string())?;
            simulator
                .button(button, Direction::Click)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(input_error)?
        .map_err(input_error)
    }
    /// Releases every mouse button `hold_mouse` left held down.
    ///
    /// Called once the MCP client disconnects, and again from [`Drop`] as a backstop, so a
    /// session that ends between a `press` and its matching `release` never leaves a
    /// button stuck down in the OS input state. Idempotent: the held set is drained before
    /// the release events are sent, so a second call is a no-op.
    pub fn release_held_buttons(&self) {
        let held = self.held_buttons.take_all();
        if held.is_empty() {
            return;
        }

        let mut simulator = match self.input.lock() {
            Ok(lock) => lock,
            Err(poisoned) => poisoned.into_inner(),
        };
        for button in held {
            match simulator.button(button, Direction::Release) {
                Ok(()) => log::info!("released {button:?} mouse button still held at shutdown"),
                Err(error) => {
                    log::warn!("failed to release {button:?} mouse button at shutdown: {error}")
                }
            }
        }
    }
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
                match self.frame_buffer.latest_async().await {
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
            // A closed capture session will never publish again: `on_closed` cleared the
            // buffer and woke us, and no future frame can make this loop succeed. Bail out
            // now instead of sleeping out the remaining timeout for nothing; the caller
            // then reports the missing buffer.
            if self.frame_buffer.is_closed() {
                return Ok(true);
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

/// Backstop release of any mouse button the session left held.
///
/// The normal path calls [`GameServer::release_held_buttons`] after the client disconnects;
/// this covers teardown that never reaches it (a panicking task, an early `?` return), so
/// the desktop is never left with a button pressed. The held set is shared and drained by
/// the release, so running twice releases nothing the second time.
impl<Input: Keyboard + Mouse + Send + 'static> Drop for GameServer<Input> {
    fn drop(&mut self) {
        self.release_held_buttons();
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
