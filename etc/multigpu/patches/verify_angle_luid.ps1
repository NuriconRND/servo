# Verifies that the multi-GPU wall will actually render on multiple GPUs, and fixes the
# two things that silently break it.
#
# WHY THIS EXISTS (2026-08-24). A 4-GPU wall was running entirely on ONE GPU for a long
# time and NOTHING reported it: "Selected DXGI adapter index 0/1/2/3" was logged normally,
# there was no warning and no fallback message. The cause was the mozangle ANGLE LUID
# display-cache patch not being applied. Because mozangle is a crates.io dependency built
# from the cargo REGISTRY CACHE (outside this repo), that patch cannot be committed and
# quietly reverts on every new machine / new CARGO_HOME / cache re-extract.
#
# It checks three things, in the order they bite:
#   1. Is the patch applied to the mozangle source the build actually compiles?
#   2. Is target\<profile>\libGLESv2.dll the freshly built (patched) one?
#      cargo NEVER copies it out of build\mozangle-*\out\ -- only mach's packaging step
#      does, and `cargo build --example` has no such step. Measured: an incremental build
#      leaves both files untouched, so a one-time copy stays valid until mozangle is
#      actually rebuilt (cargo clean, forced fingerprint removal, registry re-extract).
#   3. Is the copy next to the example exe (target\<profile>\examples\) also current?
#      mach does not package anything for examples, so that directory is populated by hand
#      and drifts easily. (Checked 2026-08-24: the in-house GStreamer bundle does NOT ship
#      libGLESv2.dll/libEGL.dll, so copying it in does not clobber ANGLE -- an earlier
#      guess that it did was wrong. The directory still drifts for other reasons: it is
#      only ever filled manually.)
#
# This file is pure ASCII on purpose (a Korean launcher once failed to parse on a test
# machine that decodes with a legacy console codepage).
#
# Usage:
#   etc\multigpu\patches\verify_angle_luid.ps1              # check and fix stale copies
#   etc\multigpu\patches\verify_angle_luid.ps1 -Check       # report only, non-zero exit on problems
#   etc\multigpu\patches\verify_angle_luid.ps1 -Profile debug

param(
    [ValidateSet("release", "debug")]
    [string] $Profile = "release",
    # Report only; do not copy anything. Exits 1 if any problem is found.
    [switch] $Check
)

$ErrorActionPreference = "Stop"
# $PSScriptRoot = <worktree>\etc\multigpu\patches  -> THREE levels up is the worktree root.
# (apply_mozangle_angle_luid.ps1 goes up only two and lands on ...\etc -- a latent bug there;
#  it still worked because it also probes CARGO_HOME and %USERPROFILE%\.cargo.)
$repo = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$problems = 0
$fixed = 0

function Fail($msg)  { Write-Warning $msg; $script:problems++ }
function Note($msg)  { Write-Host "  $msg" }

Write-Host "verify_angle_luid: repo=$repo profile=$Profile"

# ---------------------------------------------------------------------------
# 1. Is the ANGLE LUID patch applied to the mozangle the build compiles?
# ---------------------------------------------------------------------------
$cargoHomes = @()
if ($env:CARGO_HOME) { $cargoHomes += $env:CARGO_HOME }
$cargoHomes += (Join-Path $repo '.servo\cargo-home')
$cargoHomes += (Join-Path (Split-Path -Parent $repo) 'servo\.servo\cargo-home')
$cargoHomes += (Join-Path $env:USERPROFILE '.cargo')

$checkedAny = $false
foreach ($ch in ($cargoHomes | Select-Object -Unique)) {
    $disp = Get-ChildItem (Join-Path $ch 'registry\src') -Directory -Filter 'index.crates.io-*' -EA SilentlyContinue |
            ForEach-Object { Join-Path $_.FullName 'mozangle-0.5.5\gfx\angle\checkout\src\libANGLE\Display.cpp' } |
            Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $disp) { continue }
    $checkedAny = $true
    $refs = (Select-String -Path $disp -Pattern 'luidHigh|luidLow' | Measure-Object).Count
    if ($refs -gt 0) {
        Note "[ok]   patch applied ($refs luid refs): $ch"
    } else {
        Fail "ANGLE LUID patch NOT applied in $ch -- every wall tile will render on ONE GPU. Run etc\multigpu\patches\apply_mozangle_angle_luid.ps1, then delete target\$Profile\{build,.fingerprint,deps} entries matching mozangle-* to force an ANGLE rebuild."
    }
}
if (-not $checkedAny) {
    Fail "No mozangle-0.5.5 source tree found under any known CARGO_HOME -- cannot verify the patch."
}

# ---------------------------------------------------------------------------
# 2/3. Are the deployed ANGLE DLLs the freshly built (patched) ones?
# ---------------------------------------------------------------------------
$out = Get-ChildItem (Join-Path $repo "target\$Profile\build") -Directory -Filter 'mozangle-*' -EA SilentlyContinue |
       ForEach-Object { Join-Path $_.FullName 'out' } |
       Where-Object { Test-Path (Join-Path $_ 'libGLESv2.dll') } | Select-Object -First 1

if (-not $out) {
    Fail "No built ANGLE found under target\$Profile\build\mozangle-*\out -- build first."
} else {
    Note "[ok]   built ANGLE: $out"
    $targets = @(
        (Join-Path $repo "target\$Profile"),
        (Join-Path $repo "target\$Profile\examples")
    ) | Where-Object { Test-Path $_ }

    foreach ($dir in $targets) {
        foreach ($name in @('libGLESv2.dll', 'libEGL.dll')) {
            $src = Join-Path $out $name
            $dst = Join-Path $dir $name
            if (-not (Test-Path $src)) { continue }
            $srcHash = (Get-FileHash $src -Algorithm SHA256).Hash
            $srcLen = (Get-Item $src).Length
            if (-not (Test-Path $dst)) {
                if ($Check) { Fail "missing: $dst" }
                else { Copy-Item $src $dst -Force; $fixed++; Note "[fix]  copied (was missing): $dst" }
                continue
            }
            # Compare by HASH, not size: a pre-patch and post-patch build can come out the
            # same size, and then a size check silently misses the stale file.
            $dstHash = (Get-FileHash $dst -Algorithm SHA256).Hash
            $dstLen = (Get-Item $dst).Length
            if ($srcHash -eq $dstHash) {
                Note "[ok]   up to date: $dst ($dstLen bytes)"
            } elseif ($Check) {
                Fail "STALE ANGLE: $dst does not match the built one ($dstLen vs $srcLen bytes). If it predates the LUID patch, every wall tile renders on ONE GPU."
            } else {
                Copy-Item $src $dst -Force
                $fixed++
                Note "[fix]  refreshed: $dst ($dstLen -> $srcLen bytes)"
            }
        }
    }
}

Write-Host ""
if ($problems -gt 0) {
    Write-Warning "verify_angle_luid: $problems problem(s) found."
    exit 1
}
if ($fixed -gt 0) { Write-Host "verify_angle_luid: OK ($fixed file(s) refreshed)." }
else              { Write-Host "verify_angle_luid: OK (nothing to do)." }
# Set the success code EXPLICITLY. Without this, a PowerShell script that just falls off
# the end leaves $LASTEXITCODE at whatever the previous command set -- so a caller doing
# `verify...ps1; if ($LASTEXITCODE -ne 0) {...}` reads a stale failure. Measured 2026-08-24.
exit 0
