# Packages this worktree's winit_wall into a self-contained folder that runs on a test
# machine with no dev environment (no Rust, no GStreamer install, no ANGLE).
#
# This is the pref-era wall: engine knobs are --pref, not environment variables.
#
# Three things this script exists to get right, all of which were learned the hard way:
#
#  1. ANGLE must be the LUID-patched build. `cargo build --example` never copies
#     libGLESv2.dll out of target\<profile>\build\mozangle-*\out\, so target\<profile>\
#     can silently hold a pre-patch DLL -- and then EVERY wall tile renders on ONE GPU
#     with no warning anywhere. verify_angle_luid.ps1 is run first and hard-fails here.
#  2. GStreamer plugins are loaded from the EXE's own directory. mach packages them for
#     servoshell but NOT for examples, so they are copied in full here (a curated list
#     has silently missed dependencies before, and the dev box hides it because a system
#     GStreamer sits on PATH).
#  3. The 6x6 page references its source as `../Wildlife_....mp4`, so tests\ layout is
#     preserved under pages\.
#  4. `webgpu` is part of the STANDARD feature set. It is a cargo feature, not a pref, so
#     an engine built without it has no `navigator.gpu` at all and `dom_webgpu_enabled=true`
#     silently does nothing -- exactly the shape of failure as (1). This script does not
#     build, it copies whatever exe it finds, so it checks the exe instead and hard-fails.
#
# Pure ASCII on purpose (a Korean launcher once failed to parse on a test machine that
# decodes with a legacy console codepage).
#
# Usage:
#   etc\multigpu\make_wall_dist.ps1
#   etc\multigpu\make_wall_dist.ps1 -Out D:\WallDist -Force

# ★[CmdletBinding()] 을 빼지 말 것.★ 이것이 없으면 PowerShell 은 모르는 명명
# 파라미터를 **조용히 무시하고 그냥 돈다** -- 오류도 경고도 없다. 이 벽의 A/B 는
# 전부 이 스크립트의 스위치로 설정되므로, 배포본이 오래되면 새 스위치가 말없이
# 사라지고 런은 완벽하게 정상으로 보인다. 실제로 2026-09-17 에 -VsyncPhase 를
# 넘긴 네 번의 실기가 통째로 날아갔다(옛 스크립트 + 기본값으로 네 번 같은 런).
# 엔진 쪽은 시끄럽다 -- 모르는 pref 이름은 set_value 가, 타입 불일치는
# try_into().unwrap() 이 패닉한다. 조용한 구멍은 여기 하나뿐이었다.
[CmdletBinding()]
param(
    [string] $Out = "",
    [string] $GstRoot = "F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64",
    [ValidateSet("release", "debug")]
    [string] $Profile = "release",
    [switch] $Force
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)    # <worktree>
if ($Out -eq "") { $Out = Join-Path $repo "target\wall_dist" }

$exe = Join-Path $repo "target\$Profile\examples\winit_wall.exe"
$features = "media-gstreamer,no-wgl,webgpu"
$buildCmd = "cargo build -p servo --example winit_wall --features $features --$Profile"
if (!(Test-Path $exe))     { throw "winit_wall.exe not found: $exe  (build it first: $buildCmd)" }
if (!(Test-Path $GstRoot)) { throw "GStreamer root not found: $GstRoot" }

# --- 1. ANGLE must be the patched build. Hard-fail rather than ship a one-GPU wall. ---
Write-Host "Checking ANGLE LUID patch state..."
& (Join-Path $PSScriptRoot 'patches\verify_angle_luid.ps1') -Profile $Profile
if ($LASTEXITCODE -ne 0) {
    throw "ANGLE verification failed -- refusing to package. A dist built now would render every tile on one GPU."
}

# --- 1b. WebGPU must be compiled in. Same reason as ANGLE: shipping without it fails silently.
# Two markers, both verified absent from a `media-gstreamer,no-wgl` build and present in a
# `...,webgpu` one: `wgpu_core` (the crate is linked at all) and `GPUAdapter` (the DOM
# bindings the pref actually switches on). findstr reads the 144 MB exe in ~0.1s.
Write-Host "Checking the engine was built with the webgpu feature..."
foreach ($marker in @("wgpu_core", "GPUAdapter")) {
    & findstr.exe /M /C:$marker $exe | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "$exe has no '$marker' -- it was built WITHOUT the webgpu feature. Rebuild with:`n  $buildCmd`nRefusing to package: dom_webgpu_enabled=true cannot enable a feature that is not compiled in, so WebGPU pages would hang with no error."
    }
}

