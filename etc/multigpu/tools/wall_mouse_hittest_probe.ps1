# wall_mouse_hittest_probe.ps1
#
# Diagnose WHY winit delivers ~0 CursorMoved to the borderless-fullscreen wall
# tile windows, WITHOUT rebuilding Servo.
#
# The decisive question: when the cursor sits over a wall tile, which window does
# the OS hit-test to (i.e. which HWND would receive WM_MOUSEMOVE)? If it is NOT
# servoshell's winit window, that window is eating the mouse and we have the root
# cause. If it IS servoshell's window, the loss is inside winit's pumping.
#
# Usage:
#   1. Launch the wall (e.g. etc\multigpu\run_three_retargeting_wall.ps1).
#   2. Run this script. While it samples, move/hold a REAL mouse over a tile.
#      powershell -ExecutionPolicy Bypass -File etc\multigpu\tools\wall_mouse_hittest_probe.ps1 -DurationSec 15
#
param(
    [int]    $DurationSec = 15,
    [int]    $SampleHz    = 5,
    [string] $ProcessName = "servoshell"
)

$ErrorActionPreference = "Stop"

Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public struct POINT { public int X; public int Y; }
public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
public static class W {
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
    [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern IntPtr GetCapture();
    [DllImport("user32.dll")] public static extern IntPtr GetParent(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h, uint flags);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr h, int idx);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int max);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int max);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr parent, EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    public delegate bool EnumProc(IntPtr h, IntPtr l);
}
"@

# Make this probe DPI-aware so GetCursorPos/WindowFromPoint use real physical pixels
# (matches Servo's per-monitor-DPI-aware process; avoids coordinate virtualization).
try { [W]::SetProcessDPIAware() | Out-Null } catch {}

$GWL_STYLE   = -16
$GWL_EXSTYLE = -20
$GA_ROOT     = 2

function Get-ProcName([uint32]$pid_) {
    try { return (Get-Process -Id $pid_ -ErrorAction Stop).ProcessName } catch { return "pid$pid_" }
}

