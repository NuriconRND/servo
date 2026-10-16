# Tell which device-auth public key an exe was built with.
#
# The key is embedded at build time with `include_str!` (components/sdwall_device_auth/key.rs),
# so the PEM body is present verbatim in the binary. The first body line (64 chars) differs per key.
function Get-DeviceAuthKeyKind([string] $Exe, [string] $Repo) {
    $keys = Join-Path $Repo "components\sdwall_device_auth\keys"
    $testLine = (Get-Content (Join-Path $keys "test\response-signing-test-public.pem"))[1]
    $releaseLine = (Get-Content (Join-Path $keys "response-signing-public.pem"))[1]
    $hasTest = Select-String -Path $Exe -Pattern $testLine -SimpleMatch -Quiet
    $hasRelease = Select-String -Path $Exe -Pattern $releaseLine -SimpleMatch -Quiet
    if ($hasTest -and -not $hasRelease) { return 'test' }
    if ($hasRelease -and -not $hasTest) { return 'release' }
    return 'unknown'
}
