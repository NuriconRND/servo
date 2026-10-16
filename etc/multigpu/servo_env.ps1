$ErrorActionPreference = "Stop"

$servoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path

# ★툴체인은 **본 체크아웃**에만 있다.★ 이 저장소는 git 워크트리로도 쓰이는데,
# `.rustup` 과 `.servo\cargo-home` 은 본 체크아웃 하나에만 두고 워크트리들이 공유한다.
# 그래서 `$servoRoot` 아래에 없으면 `.git` 이 가리키는 곳에서 본 체크아웃을 찾아
# 거기를 쓴다 -- 워크트리의 `.git` 은 디렉터리가 아니라
# `gitdir: <본체크아웃>\.git\worktrees\<이름>` 한 줄이 든 ***파일***이다.
# 이 해석이 없으면 워크트리에서 소싱했을 때 `cargo` 자체를 못 찾는다.
$toolRoot = $servoRoot
if (-not (Test-Path (Join-Path $toolRoot ".rustup\toolchains"))) {
    $gitFile = Join-Path $servoRoot ".git"
    if (Test-Path $gitFile -PathType Leaf) {
        $gitDir = (Get-Content $gitFile -TotalCount 1) -replace '^gitdir:\s*', ''
        # <본체크아웃>\.git\worktrees\<이름> 에서 세 단계 올라가면 본 체크아웃이다.
        $candidate = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $gitDir))
        if ($candidate -and (Test-Path (Join-Path $candidate ".rustup\toolchains"))) {
            $toolRoot = (Resolve-Path $candidate).Path
        }
    }
}
$rustToolchainBin = Join-Path $toolRoot ".rustup\toolchains\1.95.0-x86_64-pc-windows-msvc\bin"
$llvmBin = "C:\Program Files\LLVM\bin"
$vsCmakeBin = "C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"

$env:PYTHONUTF8 = "1"
$env:PYTHONIOENCODING = "utf-8"
$env:UV_CACHE_DIR = Join-Path $toolRoot ".servo\uv-cache"
$env:RUSTUP_HOME = Join-Path $toolRoot ".rustup"
$env:CARGO_HOME = Join-Path $toolRoot ".servo\cargo-home"
# GStreamer. `media-gstreamer` 를 켜고 빌드하면 glib-sys/gobject-sys 의 빌드 스크립트가
# `pkg-config` 를 부르는데, 그것을 찾는 것도 찾은 뒤 어디를 뒤지게 할지도 여기서 정해야 한다.
#
# 해석 순서는 Servo 의 `gstreamer_root()` 와 같게 맞춘다 -- 부트스트랩 사본 -> 환경 변수 ->
# `C:\gstreamer`. 판정은 Servo 와 똑같이 `bin\ffi-7.dll` 의 존재뿐이고 버전은 보지 않는다.
$gstRoot =
    if (Test-Path (Join-Path $servoRoot "target\dependencies\gstreamer\1.0\msvc_x86_64\bin\ffi-7.dll")) {
        Join-Path $servoRoot "target\dependencies\gstreamer\1.0\msvc_x86_64"
    } elseif ($env:GSTREAMER_1_0_ROOT_MSVC_X86_64 -and
              (Test-Path (Join-Path $env:GSTREAMER_1_0_ROOT_MSVC_X86_64 "bin\ffi-7.dll"))) {
        $env:GSTREAMER_1_0_ROOT_MSVC_X86_64
    } elseif (Test-Path "C:\gstreamer\1.0\msvc_x86_64\bin\ffi-7.dll") {
        "C:\gstreamer\1.0\msvc_x86_64"
    } else { $null }

$env:PATH = @(
    Join-Path $servoRoot "target\debug"
    $llvmBin
    $vsCmakeBin
    $rustToolchainBin
    # ★`pkg-config.exe` 가 여기 있다.★ 이것이 빠져 있으면 `PKG_CONFIG_PATH` 를 아무리
    # 잘 세워도 소용이 없다 -- 실행 파일 자체를 못 찾아 glib-sys 가
    # "Could not run `pkg-config`" 로 죽고, `media-gstreamer` 릴리스 빌드가 통째로 막힌다.
    # 조건이 거짓이면 배열에 아무것도 들어가지 않는다 -- 빈 칸이 남지 않는다.
    if ($gstRoot) { Join-Path $gstRoot "bin" }
    $env:PATH
) -join ";"

if ($gstRoot) {
    # ★PKG_CONFIG_PATH 는 슬래시를 **앞으로** 써야 한다.★ Servo 는 크로스 타깃(OHOS)에만
    # 이 변수를 세우므로 호스트 Windows 에서는 pkg-config 가 컴파일된 기본 검색 경로로
    # 떨어진다. 이 GStreamer 번들은 그 기본 경로를 ***역슬래시***로 담고 있어서,
    # pkg-config 가 내놓는 `-L<...>\lib` 의 역슬래시가 lld-link 에 닿을 즈음 이스케이프로
    # 먹힌다 -- `.lib` 가 멀쩡히 있는데도 `could not open 'gstreamer-1.0.lib'` 가 난다.
    # 앞슬래시로 준 경로가 먼저 검색되고 링커까지 살아남는다.
    $pcLib = (Join-Path $gstRoot "lib\pkgconfig") -replace '\\', '/'
    $pcShare = (Join-Path $gstRoot "share\pkgconfig") -replace '\\', '/'
    $env:PKG_CONFIG_PATH = "$pcLib;$pcShare"
}

Write-Host "Servo environment configured:"
Write-Host "  SERVO_ROOT=$servoRoot"
Write-Host "  TOOL_ROOT=$toolRoot"
Write-Host "  RUSTUP_HOME=$env:RUSTUP_HOME"
Write-Host "  CARGO_HOME=$env:CARGO_HOME"
Write-Host "  UV_CACHE_DIR=$env:UV_CACHE_DIR"
Write-Host "  GSTREAMER_ROOT=$gstRoot"
Write-Host "  PKG_CONFIG_PATH=$env:PKG_CONFIG_PATH"
