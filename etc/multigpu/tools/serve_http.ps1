# Serves a folder over http on loopback. Split out of run_wall_dist.ps1 -Serve so the
# same server can be started by hand, without a wall run attached.
#
# Why a wall needs this at all: two things do not work from a file:/// URL.
#
#   1. ES modules do not load ("Fetching module script failed Unsupported scheme"), which
#      rules out every three.js page.
#   2. ***WebGPU hangs, with no error anywhere.*** The constellation keys the WebGPU thread
#      by host name; a file:// URL has an opaque origin and therefore no host, so that path
#      logs `Invalid host url` and returns WITHOUT answering the response channel --
#      requestAdapter() then neither resolves nor rejects and the page sits forever. If a
#      WebGPU page looks frozen, run the engine with RUST_LOG=warn and look for that line.
#
# Pure ASCII on purpose (a Korean launcher once failed to parse on a test machine that
# decodes with a legacy console codepage, swallowing a closing quote).
#
# Usage:
#   .\serve_http.ps1                          # serves pages\html next to this script
#   .\serve_http.ps1 -Root D:\some\dir -Port 9000
#   $srv = .\serve_http.ps1 -Background       # returns the Process; stop it yourself
#   Stop-Process -Id $srv.Id -Force
#
# Foreground is the default and blocks until Ctrl+C, which is what a person wants.
# -Background is for scripts: it returns only once THIS server owns the listening socket,
# so a caller never races startup and never mistakes someone else's server for its own.

param(
    # Default: pages\html beside this script (the dist layout), else pages\html under the
    # repo's tests\ (the worktree layout). Neither existing is an error worth guessing past.
    [string] $Root = "",
    [int]    $Port = 8731,
    # Loopback by default. Serving a dev tree on 0.0.0.0 exposes it to the network, so that
    # has to be asked for.
    [string] $Bind = "127.0.0.1",
    [switch] $Background,
    # How long -Background waits for its own listening socket before giving up.
    [int]    $TimeoutSec = 10
)

$ErrorActionPreference = "Stop"

# Which pids are listening on this port? Get-NetTCPConnection is the precise answer and is
# present on every supported Windows; netstat is the fallback for a machine where the
# NetTCPIP module is missing.
function Get-NetTcpListenerPid([int] $Port) {
    $conns = @(Get-NetTCPConnection -LocalPort $Port -State Listen -EA SilentlyContinue)
    if ($conns.Count) { return @($conns | ForEach-Object { [int]$_.OwningProcess } | Sort-Object -Unique) }
    if ($null -eq (Get-Command Get-NetTCPConnection -EA SilentlyContinue)) {
        $out = & netstat.exe -ano -p TCP 2>$null
        $pids = @($out | Where-Object { $_ -match "^\s+TCP\s+\S+:$Port\s+\S+\s+LISTENING\s+(\d+)\s*$" } |
            ForEach-Object { [int]$Matches[1] } | Sort-Object -Unique)
        return $pids
    }
    return @()
}

if ($Root -eq "") {
    foreach ($c in @((Join-Path $PSScriptRoot "pages\html"),
                     (Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) "..\tests\html"))) {
        if (Test-Path $c) { $Root = (Resolve-Path $c).Path; break }
    }
    if ($Root -eq "") { throw "no -Root given and no pages\html found next to $PSScriptRoot" }
}
if (!(Test-Path $Root)) { throw "root not found: $Root" }
$Root = (Resolve-Path $Root).Path

$py = (Get-Command python -EA SilentlyContinue).Source
if (-not $py) { $py = (Get-Command py -EA SilentlyContinue).Source }
if (-not $py) { throw "python not found on PATH (this runs: python -m http.server)" }

$pyArgs = @("-m", "http.server", "$Port", "--bind", $Bind, "--directory", $Root)
$url = "http://{0}:{1}/" -f $(if ($Bind -eq "0.0.0.0") { "127.0.0.1" } else { $Bind }), $Port

if (-not $Background) {
    Write-Host "serving $Root at $url   (Ctrl+C to stop)"
    & $py @pyArgs
    exit $LASTEXITCODE
}

# Refuse before starting if the port is taken. python -m http.server does exit on its own
# in that case, but saying so here gives the real reason instead of a startup-timeout.
$taken = @(Get-NetTcpListenerPid -Port $Port)
if ($taken.Count) {
    throw "port $Port is already in use (pid $($taken -join ', ')). Pass -Port <n> or stop it."
}

$proc = Start-Process -FilePath $py -ArgumentList $pyArgs -PassThru -WindowStyle Hidden

# Wait until OUR process owns a listening socket on the port -- not merely until something
# answers there.
#
# ***A plain "can I connect?" probe is wrong and was wrong here:*** with another server
# already on the port, python exits and the probe still succeeds, because the OTHER server
# answers. The check reported success while handing back a dead process. So match the
# owning pid.
#
# The point of waiting at all: a server that is up but not yet listening makes the first
# page fail to load, and a page that does not load reads as "the engine cannot render this"
# -- a wrong and expensive conclusion.
$deadline = (Get-Date).AddSeconds($TimeoutSec)
$listening = $false
while ((Get-Date) -lt $deadline) {
    if ($proc.HasExited) { throw "http server exited at startup (port $Port in use, or python rejected the arguments)" }
    if ((Get-NetTcpListenerPid -Port $Port) -contains $proc.Id) { $listening = $true; break }
    Start-Sleep -Milliseconds 150
}
if (-not $listening) {
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -EA SilentlyContinue }
    throw "http server did not start listening on ${Bind}:${Port} within ${TimeoutSec}s"
}

Write-Host "  serving $Root at $url  (pid $($proc.Id))"
$proc
