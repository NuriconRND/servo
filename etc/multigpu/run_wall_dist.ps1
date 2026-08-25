# Runs the packaged multi-GPU wall (pref-era: engine knobs are --pref, not env vars).
#
# Pure ASCII on purpose -- a Korean launcher failed to parse on a test machine that
# decodes with a legacy console codepage, swallowing a closing quote.
#
# RUST_LOG is set UNCONDITIONALLY. It used to be `if (-not $env:RUST_LOG)` and that
# silently defeated the diagnostics twice: a value left over in the shell (without
# "media=info") won, so the WALLDIAG lines from the media-thread crate never appeared.
# NOTE that crate logs under the target "media", NOT "servo_media_thread" -- its
# Cargo.toml sets [lib] name = "media".

param(
    [string] $Layout = "wall_layout.multigpu.json",
    [string] $Url = "",                  # empty = bundled 6x6 page
    [int]    $Rows = 6,
    [int]    $Cols = 6,
    [int]    $SyncGroup = 0,             # 0 = auto (Rows * Cols)
    # Turn the synchronized group start OFF. It is worth an A/B because releasing
    # the group pins every pipeline to a SHARED base time with start_time=NONE, so
    # running time tracks the wall clock and is never re-based. Together with the
    # smooth policy (qos=false, drop=false, max-lateness=-1) that leaves a pipeline
    # no way to recover once it falls behind -- it just decodes flat out forever.
    [switch] $NoSyncGroup,
    [int]    $DecoderThreads = 1,
    [string] $TileSize = "display",      # gfx_wr_picture_tile_size
    [int]    $RefreshHz = 60,
    [ValidateSet("off", "on", "surface")]
    [string] $DComp = "surface",
    [switch] $Vsync,                     # gfx_vsync_enabled (default off; see note below)
    [string] $VideoEscape = "",          # gfx_video_escape_mode; "external" to enable
    # appsink qos, isolated from the sink policy. Empty = policy default (Smooth => off).
    # With qos off the decoder cannot skip frames under load, so an overloaded wall falls
    # off a cliff instead of degrading. See media_video_sink_qos in configuration.md.
    [ValidateSet("", "on", "off")]
    [string] $SinkQos = "",
    # media_video_sink_policy. Changes qos AND drop AND max-lateness AND max-buffers at
    # once -- use -SinkQos when you want to move only qos.
    [ValidateSet("", "smooth", "low-latency")]
    [string] $SinkPolicy = "",
    [int]    $MaxPending = 0,            # 0 = leave default (1)
    [int]    $MinIntervalMs = 0,         # 0 = leave default (16)
    [int]    $DurationSec = 0,
    [string] $LogPath = "",
    [switch] $KeepRustLog,
    # SERVO_D3D11_PROFILE=1: per-pipeline stage timings, one heartbeat line per second per
    # video plus any frame over the threshold. The copy= field is the CPU memcpy of the
    # decoded planes into GPU-mapped memory -- the thing to look at when decode is cheap
    # but the wall still saturates CPU.
    [switch] $D3d11Profile,
    # SERVO_D3D11_PROFILE_MS threshold (default 8). Lower it to see more frames.
    [int]    $D3d11ProfileMs = 0,
    # Attribute the wall's CPU to its individual threads while it plays. Answers the
    # question D3D11PROF cannot: decode and upload run on ~N parallel streaming
    # threads, so if one of the FEW single-threaded stages (Compositor, Renderer,
    # Script) is pinned at ~1.0 cores, that thread is the ceiling and the GPU is
    # starved behind it. Needs -DurationSec (there is nothing to sample otherwise).
    [switch] $ThreadCpu,
    # Seconds to let playback settle before sampling. The opening seconds are
    # pipeline setup and first-frame staging, which are not the steady state.
    [int]    $ThreadCpuWarmupSec = 8
)

$ErrorActionPreference = "Stop"
$here   = $PSScriptRoot
$engine = Join-Path $here "engine"
$exe    = Join-Path $engine "winit_wall.exe"
$layout = Join-Path $here "config\$Layout"

