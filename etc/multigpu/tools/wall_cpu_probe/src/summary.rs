//! Steady-state window and the <out>.cpu.summary.txt report (2026-11-18).
//!
//! The report keeps the deleted thread_cpu_probe's table format (a5a9dd81cb0^) verbatim --
//! "cores %cpu n hottest thread group", TOTAL, process total, "ALL PROCESSES, ALL GROUPS"
//! with the top processes -- so it reads side by side with the 425 .threads.txt files of
//! 2026-08/09 (e.g. logs_perf\23\p54: ServoGstVideo 54 / 27.10 / 0.53, machine 28.32 of 80).
//! Semantics kept from the old tool:
//!   cores   = CPU-seconds inside the window / window seconds (a thread alive for half the
//!             window at 1.0 core reads 0.50, as before)
//!   n       = threads of that family alive at the END of the window
//!   hottest = the busiest single thread of the family, same unit as cores
//!   family  = collapsed from the name the thread had LAST (names arrive late: a GStreamer
//!             streaming thread is tagged on its first frame; since 2026-11-18 every live
//!             thread's name is re-read every sample, so "last" really is the last one)
//! Differences, all in the old tool's favour to drop:
//!   * thread and process cores are TSC-cycle based (QueryThreadCycleTime / process
//!     CycleTime), not 15.625 ms tick-charged; the tick figure is printed for reconciliation
//!   * the machine line covers every process present in a snapshot, protected ones included,
//!     and (2026-11-18) a process that exits is counted up to its exit through a handle held
//!     since first sight. NOT covered: a process that starts AND exits between two samples
//!     is in no snapshot; its CPU shows up only as "unaccounted". DPC/ISR (charged to no
//!     process) is listed as its own row
//!   * "one group only (GetSystemTimes)" is replaced by per-group and per-node busy from
//!     NtQuerySystemInformationEx per group, i.e. where CPU time was actually spent
//!   * threads that started or exited inside the window are counted (handle kept open for
//!     the thread's life, new threads counted from zero), so "unattributed" shrinks to
//!     threads that lived and died inside one interval.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

/// CPU-seconds of one process inside the window.
pub struct ProcAcc {
    pub name: String,
    pub pid: usize,
    pub tick: f64,
    pub cyc: f64,
    /// Exited inside the window (its last partial interval read through the held handle).
    pub exited: bool,
}

/// CPU-seconds of one target thread identity (tid, CreateTime) inside the window.
pub struct ThrAcc {
    pub tid: usize,
    pub name: Option<String>,
    pub family: String,
    pub cyc: f64,
    pub tick: f64,
    pub started_in_window: bool,
    /// Index of the last window sample this thread was alive in.
    pub last_sample: usize,
}

#[derive(Default, Clone)]
pub struct NodeAcc {
    pub lps: usize,
    pub cores: usize,
    /// Sigma(mean LP busy x dt)
    pub busy: f64,
    pub both90: f64,
    pub one90: f64,
    pub lp90: f64,
}

/// Window accumulators. Every per-sample figure is added as value x dt, and divided by the
/// summed dt at the end, so a late sample weighs what it covered.
#[derive(Default)]
pub struct Window {
    pub samples: usize,
    pub secs: f64,
    pub secs_snap: f64,
    pub t_start: f64,
    pub t_end: f64,
    pub t_eng_start: f64,
    pub t_eng_end: f64,
    pub lp_busy: Vec<Vec<f64>>,
    pub lp_cyc_busy: Vec<Vec<f64>>,
    /// Sigma(per-LP busy fraction x dt), the PDH % Processor Time view.
    pub machine: f64,
    pub machine_cyc: f64,
    /// (2026-11-18) Sigma(charged busy ticks per second x dt_snap) and DPC/ISR likewise: the
    /// reconciliation side, same tick domain and interval as the processes.
    pub machine_ticks: f64,
    pub dpc_isr: f64,
    pub procs_tick: f64,
    pub procs_cyc: f64,
    pub unacc: f64,
    pub both90: f64,
    pub one90: f64,
    pub lp90: f64,
    pub nodes: BTreeMap<u32, NodeAcc>,
    pub procs: HashMap<(usize, i64), ProcAcc>,
    pub target_tick: f64,
    pub target_cyc: f64,
    pub threads: HashMap<(usize, i64), ThrAcc>,
    pub started: usize,
    pub exited: usize,
    pub procs_started: usize,
    pub procs_exited: usize,
    pub tgt_unattr_cyc: f64,
    pub tgt_unattr_tick: f64,
}

