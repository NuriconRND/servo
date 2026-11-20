//! Processor topology, read once at startup (2026-11-18).
//!
//! The LP -> physical core map comes from GetSystemCpuSetInformation (Group,
//! LogicalProcessorIndex, CoreIndex), keyed by (group, CoreIndex). The physical-core count
//! is cross-checked against GetLogicalProcessorInformationEx(RelationProcessorCore).
//!
//! The LP -> NUMA node map comes from GetNumaProcessorNodeEx(PROCESSOR_NUMBER{group, lp})
//! (2026-11-18), which returns the machine-wide node number. CpuSet NumaNodeIndex is NOT
//! used for it: SYSTEM_CPU_SET_INFORMATION documents every CpuSet value other than Group as
//! group-relative, so on the wall's two groups (one node each) it could read node 0 for both
//! and collapse the per-node split. It is still read, and the number of LPs where it differs
//! from GetNumaProcessorNodeEx goes on the CPUTOPO line (cpuset_node_mismatch=), which
//! answers on the first wall run whether the kernel returns global numbers there. The
//! distinct node count is checked against GetNumaHighestNodeNumber + 1 (CPUWARN if not).
//!
//! Why a physical-core view at all: with SMT, 80 LPs are 40 cores, and "all 80 LPs busy"
//! cannot be told apart from "40 cores, each with both siblings half-loaded" per LP alone.

use std::collections::{BTreeMap, BTreeSet};

use windows_sys::Win32::System::Kernel::PROCESSOR_NUMBER;
use windows_sys::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GetSystemCpuSetInformation, RelationProcessorCore,
    SYSTEM_CPU_SET_INFORMATION, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows_sys::Win32::System::Threading::{
    GetActiveProcessorCount, GetActiveProcessorGroupCount, GetCurrentProcess,
    GetNumaHighestNodeNumber, GetNumaProcessorNodeEx,
};

pub struct Core {
    pub group: usize,
    pub node: u32,
    /// LP indices within `group` (SMT siblings).
    pub lps: Vec<usize>,
}

pub struct Topo {
    /// Active LPs per group.
    pub groups: Vec<usize>,
    /// [group][lp] -> machine-wide NUMA node from GetNumaProcessorNodeEx (u32::MAX = unknown).
    pub node_of: Vec<Vec<u32>>,
    /// LPs whose CpuSet NumaNodeIndex differs from GetNumaProcessorNodeEx (2026-11-18).
    pub cpuset_node_mismatch: usize,
    /// [group][lp] -> parked at startup.
    pub parked: Vec<Vec<bool>>,
    /// Physical cores, from CpuSet (Group, CoreIndex).
    pub cores: Vec<Core>,
    /// Physical cores per GetLogicalProcessorInformationEx (cross-check).
    pub glpi_cores: usize,
    /// GetNumaHighestNodeNumber + 1 (cross-check).
    pub numa_highest_plus1: u32,
    /// Distinct NUMA nodes in `node_of` (u32::MAX included when a lookup failed).
    pub nodes: Vec<u32>,
}

/// CPUWARN text when the per-LP node map does not look like the machine's node set
/// (2026-11-18): an LP whose node could not be read, or a distinct-node count other than
/// GetNumaHighestNodeNumber + 1 (0 = that call failed, then nothing is compared).
pub fn node_warning(nodes: &[u32], highest_plus1: u32) -> Option<String> {
    let unknown = nodes.contains(&u32::MAX);
    let known = nodes.iter().filter(|&&n| n != u32::MAX).count();
    if unknown {
        return Some(format!(
            "GetNumaProcessorNodeEx failed for some LPs (node shown as {}); per-node figures are incomplete",
            u32::MAX
        ));
    }
    if highest_plus1 > 0 && known != highest_plus1 as usize {
        return Some(format!(
            "{known} distinct NUMA node(s) in the per-LP map but GetNumaHighestNodeNumber+1 = {highest_plus1}; per-node figures may be merged or mislabelled"
        ));
    }
    None
}

