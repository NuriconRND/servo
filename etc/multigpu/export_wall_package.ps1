param(
    # Where the package tree is written. The .zip is created next to it.
    [string]   $OutDir = "",
    # Extra media to drop into the package's tests\ dir (e.g. the untracked 4K clip).
    # The 6x6 page can then use it via run_wall.ps1 -Src ../<filename>.
    [string[]] $Media = @(),
    # Stage the folder but don't compress it.
    [switch]   $NoZip,
    # Skip the 2x2 smoke run that proves the staged tree runs standalone.
    [switch]   $SkipVerify,
    # Overwrite an existing output dir / zip without asking.
    [switch]   $Force
)

# Export a standalone, repo-independent copy of the video wall for another machine:
# unzip anywhere, run run_wall.ps1. Requires Win10/11 x64 + a D3D11 GPU + the VC++ x64
# redistributable on the target (run_wall.ps1 checks for vcruntime140 and says so).
#
# Layout produced (flat -- servoshell's resources/ lookup walks the exe's ancestors, and
# run_wall.ps1 detects this layout by finding servoshell.exe beside itself):
#
#   ServoWallPackage\
#     servoshell.exe
#     *.dll                 all of target\release's DLLs (bundled GStreamer 1.22.8)
#     resources\            from the repo root
#     tests\
#       Wildlife_FHD30fps_counter_10Mbitrate.mp4
#       html\video_grid_6x6_play.html, video_4k_grid_play.html
#     run_wall.ps1          verbatim copy of etc\multigpu\run_video_wall_d3d11.ps1
#     README.md
#
# DLLs are copied by rule (whatever the build produced) rather than from a pinned list --
# this project has already moved between GStreamer 1.22.8 and 1.26.8, and a stale list
# would silently ship the wrong set. What IS pinned is the must-exist check below, for
# the few files whose absence fails *quietly*.

$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$relDir   = Join-Path $repoRoot "target\release"
$srcExe   = Join-Path $relDir "servoshell.exe"

if ($OutDir -eq "") { $OutDir = Join-Path $repoRoot "target\ServoWallPackage" }
$zipPath = $OutDir.TrimEnd('\') + ".zip"

if (!(Test-Path $srcExe)) {
    throw "servoshell.exe not found -- build first (.\mach build --release): $srcExe"
}

# --- clean / create the staging tree ---------------------------------------------------
foreach ($p in @($OutDir, $zipPath)) {
    if (Test-Path $p) {
        if (!$Force) { throw "Already exists: $p  (pass -Force to overwrite)" }
        Remove-Item $p -Recurse -Force
    }
}
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $OutDir "tests\html") -Force | Out-Null

# --- copy ------------------------------------------------------------------------------
Write-Host "Staging -> $OutDir"

Copy-Item $srcExe -Destination $OutDir
$dlls = Get-ChildItem $relDir -Filter *.dll -File
$dlls | Copy-Item -Destination $OutDir
Write-Host ("  servoshell.exe + {0} DLLs ({1:N0} MB)" -f $dlls.Count,
    ((($dlls | Measure-Object Length -Sum).Sum + (Get-Item $srcExe).Length) / 1MB))
# 96 was the count for the verified 1.22.8 bundle; a change is worth a look, not a failure.
if ($dlls.Count -ne 96) {
    Write-Host "  NOTE: DLL count is $($dlls.Count), expected 96 -- dependencies changed since the last known-good package."
}

Copy-Item (Join-Path $repoRoot "resources") -Destination $OutDir -Recurse
$pages = @("video_grid_6x6_play.html", "video_4k_grid_play.html")
foreach ($p in $pages) {
    Copy-Item (Join-Path $repoRoot "tests\html\$p") -Destination (Join-Path $OutDir "tests\html")
}

$defaultVideo = "Wildlife_FHD30fps_counter_10Mbitrate.mp4"
Copy-Item (Join-Path $repoRoot "tests\$defaultVideo") -Destination (Join-Path $OutDir "tests")

foreach ($m in $Media) {
    if (!(Test-Path $m)) { throw "-Media file not found: $m" }
    Copy-Item $m -Destination (Join-Path $OutDir "tests")
    Write-Host ("  + media: {0} ({1:N0} MB)" -f (Split-Path $m -Leaf), ((Get-Item $m).Length / 1MB))
}

