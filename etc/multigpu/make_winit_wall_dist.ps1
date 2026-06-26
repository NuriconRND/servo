# Assemble a portable distribution of the `winit_wall` multi-GPU wall test harness.
#
# Produces a self-contained folder (exe + all runtime DLLs + probe pages + test_media
# + wall-layout configs) that can be copied to another Windows machine and run without
# installing GStreamer / VC++ redist (resources are baked into the exe; the custom
# GStreamer 1.26.8 DLLs + plugins and ANGLE are bundled beside the exe).
#
# The probe HTML pages hardcode an absolute `file:///.../servo/test_media/...` base to
# the build machine; this script rewrites those COPIES to a relative `test_media/` path
# and places test_media next to the pages, so the bundle is path-independent.
#
# Usage (from anywhere; dot-sourcing not required):
#   pwsh .\etc\multigpu\make_winit_wall_dist.ps1            # assemble (exe must exist)
#   pwsh .\etc\multigpu\make_winit_wall_dist.ps1 -Build     # build the exe first
#   pwsh .\etc\multigpu\make_winit_wall_dist.ps1 -OutDir D:\winit_wall_kit
#
# Build prerequisite (only with -Build): a working Servo build env. The script
# dot-sources scripts\servo_env.ps1 and runs the example cargo build.

[CmdletBinding()]
param(
    [string]$OutDir,
    [switch]$Build,
    # Bundle d3dcompiler_47.dll from System32 (ANGLE needs it; it is in-box on Win10/11,
    # but bundling makes the kit bulletproof on minimal images).
    [bool]$IncludeD3DCompiler = $true
)

$ErrorActionPreference = "Stop"

# Repo layout: this script lives at <servo>/etc/multigpu/.
$servoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$workspaceRoot = (Resolve-Path (Join-Path $servoRoot "..")).Path
if (-not $OutDir) { $OutDir = Join-Path $servoRoot "target\winit_wall_dist" }

$releaseDir = Join-Path $servoRoot "target\release"
$exeSrc = Join-Path $releaseDir "examples\winit_wall.exe"
$features = "media-gstreamer,no-wgl"

function Section($msg) { Write-Host "==== $msg ====" -ForegroundColor Cyan }

# ---- 1. (optional) build -------------------------------------------------------
if ($Build) {
    Section "Building winit_wall.exe (release, $features)"
    . (Join-Path $servoRoot "..\scripts\servo_env.ps1")
    Push-Location $servoRoot
    try {
        & cargo build --release -p servo --example winit_wall --features $features -j 8
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    } finally { Pop-Location }
}

if (-not (Test-Path $exeSrc)) {
    throw "winit_wall.exe not found at $exeSrc. Build it first (re-run with -Build, or: " +
          "cargo build --release -p servo --example winit_wall --features $features)."
}

# ---- 2. fresh output dir -------------------------------------------------------
Section "Output: $OutDir"
if (Test-Path $OutDir) { Remove-Item -Recurse -Force $OutDir }
$pagesDir = Join-Path $OutDir "pages"
$configDir = Join-Path $OutDir "config"
$mediaDir = Join-Path $pagesDir "test_media"
New-Item -ItemType Directory -Force -Path $OutDir, $pagesDir, $configDir, $mediaDir | Out-Null

# ---- 3. exe + runtime DLLs -----------------------------------------------------
Section "Copying exe + runtime DLLs"
Copy-Item $exeSrc (Join-Path $OutDir "winit_wall.exe")
# All DLLs that mach copied next to servoshell: GStreamer core/plugins (incl custom
# gstwinks/gstd3d11), ANGLE (libEGL/libGLESv2), FFmpeg, OpenSSL, VC runtime, etc.
$dlls = Get-ChildItem (Join-Path $releaseDir "*.dll") -ErrorAction SilentlyContinue
if (-not $dlls) { throw "No DLLs in $releaseDir — build/package the project first (mach build --release)." }
$dlls | Copy-Item -Destination $OutDir
Write-Host "  $($dlls.Count) DLLs (from target\release)"

# Copy the COMPLETE set of DLLs from the GStreamer root's bin/ (all base libraries +
# all third-party dependencies). mach's curated GSTREAMER_*_LIBS lists can lag the
# actual plugin dependency set — e.g. 1.26.8's gstd3d11/gstmediafoundation need the
# split-out gst{d3dshader,dxva,winrt}-1.0-0, and gstsrtp needs srtp2-1.dll — and any
# omission makes a plugin fail to load on a machine without a system GStreamer on PATH
# ("ErrorLoadingPlugins"). Copying the whole bin/ is the simplest way to guarantee
# every registered plugin's transitive deps travel with the kit. (ANGLE libEGL/
# libGLESv2 come from target\release and are not in bin/, so they are untouched.)
$gstRoot = @(
    (Join-Path $servoRoot "target\dependencies\gstreamer\1.0\msvc_x86_64"),
    $env:GSTREAMER_1_0_ROOT_MSVC_X86_64,
    "C:\gstreamer\1.0\msvc_x86_64"
) | Where-Object { $_ -and (Test-Path (Join-Path $_ "bin\ffi-7.dll")) } | Select-Object -First 1
if ($gstRoot) {
    $binDlls = Get-ChildItem (Join-Path $gstRoot "bin\*.dll") -ErrorAction SilentlyContinue
    $binDlls | Copy-Item -Destination $OutDir -Force
    Write-Host "  + $($binDlls.Count) GStreamer bin DLLs (complete base + third-party deps, from $gstRoot)"
} else {
    Write-Warning "GStreamer root not found; relying on target\release DLLs only (plugins may miss deps)."
}

