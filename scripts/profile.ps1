<#
.SYNOPSIS
    Profiles autorpg-mcp with samply (or without, if samply is missing).

.DESCRIPTION
    Builds the profiling binary, then runs the MCP load harness
    (scripts/profile-server.mjs) either under `samply record` or bare. The
    harness drives the server's tools for a fixed duration, because a stdio MCP
    server samples as idle otherwise.

    The `profiling` profile (release semantics, thin LTO, full debug info) is
    used by default so samply can resolve Rust frames to functions and lines;
    pass -Release to profile the shipping binary instead (fat LTO, no debug
    info: accurate CPU cost, poor symbol names).

.PARAMETER Duration
    Seconds of tool traffic after warmup (0 = until Ctrl+C).

.PARAMETER Tools
    Comma-separated scenarios cycled round-robin. Known scenarios:
    capture, capture_wait, metrics, zone, move, key, text, mouse, path, click,
    scroll, hold, wait.

.PARAMETER Release
    Profile the release binary instead of the profiling binary.

.PARAMETER Feature
    Also enable the `input-simulator` Cargo feature.

.PARAMETER NoSamply
    Skip samply and only run the harness (reports latency + CPU time).

.PARAMETER RequireSamply
    Fail instead of falling back when samply (or the xperf it needs on Windows)
    is unavailable.

.PARAMETER Serve
    Keep samply's local web UI alive after recording (blocks until Ctrl+C).
    Off by default: the profile is written to disk and can be opened later with
    `samply load <Output>`.

.PARAMETER Stats
    Path for the JSON summary written by the harness.

.EXAMPLE
    .\scripts\profile.ps1 -Duration 30

.EXAMPLE
    .\scripts\profile.ps1 -Tools capture,capture_wait,mouse,path -Duration 15

.EXAMPLE
    .\scripts\profile.ps1 -NoSamply -Release -Duration 20

.EXAMPLE
    .\scripts\profile.ps1 -Serve -Duration 30
    # then: samply load target\profile.json.gz
#>
param(
    [int]$Duration = 20,
    # Scenario list. Accepts both -Tools capture,metrics (PowerShell binds this as
    # an array) and -Tools "capture,metrics".
    [string[]]$Tools = @("capture", "metrics", "mouse", "key"),
    [int]$Warmup = 1500,
    [switch]$Release,
    [switch]$Feature,
    [switch]$NoSamply,
    [switch]$RequireSamply,
    # Keep samply's local web UI alive after recording (it then blocks until
    # Ctrl+C). Off by default: the profile is written to disk and can be opened
    # at any time with `samply load <Output>`.
    [switch]$Serve,
    [string]$Stats = "",
    # Inside target/ so profile output stays out of the repository root.
    [string]$Output = "target\profile.json.gz",
    [switch]$Verbose
)

$ErrorActionPreference = "Stop"

# Cargo, samply and node write their progress and warnings to stderr, and
# PowerShell 5.1 turns a native command's stderr into a NativeCommandError
# (fatal under $ErrorActionPreference = "Stop") even when the command succeeds.
# The preference is therefore relaxed for the child processes and success is
# judged from their exit codes instead; cmdlet-level failures are either guarded
# with -ErrorAction SilentlyContinue or checked explicitly.
$ErrorActionPreference = "Continue"

if (-not $env:CARGO_HOME) {
    $env:CARGO_HOME = "D:\rust\cargo"
}

