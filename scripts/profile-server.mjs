#!/usr/bin/env node
// MCP load harness for autorpg-mcp.
//
// The server is stdio-driven and does almost nothing while it waits for a tool
// call, so a profiler attached to an idle process samples nothing useful. This
// script spawns the server, performs the MCP handshake, and then drives a
// repeatable tool mix for a fixed wall-clock duration, printing per-scenario
// latency statistics (and optionally writing them as JSON).
//
// It is meant to be launched *under* a profiler, e.g. via scripts/profile.ps1:
//
//   samply record -- node scripts/profile-server.mjs --exe target/profiling/autorpg-mcp.exe
//
// Running it directly is also useful: it reports how long each tool takes and
// how much CPU the server burned, which is enough to compare builds without a
// profiler.
//
// Usage:
//   node scripts/profile-server.mjs [options]
//
//   --exe <path>       Server binary to run (required).
//   --cwd <path>       Working directory for the server; also where autorpg-mcp.toml
//                      is looked up (default: current directory).
//   --config <path>    Config file passed as argv[1] and AUTORPG_MCP_CONFIG.
//   --duration <s>     Seconds of tool traffic after warmup (default 20; 0 = until Ctrl+C).
//   --warmup <ms>      Settle time after initialize, before tool traffic (default 1500).
//   --tools <list>     Comma-separated scenario names, cycled round-robin (default: capture,metrics,mouse,key).
//   --timeout <ms>     Per-call timeout (default 20000).
//   --stats <path>     Write the summary as JSON to this path.
//   --verbose          Forward the server's stderr and log every call.
//   --help             Print this text.
//
// Scenarios: capture, capture_wait, metrics, zone, move, key, text, mouse, path,
//            click, scroll, hold, wait.

