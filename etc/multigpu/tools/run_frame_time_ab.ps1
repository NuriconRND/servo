# Frame-time A/B -- six runs. ASCII only (PowerShell 5.1 reads a BOM-less .ps1 as the
# system ANSI codepage) and hashtable splatting (an array splat passes every element
# positionally, so "-Layout" would become a value instead of a parameter name).
#
# JUDGE ON dx_px spread from ANIMSTEP. TIMELINESTEP is NO LONGER valid here: built_at is
# now the pacing clock, so its gaps are even by construction.
#
#   run  divisor  frame_time  SampleLead   what it answers
#    1      1        off          on       judder baseline
#    2      1        ON           on       *** does this change fix it ***
#    3      4        off          on       already-clean control
#    4      4        ON           on       does it make the clean side worse
#    5      1        off          OFF      SampleLead's existing benefit
#    6      1        ON           OFF      is SampleLead still needed once the anchor is fixed
#
# dx_px p50 must be 38.4 in all six. If one is lower, the timeline fell behind and stuck.

[CmdletBinding()]
param([int] $DurationSec = 120)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

$base = @{
    Layout = "wall_layout.multigpu.json"; Serve = $true
    Url = "wall_anim_jitter_probe.html"
    IgnoreCertErrors = $true; PageFeatures = $true
    DComp = "surface"; RefreshHz = 60; DwmAlign = 14; CommitAlign = 8
    DirectRender = $true; DcompParallelCommit = $true; PacingThread = $true
    ParallelTiles = $true; PipelineMode = "uridecodebin3"; DecoderThreads = 1
    SinkPacing = "thread"; NumaNode = "1"; NoSyncGroup = $true
    DurationSec = $DurationSec; PresentCadence = $true; DcompBindProf = $true
    AnimStepSeq = $true
}
$prefs = @("dom_webgl2_enabled=true", "dom_webgpu_enabled=true",
           "gfx_precache_shaders=true", "media_wrap_init_max_per_frame=2")
$judder = $prefs + "gfx_paint_side_animation_tick_divisor=1"

$runs = @(
    @{ N = "1/6 div=1 ft=off lead=on";  Pref = $judder; Ft = "off"; Lead = 1;  Log = "ft_1_off.err.log" }
    @{ N = "2/6 div=1 ft=ON  lead=on";  Pref = $judder; Ft = "on";  Lead = 1;  Log = "ft_2_on.err.log" }
    @{ N = "3/6 div=4 ft=off lead=on";  Pref = $prefs;  Ft = "off"; Lead = 1;  Log = "ft_3_ctrl.err.log" }
    @{ N = "4/6 div=4 ft=ON  lead=on";  Pref = $prefs;  Ft = "on";  Lead = 1;  Log = "ft_4_ctrl_on.err.log" }
    @{ N = "5/6 div=1 ft=off lead=OFF"; Pref = $judder; Ft = "off"; Lead = -1; Log = "ft_5_nolead.err.log" }
    @{ N = "6/6 div=1 ft=ON  lead=OFF"; Pref = $judder; Ft = "on";  Lead = -1; Log = "ft_6_on_nolead.err.log" }
)

foreach ($run in $runs) {
    Write-Host "`n$($run.N)  -> $($run.Log)" -ForegroundColor Cyan
    & .\run_wall_dist.ps1 @base -Pref $run.Pref -FrameTime $run.Ft -SampleLead $run.Lead -LogPath $run.Log
}

Write-Host "`nDone. Hand over the six .err.log files." -ForegroundColor Green
