# 애니메이션 타임라인 평활 A/B -- 네 번 돌린다 (설계 문서 51.4)
#
# ★판정 지표는 dx_px 다.★ 프로브가 화면의 실제 변위를 직접 재고, 그 균일함이 저더의
# 정의다. TIMELINESTEP 의 built_at_gap_ms 는 ***이 변경으로 움직이지 않는다*** --
# built_at 은 레이아웃이 디스플레이 리스트를 만든 시각이고, 평활은 그 리스트에 담기는
# 값만 바꾸기 때문이다. 50 에서 built_at 을 본 것은 원인을 짚기 위한 것이었지, 고쳤는지
# 보는 지표가 아니다. built_at 이 안 움직인다고 해서 평활이 안 걸린 것이 아니다.
#
# ★평활이 실제로 켜졌는지는 로그 한 줄로 확인한다:★
#   "animation timeline smoothing enabled at 60Hz"
# 이 줄이 없으면 pref 가 안 먹은 것이다. 범위를 벗어난 값은 경고를 남긴다.
#
#                      (1) 끔      (2) 켬          (3) 대조군    (4) 대조군+켬
#   divisor               1           1               4             4
#   dx_px 이탈         약 3.5%     ★1.8% 이하★      1.81%        ★1.81% 이하★
#   dx_px p50            38.4        38.4            38.4          38.4
#   육안                 저더      저더 없음        저더 없음     저더 없음
#
# (2)에서 dx 이탈이 (3) 수준으로 내려가면 성공이다.
#
# ★(4)가 리뷰에서 추가된 칸이다.★ divisor=4 는 50 이 "이미 깨끗하다" 고 측정한 조건이다.
# 거기에 평활을 켜서 ***나빠지지 않는지*** 본다. 평활된 값과 페인트 쪽이 쓰는 built_at
# 타임스탬프가 가리키는 순간이 최대 한 주기 어긋나므로, 페인트 쪽이 실제로 외삽하는
# divisor=4 에서는 리스트가 도착할 때마다 그 잔차만큼 튈 수 있다. (4)가 (3)보다 나쁘면
# 평활은 divisor=1 전용으로 묶어야 한다.
#
# ★속도도 본다.★ dx_px 의 p50 이 넷 다 38.4 여야 한다. 어느 하나만 낮으면 타임라인이
# 뒤처진 채 고정된 것이다 -- 그건 드리프트 테스트가 놓친 경우이고, 바로 알려 달라.

[CmdletBinding()]
param(
    # 각 실행의 길이(초). 짧게 돌려보려면 줄인다.
    [int] $DurationSec = 120,

    # 평활에 줄 예상 틱 주파수. 보통 -RefreshHz 와 같은 값이다.
    [ValidateRange(1, 1000)]
    [int] $SmoothingHz = 60
)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

$base = @(
    "-Layout", "wall_layout.multigpu.json", "-Serve",
    "-Url", "wall_anim_jitter_probe.html",
    "-IgnoreCertErrors", "-PageFeatures",
    "-DComp", "surface", "-RefreshHz", "60", "-DwmAlign", "14", "-CommitAlign", "8",
    "-SampleLead", "1", "-DirectRender", "-DcompParallelCommit", "-PacingThread",
    "-ParallelTiles", "-PipelineMode", "uridecodebin3", "-DecoderThreads", "1",
    "-SinkPacing", "thread", "-NumaNode", "1", "-NoSyncGroup",
    "-DurationSec", "$DurationSec", "-PresentCadence", "-DcompBindProf", "-AnimStepSeq"
)
$prefs = @(
    "dom_webgl2_enabled=true", "dom_webgpu_enabled=true",
    "gfx_precache_shaders=true", "media_wrap_init_max_per_frame=2"
)
# 저더가 나오는 조건. 틱이 60Hz 로 오고, 거기서 도착 지연의 변동이 값에 그대로 실린다.
$judder = $prefs + "gfx_paint_side_animation_tick_divisor=1"

Write-Host "`n(1/4) divisor=1, smoothing OFF           -> smooth_off.err.log" -ForegroundColor Cyan
& .\run_wall_dist.ps1 @base -Pref $judder -LogPath smooth_off.err.log

Write-Host "`n(2/4) divisor=1, smoothing ON ($SmoothingHz Hz) -> smooth_on.err.log" -ForegroundColor Cyan
& .\run_wall_dist.ps1 @base -Pref $judder -TimelineSmoothing $SmoothingHz -LogPath smooth_on.err.log

Write-Host "`n(3/4) divisor=4 (default), smoothing OFF -> smooth_control.err.log" -ForegroundColor Cyan
& .\run_wall_dist.ps1 @base -Pref $prefs -LogPath smooth_control.err.log

Write-Host "`n(4/4) divisor=4 (default), smoothing ON  -> smooth_control_on.err.log" -ForegroundColor Cyan
& .\run_wall_dist.ps1 @base -Pref $prefs -TimelineSmoothing $SmoothingHz -LogPath smooth_control_on.err.log

Write-Host "`nDone. Hand over the four .err.log files (recorded in design doc 51.4)." -ForegroundColor Green