impl Topo {
    pub fn lps(&self) -> usize {
        self.groups.iter().sum()
    }

    /// LPs per node, ordered by node.
    pub fn lps_per_node(&self) -> BTreeMap<u32, usize> {
        let mut m = BTreeMap::new();
        for g in &self.node_of {
            for &n in g {
                *m.entry(n).or_insert(0) += 1;
            }
        }
        m
    }

    /// "g0:0+1,2+3,...;g1:..." -- the sibling sets as the OS reports them.
    pub fn sibling_map(&self) -> String {
        let mut by_group: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for c in &self.cores {
            by_group.entry(c.group).or_default().push(
                c.lps
                    .iter()
                    .map(|l| l.to_string())
                    .collect::<Vec<_>>()
                    .join("+"),
            );
        }
        by_group
            .into_iter()
            .map(|(g, v)| format!("g{g}:{}", v.join(",")))
            .collect::<Vec<_>>()
            .join(";")
    }

    pub fn smt_width(&self) -> usize {
        self.cores.iter().map(|c| c.lps.len()).max().unwrap_or(1)
    }
}

pub fn topology() -> Topo {
    // SAFETY: plain reads of static system topology.
    let gc = unsafe { GetActiveProcessorGroupCount() } as usize;
    let groups: Vec<usize> = (0..gc)
        .map(|g| unsafe { GetActiveProcessorCount(g as u16) } as usize)
        .collect();
    let mut cpuset_node: Vec<Vec<u32>> = groups.iter().map(|&n| vec![u32::MAX; n]).collect();
    let mut parked: Vec<Vec<bool>> = groups.iter().map(|&n| vec![false; n]).collect();
    // (group, core index) -> LPs
    let mut core_map: BTreeMap<(usize, u8), Vec<usize>> = BTreeMap::new();

    let mut len = 0u32;
    unsafe {
        GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut len, GetCurrentProcess(), 0)
    };
    let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
    if unsafe {
        GetSystemCpuSetInformation(
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
            GetCurrentProcess(),
            0,
        )
    } != 0
    {
        let base = buf.as_ptr() as *const u8;
        let mut off = 0usize;
        while off < len as usize {
            // SAFETY: entries are variable-size; Size gives the stride.
            let e = unsafe { &*(base.add(off) as *const SYSTEM_CPU_SET_INFORMATION) };
            if e.Size == 0 {
                break;
            }
            let c = unsafe { e.Anonymous.CpuSet };
            let (g, i) = (c.Group as usize, c.LogicalProcessorIndex as usize);
            if g < groups.len() && i < groups[g] {
                cpuset_node[g][i] = c.NumaNodeIndex as u32;
                parked[g][i] = unsafe { c.Anonymous1.AllFlags } & 1 != 0; // bit 0 = Parked
                // CoreIndex is group-relative too, hence the (group, CoreIndex) key.
                core_map.entry((g, c.CoreIndex)).or_default().push(i);
            }
            off += e.Size as usize;
        }
    }
    // Machine-wide node per LP (2026-11-18), see the module doc.
    let mut cpuset_node_mismatch = 0usize;
    let node_of: Vec<Vec<u32>> = groups
        .iter()
        .enumerate()
        .map(|(g, &n)| {
            (0..n)
                .map(|i| {
                    let pn = PROCESSOR_NUMBER {
                        Group: g as u16,
                        Number: i as u8,
                        Reserved: 0,
                    };
                    let mut raw = 0u16;
                    // SAFETY: plain read of static topology into a local.
                    let node = if unsafe { GetNumaProcessorNodeEx(&pn, &mut raw) } != 0
                        && raw != u16::MAX
                    {
                        raw as u32
                    } else {
                        u32::MAX
                    };
                    if node != cpuset_node[g][i] {
                        cpuset_node_mismatch += 1;
                    }
                    node
                })
                .collect()
        })
        .collect();
    let cores: Vec<Core> = core_map
        .into_iter()
        .map(|((g, _), mut lps)| {
            lps.sort_unstable();
            Core {
                group: g,
                node: node_of[g][lps[0]],
                lps,
            }
        })
        .collect();

    // Cross-check: one RelationProcessorCore record per physical core.
    let mut glpi_cores = 0usize;
    let mut blen = 0u32;
    unsafe {
        GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut blen)
    };
    let mut gbuf = vec![0u64; (blen as usize).div_ceil(8).max(1)];
    if unsafe {
        GetLogicalProcessorInformationEx(RelationProcessorCore, gbuf.as_mut_ptr().cast(), &mut blen)
    } != 0
    {
        let base = gbuf.as_ptr() as *const u8;
        let mut off = 0usize;
        while off < blen as usize {
            // SAFETY: variable-size records; Size gives the stride.
            let e = unsafe { &*(base.add(off) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX) };
            if e.Size == 0 {
                break;
            }
            if e.Relationship == RelationProcessorCore {
                glpi_cores += 1;
            }
            off += e.Size as usize;
        }
    }

    let mut highest = 0u32;
    let numa_highest_plus1 = if unsafe { GetNumaHighestNodeNumber(&mut highest) } != 0 {
        highest + 1
    } else {
        0
    };
    let nodes: Vec<u32> = node_of
        .iter()
        .flatten()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Topo {
        groups,
        node_of,
        cpuset_node_mismatch,
        parked,
        cores,
        glpi_cores,
        numa_highest_plus1,
        nodes,
    }
}

