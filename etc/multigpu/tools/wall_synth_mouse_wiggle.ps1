# wall_synth_mouse_wiggle.ps1
# Drives synthetic mouse movement over a point so we can test whether the wall page
# receives mousemove events. Uses SetCursorPos to a sequence of DISTINCT points (a small
# circle) — relative SendInput proved unreliable from an external process, while
# SetCursorPos reliably moves the cursor and generates WM_MOUSEMOVE.
# Pair with a probe page (mouse_count_probe.html) and grep the Servo log for 'mouseEv/s='.
#
#   powershell -ExecutionPolicy Bypass -File wall_synth_mouse_wiggle.ps1 -X 960 -Y 540 -DurationSec 8
param(
    [int] $X = 960,
    [int] $Y = 540,
    [int] $DurationSec = 8,
    [int] $Hz = 100,
    [int] $Radius = 40
)
$ErrorActionPreference = "Stop"
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class SI {
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
"@
try { [SI]::SetProcessDPIAware() | Out-Null } catch {}
Write-Host "Moving synthetic cursor in a circle r=$Radius around ($X,$Y) for $DurationSec s at ${Hz}Hz..."
$sleepMs = [int](1000 / [Math]::Max(1, $Hz))
$end = (Get-Date).AddSeconds($DurationSec)
$k = 0
while ((Get-Date) -lt $end) {
    # Distinct point each step => the cursor actually moves => WM_MOUSEMOVE is generated.
    $ang = ($k % 60) * (6.2831853 / 60.0)
    $px = $X + [int]([Math]::Cos($ang) * $Radius)
    $py = $Y + [int]([Math]::Sin($ang) * $Radius)
    [SI]::SetCursorPos($px, $py) | Out-Null
    $k++
    Start-Sleep -Milliseconds $sleepMs
}
Write-Host "Done ($k cursor moves)."