# One launcher, copied verbatim -- it detects the flat layout at runtime.
Copy-Item (Join-Path $PSScriptRoot "run_video_wall_d3d11.ps1") -Destination (Join-Path $OutDir "run_wall.ps1")

# --- must-exist checks -----------------------------------------------------------------
# Only for files whose absence would NOT be obvious at runtime.
$mustExist = @{
    "servoshell.exe"                 = "the shell itself"
    # Dropping gstd3d11.dll makes -LegacyUpload silently fall back to the Raw path, which
    # would invalidate an A/B measurement rather than fail (ai-notes.md 3-n, trap 2).
    "gstd3d11.dll"                   = "legacy upload path A/B validity"
    "resources"                      = "servoshell won't start without it"
    "tests\html\$($pages[0])"        = "the default wall page"
    "tests\$defaultVideo"            = "the default video source"
    "run_wall.ps1"                   = "the launcher"
}
$missing = @()
foreach ($k in $mustExist.Keys) {
    if (!(Test-Path (Join-Path $OutDir $k))) { $missing += "$k  ($($mustExist[$k]))" }
}
if ($missing.Count -gt 0) {
    throw "Package is incomplete, missing:`n  " + ($missing -join "`n  ")
}

# The 4K page hardcodes ../4k_3DMark.mp4 (no ?src override, unlike the 6x6 page), so it
# only works if a file with exactly that name is present.
if (!(Test-Path (Join-Path $OutDir "tests\4k_3DMark.mp4"))) {
    Write-Host "  NOTE: 4k_3DMark.mp4 is absent -- video_4k_grid_play.html will show nothing."
    Write-Host "        It hardcodes that exact filename; re-run with -Media <path to 4k_3DMark.mp4> to include it."
}

# --- README ----------------------------------------------------------------------------
$readme = @'
# Servo Video Wall -- standalone package

Unzip anywhere and run. No repo, no build environment, no install.

## Requirements

- Windows 10/11 x64
- A D3D11-capable GPU
- Microsoft Visual C++ x64 redistributable (the launcher checks and tells you if missing)

## Run

```powershell
.\run_wall.ps1 -Cols 6 -Rows 8
```

Useful switches:

