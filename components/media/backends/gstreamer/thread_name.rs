/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Names for the GStreamer streaming threads, so a CPU sampler can see them.
//!
//! Rust's std sets an OS thread description for every thread it spawns with a
//! name, which is what lets an external sampler attribute CPU time to
//! "Compositor", "Script", and so on. GStreamer's streaming threads are created
//! by GLib in C and carry no such description. On a wall playing 45 videos
//! those ~45 threads are the ones doing the decode and the plane upload -- the
//! ones a CPU investigation cares about most -- and without a name they are
//! indistinguishable from every other unnamed thread in the process.
//!
//! So tag them from the inside, on their first appsink callback, and log the
//! tid -> name mapping as well: with the log alone a sampler's output can be
//! read after the fact, without the sampler having to have been attached.
//!
//! Consumed by `etc/multigpu/tools/thread_cpu_probe`.

#[cfg(target_os = "windows")]
mod imp {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use log::info;
    use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, GetCurrentThreadId, GetNumaHighestNodeNumber, GetNumaNodeProcessorMaskEx,
        SetThreadDescription, SetThreadGroupAffinity,
    };

    thread_local! {
        /// A streaming thread serves one appsink, so the first tag it gets is
        /// the right one. Latching also keeps this off the per-frame path: a
        /// syscall per decoded frame is exactly the kind of cost this module
        /// exists to measure, not to add.
        static TAGGED: Cell<bool> = const { Cell::new(false) };
    }

    static VIDEO_THREADS: AtomicUsize = AtomicUsize::new(0);
    static AUDIO_THREADS: AtomicUsize = AtomicUsize::new(0);

    pub fn tag_video_streaming_thread() {
        let index = tag("ServoGstVideo", &VIDEO_THREADS);
        // `tag` returns usize::MAX when this thread was already tagged; pinning is a
        // once-per-thread action, so skip that case.
        if index != usize::MAX && servo_config::pref!(media_numa_pin_streaming_threads) {
            pin_to_node(index);
        }
    }

    /// Pin this streaming thread to one NUMA node, round-robin by video index.
    ///
    /// ***The point is memory, not cores.*** Threads already reach both sockets when nothing
    /// pins them -- that is the 1.005 cores-per-video case. What they do not get is local
    /// memory: the heap is first-touched on whichever node ran first, Windows then migrates
    /// the thread, and every 3.1MB frame afterwards crosses the interconnect. Hard-confining
    /// the whole process to one node fixes the locality and takes away half the physical
    /// cores instead (0.735, with SMT saturating at 54 videos).
    ///
    /// Nailing each thread to a node gets both: what it allocates lands on its node and it
    /// is the one that reads it back, and the videos split evenly over all 40 physical cores.
    ///
    /// Failures are silent by design -- an unpinned thread still decodes correctly, and this
    /// is a performance knob, not a correctness one. The THREADMAP line records what was
    /// asked for, so `thread_cpu_probe`'s group breakdown can confirm it actually happened.
    fn pin_to_node(index: usize) {
        // SAFETY: both calls only read static topology into out-parameters.
        let mut highest: u32 = 0;
        if unsafe { GetNumaHighestNodeNumber(&mut highest) } == 0 {
            return;
        }
        let nodes = highest as usize + 1;
        if nodes < 2 {
            return; // one node: nothing to spread across
        }
        let node = (index % nodes) as u16;
        let mut affinity: GROUP_AFFINITY = unsafe { std::mem::zeroed() };
        // SAFETY: `affinity` is a zeroed out-parameter sized by the API.
        if unsafe { GetNumaNodeProcessorMaskEx(node, &mut affinity) } == 0 {
            return;
        }
        // SAFETY: GetCurrentThread returns a pseudo-handle that must not be closed, and
        // `affinity` was filled by the call above. A null previous-affinity out-param is
        // allowed and means "do not report it".
        unsafe {
            SetThreadGroupAffinity(GetCurrentThread(), &affinity, std::ptr::null_mut());
        }
        info!(
            target: "media",
            "THREADMAP numa pin: ServoGstVideo-{index} -> node {node} (group {}, mask 0x{:x})",
            affinity.Group, affinity.Mask
        );
    }

    pub fn tag_audio_streaming_thread() {
        let _ = tag("ServoGstAudio", &AUDIO_THREADS);
    }

    /// Returns this thread's index within its kind, or `usize::MAX` if it was already tagged
    /// (so a caller that acts on the index does so once, on the first callback only).
    fn tag(kind: &str, counter: &AtomicUsize) -> usize {
        if TAGGED.with(|tagged| tagged.replace(true)) {
            return usize::MAX;
        }
        let index = counter.fetch_add(1, Ordering::Relaxed);
        let name = format!("{kind}-{index}");
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call, and
        // GetCurrentThread returns a pseudo-handle that must not be closed.
        // A failure costs only the name, so it is deliberately not propagated.
        let tid = unsafe {
            SetThreadDescription(GetCurrentThread(), wide.as_ptr());
            GetCurrentThreadId()
        };
        info!(target: "media", "THREADMAP tid={tid} name={name}");
        index
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    pub fn tag_video_streaming_thread() {}
    pub fn tag_audio_streaming_thread() {}
}

pub use imp::{tag_audio_streaming_thread, tag_video_streaming_thread};