if ((Test-Path $Out) -and -not $Force) { throw "$Out already exists (use -Force)" }
# Empty the folder, do not delete it. ***A shell sitting in the dist -- or anything that ever
# opened a handle to it -- keeps the DIRECTORY locked while its CONTENTS delete fine.*** The
# old line deleted the contents, then threw "the process cannot access the file" on the folder
# itself, leaving a gutted dist that looked packaged. That cost several rounds of confusing
# "the exe is missing" symptoms.
if (Test-Path $Out) {
    Get-ChildItem $Out -Force | ForEach-Object { Remove-Item $_.FullName -Recurse -Force -EA SilentlyContinue }
    $left = @(Get-ChildItem $Out -Recurse -Force -EA SilentlyContinue)
    if ($left.Count) { throw "could not empty $Out ($($left.Count) items left); close anything using it" }
}
New-Item -ItemType Directory -Path $Out -Force | Out-Null

# --- 2. engine\ : exe + every DLL it can possibly need ---
$engine = Join-Path $Out "engine"
New-Item -ItemType Directory -Path $engine -Force | Out-Null
Copy-Item $exe $engine -Force
Copy-Item (Join-Path $repo "target\$Profile\*.dll") $engine -Force        # ANGLE + MSVC runtime
Copy-Item (Join-Path $GstRoot "bin\*.dll")               $engine -Force   # GStreamer libs + deps
Copy-Item (Join-Path $GstRoot "lib\gstreamer-1.0\*.dll") $engine -Force   # GStreamer plugins
# ANGLE last: whatever else was copied must not win over the patched build.
foreach ($n in @('libGLESv2.dll', 'libEGL.dll')) {
    $src = Join-Path $repo "target\$Profile\$n"
    if (Test-Path $src) { Copy-Item $src (Join-Path $engine $n) -Force }
}

# --- 2b. thread_cpu_probe: attributes the running wall's CPU to named threads ---
# Its own tiny crate with its own target dir, so it is built on demand rather than
# assumed present. Without it, run_wall_dist.ps1 -ThreadCpu has nothing to run.
$probeManifest = Join-Path $repo "etc\multigpu\tools\thread_cpu_probe\Cargo.toml"
$probeExe = Join-Path $repo "etc\multigpu\tools\thread_cpu_probe\target\release\thread_cpu_probe.exe"
if (!(Test-Path $probeExe)) {
    Write-Host "Building thread_cpu_probe..."
    & cargo build --release --manifest-path $probeManifest
}
if (Test-Path $probeExe) {
    Copy-Item $probeExe $engine -Force
} else {
    Write-Warning "thread_cpu_probe.exe could not be built -- run_wall_dist.ps1 -ThreadCpu will not work."
}

# --- 3. config\ : wall layouts ---
$cfg = Join-Path $Out "config"
New-Item -ItemType Directory -Path $cfg -Force | Out-Null
Copy-Item (Join-Path $repo "etc\multigpu\config\wall_layout.*.json") $cfg -Force