| Switch | Meaning |
|---|---|
| `-Cols N -Rows N` | grid size (tiles = Cols x Rows) |
| `-WindowSize 1920x1080` | window size |
| `-MoveX 1920 -MoveY 0` | move the window onto another monitor and foreground it |
| `-DecoderThreads N` | avdec_h264 threads per tile (1 is right for 1080p30) |
| `-Sync N` | `-1` (default) = all tiles lockstep, `0` = independent start |
| `-DComp` | WR native compositor (DirectComposition) -- see A/B below |
| `-TileSize 1920x1080` | WR picture-cache tile size (default 1024x512) |
| `-Src ../<file>` | use another video in `tests\` (6x6 page only) |
| `-Detach` | return immediately instead of waiting; leaves the window up |

The window is occluded-throttled if it's behind something, so keep it foregrounded while
measuring. Logs land in `logs\`.

## Reading the result

On launch the launcher prints, and these are the pass conditions:

```
d3d11_active_markers=N direct_file=N (expect N each)   <- both must equal the tile count
PASS: dcomp_engaged_markers=1                          <- only when -DComp is passed
```

Then check `logs\*.log` for `panic` (must be 0) and `import` failures (must be 0 --
they mean black tiles). Each tile draws its own frame counter: with `-Sync -1` all tiles
should stay within +-1 of each other.

## What to measure

**Capacity on this machine.** Raise `-Cols`/`-Rows` until the frame counters stop keeping
up with the source rate (the 1080p30 clip should advance 30 counter ticks per second).
That tile count is the machine's ceiling for this content.

**DComp A/B (for bandwidth-limited GPUs, e.g. older AMD).** Run the same grid twice --
once without `-DComp`, once with -- and each time grow the window from 1080p toward
full-monitor while watching GPU% and smoothness. Without the gate, WebRender redraws
content into picture-cache tiles and then draws those tiles into the backbuffer, so cost
scales with window area. With it, WR draws into DirectComposition surfaces and DWM
composites them, removing that second pass. Expect `-DComp` to flatten the GPU% slope as
the window grows. If it doesn't, send the `[dcomp-native]` lines from the log.

Caveat: video decode dominates GPU occupancy at high tile counts and can bury the signal.
Use a small grid (e.g. 2x2) for the window-enlarge sweep.

## Known limits

- Multi-GPU machines: the GStreamer D3D11 device is created on adapter 0 while the
  renderer picks its own adapter. If they differ, shared-handle import fails -- black
  tiles plus `import` warnings in the log. Adapter affinity is not implemented yet.
- `video_4k_grid_play.html` needs `tests\4k_3DMark.mp4` (that exact name) to be present.
- Software decode only; hardware decode is not enabled.
'@
Set-Content -Path (Join-Path $OutDir "README.md") -Value $readme -Encoding UTF8

$staged = Get-ChildItem $OutDir -Recurse -File
Write-Host ("Staged: {0} files, {1:N0} MB" -f $staged.Count, ((($staged | Measure-Object Length -Sum).Sum) / 1MB))

# --- smoke: prove the staged tree runs on its own --------------------------------------
# Run FROM the staging dir, so a file we forgot to copy shows up here rather than on the
# target machine. Uses a 2x2 grid to keep it quick.
if (!$SkipVerify) {
    Write-Host "Verifying (2x2 smoke run from the staged tree)..."
    # 6>&1 merges the launcher's Write-Host (information stream) into the pipeline -- without
    # it $out comes back empty and every marker reads as absent.
    $out = & (Join-Path $OutDir "run_wall.ps1") -Cols 2 -Rows 2 -Detach -LogPrefix "pkg_verify" 6>&1 2>&1 | Out-String
    ($out.Trim() -split "`r?`n") | ForEach-Object { Write-Host "  $_" }

    # Kill the detached window before judging, so a failure can't leave it running. Wait for
    # it to actually exit: its stderr redirect holds the log file open, and deleting the log
    # dir below fails while that handle lives (which is how a verify log once shipped inside
    # the package).
    if ($out -match 'PID=(\d+)') {
        $verifyPid = [int]$Matches[1]
        Stop-Process -Id $verifyPid -Force -ErrorAction SilentlyContinue
        Wait-Process -Id $verifyPid -Timeout 15 -ErrorAction SilentlyContinue
    }

    # Judge from the log rather than the launcher's console text: the log is the evidence,
    # and these are the same markers the launcher counts.
    $verifyLog = Get-ChildItem (Join-Path $OutDir "logs") -Filter "pkg_verify_*" -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime | Select-Object -Last 1
    if (!$verifyLog) { throw "Smoke run FAILED: the staged package produced no log at all." }

    $count = { param($pattern) (Select-String -Path $verifyLog.FullName -Pattern $pattern -SimpleMatch -ErrorAction SilentlyContinue | Measure-Object).Count }
    $d3d11  = & $count "profile_id="
    $direct = & $count "direct file playback"
    $panics = & $count "panic"

    if ($d3d11 -ne 4 -or $direct -ne 4 -or $panics -ne 0) {
        throw ("Smoke run FAILED (d3d11=$d3d11 direct_file=$direct panics=$panics, expected 4/4/0)." +
               "`nThe staged package does not run standalone -- not zipping. Log: $($verifyLog.FullName)")
    }
    Write-Host "  PASS: d3d11=4/4 direct_file=4/4 panics=0 (ran standalone from the staged tree)"

    # Drop the verify log so it doesn't ship. Assert rather than assume: a swallowed failure
    # here is how it got into the zip before. (On a FAILED run we throw above and the log
    # stays put on purpose, as evidence.)
    $logsDir = Join-Path $OutDir "logs"
    for ($i = 0; $i -lt 10 -and (Test-Path $logsDir); $i++) {
        Remove-Item $logsDir -Recurse -Force -ErrorAction SilentlyContinue
        if (Test-Path $logsDir) { Start-Sleep -Milliseconds 300 }
    }
    if (Test-Path $logsDir) {
        throw "Could not delete the verify log dir; it would ship inside the package: $logsDir"
    }
}

# --- zip -------------------------------------------------------------------------------
if (!$NoZip) {
    Write-Host "Compressing -> $zipPath (this takes a minute)"
    Compress-Archive -Path (Join-Path $OutDir "*") -DestinationPath $zipPath -CompressionLevel Optimal
    Write-Host ("Done: {0} ({1:N0} MB)" -f $zipPath, ((Get-Item $zipPath).Length / 1MB))
} else {
    Write-Host "Done: $OutDir (-NoZip, not compressed)"
}
