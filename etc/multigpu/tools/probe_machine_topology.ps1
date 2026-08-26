# Dumps the facts needed to interpret a wall run on a multi-group box, and nothing else.
#
# Why this exists: two runs of the SAME command with the SAME binary landed in two stable
# states (28 presents/s vs 5, decode threads 0.77 vs 0.98 cores), and the only field that
# separated them nine times out of nine was the processor group thread_cpu_probe reported.
# But that field alone does not explain anything and it does not even add up -- one run was
# credited with 46.4 cores while sitting in a group that has 40 logical processors. So
# before any more theories: get the machine's real shape.
#
# Four things decide how to read a wall run here, and none of them are in the run's own log:
#
#  1. The Windows version. Up to Windows 10 / Server 2019 a process is assigned to ONE
#     processor group at creation and every thread it makes goes there -- so on this box the
#     wall would only ever see 40 of the 80 logical processors, which is not enough for 45
#     decode threads. Windows 11 / Server 2022 made processes span all groups by default.
#     ***This single fact changes what "0.98 cores per decode thread" means:*** either the
#     threads are oversubscribed onto half the machine, or they are genuinely CPU-bound.
#  2. Logical processors per group.
#  3. Which NUMA node each group is, and which node the GPU hangs off. Cross-node PCIe
#     traffic is the standing suspicion for why one group would be worse than the other.
#  4. Whether the two groups are two sockets or one socket split in half.
#
# Pure ASCII on purpose (a Korean launcher once failed to parse on a test machine that
# decodes with a legacy console codepage).
#
# Usage:  .\probe_machine_topology.ps1

$ErrorActionPreference = "Continue"

Write-Host "=== OS =========================================================="
$os = Get-CimInstance Win32_OperatingSystem
"  {0}" -f $os.Caption
"  version {0}  build {1}" -f $os.Version, $os.BuildNumber
# 22000 = Windows 11 21H2, 20348 = Server 2022. At or above either, processes are group-aware
# by default and a process can use every logical processor without asking.
$build = [int]$os.BuildNumber
if ($build -ge 20348) {
    "  -> processes span ALL processor groups by default on this build"
} else {
    "  -> ***processes are pinned to ONE processor group at creation on this build***"
    "     A process here sees only its own group's logical processors unless it calls"
    "     SetThreadGroupAffinity itself. On a 2-group box that is HALF the machine."
}

Write-Host ""
Write-Host "=== processor groups ============================================"
Add-Type -Namespace Win32 -Name Topo -MemberDefinition @'
[DllImport("kernel32.dll")] public static extern uint GetActiveProcessorCount(ushort g);
[DllImport("kernel32.dll")] public static extern ushort GetActiveProcessorGroupCount();
'@
$gc = [Win32.Topo]::GetActiveProcessorGroupCount()
"  groups: $gc"
for ($g = 0; $g -lt $gc; $g++) {
    "    group {0} : {1} logical processors" -f $g, [Win32.Topo]::GetActiveProcessorCount([ushort]$g)
}
"    ALL     : {0} logical processors" -f [Win32.Topo]::GetActiveProcessorCount([ushort]0xFFFF)

Write-Host ""
Write-Host "=== sockets / cores ============================================="
Get-CimInstance Win32_Processor | ForEach-Object {
    "  {0}  cores={1} logical={2}" -f $_.Name.Trim(), $_.NumberOfCores, $_.NumberOfLogicalProcessors
}

Write-Host ""
Write-Host "=== NUMA nodes =================================================="
# Win32_NumaNode is not present everywhere; fall back to the group count.
$numa = Get-CimInstance -ClassName Win32_NumaNode -ErrorAction SilentlyContinue
if ($numa) {
    $numa | ForEach-Object { "  node {0}  {1}" -f $_.NodeId, $_.Caption }
} else {
    "  Win32_NumaNode not exposed; see 'group' counts above (groups usually track nodes 1:1)"
}

Write-Host ""
Write-Host "=== display adapters: which NUMA node is the GPU on? ============"
# This is the number the whole 'group 0 is slower' theory needs. DXGI does not expose it --
# it is a PnP device property, so ask the device.
Get-PnpDevice -Class Display -Status OK -ErrorAction SilentlyContinue | ForEach-Object {
    $dev  = $_
    $numaProp = ($dev | Get-PnpDeviceProperty -KeyName 'DEVPKEY_Device_Numa_Node'    -ErrorAction SilentlyContinue).Data
    $loc      = ($dev | Get-PnpDeviceProperty -KeyName 'DEVPKEY_Device_LocationInfo' -ErrorAction SilentlyContinue).Data
    $prox     = ($dev | Get-PnpDeviceProperty -KeyName 'DEVPKEY_Device_Numa_Proximity_Domain' -ErrorAction SilentlyContinue).Data
    "  {0}" -f $dev.FriendlyName
    "      NUMA node = {0}   proximity domain = {1}" -f `
        $(if ($null -ne $numaProp) { $numaProp } else { "not reported" }), `
        $(if ($null -ne $prox)     { $prox }     else { "not reported" })
    "      location  = {0}" -f $loc
}

Write-Host ""
Write-Host "=== what to do with this ========================================"
"  - If the build is below 20348 AND there are 2 groups, the wall has been running on"
"    HALF this machine all along. 45 decode threads on 40 logical processors is"
"    oversubscription, and 0.98 cores per thread means the group is full, not that"
"    decoding is expensive. That is a bigger finding than any knob measured so far."
"  - If the GPU reports a NUMA node, runs that land in the OTHER group pay for every"
"    upload and present across the interconnect -- which is the standing explanation"
"    for why group 0 runs collapse and group 1 runs do not."
