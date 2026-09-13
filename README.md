# autorpg-mcp

An MCP (Model Context Protocol) server that exposes screen capture, input simulation and
game-state tracking tools so an LLM client can play an action RPG: read the screen, aim,
attack, dodge, drink potions and move between mobs.

The server is a stdio MCP process. It captures the desktop (or a single window) with the
Windows Graphics Capture API, publishes a downscaled JPEG preview on demand, parses live
telemetry (HP, stamina, ability cooldowns) out of the frame, and dispatches keyboard and
mouse input as physical scancodes so DirectInput/RawInput game engines receive it.

## Requirements

- Windows 10 1903 or newer (Windows Graphics Capture).
- Rust with edition 2024 support.
- A game running in the foreground, or any window to capture.

## Build

The capture thread runs a tight per-frame loop (staging map, downsample, pixel repack) on
every compositor update. An unoptimized build executes that loop roughly 50x slower and pins
a core, so the binary the MCP client launches must always be the release build.

```powershell
cargo build --release
```

The binary is written to `target\release\autorpg-mcp.exe`.

### Input backend

Input simulation has two interchangeable backends behind the `input-simulator` Cargo
feature. Both implement the `enigo` `Keyboard`/`Mouse` traits, so the tool code is identical.

```powershell
# Default: enigo (cross-platform input simulation)
cargo build --release

# Alternative: input-simulator (virtual-key space, USB HID usage table)
cargo build --release --features input-simulator
```

The enigo backend accepts every Set 1 scancode. The input-simulator backend works in
virtual-key space, so a scancode without a virtual-key equivalent is rejected as an invalid
argument instead of failing internally.

## Configuration

The server reads an optional `autorpg-mcp.toml` from the working directory. The path can be
overridden with the first CLI argument or the `AUTORPG_MCP_CONFIG` environment variable.
Every key is optional and falls back to the defaults shown in the checked-in file. Unknown
keys are rejected at startup so a typo fails loudly instead of being silently ignored.

See [`autorpg-mcp.toml`](autorpg-mcp.toml) for the annotated reference. The main sections:

| Section | Purpose |
| --- | --- |
| `[capture]` | Frame interval, JPEG quality, preview size, cursor, monitor vs window target |
| `[server]` | Hold limits, stale-frame hash distance, wait timeouts |
| `[game]` | Reference aspect ratio the game profile authors its UI layout against |
| `[prompts]` | Path to the prompt instructions template |

### Capture target

`capture.target` selects what is recorded:

- `"monitor"` (default): the whole primary monitor.
- `"window"`: a single window whose title matches `capture.window_name`. Exact titles win;
  otherwise the first window whose title contains the text (case-insensitively) is used.
  When no window matches, the error lists the visible window titles.

### Preview size

`capture.preview_edge` is the longest edge of the published preview. The captured frame is
scaled down to fit it (never upscaled). Lower values shrink every published frame's pixel
count, and with it the vision model's token cost, at the price of fine visual detail. The
game profile authors its UI layout against the same box.

## MCP client setup

Register the release binary as a stdio server. Example for a VS Code MCP configuration:

```json
{
  "servers": {
    "autorpg": {
      "type": "stdio",
      "command": "path_to_your\\autorpg-mcp.exe",
      "args": []
    }
  }
}
```

The server logs to stderr; stdout carries the JSON-RPC stream.

## Tools