if (!(Test-Path $exe))    { throw "winit_wall.exe not found: $exe" }
if (!(Test-Path $layout)) { throw "layout not found: $layout" }

if ($Url -eq "") {
    $page = Join-Path $here "pages\html\video_grid_6x6_play.html"
    if (!(Test-Path $page)) { throw "bundled page not found: $page" }
    $Url = "file:///" + (($page -replace '\\', '/')) + "?rows=$Rows&cols=$Cols"
}
if ($NoSyncGroup)      { $SyncGroup = 0 }
elseif ($SyncGroup -le 0) { $SyncGroup = $Rows * $Cols }
if ($LogPath -eq "")  { $LogPath = Join-Path $here ("wall_{0}.err.log" -f (Get-Date -Format "yyyyMMdd_HHmmss")) }

# Old env knobs block startup in this build (servo_config::removed_env). Clear any that
# leaked in from an earlier session so the run does not die with a migration notice.
Get-ChildItem Env: | Where-Object { $_.Name -like "SERVO_*" } |
    ForEach-Object { [Environment]::SetEnvironmentVariable($_.Name, $null, "Process") }

# Set AFTER the SERVO_* wipe above, or it gets cleared.
if ($D3d11Profile)        { $env:SERVO_D3D11_PROFILE = "1" }
if ($D3d11ProfileMs -gt 0) { $env:SERVO_D3D11_PROFILE_MS = "$D3d11ProfileMs" }

$env:GST_PLUGIN_PATH            = ""
$env:GST_PLUGIN_SYSTEM_PATH_1_0 = ""
$env:PATH = "$engine;$env:PATH"
if (-not $KeepRustLog) {
    $env:RUST_LOG = "warn,paint=info,media=info," +
                    "servo_media_gstreamer=info,servo_media_gstreamer_render_d3d11=info"
}
if ($env:RUST_LOG -notmatch "(^|,)media=") {
    Write-Warning "RUST_LOG has no 'media=' target -- the WALLDIAG consumer-device/wrap lines will be MISSING."
}

$tiles = $Rows * $Cols
$argList = @(
    "--wall-layout", $layout, "--wall-all-tiles",
    "--pref", "gfx_dcomp_mode=$DComp",
    "--pref", "gfx_vsync_enabled=$($Vsync.IsPresent.ToString().ToLower())",
    "--pref", "gfx_refresh_hz=$RefreshHz",
    "--pref", "gfx_wr_picture_tile_size=$TileSize",
    "--pref", "media_d3d11_enabled=true",
    "--pref", "media_direct_file_enabled=true",
    "--pref", "media_gapless_loop_enabled=true",
    "--pref", "media_avdec_max_threads=$DecoderThreads",
    "--pref", "media_sync_group_target=$SyncGroup"
)
if ($VideoEscape -ne "")  { $argList += @("--pref", "gfx_video_escape_mode=$VideoEscape") }
if ($SinkQos -ne "")      { $argList += @("--pref", "media_video_sink_qos=$SinkQos") }
if ($SinkPolicy -ne "")   { $argList += @("--pref", "media_video_sink_policy=$SinkPolicy") }
if ($MaxPending -gt 0)    { $argList += @("--pref", "gfx_wall_frame_max_pending=$MaxPending") }
if ($MinIntervalMs -gt 0) { $argList += @("--pref", "gfx_wall_frame_min_interval_ms=$MinIntervalMs") }
$argList += $Url

