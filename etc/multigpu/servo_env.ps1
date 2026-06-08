$ErrorActionPreference = "Stop"

$servoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$rustToolchainBin = Join-Path $servoRoot ".rustup\toolchains\1.95.0-x86_64-pc-windows-msvc\bin"
$llvmBin = "C:\Program Files\LLVM\bin"
$vsCmakeBin = "C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"

$env:PYTHONUTF8 = "1"
$env:PYTHONIOENCODING = "utf-8"
$env:UV_CACHE_DIR = Join-Path $servoRoot ".servo\uv-cache"
$env:RUSTUP_HOME = Join-Path $servoRoot ".rustup"
$env:CARGO_HOME = Join-Path $servoRoot ".servo\cargo-home"
$env:PATH = @(
    Join-Path $servoRoot "target\debug"
    $llvmBin
    $vsCmakeBin
    $rustToolchainBin
    $env:PATH
) -join ";"

Write-Host "Servo environment configured:"
Write-Host "  SERVO_ROOT=$servoRoot"
Write-Host "  RUSTUP_HOME=$env:RUSTUP_HOME"
Write-Host "  CARGO_HOME=$env:CARGO_HOME"
Write-Host "  UV_CACHE_DIR=$env:UV_CACHE_DIR"