| Tool | Description |
| --- | --- |
| `capture_screen` | Grabs a frame. Blocks until the screen visibly changes, or returns immediately with `force: true` for static screens (menus, dialogue, inventory). |
| `get_game_metrics` | Returns the current in-memory metrics snapshot as JSON. |
| `set_zone` | Updates the active zone/area name when entering a new region. |
| `move_player` | Holds a WASD movement key for `duration_ms` (layout-independent scancode). |
| `press_key` | Presses a single character or named key (`space`, `enter`, `escape`, `tab`, `f1`) as a physical scancode, optionally holding it. |
| `input_text` | Types text into the focused input field using Unicode text events. Game engines ignore these; use `press_key` for gameplay input. |
| `move_mouse` | Moves to absolute image-space coordinates (auto-scaled to native display pixels) or by a raw relative delta. Absolute moves follow a human-like WindMouse path by default. |
| `click_mouse` | Clicks left/right/middle, optionally double. |
| `hold_mouse` | Press-and-hold, press or release a button. `press` + `move_mouse` + `release` performs a drag or camera orbit. |
| `scroll_mouse` | Scrolls the wheel; positive up, negative down. |
| `wait` | Sleeps for a fixed delay (loading screens, respawns, teleports, potion animations). |

### Telemetry

`capture_screen` returns server-parsed telemetry with every frame:

```
[HP: X% | Stamina: Y% | Q: READY/COOLDOWN | R: ... | F: ... | G: ... | Combat: IN/OUT | Zone: Name]
```

Combat stats, weapon cooldowns and the crossed-swords combat flag are maintained
automatically. If a bar cannot be measured,
HP or Stamina is reported as `?` instead of a number, so a failed scan never reads as full
health.

### Coordinate mapping

Absolute mouse coordinates are in the captured frame's image space (the captured source
scaled to `preview_edge` on its longest edge). They are scaled up by the source's own pixel
dimensions, read out of the latest published frame, so a position picked off the image lands
on the same physical spot. Using the source rather than the display keeps the mapping correct
for window capture. The process runs with per-monitor-v2 DPI awareness, so source pixels are
physical pixels.

## Prompts

| Prompt | Description |
| --- | --- |
| `start_farm` | Starts an automated mob-farming loop that moves to nearby mobs, attacks them, and uses abilities/potions until the target duration elapses. |

Arguments: `duration_minutes` (default 10), `target` (default "any nearby mob"),
`potion_threshold` (default 30).

The prompt body is read from `prompts.instructions_path` (default
[`ai_instructions.md`](ai_instructions.md)) relative to the configuration file's directory.
`{target}`, `{duration}` and `{potion_threshold}` are substituted per call, and the file is
re-read on every call, so edits apply without a restart.

## Architecture

```
src/
  main.rs        Startup: DPI awareness, config load, capture thread, MCP stdio service
  config.rs      TOML configuration with defaults and sanitization
  capture.rs     Windows Graphics Capture handler: downsample, hash, publish
  state.rs       Shared session state, frame payload, JPEG encode, RGB view
  server.rs      MCP tools, prompts and argument schemas
  input.rs       Input backend abstraction (enigo / input-simulator) and scancode maps
  windmouse.rs   Human-like cursor path interpolation
  games/         Game profiles: telemetry parsing per game
  error.rs       MCP error helpers
```

The capture thread publishes the latest preview into a shared buffer and signals a
`Notify` after every frame. JPEG encoding is deferred to the consumer, so the capture thread
never pays for it while no client is querying frames. `capture_screen` parks on the
notification instead of polling, and telemetry reads straight out of the shared buffer
without copying the frame.

## Profiling

See [`scripts/README.md`](scripts/README.md). Two scripts drive a realistic tool workload
while samples are taken:

```powershell
# Default: 20 s of capture, metrics, mouse and key traffic under samply.
.\scripts\profile.ps1

# Focus on the capture path (JPEG encode, base64, telemetry parsing).
.\scripts\profile.ps1 -Tools capture,capture_wait -Duration 30

# No profiler: just latency + CPU per scenario.
.\scripts\profile.ps1 -NoSamply -Stats target\harness.json
```

The `profiling` Cargo profile inherits release semantics with `debug = 2` and thin LTO, so
samply can resolve Rust frames to functions and lines.

## Notes

- Stop a running server before rebuilding: it locks `target\...\autorpg-mcp.exe` and the
  link step fails with "Access is denied (os error 5)".
- The server targets the real desktop: it moves the mouse and presses keys. Keep the game in
  the foreground.