# --- 4. pages\ : the 6x6 page plus its source, keeping the tests\ shape ---
$pages = Join-Path $Out "pages"
New-Item -ItemType Directory -Path (Join-Path $pages "html") -Force | Out-Null
Copy-Item (Join-Path $repo "tests\html\video_grid_6x6_play.html") (Join-Path $pages "html") -Force
Copy-Item (Join-Path $repo "tests\html\video_grid_6x6_perf.html") (Join-Path $pages "html") -Force -EA SilentlyContinue
# DOM shapes and animation, i.e. everything the video path does not exercise: layout ->
# display list -> WebRender. Ships with the dist because the wall is where seams, the
# overlapPx guard band, and per-tile frame agreement can actually be judged.
Copy-Item (Join-Path $repo "tests\html\multigpu_wall_shape_anim_probe.html") (Join-Path $pages "html") -Force -EA SilentlyContinue
# WebGL2 and WebGPU minimum probes. Both contexts are OFF by default, so these only draw
# with `-Pref dom_webgl2_enabled=true` / `-Pref dom_webgpu_enabled=true`, and the WebGPU one
# additionally needs `-Serve` (it hangs silently on file://) AND an engine built with the
# `webgpu` cargo feature -- the pref alone cannot conjure a feature that is not compiled in.
Copy-Item (Join-Path $repo "tests\html\wall_webgl2_min_triangle.html") (Join-Path $pages "html") -Force -EA SilentlyContinue
# 같은 그림, 캔버스만 640x360 고정. min_triangle 은 innerWidth 를 쓰므로 월에서 백버퍼가
# 11520x4320(199MB)이 된다 — 두 페이지를 나란히 돌려 캔버스 픽셀 수가 DComp Commit 비용을
# 좌우하는지 가른다.
Copy-Item (Join-Path $repo "tests\html\wall_webgl2_small_canvas.html") (Join-Path $pages "html") -Force -EA SilentlyContinue
Copy-Item (Join-Path $repo "tests\html\multigpu_wall_webgpu_min_probe.html") (Join-Path $pages "html") -Force -EA SilentlyContinue
# These two were only ever hand-copied into a dist, so every repackage silently dropped them.
Copy-Item (Join-Path $repo "tests\html\webgl2_ctx_probe.html") (Join-Path $pages "html") -Force -EA SilentlyContinue
Copy-Item (Join-Path $repo "tests\html\multigpu_wall_stress_cases.html") (Join-Path $pages "html") -Force -EA SilentlyContinue
# Capture-card probe. This is the ONLY way to verify the shared capture hub: nothing
# automated covers the getUserMedia -> hub wiring (it needs the card), so the on-hardware
# run with this page is a merge gate, not a follow-up. Needs -PageFeatures for
# dom_webrtc_enabled, and is driven by ?cycles=N from the query string because winit_wall
# never forwards input -- its buttons cannot be clicked on the wall.
Copy-Item (Join-Path $repo "tests\html\multigpu_capture_card_probe.html") (Join-Path $pages "html") -Force -EA SilentlyContinue
# Concurrent-consumer probe: N getUserMedia on ONE port, all live at once. This is
# the case the hub exists for and the one the sequential probe never exercises --
# its `consumers=` never went above 1. Driven by ?multi=N, again no `&`.
Copy-Item (Join-Path $repo "tests\html\multigpu_capture_card_multi_probe.html") (Join-Path $pages "html") -Force -EA SilentlyContinue
# Does this engine run the animation the wall application actually creates? The
# application prefers `Element.animate()` and falls back to a runtime-injected `@keyframes`
# plus an inline `animation` shorthand. This page walks exactly that fallback and says
# through `console` whether the computed value ever moves.
#
# NOTE: this engine now HAS `Element.animate()`, but only behind
# `-Pref dom_web_animations_enabled=true` (default off). With the pref off the application
# still takes the fallback and this page is the probe for it; with the pref on the
# application switches away from the fallback entirely -- see the next page.
Copy-Item (Join-Path $repo "tests\html\wall_css_animation_support_probe.html") (Join-Path $pages "html") -Force -EA SilentlyContinue
# Web Animations smoke test. Run it BOTH ways: with the pref off it must report
# `typeof Element.prototype.animate = undefined` and stop there (that is the proof the
# feature is inert by default); with `-Pref dom_web_animations_enabled=true` all eight
# cases must pass. Case 8 is the regression probe for a use-after-free found in review --
# 20 animate() calls on detached, unreferenced elements. The page carries its own
# pass/fail checklist at the top, so it can be read cold.
Copy-Item (Join-Path $repo "tests\html\web_animations_minimal.html") (Join-Path $pages "html") -Force -EA SilentlyContinue
# Animation jitter probe. One constant-velocity translateX, run through BOTH paths at once
# (Element.animate and CSS @keyframes) on the same screen, over a ruler, with an in-page
# rAF interval distribution. If both rows stutter identically the animation path is not the
# cause -- the two compute their values in completely different code -- and what is left is
# the frame pipeline they share. Prints ANIMJITTER (p50/p95/p99/max, >20ms count) to the log
# once a second, so a run can be judged without watching the wall.
Copy-Item (Join-Path $repo "tests\html\wall_anim_jitter_probe.html") (Join-Path $pages "html") -Force -EA SilentlyContinue
# Does a CSS animation survive a blocked script thread? Blocks its own script thread on a
# timer, the way a content switch does, with a CSS-animated bar next to a rAF counter.
Copy-Item (Join-Path $repo "tests\html\wall_paint_animation_probe.html") (Join-Path $pages "html") -Force -EA SilentlyContinue
Copy-Item (Join-Path $repo "tests\Wildlife_FHD30fps_counter_10Mbitrate.mp4") $pages -Force

