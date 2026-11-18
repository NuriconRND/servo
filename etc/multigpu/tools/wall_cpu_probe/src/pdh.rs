//! Optional (--pdh) PDH counters, to calibrate against Task Manager / Resource Monitor
//! (2026-11-18).
//!
//! `\Processor Information(g,n)\% Processor Time` is the same tick-charged busy as CPULP
//! (dev box: equal within one tick). `% Processor Utility` is frequency-scaled
//! (~ idle-cycle busy x `% Processor Performance`) and can exceed 100 -- with turbo an LP
//! that is 80% time-busy can already read 100 there. KB3200459 says Task Manager reads the
//! Utility counters; on 26200 Task Manager's default CPU view was reported time-based
//! (24H2 KB5058411), and what Resource Monitor plots is unverified -- that is what these
//! lines are for: run once next to the GUI and see which column it matches.
//!
//! English counter names (PdhAddEnglishCounterW): the wall's Windows may be localized, and
//! the localized-path API fails there (measure_decode_only.ps1:594-610 hit exactly that).

use windows_sys::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
    PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCloseQuery,
    PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};

/// pdh.h value; not exported by windows-sys 0.61.2.
const PDH_FMT_NOCAP100: u32 = 0x0000_8000;

pub struct Pdh {
    query: PDH_HQUERY,
    counters: Vec<(&'static str, PDH_HCOUNTER)>,
    buf: Vec<u64>,
}

impl Drop for Pdh {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

impl Pdh {
    /// Returns the open query and the labels of the counters that could NOT be added.
    pub fn open() -> Result<(Pdh, Vec<&'static str>), String> {
        let mut query: PDH_HQUERY = std::ptr::null_mut();
        let st = unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) };
        if st != 0 {
            return Err(format!("PdhOpenQueryW failed 0x{st:08x}"));
        }
        let mut counters = Vec::new();
        let mut failed = Vec::new();
        for (label, path) in [
            ("util", "\\Processor Information(*)\\% Processor Utility"),
            ("time", "\\Processor Information(*)\\% Processor Time"),
            (
                "perf",
                "\\Processor Information(*)\\% Processor Performance",
            ),
        ] {
            let w: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
            let mut c: PDH_HCOUNTER = std::ptr::null_mut();
            if unsafe { PdhAddEnglishCounterW(query, w.as_ptr(), 0, &mut c) } == 0 {
                counters.push((label, c));
            } else {
                failed.push(label);
            }
        }
        unsafe { PdhCollectQueryData(query) }; // rate counters need two collections
        Ok((
            Pdh {
                query,
                counters,
                buf: Vec::new(),
            },
            failed,
        ))
    }

    /// Per counter: (label, values[group][lp]); instances are "g,n", totals are skipped.
    /// A value PDH did not deliver stays NaN.
    pub fn sample(&mut self, groups: &[usize]) -> Vec<(&'static str, Vec<Vec<f64>>)> {
        unsafe { PdhCollectQueryData(self.query) };
        let mut out = Vec::new();
        for &(label, c) in &self.counters {
            let fmt = PDH_FMT_DOUBLE | PDH_FMT_NOCAP100;
            let (mut size, mut count) = (0u32, 0u32);
            let st = unsafe {
                PdhGetFormattedCounterArrayW(c, fmt, &mut size, &mut count, std::ptr::null_mut())
            };
            if st != PDH_MORE_DATA {
                continue;
            }
            self.buf.resize((size as usize).div_ceil(8), 0);
            let items = self.buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if unsafe { PdhGetFormattedCounterArrayW(c, fmt, &mut size, &mut count, items) } != 0 {
                continue;
            }
            let mut vals: Vec<Vec<f64>> = groups.iter().map(|&n| vec![f64::NAN; n]).collect();
            for i in 0..count as usize {
                // SAFETY: PDH wrote `count` items into `buf`; names point into it.
                let it = unsafe { &*items.add(i) };
                if it.FmtValue.CStatus != PDH_CSTATUS_VALID_DATA
                    && it.FmtValue.CStatus != PDH_CSTATUS_NEW_DATA
                {
                    continue;
                }
                let mut n = 0;
                while unsafe { *it.szName.add(n) } != 0 {
                    n += 1;
                }
                let name =
                    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(it.szName, n) });
                let mut parts = name.split(',');
                if let (Some(Ok(g)), Some(Ok(lp))) = (
                    parts.next().map(str::parse::<usize>),
                    parts.next().map(str::parse::<usize>),
                ) && g < vals.len()
                    && lp < vals[g].len()
                {
                    vals[g][lp] = unsafe { it.FmtValue.Anonymous.doubleValue };
                }
            }
            out.push((label, vals));
        }
        out
    }
}
