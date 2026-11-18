//! wall_cpu_probe -- external, read-only CPU probe for winit_wall (2026-11-18).
//!
//! WHY (2026-11-18). On 348279f90d6 the operator saw all 80 logical processors near 100% in
//! Resource Monitor with only `-NumaNode auto`, where the 2026-08/09 runs had measured 54
//! videos at 10-35% of the machine with the deleted thread_cpu_probe. The build has had no
//! CPU readout since the 2026-10-27/28 instrumentation strip, and the old probe had blind
//! spots that matter for exactly this question: one 20 s window, GetSystemTimes for one
//! processor group only, no DPC/ISR, no context switches, and threads that came and went
//! inside the window were lost (+8 to +12 cores "unattributed" on the IPWall runs).
//!
//! A separate process. It never injects into, suspends, or writes to the engine, and the
//! engine does no I/O for it (no-I/O-on-timing-critical-threads rule). Once per interval, on
//! an absolute QPC schedule, it takes:
//!   1. per-LP idle/kernel/user/DPC/ISR for EVERY processor group
//!      NtQuerySystemInformationEx(SystemProcessorPerformanceInformation, &group)   -> CPULP
//!      aggregated per physical core and NUMA node (GetSystemCpuSetInformation)    -> CPUCORE
//!   2. per-LP idle cycles per group (QueryIdleProcessorCycleTimeEx)               -> CPUCYC
//!   3. ONE whole-system process+thread snapshot
//!      NtQuerySystemInformation(SystemExtendedProcessInformation), with the per-LP times
//!      of step 1 read once more right after it (2026-11-18: the CPUPROC reconciliation
//!      uses the mean of the two reads, in charged ticks on both sides)
//!      -> CPU of every process, protected ones too                                 -> CPUPROC
//!         a process that leaves the snapshot is read once more through a
//!         PROCESS_QUERY_LIMITED_INFORMATION handle held since first sight, so the
//!         CPU it used between the last snapshot and its exit is counted (2026-11-18)
//!      -> thread states (ready / running / ready after a yield)                    -> CPUSTATE
//!      -> for the target's threads: kernel/user time, context switches, state,
//!         wait reason, priority, Win32StartAddress
//!   4. per target thread through a handle kept open for the thread's life, accepted only
//!      if its CreateTime and owning pid match the snapshot entry (tid reuse):
//!      QueryThreadCycleTime (the cores figure), GetThreadDescription EVERY sample
//!      (2026-11-18: GStreamer names a pad task at task start and Servo renames the appsink
//!      thread to ServoGstVideo-N later), ideal processor + group affinity for TSV rows -> CPUFAM
//!   5. optional (--pdh): PDH \Processor Information(*)\% Processor Utility|Time|Performance
//!                                                                                   -> CPUUTIL
//! It writes one tagged line per section per interval to <out>.cpu.log, rows for the
//! busiest target threads (top --tsv-top by cores per interval, plus new and exited threads;
//! all of them with --tsv-all; no more rows past --tsv-max-mb) to <out>.cpu.threads.tsv, and
//! the steady-state means (after --warmup-sec) to <out>.cpu.summary.txt in the old
//! thread_cpu_probe table format. A previous run's summary under the same stem is deleted
//! first thing, before any check can fail. It exits by itself when the target exits: the
//! wait between samples IS a wait on the target's process handle.
//!
//! Every line carries t= (seconds since probe start), utc= (GetSystemTimePreciseAsFileTime)
//! and t_eng= (seconds since the target's CreateTime) so it lines up with the err.log.
//!
//! FUTURE WORK (2026-11-18): WHERE a thread actually ran (group/node) cannot be read from
//! outside without ETW -- affinity, CPU sets and the ideal processor are permissions and
//! preferences, and on Windows 11 an unpinned thread runs in either group. A sampled-profile
//! ETW mode (private system-logger session, EVENT_TRACE_FLAG_PROFILE, PerfInfo opcode 46,
//! LP from BufferContext.ProcessorIndex) compiled and ran in the design skeleton
//! (jobs/17ec0401/tmp/wf18/cpuprobe_skel/src/etw.rs), but its sample count (2048 in 3 s
//! against a per-LP row summing to ~0.31 cores) is unexplained and it needs elevation, so it
//! is NOT in this version. Per-group busy in CPULP is where CPU was spent; per-thread
//! placement is not reported.

mod family;
mod nt;
mod pdh;
mod summary;
mod topo;

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs::File;
use std::io::{BufWriter, Write};