function Describe-Window([IntPtr]$h) {
    if ($h -eq [IntPtr]::Zero) { return "<null>" }
    $cls = New-Object System.Text.StringBuilder 256
    [W]::GetClassName($h, $cls, 256) | Out-Null
    $title = New-Object System.Text.StringBuilder 256
    [W]::GetWindowText($h, $title, 256) | Out-Null
    [uint32]$pid_ = 0
    [W]::GetWindowThreadProcessId($h, [ref]$pid_) | Out-Null
    $r = New-Object RECT
    [W]::GetWindowRect($h, [ref]$r) | Out-Null
    $style   = [W]::GetWindowLong($h, $GWL_STYLE)
    $exstyle = [W]::GetWindowLong($h, $GWL_EXSTYLE)
    $vis     = [W]::IsWindowVisible($h)
    $root    = [W]::GetAncestor($h, $GA_ROOT)
    $proc    = Get-ProcName $pid_
    $isChild = ($root -ne $h)
    return ("hWnd=0x{0:X} cls='{1}' proc={2}(pid {3}) title='{4}' rect=({5},{6})-({7},{8}) style=0x{9:X8} exstyle=0x{10:X8} visible={11} root=0x{12:X} isChildOfOther={13}" -f `
        [int64]$h, $cls.ToString(), $proc, $pid_, $title.ToString(), $r.Left,$r.Top,$r.Right,$r.Bottom, $style, $exstyle, $vis, [int64]$root, $isChild)
}

Write-Host "=== Servo top-level + child windows ($ProcessName) ===" -ForegroundColor Cyan
$servoPids = @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id)
if (-not $servoPids -or $servoPids.Count -eq 0) {
    Write-Host "  (no '$ProcessName' process running -- launch the wall first)" -ForegroundColor Yellow
} else {
    Write-Host "  servo pids: $($servoPids -join ', ')"
    $script:servoPids = $servoPids
    $script:topLevels = New-Object System.Collections.ArrayList
    $enumTop = [W+EnumProc]{
        param($h, $l)
        [uint32]$pid_ = 0
        [W]::GetWindowThreadProcessId($h, [ref]$pid_) | Out-Null
        if ($script:servoPids -contains [int]$pid_) { [void]$script:topLevels.Add($h) }
        return $true
    }
    [W]::EnumWindows($enumTop, [IntPtr]::Zero) | Out-Null
    foreach ($h in $script:topLevels) {
        Write-Host ("  TOP   " + (Describe-Window $h))
        $script:children = New-Object System.Collections.ArrayList
        $enumChild = [W+EnumProc]{ param($c, $l) [void]$script:children.Add($c); return $true }
        [W]::EnumChildWindows($h, $enumChild, [IntPtr]::Zero) | Out-Null
        foreach ($c in $script:children) { Write-Host ("    CHILD " + (Describe-Window $c)) }
        if ($script:children.Count -eq 0) { Write-Host "    (no child windows)" }
    }
}

Write-Host ""
Write-Host "=== Auto hit-test of each Servo window CENTER (no mouse needed) ===" -ForegroundColor Cyan
# WindowFromPoint(center) tells us which HWND the OS would deliver WM_MOUSEMOVE to
# for a cursor at that tile's center -- the decisive test, independent of the cursor.
if ($script:topLevels -and $script:topLevels.Count -gt 0) {
    foreach ($h in $script:topLevels) {
        $r = New-Object RECT
        [W]::GetWindowRect($h, [ref]$r) | Out-Null
        $cx = [int](($r.Left + $r.Right) / 2)
        $cy = [int](($r.Top + $r.Bottom) / 2)
        $p = New-Object POINT; $p.X = $cx; $p.Y = $cy
        $hit = [W]::WindowFromPoint($p)
        $rootHit = [W]::GetAncestor($hit, $GA_ROOT)
        $verdict = if ($hit -eq $h) { "HIT==window (mouse reaches this HWND directly)" }
                   elseif ($rootHit -eq $h) { "HIT is a CHILD of this window (child eats mouse!)" }
                   else { "HIT is a DIFFERENT window (overlay/other eats mouse!)" }
        Write-Host ("  Servo win 0x{0:X} center=({1},{2}) -> {3}" -f [int64]$h,$cx,$cy,$verdict) -ForegroundColor Yellow
        Write-Host ("      under-center: " + (Describe-Window $hit))
    }
} else {
    Write-Host "  (no Servo windows enumerated; launch the wall first)"
}

Write-Host ""
Write-Host "=== Hit-test sampling for $DurationSec s (move a REAL mouse over a tile) ===" -ForegroundColor Cyan
$deadline = (Get-Date).AddSeconds($DurationSec)
$sleepMs = [int](1000 / [Math]::Max(1,$SampleHz))
$lastLine = ""
while ((Get-Date) -lt $deadline) {
    $p = New-Object POINT
    if ([W]::GetCursorPos([ref]$p)) {
        $hUnder = [W]::WindowFromPoint($p)
        $rootUnder = [W]::GetAncestor($hUnder, $GA_ROOT)
        [uint32]$underPid = 0
        [W]::GetWindowThreadProcessId($hUnder, [ref]$underPid) | Out-Null
        $isServo = ($servoPids -contains [int]$underPid)
        $clsU = New-Object System.Text.StringBuilder 256
        [W]::GetClassName($hUnder, $clsU, 256) | Out-Null
        $fg = [W]::GetForegroundWindow()
        $clsFg = New-Object System.Text.StringBuilder 256
        [W]::GetClassName($fg, $clsFg, 256) | Out-Null
        [uint32]$fgPid = 0
        [W]::GetWindowThreadProcessId($fg, [ref]$fgPid) | Out-Null
        $line = ("cursor=({0},{1}) underCursor=0x{2:X}[{3}] proc={4} isServo={5} root=0x{6:X} | foreground=0x{7:X}[{8}] proc={9}" -f `
            $p.X,$p.Y,[int64]$hUnder,$clsU.ToString(),(Get-ProcName $underPid),$isServo,[int64]$rootUnder,[int64]$fg,$clsFg.ToString(),(Get-ProcName $fgPid))
        if ($line -ne $lastLine) {
            $color = if ($isServo) { "Green" } else { "Red" }
            Write-Host $line -ForegroundColor $color
            $lastLine = $line
        }
    }
    Start-Sleep -Milliseconds $sleepMs
}
Write-Host ""
Write-Host "INTERPRETATION:" -ForegroundColor Cyan
Write-Host "  * underCursor isServo=True (green) while hovering a tile  -> mouse DOES hit Servo's HWND;"
Write-Host "    the loss is inside winit's message pumping (look at dispatch_peeked_messages / RedrawRequested)."
Write-Host "  * underCursor isServo=False (red) or a CHILD/overlay class -> THAT window eats the mouse (root cause)."
Write-Host "  * Compare 'underCursor' class to the Servo TOP/CHILD window classes listed above."
