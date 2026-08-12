param(
    [int]    $Cols = 8,
    [int]    $Rows = 6,
    # Sync group size. Default -1 = all tiles (Cols*Rows). 0 = sync OFF (independent start).
    [int]    $Sync = -1,
    # avdec_h264 worker threads per decoder (1 recommended; consider 2-3 at 45+ tiles).
    [int]    $DecoderThreads = 1,
    [string] $WindowSize = "1920x1080",
    # Move the window to this desktop position after startup (e.g. second monitor at 1920,0).
    # Both -1 = leave where it opens.
    [int]    $MoveX = -1,
    [int]    $MoveY = -1,
    [switch] $Detach,
    # WR Native Compositor (DirectComposition) gate -- spec
    # docs/superpowers/specs/2026-07-13-wr-native-compositor-design.md. Off (default) =
    # current Draw-compositor path, byte-identical. On passes `--pref gfx_dcomp_mode=on`
    # to this process only; the switch is re-evaluated every run (config-surface-consolidation
    # Task 3: the gate is a `gfx_dcomp_mode` pref now, not an env var -- servoshell reads it
    # unconditionally at startup, so a stale `$env:SERVO_COMPOSITOR_DCOMP` in this shell can
    # no longer leak in or be needed to clear).
    [switch] $DComp,
    # DComp storage-mode selector (spec docs/superpowers/specs/2026-07-14-dcomp-swapchain-
    # content-design.md). Requires -DComp. Without -DCompSurface, -DComp alone selects the
    # swap-chain HYBRID path (`--pref gfx_dcomp_mode=on`): opaque surfaces with repeated
    # full-repaint promote to a flip swapchain (probe-parity Present), everything else stays
    # on the virtual-surface path. With -DCompSurface, -DComp -DCompSurface selects the
    # VIRTUAL-SURFACE-ONLY legacy path (`--pref gfx_dcomp_mode=surface`): no swapchain
    # promotion ever happens -- kept for AMD A/B against the hybrid path. Only the
    # *storage* backend is legacy here (virtual surface only, no swapchain); the deferred
    # AddVisual (Task 4 layer culling) and the end_frame GL flush apply in BOTH modes --
    # so "=surface" is NOT byte-identical to the pre-this-project baseline. Read AMD
    # results with that in mind. -DCompSurface without -DComp is a no-op (warns and is
    # ignored; DComp stays off).
    [switch] $DCompSurface,
    # Video WR-escape gate (spec docs/superpowers/specs/2026-07-17-video-wr-escape-design.md).
    # Only takes effect when -DComp is also set (`gfx_dcomp_mode=on|surface`); layout
    # reads this only after confirming the DComp gate itself is on. "external" is the only
    # valid token: sets PREFER|SUPPORTS_EXTERNAL_COMPOSITOR_SURFACE (video escapes the WR
    # content pass to a compositor-owned external surface). Empty (default) or any other
    # token = off, no flags set, display list byte-identical to pre-this-project. Now a
    # pref (`gfx_video_escape_mode`, config-surface-consolidation Task 4) passed via
    # --pref like -DComp above -- no env var, so nothing to clear when omitted.
    [string] $VideoEscape = "",
    # WR picture-cache tile size override, "WxH" (e.g. "1920x1080" = one window-sized
    # tile per slice). Empty (default) = WR default 1024x512. Sets
    # `--pref gfx_wr_picture_tile_size` (WxH, or `display` for the tile window size). A/B knob
    # for per-tile overhead / invalidation granularity -- total write bandwidth is
    # unchanged by tile size.
    [string] $TileSize = "",
    [string] $LogPrefix = "video_wall_d3d11",
    # Page under the repo root. Default = 1080p30 grid. For the 4K60 source use
    # tests\html\video_4k_grid_play.html with a SMALL grid and MORE decoder threads,
    # e.g.: -Page tests\html\video_4k_grid_play.html -Cols 2 -Rows 2 -DecoderThreads 6
    # (4K60 decode is ~8x a 1080p30 tile; a 20-core box fits about 4 such tiles).
    [string] $Page = "tests\html\video_grid_6x6_play.html",
    # Optional video source (relative to the page, i.e. tests\html\). Appended as
    # &src=<value> to the page URL. Empty = leave the page default unchanged. Used
    # e.g. for 10-bit verification: -Src ../jellyfish-60-mbps-hd-hevc-10bit.mp4
    [string] $Src = ""
)