import { execFileSync, spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
import { resolve } from "node:path";
import process from "node:process";
import { setTimeout as sleep } from "node:timers/promises";

const SCENARIOS = {
  // Forced capture: the full telemetry + JPEG encode + base64 path.
  capture: { tool: "capture_screen", args: { force: true } },
  // Non-forced capture: exercises wait_for_screen_change (blocks up to the
  // configured timeout whenever the screen is static).
  capture_wait: { tool: "capture_screen", args: {} },
  metrics: { tool: "get_game_metrics", args: {} },
  zone: { tool: "set_zone", args: { location: "Profiling Zone" } },
  move: { tool: "move_player", args: { direction: "forward", duration_ms: 50 } },
  key: { tool: "press_key", args: { key: "e", duration_ms: 20 } },
  text: { tool: "input_text", args: { text: "profile" } },
  mouse: { tool: "move_mouse", args: { x: 512, y: 288, human_like: false } },
  // WindMouse path: many sleeps plus many OS input events per call.
  path: { tool: "move_mouse", args: { x: 512, y: 288, human_like: true } },
  click: { tool: "click_mouse", args: { button: "left" } },
  scroll: { tool: "scroll_mouse", args: { amount: 1 } },
  hold: { tool: "hold_mouse", args: { button: "left", duration_ms: 20 } },
  wait: { tool: "wait", args: { duration_ms: 100 } },
};

const DEFAULT_TOOLS = "capture,metrics,mouse,key";

const HELP = `MCP load harness for autorpg-mcp (see the header of this file).

Options:
  --exe <path>     Server binary to run (required).
  --cwd <path>     Working directory for the server (default: current directory).
  --config <path>  Config file (passed as argv[1] and AUTORPG_MCP_CONFIG).
  --duration <s>   Seconds of tool traffic after warmup (default 20; 0 = until Ctrl+C).
  --warmup <ms>    Settle time after initialize (default 1500).
  --tools <list>   Comma-separated scenarios cycled round-robin (default ${DEFAULT_TOOLS}).
  --timeout <ms>   Per-call timeout (default 20000).
  --stats <path>   Write the summary as JSON.
  --verbose        Forward server stderr and log every call.
  --help           Print this text.

Scenarios: ${Object.keys(SCENARIOS).join(", ")}`;

/// Minimal newline-delimited JSON-RPC client for an MCP stdio server.
class McpClient {
  #child;
  #exit;
  #nextId = 1;
  #pending = new Map();
  #stdoutBuffer = "";
  #stderr = "";
  #timeoutMs;
  #verbose;

  constructor({ exe, cwd, config, timeoutMs, verbose }) {
    this.#timeoutMs = timeoutMs;
    this.#verbose = verbose;

    const env = { ...process.env };
    if (config) {
      env.AUTORPG_MCP_CONFIG = config;
    }

    this.#child = spawn(exe, config ? [config] : [], {
      cwd,
      env,
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
    });
    this.pid = this.#child.pid;

    this.#exit = new Promise((settle) => {
      this.#child.once("exit", (code, signal) => settle({ code, signal }));
    });

    this.#child.on("error", (error) => this.#rejectAll(error));
    // The server may die mid-run (crash, or a killed capture thread); without
    // these handlers a write to a closed pipe would throw asynchronously.
    this.#child.stdin.on("error", (error) => this.#rejectAll(error));
    this.#child.stdout.setEncoding("utf8");
    this.#child.stdout.on("data", (chunk) => this.#onStdout(chunk));
    this.#child.stderr.setEncoding("utf8");
    this.#child.stderr.on("data", (chunk) => {
      this.#stderr += chunk;
      if (this.#verbose) {
        process.stderr.write(chunk);
      }
    });
  }

  /// Resolves once the child process has actually spawned.
  async waitForSpawn() {
    if (this.#child.exitCode !== null || this.#child.signalCode !== null) {
      throw new Error("server exited immediately after spawn");
    }
    await new Promise((settle, fail) => {
      this.#child.once("spawn", settle);
      this.#child.once("error", fail);
    });
    return this.pid;
  }

  async request(method, params) {
    const id = this.#nextId++;
    const payload = `${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`;
    return new Promise((settle, fail) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id);
        fail(new Error(`${method} timed out after ${this.#timeoutMs} ms`));
      }, this.#timeoutMs);
      this.#pending.set(id, { settle, fail, timer });
      this.#child.stdin.write(payload);
    });
  }

  notify(method, params) {
    this.#child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method, params })}\n`);
  }

  /// Performs the MCP handshake and returns the server's reported info.
  async initialize() {
    const result = await this.request("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "autorpg-mcp-load-harness", version: "1.0.0" },
    });
    this.notify("notifications/initialized", {});
    return result?.serverInfo ?? {};
  }

  async listTools() {
    const result = await this.request("tools/list", {});
    return (result?.tools ?? []).map((tool) => tool.name);
  }

  /// Calls a tool. Never throws on a tool-level error: those are counted by the
  /// caller (the game may not be running, or no frame may be published yet).
  async callTool(name, args) {
    const started = performance.now();
    let result;
    try {
      result = await this.request("tools/call", { name, arguments: args });
    } catch (error) {
      return { ok: false, ms: performance.now() - started, error: String(error.message ?? error) };
    }
    const ms = performance.now() - started;
    const content = result?.content ?? [];
    return {
      ok: result?.isError !== true,
      ms,
      hasImage: content.some((block) => block.type === "image"),
      text: content
        .filter((block) => block.type === "text")
        .map((block) => block.text)
        .join(" "),
    };
  }

  /// Closes stdin so the server shuts down, then force-kills it if it lingers.
  async close() {
    this.#child.stdin.end();
    const exited = await Promise.race([
      this.#exit.then(() => true),
      sleep(5000).then(() => false),
    ]);
    if (!exited) {
      this.#child.kill();
      await this.#exit;
    }
    return this.#stderr;
  }

  #onStdout(chunk) {
    this.#stdoutBuffer += chunk;
    let newline;
    while ((newline = this.#stdoutBuffer.indexOf("\n")) !== -1) {
      const line = this.#stdoutBuffer.slice(0, newline).trim();
      this.#stdoutBuffer = this.#stdoutBuffer.slice(newline + 1);
      if (!line) {
        continue;
      }
      let message;
      try {
        message = JSON.parse(line);
      } catch {
        if (this.#verbose) {
          process.stderr.write(`unparseable server line: ${line}\n`);
        }
        continue;
      }
      if (message.id === undefined) {
        continue;
      }
      const pending = this.#pending.get(message.id);
      if (!pending) {
        continue;
      }
      this.#pending.delete(message.id);
      clearTimeout(pending.timer);
      if (message.error) {
        pending.fail(new Error(`${message.error.message} (${message.error.code})`));
      } else {
        pending.settle(message.result);
      }
    }
  }

  #rejectAll(error) {
    for (const [, pending] of this.#pending) {
      clearTimeout(pending.timer);
      pending.fail(error);
    }
    this.#pending.clear();
  }
}

