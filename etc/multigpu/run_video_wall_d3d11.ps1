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
# path:
#
#   SERVO_MEDIA_D3D11_VIDEO=1   per-pipeline GPU upload/convert; renderer only binds
#                               GPU-resident shared textures (upload 27ms -> 0.01ms/frame)
#   SERVO_MEDIA_DIRECT_FILE=1   file:// media is read by GStreamer directly (filesrc);
#                               removes the per-rewind script-thread byte round-trip that
#                               caused per-tile freezes / sync boundary stalls
#   SERVO_MEDIA_GAPLESS_LOOP=1  SEGMENT rewind looping (no EOS/flush); pristine loop
#                               boundaries and lockstep survives across loops
#   SERVO_MEDIA_SYNC_GROUP=N    all N tiles start on a shared clock (+-1 frame lockstep)
#   SERVO_WIN_VSYNC=1           DWM vsync pacing driver
#   SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF=1
#                               inert while DIRECT_FILE is active; kept as a safety net
#                               for any tile that falls back to the servosrc push path
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

# Final recipe (see header). Values live only in this process environment.
$env:SERVO_MEDIA_D3D11_VIDEO = "1"
$env:SERVO_MEDIA_DIRECT_FILE = "1"
$env:SERVO_MEDIA_GAPLESS_LOOP = "1"
$env:SERVO_MEDIA_SYNC_GROUP = "$Sync"
$env:SERVO_WIN_VSYNC = "1"
$env:SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF = "1"
$env:SERVO_GSTREAMER_AVDEC_MAX_THREADS = "$DecoderThreads"
# Keep GStreamer env clean: the bundled 1.22.x runtime in target\release must not mix
# with any system GStreamer plugins (ABI mismatch).
$env:GST_PLUGIN_PATH = ""
$env:GST_PLUGIN_SYSTEM_PATH_1_0 = ""
if (-not $env:RUST_LOG) {
    $env:RUST_LOG = "warn,servo_media_gstreamer=info,servo_media_gstreamer_render_d3d11=info"
}

$url = "file:///" + ($pagePath -replace '\\', '/') + "?cols=$Cols&rows=$Rows"
if ($Src -ne "") {
    $url += "&src=" + [Uri]::EscapeDataString($Src)
}

Write-Host "Launching $Cols x $Rows = $tiles tiles (sync=$Sync, decoder_threads=$DecoderThreads)"
Write-Host "Log: $logPath"

$proc = Start-Process -FilePath $servoExe -ArgumentList @("--window-size", $WindowSize, $url) `
    -RedirectStandardError $logPath -PassThru

Start-Sleep -Seconds 10

# Optionally move the REAL window (class 'Window Class'; winit also creates a helper
# window that must not be targeted) and bring it to the foreground. An occluded window
# gets render-throttled, so foregrounding matters for a valid display.
if ($MoveX -ge 0 -and $MoveY -ge 0) {
    $sizeParts = $WindowSize -split 'x'
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
    public static void Move(uint targetPid, int x, int y, int w, int hgt) {
        EnumWindows((h, l) => {
            uint pid; GetWindowThreadProcessId(h, out pid);
            if (pid == targetPid && IsWindowVisible(h)) {
                var sb = new StringBuilder(256); GetClassName(h, sb, 256);
                if (sb.ToString() == "Window Class") {
                    SetWindowPos(h, IntPtr.Zero, x, y, w, hgt, 0x0040);
                    SetForegroundWindow(h);
                    return false;
                }
            }
            return true;
        }, IntPtr.Zero);
    }
}
'@
    [ServoWallWnd]::Move($proc.Id, $MoveX, $MoveY, [int]$sizeParts[0], [int]$sizeParts[1])
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

if (-not $Detach) {
    Write-Host "Press Ctrl+C to stop; killing servoshell on exit..."
    try { Wait-Process -Id $proc.Id } finally {
        Get-Process servoshell -ErrorAction SilentlyContinue | Stop-Process -Force
    }
}
