# ServoWallPackage launcher (portable; no dev environment needed).
# Rebuilt 2026-07-18 from etc\multigpu\run_video_wall_d3d11.ps1 (same recipe/params),
# adapted to package-relative paths.
#
# Requirements: Win10/11 x64, D3D11 GPU, VC++ 2015-2022 x64 redistributable.
#
# Korean fonts: pages use "Noto Sans KR" (body) / "Noto Sans CJK KR" (bold elements).
# On a machine without these fonts Korean text falls back to tofu/serif — run
# .\install_fonts.ps1 ONCE (per-user, offline, no admin, no reboot) before first display.
# (Servo does not load @font-face web fonts, cannot select weights within a family,
#  and fails to name-match the installed "Malgun Gothic" — measured 2026-07-18.)
#
# AMD 2-way A/B (performance read-out; use the PURE wall page):
#   1) .\run_wall.ps1 -Cols 9 -Rows 5 -Sync -1 -DComp                        (baseline hybrid)
#   2) .\run_wall.ps1 -Cols 9 -Rows 5 -Sync -1 -DComp -VideoEscape external  (full WR escape)
# Read-out: 2>1 confirms the per-tile submission-overhead hypothesis. Compare GPU%
# of 1 vs 2 (DWM visual-count cost). external does NOT need -TileSize (A/B showed no
# difference once video leaves the WR content pass).
#
# fps-cliff investigation (external, tile-count sweep): set $env:SERVO_VIDEO_ESCAPE_PROF='1'
# before the run; the log then carries 1 Hz lines like
#   [vesc-prof] frames=.. converts=.. presents=.. acquire_ms=.. convert_ms=.. present_ms=..
# Collect 3-4 lines per tile count (e.g. 30 vs 36). If present_ms explodes with count,
# the per-swapchain Present serialization is the cliff (next step: shared video canvas).

param(
    [int]    $Cols = 8,
    [int]    $Rows = 6,
    # Sync group size. Default -1 = all tiles (Cols*Rows). 0 = sync OFF (independent start).
    # Pages with extra non-grid videos need the TOTAL video count explicitly
    # (complex_media_stress/transforms: 12 grid + 1 PiP => -Sync 13).
    [int]    $Sync = -1,
    # avdec worker threads per decoder (1 recommended; consider 2-3 at 45+ tiles; 6 for 4K).
    [int]    $DecoderThreads = 1,
    [string] $WindowSize = "1920x1080",
    # Move the window to this desktop position after startup (e.g. second monitor at 1920,0).
    # Both -1 = leave where it opens.
    [int]    $MoveX = -1,
    [int]    $MoveY = -1,
    [switch] $Detach,
    # WR Native Compositor (DirectComposition) gate. Off (default) = Draw compositor.
    [switch] $DComp,
    # With -DComp: virtual-surface-only legacy storage (AMD A/B); without it, hybrid.
    [switch] $DCompSurface,
    # Video WR-escape gate (requires -DComp): "external" is the only valid token — final
    # path (per-video DComp swapchain, video leaves the WR content pass entirely).
    [string] $VideoEscape = "",
    # WR picture-cache tile size override "WxH" (baseline recipe: 3840x3240; NOT needed
    # for external).
    [string] $TileSize = "",
    [string] $LogPrefix = "wall",
    # Page under this package. IMPORTANT: this launcher always appends ?cols=&rows= to the
    # URL; pages that read them (mixed/stress/transforms) need matching -Cols/-Rows:
    #   mixed_media_demo:          -Cols 3 -Rows 2 -Sync 6
    #   complex_media_stress:      -Cols 4 -Rows 3 -Sync 13
    #   complex_media_transforms:  -Cols 4 -Rows 3 -Sync 13
    #   video_4k_grid_play:        -Cols 2 -Rows 2 -DecoderThreads 6
    [string] $Page = "tests\html\video_grid_6x6_play.html",
    # Optional video source (relative to the page, i.e. tests\html\). 10-bit demo:
    #   -Src ../jellyfish-60-mbps-hd-hevc-10bit.mp4
    [string] $Src = ""
)

$ErrorActionPreference = "Stop"

$pkgRoot  = $PSScriptRoot
$servoExe = Join-Path $pkgRoot "servoshell.exe"
$pagePath = Join-Path $pkgRoot $Page
$logDir   = Join-Path $pkgRoot "logs"
$timestamp = Get-Date -Format "yyyyMMdd_HHmmss"
$logPath  = Join-Path $logDir "${LogPrefix}_${timestamp}_stderr.log"

if (!(Test-Path $servoExe)) { throw "servoshell.exe not found next to this script: $servoExe" }
if (!(Test-Path $pagePath)) { throw "Page not found: $pagePath" }
if (!(Test-Path (Join-Path $env:SystemRoot "System32\vcruntime140.dll"))) {
    throw "VC++ x64 redistributable missing (vcruntime140.dll) - install it first."
}
New-Item -ItemType Directory -Path $logDir -Force | Out-Null

$tiles = $Cols * $Rows
if ($Sync -lt 0) { $Sync = $tiles }

