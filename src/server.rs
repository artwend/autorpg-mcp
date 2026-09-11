//! MCP server: application context, tool argument schemas and tool implementations.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use enigo::{Axis, Button, Coordinate, Direction, Key, Keyboard, Mouse};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, PromptMessage, Role, ServerCapabilities, ServerInfo},
    schemars,
    prompt, prompt_handler, prompt_router, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use base64::Engine;

use crate::error::{input_error, internal_error, invalid_params};
use crate::input::{direction_scancode, ops};
use crate::state::{SharedFrameBuffer, SharedSession};

/// Application context shared by all MCP tools.
#[derive(Clone)]
pub struct GameServer<Input: Keyboard + Mouse + Send + 'static> {
    session: SharedSession,
    frame_buffer: SharedFrameBuffer,
    input: Arc<Mutex<Input>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct UpdateMetricsArgs {
    /// Current player HP evaluation
    hp: i32,
    /// Current zone location identifier
    location: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DumpSessionArgs {
    /// Named prefix string for target JSON log tracking file
    prefix: Option<String>,
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
    /// The key to press, as a single character (e.g. "e", "1", " ")
    key: String,
    /// How long to hold the key in milliseconds (default 50, max 10000)
    duration_ms: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct MoveMouseArgs {
    /// Target X coordinate in pixels
    x: i32,
    /// Target Y coordinate in pixels
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
    /// Scroll amount in wheel clicks; positive scrolls down, negative scrolls up
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

/// Maximum duration for hold-style input operations.
const MAX_HOLD_MS: u64 = 10_000;

#[tool_router]
impl<Input: Keyboard + Mouse + Send + 'static> GameServer<Input> {
    pub fn new(session: SharedSession, frame_buffer: SharedFrameBuffer, input: Input) -> Self {
        Self {
            session,
            frame_buffer,
            input: Arc::new(Mutex::new(input)),
        }
    }

    /// Grabs a highly optimized frame of the primary monitor.
    #[tool(description = "Grabs a highly optimized frame of the primary monitor.")]
    async fn capture_screen(&self) -> Result<CallToolResult, McpError> {
        let guard = match self.frame_buffer.lock() {
            Ok(lock) => lock,
            Err(poisoned) => poisoned.into_inner(),
        };

        if let Some(bytes) = guard.as_ref() {
            let img_base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
            Ok(CallToolResult::success(vec![
                ContentBlock::image(img_base64, "image/png")
            ]))
        } else {
            Ok(CallToolResult::error(vec![ContentBlock::text(
                "No active display buffer detected yet. Try again.",
            )]))
        }
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

    /// Moves the player by holding a WASD movement key for the given duration.
    #[tool(description = "Moves the player by holding a WASD movement key (layout-independent scancode).")]
    async fn move_player(
        &self,
        Parameters(MovePlayerArgs { direction, duration_ms }): Parameters<MovePlayerArgs>,
    ) -> Result<CallToolResult, McpError> {
        let scancode = direction_scancode(&direction).ok_or_else(|| {
            invalid_params(format!(
                "Invalid direction mapping: {}. Use forward/back/left/right.",
                direction
            ))
        })?;
        let duration = Duration::from_millis(duration_ms.unwrap_or(500).min(MAX_HOLD_MS));

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
    #[tool(description = "Presses a keyboard key given as a single character, optionally holding it.")]
    async fn press_key(
        &self,
        Parameters(PressKeyArgs { key, duration_ms }): Parameters<PressKeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut chars = key.chars();
        let ch = chars.next().filter(|_| chars.next().is_none()).ok_or_else(|| {
            invalid_params(format!(
                "Invalid key: {:?}. Must be a single character.",
                key
            ))
        })?;
        let duration = Duration::from_millis(duration_ms.unwrap_or(50).min(MAX_HOLD_MS));

        let input = Arc::clone(&self.input);
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut simulator = input.lock().map_err(|e| e.to_string())?;
            ops::hold_key(&mut *simulator, Key::Unicode(ch), duration).map_err(|e| e.to_string())
        })
        .await
        .map_err(input_error)?
        .map_err(input_error)?;

        let message = format!("Key '{}' pressed for {} ms.", ch, duration.as_millis());
        self.session.write().await.record_event(message.clone());

        Ok(CallToolResult::success(vec![ContentBlock::text(message)]))
    }

    /// Moves the mouse cursor to absolute (or relative) screen coordinates.
    #[tool(description = "Moves the mouse cursor to absolute screen coordinates, or relative to its current position.")]
    async fn move_mouse(
        &self,
        Parameters(MoveMouseArgs { x, y, relative }): Parameters<MoveMouseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let coordinate = if relative.unwrap_or(false) {
            Coordinate::Rel
        } else {
            Coordinate::Abs
        };

        let input = Arc::clone(&self.input);
        let (x, y) = tokio::task::spawn_blocking(move || -> Result<(i32, i32), String> {
            let mut simulator = input.lock().map_err(|e| e.to_string())?;
            simulator.move_mouse(x, y, coordinate).map_err(|e| e.to_string())?;
            simulator.location().map_err(|e| e.to_string())
        })
        .await
        .map_err(input_error)?
        .map_err(input_error)?;

        let message = format!("Mouse moved to ({}, {}).", x, y);
        self.session.write().await.record_event(message.clone());

        Ok(CallToolResult::success(vec![ContentBlock::text(message)]))
    }

    /// Clicks a mouse button (left/right/middle, optional double click).
    #[tool(description = "Clicks a mouse button: left (default), right or middle; supports double click.")]
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
                )))
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
    #[tool(description = "Scrolls the mouse wheel; positive amounts scroll down, negative scroll up.")]
    async fn scroll_mouse(
        &self,
        Parameters(ScrollMouseArgs { amount }): Parameters<ScrollMouseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let input = Arc::clone(&self.input);
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut simulator = input.lock().map_err(|e| e.to_string())?;
            simulator
                .scroll(amount, Axis::Vertical)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(input_error)?
        .map_err(input_error)?;

        let message = format!(
            "Scrolled {} clicks {}.",
            amount.abs(),
            if amount >= 0 { "down" } else { "up" }
        );
        self.session.write().await.record_event(message.clone());

        Ok(CallToolResult::success(vec![ContentBlock::text(message)]))
    }
}

