//! ntdll pieces that windows-sys 0.61.2 does not declare, x64 layouts (2026-11-18).
//!
//! windows-sys has `NtQuerySystemInformation` (Wdk_System_SystemInformation) and the
//! winternl.h *redacted* SYSTEM_PROCESS_INFORMATION / SYSTEM_THREAD_INFORMATION in
//! Win32_System_WindowsProgramming, where the fields this probe needs (CycleTime,
//! CreateTime/UserTime/KernelTime of the process, ContextSwitches/State of the thread,
//! Win32StartAddress) are hidden inside `Reserved*` arrays. The full layouts below are the
//! documented-by-practice ones (phnt / System Informer). `self_check` in main.rs verifies
//! them at startup against GetProcessTimes/GetThreadTimes/the own image range before
//! anything is reported, and the const asserts pin the sizes.
#![allow(non_upper_case_globals, dead_code)]

use std::ffi::c_void;

use windows_sys::Win32::Foundation::NTSTATUS;

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    /// For the per-processor classes the input buffer is the processor GROUP (u16).
    /// Plain NtQuerySystemInformation (and GetSystemTimes) answer those classes for the
    /// CALLER's group only -- the 2026-08-26 "process 46.40 > machine 38.29" bug.
    pub fn NtQuerySystemInformationEx(
        class: i32,
        input: *const c_void,
        input_len: u32,
        output: *mut c_void,
        output_len: u32,
        return_len: *mut u32,
    ) -> NTSTATUS;
}

pub const SystemProcessorPerformanceInformation: i32 = 8;
pub const SystemExtendedProcessInformation: i32 = 57;

/// SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION, 48 bytes, 100 ns units, tick-charged.
/// KernelTime INCLUDES IdleTime; DPC and ISR time are inside KernelTime too, and are not
/// charged to any thread or process.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct LpTimes {
    pub idle: i64,
    pub kernel: i64,
    pub user: i64,
    pub dpc: i64,
    pub isr: i64,
    pub isr_count: u32,
}

/// SYSTEM_PROCESS_INFORMATION, x64, 0x100 bytes; the thread array follows immediately.
#[repr(C)]
pub struct SysProc {
    pub next: u32,             // 0x00 NextEntryOffset (0 = last)
    pub threads: u32,          // 0x04 NumberOfThreads
    pub ws_private: i64,       // 0x08 WorkingSetPrivateSize
    pub hard_faults: u32,      // 0x10 HardFaultCount
    pub threads_hwm: u32,      // 0x14 NumberOfThreadsHighWatermark
    pub cycle_time: u64,       // 0x18 CycleTime (TSC cycles, includes exited threads)
    pub create_time: i64,      // 0x20 CreateTime (FILETIME ticks)
    pub user_time: i64,        // 0x28 UserTime  (100 ns, tick-charged)
    pub kernel_time: i64,      // 0x30 KernelTime (100 ns, tick-charged)
    pub image_len: u16,        // 0x38 ImageName.Length (bytes)
    pub image_max: u16,        // 0x3A ImageName.MaximumLength
    _pad0: u32,                // 0x3C
    pub image_buf: *const u16, // 0x40 ImageName.Buffer (points INTO the output buffer)
    pub base_priority: i32,    // 0x48
    _pad1: u32,                // 0x4C
    pub pid: usize,            // 0x50 UniqueProcessId
    pub parent_pid: usize,     // 0x58 InheritedFromUniqueProcessId
    pub handle_count: u32,     // 0x60
    pub session_id: u32,       // 0x64
    _rest: [u8; 0x100 - 0x68], // 0x68 UniqueProcessKey .. OtherTransferCount
}

/// SYSTEM_THREAD_INFORMATION, x64, 0x50 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SysThread {
    pub kernel_time: i64,      // 0x00 (100 ns, tick-charged)
    pub user_time: i64,        // 0x08
    pub create_time: i64,      // 0x10
    pub wait_time: u32,        // 0x18 (ticks since last state change)
    _pad: u32,                 // 0x1C
    pub start_address: usize,  // 0x20 (ntdll!RtlUserThreadStart for user threads)
    pub pid: usize,            // 0x28 ClientId.UniqueProcess
    pub tid: usize,            // 0x30 ClientId.UniqueThread
    pub priority: i32,         // 0x38 current (dynamic) priority
    pub base_priority: i32,    // 0x3C
    pub context_switches: u32, // 0x40 cumulative count of switches INTO this thread
    pub state: u32, // 0x44 KTHREAD_STATE: 1 Ready 2 Running 4 Terminated 5 Waiting 7 DeferredReady
    pub wait_reason: u32, // 0x48 KWAIT_REASON
    _pad2: u32,     // 0x4C
}

/// SYSTEM_EXTENDED_THREAD_INFORMATION, x64, 0x88 bytes (class 57).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SysExtThread {
    pub t: SysThread,
    pub stack_base: usize,  // 0x50
    pub stack_limit: usize, // 0x58
    pub win32_start: usize, // 0x60 the real user start routine (module of an unnamed thread)
    pub teb: usize,         // 0x68
    _r: [usize; 3],         // 0x70 Reserved2..4
}