# Launch the multi-<video> wall demo with the FINAL D3D11 per-pipeline upload recipe
# (2026-07-10). One servoshell window, N independent <video> tiles, all on the D3D11
# path. Since config-surface-consolidation Task 5 all five media knobs below are prefs
# (`--pref`), not env vars -- see $prefArgs further down:
#
#   --pref media_d3d11_enabled=true         per-pipeline GPU upload/convert; renderer only
#                               binds GPU-resident shared textures (upload 27ms -> 0.01ms/frame)
#   --pref media_direct_file_enabled=true    file:// media is read by GStreamer directly
#                               (filesrc); removes the per-rewind script-thread byte
#                               round-trip that caused per-tile freezes / sync boundary stalls
#   --pref media_gapless_loop_enabled=true  SEGMENT rewind looping (no EOS/flush); pristine
#                               loop boundaries and lockstep survives across loops
#   --pref media_sync_group_target=N       all N tiles start on a shared clock (+-1 frame
#                               lockstep) -- N is a pipeline-count target, not a boolean
#   --pref gfx_vsync_enabled=true (config-surface-consolidation Task 2/3 moved this off
#                               SERVO_WIN_VSYNC, which servoshell no longer reads at all)
#                               DWM vsync pacing driver
#   SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF=1 (still an env var -- debug_env investigation
#                               knob, out of Task 5's scope) inert while direct-file is
#                               active; kept as a safety net for any tile that falls back
#                               to the servosrc push path
#
# -DComp (optional, off by default): `--pref gfx_dcomp_mode=on|surface` (CLI, not env --
# Task 3 moved this off SERVO_COMPOSITOR_DCOMP, which servoshell no longer reads at all) --
# WR Native Compositor (DirectComposition). WR draws picture-cache tiles directly into DComp surfaces
# and DWM composites them, eliminating the per-frame tile->backbuffer draw pass (specs
# docs/superpowers/specs/2026-07-13-wr-native-compositor-design.md and
# 2026-07-14-dcomp-swapchain-content-design.md). Aimed at the window-enlarge GPU%/framerate
# falloff on bandwidth-limited GPUs (older AMD). On failure it falls back to the current
# Draw compositor (screen still shows). Two DComp modes (see -DCompSurface above):
# HYBRID (-DComp alone) promotes repeatedly-full-repaint opaque surfaces to a flip
# swapchain (probe-parity Present, no DComp virtual-surface lend/return per frame);
# SURFACE (-DComp -DCompSurface) is the pre-promotion virtual-surface-only legacy path.
# AMD read-out procedure (3-way A/B): (1) run WITHOUT -DComp (Draw + present-path-fast),
# grow the window from 1080p to full-monitor while watching GPU% / perceived smoothness;
# (2) repeat WITH -DComp -DCompSurface (virtual surface); (3) repeat WITH -DComp alone
# (swapchain hybrid); (4) compare all three against the probe (decode-copy-dyn) baseline
# and against each other's PresentMon PresentMode. If (3) improves on (2), the DComp virtual-
# surface lend/return mechanism is the confirmed culprit; if (3) == (2), the falloff has a
# different cause -- attach the log's [dcomp-native] lines and PresentMon CSVs to the report.
#
# Multi-GPU caveat: the gst D3D11 device is created on adapter 0 while the renderer
# (ANGLE) picks its own adapter. On a multi-GPU box a mismatch makes shared-handle
# import fail (black tiles + 'import' warnings in the log) -- adapter affinity is the
# planned follow-up (spec section 4.5). Single-GPU machines are unaffected.

$ErrorActionPreference = "Stop"

$servoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$servoExe  = Join-Path $servoRoot "target\release\servoshell.exe"
$pagePath  = Join-Path $servoRoot $Page
$logDir    = Join-Path $servoRoot "target\multigpu_logs"
$timestamp = Get-Date -Format "yyyyMMdd_HHmmss"
$logPath   = Join-Path $logDir "${LogPrefix}_${timestamp}_stderr.log"