use family::{basename, family_of, token};
use nt::{
    LpTimes, STATE_DEFERRED_READY, STATE_READY, STATE_RUNNING, STATE_TERMINATED, Snapshot,
    WAIT_YIELD_EXECUTION, image_name, lp_times, state_name, wait_name,
};
use summary::{Meta, NodeAcc, Validity, Window};
use topo::node_warning;
use windows_sys::Wdk::System::SystemInformation::NtQueryTimerResolution;
use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Kernel::PROCESSOR_NUMBER;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetMappedFileNameW, K32GetModuleInformation, MODULEINFO,
};
use windows_sys::Win32::System::SystemInformation::{
    GROUP_AFFINITY, GetSystemTimePreciseAsFileTime,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentProcessorNumberEx, GetCurrentThread,
    GetCurrentThreadId, GetProcessIdOfThread, GetProcessTimes, GetThreadDescription,
    GetThreadGroupAffinity, GetThreadIdealProcessorEx, GetThreadTimes, OpenProcess, OpenThread,
    PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_QUERY_LIMITED_INFORMATION,
    WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::{
    QueryIdleProcessorCycleTimeEx, QueryProcessCycleTime, QueryThreadCycleTime,
};

const TICKS_PER_SEC: f64 = 10_000_000.0;
const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "usage: wall_cpu_probe [--pid N | --process NAME] [--out STEM] [--interval-ms 1000]
                      [--warmup-sec 8] [--max-sec S] [--top 8] [--pdh] [--tsv-top 64]
                      [--tsv-all] [--tsv-max-mb 256] [--note TEXT]
  --pid N          target process id (the launcher passes this)
  --process NAME   otherwise: the one process whose image name contains NAME (default winit_wall)
  --out STEM       writes STEM.cpu.log, STEM.cpu.threads.tsv, STEM.cpu.summary.txt (default wall_cpu)
  --interval-ms    sample period on an absolute schedule (default 1000, minimum 100)
  --warmup-sec     samples starting before this are left out of the summary (default 8)
  --max-sec        stop after this many seconds even if the target is still running
  --top            other processes named per CPUPROC line (default 8)
  --pdh            also log PDH % Processor Utility/Time/Performance per LP (CPUUTIL)
  --tsv-top N      .cpu.threads.tsv rows per interval: the N busiest target threads by cores
                   among those with >= 0.002 cores or >= 20 context switches/s, plus every
                   new and every exited thread (default 64)
  --tsv-all        write EVERY target thread to .cpu.threads.tsv each interval
  --tsv-max-mb M   stop writing .cpu.threads.tsv (one CPUWARN line) once it reaches M MB
                   (default 256; the .cpu.log lines and the summary are unaffected)
  --note TEXT      free text recorded on CPUTOPO and in the summary (launcher banner)
The probe ends by itself when the target exits.";

struct Args {
    pid: Option<u32>,
    process: String,
    out: String,
    interval_ms: u64,
    warmup_sec: f64,
    max_sec: f64,
    top: usize,
    pdh: bool,
    tsv_top: usize,
    tsv_all: bool,
    tsv_max_mb: f64,
    note: String,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        pid: None,
        process: "winit_wall".into(),
        out: "wall_cpu".into(),
        interval_ms: 1000,
        warmup_sec: 8.0,
        max_sec: f64::INFINITY,
        top: 8,
        pdh: false,
        tsv_top: 64,
        tsv_all: false,
        tsv_max_mb: 256.0,
        note: String::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--pid" => a.pid = Some(value()?.parse().map_err(|_| "--pid must be a number")?),
            "--process" => a.process = value()?,
            "--out" => a.out = value()?,
            "--interval-ms" => {
                a.interval_ms = value()?
                    .parse()
                    .map_err(|_| "--interval-ms must be a number")?
            },
            "--warmup-sec" => {
                a.warmup_sec = value()?
                    .parse()
                    .map_err(|_| "--warmup-sec must be a number")?
            },
            "--max-sec" => {
                a.max_sec = value()?.parse().map_err(|_| "--max-sec must be a number")?
            },
            "--top" => a.top = value()?.parse().map_err(|_| "--top must be a number")?,
            "--pdh" => a.pdh = true,
            "--tsv-top" => {
                a.tsv_top = value()?.parse().map_err(|_| "--tsv-top must be a number")?
            },
            "--tsv-all" => a.tsv_all = true,
            "--tsv-max-mb" => {
                a.tsv_max_mb = value()?
                    .parse()
                    .map_err(|_| "--tsv-max-mb must be a number")?
            },
            "--note" => a.note = value()?,
            "--help" | "-h" => return Err("help".into()),
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    if a.interval_ms < 100 {
        return Err("--interval-ms must be at least 100".into());
    }
    if a.max_sec <= 0.0 {
        return Err("--max-sec must be positive".into());
    }
    if a.tsv_max_mb.is_nan() || a.tsv_max_mb <= 0.0 {
        return Err("--tsv-max-mb must be positive".into());
    }
    Ok(a)
}

// ---------------------------------------------------------------- clocks

fn qpc() -> i64 {
    let mut q = 0i64;
    unsafe { QueryPerformanceCounter(&mut q) };
    q
}

fn qpf() -> i64 {
    let mut f = 0i64;
    unsafe { QueryPerformanceFrequency(&mut f) };
    f.max(1)
}

fn rdtsc() -> u64 {
    // SAFETY: RDTSC is available on every x86_64 CPU.
    unsafe { core::arch::x86_64::_rdtsc() }
}

fn ft(f: FILETIME) -> i64 {
    (((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64) as i64
}

fn now_ft() -> i64 {
    let mut f = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    unsafe { GetSystemTimePreciseAsFileTime(&mut f) };
    ft(f)
}

/// FILETIME (100 ns since 1601-01-01 UTC) -> "2026-11-18T10:15:00.123Z".
fn iso_utc(filetime: i64) -> String {
    let ms_total = filetime.div_euclid(10_000) - 11_644_473_600_000;
    let days = ms_total.div_euclid(86_400_000);
    let ms = ms_total.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// Days since 1970-01-01 -> (year, month, day), proleptic Gregorian (H. Hinnant).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe as i64 + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

// ---------------------------------------------------------------- output

struct Out {
    log: BufWriter<File>,
    tsv: BufWriter<File>,
    q0: i64,
    qf: i64,
    eng_ct: i64,
    /// (2026-11-18) Bytes written to the TSV so far, the --tsv-max-mb cap, and whether the
    /// cap has been hit (then no more rows, one CPUWARN in the log).
    tsv_bytes: u64,
    tsv_cap: u64,
    tsv_stopped: bool,
}

impl Out {
    /// (2026-11-18) Deletes a previous run's <stem>.cpu.summary.txt FIRST, before anything
    /// that can fail: the launcher prints the summary it finds after the run, and a probe that
    /// dies at startup (or cannot even create its log) must not leave an earlier run's summary
    /// standing in for this one. A failed delete is returned for a CPUWARN once the log exists.
    fn create(
        stem: &str,
        q0: i64,
        qf: i64,
        tsv_cap: u64,
    ) -> std::io::Result<(Out, Option<String>)> {
        let summary = format!("{stem}.cpu.summary.txt");
        let stale = match std::fs::remove_file(&summary) {
            Ok(()) => None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => Some(format!(
                "cannot delete the previous {summary} ({e}); if this run writes no summary, that file is an EARLIER run's"
            )),
        };
        let log = BufWriter::new(File::create(format!("{stem}.cpu.log"))?);
        let mut tsv = BufWriter::new(File::create(format!("{stem}.cpu.threads.tsv"))?);
        let header = "t\tutc\tt_eng\ttid\tfamily\tname\tcores\ttick_cores\tkern_cores\tcs_s\tstate\twait\tprio\tideal\taff\tstart_module\tnew";
        writeln!(tsv, "{header}")?;
        Ok((
            Out {
                log,
                tsv,
                q0,
                qf,
                eng_ct: 0,
                tsv_bytes: header.len() as u64 + 1,
                tsv_cap,
                tsv_stopped: false,
            },
            stale,
        ))
    }

    /// One TSV row unless the --tsv-max-mb cap is reached (2026-11-18). The first row that
    /// would cross it writes one CPUWARN to the log instead and switches the TSV off for the
    /// rest of the run. Returns whether the row was written.
    fn tsv_row(&mut self, row: &str, stamp: &str) -> bool {
        if self.tsv_stopped {
            return false;
        }
        let n = row.len() as u64 + 1;
        if self.tsv_bytes + n > self.tsv_cap {
            self.tsv_stopped = true;
            let msg = format!(
                "CPUWARN {stamp} .cpu.threads.tsv reached its {:.2} MB cap (--tsv-max-mb) at {} bytes: no more TSV rows this run; the .cpu.log lines and the summary are unaffected",
                self.tsv_cap as f64 / 1_048_576.0,
                self.tsv_bytes
            );
            self.line(&msg);
            return false;
        }
        self.tsv_bytes += n;
        let _ = writeln!(self.tsv, "{row}");
        true
    }

    fn stamp(&self, q: i64, filetime: i64) -> String {
        let t_eng = if self.eng_ct > 0 {
            format!("{:.3}", (filetime - self.eng_ct) as f64 / TICKS_PER_SEC)
        } else {
            "-".into()
        };
        format!(
            "t={:.3} utc={} t_eng={t_eng}",
            (q - self.q0) as f64 / self.qf as f64,
            iso_utc(filetime)
        )
    }

    fn line(&mut self, s: &str) {
        let _ = writeln!(self.log, "{s}");
    }

    /// Records the reason in the log too: the launcher starts the probe hidden, so stderr
    /// is lost and the .cpu.log is the only place an operator will look.
    fn fatal(&mut self, code: i32, msg: &str) -> ! {
        let st = self.stamp(qpc(), now_ft());
        self.line(&format!("CPUERROR {st} {msg}"));
        let _ = self.log.flush();
        let _ = self.tsv.flush();
        eprintln!("wall_cpu_probe: {msg}");
        std::process::exit(code)
    }
}

// ---------------------------------------------------------------- per-thread helpers

struct Thr {
    handle: HANDLE,
    name: Option<String>,
    module: Option<String>,
    win32_start: usize,
    kt: i64,
    ut: i64,
    cs: u32,
    cyc: u64,
    /// First seen in this sample (counted from zero, or baselined when seen late).
    new: bool,
    /// Probe t at first sight; for the summary's "started during sampling".
    born_t: f64,
    fresh: bool,
}

impl Drop for Thr {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

/// What one GetThreadDescription read did to a thread's name (2026-11-18).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NameRead {
    /// Same name as before, or nothing to read (no handle, read failed, empty name).
    Unchanged,
    /// The thread's first name.
    Named,
    /// A different name from the previous read.
    Renamed,
}

/// Re-reads the thread's description; called for every live target thread EVERY sample
/// (2026-11-18). Names change after creation: GStreamer names a pad-task thread when the task
/// starts ("multiqueue4:src_0"), Servo renames the appsink streaming thread to
/// "ServoGstVideo-N" on its first sample (thread_name.rs), and a pooled GstTask thread is
/// renamed again when reused. Reading only until the first name kept the pad-task name for
/// the whole run. A failed or empty read keeps the last name. No allocation when unchanged.
fn refresh_name(h: HANDLE, name: &mut Option<String>) -> NameRead {
    if h.is_null() {
        return NameRead::Unchanged;
    }
    let mut w: *mut u16 = std::ptr::null_mut();
    // SAFETY: on success the API hands over a LocalAlloc'd, NUL-terminated buffer, freed below.
    if unsafe { GetThreadDescription(h, &mut w) } < 0 || w.is_null() {
        return NameRead::Unchanged;
    }
    let mut n = 0;
    while unsafe { *w.add(n) } != 0 {
        n += 1;
    }
    let s = unsafe { std::slice::from_raw_parts(w, n) };
    let same = name
        .as_deref()
        .is_some_and(|old| old.encode_utf16().eq(s.iter().copied()));
    let r = if n == 0 || same {
        NameRead::Unchanged
    } else {
        let had = name.is_some();
        *name = Some(String::from_utf16_lossy(s));
        if had {
            NameRead::Renamed
        } else {
            NameRead::Named
        }
    };
    unsafe { LocalFree(w as *mut c_void as HLOCAL) };
    r
}

fn cycles(h: HANDLE) -> Option<u64> {
    let mut c = 0u64;
    (!h.is_null() && unsafe { QueryThreadCycleTime(h, &mut c) } != 0).then_some(c)
}

/// (CreateTime, kernel ticks, user ticks) through a thread handle; used for the final times
/// of a thread that is gone from the snapshot (the thread object outlives the thread while a
/// handle is open) and for the identity check when the handle is opened.
fn thread_times(h: HANDLE) -> Option<(i64, i64, i64)> {
    let z = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    (!h.is_null() && unsafe { GetThreadTimes(h, &mut c, &mut e, &mut k, &mut u) } != 0)
        .then(|| (ft(c), ft(k), ft(u)))
}

/// OpenThread for a snapshot entry, accepted only when the handle is that very thread: same
/// CreateTime and same owning process (2026-11-18). Between the snapshot and this call the
/// thread can exit and its tid be handed to a new thread in any process; a handle to that one
/// would charge its cycles (its whole life, for a key counted from zero) to this key, and its
/// final times to the key's exit. Returns (handle or null, rejected for a mismatch); a
/// rejected thread falls back to ticks like one that could not be opened.
fn open_thread_checked(tid: usize, create_time: i64, pid: u32) -> (HANDLE, bool) {
    let h = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid as u32) };
    if h.is_null() {
        return (h, false);
    }
    let same = unsafe { GetProcessIdOfThread(h) } == pid
        && thread_times(h).is_some_and(|(c, _, _)| c == create_time);
    if same {
        (h, false)
    } else {
        unsafe { CloseHandle(h) };
        (std::ptr::null_mut(), true)
    }
}

/// One process identity (pid, CreateTime) as of the previous snapshot (2026-11-18).
struct ProcRec {
    /// Kernel+user ticks and cycles at the previous snapshot (the next interval's baseline).
    tick: i64,
    cyc: u64,
    name: String,
    /// PROCESS_QUERY_LIMITED_INFORMATION (granted for protected processes too), opened at
    /// first sight and checked against the CreateTime; null when refused. Read once more when
    /// the process leaves the snapshot, so the CPU it used between the previous snapshot and
    /// its exit is counted instead of showing up only as unacc.
    handle: HANDLE,
}

impl Drop for ProcRec {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

/// See ProcRec::handle. Null when refused or when the pid already belongs to another process.
fn open_process_checked(pid: usize, create_time: i64) -> HANDLE {
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32) };
    if h.is_null() {
        return h;
    }
    let z = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    if unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) } != 0 && ft(c) == create_time {
        h
    } else {
        unsafe { CloseHandle(h) };
        std::ptr::null_mut()
    }
}

