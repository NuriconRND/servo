param(
    [string] $Page = "keyframes_simplelight.html",
    [string] $Tag  = "single",
    [int]    $Port  = 8753,
    [int]    $WaitSec = 16,
    [int]    $Width  = 1280,
    [int]    $Height = 800
)

# 단일 창(월 아님)으로 three.js 페이지를 띄우고, 해당 servoshell 창만 캡처 + stdout/stderr 로그 수집.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing

$sig = @'
using System;using System.Runtime.InteropServices;using System.Collections.Generic;
public class W32{
 [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb,IntPtr l);
 public delegate bool EnumWindowsProc(IntPtr h,IntPtr l);
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h,out uint pid);
 [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
 [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h,int n);
 [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
 [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
 [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h,out RECT r);
 [StructLayout(LayoutKind.Sequential)] public struct RECT{public int Left,Top,Right,Bottom;}
 public static List<IntPtr> ByPid(uint t){var r=new List<IntPtr>();EnumWindows((h,l)=>{if(IsWindowVisible(h)){uint p;GetWindowThreadProcessId(h,out p);if(p==t)r.Add(h);}return true;},IntPtr.Zero);return r;}
}
'@
Add-Type $sig

Get-Process servoshell -ErrorAction SilentlyContinue | Stop-Process -Force
# 기존 http.server(이 포트) 정리는 생략 — 같은 포트면 재사용됨.

$root    = (Resolve-Path (Join-Path $PSScriptRoot "..\..\..")).Path
$exe     = Join-Path $root 'target\release\servoshell.exe'
$htmlDir = Join-Path $root 'tests\html'
$logdir  = Join-Path $root 'target\multigpu_logs'
New-Item -ItemType Directory -Force -Path $logdir | Out-Null
$ts  = Get-Date -Format 'yyyyMMdd_HHmmss'
$out = Join-Path $logdir "${Tag}_${ts}"
$url = "http://127.0.0.1:$Port/$Page"

# HTTP 서버 (이미 떠 있으면 무시)
$py = (Get-Command python -ErrorAction SilentlyContinue).Source
if (-not $py) { throw "python not found on PATH" }
$listening = Test-NetConnection -ComputerName 127.0.0.1 -Port $Port -InformationLevel Quiet -WarningAction SilentlyContinue
if (-not $listening) {
    Write-Host "Starting HTTP server: python -m http.server $Port (root=$htmlDir)"
    Start-Process -FilePath $py -ArgumentList @("-m","http.server","$Port","--bind","127.0.0.1") `
        -WorkingDirectory $htmlDir -WindowStyle Hidden | Out-Null
    Start-Sleep -Seconds 1
} else {
    Write-Host "HTTP server already listening on $Port"
}

$env:RUST_LOG = 'warn'
# WebGL2는 dom_webgl2_enabled pref가 켜져 있거나 host가 화이트리스트(www.servoexperiments.com)일 때만 활성.
# 127.0.0.1/threejs.org는 화이트리스트가 아니므로 명시적으로 켜야 three.js(WebGL2 전용)가 컨텍스트를 얻음.
$args = @('--pref', 'dom_webgl2_enabled=true', '--window-size', "${Width}x${Height}", $url)

$p = Start-Process -FilePath $exe -ArgumentList $args -WorkingDirectory $root `
    -RedirectStandardOutput "$out.out.log" -RedirectStandardError "$out.err.log" -PassThru
Start-Sleep -Seconds $WaitSec

# 창 전면화 + 해당 창 rect 캡처
$rect = $null
foreach ($h in [W32]::ByPid([uint32]$p.Id)) {
    [void][W32]::ShowWindow($h, 5); [void][W32]::BringWindowToTop($h); [void][W32]::SetForegroundWindow($h)
    $r = New-Object W32+RECT
    if ([W32]::GetWindowRect($h, [ref]$r)) {
        if (($r.Right - $r.Left) -gt 200) { $rect = $r }
    }
}
Start-Sleep -Milliseconds 800

$shot = Join-Path $logdir "${Tag}.png"
if ($rect) {
    $w = $rect.Right - $rect.Left; $h = $rect.Bottom - $rect.Top
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($rect.Left, $rect.Top, 0, 0, $bmp.Size)
} else {
    $vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $bmp = New-Object System.Drawing.Bitmap($vs.Width, $vs.Height)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($vs.X, $vs.Y, 0, 0, $bmp.Size)
}
$bmp.Save($shot, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()

Write-Host "pid=$($p.Id)  shot=$shot"
Write-Host "=== console (SIMPLELIGHT / SOLID / probe) ==="
Select-String -Path "$out.out.log","$out.err.log" -Pattern 'SIMPLELIGHT|centerPixel|meshes=|LOAD ERROR' -ErrorAction SilentlyContinue | ForEach-Object { $_.Line } | Select-Object -Last 20
Write-Host "=== WebGL errors / backtrace (last 40) ==="
Select-String -Path "$out.err.log" -Pattern 'WebGL .*(InvalidOperation|InvalidValue|InvalidEnum)|JS backtrace|drawElements|renderBufferDirect' -ErrorAction SilentlyContinue | ForEach-Object { $_.Line } | Select-Object -Last 40
Write-Host "outlog=$out.out.log"
Write-Host "errlog=$out.err.log"