Write-Host "Wall (pref-era) -- $tiles tiles requested by the page grid"
Write-Host "  layout=$layout"
Write-Host "  dcomp=$DComp tile_size=$TileSize refresh=${RefreshHz}Hz vsync=$($Vsync.IsPresent) escape=$(if($VideoEscape -eq ''){'off'}else{$VideoEscape})"
Write-Host "  sync_group=$(if($SyncGroup -le 0){'off'}else{$SyncGroup}) decoder_threads=$DecoderThreads sink_qos=$(if($SinkQos -eq ''){'policy'}else{$SinkQos}) sink_policy=$(if($SinkPolicy -eq ''){'default'}else{$SinkPolicy})"
Write-Host "  d3d11_profile=$($D3d11Profile.IsPresent)$(if($D3d11ProfileMs -gt 0){" threshold=${D3d11ProfileMs}ms"})"
Write-Host "  RUST_LOG=$env:RUST_LOG"
Write-Host "  log=$LogPath"

$proc = Start-Process -FilePath $exe -ArgumentList $argList -WorkingDirectory $here `
    -RedirectStandardError $LogPath -PassThru

if ($DurationSec -gt 0) {
    $sampled = $false
    if ($ThreadCpu) {
        $probe  = Join-Path $engine "thread_cpu_probe.exe"
        $window = $DurationSec - $ThreadCpuWarmupSec - 2
        if (!(Test-Path $probe)) {
            Write-Warning "thread_cpu_probe.exe is not in engine\ -- repackage with make_wall_dist.ps1. Skipping the thread breakdown."
        } elseif ($window -lt 5) {
            Write-Warning "-DurationSec $DurationSec leaves only ${window}s after a ${ThreadCpuWarmupSec}s warmup. Use at least -DurationSec $($ThreadCpuWarmupSec + 12). Skipping the thread breakdown."
        } else {
            Start-Sleep -Seconds $ThreadCpuWarmupSec
            $threadLog = [IO.Path]::ChangeExtension($LogPath, ".threads.txt")
            Write-Host ""
            Write-Host "--- thread CPU breakdown (${window}s window, after ${ThreadCpuWarmupSec}s warmup) ---"
            # NOT Tee-Object: on Windows PowerShell 5.1 it writes UTF-16, and the
            # saved file then reads as spaced-out garbage in grep/less. The probe
            # emits pure ASCII, so say so.
            $probeOut = & $probe --pid $proc.Id --duration $window --top 20 2>&1
            $probeOut | ForEach-Object { Write-Host $_ }
            $probeOut | Out-File -FilePath $threadLog -Encoding ascii
            Write-Host "  saved: $threadLog"
            Start-Sleep -Seconds 2
            $sampled = $true
        }
    }
    if (-not $sampled) { Start-Sleep -Seconds $DurationSec }
    # Wall tile windows ignore WM_CLOSE. env_logger writes stderr unbuffered, so a
    # force-kill does not lose log output.
    if (!$proc.HasExited) { Stop-Process -Id $proc.Id -Force }
    Start-Sleep -Seconds 2
} else {
    if ($ThreadCpu) {
        Write-Warning "-ThreadCpu needs -DurationSec to know when to sample. Run the probe by hand instead: engine\thread_cpu_probe.exe --duration 20"
    }
    Write-Host "Running. To stop: Ctrl+C then  Stop-Process -Name winit_wall -Force"
    Wait-Process -Id $proc.Id
}

if (!(Test-Path $LogPath)) { exit 0 }

function CountOf($pattern) {
    (Select-String -Path $LogPath -Pattern $pattern -SimpleMatch -EA SilentlyContinue | Measure-Object).Count
}

$d3d11   = CountOf "profile_id="
$direct  = CountOf "direct file playback"
$dcompMk = CountOf "[dcomp-native] engaged"
$tileMk  = CountOf "[wr-tile-size] picture tile size override"
$panic   = CountOf "panicked"
$egl     = CountOf "EGLImage"
$fanout  = CountOf "fan-out is BROKEN"

Write-Host ""
Write-Host "markers: d3d11=$d3d11/$tiles direct_file=$direct/$tiles dcomp_engaged=$dcompMk wr_tile_override=$tileMk panics=$panic"

# --- per-tile render rate: the wall runs at the SLOWEST tile, not the average ---
$ends = Select-String -Path $LogPath -Pattern "Wall render end: painter PainterId\((\d+)\)" -EA SilentlyContinue
$perPainter = @{}
foreach ($m in $ends) { $p = $m.Matches[0].Groups[1].Value; $perPainter[$p] = 1 + $perPainter[$p] }
if ($perPainter.Count -gt 0 -and $DurationSec -gt 0) {
    $line = ($perPainter.Keys | Sort-Object | ForEach-Object {
        "P{0}={1:N1}fps" -f $_, ($perPainter[$_] / $DurationSec) }) -join "  "
    Write-Host "tiles  : $line"
    $rates = $perPainter.Values | ForEach-Object { $_ / $DurationSec }
    $spread = ($rates | Measure-Object -Maximum).Maximum - ($rates | Measure-Object -Minimum).Minimum
    if ($spread -gt 2.0) {
        Write-Warning ("Tile rates differ by {0:N1} fps -- the wall is NOT coherent. A config that raises the average while splitting the tiles is a failure, not a win." -f $spread)
    }
}

# --- multi-GPU health ---
Write-Host ""
Write-Host "WALLDIAG -- ring owner devices (expect one per GPU that shows video):"
Select-String -Path $LogPath -Pattern "device=0x[0-9a-f]+" -AllMatches -EA SilentlyContinue |
    ForEach-Object { $_.Matches | ForEach-Object { $_.Value } } |
    Group-Object | Sort-Object Name | ForEach-Object { "  {0}  x{1}" -f $_.Name, $_.Count } | Write-Host
Write-Host "WALLDIAG -- per-painter wrap outcome (expect OK once per painter, no FAIL):"
Select-String -Path $LogPath -Pattern "WALLDIAG wrap (OK|FAIL)" -EA SilentlyContinue |
    ForEach-Object { "  " + ($_.Line -replace "^.*WALLDIAG", "WALLDIAG") } | Write-Host
Write-Host "EGLImage wrap failures: $egl   fan-out BROKEN warnings: $fanout"

# --- D3D11PROF: where the per-frame media time goes (only with -D3d11Profile) ---
$prof = Select-String -Path $LogPath -Pattern "D3D11PROF id=\d+ over=\S+ total=([\d.]+) claim=([\d.]+) copy=([\d.]+) publish=([\d.]+)" -EA SilentlyContinue
if ($prof.Count -gt 0) {
    $tot = @(); $cop = @(); $cla = @(); $pub = @()
    foreach ($m in $prof) {
        $g = $m.Matches[0].Groups
        $tot += [double]$g[1].Value; $cla += [double]$g[2].Value
        $cop += [double]$g[3].Value; $pub += [double]$g[4].Value
    }
    function Stat($a, $name) {
        $sorted = $a | Sort-Object
        $p50 = $sorted[[int]($sorted.Count * 0.5)]
        $p90 = $sorted[[int]($sorted.Count * 0.9)]
        $avg = ($a | Measure-Object -Average).Average
        "  {0,-8} p50={1,7:N2}ms  p90={2,7:N2}ms  avg={3,7:N2}ms" -f $name, $p50, $p90, $avg
    }
    Write-Host ""
    Write-Host "D3D11PROF -- per-frame media stage timings (n=$($prof.Count) samples):"
    Write-Host (Stat $tot "total")
    Write-Host (Stat $cla "claim")
    Write-Host (Stat $cop "copy")
    Write-Host (Stat $pub "publish")
    $copyShare = 100 * (($cop | Measure-Object -Sum).Sum) / [math]::Max((($tot | Measure-Object -Sum).Sum), 0.001)
    Write-Host ("  copy is {0:N0}% of total media stage time" -f $copyShare)
    Write-Host "  (copy = CPU memcpy of decoded planes into GPU-mapped memory)"
}

if ($fanout -gt 0) { Write-Warning "GPU fan-out is broken -- tiles share one D3D11 device. See the warning text in the log." }
if ($egl -gt 0)    { Write-Warning "Some tiles could not wrap video textures ($egl occurrences) -- those tiles show green." }
exit 0