/// Final (kernel+user ticks, cycles) of a process through its handle; the same quantities the
/// snapshot reports (both include the process's exited threads).
fn process_final(h: HANDLE) -> Option<(i64, u64)> {
    if h.is_null() {
        return None;
    }
    let z = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    let mut cyc = 0u64;
    (unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) } != 0
        && unsafe { QueryProcessCycleTime(h, &mut cyc) } != 0)
        .then(|| (ft(k) + ft(u), cyc))
}

/// Over every LP of every group: (kernel + user - idle, DPC + ISR) in 100 ns ticks. These
/// are charged at the same clock ticks as thread and process times, so a difference of two
/// sums is directly comparable with the processes' tick deltas (the unacc reconciliation).
fn lp_sums(v: &[Vec<LpTimes>]) -> (i64, i64) {
    v.iter().flatten().fold((0, 0), |a, t| {
        (a.0 + t.kernel + t.user - t.idle, a.1 + t.dpc + t.isr)
    })
}

/// One candidate .cpu.threads.tsv row of a live target thread (2026-11-18). Kept as data
/// and formatted (with the ideal/affinity readback) only if it makes the --tsv-top cut.
struct TsvRow {
    tid: usize,
    family: String,
    name: Option<String>,
    module: Option<String>,
    handle: HANDLE,
    cores: f64,
    tick: f64,
    kern: f64,
    cs: f64,
    state: u32,
    wait: u32,
    prio: i32,
    /// "new" / "late" on the interval a thread is first seen (always written), else "-".
    tag: &'static str,
}

/// Ideal processor and primary-group affinity: the scheduler's PREFERENCE and PERMISSION,
/// not where the thread ran (Win11 multi-group threads can run outside this group).
fn placement(h: HANDLE) -> (String, String) {
    if h.is_null() {
        return ("-".into(), "-".into());
    }
    let mut p: PROCESSOR_NUMBER = unsafe { std::mem::zeroed() };
    let ideal = if unsafe { GetThreadIdealProcessorEx(h, &mut p) } != 0 {
        format!("{}:{}", p.Group, p.Number)
    } else {
        "-".into()
    };
    let mut a: GROUP_AFFINITY = unsafe { std::mem::zeroed() };
    let aff = if unsafe { GetThreadGroupAffinity(h, &mut a) } != 0 {
        format!("{}:{}", a.Group, a.Mask.count_ones())
    } else {
        "-".into()
    };
    (ideal, aff)
}

/// Module (file name) the address lies in, by the mapped section backing it. Needs
/// PROCESS_QUERY_INFORMATION on the target. Cached per start address: thread-pool and
/// CRT threads share a handful of start routines.
fn start_module(hproc: HANDLE, addr: usize, cache: &mut HashMap<usize, String>) -> String {
    if addr == 0 {
        return String::new();
    }
    cache
        .entry(addr)
        .or_insert_with(|| {
            let mut buf = [0u16; 1024];
            let n = unsafe {
                K32GetMappedFileNameW(
                    hproc,
                    addr as *const c_void,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                )
            };
            if n == 0 {
                "?".into()
            } else {
                basename(&String::from_utf16_lossy(&buf[..n as usize])).to_string()
            }
        })
        .clone()
}

// ---------------------------------------------------------------- startup readback

/// Refuse to report anything if the hand-written ntdll layouts are wrong. Our own process
/// and thread must appear in the snapshot with the creation times the documented APIs give,
/// our own thread must read Running (it is executing the query), and its Win32StartAddress
/// must lie inside our own image (the initial thread starts at the image entry point).
fn self_check(snap: &Snapshot) -> Result<String, String> {
    let me = unsafe { GetCurrentProcessId() } as usize;
    let my_tid = unsafe { GetCurrentThreadId() } as usize;
    let z = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    let (mut tc, mut te, mut tk, mut tu) = (z, z, z, z);
    unsafe { GetThreadTimes(GetCurrentThread(), &mut tc, &mut te, &mut tk, &mut tu) };
    let mut mi: MODULEINFO = unsafe { std::mem::zeroed() };
    let image = unsafe {
        K32GetModuleInformation(
            GetCurrentProcess(),
            GetModuleHandleW(std::ptr::null()),
            &mut mi,
            size_of::<MODULEINFO>() as u32,
        )
    } != 0;
    let (base, size) = (mi.lpBaseOfDll as usize, mi.SizeOfImage as usize);
    let (mut ok_p, mut ok_t, mut ok_state, mut ok_start) = (false, false, false, false);
    let mut found = false;
    snap.for_each(|p, threads| {
        if p.pid != me {
            return;
        }
        found = true;
        ok_p = p.create_time == ft(c) && image_name(p).to_lowercase().ends_with(".exe");
        if let Some(t) = threads.iter().find(|t| t.t.tid == my_tid) {
            ok_t = t.t.create_time == ft(tc) && t.t.pid == me;
            ok_state = t.t.state == STATE_RUNNING;
            ok_start = image && t.win32_start >= base && t.win32_start < base + size;
        }
    });
    if found && ok_p && ok_t && ok_state && ok_start {
        Ok(
            "OK (own process/thread CreateTime, own thread Running, start address in own image)"
                .into(),
        )
    } else {
        Err(format!(
            "ntdll layout readback failed (own process found {found}, process CreateTime/image {ok_p}, thread CreateTime/pid {ok_t}, thread state Running {ok_state}, Win32StartAddress in own image {ok_start}); the SYSTEM_*_INFORMATION layouts in nt.rs do not match this Windows build -- refusing to report"
        ))
    }
}

// ---------------------------------------------------------------- per-sample family sums

#[derive(Default)]
struct Fam {
    n: usize,
    cyc: f64,
    tick: f64,
    kern: f64,
    cs: f64,
    hot: f64,
}

fn arr(v: &[f64]) -> String {
    v.iter()
        .map(|x| format!("{:.0}", 100.0 * x))
        .collect::<Vec<_>>()
        .join(",")
}