if (!(Test-Path $servoExe)) { throw "servoshell.exe not found (build first: .\mach build --release): $servoExe" }
if (!(Test-Path $pagePath)) { throw "Page not found: $pagePath" }
New-Item -ItemType Directory -Path $logDir -Force | Out-Null

$tiles = $Cols * $Rows
if ($Sync -lt 0) { $Sync = $tiles }

# Final recipe (see header). SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF stays an env var -- it's
# a debug_env investigation knob, not part of config-surface-consolidation Task 5.
$env:SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF = "1"
# gfx_vsync_enabled / gfx_dcomp_mode / the five media_* knobs below are ALL prefs now, not
# env vars (config-surface-consolidation Task 2/3/5) -- servoshell reads its pref set
# unconditionally at startup, so setting `$env:SERVO_WIN_VSYNC`/`$env:SERVO_COMPOSITOR_DCOMP`/
# `$env:SERVO_MEDIA_D3D11_VIDEO` etc. here would silently do nothing (the same trap Task 3
# hit for gfx_dcomp_mode). Pass `--pref` CLI args instead, appended to $prefArgs below and
# spliced into the Start-Process -ArgumentList.
$prefArgs = @(
    "--pref", "gfx_vsync_enabled=true",
    "--pref", "media_d3d11_enabled=true",
    "--pref", "media_direct_file_enabled=true",
    "--pref", "media_gapless_loop_enabled=true",
    "--pref", "media_sync_group_target=$Sync",
    "--pref", "media_avdec_max_threads=$DecoderThreads"
)
# WR Native Compositor gate: only added when requested (same convention as -Sync/
# -DecoderThreads above -- this process's args are the single source of truth, so there is
# no stale-env risk to guard against anymore).
# -DCompSurface without -DComp is a no-op (warn + ignore): DComp stays fully off.
if ($DComp -and $DCompSurface) {
    $prefArgs += @("--pref", "gfx_dcomp_mode=surface")
    $dcompMode = "surface"
} elseif ($DComp) {
    $prefArgs += @("--pref", "gfx_dcomp_mode=on")
    $dcompMode = "hybrid"
} else {
    if ($DCompSurface) {
        Write-Warning "-DCompSurface requires -DComp; ignoring -DCompSurface (DComp stays off)."
    }
    $dcompMode = "off"
}
# Video WR-escape gate is a pref now too (config-surface-consolidation Task 4:
# gfx_video_escape_mode, formerly SERVO_VIDEO_ESCAPE) -- servoshell reads its pref set
# unconditionally at startup, so setting `$env:SERVO_VIDEO_ESCAPE` here would now silently
# do nothing (the same trap Task 3 hit for gfx_dcomp_mode). Appended to $prefArgs like
# -DComp above; layout only honors it once the DComp gate itself is confirmed on, so
# passing it without -DComp is inert (harmless) rather than a hard error.
if ($VideoEscape -ne "") {
    $prefArgs += @("--pref", "gfx_video_escape_mode=$VideoEscape")
}
# WR picture-cache tile size is a pref now (config-surface-consolidation, was env
# SERVO_WR_PICTURE_TILE_SIZE) -- servoshell reads its pref set unconditionally at startup, so
# setting the old env var here would be blocked at startup. Accepts WxH or the token
# `display` (each tile window uses its own size). Omitted = no override (WR default).
if ($TileSize -ne "") {
    $prefArgs += @("--pref", "gfx_wr_picture_tile_size=$TileSize")
}
# Keep GStreamer env clean: the bundled 1.22.x runtime in target\release must not mix
# with any system GStreamer plugins (ABI mismatch).
$env:GST_PLUGIN_PATH = ""
$env:GST_PLUGIN_SYSTEM_PATH_1_0 = ""
if (-not $env:RUST_LOG) {
    $env:RUST_LOG = "warn,servo_media_gstreamer=info,servo_media_gstreamer_render_d3d11=info"
    # paint=info is required to see the "[dcomp-native] engaged" marker (info level);
    # only added when -DComp is requested to keep the off-path log volume unchanged.
    if ($DComp) { $env:RUST_LOG += ",paint=info" }
}