impl Window {
    pub fn add_proc(
        &mut self,
        key: (usize, i64),
        name: &str,
        tick_sec: f64,
        cyc_sec: f64,
        exited: bool,
    ) {
        let e = self.procs.entry(key).or_insert_with(|| ProcAcc {
            name: name.to_string(),
            pid: key.0,
            tick: 0.0,
            cyc: 0.0,
            exited: false,
        });
        e.tick += tick_sec;
        e.cyc += cyc_sec;
        e.exited |= exited;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_thread(
        &mut self,
        key: (usize, i64),
        name: Option<&str>,
        family: &str,
        cyc_sec: f64,
        tick_sec: f64,
        born_in_window: bool,
        alive: bool,
    ) {
        let sample = self.samples;
        let e = self.threads.entry(key).or_insert_with(|| ThrAcc {
            tid: key.0,
            name: None,
            family: String::new(),
            cyc: 0.0,
            tick: 0.0,
            started_in_window: born_in_window,
            last_sample: usize::MAX,
        });
        if name.is_some() {
            e.name = name.map(str::to_string);
        }
        e.family = family.to_string();
        e.cyc += cyc_sec;
        e.tick += tick_sec;
        if alive {
            e.last_sample = sample;
        }
    }
}

/// Run-level validity figures (every sample except the first interval and the exit one).
#[derive(Default)]
pub struct Validity {
    pub samples: usize,
    /// (2026-11-18) Sigma(unacc x dt_snap) and Sigma(dt_snap): the dt-weighted mean, whose
    /// tick-quantisation error telescopes to the two edges of the run (unacc_edge_*, in
    /// core-seconds: half the busy ticks charged inside the first and last read windows).
    pub unacc_sum: f64,
    pub unacc_secs: f64,
    pub unacc_edge_first: f64,
    pub unacc_edge_last: f64,
    pub unacc_max: f64,
    /// (2026-11-18) Per-sample quantisation bound of unacc (q_bound, measured), the samples
    /// beyond it + 0.02, and what it came from (busy LPs, per-LP read window, ticks in it).
    pub q_bound_sum: f64,
    pub q_bound_max: f64,
    pub unacc_beyond: usize,
    pub busy_lps_sum: f64,
    pub recon_ms_sum: f64,
    pub win_ticks_sum: f64,
    pub probe_ms_sum: f64,
    pub probe_ms_max: f64,
    /// (2026-11-18) Time spent in the every-sample GetThreadDescription reads.
    pub names_ms_sum: f64,
    pub names_ms_max: f64,
    pub probe_cyc_sum: f64,
    pub probe_cyc_max: f64,
    pub late: usize,
    pub groups_identical: usize,
    pub lp_read_failures: usize,
    pub coverage_min: Vec<f64>,
    pub coverage_max: Vec<f64>,
    pub coverage_sum: Vec<f64>,
    pub started: usize,
    pub exited: usize,
    pub first_seen_late: usize,
    pub no_handle: usize,
    pub skipped_deadlines: usize,
    /// (2026-11-18) Thread renames seen, and thread handles refused for a tid-reuse mismatch.
    pub renamed: usize,
    pub tid_mismatch: usize,
    /// (2026-11-18) Process churn: started after the previous snapshot, left the snapshot,
    /// of those not readable at exit (no handle), new ones whose handle was refused, and
    /// processes whose handle was refused at the baseline.
    pub procs_started: usize,
    pub procs_exited: usize,
    pub procs_exit_unread: usize,
    pub procs_new_no_handle: usize,
    pub proc_no_handle_at_start: usize,
    /// (2026-11-18) .cpu.threads.tsv rows written / active rows left out by --tsv-top, and
    /// whether the --tsv-max-mb cap stopped it.
    pub tsv_rows: usize,
    pub tsv_skipped: usize,
    pub tsv_stopped: bool,
}

pub struct Meta {
    pub label: String,
    pub cpus: usize,
    pub groups: Vec<usize>,
    pub nodes: usize,
    pub cores: usize,
    pub smt: usize,
    pub interval_ms: u64,
    pub warmup_sec: f64,
    pub note: String,
    pub target_start_utc: String,
    pub tick_ms: f64,
    pub self_check: String,
    pub pdh: bool,
    pub end_reason: String,
    pub tsv_top: usize,
    pub tsv_all: bool,
    pub tsv_max_mb: f64,
}

fn pct(cores: f64, cpus: usize) -> f64 {
    100.0 * cores / cpus.max(1) as f64
}

fn ints(v: &[f64]) -> String {
    v.iter()
        .map(|x| format!("{x:.0}"))
        .collect::<Vec<_>>()
        .join(",")
}

pub fn render(m: &Meta, w: &Window, v: &Validity) -> String {
    let mut s = String::new();
    let cpus = m.cpus;
    let _ = writeln!(s, "wall_cpu_probe: {}  sampling {:.0}s", m.label, w.secs);
    let _ = writeln!(
        s,
        "  topology: {cpus} logical cpus in {} processor group(s) [{}], {} NUMA node(s), {} physical cores (SMT x{})",
        m.groups.len(),
        m.groups
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("+"),
        m.nodes,
        m.cores,
        m.smt
    );
    let _ = writeln!(
        s,
        "  window  : t={:.1}..{:.1} s since probe start, {} samples of {} ms after a {} s warmup (all figures below are window means)",
        w.t_start, w.t_end, w.samples, m.interval_ms, m.warmup_sec
    );
    let _ = writeln!(
        s,
        "  target  : started {}, window = {:.1}..{:.1} s after target start; run ended: {}",
        m.target_start_utc, w.t_eng_start, w.t_eng_end, m.end_reason
    );
    if !m.note.is_empty() {
        let _ = writeln!(s, "  note    : {}", m.note);
    }
    if w.samples == 0 || w.secs <= 0.0 {
        let _ = writeln!(
            s,
            "\n  NO STEADY-STATE SAMPLES: the run ended before the {} s warmup was over. See the .cpu.log lines.",
            m.warmup_sec
        );
        render_validity(&mut s, m, v);
        return s;
    }
    let secs = w.secs;
    let secs_snap = w.secs_snap.max(1e-9);

    // ---- per-family table, old format
    struct Fam {
        cores: f64,
        n: usize,
        hottest: f64,
    }
    let last = w.samples.saturating_sub(1);
    let mut fams: HashMap<&str, Fam> = HashMap::new();
    let (mut total, mut alive) = (0.0, 0usize);
    let (mut unnamed_cores, mut unnamed_n) = (0.0, 0usize);
    for t in w.threads.values() {
        let cores = t.cyc / secs_snap;
        let f = fams.entry(t.family.as_str()).or_insert(Fam {
            cores: 0.0,
            n: 0,
            hottest: 0.0,
        });
        f.cores += cores;
        f.hottest = f.hottest.max(cores);
        total += cores;
        if t.last_sample == last {
            f.n += 1;
            alive += 1;
        }
        if t.family.starts_with("(unnamed") {
            unnamed_cores += cores;
            if t.last_sample == last {
                unnamed_n += 1;
            }
        }
    }
    let unnamed_fams = fams.keys().filter(|k| k.starts_with("(unnamed")).count();
    let mut ranked: Vec<(&str, &Fam)> = fams.iter().map(|(k, v)| (*k, v)).collect();
    ranked.sort_by(|a, b| b.1.cores.total_cmp(&a.1.cores));
    let _ = writeln!(s);
    let _ = writeln!(s, "  cores   %cpu    n  hottest  thread group");
    let _ = writeln!(
        s,
        "  ------  -----  ---  -------  --------------------------------"
    );
    for (name, f) in &ranked {
        if f.cores < 0.005 {
            continue;
        }
        let flag = if f.hottest >= 0.90 {
            "  <-- SATURATED (one thread is the ceiling)"
        } else {
            ""
        };
        let _ = writeln!(
            s,
            "  {:6.2}  {:4.1}%  {:3}  {:7.2}  {}{}",
            f.cores,
            pct(f.cores, cpus),
            f.n,
            f.hottest,
            name,
            flag
        );
    }
    let _ = writeln!(
        s,
        "  ------  -----  ---  -------  --------------------------------"
    );
    let _ = writeln!(
        s,
        "  {:6.2}  {:4.1}%  {:3}           TOTAL (sum of threads)",
        total,
        pct(total, cpus),
        alive
    );
    let proc_cyc = w.target_cyc / secs_snap;
    let proc_tick = w.target_tick / secs_snap;
    let _ = writeln!(
        s,
        "  {:6.2}  {:4.1}%               process total (snapshot cycles; tick-charged {:.2})",
        proc_cyc,
        pct(proc_cyc, cpus),
        proc_tick
    );
    let gap = proc_cyc - total;
    if gap.abs() > 0.15 * proc_cyc.max(0.01) {
        let _ = writeln!(
            s,
            "  note: {gap:+.2} cores unattributed -- {} thread(s) exited and {} started during the window",
            w.exited, w.started
        );
    }
    if unnamed_fams > 1 {
        let _ = writeln!(
            s,
            "  note: the (unnamed:*) rows together: {unnamed_cores:.2} cores in {unnamed_n} threads (the old tool's single \"(unnamed)\" row)"
        );
    }

    // ---- machine, old wording plus the parts the old tool could not see
    let machine_procs = w.procs_tick / secs_snap;
    let target_tick = w.target_tick / secs_snap;
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "  ALL PROCESSES, ALL GROUPS: {:.2} cores busy of {} ({:.1}%)  -- this process is {:.0}% of it",
        machine_procs,
        cpus,
        pct(machine_procs, cpus),
        100.0 * target_tick / machine_procs.max(0.01)
    );
    // (2026-11-18) Says what is and is not covered; it used to claim "complete".
    let _ = writeln!(
        s,
        "  (tick-charged like GetProcessTimes; every process present in a snapshot, protected ones included, each counted up to its exit;"
    );
    let _ = writeln!(
        s,
        "   NOT included: processes that started and exited between two samples -- their CPU is in 'unaccounted' below)"
    );
    // Live processes one row per pid; exited ones folded by name, so a spawn loop reads as
    // one row with a count instead of filling the list.
    let mut rows: Vec<(f64, String)> = Vec::new();
    let mut gone: HashMap<&str, (f64, usize)> = HashMap::new();
    for p in w.procs.values() {
        if p.exited {
            let e = gone.entry(p.name.as_str()).or_default();
            e.0 += p.tick;
            e.1 += 1;
        } else {
            rows.push((p.tick, format!("{}  (pid {})", p.name, p.pid)));
        }
    }
    for (name, (tick, n)) in &gone {
        rows.push((
            *tick,
            format!(
                "{name}  ({n} process{} that exited in the window)",
                if *n == 1 { "" } else { "es" }
            ),
        ));
    }
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (tick, label) in rows.iter().take(10) {
        let c = tick / secs_snap;
        if c < 0.01 {
            break;
        }
        let _ = writeln!(s, "    {c:6.2} cores  {label}");
    }
    let dpc = w.dpc_isr / secs_snap;
    let _ = writeln!(s, "    {dpc:6.2} cores  (DPC+ISR, charged to no process)");
    let mut gl: Vec<(&str, (f64, usize))> = gone.iter().map(|(k, v)| (*k, *v)).collect();
    gl.sort_by(|a, b| b.1.0.total_cmp(&a.1.0).then(b.1.1.cmp(&a.1.1)));
    let top_gone: Vec<String> = gl
        .iter()
        .take(5)
        .map(|(n, (t, c))| format!("{n} x{c} {:.2} cores", t / secs_snap))
        .collect();
    let _ = writeln!(
        s,
        "  process churn in the window: {} started, {} exited{}{}",
        w.procs_started,
        w.procs_exited,
        if top_gone.is_empty() {
            ""
        } else {
            "; exited by name: "
        },
        top_gone.join(", ")
    );
    let machine_ticks = w.machine_ticks / secs_snap;
    let _ = writeln!(
        s,
        "  per-LP busy, all groups: {:.2} cores of {} ({:.1}%) = processes {:.2} + DPC/ISR {:.2} + unaccounted {:+.2}  (charged ticks, same interval on every term)",
        machine_ticks,
        cpus,
        pct(machine_ticks, cpus),
        machine_procs,
        dpc,
        w.unacc / secs_snap
    );
    let mcyc = w.machine_cyc / secs;
    let _ = writeln!(
        s,
        "  per-LP busy by idle cycles (precise, frequency-blind): {:.2} cores ({:.1}%); processes by cycles {:.2}",
        mcyc,
        pct(mcyc, cpus),
        w.procs_cyc / secs_snap
    );

