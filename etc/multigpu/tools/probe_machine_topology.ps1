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
#  1. ***Which NUMA node the GPU hangs off.*** This is the one that matters now. Confirmed
#     2026-08-26 by forcing the node with `run_wall_dist.ps1 -NumaNode`: three runs on node 0
#     collapsed to 6 fps (Present 2.9-3.0 ms per call, dwm.exe at 0.65 cores) and three runs
#     on node 1 were fine (23-29.5 fps, 0.65-0.73 ms, dwm at 0.09). Requested node matched the
#     group the threads ran in 6/6, so this is causation, not correlation. The standing
#     explanation is that the GPU is on one node and the far node pays the interconnect on
#     every upload and present -- this probe is what confirms or kills that.
#  2. Logical processors per group, and whether the groups are two sockets or one split.
#  3. The Windows build, only to know whether a process spans groups by default
#     (Windows 11 / Server 2022 and later do; earlier ones pin to one group at creation).
#
# NOT a capacity question: the collapsed runs left 31 of 80 cores idle.
#
# Pure ASCII on purpose (a Korean launcher once failed to parse on a test machine that
# decodes with a legacy console codepage).
#
# Usage:  .\probe_machine_topology.ps1

$ErrorActionPreference = "Continue"

Write-Host "=== OS =========================================================="
$os = Get-CimInstance Win32_OperatingSystem
Write-Host ("  {0}" -f $os.Caption)
Write-Host ("  version {0}  build {1}" -f $os.Version, $os.BuildNumber)
# 22000 = Windows 11 21H2, 20348 = Server 2022. At or above either, processes are group-aware
# by default and a process can use every logical processor without asking.
$build = [int]$os.BuildNumber
if ($build -ge 20348) {
    Write-Host ("  -> processes span ALL processor groups by default on this build")
} else {
    Write-Host ("  -> processes are pinned to ONE processor group at creation on this build")
    Write-Host ("     (Windows 11 / Server 2022 and later span all groups instead).")
}

Write-Host ""
Write-Host "=== processor groups ============================================"
Add-Type -Namespace Win32 -Name Topo -MemberDefinition @'
[DllImport("kernel32.dll")] public static extern uint GetActiveProcessorCount(ushort g);
[DllImport("kernel32.dll")] public static extern ushort GetActiveProcessorGroupCount();
'@
# [System.UInt16] on purpose below: [ushort] is a PowerShell 7 type accelerator and the
# test machine runs Windows PowerShell 5.1, where it fails with "type not found" and the
# whole group section prints nothing but errors.
$gc = [Win32.Topo]::GetActiveProcessorGroupCount()
Write-Host ("  groups: $gc")
for ($g = 0; $g -lt $gc; $g++) {
    Write-Host ("    group {0} : {1} logical processors" -f $g, [Win32.Topo]::GetActiveProcessorCount([System.UInt16]$g))
}
Write-Host ("    ALL     : {0} logical processors" -f [Win32.Topo]::GetActiveProcessorCount([System.UInt16]0xFFFF))

Write-Host ""
Write-Host "=== sockets / cores ============================================="
Get-CimInstance Win32_Processor | ForEach-Object {
    Write-Host ("  {0}  cores={1} logical={2}" -f $_.Name.Trim(), $_.NumberOfCores, $_.NumberOfLogicalProcessors)
}

Write-Host ""
Write-Host "=== NUMA nodes =================================================="
# Win32_NumaNode is not present everywhere; fall back to the group count.
$numa = Get-CimInstance -ClassName Win32_NumaNode -ErrorAction SilentlyContinue
if ($numa) {
    $numa | ForEach-Object { "  node {0}  {1}" -f $_.NodeId, $_.Caption }
} else {
    Write-Host ("  Win32_NumaNode not exposed; see 'group' counts above (groups usually track nodes 1:1)")
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
    Write-Host ("  {0}" -f $dev.FriendlyName)
    $numaText = if ($null -ne $numaProp) { $numaProp } else { "not reported" }
    $proxText = if ($null -ne $prox)     { $prox }     else { "not reported" }
    Write-Host ("      NUMA node = {0}   proximity domain = {1}" -f $numaText, $proxText)
    Write-Host ("      location  = {0}" -f $loc)
}

Write-Host ""
Write-Host "=== what to do with this ========================================"
Write-Host ("  - If the GPU reports a NUMA node, pin the wall to it and the bistability is over:")
Write-Host ("      .\run_wall_dist.ps1 ... -NumaNode <that node>")
Write-Host ("    On this box node N has matched processor group N, and node 1 is the good one.")
Write-Host ("  - If the GPU reports NO node (the property is often absent on consumer parts), the")
Write-Host ("    node cannot be derived and -NumaNode 1 stays an empirical setting, not a derived")
Write-Host ("    one. Say so rather than inventing a reason.")
Write-Host ("  - Either way this is NOT a capacity problem: the collapsed runs left 31 of 80 cores")
Write-Host ("    idle, and the only thing outside our process that moved was dwm.exe (0.09 -> 0.65")
Write-Host ("    cores) while the screen updated FIVE TIMES LESS often.")