$url = "file:///" + ($pagePath -replace '\\', '/') + "?cols=$Cols&rows=$Rows"
if ($Src -ne "") {
    $url += "&src=" + [Uri]::EscapeDataString($Src)
}

# Task 10c: when -MoveX/-MoveY position the window onto a target monitor, $WindowSize is
# meant as that monitor/tile's footprint (the OUTER window rect should end up matching
# it, e.g. "1920x1080" for a 1080p tile) -- NOT the CLIENT size servoshell's
# --window-size argument actually requests (winit's with_inner_size(), i.e. servoshell
# always creates the window at exactly the requested CLIENT size, chrome added on top).
# Compute the OS title-bar/border delta via AdjustWindowRectEx (no throwaway window
# needed -- decorations are a fixed style, not content-dependent) and subtract it BEFORE
# the process is created, so the window is born at its final client size and never
# resizes afterward. This replaces the old approach of creating at $WindowSize and then
# shrinking the client with a post-creation SetWindowPos resize, which is what produced
# the runtime client-size change these Task 10 fixes eliminate (frozen DComp virtual-
# surface stale content -- .superpowers/sdd/task-10-report.md section 5,
# task-10b-report.md). Without a move (leave-where-it-opens case) $WindowSize is used
# as-is, unchanged from before.
$requestedWindowSize = $WindowSize
if ($MoveX -ge 0 -and $MoveY -ge 0) {
    Add-Type @'
using System;
using System.Runtime.InteropServices;
public class ServoWallDeco {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool AdjustWindowRectEx(ref RECT lpRect, uint dwStyle, bool bMenu, uint dwExStyle);
}
'@
    $WS_OVERLAPPEDWINDOW = 0x00CF0000
    $rect = New-Object ServoWallDeco+RECT
    $rect.Left = 0; $rect.Top = 0
    $targetParts = $WindowSize -split 'x'
    $targetW = [int]$targetParts[0]
    $targetH = [int]$targetParts[1]
    $rect.Right = $targetW; $rect.Bottom = $targetH
    [ServoWallDeco]::AdjustWindowRectEx([ref]$rect, $WS_OVERLAPPEDWINDOW, $false, 0) | Out-Null
    $deltaW = ($rect.Right - $rect.Left) - $targetW
    $deltaH = ($rect.Bottom - $rect.Top) - $targetH
    $innerW = $targetW - $deltaW
    $innerH = $targetH - $deltaH
    $requestedWindowSize = "${innerW}x${innerH}"
    Write-Host "Decoration delta = ${deltaW}x${deltaH}; requesting client size $requestedWindowSize so outer window fits target $WindowSize at ($MoveX,$MoveY)"
}

Write-Host "Launching $Cols x $Rows = $tiles tiles (sync=$Sync, decoder_threads=$DecoderThreads, dcomp=$dcompMode)"
Write-Host "Log: $logPath"