    // ---- where CPU was spent: groups, nodes, physical cores
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "  processor group busy (per-LP time = where CPU was actually spent, not thread affinity):"
    );
    for (g, row) in w.lp_busy.iter().enumerate() {
        let c: f64 = row.iter().sum::<f64>() / secs;
        let cc: f64 = w.lp_cyc_busy[g].iter().sum::<f64>() / secs;
        let _ = writeln!(
            s,
            "    group {g:<2}: {c:6.2} of {:<3} ({:5.1}%)  by idle cycles {cc:6.2}",
            row.len(),
            100.0 * c / row.len().max(1) as f64
        );
    }
    let _ = writeln!(
        s,
        "  NUMA node busy (physical-core figures are means per sample):"
    );
    for (node, a) in &w.nodes {
        let busy_pct = 100.0 * a.busy / secs;
        let _ = writeln!(
            s,
            "    node {node:<2} : {:6.2} of {:<3} ({busy_pct:5.1}%)  {} physical cores: all siblings >=90% {:.1}, one sibling >=90% {:.1}; LPs >=90% {:.1}",
            busy_pct / 100.0 * a.lps as f64,
            a.lps,
            a.cores,
            a.both90 / secs,
            a.one90 / secs,
            a.lp90 / secs
        );
    }
    let _ = writeln!(
        s,
        "    machine : {} physical cores: all siblings >=90% {:.1}, one sibling >=90% {:.1}; LPs >=90% {:.1} of {cpus}",
        m.cores,
        w.both90 / secs,
        w.one90 / secs,
        w.lp90 / secs
    );
    let _ = writeln!(s, "  per-LP busy% (tick-charged, = PDH % Processor Time):");
    for (g, row) in w.lp_busy.iter().enumerate() {
        let v: Vec<f64> = row.iter().map(|x| 100.0 * x / secs).collect();
        let _ = writeln!(s, "    g{g}=[{}]", ints(&v));
    }
    let _ = writeln!(s, "  per-LP busy% (idle cycles):");
    for (g, row) in w.lp_cyc_busy.iter().enumerate() {
        let v: Vec<f64> = row.iter().map(|x| 100.0 * x / secs).collect();
        let _ = writeln!(s, "    g{g}=[{}]", ints(&v));
    }

    // ---- hottest threads, old format
    let mut thr: Vec<&ThrAcc> = w.threads.values().collect();
    thr.sort_by(|a, b| b.cyc.total_cmp(&a.cyc));
    let _ = writeln!(s);
    let _ = writeln!(s, "  hottest individual threads:");
    for t in thr.iter().take(20) {
        let c = t.cyc / secs_snap;
        if c < 0.005 {
            break;
        }
        let name = t
            .name
            .clone()
            .unwrap_or_else(|| format!("{} tid {}", t.family, t.tid));
        let _ = writeln!(
            s,
            "    {c:6.2} cores  {}  (tid {}){}{}",
            name,
            t.tid,
            if t.started_in_window {
                "  (started during sampling)"
            } else {
                ""
            },
            if t.last_sample != last {
                "  (exited)"
            } else {
                ""
            }
        );
    }
    let _ = writeln!(
        s,
        "  target minus threads: {:+.2} cores by cycles, {:+.2} by ticks (threads that lived and died inside one interval)",
        w.tgt_unattr_cyc / secs_snap,
        w.tgt_unattr_tick / secs_snap
    );
    render_validity(&mut s, m, v);
    s
}