/// Reads the server's cumulative CPU seconds straight from Windows, since Node
/// cannot query another process's CPU accounting. Returns null when unavailable.
function processCpuSeconds(pid) {
  try {
    const output = execFileSync(
      "powershell",
      ["-NoProfile", "-Command", `(Get-Process -Id ${pid} -ErrorAction Stop).CPU`],
      { encoding: "utf8", windowsHide: true },
    ).trim();
    const seconds = Number.parseFloat(output);
    return Number.isFinite(seconds) ? seconds : null;
  } catch {
    return null;
  }
}

function percentile(sorted, fraction) {
  if (sorted.length === 0) {
    return 0;
  }
  const index = Math.min(sorted.length - 1, Math.floor(fraction * sorted.length));
  return sorted[index];
}

function parseArgs(argv) {
  const options = {
    duration: 20,
    warmup: 1500,
    tools: DEFAULT_TOOLS,
    timeout: 20000,
    cwd: process.cwd(),
    verbose: false,
  };
  for (let index = 0; index < argv.length; index++) {
    const argument = argv[index];
    const equals = argument.indexOf("=");
    const name = (equals === -1 ? argument : argument.slice(0, equals)).replace(/^--/, "");
    const inlineValue = equals === -1 ? undefined : argument.slice(equals + 1);
    const value = () => (inlineValue !== undefined ? inlineValue : argv[++index]);
    switch (name) {
      case "exe":
        options.exe = value();
        break;
      case "cwd":
        options.cwd = resolve(value());
        break;
      case "config":
        options.config = resolve(value());
        break;
      case "duration":
        options.duration = Number(value());
        break;
      case "warmup":
        options.warmup = Number(value());
        break;
      case "tools":
        options.tools = value();
        break;
      case "timeout":
        options.timeout = Number(value());
        break;
      case "stats":
        options.stats = resolve(value());
        break;
      case "verbose":
        options.verbose = true;
        break;
      case "help":
      case "h":
        options.help = true;
        break;
      default:
        throw new Error(`unknown argument: ${argument}`);
    }
  }
  return options;
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) {
    console.log(HELP);
    return;
  }
  if (!options.exe) {
    throw new Error(`--exe is required\n\n${HELP}`);
  }

  const scenarioNames = options.tools
    .split(",")
    .map((name) => name.trim())
    .filter(Boolean);
  for (const name of scenarioNames) {
    if (!SCENARIOS[name]) {
      throw new Error(
        `unknown scenario ${JSON.stringify(name)} (known: ${Object.keys(SCENARIOS).join(", ")})`,
      );
    }
  }

  const client = new McpClient({
    exe: options.exe,
    cwd: options.cwd,
    config: options.config,
    timeoutMs: options.timeout,
    verbose: options.verbose,
  });
  const pid = await client.waitForSpawn();
  console.log(`server pid ${pid}: ${options.exe}`);

  const info = await client.initialize();
  const tools = await client.listTools();
  console.log(`server ${info.name ?? "?"} ${info.version ?? ""}; tools: ${tools.join(", ")}`);

  // Let the capture thread publish a first frame before the load starts:
  // capture_screen fails with "No active display buffer detected yet" until then.
  await sleep(options.warmup);
  let primed = false;
  for (let attempt = 0; attempt < 25 && !primed; attempt++) {
    const probe = await client.callTool("capture_screen", { force: true });
    primed = probe.ok && probe.hasImage;
    if (!primed) {
      await sleep(200);
    }
  }
  console.log(
    primed
      ? `warmup: capture primed after ${options.warmup} ms settle`
      : "warmup: no frame published yet (is the capture target visible?)",
  );

  const cpuBefore = processCpuSeconds(pid);
  const startedAt = performance.now();
  const deadline = options.duration > 0 ? startedAt + options.duration * 1000 : Infinity;

  const stats = new Map(
    scenarioNames.map((name) => [name, { calls: 0, errors: 0, total: 0, max: 0, samples: [] }]),
  );

  let interrupted = false;
  const onInterrupt = () => {
    interrupted = true;
  };
  process.on("SIGINT", onInterrupt);

  let index = 0;
  let calls = 0;
  while (performance.now() < deadline && !interrupted) {
    const name = scenarioNames[index++ % scenarioNames.length];
    const scenario = SCENARIOS[name];
    const outcome = await client.callTool(scenario.tool, scenario.args);
    const entry = stats.get(name);
    entry.calls++;
    if (!outcome.ok) {
      entry.errors++;
    }
    entry.total += outcome.ms;
    entry.max = Math.max(entry.max, outcome.ms);
    entry.samples.push(outcome.ms);
    calls++;
    if (options.verbose) {
      const status = outcome.ok ? "ok" : `error: ${outcome.error ?? outcome.text}`;
      console.log(`${name} ${outcome.ms.toFixed(1)} ms ${status}`);
    }
  }

  const wallMs = performance.now() - startedAt;
  const cpuAfter = processCpuSeconds(pid);
  const summary = [];
  for (const [name, entry] of stats) {
    const sorted = [...entry.samples].sort((a, b) => a - b);
    summary.push({
      scenario: name,
      tool: SCENARIOS[name].tool,
      calls: entry.calls,
      errors: entry.errors,
      avg_ms: entry.calls ? entry.total / entry.calls : 0,
      p95_ms: percentile(sorted, 0.95),
      max_ms: entry.max,
    });
  }

  console.log("");
  console.log(
    `${"scenario".padEnd(14)}${"calls".padStart(6)}${"errors".padStart(8)}` +
      `${"avg ms".padStart(10)}${"p95 ms".padStart(10)}${"max ms".padStart(10)}`,
  );
  for (const row of summary) {
    console.log(
      `${row.scenario.padEnd(14)}${String(row.calls).padStart(6)}${String(row.errors).padStart(8)}` +
        `${row.avg_ms.toFixed(2).padStart(10)}${row.p95_ms.toFixed(2).padStart(10)}` +
        `${row.max_ms.toFixed(2).padStart(10)}`,
    );
  }

  const wallSeconds = wallMs / 1000;
  const cpuSeconds = cpuBefore !== null && cpuAfter !== null ? cpuAfter - cpuBefore : null;
  console.log("");
  console.log(
    `${calls} calls in ${wallSeconds.toFixed(2)} s (${(calls / wallSeconds).toFixed(2)} calls/s)` +
      (cpuSeconds !== null
        ? `; server CPU ${cpuSeconds.toFixed(2)} s (${((cpuSeconds / wallSeconds) * 100).toFixed(1)}% of one core)`
        : ""),
  );

  const stderr = await client.close();

  if (options.stats) {
    writeFileSync(
      options.stats,
      `${JSON.stringify(
        {
          exe: options.exe,
          pid,
          server: info,
          tools,
          scenarios: summary,
          tools_requested: scenarioNames,
          calls,
          wall_seconds: wallSeconds,
          cpu_seconds: cpuSeconds,
          cpu_percent_of_one_core: cpuSeconds !== null ? (cpuSeconds / wallSeconds) * 100 : null,
          interrupted,
          server_stderr: stderr,
        },
        null,
        2,
      )}\n`,
    );
    console.log(`stats written to ${options.stats}`);
  }
}

main().catch((error) => {
  console.error(`harness failed: ${error.message ?? error}`);
  process.exitCode = 1;
});
