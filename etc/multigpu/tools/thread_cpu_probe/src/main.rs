// Attributes a running process's CPU time to its individual threads, by name.
//
// WHY THIS EXISTS (2026-08-25). The wall saturates CPU at ~45 videos while a
// non-Servo reference wall plays 54 with headroom, and GPU utilization FALLS as
// fps drops -- the GPU is being starved. Two costs were measured and ruled out:
// decode alone is ~30% CPU at 45 videos (etc/multigpu/tools/measure_decode_only.ps1),
// and the D3D11 plane upload is ~3 cores by arithmetic (D3D11PROF copy p50 x 45
// videos x 30fps), spread over 45 parallel streaming threads. That leaves most
// of the CPU unexplained, and guessing at it has already produced several wrong
// answers.
//
// A per-thread breakdown ends the guessing: it says outright whether the cost
// sits on the 45 decode threads (parallel, fine) or on one of the few
// single-threaded stages -- Compositor, Renderer, Script -- which is what "the
// GPU is starved" would look like. A thread pinned at ~1.0 cores while dozens
// of cores idle IS the bottleneck, and this prints exactly that.
//
// Thread names come from the OS thread description. Rust's std sets one for
// every named thread it spawns, so the engine's own threads are named for free.
// GStreamer's streaming threads are GLib/C threads with no description, so the
// engine tags them from the inside -- see
// components/media/backends/gstreamer/thread_name.rs, which also logs
// "THREADMAP tid=<n> name=<n>" so a captured log can be read the same way.
//
// Usage:
//   thread_cpu_probe.exe                          # find winit_wall, sample 20s
//   thread_cpu_probe.exe --duration 30 --top 25
//   thread_cpu_probe.exe --pid 1234
//   thread_cpu_probe.exe --process servoshell

use std::collections::HashMap;
use std::ffi::c_void;
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows_sys::Win32::System::Threading::{
    ALL_PROCESSOR_GROUPS, GetActiveProcessorCount, GetActiveProcessorGroupCount,
    GetNumaHighestNodeNumber, GetProcessTimes, GetSystemTimes, GetThreadDescription,
    GetThreadGroupAffinity, GetThreadTimes, OpenProcess, OpenThread,
    PROCESS_QUERY_LIMITED_INFORMATION, THREAD_QUERY_LIMITED_INFORMATION,
};
use windows_sys::core::PWSTR;

/// FILETIME ticks are 100ns, so this many make a second.
const TICKS_PER_SEC: f64 = 10_000_000.0;

struct Args {
    pid: Option<u32>,
    process: String,
    duration: f64,
    top: usize,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        pid: None,
        process: "winit_wall".to_string(),
        duration: 20.0,
        top: 15,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--pid" => args.pid = Some(value()?.parse().map_err(|_| "--pid must be a number")?),
            "--process" => args.process = value()?,
            "--duration" => {
                args.duration = value()?
                    .parse()
                    .map_err(|_| "--duration must be a number")?
            },
            "--top" => args.top = value()?.parse().map_err(|_| "--top must be a number")?,
            "--help" | "-h" => return Err("help".to_string()),
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    if args.duration <= 0.0 {
        return Err("--duration must be positive".to_string());
    }
    Ok(args)
}

/// Every process whose exe name contains `needle`, case-insensitively.
fn find_processes(needle: &str) -> Vec<(u32, String)> {
    let needle = needle.to_lowercase();
    let mut found = Vec::new();
    // SAFETY: the snapshot handle is checked before use and closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return found;
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    // SAFETY: `entry` is zeroed with dwSize set, as the API requires.
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while ok {
        let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
        if name.to_lowercase().contains(&needle) {
            found.push((entry.th32ProcessID, name));
        }
        ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    found
}

fn thread_ids(pid: u32) -> Vec<u32> {
    let mut ids = Vec::new();
    // A thread snapshot cannot be scoped to one process (the pid argument is
    // ignored for TH32CS_SNAPTHREAD), so it is filtered by owner here.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return ids;
    }
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut ok = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while ok {
        if entry.th32OwnerProcessID == pid {
            ids.push(entry.th32ThreadID);
        }
        ok = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    ids
}

fn ticks(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

struct ThreadSample {
    cpu: u64,
    created: u64,
    name: Option<String>,
    group: Option<u16>,
}

/// Reading the name costs an extra call per thread, so it is only done on the
/// final sample -- which is also the correct one: a streaming thread is named
/// on its first frame, which may well be after the first sample was taken.
fn sample_threads(pid: u32, with_names: bool) -> HashMap<u32, ThreadSample> {
    let mut out = HashMap::new();
    for tid in thread_ids(pid) {
        // SAFETY: the handle is checked and closed on every path below.
        let handle = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid) };
        if handle.is_null() {
            continue;
        }
        let mut creation: FILETIME = unsafe { std::mem::zeroed() };
        let mut exit: FILETIME = unsafe { std::mem::zeroed() };
        let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
        let mut user: FILETIME = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetThreadTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0
        };
        if ok {
            out.insert(
                tid,
                ThreadSample {
                    cpu: ticks(kernel) + ticks(user),
                    created: ticks(creation),
                    name: if with_names {
                        thread_name(handle)
                    } else {
                        None
                    },
                    group: if with_names {
                        thread_group(handle)
                    } else {
                        None
                    },
                },
            );
        }
        unsafe { CloseHandle(handle) };
    }
    out
}

