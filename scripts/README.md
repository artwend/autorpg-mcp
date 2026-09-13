# Profiling autorpg-mcp with samply

The server is a stdio MCP process: without a client it sits in `service.waiting()` and a
profiler samples nothing but the idle capture thread. Profiling therefore means driving a
realistic tool workload while samples are being taken.

Two scripts do that:

- `scripts/profile-server.mjs` - MCP client + load harness. Spawns the server, performs the
  handshake, waits for the first frame, then cycles a scenario mix for a fixed duration and
  prints per-scenario latency plus the server's CPU time (read from Windows via
  `Get-Process ... .CPU`).
- `scripts/profile.ps1` - builds a profile-friendly binary, checks for samply/xperf, and runs
  the harness under `samply record`.

## Install samply

```powershell
cargo install --locked samply
```

On Windows samply records ETW through `xperf`, which is not installed by default. Install the
Windows ADK and select only **Windows Performance Toolkit**:
<https://learn.microsoft.com/en-us/windows-hardware/test/wpt/>

Until then `profile.ps1` warns and falls back to running the harness without a profiler
(latency and CPU numbers are still produced). `-RequireSamply` turns the warning into an error.

## Run it

```powershell
# Default: 20 s of capture, metrics, mouse and key traffic under samply.
.\scripts\profile.ps1

# Focus on the capture path (JPEG encode, base64, telemetry parsing).
.\scripts\profile.ps1 -Tools capture,capture_wait -Duration 30

# Compare the shipping binary (release: fat LTO, no debug info).
.\scripts\profile.ps1 -Release -Duration 20

# No profiler: just latency + CPU per scenario.
.\scripts\profile.ps1 -NoSamply -Stats target\harness.json
```

samply serves the recorded profile at `http://127.0.0.1:3000+` and opens the browser when the
run finishes. The profile is written to `target\profile.json.gz`.

For long runs with lots of threads, `--main-thread-only` and `--reuse-threads` are worth adding
to the `samply record` invocation (edit `$samplyArgs` or call samply directly).

## Scenarios

Pass any subset via `-Tools`, cycled round-robin:

| Scenario | Tool | What it stresses |
| --- | --- | --- |
| `capture` | `capture_screen` (force) | telemetry parse, JPEG encode, base64 |
| `capture_wait` | `capture_screen` | `wait_for_screen_change` notification path |
| `metrics` | `get_game_metrics` | session lock + serde |
| `zone` | `set_zone` | session write + event history |
| `move` | `move_player` | spawn_blocking + scancode dispatch |
| `key` | `press_key` | scancode dispatch |
| `text` | `input_text` | Unicode text events |
| `mouse` | `move_mouse` (instant) | coordinate scaling + one OS event |
| `path` | `move_mouse` (WindMouse) | many events + per-step sleeps |
| `click`, `scroll`, `hold` | mouse tools | button/scroll dispatch |
| `wait` | `wait` | async timer |

## Profiles

- `profiling` (default): release semantics with `debug = 2` and thin LTO - symbolicated frames
  with line numbers, at a small cost in inlining versus `release`.
- `release`: the shipping binary (fat LTO, no debug info). Use it when the absolute CPU number
  matters more than readable frames.

Raw CPU cost per scenario is measured by the harness itself, which is the reliable number for
comparing the `input-simulator` feature against the enigo backend, or before/after a change.

## Notes

- Stop a running server before profiling: it locks `target\...\autorpg-mcp.exe` and the link
  step fails with "Access is denied (os error 5)". `profile.ps1` does this for you.
- A tool-level error is counted, not fatal. Without a running game some tools legitimately fail
  (no frame yet, wrong window focus), so check the `errors` column before trusting a number.
- The harness targets the real desktop: it moves the mouse and presses keys. Keep the game in
  the foreground, or restrict the mix to read-only scenarios (`capture`, `capture_wait`,
  `metrics`).