/// Per-sample physical-core figures from per-LP busy fractions.
#[derive(Default, Clone)]
pub struct CoreStats {
    /// Cores whose every sibling is >= 90% busy.
    pub both90: usize,
    /// Cores with at least one but not every sibling >= 90% busy.
    pub one90: usize,
    /// LPs >= 90% busy.
    pub lp90: usize,
}

/// Returns (whole machine, per node in `topo.nodes` order with the node's mean LP busy).
pub fn core_stats(topo: &Topo, busy: &[Vec<f64>]) -> (CoreStats, Vec<(u32, f64, CoreStats)>) {
    let mut all = CoreStats::default();
    let mut per: BTreeMap<u32, (f64, usize, CoreStats)> = BTreeMap::new();
    for (g, row) in busy.iter().enumerate() {
        for (i, &b) in row.iter().enumerate() {
            let n = topo.node_of[g][i];
            let e = per.entry(n).or_default();
            e.0 += b;
            e.1 += 1;
            if b >= 0.9 {
                e.2.lp90 += 1;
                all.lp90 += 1;
            }
        }
    }
    for c in &topo.cores {
        let hot = c.lps.iter().filter(|&&l| busy[c.group][l] >= 0.9).count();
        let e = per.entry(c.node).or_default();
        if hot == c.lps.len() {
            e.2.both90 += 1;
            all.both90 += 1;
        } else if hot > 0 {
            e.2.one90 += 1;
            all.one90 += 1;
        }
    }
    let per = per
        .into_iter()
        .map(|(n, (sum, cnt, s))| (n, sum / cnt.max(1) as f64, s))
        .collect();
    (all, per)
}

#[cfg(test)]
mod tests {
    use super::node_warning;

    #[test]
    fn node_map_matching_the_machine_is_quiet() {
        assert_eq!(node_warning(&[0], 1), None);
        assert_eq!(node_warning(&[0, 1], 2), None);
        // GetNumaHighestNodeNumber failed: nothing to compare against.
        assert_eq!(node_warning(&[0, 1], 0), None);
    }

    #[test]
    fn collapsed_or_unknown_nodes_warn() {
        // The wall's failure mode: two nodes, but group-relative numbers merge them into one.
        assert!(
            node_warning(&[0], 2)
                .unwrap()
                .contains("1 distinct NUMA node")
        );
        assert!(node_warning(&[0, u32::MAX], 2).unwrap().contains("failed"));
    }
}
