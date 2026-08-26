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
#
# Pure ASCII on purpose (a Korean launcher once failed to parse on a test machine that
# decodes with a legacy console codepage).
#
# Usage:
#   etc\multigpu\make_wall_dist.ps1
#   etc\multigpu\make_wall_dist.ps1 -Out D:\WallDist -Force

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
if (!(Test-Path $exe))     { throw "winit_wall.exe not found: $exe  (build it first: cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl --$Profile)" }
if (!(Test-Path $GstRoot)) { throw "GStreamer root not found: $GstRoot" }

# --- 1. ANGLE must be the patched build. Hard-fail rather than ship a one-GPU wall. ---
Write-Host "Checking ANGLE LUID patch state..."
& (Join-Path $PSScriptRoot 'patches\verify_angle_luid.ps1') -Profile $Profile
if ($LASTEXITCODE -ne 0) {
    throw "ANGLE verification failed -- refusing to package. A dist built now would render every tile on one GPU."
}

if ((Test-Path $Out) -and -not $Force) { throw "$Out already exists (use -Force)" }
if (Test-Path $Out) { Remove-Item $Out -Recurse -Force }
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
Copy-Item (Join-Path $repo "tests\Wildlife_FHD30fps_counter_10Mbitrate.mp4") $pages -Force

# --- 5. launcher ---
Copy-Item (Join-Path $PSScriptRoot "run_wall_dist.ps1") $Out -Force
# The decode baseline tool ships too: the wall's cores-per-video number is only
# readable next to this machine's single-thread decode ceiling.
Copy-Item (Join-Path $PSScriptRoot "tools\measure_decode_only.ps1") $Out -Force
# The machine's shape decides how to read every number this dist produces. Processor group
# placement was measured to be the difference between 29 fps and 6 fps on 45 videos (2026-08-26,
# forced with -NumaNode, 6/6), so the group/NUMA/GPU-node facts have to be available ON the test
# machine, not only in the dev worktree.
Copy-Item (Join-Path $PSScriptRoot "tools\probe_machine_topology.ps1") $Out -Force

$dll = (Get-ChildItem (Join-Path $engine "*.dll") | Measure-Object).Count
$size = [math]::Round(((Get-ChildItem $Out -Recurse -Force | Measure-Object -Property Length -Sum).Sum / 1GB), 2)
Write-Host ""
Write-Host ("Packaged: {0}" -f $Out)
Write-Host ("  dll={0}  size={1} GB" -f $dll, $size)
Write-Host ("  angle libGLESv2.dll = {0} bytes (patched build)" -f (Get-Item (Join-Path $engine 'libGLESv2.dll')).Length)
Write-Host ""
Write-Host "Copy the folder to the test machine and run:"
Write-Host "  .\run_wall_dist.ps1 -DurationSec 30"
Write-Host "  .\run_wall_dist.ps1 -Layout wall_layout.multigpu.reversed.json -DurationSec 30"
Write-Host "  .\run_wall_dist.ps1 -DurationSec 40 -ThreadCpu      # where the CPU actually goes"
