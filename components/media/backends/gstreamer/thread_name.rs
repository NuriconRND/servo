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
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, GetCurrentThreadId, SetThreadDescription,
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
        tag("ServoGstVideo", &VIDEO_THREADS);
    }

    pub fn tag_audio_streaming_thread() {
        tag("ServoGstAudio", &AUDIO_THREADS);
    }

    fn tag(kind: &str, counter: &AtomicUsize) {
        if TAGGED.with(|tagged| tagged.replace(true)) {
            return;
        }
        let name = format!("{kind}-{}", counter.fetch_add(1, Ordering::Relaxed));
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call, and
        // GetCurrentThread returns a pseudo-handle that must not be closed.
        // A failure costs only the name, so it is deliberately not propagated.
        let tid = unsafe {
            SetThreadDescription(GetCurrentThread(), wide.as_ptr());
            GetCurrentThreadId()
        };
        info!(target: "media", "THREADMAP tid={tid} name={name}");
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    pub fn tag_video_streaming_thread() {}
    pub fn tag_audio_streaming_thread() {}
}

pub use imp::{tag_audio_streaming_thread, tag_video_streaming_thread};
