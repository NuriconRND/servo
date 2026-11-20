//! Thread families (2026-11-18).
//!
//! `group_of` is the deleted thread_cpu_probe's rule set, restored verbatim from
//! a5a9dd81cb0^:etc/multigpu/tools/thread_cpu_probe/src/main.rs together with its unit tests,
//! so a family name here means exactly what it meant in the 2026-08/09 .threads.txt files
//! ("ServoGstVideo-3" -> "ServoGstVideo", "multiqueue4:src_0" -> "multiqueue:src").
//!
//! New: an unnamed thread is no longer one "(unnamed)" bucket. It is labelled by the module
//! its Win32StartAddress lies in, "(unnamed:ucrtbase.dll)" (GLib/CRT _beginthreadex threads),
//! "(unnamed:ntdll.dll)" (thread-pool workers), "(unnamed:<driver>.dll)" (UMD threads). The
//! 2026-08-26 v54_dt2 run hid 167 avdec workers using 55 cores in a single "(unnamed)" row.

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
pub fn group_of(name: Option<&str>) -> String {
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

/// The family a thread is reported under: its collapsed name when it has one, otherwise
/// "(unnamed:<module of its start address>)", or "(unnamed)" when the module is unknown.
pub fn family_of(name: Option<&str>, start_module: Option<&str>) -> String {
    match (name, start_module) {
        (Some(_), _) => group_of(name),
        (None, Some(m)) if !m.is_empty() => format!("(unnamed:{m})"),
        (None, _) => group_of(None),
    }
}

/// Last path component of an NT device path or a Win32 path
/// ("\Device\HarddiskVolume3\Windows\System32\ntdll.dll" -> "ntdll.dll").
pub fn basename(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// Log lines are space-separated `key=value` fields, so a family or process name must not
/// contain a space or an '='. Only used when printing; the family key itself is untouched.
pub fn token(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_whitespace() || c == '=' {
                '_'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{basename, family_of, group_of, token};

    fn group(name: &str) -> String {
        group_of(Some(name))
    }

    // The five tests below are the old thread_cpu_probe's, unchanged (a5a9dd81cb0^).

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

    // New in wall_cpu_probe (2026-11-18).

    #[test]
    fn current_engine_thread_names_collapse() {
        // Names the 348279f90d6 engine gives its threads (painter.rs:790, webrender init.rs).
        assert_eq!(group("WRWorker#12"), "WRWorker");
        assert_eq!(group("WRRenderBackend#3"), "WRRenderBackend");
        assert_eq!(group("WRSceneBuilderLP#1"), "WRSceneBuilderLP");
        assert_eq!(group("DcompCommit2"), "DcompCommit");
        assert_eq!(group("GstPlayerDisposal17"), "GstPlayerDisposal");
        assert_eq!(group("WallTilePainter"), "WallTilePainter");
    }

    #[test]
    fn unnamed_threads_are_labelled_by_start_module() {
        assert_eq!(
            family_of(None, Some("ucrtbase.dll")),
            "(unnamed:ucrtbase.dll)"
        );
        assert_eq!(family_of(None, Some("")), "(unnamed)");
        assert_eq!(family_of(None, None), "(unnamed)");
        // A name always wins over the module.
        assert_eq!(
            family_of(Some("ServoGstVideo-9"), Some("ucrtbase.dll")),
            "ServoGstVideo"
        );
    }

    #[test]
    fn basename_takes_the_last_component() {
        assert_eq!(
            basename("\\Device\\HarddiskVolume3\\Windows\\System32\\ntdll.dll"),
            "ntdll.dll"
        );
        assert_eq!(basename("C:/x/y/amdxx64.dll"), "amdxx64.dll");
        assert_eq!(basename("plain.dll"), "plain.dll");
    }

    #[test]
    fn token_keeps_fields_parseable() {
        assert_eq!(token("Some Thread=x"), "Some_Thread_x");
        assert_eq!(token("multiqueue:src"), "multiqueue:src");
    }
}