$argumentList = @("--window-size", $requestedWindowSize) + $prefArgs + @($url)
$proc = Start-Process -FilePath $servoExe -ArgumentList $argumentList `
    -RedirectStandardError $logPath -PassThru

Start-Sleep -Seconds 10

# Optionally move the REAL window (class 'Window Class'; winit also creates a helper
# window that must not be targeted) and bring it to the foreground. An occluded window
# gets render-throttled, so foregrounding matters for a valid display.
#
# IMPORTANT (Task 10c): this must be a PURE MOVE -- position only, SWP_NOSIZE set --
# and must NOT pass a width/height to resize the window. servoshell creates the window
# with winit's with_inner_size(), i.e. $WindowSize is already the CLIENT (inner) size;
# the OS then adds title bar/border chrome on top, so the window's OUTER rect is larger
# than $WindowSize from the very first frame (e.g. requested client 1920x1080 => outer
# ~1936x1119). A prior version of this script re-issued SetWindowPos with cx/cy =
# $WindowSize (1920x1080) as the new OUTER size, which shrank the CLIENT area to
# ~1904x1041 (outer minus chrome) a few hundred ms after window creation. On the
# DComp virtual-surface compositor path that post-creation client-size change leaves
# stale/frozen content in the region the old layout painted but the new layout no
# longer covers (see .superpowers/sdd/task-10-report.md section 5, task-10b-report.md).
# Wall deployments never resize at runtime, so a size-changing move here was the ONLY
# source of a runtime client-size change -- removing it makes the client size constant
# for the lifetime of the process, which eliminates that stale-content class entirely.
if ($MoveX -ge 0 -and $MoveY -ge 0) {
    Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public class ServoWallWnd {
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr l);
    public delegate bool EnumWindowsProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int hgt, uint flags);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    public static void Move(uint targetPid, int x, int y) {
        EnumWindows((h, l) => {
            uint pid; GetWindowThreadProcessId(h, out pid);
            if (pid == targetPid && IsWindowVisible(h)) {
                var sb = new StringBuilder(256); GetClassName(h, sb, 256);
                if (sb.ToString() == "Window Class") {
                    // SWP_NOSIZE (0x0001): position only, w/h args are ignored by the
                    // API when this flag is set -- the window's size (client + chrome)
                    // as established at creation time is left untouched.
                    // SWP_SHOWWINDOW (0x0040): unchanged from before.
                    SetWindowPos(h, IntPtr.Zero, x, y, 0, 0, 0x0001 | 0x0040);
                    SetForegroundWindow(h);
                    return false;
                }
            }
            return true;
        }, IntPtr.Zero);
    }
}
'@
    [ServoWallWnd]::Move($proc.Id, $MoveX, $MoveY)
}

# Sanity markers from the first seconds of the log.
# Task 5 rewrote the producer: the per-tile activation line is now
# "D3D11 video: plane 링 프로듀서 경로 활성 (profile_id=N)". Match the ASCII token
# "profile_id=" (unique to that line, one per pipeline) so Select-String does not
# depend on the log's UTF-8 Korean bytes decoding correctly under Windows PowerShell.
$d3d11  = (Select-String -Path $logPath -Pattern "profile_id=" -SimpleMatch -ErrorAction SilentlyContinue | Measure-Object).Count
$direct = (Select-String -Path $logPath -Pattern "direct file playback" -ErrorAction SilentlyContinue | Measure-Object).Count
Write-Host "PID=$($proc.Id) d3d11_active_markers=$d3d11 direct_file=$direct (expect $tiles each)"
if ($direct -lt $tiles) {
    Write-Host "WARNING: not all tiles are on the direct-file path -- check the log for fallbacks."
}

# DComp gate marker: only meaningful when -DComp was requested; verified the same way as
# the d3d11/direct-file markers above (count occurrences, WARN on mismatch). The "engaged"
# marker fires identically for both DComp modes -- the gate is truthy for either env value
# ("1" or "surface"); only the internal storage_mode() differs, so this check alone cannot
# tell hybrid from surface apart. To confirm the mode actually taken, look for
# "[dcomp-dbg] promote" in the log: hybrid should show promote>=1 on repeatedly-full-repaint
# opaque surfaces, surface mode must show none (no swapchain promotion ever happens in that
# mode). That line is gated behind SERVO_DCOMP_DEBUG=1, which is NOT part of this script's
# default RUST_LOG -- set $env:SERVO_DCOMP_DEBUG="1" manually before running to check it.
$dcompEngaged = (Select-String -Path $logPath -Pattern "[dcomp-native] engaged" -SimpleMatch -ErrorAction SilentlyContinue | Measure-Object).Count
if ($DComp) {
    if ($dcompEngaged -ge 1) {
        Write-Host "PASS: dcomp_engaged_markers=$dcompEngaged (WR Native Compositor active, mode=$dcompMode)"
    } else {
        Write-Host "WARNING: -DComp was requested but no '[dcomp-native] engaged' marker was found -- check the log for a fallback to the Draw compositor."
    }
} else {
    if ($dcompEngaged -ge 1) {
        Write-Host "WARNING: dcomp_engaged_markers=$dcompEngaged but -DComp was NOT requested -- unexpected (the gate is CLI-only now, no env to leak in)."
    } else {
        Write-Host "PASS: dcomp_engaged_markers=0 (gate off, as expected)"
    }
}

if (-not $Detach) {
    Write-Host "Press Ctrl+C to stop; killing servoshell on exit..."
    try { Wait-Process -Id $proc.Id } finally {
        Get-Process servoshell -ErrorAction SilentlyContinue | Stop-Process -Force
    }
}