/// Which processor group this thread is scheduled in.
///
/// Above 64 logical processors Windows splits the machine into processor groups, and a
/// thread only ever runs inside one of them. On a wall run that matters: whether the 45
/// decode threads all landed in one group or got split across two decides how much of
/// their memory traffic crosses the interconnect -- and that is a startup placement
/// decision that persists for the life of the process. Two runs of the SAME command
/// measured 0.96x and 0.60x playback with identical CPU consumption, and the split was
/// set within the first second, so placement is the first thing to rule in or out.
fn thread_group(handle: HANDLE) -> Option<u16> {
    let mut affinity: GROUP_AFFINITY = unsafe { std::mem::zeroed() };
    // SAFETY: `affinity` is a zeroed out-parameter; the call only writes to it.
    let ok = unsafe { GetThreadGroupAffinity(handle, &mut affinity) != 0 };
    ok.then_some(affinity.Group)
}

fn thread_name(handle: HANDLE) -> Option<String> {
    let mut wide: PWSTR = std::ptr::null_mut();
    // SAFETY: on success the API hands over a LocalAlloc'd buffer, freed below.
    let hr = unsafe { GetThreadDescription(handle, &mut wide) };
    if hr < 0 || wide.is_null() {
        return None;
    }
    let mut len = 0usize;
    while unsafe { *wide.add(len) } != 0 {
        len += 1;
    }
    let name = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(wide, len) });
    unsafe { LocalFree(wide as *mut c_void as HLOCAL) };
    if name.is_empty() { None } else { Some(name) }
}

/// Busy CPU ticks for the WHOLE machine, so the report can say whether the process
/// accounts for what the box is actually doing.
///
/// This exists because a 45-video wall run showed 68% for the process while the machine
/// was observed at 100%. A per-process tool cannot tell whether the missing quarter is
/// another process, the GPU driver, or kernel work outside this process -- and that gap
/// changes what the whole measurement means. GetSystemTimes counts idle inside kernel
/// time, so busy = kernel + user - idle.
fn machine_cpu() -> Option<u64> {
    let mut idle: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) != 0 };
    ok.then(|| (ticks(kernel) + ticks(user)).saturating_sub(ticks(idle)))
}

fn process_cpu(pid: u32) -> Option<u64> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let ok =
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0 };
    unsafe { CloseHandle(handle) };
    ok.then(|| ticks(kernel) + ticks(user))
}

/// Collapses per-instance names onto the pool they belong to, so 45 identical
/// pipelines read as one line instead of 45. Two rules, both needed:
///
///   "ServoGstVideo-3" -> "ServoGstVideo"       trailing instance index
///   "Script#1"        -> "Script"
///   "multiqueue4:src_0" -> "multiqueue:src"    GStreamer names a pad task after
///   "avdec_h264-0:src"  -> "avdec_h264:src"    its element, index included
///
/// The second rule only strips digits that sit immediately before the pad
/// separator, so a version number inside an element name ("h264") survives.
fn group_of(name: Option<&str>) -> String {
    let Some(name) = name else {
        return "(unnamed)".to_string();
    };
    // 1. trailing instance index, e.g. "-3", "#1", "_0".
    let trimmed = name.trim_end_matches(|c: char| c.is_ascii_digit() || "-_#()., ".contains(c));
    let trimmed = if trimmed.is_empty() { name } else { trimmed };

    // 2. element index right before a pad separator.
    let Some(colon) = trimmed.find(':') else {
        return trimmed.to_string();
    };
    let head = trimmed[..colon].trim_end_matches(|c: char| c.is_ascii_digit());
    let head = head.trim_end_matches(['-', '_']);
    if head.is_empty() {
        return trimmed.to_string();
    }
    format!("{head}{}", &trimmed[colon..])
}