const _: () = assert!(size_of::<LpTimes>() == 48);
const _: () = assert!(size_of::<SysProc>() == 0x100);
const _: () = assert!(size_of::<SysThread>() == 0x50);
const _: () = assert!(size_of::<SysExtThread>() == 0x88);
const _: () = assert!(std::mem::offset_of!(SysProc, pid) == 0x50);
const _: () = assert!(std::mem::offset_of!(SysProc, image_buf) == 0x40);
const _: () = assert!(std::mem::offset_of!(SysThread, context_switches) == 0x40);
const _: () = assert!(std::mem::offset_of!(SysThread, state) == 0x44);
const _: () = assert!(std::mem::offset_of!(SysExtThread, win32_start) == 0x60);

pub const STATE_READY: u32 = 1;
pub const STATE_RUNNING: u32 = 2;
pub const STATE_TERMINATED: u32 = 4;
pub const STATE_DEFERRED_READY: u32 = 7;
pub const WAIT_YIELD_EXECUTION: u32 = 33;

pub fn state_name(s: u32) -> &'static str {
    match s {
        0 => "Init",
        1 => "Ready",
        2 => "Run",
        3 => "Standby",
        4 => "Term",
        5 => "Wait",
        6 => "Transition",
        7 => "DefReady",
        _ => "?",
    }
}

/// KWAIT_REASON names for the values that matter when telling spin from work.
pub fn wait_name(w: u32) -> &'static str {
    match w {
        0 => "Executive",
        1 => "FreePage",
        2 => "PageIn",
        4 => "DelayExecution",
        5 => "Suspended",
        6 => "UserRequest",
        7 => "WrExecutive",
        11 => "WrDelayExecution",
        13 => "WrUserRequest",
        15 => "WrQueue",
        16 => "WrLpcReceive",
        17 => "WrLpcReply",
        21 => "WrKeyedEvent",
        27 => "WrResource",
        28 => "WrPushLock",
        30 => "WrQuantumEnd",
        31 => "WrDispatchInt",
        32 => "WrPreempted",
        33 => "WrYieldExecution",
        37 => "WrAlertByThreadId",
        _ => "other",
    }
}

/// One reusable, 8-aligned buffer for the whole-system process+thread snapshot.
pub struct Snapshot {
    buf: Vec<u64>,
    pub bytes: usize,
}

impl Snapshot {
    pub fn new() -> Self {
        Snapshot {
            buf: vec![0; 1 << 17],
            bytes: 0,
        }
    }

    /// ONE syscall for every thread of every process (protected ones included, which a
    /// handle-based walk cannot open). Grows the buffer on STATUS_INFO_LENGTH_MISMATCH.
    pub fn take(&mut self) -> bool {
        use windows_sys::Wdk::System::SystemInformation::NtQuerySystemInformation;
        use windows_sys::Win32::Foundation::STATUS_INFO_LENGTH_MISMATCH;
        loop {
            let mut ret = 0u32;
            // SAFETY: buffer and its length in bytes are passed together.
            let st = unsafe {
                NtQuerySystemInformation(
                    SystemExtendedProcessInformation,
                    self.buf.as_mut_ptr().cast(),
                    (self.buf.len() * 8) as u32,
                    &mut ret,
                )
            };
            if st == STATUS_INFO_LENGTH_MISMATCH {
                let want = (ret as usize).max(self.buf.len() * 8) * 5 / 4 + 65536;
                self.buf.resize(want.div_ceil(8), 0);
                continue;
            }
            self.bytes = if st >= 0 { ret as usize } else { 0 };
            return st >= 0;
        }
    }

    pub fn for_each(&self, mut f: impl FnMut(&SysProc, &[SysExtThread])) {
        if self.bytes == 0 {
            return;
        }
        let base = self.buf.as_ptr() as *const u8;
        let mut off = 0usize;
        loop {
            // SAFETY: the kernel filled a chain of SysProc headers each followed by
            // `threads` SysExtThread records; `next` is the byte offset to the next header.
            let p = unsafe { &*(base.add(off) as *const SysProc) };
            let threads = unsafe {
                std::slice::from_raw_parts(
                    base.add(off + size_of::<SysProc>()) as *const SysExtThread,
                    p.threads as usize,
                )
            };
            f(p, threads);
            if p.next == 0 {
                break;
            }
            off += p.next as usize;
        }
    }
}

pub fn image_name(p: &SysProc) -> String {
    if p.image_buf.is_null() || p.image_len == 0 {
        return if p.pid == 0 {
            "Idle".into()
        } else {
            "?".into()
        };
    }
    // SAFETY: Buffer points into the snapshot buffer, Length is in bytes.
    let s = unsafe { std::slice::from_raw_parts(p.image_buf, p.image_len as usize / 2) };
    String::from_utf16_lossy(s)
}

/// Per-LP idle/kernel/user/DPC/ISR of ONE group.
pub fn lp_times(group: u16, n: usize, out: &mut Vec<LpTimes>) -> bool {
    out.clear();
    out.resize(n, LpTimes::default());
    let bytes = (n * size_of::<LpTimes>()) as u32;
    let mut ret = 0u32;
    // SAFETY: input is the u16 group number; output sized for n entries.
    let st = unsafe {
        NtQuerySystemInformationEx(
            SystemProcessorPerformanceInformation,
            (&group as *const u16).cast(),
            2,
            out.as_mut_ptr().cast(),
            bytes,
            &mut ret,
        )
    };
    st >= 0 && ret == bytes
}
