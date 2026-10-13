<#
.SYNOPSIS
  장비 인증 전달을 사람 없이 확인한다 (servo-handoff/servo-device-auth.md 3절).
.DESCRIPTION
  경우마다 DeviceService fake-auth 를 띄우고, winit_wall 로 확인 페이지를 열고, 엔진 로그의
  `[device-auth-check]` 줄과 자가 시험 판정으로 PASS/FAIL 을 낸다.
    시험 키 빌드: ok / invalid / unreachable (각각 새로고침·이동·# 자가 시험) + unreachable->ok
    실제 키 빌드: 서명 틀림 + 자가 시험 인자 거부
  먼저 `. ..\scripts\servo_env.ps1` 로 DLL PATH 를 잡는다.
#>
param(
    [string] $Exe = "",
    [string] $DeviceService = "",
    [string] $Layout = "",
    [string] $OutDir = "",
    [int] $TimeoutSec = 90
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$root = Split-Path $repo -Parent
if (!$Exe) { $Exe = Join-Path $repo "target\release\examples\winit_wall.exe" }
if (!$DeviceService) { $DeviceService = Join-Path $root "servo-handoff\DeviceService\DeviceService.exe" }
if (!$Layout) { $Layout = Join-Path $root "config\wall_layout.local_3x1.json" }
if (!$OutDir) { $OutDir = Join-Path $root "device_auth_check" }
$testKey = Join-Path $root "servo-handoff\keys\test\response-signing-test-private.pem"
. (Join-Path $PSScriptRoot "device_auth_key_kind.ps1")
New-Item -ItemType Directory -Force $OutDir | Out-Null

$kind = Get-DeviceAuthKeyKind -Exe $Exe -Repo $repo
Write-Host "engine=$Exe key=$kind"
if ($kind -eq 'unknown') { throw "cannot tell which device-auth key $Exe was built with" }

# `cargo build --example` 는 exe 옆에 ANGLE(libEGL/libGLESv2)도 GStreamer 플러그인도 두지 않는다.
#  - ANGLE 이 없으면 surfman 이 "egl function was not loaded" 로 패닉한다.
#  - 플러그인은 exe 와 같은 폴더에서만 읽는다(components/servo/servo.rs). 없으면 미디어가
#    처음 초기화될 때 "Error initializing GStreamer" 를 남기고 process::exit(1) 로 끝난다.
# 그래서 exe 옆에 플러그인이 없으면 exe·ANGLE·플러그인을 $OutDir\engine 에 모아 거기서 돌린다
# (make_wall_dist.ps1 과 같은 구성). 배포 폴더 exe 처럼 이미 갖춰져 있으면 그대로 쓴다.
$savedPath = $env:PATH
$gstRoot = $env:GSTREAMER_1_0_ROOT_MSVC_X86_64
if ($gstRoot) { $env:PATH = "$gstRoot\bin;$env:PATH" }
$exeDir = Split-Path $Exe -Parent
if (!(Test-Path (Join-Path $exeDir 'gstcoreelements.dll'))) {
    if (!$gstRoot) { throw "GSTREAMER_1_0_ROOT_MSVC_X86_64 is not set; dot-source scripts\servo_env.ps1 first" }
    $profileDir = Split-Path $exeDir -Parent
    $angle = Get-ChildItem (Join-Path $profileDir 'build\mozangle-*\out\libEGL.dll') -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (Test-Path (Join-Path $profileDir 'libEGL.dll')) { $angleDir = $profileDir }
    elseif ($angle) { $angleDir = $angle.DirectoryName }
    else { throw "no libEGL.dll in $profileDir or $profileDir\build\mozangle-*\out" }
    $stage = Join-Path $OutDir 'engine'
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    New-Item -ItemType Directory -Force $stage | Out-Null
    Copy-Item $Exe $stage
    Copy-Item (Join-Path $gstRoot 'lib\gstreamer-1.0\*.dll') $stage
    foreach ($n in @('libEGL.dll', 'libGLESv2.dll')) { Copy-Item (Join-Path $angleDir $n) $stage }
    $Exe = Join-Path $stage (Split-Path $Exe -Leaf)
    Write-Host "staged engine=$Exe angle=$angleDir"
}

$fakes = New-Object System.Collections.ArrayList
$results = New-Object System.Collections.ArrayList

function Wait-Port([int] $Port) {
    for ($i = 0; $i -lt 100; $i++) {
        try { $c = New-Object Net.Sockets.TcpClient('127.0.0.1', $Port); $c.Close(); return }
        catch { Start-Sleep -Milliseconds 200 }
    }
    throw "fake-auth did not open port $Port"
}

function Start-FakeAuth([string] $Name, [string[]] $FakeArgs, [int] $Port) {
    $a = @('fake-auth') + $FakeArgs + @('--port', "$Port", '--key', $testKey)
    $p = Start-Process -FilePath $DeviceService -ArgumentList $a -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput (Join-Path $OutDir "$Name.fake$Port.out.log") `
        -RedirectStandardError (Join-Path $OutDir "$Name.fake$Port.err.log")
    [void]$fakes.Add($p)
    Wait-Port $Port
}

function Stop-FakeAuth {
    foreach ($p in $fakes) { if (!$p.HasExited) { Stop-Process -Id $p.Id -Force } }
    $fakes.Clear()
    Start-Sleep -Milliseconds 500
}

# 엔진을 띄우고 스스로 끝날 때까지 기다린다(자가 시험 판정 또는 --capture-sec).
# 강제 종료하면 stderr 가 비므로 시간 초과는 그 자체로 실패다.
function Invoke-Engine([string] $Name, [string] $Url, [string[]] $Extra) {
    $log = Join-Path $OutDir "$Name.err.log"
    $out = Join-Path $OutDir "$Name.out.log"
    $a = @('--wall-layout', $Layout, '--wall-tile-index', '0') + $Extra + @($Url)
    $env:RUST_LOG = 'warn'
    $p = Start-Process -FilePath $Exe -ArgumentList $a -PassThru `
        -RedirectStandardError $log -RedirectStandardOutput $out
    $null = $p.Handle  # PS 5.1: 핸들을 잡아 두지 않으면 ExitCode 가 비어 온다.
    $timedOut = -not $p.WaitForExit($TimeoutSec * 1000)
    if ($timedOut) { Stop-Process -Id $p.Id -Force }
    $lines = @(Get-Content $log -Encoding UTF8) + @(Get-Content $out -Encoding UTF8)
    return @{ Log = $log; TimedOut = $timedOut; Lines = $lines; ExitCode = $p.ExitCode }
}

function Get-Notices($Run) { @($Run.Lines | Where-Object { $_ -match '\[device-auth-check\] 알림' }) }

function Add-Result([string] $Name, [bool] $Pass, [string] $Detail) {
    [void]$results.Add([pscustomobject]@{ Case = $Name; Result = $(if ($Pass) { 'PASS' } else { 'FAIL' }); Detail = $Detail })
}

# 공통 위생: 패닉 없음, 지문 없음, 모양이 다른 값 없음, 중간에 GStreamer 초기화 실패로
# process::exit(1) 한 흔적 없음(그러면 판정이 짧게 잘린 로그로 난다).
function Test-Hygiene($Run) {
    $bad = @($Run.Lines | Where-Object { $_ -match 'panicked|fingerprint|모양이 다름|Error initializing GStreamer' })
    return $bad.Count -eq 0
}

# 자가 시험 경우: 새 문서 셋 모두 기대 값, # 이동 뒤 추가 주입 없음.
function Test-SelfTestCase([string] $Name, [string[]] $FakeArgs, [int] $FakePort, [int] $PagePort, [string] $State, [string] $ProjectKey) {
    try {
        Start-FakeAuth $Name $FakeArgs $FakePort
        $run = Invoke-Engine $Name "http://127.0.0.1:$PagePort/ipwall/device-auth-check" @('--device-auth-selftest')
    } finally { Stop-FakeAuth }
    $notices = Get-Notices $run
    $want = @($notices | Where-Object {
        $_ -match ('"state":"' + $State + '"') -and $_ -match '맞음' -and
        ($ProjectKey -eq '' -or $_ -match ('"projectKey":"' + $ProjectKey + '"'))
    })
    $pass = (-not $run.TimedOut) -and
        (@($run.Lines | Where-Object { $_ -match 'device-auth selftest: PASS' }).Count -eq 1) -and
        ($want.Count -ge 3) -and ($want.Count -eq $notices.Count) -and (Test-Hygiene $run)
    Add-Result $Name $pass "notices=$($notices.Count) expected=$($want.Count) timedOut=$($run.TimedOut) exit=$($run.ExitCode) log=$($run.Log)"
}

if ($kind -eq 'test') {
    Test-SelfTestCase 'ok' @('--state', 'ok', '--project-key', '123412') 7250 7250 'ok' '123412'
    Test-SelfTestCase 'invalid' @('--state', 'invalid') 7250 7250 'invalid' ''
    # 7250 을 비우고 확인 페이지는 7251 에서 연다. 같은지 판정은 보지 않는다(모양만).
    Test-SelfTestCase 'unreachable' @('--state', 'invalid') 7251 7251 'unreachable' ''

    # unreachable -> ok: 엔진이 뜬 뒤 7250 에 fake-auth 를 띄운다. 같은 문서에 값이 바뀌어 다시 들어와야 한다.
    $name = 'unreachable_to_ok'
    $job = $null
    try {
        Start-FakeAuth $name @('--state', 'invalid') 7251
        $png = Join-Path $OutDir "$name.png"
        $job = Start-Job -ScriptBlock {
            param($ds, $key, $dir)
            Start-Sleep -Seconds 8
            & $ds fake-auth --state ok --project-key 123412 --port 7250 --key $key *> (Join-Path $dir 'unreachable_to_ok.fake7250.log')
        } -ArgumentList $DeviceService, $testKey, $OutDir
        $run = Invoke-Engine $name "http://127.0.0.1:7251/ipwall/device-auth-check" @('--capture', $png, '--capture-sec', '20')
    } finally {
        if ($job) { Stop-Job $job -ErrorAction SilentlyContinue; Remove-Job $job -Force -ErrorAction SilentlyContinue }
        Get-Process DeviceService -ErrorAction SilentlyContinue | Stop-Process -Force
        Stop-FakeAuth
    }
    $notices = [string[]](Get-Notices $run)
    $u = [array]::FindIndex($notices, [Predicate[string]]{ param($l) $l -match '"state":"unreachable"' })
    $o = [array]::FindIndex($notices, [Predicate[string]]{ param($l) $l -match '"state":"ok"' -and $l -match '"projectKey":"123412"' })
    Add-Result $name (($u -ge 0) -and ($o -gt $u) -and (Test-Hygiene $run)) "unreachable@$u ok@$o exit=$($run.ExitCode) log=$($run.Log)"
}
else {
    # 서명 틀림: 실제 키 빌드는 시험 키가 서명한 "ok"를 invalid 로 넘긴다.
    $name = 'signature_mismatch'
    try {
        Start-FakeAuth $name @('--state', 'ok', '--project-key', '123412') 7250
        $run = Invoke-Engine $name "http://127.0.0.1:7250/ipwall/device-auth-check" @('--capture', (Join-Path $OutDir "$name.png"), '--capture-sec', '12')
    } finally { Stop-FakeAuth }
    $notices = Get-Notices $run
    $invalid = @($notices | Where-Object { $_ -match '"state":"invalid"' })
    $ok = @($notices | Where-Object { $_ -match '"state":"ok"' })
    Add-Result $name (($invalid.Count -ge 1) -and ($ok.Count -eq 0) -and (Test-Hygiene $run)) "invalid=$($invalid.Count) ok=$($ok.Count) exit=$($run.ExitCode) log=$($run.Log)"

    # 실제 키 빌드는 자가 시험 인자를 받지 않는다.
    $name = 'selftest_rejected'
    $run = Invoke-Engine $name "http://127.0.0.1:7250/ipwall/device-auth-check" @('--device-auth-selftest')
    $rejected = @($run.Lines | Where-Object { $_ -match 'unknown argument: --device-auth-selftest' }).Count -ge 1
    Add-Result $name ($rejected -and -not $run.TimedOut) "exit=$($run.ExitCode) log=$($run.Log)"
}

$env:PATH = $savedPath
$results | Format-Table -AutoSize | Out-String | Write-Host
if (@($results | Where-Object { $_.Result -ne 'PASS' }).Count -gt 0) { exit 1 }
exit 0
