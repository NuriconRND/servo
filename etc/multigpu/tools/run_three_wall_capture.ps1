param(
    [string] $Query = "",                 # e.g. "?solid=1"
    [string] $Tag = "three_retarget",
    [int]    $WaitSec = 17,
    [switch] $DisableWebGPU               # omit --pref dom_webgpu_enabled=true
)

# Launch the three.js retargeting wall, bring both tile windows to front, and capture the full virtual screen.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing

$sig = @'
using System;using System.Runtime.InteropServices;using System.Collections.Generic;
public class WallWin{
 [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb,IntPtr l);
 public delegate bool EnumWindowsProc(IntPtr h,IntPtr l);
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h,out uint pid);
 [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
 [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h,int n);
 [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
 [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
 public static List<IntPtr> ByPid(uint t){var r=new List<IntPtr>();EnumWindows((h,l)=>{if(IsWindowVisible(h)){uint p;GetWindowThreadProcessId(h,out p);if(p==t)r.Add(h);}return true;},IntPtr.Zero);return r;}
}
'@
Add-Type $sig

Get-Process servoshell -ErrorAction SilentlyContinue | Stop-Process -Force

$root   = (Resolve-Path (Join-Path $PSScriptRoot "..\..\..")).Path
$exe    = Join-Path $root 'target\release\servoshell.exe'
$layout = Join-Path $root 'etc\multigpu\config\wall_layout.example_2x1_dualgpu.json'
$url    = "http://127.0.0.1:8753/webgpu_fanout_three_retargeting.html$Query"
$logdir = Join-Path $root 'target\multigpu_logs'
$ts     = Get-Date -Format 'yyyyMMdd_HHmmss'
$out    = Join-Path $logdir "${Tag}_${ts}"

$env:RUST_LOG = 'info'
$args = @('--wall-layout', $layout, '--wall-all-tiles')
if (-not $DisableWebGPU) { $args += @('--pref', 'dom_webgpu_enabled=true') }
$args += $url

$p = Start-Process -FilePath $exe -ArgumentList $args -WorkingDirectory $root `
    -RedirectStandardOutput "$out.out.log" -RedirectStandardError "$out.err.log" -PassThru
Start-Sleep -Seconds $WaitSec

foreach ($h in [WallWin]::ByPid([uint32]$p.Id)) {
    [void][WallWin]::ShowWindow($h, 5); [void][WallWin]::BringWindowToTop($h); [void][WallWin]::SetForegroundWindow($h)
}
Start-Sleep -Milliseconds 1000

$vs  = [System.Windows.Forms.SystemInformation]::VirtualScreen
$bmp = New-Object System.Drawing.Bitmap($vs.Width, $vs.Height)
$g   = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($vs.X, $vs.Y, 0, 0, $bmp.Size)
$shot = Join-Path $logdir "${Tag}.png"
$bmp.Save($shot, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()

Write-Host "pid=$($p.Id)"
Write-Host "=== [retarget] ==="
Select-String -Path "$out.err.log" -Pattern '\[retarget\]' | ForEach-Object { $_.Line -replace '^.*\[retarget\]', '[retarget]' } | Select-Object -Last 12
Write-Host "shot=$shot"
Write-Host "errlog=$out.err.log"