fn render_validity(s: &mut String, m: &Meta, v: &Validity) {
    let n = v.samples.max(1) as f64;
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "  validity checks (whole run, {} samples; first interval and exit interval excluded):",
        v.samples
    );
    let _ = writeln!(s, "    layout self-check       : {}", m.self_check);
    let groups = if m.groups.len() < 2 {
        "n/a (one processor group)".to_string()
    } else if v.groups_identical == 0 {
        "OK, raw LP times of different groups never identical (Ex honours the group input)".into()
    } else {
        format!(
            "FAIL: identical raw LP times across groups in {} samples -- the per-group read is broken, per-LP numbers are invalid",
            v.groups_identical
        )
    };
    let _ = writeln!(s, "    groups distinct         : {groups}");
    let cov: Vec<String> = v
        .coverage_sum
        .iter()
        .enumerate()
        .map(|(g, sum)| {
            format!(
                "g{g} {:.3} (min {:.3} max {:.3})",
                sum / n,
                v.coverage_min.get(g).copied().unwrap_or(0.0),
                v.coverage_max.get(g).copied().unwrap_or(0.0)
            )
        })
        .collect();
    let _ = writeln!(
        s,
        "    LP time coverage        : {}  (1.000 = every LP-second accounted; LP read failures {})",
        cov.join(", "),
        v.lp_read_failures
    );
    // (2026-11-18) Judged on the dt-weighted mean, against 0.05 plus the most tick
    // quantisation can contribute to it. Per sample, unacc moves by up to half the busy ticks
    // charged while the per-LP reads straddle the snapshot (q_bound on each CPUPROC line), and
    // the next sample takes it back; over the run that telescopes to the two edges. A fixed
    // per-sample limit failed at high load with nothing wrong (16 busy LPs: 0.12-0.25).
    let secs = v.unacc_secs.max(1e-9);
    let mean = v.unacc_sum / secs;
    let allow = (v.unacc_edge_first + v.unacc_edge_last) / secs;
    let limit = 0.05 + allow;
    let _ = writeln!(
        s,
        "    unacc                   : mean {mean:+.3} cores ({}); per sample max |{:.3}|, {} of {} sample(s) beyond their tick bound + 0.02",
        if mean.abs() <= limit {
            format!("OK, limit |mean| {limit:.3} = 0.05 + {allow:.3} tick quantisation")
        } else {
            format!("CHECK: |mean| exceeds {limit:.3} = 0.05 + {allow:.3} tick quantisation")
        },
        v.unacc_max,
        v.unacc_beyond,
        v.samples
    );
    let _ = writeln!(
        s,
        "                              = per-LP busy - processes - DPC/ISR, all as charged ticks over the same interval. Expected per"
    );
    let _ = writeln!(
        s,
        "                              sample: |unacc| <= half the busy ticks charged while the per-LP reads ({:.2} ms apart) straddle the",
        v.recon_ms_sum / n
    );
    let _ = writeln!(
        s,
        "                              snapshot, this sample's and the previous one's (q_bound in CPUPROC: mean {:.3}, max {:.3} cores;",
        v.q_bound_sum / n,
        v.q_bound_max
    );
    let _ = writeln!(
        s,
        "                              {:.1} busy LPs, {:.1} ticks per window on average). The tick is charged on every LP at once, so",
        v.busy_lps_sum / n,
        v.win_ticks_sum / n
    );
    let _ = writeln!(
        s,
        "                              that is +-busy LPs x {:.4} / 2 cores in the samples where it lands in the window, 0 otherwise;",
        m.tick_ms / m.interval_ms.max(1) as f64
    );
    let _ = writeln!(
        s,
        "                              the next sample takes it back. Beyond the bound: processes that started and exited between two"
    );
    let _ = writeln!(
        s,
        "                              samples (in no snapshot; see process churn), or a probe fault."
    );
    let _ = writeln!(
        s,
        "    process churn           : started {}, exited {} (last interval read through the held handle for {}, unreadable {}); handles refused: {} at start, {} later",
        v.procs_started,
        v.procs_exited,
        v.procs_exited - v.procs_exit_unread,
        v.procs_exit_unread,
        v.proc_no_handle_at_start,
        v.procs_new_no_handle
    );
    let _ = writeln!(
        s,
        "    thread churn            : started {}, exited {}, first seen late {}, no handle {}, tid reuse rejected {}, renames seen {}",
        v.started, v.exited, v.first_seen_late, v.no_handle, v.tid_mismatch, v.renamed
    );
    let ok = v.probe_ms_max <= 10.0 && v.probe_cyc_max <= 0.01;
    let _ = writeln!(
        s,
        "    probe cost              : mean {:.2} ms, max {:.2} ms per sample; mean {:.4} cores, max {:.4} (budget 10 ms / 0.01 cores: {})",
        v.probe_ms_sum / n,
        v.probe_ms_max,
        v.probe_cyc_sum / n,
        v.probe_cyc_max,
        if ok { "OK" } else { "OVER" }
    );
    let _ = writeln!(
        s,
        "                              of which thread-name reads (every live target thread, every sample): mean {:.3} ms, max {:.3} ms",
        v.names_ms_sum / n,
        v.names_ms_max
    );
    let _ = writeln!(
        s,
        "    threads.tsv             : {} rows written, {} active rows left out ({}){}",
        v.tsv_rows,
        v.tsv_skipped,
        if m.tsv_all {
            "--tsv-all".to_string()
        } else {
            format!(
                "top {} by cores per interval plus new and exited threads",
                m.tsv_top
            )
        },
        if v.tsv_stopped {
            format!(
                "; STOPPED at the {:.2} MB cap (CPUWARN in .cpu.log)",
                m.tsv_max_mb
            )
        } else {
            String::new()
        }
    );
    let _ = writeln!(
        s,
        "    late samples            : {} (dt > 1.5 x interval), skipped deadlines {}",
        v.late, v.skipped_deadlines
    );
    let _ = writeln!(
        s,
        "    tick granularity        : {:.3} ms -> one tick = {:.4} cores per {} ms sample per counter; thread/family/process cores above are cycle-based",
        m.tick_ms,
        m.tick_ms / m.interval_ms.max(1) as f64,
        m.interval_ms
    );
    if m.pdh {
        let _ = writeln!(
            s,
            "    pdh                     : CPUUTIL lines in .cpu.log (util/time/perf per LP)"
        );
    }
}