struct Group {
    cores: f64,
    threads: usize,
    hottest: f64,
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            if message != "help" {
                eprintln!("error: {message}");
            }
            eprintln!(
                "usage: thread_cpu_probe [--pid N | --process SUBSTR] [--duration SEC] [--top N]"
            );
            std::process::exit(if message == "help" { 0 } else { 2 });
        },
    };

    let (pid, label) = match args.pid {
        Some(pid) => (pid, format!("pid {pid}")),
        None => {
            let matches = find_processes(&args.process);
            match matches.len() {
                0 => {
                    eprintln!("no running process matches {}", args.process);
                    std::process::exit(1);
                },
                1 => (
                    matches[0].0,
                    format!("{} (pid {})", matches[0].1, matches[0].0),
                ),
                _ => {
                    eprintln!("{} matches several processes -- pass --pid:", args.process);
                    for (pid, name) in matches {
                        eprintln!("  {pid}  {name}");
                    }
                    std::process::exit(1);
                },
            }
        },
    };

    // NOT available_parallelism(): on Windows that reports the CURRENT PROCESSOR
    // GROUP only, so on a multi-group box (two sockets, or more than 64 logical
    // processors) it under-counts and every percentage comes out inflated.
    // Measured 2026-08-25 on a 45-video wall: it said 40 cpus while the process
    // was genuinely burning 52.69 cores, printing "131.7%".
    let cpus = match unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) } {
        0 => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        n => n as usize,
    };
    // SAFETY: both calls are plain reads of static system topology.
    // NOT `groups`: that name already holds the thread-name HashMap below.
    let group_count = unsafe { GetActiveProcessorGroupCount() }.max(1);
    let per_group: Vec<u32> = (0..group_count)
        .map(|g| unsafe { GetActiveProcessorCount(g) })
        .collect();
    let mut highest_numa: u32 = 0;
    let numa_nodes = unsafe { GetNumaHighestNodeNumber(&mut highest_numa) != 0 }
        .then(|| highest_numa + 1)
        .unwrap_or(1);
    println!("thread_cpu_probe: {label}  sampling {:.0}s", args.duration);
    println!(
        "  topology: {cpus} logical cpus in {group_count} processor group(s) [{}], {numa_nodes} NUMA node(s)",
        per_group
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("+")
    );

    let start = Instant::now();
    let before = sample_threads(pid, false);
    let process_before = process_cpu(pid);
    let machine_before = machine_cpu();
    if before.is_empty() {
        eprintln!("no threads readable for pid {pid} (is it running? try an elevated shell)");
        std::process::exit(1);
    }
    sleep(Duration::from_secs_f64(args.duration));
    let after = sample_threads(pid, true);
    let process_after = process_cpu(pid);
    let machine_after = machine_cpu();
    let elapsed = start.elapsed().as_secs_f64();

    if after.is_empty() {
        eprintln!("the process exited during sampling -- no result");
        std::process::exit(1);
    }

    let mut groups: HashMap<String, Group> = HashMap::new();
    let mut per_thread: Vec<(String, f64, bool)> = Vec::new();
    let mut total = 0.0f64;
    let mut started_during = 0usize;

    for (tid, now) in &after {
        // A thread created after the first sample has no `before` entry -- and a
        // recycled tid has a different creation time -- so all of its CPU time
        // was spent inside the window and its total IS the delta.
        let (delta, is_new) = match before.get(tid) {
            Some(then) if then.created == now.created => (now.cpu.saturating_sub(then.cpu), false),
            _ => (now.cpu, true),
        };
        if is_new {
            started_during += 1;
        }
        let cores = delta as f64 / TICKS_PER_SEC / elapsed;
        total += cores;

        let entry = groups
            .entry(group_of(now.name.as_deref()))
            .or_insert(Group {
                cores: 0.0,
                threads: 0,
                hottest: 0.0,
            });
        entry.cores += cores;
        entry.threads += 1;
        entry.hottest = entry.hottest.max(cores);

        let name = now
            .name
            .clone()
            .unwrap_or_else(|| format!("(unnamed tid {tid})"));
        per_thread.push((name, cores, is_new));
    }

    let exited = before.keys().filter(|tid| !after.contains_key(tid)).count();

    let mut ranked: Vec<(&String, &Group)> = groups.iter().collect();
    ranked.sort_by(|a, b| b.1.cores.total_cmp(&a.1.cores));

    println!();
    println!("  cores   %cpu    n  hottest  thread group");
    println!("  ------  -----  ---  -------  --------------------------------");
    for (name, group) in &ranked {
        if group.cores < 0.005 {
            continue;
        }
        // A single thread near 1.0 cores cannot go faster no matter how many
        // cores sit idle. That is the shape of a starved pipeline, so say so.
        let flag = if group.hottest >= 0.90 {
            "  <-- SATURATED (one thread is the ceiling)"
        } else {
            ""
        };
        println!(
            "  {:6.2}  {:4.1}%  {:3}  {:7.2}  {}{}",
            group.cores,
            100.0 * group.cores / cpus as f64,
            group.threads,
            group.hottest,
            name,
            flag
        );
    }

    println!("  ------  -----  ---  -------  --------------------------------");
    println!(
        "  {:6.2}  {:4.1}%  {:3}           TOTAL (sum of threads)",
        total,
        100.0 * total / cpus as f64,
        after.len()
    );
    if let (Some(before_cpu), Some(after_cpu)) = (process_before, process_after) {
        let process_cores = after_cpu.saturating_sub(before_cpu) as f64 / TICKS_PER_SEC / elapsed;
        println!(
            "  {:6.2}  {:4.1}%               process total (GetProcessTimes)",
            process_cores,
            100.0 * process_cores / cpus as f64
        );
        if let (Some(before_all), Some(after_all)) = (machine_before, machine_after) {
            let machine_cores =
                after_all.saturating_sub(before_all) as f64 / TICKS_PER_SEC / elapsed;
            println!(
                "  {:6.2}  {:4.1}%               WHOLE MACHINE (this process is {:.0}% of it)",
                machine_cores,
                100.0 * machine_cores / cpus as f64,
                100.0 * process_cores / machine_cores.max(0.01)
            );
        }
        // The two should agree closely. A gap means threads came and went inside
        // the window, and their time is only in the process figure.
        let gap = process_cores - total;
        if gap.abs() > 0.15 * process_cores.max(0.01) {
            println!(
                "  note: {gap:+.2} cores unattributed -- {exited} thread(s) exited and \
                 {started_during} started during the window"
            );
        }
    }

    // Where the threads actually landed. Only meaningful above one group, and it is
    // exactly what separates two runs of the same command that consume the same CPU
    // but deliver different throughput.
    if group_count > 1 {
        let mut group_cores: HashMap<u16, (f64, usize)> = HashMap::new();
        for (tid, now) in &after {
            let delta = match before.get(tid) {
                Some(then) if then.created == now.created => now.cpu.saturating_sub(then.cpu),
                _ => now.cpu,
            };
            let entry = group_cores
                .entry(now.group.unwrap_or(u16::MAX))
                .or_default();
            entry.0 += delta as f64 / TICKS_PER_SEC / elapsed;
            entry.1 += 1;
        }
        let mut ranked: Vec<_> = group_cores.into_iter().collect();
        ranked.sort_by_key(|(group, _)| *group);
        println!();
        println!(
            "  processor group placement (a split process pays for cross-group memory traffic):"
        );
        for (group, (cores, threads)) in ranked {
            match group {
                u16::MAX => println!("    unknown : {threads:3} threads  {cores:6.2} cores"),
                g => println!("    group {g:<2}: {threads:3} threads  {cores:6.2} cores"),
            }
        }
    }

    if args.top > 0 {
        per_thread.sort_by(|a, b| b.1.total_cmp(&a.1));
        println!();
        println!("  hottest individual threads:");
        for (name, cores, is_new) in per_thread.iter().take(args.top) {
            if *cores < 0.005 {
                break;
            }
            println!(
                "    {:6.2} cores  {}{}",
                cores,
                name,
                if *is_new {
                    "  (started during sampling)"
                } else {
                    ""
                }
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::group_of;

    fn group(name: &str) -> String {
        group_of(Some(name))
    }

    #[test]
    fn trailing_instance_index_is_dropped() {
        assert_eq!(group("ServoGstVideo-3"), "ServoGstVideo");
        assert_eq!(group("ServoGstAudio-0"), "ServoGstAudio");
        assert_eq!(group("Script#1"), "Script");
        assert_eq!(group("WRSceneBuilder#0"), "WRSceneBuilder");
    }

    #[test]
    fn gstreamer_pad_tasks_collapse_onto_one_pool() {
        // The reason this function exists: 45 videos would otherwise print 45
        // one-thread groups and hide which pool owns the CPU.
        assert_eq!(group("multiqueue4:src_0"), "multiqueue:src");
        assert_eq!(group("multiqueue40:src_12"), "multiqueue:src");
        assert_eq!(group("queue2:sink_0"), "queue:sink");
    }

    #[test]
    fn a_version_inside_an_element_name_survives() {
        // Only digits adjacent to the pad separator are an instance index.
        assert_eq!(group("avdec_h264-0:src"), "avdec_h264:src");
        assert_eq!(group("h264parse0:src"), "h264parse:src");
    }

    #[test]
    fn names_without_an_index_are_left_alone() {
        assert_eq!(group("main"), "main");
        assert_eq!(group("Compositor"), "Compositor");
        assert_eq!(group_of(None), "(unnamed)");
    }

    #[test]
    fn an_all_digit_name_is_not_trimmed_away() {
        // Trimming would leave nothing to report, so the original is kept.
        assert_eq!(group("12345"), "12345");
        assert_eq!(group("7:src"), "7:src");
    }
}
