# NDEBUG ANGLE 검증: assert 소멸 + 월 렌더 확인
$ErrorActionPreference='Continue'
$root='D:\2_TechReview\20260606_multigpu_browser\servo'
$exe=Join-Path $root 'target\release\servoshell.exe'
$layout=Join-Path $root 'etc\multigpu\config\wall_layout.example_2x1_dualgpu.json'
$logdir=Join-Path $root 'target\multigpu_logs'

function RunCase($mode,$url,$tag,$secs){
  Get-Process servoshell -ErrorAction SilentlyContinue | Stop-Process -Force; Start-Sleep -Milliseconds 400
  $out=Join-Path $logdir $tag
  if($mode -eq 'wall'){
    $args=@('--wall-layout',$layout,'--wall-all-tiles','--pref','dom_webgl2_enabled=true',$url)
  } else {
    $args=@('--pref','dom_webgl2_enabled=true',$url)
  }
  $p=Start-Process -FilePath $exe -ArgumentList $args -WorkingDirectory $root -RedirectStandardOutput "$out.out.log" -RedirectStandardError "$out.err.log" -PassThru
  Start-Sleep -Seconds $secs
  $alive = -not $p.HasExited
  $asrt = (Select-String -Path "$out.err.log" -Pattern 'Assert failed|StateManager11' -ErrorAction SilentlyContinue | Measure-Object).Count
  $surv = (Select-String -Path "$out.out.log","$out.err.log" -Pattern 'survived 120 frames' -ErrorAction SilentlyContinue | Measure-Object).Count
  $fan  = (Select-String -Path "$out.err.log" -Pattern 'fan-out to paint targets' -ErrorAction SilentlyContinue | Measure-Object).Count
  $present = (Select-String -Path "$out.err.log" -Pattern 'Wall window present|present' -ErrorAction SilentlyContinue | Measure-Object).Count
  Write-Host ("[{0}] alive={1} assert={2} survived120={3} fanout={4} present={5}" -f $tag,$alive,$asrt,$surv,$fan,$present)
  if($alive){ Get-Process servoshell -ErrorAction SilentlyContinue | Stop-Process -Force }
}

$tri='file:///D:/2_TechReview/20260606_multigpu_browser/servo/tests/html/wall_webgl2_min_triangle.html'
$flo='file:///D:/2_TechReview/20260606_multigpu_browser/servo/tests/html/wall_webgl2_float_fbo.html'
$kf='https://threejs.org/examples/webgl_animation_keyframes.html'

Write-Host "=== 단일 창(assert 소멸 확인) ==="
RunCase single $tri 'ND_single_triangle' 10
RunCase single $flo 'ND_single_floatfbo' 10
RunCase single $kf  'ND_single_keyframes' 16
Write-Host "=== 듀얼 GPU 월(최종 목표) ==="
RunCase wall   $tri 'ND_wall_triangle' 12
RunCase wall   $kf  'ND_wall_keyframes' 22
Write-Host "done"