if ($IncludeD3DCompiler) {
    $d3dc = "C:\Windows\System32\d3dcompiler_47.dll"
    if (Test-Path $d3dc) { Copy-Item $d3dc $OutDir; Write-Host "  + d3dcompiler_47.dll" }
    else { Write-Warning "d3dcompiler_47.dll not found in System32; ANGLE relies on the target having it." }
}

# ---- 4. wall-layout configs ----------------------------------------------------
Section "Copying wall-layout configs"
$layoutSources = @(
    (Join-Path $servoRoot "etc\multigpu\config"),  # in-repo example layouts
    (Join-Path $workspaceRoot "config")            # dev-machine local_* layouts (if present)
)
$copiedLayouts = 0
foreach ($src in $layoutSources) {
    if (-not (Test-Path $src)) { continue }
    Get-ChildItem (Join-Path $src "wall_layout*.json") -ErrorAction SilentlyContinue | ForEach-Object {
        Copy-Item $_.FullName (Join-Path $configDir $_.Name) -Force
        $copiedLayouts++
    }
}
Write-Host "  $copiedLayouts layout(s)"

# ---- 5. probe pages (rewrite absolute test_media path -> relative) -------------
Section "Copying probe pages (rewriting test_media paths to relative)"
# The HTML hardcodes file:///<servoRoot>/test_media/ ; make it relative so the bundle
# is location-independent (test_media sits next to the pages).
$absPrefix = "file:///" + ($servoRoot -replace '\\', '/') + "/test_media/"
$htmlDir = Join-Path $servoRoot "tests\html"
$pages = Get-ChildItem (Join-Path $htmlDir "multigpu_*.html") -ErrorAction SilentlyContinue
$rewritten = 0
foreach ($page in $pages) {
    $text = Get-Content $page.FullName -Raw
    $new = $text -replace [regex]::Escape($absPrefix), "test_media/"
    if ($new -ne $text) { $rewritten++ }
    # Use UTF-8 without BOM.
    [System.IO.File]::WriteAllText((Join-Path $pagesDir $page.Name), $new, (New-Object System.Text.UTF8Encoding $false))
}
Write-Host "  $($pages.Count) page(s), $rewritten with rewritten media paths"

# ---- 6. test_media -------------------------------------------------------------
Section "Copying test_media"
$mediaSrc = Join-Path $servoRoot "test_media"
if (Test-Path $mediaSrc) {
    Copy-Item (Join-Path $mediaSrc "*") $mediaDir -Recurse -Force
    $mediaCount = (Get-ChildItem $mediaDir -File -Recurse).Count
    Write-Host "  $mediaCount file(s)"
} else {
    Write-Warning "test_media not found at $mediaSrc; media-file probes won't have assets."
}

# ---- 7. launcher + readme ------------------------------------------------------
Section "Writing run.bat + README.txt"
@'
@echo off
REM Run winit_wall from this kit. DLLs sit next to the exe; cd here so relative
REM page/config paths resolve. Pass the same flags as servoshell, e.g.:
REM   run.bat --wall-layout config\wall_layout.local_2x1.json --wall-all-tiles ^
REM           --pref dom_webrtc_enabled=true pages\multigpu_wall_sync_probe.html
cd /d "%~dp0"
winit_wall.exe %*
'@ | Set-Content -Path (Join-Path $OutDir "run.bat") -Encoding ascii

@"
winit_wall — portable multi-GPU wall test kit
=============================================

Self-contained: GStreamer (custom 1.26.8 incl. ksvideosrc) + ANGLE + VC runtime DLLs
are bundled next to winit_wall.exe; Servo resources are baked into the exe. No GStreamer
or VC++ redist install needed on the target.

Run (from this folder):
  run.bat --wall-layout config\wall_layout.local_2x1.json --wall-all-tiles ^
          --pref dom_webrtc_enabled=true pages\multigpu_wall_sync_probe.html

Flags (same as servoshell):
  --wall-layout <config\*.json>   wall layout (tile rects + spatial `display` index)
  --wall-all-tiles                one window per tile sharing one WebView
  --wall-tile-index <n>           single-tile preview
  --pref name[=value]             Servo prefs (e.g. dom_webrtc_enabled=true,
                                  dom_screen_capture_enabled=true, media_wall_region_upload=true)
  <page>                          probe page (relative ok), e.g. pages\multigpu_x_image_formats_probe.html

Target prerequisites:
  - Windows 10/11 x64 with a D3D11 GPU + drivers (multi-GPU: the GPUs).
  - d3dcompiler_47.dll: bundled if found; otherwise in-box on Win10/11.
  - capture-card / camera probes need the capture HARDWARE + driver on the target.
  - screen / display capture needs a display.
  - layout JSON tiles use a spatial `display` index (top-left = 0, left->right then
    top->bottom); the GPU is auto-assigned from the adapter driving that display. Legacy
    `monitor` is accepted as an alias and `gpu` is ignored (both warn).

Notes:
  - Pure CSS/WebGL/WebGPU/video-file/image probes are self-contained.
  - This minimal harness does NOT route page console.log to the terminal and does NOT
    remap input coordinates (use servoshell for interactive/console testing).
"@ | Set-Content -Path (Join-Path $OutDir "README.txt") -Encoding utf8

# ---- 8. summary ----------------------------------------------------------------
$sizeMB = [math]::Round(((Get-ChildItem $OutDir -Recurse -File | Measure-Object Length -Sum).Sum / 1MB), 1)
Section "Done"
Write-Host "  $OutDir  ($sizeMB MB)"
Write-Host "  Copy this folder to the target machine and run run.bat."