# --- 5. launcher ---
Copy-Item (Join-Path $PSScriptRoot "run_wall_dist.ps1") $Out -Force
# The decode baseline tool ships too: the wall's cores-per-video number is only
# readable next to this machine's single-thread decode ceiling.
Copy-Item (Join-Path $PSScriptRoot "tools\measure_decode_only.ps1") $Out -Force
# ...and the http server -Serve drives. WebGPU pages hang on file:// with no error at all,
# so without this the dist can only run the file:///-served pages.
Copy-Item (Join-Path $PSScriptRoot "tools\serve_http.ps1") $Out -Force
# ...and the two GStreamer executables it drives, from the SAME install the wall links against.
#
# ***Without these the baseline measured a different GStreamer than the wall runs.*** The test
# machine has an old 1.22.4 in C:\gstreamer alongside the 1.28.4 the wall uses; with no
# gst-launch in the dist the tool fell through to whichever install it could find and reported
# 1.22.4 -- and a decode baseline taken on another version is not comparable to the wall at all.
# ★루프 변수를 $exe 로 쓰지 말 것.★ 바깥 $exe(빌드된 winit_wall.exe)를 덮어쓴다 --
# 이 루프 뒤에서 $exe 를 쓰는 코드가 조용히 gst 도구 이름을 보게 된다(2026-09-17 에 겪었다).
foreach ($gstExe in @("gst-launch-1.0.exe", "gst-discoverer-1.0.exe")) {
    $src = Join-Path $GstRoot "bin\$gstExe"
    if (Test-Path $src) { Copy-Item $src $engine -Force }
    else { Write-Warning "$gstExe not found in $GstRoot; measure_decode_only.ps1 will fall back to another install" }
}
# The machine's shape decides how to read every number this dist produces. Processor group
# placement was measured to be the difference between 29 fps and 6 fps on 45 videos (2026-08-26,
# forced with -NumaNode, 6/6), so the group/NUMA/GPU-node facts have to be available ON the test
# machine, not only in the dev worktree.
Copy-Item (Join-Path $PSScriptRoot "tools\probe_machine_topology.ps1") $Out -Force

$dll = (Get-ChildItem (Join-Path $engine "*.dll") | Measure-Object).Count
$size = [math]::Round(((Get-ChildItem $Out -Recurse -Force | Measure-Object -Property Length -Sum).Sum / 1GB), 2)
# ★배포본에 신원을 새긴다.★ 2026-09-17 에 같은 실수를 두 번 했다 -- 한 번은 exe 만
# 오래됐고(벽이 돌고 있어 복사가 건너뛴 것으로 보인다) 한 번은 스크립트만 오래됐다.
# 두 경우 다 런은 완벽하게 정상으로 보였고, 어느 빌드가 돌았는지 로그만으로는 알 수
# 없어 실기 여덟 번이 날아갔다. 이 파일이 있으면 run 스크립트가 그것을 찍고,
# console.txt 만 보면 어느 빌드인지 확정된다.
#
# ★git 출력에 2>$null 을 붙이지 말 것.★ PS 5.1 에서 native exe 의 stderr 를 리다이렉트하면
# 줄마다 NativeCommandError 로 감싸이고, 이 파일의 $ErrorActionPreference="Stop" 아래에서는
# 그것이 패키징을 통째로 죽인다. 대신 전체를 try/catch 로 감싼다 -- 스탬프 실패가 배포를
# 막아서는 안 된다.
$shippedExe = Join-Path $engine "winit_wall.exe"
try {
    $stamp = @(
        ("commit   : {0}" -f (& git -C $repo rev-parse --short HEAD)),
        ("branch   : {0}" -f (& git -C $repo rev-parse --abbrev-ref HEAD)),
        ("dirty    : {0}" -f $(if (& git -C $repo status --porcelain) { "yes (uncommitted changes included)" } else { "no" })),
        ("engine   : {0} bytes  ({1:yyyy-MM-dd HH:mm:ss})" -f (Get-Item $shippedExe).Length, (Get-Item $shippedExe).LastWriteTime),
        ("packaged : {0:yyyy-MM-dd HH:mm:ss}" -f (Get-Date))
    )
    $stamp | Set-Content -Path (Join-Path $Out "BUILD.txt") -Encoding utf8
} catch {
    Write-Warning "could not write BUILD.txt: $($_.Exception.Message)"
}
Write-Host ""
Write-Host ("Packaged: {0}" -f $Out)
Write-Host ("  dll={0}  size={1} GB" -f $dll, $size)
Write-Host ("  angle libGLESv2.dll = {0} bytes (patched build)" -f (Get-Item (Join-Path $engine 'libGLESv2.dll')).Length)
Write-Host ""
Write-Host "Copy the folder to the test machine and run:"
Write-Host "  .\run_wall_dist.ps1 -DurationSec 30"
Write-Host "  .\run_wall_dist.ps1 -Layout wall_layout.multigpu.reversed.json -DurationSec 30"
Write-Host "  .\run_wall_dist.ps1 -DurationSec 40 -ThreadCpu      # where the CPU actually goes"
