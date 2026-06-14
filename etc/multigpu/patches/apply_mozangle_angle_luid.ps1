# Applies the ANGLE LUID display-cache patch to the mozangle crate that the build
# actually compiles, so each wall tile renders/composites on its own physical GPU.
#
# WHY a script (not a normal commit): mozangle is pulled from crates.io and compiled
# from the cargo *registry cache* (outside the git repo), so the edit cannot be tracked
# in this repo. This script re-applies it to whichever cargo registry the build uses.
# For a permanent/committable fix, fork mozangle with this patch and add a
# [patch.crates-io] mozangle = { git = ... } entry to Cargo.toml (see README.md).
#
# Usage:
#   etc\multigpu\patches\apply_mozangle_angle_luid.ps1            # apply only
#   etc\multigpu\patches\apply_mozangle_angle_luid.ps1 -Rebuild   # apply + force ANGLE rebuild + copy DLLs
param([switch]$Rebuild)
$ErrorActionPreference = 'Stop'
$repo  = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)   # ...\servo
$patch = Join-Path $PSScriptRoot 'mozangle-0.5.5-angle-luid-display-cache.patch'
if (-not (Test-Path $patch)) { throw "patch not found: $patch" }

# The build compiles the mozangle under whatever CARGO_HOME resolves to. CARGO_HOME is
# unset by default -> global ~/.cargo. etc\multigpu\servo_env.ps1 sets it to .servo\cargo-home.
# Patch every mozangle-0.5.5 we can find so it works either way.
$cargoHomes = @()
if ($env:CARGO_HOME) { $cargoHomes += $env:CARGO_HOME }
$cargoHomes += (Join-Path $repo '.servo\cargo-home')
$cargoHomes += (Join-Path $env:USERPROFILE '.cargo')

$applied = 0; $alreadyAll = $true
foreach ($ch in ($cargoHomes | Select-Object -Unique)) {
  $srcRoot = Join-Path $ch 'registry\src'
  if (-not (Test-Path $srcRoot)) { continue }
  $moz = Get-ChildItem $srcRoot -Directory -Filter 'index.crates.io-*' -ErrorAction SilentlyContinue |
         ForEach-Object { Join-Path $_.FullName 'mozangle-0.5.5' } |
         Where-Object { Test-Path $_ } | Select-Object -First 1
  if (-not $moz) { continue }
  $disp = Join-Path $moz 'gfx\angle\checkout\src\libANGLE\Display.cpp'
  if (-not (Test-Path $disp)) { continue }
  if (Select-String -Path $disp -Pattern 'luidHigh' -Quiet) {
    Write-Host "[skip] already patched: $moz"
    continue
  }
  $alreadyAll = $false
  Push-Location $moz
  try { & git apply -p1 $patch; if ($LASTEXITCODE -ne 0) { throw "git apply failed (exit $LASTEXITCODE)" } }
  finally { Pop-Location }
  Write-Host "[ok]   patched: $moz"
  $applied++
}
if ($applied -eq 0 -and $alreadyAll) { Write-Host "Nothing to do (all found mozangle trees already patched)." }
elseif ($applied -eq 0) { Write-Warning "No mozangle-0.5.5 tree found to patch under the cargo homes checked." }

if ($Rebuild) {
  Write-Host "Forcing ANGLE rebuild (cc does not rerun-if-changed on bundled .cpp)..."
  Get-ChildItem (Join-Path $repo 'target\release\build') -Directory -Filter 'mozangle-*' -ErrorAction SilentlyContinue | Remove-Item -Recurse -Force
  Get-ChildItem (Join-Path $repo 'target\release\.fingerprint') -Directory -Filter 'mozangle-*' -ErrorAction SilentlyContinue | Remove-Item -Recurse -Force
  Get-ChildItem (Join-Path $repo 'target\release\deps') -Filter '*mozangle*' -ErrorAction SilentlyContinue | Remove-Item -Force
  Remove-Item (Join-Path $repo 'target\release\libGLESv2.dll'),(Join-Path $repo 'target\release\libEGL.dll') -Force -ErrorAction SilentlyContinue
  $env:PATH = "C:\Program Files\LLVM\bin;$env:PATH"
  Push-Location $repo
  try {
    & cmd /c ".\mach.bat build --release"
    # mach builds the DLLs under build\mozangle-*\out but does not always copy them next to the exe.
    $out = Get-ChildItem 'target\release\build' -Directory -Filter 'mozangle-*' |
           ForEach-Object { Join-Path $_.FullName 'out' } |
           Where-Object { Test-Path (Join-Path $_ 'libGLESv2.dll') } | Select-Object -First 1
    if ($out) {
      Copy-Item (Join-Path $out 'libGLESv2.dll') 'target\release\libGLESv2.dll' -Force
      Copy-Item (Join-Path $out 'libEGL.dll')    'target\release\libEGL.dll'    -Force
      Write-Host "Copied fresh ANGLE DLLs next to servoshell.exe."
    }
  } finally { Pop-Location }
}