$repo = Split-Path -Parent $PSScriptRoot
Push-Location $repo
try {
    # A running server locks the output binary and fails the link step.
    Get-Process -Name autorpg-mcp -ErrorAction SilentlyContinue | Stop-Process -Force

    $profileName = if ($Release) { "release" } else { "profiling" }
    $exe = Join-Path $repo "target\$profileName\autorpg-mcp.exe"

    $cargoArgs = @("build", "--profile", $profileName)
    if ($Feature) {
        $cargoArgs += @("--features", "input-simulator")
    }
    Write-Host "building: cargo $($cargoArgs -join ' ')" -ForegroundColor Cyan
    & cargo @cargoArgs 2>&1 | ForEach-Object { Write-Host $_ }
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed (exit code $LASTEXITCODE)"
    }
    if (-not (Test-Path $exe)) {
        throw "build reported success but $exe does not exist"
    }

    # samply on Windows records ETW through xperf, which comes from the Windows
    # Performance Toolkit. Missing pieces downgrade to a plain harness run unless
    # -RequireSamply was passed, so the script stays useful on a bare machine.
    $samply = if ($NoSamply) { $null } else { (Get-Command samply -ErrorAction SilentlyContinue).Source }
    if ($samply) {
        $xperf = (Get-Command xperf -ErrorAction SilentlyContinue).Source
        if (-not $xperf) {
            $candidate = Join-Path "${env:ProgramFiles(x86)}" "Windows Kits\10\Windows Performance Toolkit\xperf.exe"
            if (Test-Path $candidate) {
                $xperf = $candidate
            }
        }
        if (-not $xperf) {
            $message = "samply was found but xperf was not. On Windows samply records ETW through " +
                "xperf, which ships with the Windows Performance Toolkit. Install the Windows ADK " +
                "(https://learn.microsoft.com/en-us/windows-hardware/test/wpt/) and uncheck everything " +
                "except 'Windows Performance Toolkit'."
            if ($RequireSamply) {
                throw $message
            }
            Write-Warning $message
            Write-Warning "falling back to the harness without a profiler; latency and CPU are still reported"
            $samply = $null
        }
    }
    elseif (-not $NoSamply) {
        if ($RequireSamply) {
            throw "samply not found (cargo install --locked samply)"
        }
        Write-Warning "samply not found (cargo install --locked samply); running the harness without a profiler."
    }

    $harness = Join-Path $repo "scripts\profile-server.mjs"
    $toolList = ($Tools | ForEach-Object { $_.Split(",") } | ForEach-Object { $_.Trim() } | Where-Object { $_ }) -join ","
    $nodeArgs = @($harness, "--exe", $exe, "--cwd", $repo, "--duration", "$Duration", "--warmup", "$Warmup", "--tools", $toolList)
    if ($Stats) {
        $nodeArgs += @("--stats", $Stats)
    }
    if ($Verbose) {
        $nodeArgs += "--verbose"
    }

    if ($samply) {
        $samplyArgs = @("record", "--output", $Output)
        if ($Duration -gt 0) {
            # Stop sampling shortly after the harness stops driving the server;
            # without this the profile keeps growing while the window stays open.
            $samplyArgs += @("--duration", "$($Duration + 10)")
        }
        if (-not $Serve) {
            # `samply record` otherwise keeps a local web server alive until Ctrl+C.
            $samplyArgs += @("-s", "-n")
        }
        $startedAt = Get-Date
        Write-Host "profiling: samply record (output $Output)" -ForegroundColor Cyan
        if ($Serve) {
            Write-Host "samply serves the profile at http://127.0.0.1:3000+ ; press Ctrl+C to close it" -ForegroundColor DarkGray
        }
        else {
            Write-Host "open the profile afterwards with: samply load $Output" -ForegroundColor DarkGray
        }
        & $samply @samplyArgs -- node @nodeArgs 2>&1 | ForEach-Object { Write-Host $_ }
        $runExitCode = $LASTEXITCODE
        if ($runExitCode -ne 0) {
            # samply reports a non-zero code when its interactive server is torn
            # down, so a fresh profile file is the real success signal.
            $profileFile = if ([System.IO.Path]::IsPathRooted($Output)) { Get-Item $Output -ErrorAction SilentlyContinue } else { Get-Item (Join-Path $repo $Output) -ErrorAction SilentlyContinue }
            if ($null -eq $profileFile -or $profileFile.LastWriteTime -lt $startedAt) {
                throw "samply failed (exit code $runExitCode) and no profile was written to $Output"
            }
            Write-Warning "samply exited with code $runExitCode but wrote $Output; open it with: samply load $Output"
        }
    }
    else {
        Write-Host "running harness without samply" -ForegroundColor Cyan
        & node @nodeArgs 2>&1 | ForEach-Object { Write-Host $_ }
        if ($LASTEXITCODE -ne 0) {
            throw "profiling run failed (exit code $LASTEXITCODE)"
        }
    }
}
finally {
    Pop-Location
}

