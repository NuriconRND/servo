param(
    [string] $Url = "https://threejs.org/examples/webgl_animation_keyframes.html",
    [string] $Layout = "wall_layout.example_2x1_dualgpu.json",
    [string] $Tag = "fanout",
    [string] $Pref = "dom_webgl2_enabled=true",   # use dom_webgpu_enabled=true for WebGPU pages
    [int]    $LoadSec = 14,
    [int]    $SampleSec = 16
)

# Display the live three.js keyframes page on the 2x1 dual-GPU wall and verify
# GPU fan-out plus per-GPU rendering (fan-out log, per-GPU render logs, nvidia-smi).
$ErrorActionPreference = 'Stop'
$root   = (Resolve-Path (Join-Path $PSScriptRoot "..\..\..")).Path
$exe    = Join-Path $root 'target\release\servoshell.exe'
$layout = Join-Path $root "etc\multigpu\config\$Layout"
$logdir = Join-Path $root 'target\multigpu_logs'
$ts     = Get-Date -Format 'HHmmss'
$log    = Join-Path $logdir "${Tag}_${ts}"
$smi    = "C:\Windows\System32\nvidia-smi.exe"

Get-Process servoshell -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

Write-Host "=== baseline GPU (idx, memMiB, util%) ==="
& $smi --query-gpu=index,memory.used,utilization.gpu --format=csv,noheader

$env:RUST_LOG = 'info'
$p = Start-Process -FilePath $exe -ArgumentList @(
    '--wall-layout', $layout, '--wall-all-tiles',
    '--pref', $Pref, $Url
) -WorkingDirectory $root -RedirectStandardOutput "$log.out.log" -RedirectStandardError "$log.err.log" -PassThru
Write-Host "pid=$($p.Id) loading ${LoadSec}s..."
Start-Sleep -Seconds $LoadSec

# Sample nvidia-smi during active render: per-GPU util + servoshell compute-app per GPU.
Write-Host "=== sampling GPU during render (${SampleSec}s) ==="
$samples = @()
for ($i=0; $i -lt $SampleSec; $i++) {
    $row = (& $smi --query-gpu=index,memory.used,utilization.gpu --format=csv,noheader) -join '   ||   '
    $samples += $row
    Start-Sleep -Seconds 1
}
Write-Host "--- per-GPU util samples (each line = GPU0  ||  GPU1) ---"
$samples | ForEach-Object { Write-Host $_ }

Write-Host "=== compute-apps (which GPUs servoshell runs on) ==="
& $smi --query-compute-apps=gpu_bus_id,pid,process_name,used_memory --format=csv,noheader | Where-Object { $_ -match 'servoshell' }

Get-Process servoshell -ErrorAction SilentlyContinue | Stop-Process -Force

$err = "$log.err.log"
Write-Host "=== WebGL multi-GPU fan-out log ==="
Select-String -Path $err -Pattern 'multi-GPU backend fan-out' | Select-Object -Last 2 | ForEach-Object { ($_.Line -replace '^.*\] ','') }
Write-Host "=== per-GPU 'Wall render end' counts (requested_gpu) ==="
$g0 = (Select-String -Path $err -Pattern 'Wall render end.*requested_gpu=Some\(0\)').Count
$g1 = (Select-String -Path $err -Pattern 'Wall render end.*requested_gpu=Some\(1\)').Count
Write-Host "requested_gpu=0 renders: $g0    requested_gpu=1 renders: $g1"
Write-Host "=== per-GPU 'Wall repaint target' counts ==="
$r0 = (Select-String -Path $err -Pattern 'Wall repaint target.*requested_gpu=Some\(0\)').Count
$r1 = (Select-String -Path $err -Pattern 'Wall repaint target.*requested_gpu=Some\(1\)').Count
Write-Host "primary(gpu0) repaints: $r0    secondary(gpu1) repaints: $r1"
Write-Host "=== errors ==="
Write-Host ("WebGL context errors: " + (Select-String -Path $err -Pattern 'Error creating WebGL context').Count + "   InvalidOperation: " + (Select-String -Path $err -Pattern 'WebGL error: InvalidOperation').Count)
Write-Host "errlog=$err"