# Final display recipe. SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF stays an env var -- it's a
# debug_env investigation knob, not part of config-surface-consolidation Task 5.
$env:SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF = "1"
# gfx_vsync_enabled / gfx_dcomp_mode / the five media_* knobs below are ALL prefs now, not
# env vars (config-surface-consolidation Task 2/3/5) -- servoshell reads its pref set
# unconditionally at startup, so setting `$env:SERVO_WIN_VSYNC`/`$env:SERVO_COMPOSITOR_DCOMP`/
# `$env:SERVO_MEDIA_D3D11_VIDEO` etc. here would silently do nothing. Pass `--pref` CLI args
# instead, appended to $prefArgs below and spliced into the Start-Process -ArgumentList.
$prefArgs = @(
    "--pref", "gfx_vsync_enabled=true",
    "--pref", "media_d3d11_enabled=true",
    "--pref", "media_direct_file_enabled=true",
    "--pref", "media_gapless_loop_enabled=true",
    "--pref", "media_sync_group_enabled=$Sync",
    "--pref", "media_avdec_max_threads=$DecoderThreads"
)
if ($DComp -and $DCompSurface) {
    $prefArgs += @("--pref", "gfx_dcomp_mode=surface")
    $dcompMode = "surface"
} elseif ($DComp) {
    $prefArgs += @("--pref", "gfx_dcomp_mode=on")
    $dcompMode = "hybrid"
} else {
    if ($DCompSurface) {
        Write-Warning "-DCompSurface requires -DComp; ignoring (DComp stays off)."
    }
    $dcompMode = "off"
}
# 비디오 WR 탈출 게이트도 이제 pref 다(config-surface-consolidation Task 4:
# gfx_video_escape_mode, 구 SERVO_VIDEO_ESCAPE) -- servoshell 은 자신의 pref 세트를
# 기동 시 무조건 읽으므로 `$env:SERVO_VIDEO_ESCAPE` 를 여기서 설정해도 이제 조용히
# 무시된다(Task 3 의 gfx_dcomp_mode 와 같은 함정). 위 DComp 블록과 같은 방식으로
# $prefArgs 에 추가한다.
if ($VideoEscape -ne "") {
    $prefArgs += @("--pref", "gfx_video_escape_mode=$VideoEscape")
}
if ($TileSize -ne "") {
    $env:SERVO_WR_PICTURE_TILE_SIZE = $TileSize
} else {
    Remove-Item Env:\SERVO_WR_PICTURE_TILE_SIZE -ErrorAction SilentlyContinue
}
# Keep GStreamer env clean: the bundled 1.22.x runtime (DLLs next to the exe) must not
# mix with any system GStreamer plugins (ABI mismatch).
$env:GST_PLUGIN_PATH = ""
$env:GST_PLUGIN_SYSTEM_PATH_1_0 = ""
if (-not $env:RUST_LOG) {
    $env:RUST_LOG = "warn,servo_media_gstreamer=info,servo_media_gstreamer_render_d3d11=info"
    if ($DComp) { $env:RUST_LOG += ",paint=info" }
}

$url = "file:///" + ($pagePath -replace '\\', '/') + "?cols=$Cols&rows=$Rows"
if ($Src -ne "") {
    $url += "&src=" + [Uri]::EscapeDataString($Src)
}

# When -MoveX/-MoveY target a monitor, $WindowSize means the OUTER window footprint.
# servoshell's --window-size requests the CLIENT size, so subtract the decoration delta
# BEFORE creation (AdjustWindowRectEx) - the window is born at its final client size and
# never resizes (a post-creation resize leaves stale DComp virtual-surface content).
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

# Optionally move the REAL window (class 'Window Class') and bring it to the foreground.
# PURE MOVE only (SWP_NOSIZE) - a post-creation resize would leave stale DComp content.
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

# Sanity markers from the first seconds of the log (ASCII tokens; the log may carry
# UTF-8 Korean that Windows PowerShell decodes unreliably).
$d3d11  = (Select-String -Path $logPath -Pattern "profile_id=" -SimpleMatch -ErrorAction SilentlyContinue | Measure-Object).Count
$direct = (Select-String -Path $logPath -Pattern "direct file playback" -ErrorAction SilentlyContinue | Measure-Object).Count
Write-Host "PID=$($proc.Id) d3d11_active_markers=$d3d11 direct_file=$direct (expect $tiles each)"
if ($direct -lt $tiles) {
    Write-Host "WARNING: not all tiles are on the direct-file path -- check the log for fallbacks."
}

$dcompEngaged = (Select-String -Path $logPath -Pattern "[dcomp-native] engaged" -SimpleMatch -ErrorAction SilentlyContinue | Measure-Object).Count
if ($DComp) {
    if ($dcompEngaged -ge 1) {
        Write-Host "PASS: dcomp_engaged_markers=$dcompEngaged (WR Native Compositor active, mode=$dcompMode)"
    } else {
        Write-Host "WARNING: -DComp requested but no '[dcomp-native] engaged' marker -- check the log for a Draw-compositor fallback."
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