// ---------------------------------------------------------------- main

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if e != "help" {
                eprintln!("error: {e}");
            }
            eprintln!("{USAGE}");
            std::process::exit(if e == "help" { 0 } else { 2 });
        },
    };
    let qf = qpf();
    let (q0, tsc0) = (qpc(), rdtsc());
    let tsv_cap = (args.tsv_max_mb * 1_048_576.0) as u64;
    let (mut out, stale_summary) = match Out::create(&args.out, q0, qf, tsv_cap) {
        Ok(o) => o,
        Err(e) => {
            eprintln!(
                "wall_cpu_probe: cannot create {}.cpu.log/.cpu.threads.tsv: {e}",
                args.out
            );
            std::process::exit(1);
        },
    };
    // The machine may be saturated; a starved probe samples late (dt is measured, so the
    // numbers stay honest, but the timeline smears). Above-normal costs the engine nothing
    // measurable at a few ms of work per second.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
    let me = unsafe { GetCurrentProcessId() } as usize;

    let topo = topo::topology();
    let mut snap = Snapshot::new();
    if !snap.take() {
        out.fatal(
            3,
            "NtQuerySystemInformation(SystemExtendedProcessInformation) failed",
        );
    }
    let self_check_text = match self_check(&snap) {
        Ok(s) => s,
        Err(e) => out.fatal(3, &e),
    };

    // ---- target
    let pid = match args.pid {
        Some(p) => p,
        None => {
            let needle = args.process.to_lowercase();
            let mut hits = Vec::new();
            snap.for_each(|p, _| {
                if p.pid != me && p.pid != 0 && image_name(p).to_lowercase().contains(&needle) {
                    hits.push((p.pid as u32, image_name(p)));
                }
            });
            if hits.len() != 1 {
                let list: Vec<String> = hits.iter().map(|(p, n)| format!("{n}({p})")).collect();
                out.fatal(
                    1,
                    &format!(
                        "{} processes match '{}' [{}] -- pass --pid",
                        hits.len(),
                        args.process,
                        list.join(" ")
                    ),
                );
            }
            hits[0].0
        },
    };
    // PROCESS_QUERY_INFORMATION for the start-module lookup (mapped file name); the target is
    // started by the same launcher under the same token, so it is normally granted.
    let mut module_lookup = true;
    let mut hproc = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
    if hproc.is_null() {
        module_lookup = false;
        hproc = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
    }
    if hproc.is_null() {
        out.fatal(
            1,
            &format!("cannot open pid {pid} (gone, or another user's process)"),
        );
    }
    let z = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut pc, mut pe, mut pk, mut pu) = (z, z, z, z);
    unsafe { GetProcessTimes(hproc, &mut pc, &mut pe, &mut pk, &mut pu) };
    let eng_ct = ft(pc);
    let mut target_name = String::from("?");
    let mut ct_ok = false;
    snap.for_each(|p, _| {
        if p.pid == pid as usize {
            target_name = image_name(p);
            ct_ok = p.create_time == eng_ct;
        }
    });
    if !ct_ok {
        out.fatal(
            3,
            &format!("target pid {pid}: snapshot CreateTime does not match GetProcessTimes (layout or pid reuse)"),
        );
    }
    out.eng_ct = eng_ct;
    let tkey = (pid as usize, eng_ct);

    // ---- clocks: TSC rate (cycles -> seconds) and the tick granularity
    let (qa, ta) = (qpc(), rdtsc());
    std::thread::sleep(std::time::Duration::from_millis(50));
    let (qb, tb) = (qpc(), rdtsc());
    let tsc_hz0 = (tb - ta) as f64 / ((qb - qa) as f64 / qf as f64);
    // NtQueryTimerResolution(Maximum, Minimum, Current) in 100 ns: Maximum is the COARSEST
    // period (15.625 ms) -- and that, not the current timer resolution, is the granularity of
    // every tick-charged time here (KernelTime/UserTime/per-LP idle). Seen 2026-11-18 on the
    // dev box with the timer at 1.000 ms: a spinner's kern_cores moved in 0.0156 steps.
    let (mut tmax, mut tmin, mut tcur) = (0u32, 0u32, 0u32);
    unsafe { NtQueryTimerResolution(&mut tmax, &mut tmin, &mut tcur) };
    let tick_ms = tmax.max(tmin) as f64 / 1e4;
    let tick_s = (tick_ms / 1e3).max(1e-4);
    let mut pn: PROCESSOR_NUMBER = unsafe { std::mem::zeroed() };
    unsafe { GetCurrentProcessorNumberEx(&mut pn) };

    let ngroups = topo.groups.len();
    let lpn = topo.lps_per_node();
    let start_q = qpc();
    let start_ft = now_ft();
    let topo_line = format!(
        "CPUTOPO {} lps={} groups={} lps_per_group={:?} nodes={:?} lps_per_node={{{}}} numa_highest_plus1={} cpuset_node_mismatch={} cores={} glpi_cores={} smt={} siblings={} parked_at_start={} tick_ms={:.3} timer_now_ms={:.3} tsc_mhz={:.1} qpc_hz={} probe_on={}:{} target_pid={pid} target={} target_start={} interval_ms={} warmup_sec={} max_sec={} pdh={} tsv={} tsv_max_mb={} module_lookup={} self_check=ok version={VERSION} note=\"{}\"",
        out.stamp(start_q, start_ft),
        topo.lps(),
        ngroups,
        topo.groups,
        topo.nodes,
        lpn.iter()
            .map(|(n, c)| format!("{n}:{c}"))
            .collect::<Vec<_>>()
            .join(","),
        topo.numa_highest_plus1,
        topo.cpuset_node_mismatch,
        topo.cores.len(),
        topo.glpi_cores,
        topo.smt_width(),
        topo.sibling_map(),
        topo.parked.iter().flatten().filter(|&&p| p).count(),
        tick_ms,
        tcur as f64 / 1e4,
        tsc_hz0 / 1e6,
        qf,
        pn.Group,
        pn.Number,
        token(&target_name),
        iso_utc(eng_ct),
        args.interval_ms,
        args.warmup_sec,
        args.max_sec,
        args.pdh as u8,
        if args.tsv_all {
            "all".to_string()
        } else {
            format!("top{}", args.tsv_top)
        },
        args.tsv_max_mb,
        module_lookup as u8,
        args.note.replace('"', "'")
    );
    out.line(&topo_line);
    if let Some(w) = stale_summary {
        out.line(&format!("CPUWARN {} {w}", out.stamp(start_q, start_ft)));
    }
    if topo.cores.len() != topo.glpi_cores {
        out.line(&format!(
            "CPUWARN {} physical-core count differs: CpuSet {} vs GetLogicalProcessorInformationEx {} -- CPUCORE figures may be wrong",
            out.stamp(start_q, start_ft),
            topo.cores.len(),
            topo.glpi_cores
        ));
    }
    if let Some(w) = node_warning(&topo.nodes, topo.numa_highest_plus1) {
        out.line(&format!(
            "CPUWARN {} {w} -- CPUCORE node figures may be wrong",
            out.stamp(start_q, start_ft)
        ));
    }

    let mut pdh = None;
    if args.pdh {
        match pdh::Pdh::open() {
            Ok((p, failed)) => {
                if !failed.is_empty() {
                    out.line(&format!(
                        "CPUWARN {} PDH counters not available: {}",
                        out.stamp(start_q, start_ft),
                        failed.join(",")
                    ));
                }
                pdh = Some(p);
            },
            Err(e) => out.line(&format!("CPUWARN {} {e}", out.stamp(start_q, start_ft))),
        }
    }

    // ---- baselines. (2026-11-18) Same order as every sample: idle cycles, per-LP times,
    // the snapshot, per-LP times again (see the unacc reconciliation in the loop).
    let base_q = qpc();
    let mut prev_lp: Vec<Vec<LpTimes>> = topo.groups.iter().map(|_| Vec::new()).collect();
    let mut cur_lp = prev_lp.clone();
    let mut post_lp = prev_lp.clone();
    let mut prev_idle: Vec<Vec<u64>> = topo.groups.iter().map(|&n| vec![0u64; n]).collect();
    let mut cur_idle = prev_idle.clone();
    let (mut prev_q, mut prev_tsc) = (qpc(), rdtsc());
    for (g, &n) in topo.groups.iter().enumerate() {
        let mut len = (n * 8) as u32;
        unsafe { QueryIdleProcessorCycleTimeEx(g as u16, &mut len, prev_idle[g].as_mut_ptr()) };
    }
    for (g, &n) in topo.groups.iter().enumerate() {
        if !lp_times(g as u16, n, &mut prev_lp[g]) {
            out.fatal(
                3,
                &format!("NtQuerySystemInformationEx(SystemProcessorPerformanceInformation, group {g}) failed or returned a short buffer"),
            );
        }
    }
    // Identical raw arrays for two groups mean the Ex call ignored the group input and every
    // per-LP number would be one group read twice. Untestable on a one-group dev box, so it
    // is checked on the wall at every start.
    for g in 1..ngroups {
        if prev_lp[g] == prev_lp[0] {
            out.fatal(
                3,
                &format!("groups 0 and {g} returned identical per-LP times: the per-group read ignores its group input"),
            );
        }
    }
    let snap_qa = qpc();
    let mut prev_snap_ft = now_ft();
    snap.take();
    let mut prev_snap_q = (snap_qa + qpc()) / 2;
    for (g, &n) in topo.groups.iter().enumerate() {
        lp_times(g as u16, n, &mut post_lp[g]);
    }
    let (b_pre, d_pre) = lp_sums(&prev_lp);
    let (b_post, d_post) = lp_sums(&post_lp);
    // Sums of the pre- and post-snapshot reads (twice the mean), the reconciliation baseline,
    // and the busy ticks charged between the two reads (the quantisation bound's input).
    let (mut prev_recon_busy, mut prev_recon_dpc) = (b_pre + b_post, d_pre + d_post);
    let mut prev_win_busy = (b_post - b_pre).max(0);
    // (pid, CreateTime) -> process as of the previous snapshot, with a handle held for its life
    let mut prev_proc: HashMap<(usize, i64), ProcRec> = HashMap::new();
    // (tid, CreateTime) -> thread record with an open handle
    let mut threads: HashMap<(usize, i64), Thr> = HashMap::new();
    let (mut base_proc_nohandle, mut base_tid_mismatch, mut base_named) = (0usize, 0usize, 0usize);
    snap.for_each(|p, ths| {
        if p.pid == 0 {
            return; // Idle
        }
        let h = open_process_checked(p.pid, p.create_time);
        if h.is_null() {
            base_proc_nohandle += 1;
        }
        prev_proc.insert(
            (p.pid, p.create_time),
            ProcRec {
                tick: p.kernel_time + p.user_time,
                cyc: p.cycle_time,
                name: image_name(p),
                handle: h,
            },
        );
        if (p.pid, p.create_time) != tkey {
            return;
        }
        for th in ths {
            let s = &th.t;
            let (h, mismatch) = open_thread_checked(s.tid, s.create_time, pid);
            if mismatch {
                base_tid_mismatch += 1;
            }
            let mut name = None;
            if refresh_name(h, &mut name) != NameRead::Unchanged {
                base_named += 1;
            }
            threads.insert(
                (s.tid, s.create_time),
                Thr {
                    handle: h,
                    name,
                    module: None,
                    win32_start: th.win32_start,
                    kt: s.kernel_time,
                    ut: s.user_time,
                    cs: s.context_switches,
                    cyc: cycles(h).unwrap_or(0),
                    new: false,
                    born_t: 0.0,
                    fresh: false,
                },
            );
        }
    });
    // (2026-11-18) What the baseline cost and how much of the machine it could open; the
    // per-process handles are what lets an exiting process's last interval be counted.
    let base_ms = (qpc() - base_q) as f64 * 1000.0 / qf as f64;
    out.line(&format!(
        "CPUBASE {} procs={} proc_no_handle={base_proc_nohandle} tgt_threads={} tgt_thread_handles={} tgt_named={base_named} tid_mismatch={base_tid_mismatch} ms={base_ms:.2}",
        out.stamp(qpc(), now_ft()),
        prev_proc.len(),
        threads.len(),
        threads.values().filter(|t| !t.handle.is_null()).count()
    ));
    let _ = out.log.flush();

    // A thread or process first seen now counts from ZERO when it was created after the
    // previous snapshot (minus one tick of slack: CreateTime is the coarse system time).
    let slack = (tmax.max(tmin) as i64).max(156_250) + 10_000;
    let interval_q = args.interval_ms as i64 * qf / 1000;
    let mut k = 1i64;
    let mut sample_no = 0usize;
    let mut mod_cache: HashMap<usize, String> = HashMap::new();
    let mut dead: HashSet<(usize, i64)> = HashSet::new();
    let mut win = Window {
        lp_busy: topo.groups.iter().map(|&n| vec![0.0; n]).collect(),
        lp_cyc_busy: topo.groups.iter().map(|&n| vec![0.0; n]).collect(),
        ..Default::default()
    };
    for c in &topo.cores {
        win.nodes.entry(c.node).or_default().cores += 1;
    }
    for (n, c) in &lpn {
        win.nodes.entry(*n).or_default().lps = *c;
    }
    let mut val = Validity {
        coverage_min: vec![f64::INFINITY; ngroups],
        coverage_max: vec![0.0; ngroups],
        coverage_sum: vec![0.0; ngroups],
        proc_no_handle_at_start: base_proc_nohandle,
        ..Default::default()
    };
    let mut end_reason = String::from("target exited");

    loop {
        // ---- wait: the inter-sample sleep IS a wait on the target's process handle
        let deadline = q0 + k * interval_q;
        k += 1;
        let now = qpc();
        let wait_ms = if deadline > now {
            ((deadline - now) * 1000 + qf - 1) / qf
        } else {
            0
        };
        let exited = unsafe { WaitForSingleObject(hproc, wait_ms as u32) } == WAIT_OBJECT_0;
        let work_q0 = qpc();
        // A starved probe does not burst to catch up: the next deadline stays on the grid.
        while q0 + k * interval_q <= work_q0 {
            k += 1;
            val.skipped_deadlines += 1;
        }
        sample_no += 1;

        // ---- 1+2: idle cycles, then per-LP times right before the snapshot, every group
        let now_q = qpc();
        let now_fts = now_ft();
        let now_tsc = rdtsc();
        for (g, &n) in topo.groups.iter().enumerate() {
            let mut len = (n * 8) as u32;
            unsafe { QueryIdleProcessorCycleTimeEx(g as u16, &mut len, cur_idle[g].as_mut_ptr()) };
        }
        let mut lp_ok = true;
        let lp_q0 = qpc();
        for (g, &n) in topo.groups.iter().enumerate() {
            lp_ok &= lp_times(g as u16, n, &mut cur_lp[g]);
        }
        // ---- 3: one snapshot of everything, then the per-LP times again
        let snap_qa = qpc();
        let snap_ft = now_ft();
        let snap_ok = snap.take();
        let snap_qb = qpc();
        for (g, &n) in topo.groups.iter().enumerate() {
            lp_ok &= lp_times(g as u16, n, &mut post_lp[g]);
        }
        let lp_q1 = qpc();
        let snap_q = (snap_qa + snap_qb) / 2;

        let dt = (now_q - prev_q) as f64 / qf as f64;
        let dt_snap = ((snap_q - prev_snap_q) as f64 / qf as f64).max(1e-6);
        let t = (now_q - q0) as f64 / qf as f64;
        let tsc_hz = (now_tsc - tsc0) as f64 / ((now_q - q0) as f64 / qf as f64);
        let dtsc = (now_tsc - prev_tsc) as f64;
        let stamp = out.stamp(now_q, now_fts);
        let t_eng = (now_fts - eng_ct) as f64 / TICKS_PER_SEC;
        let in_valid = sample_no > 1 && !exited;
        let in_window = in_valid && t - dt >= args.warmup_sec - 1e-3;
        if in_window {
            if win.samples == 0 {
                win.t_start = t - dt;
                win.t_eng_start = t_eng - dt;
            }
            win.t_end = t;
            win.t_eng_end = t_eng;
        }

        // ---- the unacc reconciliation (2026-11-18). Per-LP busy and process times are both
        // charged at clock ticks (15.625 ms), so both sides are taken as charged ticks over the
        // SAME interval and divided by the same dt. (Before, per-LP busy was each LP's own
        // busy/total fraction -- exactly 1.0 for a busy LP whether its interval held 63, 64 or
        // 65 ticks -- against process ticks over wall time, so every busy LP added up to one
        // tick of noise: unacc up to +0.25 with 16 spinners on the dev box, ~1.25 cores at 80.)
        // What remains is a tick charged while the per-LP reads straddle the snapshot: against
        // the mean of the reads before and after it, each such busy tick is off by exactly half
        // a tick (the snapshot read its thread before or after it). So |unacc| is at most half
        // the busy ticks charged inside this window plus the previous one -- q_bound, measured
        // every sample, no model. Seen on the dev box: the tick is charged on EVERY LP at the
        // same instant, so when it falls inside the window all busy LPs move together
        // (+-busy/2 ticks: 0.12 cores at 16 busy LPs) and the next sample takes it back.
        let (b_pre, d_pre) = lp_sums(&cur_lp);
        let (b_post, d_post) = lp_sums(&post_lp);
        let (recon_busy, recon_dpc) = (b_pre + b_post, d_pre + d_post);
        let machine_ticks = (recon_busy - prev_recon_busy) as f64 / 2.0 / TICKS_PER_SEC / dt_snap;
        let dpcisr_ticks = (recon_dpc - prev_recon_dpc) as f64 / 2.0 / TICKS_PER_SEC / dt_snap;
        let recon_window = (lp_q1 - lp_q0) as f64 / qf as f64;
        let win_busy = (b_post - b_pre).max(0);
        let q_bound = (win_busy + prev_win_busy) as f64 / 2.0 / TICKS_PER_SEC / dt_snap;
        let win_ticks = win_busy as f64 / (tick_s * TICKS_PER_SEC);
        let snap_ms = (snap_qb - snap_qa) as f64 * 1000.0 / qf as f64;

        // ---- per LP
        let mut busy: Vec<Vec<f64>> = Vec::with_capacity(ngroups);
        let mut line_lp = format!("CPULP {stamp} dt={dt:.3}");
        let mut line_cyc = format!("CPUCYC {stamp}");
        let (mut machine, mut dpcisr, mut machine_cyc) = (0.0f64, 0.0f64, 0.0f64);
        let mut identical = false;
        for g in 0..ngroups {
            let (mut gsum, mut gcyc, mut gdpc, mut gtotal) = (0.0, 0.0, 0.0, 0.0);
            let mut row = Vec::with_capacity(topo.groups[g]);
            let mut crow = Vec::with_capacity(topo.groups[g]);
            for (i, (c, p)) in cur_lp[g].iter().zip(&prev_lp[g]).enumerate() {
                let total = ((c.kernel - p.kernel) + (c.user - p.user)) as f64;
                let idle = (c.idle - p.idle) as f64;
                let b = if total > 0.0 {
                    ((total - idle) / total).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                gdpc += ((c.dpc - p.dpc) + (c.isr - p.isr)) as f64 / TICKS_PER_SEC / dt;
                gtotal += total / TICKS_PER_SEC;
                gsum += b;
                row.push(b);
                let ib = cur_idle[g][i].wrapping_sub(prev_idle[g][i]) as f64;
                let cb = if dtsc > 0.0 {
                    (1.0 - ib / dtsc).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                gcyc += cb;
                crow.push(cb);
            }
            if g > 0 && cur_lp[g] == cur_lp[0] {
                identical = true;
            }
            let coverage = gtotal / (topo.groups[g] as f64 * dt);
            machine += gsum;
            machine_cyc += gcyc;
            dpcisr += gdpc;
            line_lp += &format!(
                " g{g}=[{}] g{g}_cores={gsum:.2} g{g}_dpc_isr={gdpc:.2}",
                arr(&row)
            );
            line_cyc += &format!(" g{g}=[{}] g{g}_cores={gcyc:.2}", arr(&crow));
            if in_valid {
                val.coverage_sum[g] += coverage;
                val.coverage_min[g] = val.coverage_min[g].min(coverage);
                val.coverage_max[g] = val.coverage_max[g].max(coverage);
            }
            if in_window {
                for (i, (b, cb)) in row.iter().zip(&crow).enumerate() {
                    win.lp_busy[g][i] += b * dt;
                    win.lp_cyc_busy[g][i] += cb * dt;
                }
            }
            busy.push(row);
        }
        line_lp += &format!(" machine_cores={machine:.2} dpc_isr_cores={dpcisr:.2}");
        if !lp_ok {
            line_lp += " read=FAILED";
        }
        line_cyc += &format!(
            " machine_cores={machine_cyc:.2} tsc_mhz={:.1}",
            tsc_hz / 1e6
        );
        out.line(&line_lp);

        // ---- per physical core / node
        let (all, per_node) = topo::core_stats(&topo, &busy);
        let mut line_core = format!(
            "CPUCORE {stamp} cores={} all_siblings90={} one_sibling90={} lp90={}",
            topo.cores.len(),
            all.both90,
            all.one90,
            all.lp90
        );
        for (node, mean, s) in &per_node {
            line_core += &format!(
                " | node{node} busy%={:.0} all_siblings90={} one_sibling90={} lp90={}",
                100.0 * mean,
                s.both90,
                s.one90,
                s.lp90
            );
            if in_window {
                let a: &mut NodeAcc = win.nodes.entry(*node).or_default();
                a.busy += mean * dt;
                a.both90 += s.both90 as f64 * dt;
                a.one90 += s.one90 as f64 * dt;
                a.lp90 += s.lp90 as f64 * dt;
            }
        }
        out.line(&line_core);
        out.line(&line_cyc);
        if let Some(p) = pdh.as_mut() {
            for (label, vals) in p.sample(&topo.groups) {
                let mut l = format!("CPUUTIL {stamp} counter={label}");
                let (mut sum, mut cnt) = (0.0, 0usize);
                for (g, row) in vals.iter().enumerate() {
                    let s: Vec<String> = row
                        .iter()
                        .map(|v| {
                            if v.is_nan() {
                                "-".into()
                            } else {
                                format!("{v:.0}")
                            }
                        })
                        .collect();
                    l += &format!(" g{g}=[{}]", s.join(","));
                    for v in row.iter().filter(|v| !v.is_nan()) {
                        sum += v;
                        cnt += 1;
                    }
                }
                l += &format!(" mean={:.1}", sum / cnt.max(1) as f64);
                out.line(&l);
            }
        }

        // ---- per process (all of them; protected ones included) + target threads
        let mut by_name: HashMap<String, f64> = HashMap::new();
        let (mut procs_tick, mut procs_cyc) = (0.0f64, 0.0f64);
        let (mut tgt_tick, mut tgt_cyc) = (0.0f64, 0.0f64);
        let (mut probe_tick, mut probe_cyc) = (0.0f64, 0.0f64);
        let mut next_proc: HashMap<(usize, i64), ProcRec> =
            HashMap::with_capacity(prev_proc.len() + 8);
        let (mut procs_started, mut procs_new_nohandle) = (0usize, 0usize);
        let (mut sys_ready, mut sys_run) = (0usize, 0usize);
        let (mut tgt_run, mut tgt_ready, mut tgt_yield, mut tgt_alive) = (0, 0, 0, 0usize);
        let (mut started, mut late, mut nohandle, mut unnamed) = (0usize, 0usize, 0usize, 0usize);
        let (mut renamed, mut tid_mismatch, mut names_q) = (0usize, 0usize, 0i64);
        let mut fams: HashMap<String, Fam> = HashMap::new();
        let mut seen: HashSet<(usize, i64)> = HashSet::with_capacity(threads.len() + 8);
        let mut still_dead: HashSet<(usize, i64)> = HashSet::new();
        let mut terminated: Vec<(usize, i64)> = Vec::new();
        let mut rows: Vec<TsvRow> = Vec::new();
        let tsv_prefix = format!("{t:.3}\t{}\t{t_eng:.3}", iso_utc(now_fts));
        let fresh_after = prev_snap_ft - slack;
        snap.for_each(|p, ths| {
            if p.pid == 0 {
                return; // Idle: its threads ARE the per-LP idle time
            }
            for th in ths {
                match th.t.state {
                    STATE_RUNNING => sys_run += 1,
                    STATE_READY | STATE_DEFERRED_READY => sys_ready += 1,
                    _ => {},
                }
            }
            let cpu = p.kernel_time + p.user_time;
            let key = (p.pid, p.create_time);
            let (b_tick, b_cyc, rec) = match prev_proc.remove(&key) {
                Some(mut r) => {
                    let b = (r.tick, r.cyc);
                    (r.tick, r.cyc) = (cpu, p.cycle_time);
                    (b.0, b.1, r)
                },
                None => {
                    let fresh = p.create_time > fresh_after;
                    if fresh {
                        procs_started += 1;
                    }
                    let h = open_process_checked(p.pid, p.create_time);
                    if h.is_null() {
                        procs_new_nohandle += 1;
                    }
                    // Started after the previous snapshot: its whole life is inside.
                    let (bt, bc) = if fresh { (0, 0) } else { (cpu, p.cycle_time) };
                    let r = ProcRec {
                        tick: cpu,
                        cyc: p.cycle_time,
                        name: image_name(p),
                        handle: h,
                    };
                    (bt, bc, r)
                },
            };
            let tick_sec = (cpu - b_tick).max(0) as f64 / TICKS_PER_SEC;
            let cyc_sec = p.cycle_time.saturating_sub(b_cyc) as f64 / tsc_hz;
            procs_tick += tick_sec / dt_snap;
            procs_cyc += cyc_sec / dt_snap;
            if in_window {
                win.add_proc(key, &rec.name, tick_sec, cyc_sec, false);
            }
            if p.pid == me {
                (probe_tick, probe_cyc) = (tick_sec / dt_snap, cyc_sec / dt_snap);
            }
            if key != tkey {
                *by_name.entry(rec.name.clone()).or_default() += tick_sec / dt_snap;
                next_proc.insert(key, rec);
                return;
            }
            next_proc.insert(key, rec);
            (tgt_tick, tgt_cyc) = (tick_sec / dt_snap, cyc_sec / dt_snap);
            for th in ths {
                let s = &th.t;
                let key = (s.tid, s.create_time);
                if dead.contains(&key) {
                    still_dead.insert(key);
                    continue;
                }
                seen.insert(key);
                tgt_alive += 1;
                match s.state {
                    STATE_RUNNING => tgt_run += 1,
                    STATE_READY | STATE_DEFERRED_READY => {
                        tgt_ready += 1;
                        if s.wait_reason == WAIT_YIELD_EXECUTION {
                            tgt_yield += 1;
                        }
                    },
                    _ => {},
                }
                if !threads.contains_key(&key) {
                    let (h, mismatch) = open_thread_checked(s.tid, s.create_time, pid);
                    if mismatch {
                        tid_mismatch += 1;
                    }
                    if h.is_null() {
                        nohandle += 1;
                    }
                    let fresh = s.create_time > fresh_after;
                    if fresh {
                        started += 1;
                    } else {
                        late += 1;
                    }
                    let (kt, ut, cs, cyc) = if fresh {
                        (0, 0, 0, 0)
                    } else {
                        (
                            s.kernel_time,
                            s.user_time,
                            s.context_switches,
                            cycles(h).unwrap_or(0),
                        )
                    };
                    threads.insert(
                        key,
                        Thr {
                            handle: h,
                            name: None,
                            module: None,
                            win32_start: th.win32_start,
                            kt,
                            ut,
                            cs,
                            cyc,
                            new: true,
                            born_t: t,
                            fresh,
                        },
                    );
                }
                let e = threads.get_mut(&key).expect("inserted above");
                // (2026-11-18) EVERY sample, not only while unnamed: see refresh_name.
                let nq = qpc();
                if refresh_name(e.handle, &mut e.name) == NameRead::Renamed {
                    renamed += 1;
                }
                names_q += qpc() - nq;
                if e.name.is_none() {
                    unnamed += 1;
                    if e.module.is_none() && module_lookup {
                        e.module = Some(start_module(hproc, e.win32_start, &mut mod_cache));
                    }
                }
                let d_k = (s.kernel_time - e.kt).max(0) as f64 / TICKS_PER_SEC;
                let d_u = (s.user_time - e.ut).max(0) as f64 / TICKS_PER_SEC;
                let d_cs = s.context_switches.wrapping_sub(e.cs) as f64;
                let cyc_now = cycles(e.handle);
                // No handle: fall back to ticks for this thread (counted in CPUSTATE).
                let d_cyc = match cyc_now {
                    Some(c) => c.saturating_sub(e.cyc) as f64 / tsc_hz,
                    None => d_k + d_u,
                };
                (e.kt, e.ut, e.cs) = (s.kernel_time, s.user_time, s.context_switches);
                if let Some(c) = cyc_now {
                    e.cyc = c;
                }
                let fam = family_of(e.name.as_deref(), e.module.as_deref());
                let f = fams.entry(fam.clone()).or_default();
                let (cores, tick) = (d_cyc / dt_snap, (d_k + d_u) / dt_snap);
                f.n += 1;
                f.cyc += cores;
                f.tick += tick;
                f.kern += d_k / dt_snap;
                f.cs += d_cs / dt_snap;
                f.hot = f.hot.max(cores);
                if in_window {
                    win.add_thread(
                        key,
                        e.name.as_deref(),
                        &fam,
                        d_cyc,
                        d_k + d_u,
                        e.fresh && e.born_t > win.t_start,
                        true,
                    );
                }
                if args.tsv_all || cores >= 0.002 || d_cs / dt_snap >= 20.0 || e.new {
                    rows.push(TsvRow {
                        tid: s.tid,
                        family: fam,
                        name: e.name.clone(),
                        module: e.module.clone(),
                        handle: e.handle,
                        cores,
                        tick,
                        kern: d_k / dt_snap,
                        cs: d_cs / dt_snap,
                        state: s.state,
                        wait: s.wait_reason,
                        prio: s.priority,
                        tag: match (e.new, e.fresh) {
                            (true, true) => "new",
                            (true, false) => "late",
                            _ => "-",
                        },
                    });
                }
                e.new = false;
                if s.state == STATE_TERMINATED {
                    terminated.push(key);
                }
            }
        });

        // ---- TSV rows of live threads (2026-11-18): the --tsv-top busiest by cores, plus
        // every new thread; all of them with --tsv-all. Formatted here, while every handle in
        // `rows` is still open (the exits below close some).
        rows.sort_by(|a, b| b.cores.total_cmp(&a.cores));
        let (mut kept, mut tsv_skipped) = (0usize, 0usize);
        let mut tsv_lines: Vec<String> = Vec::with_capacity(rows.len().min(args.tsv_top + 16));
        for r in &rows {
            let must = r.tag != "-";
            if !(must || args.tsv_all || kept < args.tsv_top) {
                tsv_skipped += 1;
                continue;
            }
            if !must {
                kept += 1;
            }
            let (ideal, aff) = placement(r.handle);
            tsv_lines.push(format!(
                "{tsv_prefix}\t{}\t{}\t{}\t{:.4}\t{:.4}\t{:.4}\t{:.0}\t{}\t{}\t{}\t{ideal}\t{aff}\t{}\t{}",
                r.tid,
                token(&r.family),
                r.name.as_deref().map(token).unwrap_or_else(|| "-".into()),
                r.cores,
                r.tick,
                r.kern,
                r.cs,
                state_name(r.state),
                wait_name(r.wait),
                r.prio,
                r.module.as_deref().unwrap_or("-"),
                r.tag
            ));
        }
        drop(rows);

        // ---- processes that left the snapshot (2026-11-18): the CPU they used between the
        // previous snapshot and their exit, through the handle held since first sight. Exited
        // processes are not in the snapshot even while a handle keeps the object alive
        // (checked on the dev box), so leaving the snapshot IS the exit. The target's last
        // interval comes through here too (its own handle is the fallback).
        let (mut procs_exited, mut procs_exit_unread) = (0usize, 0usize);
        let mut exited_by_name: HashMap<String, (f64, usize)> = HashMap::new();
        for (key, rec) in prev_proc.drain() {
            procs_exited += 1;
            let h = if rec.handle.is_null() && key == tkey {
                hproc
            } else {
                rec.handle
            };
            let e = exited_by_name.entry(rec.name.clone()).or_default();
            e.1 += 1;
            let Some((tick, cyc)) = process_final(h) else {
                procs_exit_unread += 1;
                continue;
            };
            let tick_sec = (tick - rec.tick).max(0) as f64 / TICKS_PER_SEC;
            let cyc_sec = cyc.saturating_sub(rec.cyc) as f64 / tsc_hz;
            e.0 += tick_sec / dt_snap;
            procs_tick += tick_sec / dt_snap;
            procs_cyc += cyc_sec / dt_snap;
            if in_window {
                win.add_proc(key, &rec.name, tick_sec, cyc_sec, true);
            }
            if key == tkey {
                (tgt_tick, tgt_cyc) = (tick_sec / dt_snap, cyc_sec / dt_snap);
            } else {
                *by_name.entry(rec.name.clone()).or_default() += tick_sec / dt_snap;
            }
        }
        prev_proc = next_proc;

        // ---- threads that left: last partial interval through the handle we still hold
        let mut exited_n = 0usize;
        let gone: Vec<(usize, i64)> = threads
            .keys()
            .filter(|k| !seen.contains(k))
            .copied()
            .chain(terminated.iter().copied())
            .collect();
        for key in gone {
            let Some(mut e) = threads.remove(&key) else {
                continue;
            };
            exited_n += 1;
            if terminated.contains(&key) {
                // Already counted from the snapshot's final values above.
                dead.insert(key);
                still_dead.insert(key);
                continue;
            }
            // A rename in its last interval still counts: the description outlives the thread.
            if refresh_name(e.handle, &mut e.name) == NameRead::Renamed {
                renamed += 1;
            }
            let (d_k, d_u) = match thread_times(e.handle) {
                Some((_, kt, ut)) => (
                    (kt - e.kt).max(0) as f64 / TICKS_PER_SEC,
                    (ut - e.ut).max(0) as f64 / TICKS_PER_SEC,
                ),
                None => (0.0, 0.0),
            };
            let d_cyc = match cycles(e.handle) {
                Some(c) => c.saturating_sub(e.cyc) as f64 / tsc_hz,
                None => d_k + d_u,
            };
            let fam = family_of(e.name.as_deref(), e.module.as_deref());
            let f = fams.entry(fam.clone()).or_default();
            let cores = d_cyc / dt_snap;
            f.cyc += cores;
            f.tick += (d_k + d_u) / dt_snap;
            f.kern += d_k / dt_snap;
            f.hot = f.hot.max(cores);
            if in_window {
                win.add_thread(
                    key,
                    e.name.as_deref(),
                    &fam,
                    d_cyc,
                    d_k + d_u,
                    e.fresh && e.born_t > win.t_start,
                    false,
                );
            }
            // Every exited thread is written (2026-11-18), whatever --tsv-top says.
            tsv_lines.push(format!(
                "{tsv_prefix}\t{}\t{}\t{}\t{cores:.4}\t{:.4}\t{:.4}\t-\tExited\t-\t-\t-\t-\t{}\t-",
                key.0,
                token(&fam),
                e.name.as_deref().map(token).unwrap_or_else(|| "-".into()),
                (d_k + d_u) / dt_snap,
                d_k / dt_snap,
                e.module.as_deref().unwrap_or("-")
            ));
        }
        dead.retain(|k| still_dead.contains(k));
        let mut tsv_written = 0usize;
        for l in &tsv_lines {
            if out.tsv_row(l, &stamp) {
                tsv_written += 1;
            }
        }
        let names_ms = names_q as f64 * 1000.0 / qf as f64;

        // ---- CPUPROC
        let mut others: Vec<(String, f64)> =
            by_name.into_iter().filter(|(_, c)| *c >= 0.01).collect();
        others.sort_by(|a, b| b.1.total_cmp(&a.1));
        let shown: f64 = others.iter().take(args.top).map(|o| o.1).sum();
        let top: Vec<String> = others
            .iter()
            .take(args.top)
            .map(|(n, c)| format!("{}={c:.2}", token(n)))
            .collect();
        let mut ex: Vec<(String, (f64, usize))> = exited_by_name.into_iter().collect();
        ex.sort_by(|a, b| b.1.0.total_cmp(&a.1.0).then(b.1.1.cmp(&a.1.1)));
        let ex_top: Vec<String> = ex
            .iter()
            .take(4)
            .map(|(n, (c, cnt))| format!("{}={c:.2}/{cnt}", token(n)))
            .collect();
        let rest = procs_tick - tgt_tick - shown;
        let unacc = machine_ticks - procs_tick - dpcisr_ticks;
        let work_ms = (qpc() - work_q0) as f64 * 1000.0 / qf as f64;
        out.line(&format!(
            "CPUPROC {stamp} machine={machine:.2} machine_ticks={machine_ticks:.2} procs={procs_tick:.2} target={tgt_tick:.2} dpc_isr={dpcisr_ticks:.2} unacc={unacc:+.2} q_bound={q_bound:.3} win_ticks={win_ticks:.0} procs_started={procs_started} procs_exited={procs_exited} procs_exit_unread={procs_exit_unread} exited: {} top: {} rest={rest:.2} | cyc: machine={machine_cyc:.2} procs={procs_cyc:.2} target={tgt_cyc:.2} | probe_ms={work_ms:.2} snap_ms={snap_ms:.2} names_ms={names_ms:.3} probe_cores={probe_cyc:.4} probe_tick={probe_tick:.3}",
            if ex_top.is_empty() {
                "-".to_string()
            } else {
                ex_top.join(" ")
            },
            top.join(" ")
        ));

        // ---- CPUSTATE
        out.line(&format!(
            "CPUSTATE {stamp} sys_ready={sys_ready} sys_run={sys_run} tgt_threads={tgt_alive} tgt_run={tgt_run} tgt_ready={tgt_ready} tgt_ready_after_yield={tgt_yield} started={started} exited={exited_n} first_seen_late={late} unnamed={unnamed} renamed={renamed} no_handle={nohandle} tid_mismatch={tid_mismatch} tsv_rows={tsv_written} tsv_skipped={tsv_skipped} snapshot_kb={}{}",
            snap.bytes / 1024,
            if exited { " final=1 (target exited)" } else { "" }
        ));

        // ---- CPUFAM
        let (sum_cyc, sum_tick): (f64, f64) = fams
            .values()
            .fold((0.0, 0.0), |a, f| (a.0 + f.cyc, a.1 + f.tick));
        let mut fl: Vec<(String, Fam)> = fams.into_iter().collect();
        fl.sort_by(|a, b| b.1.cyc.total_cmp(&a.1.cyc));
        let mut parts = Vec::new();
        let (mut other_n, mut other_c, mut other_f) = (0usize, 0.0, 0usize);
        for (n, f) in &fl {
            if f.cyc >= 0.005 || f.tick >= 0.005 {
                let kern = if f.tick > 0.0 {
                    format!("{:.0}%", 100.0 * f.kern / f.tick)
                } else {
                    "-".into()
                };
                parts.push(format!(
                    "{}={}/{:.3}/t{:.3}/k{kern}/cs{:.0}/hot{:.3}",
                    token(n),
                    f.n,
                    f.cyc,
                    f.tick,
                    f.cs,
                    f.hot
                ));
            } else {
                other_f += 1;
                other_n += f.n;
                other_c += f.cyc;
            }
        }
        out.line(&format!(
            "CPUFAM {stamp} target={tgt_cyc:.3} threads_sum={sum_cyc:.3} tgt_unattributed={:+.3} (tick: target={tgt_tick:.3} threads_sum={sum_tick:.3} tgt_unattributed={:+.3}) | {} | quiet_families={other_f}/{other_n}/{other_c:.3}",
            tgt_cyc - sum_cyc,
            tgt_tick - sum_tick,
            parts.join(" ")
        ));

        // ---- accumulate
        if in_valid {
            if val.samples == 0 {
                val.unacc_edge_first = prev_win_busy as f64 / 2.0 / TICKS_PER_SEC;
            }
            val.unacc_edge_last = win_busy as f64 / 2.0 / TICKS_PER_SEC;
            val.samples += 1;
            val.unacc_sum += unacc * dt_snap;
            val.unacc_secs += dt_snap;
            val.unacc_max = val.unacc_max.max(unacc.abs());
            val.q_bound_sum += q_bound;
            val.q_bound_max = val.q_bound_max.max(q_bound);
            if unacc.abs() > q_bound + 0.02 {
                val.unacc_beyond += 1;
            }
            val.busy_lps_sum += machine_ticks;
            val.recon_ms_sum += recon_window * 1000.0;
            val.win_ticks_sum += win_ticks;
            val.probe_ms_sum += work_ms;
            val.probe_ms_max = val.probe_ms_max.max(work_ms);
            val.names_ms_sum += names_ms;
            val.names_ms_max = val.names_ms_max.max(names_ms);
            val.probe_cyc_sum += probe_cyc;
            val.probe_cyc_max = val.probe_cyc_max.max(probe_cyc);
            if dt > 1.5 * args.interval_ms as f64 / 1000.0 {
                val.late += 1;
            }
            if identical {
                val.groups_identical += 1;
            }
            if !lp_ok || !snap_ok {
                val.lp_read_failures += 1;
            }
            // Same samples as the rest of the block: the exit interval would count the whole
            // process teardown as thread exits.
            val.started += started;
            val.exited += exited_n;
            val.first_seen_late += late;
            val.no_handle += nohandle;
            val.renamed += renamed;
            val.tid_mismatch += tid_mismatch;
            val.procs_started += procs_started;
            val.procs_exited += procs_exited;
            val.procs_exit_unread += procs_exit_unread;
            val.procs_new_no_handle += procs_new_nohandle;
        }
        val.tsv_rows += tsv_written;
        val.tsv_skipped += tsv_skipped;
        if in_window {
            win.samples += 1;
            win.secs += dt;
            win.secs_snap += dt_snap;
            win.machine += machine * dt;
            win.machine_cyc += machine_cyc * dt;
            win.machine_ticks += machine_ticks * dt_snap;
            win.dpc_isr += dpcisr_ticks * dt_snap;
            win.procs_tick += procs_tick * dt_snap;
            win.procs_cyc += procs_cyc * dt_snap;
            win.unacc += unacc * dt_snap;
            win.both90 += all.both90 as f64 * dt;
            win.one90 += all.one90 as f64 * dt;
            win.lp90 += all.lp90 as f64 * dt;
            win.target_tick += tgt_tick * dt_snap;
            win.target_cyc += tgt_cyc * dt_snap;
            win.started += started;
            win.exited += exited_n;
            win.procs_started += procs_started;
            win.procs_exited += procs_exited;
            win.tgt_unattr_cyc += (tgt_cyc - sum_cyc) * dt_snap;
            win.tgt_unattr_tick += (tgt_tick - sum_tick) * dt_snap;
        }

        let _ = out.log.flush(); // a force-kill of the probe loses at most one interval
        let _ = out.tsv.flush();
        std::mem::swap(&mut prev_lp, &mut cur_lp);
        std::mem::swap(&mut prev_idle, &mut cur_idle);
        (prev_q, prev_tsc) = (now_q, now_tsc);
        (prev_snap_q, prev_snap_ft) = (snap_q, snap_ft);
        (prev_recon_busy, prev_recon_dpc) = (recon_busy, recon_dpc);
        prev_win_busy = win_busy;
        if exited {
            break;
        }
        if t >= args.max_sec {
            end_reason = format!("--max-sec {} reached (target still running)", args.max_sec);
            break;
        }
    }

    // ---- summary
    val.tsv_stopped = out.tsv_stopped;
    let meta = Meta {
        label: format!("{target_name} (pid {pid})"),
        cpus: topo.lps(),
        groups: topo.groups.clone(),
        nodes: topo.nodes.len(),
        cores: topo.cores.len(),
        smt: topo.smt_width(),
        interval_ms: args.interval_ms,
        warmup_sec: args.warmup_sec,
        note: args.note.clone(),
        target_start_utc: iso_utc(eng_ct),
        tick_ms,
        self_check: self_check_text,
        pdh: pdh.is_some(),
        end_reason,
        tsv_top: args.tsv_top,
        tsv_all: args.tsv_all,
        tsv_max_mb: args.tsv_max_mb,
    };
    let text = summary::render(&meta, &win, &val);
    let path = format!("{}.cpu.summary.txt", args.out);
    match File::create(&path).and_then(|mut f| f.write_all(text.as_bytes())) {
        Ok(()) => println!("wall_cpu_probe: summary {path}"),
        Err(e) => {
            out.line(&format!(
                "CPUERROR {} cannot write {path}: {e}",
                out.stamp(qpc(), now_ft())
            ));
            eprintln!("wall_cpu_probe: cannot write {path}: {e}");
        },
    }
    let _ = out.log.flush();
    drop(threads);
    drop(prev_proc);
    unsafe { CloseHandle(hproc) };
}

#[cfg(test)]
mod tests {
    use super::{
        FILETIME, NameRead, civil_from_days, ft, iso_utc, open_thread_checked, refresh_name,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, GetCurrentThread, GetCurrentThreadId, GetThreadTimes, OpenThread,
        SetThreadDescription, THREAD_QUERY_LIMITED_INFORMATION,
    };

    fn own_thread_create_time() -> i64 {
        let z = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut c, mut e, mut k, mut u) = (z, z, z, z);
        assert!(unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) } != 0);
        ft(c)
    }

    fn set_own_name(name: &str) {
        let w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        assert!(unsafe { SetThreadDescription(GetCurrentThread(), w.as_ptr()) } >= 0);
    }

    // (2026-11-18) The tid-reuse guard: a handle is kept only for the thread the snapshot
    // entry describes. A wrong CreateTime or owning pid is exactly what a recycled tid reads.
    #[test]
    fn thread_handle_must_match_create_time_and_pid() {
        let (tid, pid) = unsafe { (GetCurrentThreadId() as usize, GetCurrentProcessId()) };
        let ct = own_thread_create_time();
        let (h, rejected) = open_thread_checked(tid, ct, pid);
        assert!(!h.is_null() && !rejected);
        unsafe { CloseHandle(h) };
        let (h, rejected) = open_thread_checked(tid, ct + 1, pid);
        assert!(
            h.is_null() && rejected,
            "a different CreateTime is another thread"
        );
        let (h, rejected) = open_thread_checked(tid, ct, pid + 4);
        assert!(
            h.is_null() && rejected,
            "a different owning pid is another thread"
        );
    }

    // (2026-11-18) Names are re-read every sample: a later rename must replace the first name
    // (the GStreamer pad-task name -> ServoGstVideo-N case), and an unchanged one must not count.
    #[test]
    fn a_rename_after_the_first_name_is_picked_up() {
        let h: HANDLE =
            unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, GetCurrentThreadId()) };
        assert!(!h.is_null());
        let mut name = None;
        set_own_name("multiqueue4:src_0");
        assert_eq!(refresh_name(h, &mut name), NameRead::Named);
        assert_eq!(refresh_name(h, &mut name), NameRead::Unchanged);
        set_own_name("ServoGstVideo-0");
        assert_eq!(refresh_name(h, &mut name), NameRead::Renamed);
        assert_eq!(name.as_deref(), Some("ServoGstVideo-0"));
        // An emptied description keeps the last name.
        set_own_name("");
        assert_eq!(refresh_name(h, &mut name), NameRead::Unchanged);
        assert_eq!(name.as_deref(), Some("ServoGstVideo-0"));
        unsafe { CloseHandle(h) };
    }

    #[test]
    fn iso_utc_formats_filetime() {
        // Unix epoch in FILETIME ticks.
        assert_eq!(iso_utc(116_444_736_000_000_000), "1970-01-01T00:00:00.000Z");
        // 2000-02-29T12:34:56.789Z = unix ms 951_827_696_789.
        assert_eq!(
            iso_utc((951_827_696_789 + 11_644_473_600_000) * 10_000),
            "2000-02-29T12:34:56.789Z"
        );
    }

    #[test]
    fn civil_dates_round_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_410), (2025, 11, 18));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