#[prompt_router]
impl<Input: Keyboard + Mouse + Send + 'static> GameServer<Input> {
    /// Starts an automated mob-farming loop.
    #[prompt(description = "Starts an automated mob-farming loop that moves to nearby mobs, attacks them, and uses abilities/potions until the target duration elapses.")]
    async fn start_farm(
        &self,
        Parameters(args): Parameters<StartFarmArgs>,
    ) -> Result<Vec<PromptMessage>, McpError> {
        let duration = args.duration_minutes.unwrap_or(10);
        let target = args.target.unwrap_or_else(|| "any nearby mob".to_string());
        let potion_threshold = args.potion_threshold.unwrap_or(30).clamp(0, 100);

        let instructions = format!(
            r#"# AUTOMATED MOB FARMING SESSION

You are now running an automated mob-farming loop for the ACTION RPG game.

## Objective
Farm {target} continuously for {duration} minute(s).

## Loop
1. Call `capture_screen` to grab the current frame and assess the battlefield.
2. Call `update_game_metrics` with the observed HP and zone location.
3. If HP <= {potion_threshold}%, press '1' to drink a potion.
4. If a mob is in range, attack with a left mouse click (`click_mouse`).
5. If no mob is in range, move toward the nearest mob using `move_player` (forward/back/left/right).
6. Use weapon abilities when ready: press 'Q', 'R', or 'F' (`press_key`) if the server telemetry flags them as READY.
7. Repeat steps 1-6 until {duration} minute(s) have elapsed.

## Rules
- Never let HP drop below {potion_threshold}% without drinking a potion.
- Keep moving between kills to find the next target.
- Stop immediately if HP reaches 0 or the session is interrupted.
- Report a summary of kills, potions used, and final HP when done."#
        );

        Ok(vec![PromptMessage::new_text(Role::User, instructions)])
    }
}

#[tool_handler]
#[prompt_handler]
impl<Input: Keyboard + Mouse + Send + 'static> ServerHandler for GameServer<Input> {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().enable_prompts().build())
            .with_server_info(rmcp::model::Implementation::from_build_env())
    }
}
