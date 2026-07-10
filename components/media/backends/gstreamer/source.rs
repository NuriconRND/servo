/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::convert::TryFrom;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use glib::subclass::prelude::*;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use url::Url;

const MAX_SRC_QUEUE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB.

// Opt-in servosrc byte cache (SERVO_MEDIA_SOURCE_CACHE=1, default off). When a seekable
// source of known size is played, the bytes the script pushes on the first (0..EOF
// sequential) pass are copied into a per-source buffer. Once the buffer contiguously covers
// the whole input the source becomes self-sufficient: every later seek-data (e.g. a gapless
// loop rewind, which otherwise triggers a SeekData round-trip to the script thread) is served
// locally from the cache instead. This removes the script-thread round-trip that, with many
// simultaneous looping tiles, contends and produces the per-tile stalls at loop-wrap
// boundaries (see investigation-loop-stall-report.md §14). Off => byte-for-byte unchanged.
const SOURCE_CACHE_ENV: &str = "SERVO_MEDIA_SOURCE_CACHE";
// Only cache sources at or below this size (a plain in-RAM copy per source). Larger sources
// keep the existing script round-trip.
const SOURCE_CACHE_CAP: u64 = 256 * 1024 * 1024; // 256 MB.
// Bytes fed to appsrc per need-data while self-sufficient. Bounded so the streaming-thread
// callback never does one large blocking copy; appsrc back-pressure (need-data/enough-data)
// drives repeated chunks, mirroring the script's chunked push.
const SOURCE_CACHE_SERVE_CHUNK: u64 = 8 * 1024 * 1024; // 8 MB.

fn source_cache_enabled() -> bool {
    use std::sync::LazyLock;
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var(SOURCE_CACHE_ENV).is_ok_and(|value| {
            let value = value.trim();
            value == "1" ||
                value.eq_ignore_ascii_case("true") ||
                value.eq_ignore_ascii_case("yes") ||
                value.eq_ignore_ascii_case("on")
        })
    });
    *ENABLED
}

// Implementation sub-module of the GObject
mod imp {
    use std::sync::LazyLock;

    use super::*;

    macro_rules! inner_appsrc_proxy {
        ($fn_name:ident, $return_type:ty) => {
            pub fn $fn_name(&self) -> $return_type {
                self.appsrc.$fn_name()
            }
        };

        ($fn_name:ident, $arg1:ident, $arg1_type:ty, $return_type:ty) => {
            pub fn $fn_name(&self, $arg1: $arg1_type) -> $return_type {
                self.appsrc.$fn_name($arg1)
            }
        };
    }

    #[derive(Debug, Default)]
    struct Position {
        offset: u64,
        requested_offset: u64,
    }

    // Byte cache state (see `SOURCE_CACHE_ENV`). Guarded by its own mutex; this mutex is
    // only ever acquired on its own or while the `position` mutex is already held (recording
    // path). It is never held while acquiring `position`, so there is no lock cycle.
    #[derive(Default)]
    struct SourceCache {
        // True once the size is known, the env is on, and the size is within the cap.
        eligible: bool,
        // True once `covered == total`: the source now serves seeks/needs from `data`.
        complete: bool,
        // Total input size in bytes (set when eligibility is decided).
        total: u64,
        // Highest byte offset contiguously covered from 0 by recorded pushes.
        covered: u64,
        // Serving cursor while self-sufficient: next byte to feed appsrc.
        read_offset: u64,
        // The cached bytes (pre-allocated to `total` when eligible; empty otherwise).
        data: Vec<u8>,
    }

    // The actual data structure that stores our values. This is not accessible
    // directly from the outside.
    pub struct ServoSrc {
        cat: gstreamer::DebugCategory,
        appsrc: gstreamer_app::AppSrc,
        srcpad: gstreamer::GhostPad,
        position: Mutex<Position>,
        seeking: AtomicBool,
        seekable: AtomicBool,
        size: Mutex<Option<i64>>,
        cache: Mutex<SourceCache>,
        // Fast-path mirror of `cache.complete` for the appsrc callbacks (no lock needed to
        // decide whether the source is self-sufficient).
        cache_complete: AtomicBool,
    }

    impl ServoSrc {
        pub fn set_size(&self, size: i64) {
            if self.seeking.load(Ordering::Relaxed) {
                // We ignore set_size requests if we are seeking.
                // The size value is temporarily stored so it
                // is properly set once we are done seeking.
                *self.size.lock().unwrap() = Some(size);
            } else if self.appsrc.size() == -1 {
                self.appsrc.set_size(size);
            }

            self.maybe_init_cache(size);
        }

        // Decide byte-cache eligibility once the input size is known. Idempotent: only the
        // first eligible call allocates the buffer. No-op unless the env knob is on and the
        // size is positive and within the cap (see `SOURCE_CACHE_ENV`).
        fn maybe_init_cache(&self, size: i64) {
            if !source_cache_enabled() || size <= 0 {
                return;
            }
            let total = size as u64;
            if total > SOURCE_CACHE_CAP {
                return;
            }
            let mut cache = self.cache.lock().unwrap();
            if cache.eligible {
                return;
            }
            cache.total = total;
            cache.data = vec![0u8; total as usize];
            cache.eligible = true;
            log::info!(
                "servosrc byte cache armed: size={}MB (cap {}MB)",
                total / (1024 * 1024),
                SOURCE_CACHE_CAP / (1024 * 1024),
            );
        }

        // Record bytes pushed by the script into the cache during the first pass, advancing
        // the contiguous coverage watermark. When coverage reaches the total the source
        // switches to self-sufficient mode. Called while holding the `position` lock (lock
        // order: position -> cache). No-op once complete or ineligible.
        fn cache_record(&self, start: u64, data: &[u8]) {
            let mut cache = self.cache.lock().unwrap();
            if !cache.eligible || cache.complete {
                return;
            }
            let total = cache.total;
            if start >= total {
                return;
            }
            let end = (start + data.len() as u64).min(total);
            let copy_len = (end - start) as usize;
            cache.data[start as usize..end as usize].copy_from_slice(&data[..copy_len]);
            // First playback is a sequential 0..EOF push, so `start == covered` each time;
            // only advance the watermark when this range extends the contiguous prefix.
            if start <= cache.covered && end > cache.covered {
                cache.covered = end;
            }
            if cache.covered >= total {
                cache.complete = true;
                self.cache_complete.store(true, Ordering::Relaxed);
                log::info!(
                    "servosrc source cache complete, size={}MB",
                    total / (1024 * 1024),
                );
            }
        }

        // Self-sufficient seek: point the serving cursor at `offset` and report handled so the
        // appsrc callback skips the SeekData script round-trip. Returns false when the cache
        // is not complete (caller falls back to the existing round-trip path).
        pub fn cache_serve_seek(&self, offset: u64) -> bool {
            if !self.cache_complete.load(Ordering::Relaxed) {
                return false;
            }
            let mut cache = self.cache.lock().unwrap();
            cache.read_offset = offset.min(cache.total);
            log::info!("servosrc serving seek to offset {offset} from cache");
            true
        }

        // Self-sufficient need-data: feed the next bounded chunk from the cache directly into
        // appsrc, without asking the script. Returns false when the cache is not complete
        // (caller notifies the script as before). Returning true with nothing left to serve is
        // intentional: the pipeline reaches segment end and rewinds via seek-data.
        pub fn cache_serve_need<O: IsA<gstreamer::Object>>(&self, parent: &O) -> bool {
            if !self.cache_complete.load(Ordering::Relaxed) {
                return false;
            }
            // Grab a bounded chunk, release the cache lock, then push (never hold the lock
            // across the appsrc push, which can re-enter enough-data).
            let (start, chunk) = {
                let mut cache = self.cache.lock().unwrap();
                let remaining = cache.total - cache.read_offset;
                if remaining == 0 {
                    return true;
                }
                let len = remaining.min(SOURCE_CACHE_SERVE_CHUNK);
                let start = cache.read_offset;
                let chunk = cache.data[start as usize..(start + len) as usize].to_vec();
                cache.read_offset += len;
                (start, chunk)
            };
            let chunk_len = chunk.len();
            self.push_bytes(parent, start, chunk);
            gstreamer::debug!(
                self.cat,
                obj = parent,
                "served {} bytes from cache at offset {}",
                chunk_len,
                start
            );
            true
        }

        // Split `data` into appsrc-friendly blocks tagged with absolute byte offsets and push
        // them, growing the announced size if needed. Shared block-push core for the
        // self-sufficient serving path; it touches neither the `position` nor `cache` mutex
        // (the caller owns the offset), so it is safe to call with no lock held.
        fn push_bytes<O: IsA<gstreamer::Object>>(
            &self,
            parent: &O,
            starting_offset: u64,
            data: Vec<u8>,
        ) {
            let length = data.len() as u64;
            if let Ok(size) = u64::try_from(self.appsrc.size()) &&
                starting_offset + length > size
            {
                let new_size = i64::try_from(starting_offset + length).unwrap();
                self.appsrc.set_size(new_size);
            }
            let block_size = 4096u64;
            let num_blocks = (length as f64 / block_size as f64).ceil() as u64;
            for i in 0..num_blocks {
                let start = (i * block_size) as usize;
                let size = usize::try_from(block_size.min(length - start as u64)).unwrap();
                let end = start + size;

                let buffer_offset = starting_offset + start as u64;
                let buffer_offset_end = buffer_offset + size as u64;

                let subdata = Vec::from(&data[start..end]);
                let mut buffer = gstreamer::Buffer::from_slice(subdata);
                {
                    let buffer = buffer.get_mut().unwrap();
                    buffer.set_offset(buffer_offset);
                    buffer.set_offset_end(buffer_offset_end);
                }

                match self.appsrc.push_buffer(buffer) {
                    Ok(_) |
                    Err(gstreamer::FlowError::Eos) |
                    Err(gstreamer::FlowError::Flushing) => {},
                    Err(error) => {
                        gstreamer::warning!(
                            self.cat,
                            obj = parent,
                            "cache serve push failed: {:?}",
                            error
                        );
                        break;
                    },
                }
            }
        }

        pub fn set_seekable(&self, seekable: bool) {
            self.seekable.store(seekable, Ordering::Relaxed);
        }

        pub fn set_seek_offset<O: IsA<gstreamer::Object>>(&self, parent: &O, offset: u64) -> bool {
            let mut pos = self.position.lock().unwrap();

            if pos.offset == offset || pos.requested_offset != 0 {
                false
            } else {
                self.seeking.store(true, Ordering::Relaxed);
                pos.requested_offset = offset;
                gstreamer::debug!(
                    self.cat,
                    obj = parent,
                    "seeking to offset: {}",
                    pos.requested_offset
                );

                true
            }
        }

        pub fn set_seek_done(&self) {
            self.seeking.store(false, Ordering::Relaxed);

            if let Some(size) = self.size.lock().unwrap().take() &&
                self.appsrc.size() == -1
            {
                self.appsrc.set_size(size);
            }

            let mut pos = self.position.lock().unwrap();
            pos.offset = pos.requested_offset;
            pos.requested_offset = 0;
        }

        pub fn push_buffer<O: IsA<gstreamer::Object>>(
            &self,
            parent: &O,
            data: Vec<u8>,
        ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
            // Self-sufficient mode: the cache now feeds appsrc, so a late script push (its
            // fetch was still draining when the cache completed) is redundant. Ignore it so it
            // cannot inject bytes at a stale offset and corrupt the stream. Harmless because
            // the script stops receiving NeedData/SeekData once we serve locally.
            if self.cache_complete.load(Ordering::Relaxed) {
                gstreamer::debug!(
                    self.cat,
                    obj = parent,
                    "cache complete, ignored late script push"
                );
                return Ok(gstreamer::FlowSuccess::Ok);
            }

            if self.seeking.load(Ordering::Relaxed) {
                gstreamer::debug!(self.cat, obj = parent, "seek in progress, ignored data");
                return Ok(gstreamer::FlowSuccess::Ok);
            }

            let mut pos = self.position.lock().unwrap(); // will block seeking

            let length = u64::try_from(data.len()).unwrap();
            let mut data_offset = 0;

            let buffer_starting_offset = pos.offset;

            // @TODO: optimization: update the element's blocksize by
            // X factor given current length

            pos.offset += length;

            // Record the pushed bytes into the cache (no-op unless eligible). Done while the
            // position lock is held (lock order position -> cache).
            self.cache_record(buffer_starting_offset, &data);

            gstreamer::trace!(self.cat, obj = parent, "offset: {}", pos.offset);

            // set the stream size (in bytes) to current offset if
            // size is lesser than it
            if let Ok(size) = u64::try_from(self.appsrc.size()) &&
                pos.offset > size
            {
                gstreamer::debug!(
                    self.cat,
                    obj = parent,
                    "Updating internal size from {} to {}",
                    size,
                    pos.offset
                );
                let new_size = i64::try_from(pos.offset).unwrap();
                self.appsrc.set_size(new_size);
            }

            // Split the received vec<> into buffers that are of a
            // size basesrc suggest. It is important not to push
            // buffers that are too large, otherwise incorrect
            // buffering messages can be sent from the pipeline
            let block_size = 4096;
            let num_blocks = ((length - data_offset) as f64 / block_size as f64).ceil() as u64;

            gstreamer::log!(
                self.cat,
                obj = parent,
                "Splitting the received vec into {} blocks",
                num_blocks
            );

            let mut ret: Result<gstreamer::FlowSuccess, gstreamer::FlowError> =
                Ok(gstreamer::FlowSuccess::Ok);
            for i in 0..num_blocks {
                let start = usize::try_from(i * block_size + data_offset).unwrap();
                data_offset = 0;
                let size = usize::try_from(block_size.min(length - start as u64)).unwrap();
                let end = start + size;

                let buffer_offset = buffer_starting_offset + start as u64;
                let buffer_offset_end = buffer_offset + size as u64;

                let subdata = Vec::from(&data[start..end]);
                let mut buffer = gstreamer::Buffer::from_slice(subdata);
                {
                    let buffer = buffer.get_mut().unwrap();
                    buffer.set_offset(buffer_offset);
                    buffer.set_offset_end(buffer_offset_end);
                }

                if self.seeking.load(Ordering::Relaxed) {
                    gstreamer::trace!(
                        self.cat,
                        obj = parent,
                        "stopping buffer appends due to seek"
                    );
                    ret = Ok(gstreamer::FlowSuccess::Ok);
                    break;
                }

                gstreamer::trace!(self.cat, obj = parent, "Pushing buffer {:?}", buffer);

                ret = self.appsrc.push_buffer(buffer);
                match ret {
                    Ok(_) => (),
                    Err(gstreamer::FlowError::Eos) | Err(gstreamer::FlowError::Flushing) => {
                        ret = Ok(gstreamer::FlowSuccess::Ok)
                    },
                    Err(_) => break,
                }
            }

            ret
        }

        inner_appsrc_proxy!(end_of_stream, Result<gstreamer::FlowSuccess, gstreamer::FlowError>);
        inner_appsrc_proxy!(set_callbacks, callbacks, gstreamer_app::AppSrcCallbacks, ());

        fn query(&self, pad: &gstreamer::GhostPad, query: &mut gstreamer::QueryRef) -> bool {
            gstreamer::log!(self.cat, obj = pad, "Handling query {:?}", query);

            // In order to make buffering/downloading work as we want, apart from
            // setting the appropriate flags on the player playbin,
            // the source:
            //
            // 1. Announces seekability when the media element confirmed it.
            // 2. Assumes seekable = true as default.
            // 3. Keeps assuming bandwidth limited.
            // 4. set_seekable is called when range requests are supported or not.
            let ret = match query.view_mut() {
                gstreamer::QueryViewMut::Scheduling(ref mut q) => {
                    let seekability_flag = if self.seekable.load(Ordering::Relaxed) {
                        gstreamer::SchedulingFlags::SEEKABLE
                    } else {
                        gstreamer::SchedulingFlags::SEQUENTIAL
                    };
                    q.set(
                        seekability_flag | gstreamer::SchedulingFlags::BANDWIDTH_LIMITED,
                        1,
                        -1,
                        0,
                    );
                    q.add_scheduling_modes([gstreamer::PadMode::Push]);
                    true
                },
                _ => gstreamer::Pad::query_default(pad, Some(&*self.obj()), query),
            };

            if ret {
                gstreamer::log!(self.cat, obj = pad, "Handled query {:?}", query);
            } else {
                gstreamer::info!(self.cat, obj = pad, "Didn't handle query {:?}", query);
            }
            ret
        }
    }

    // Basic declaration of our type for the GObject type system
    #[glib::object_subclass]
    impl ObjectSubclass for ServoSrc {
        const NAME: &'static str = "ServoSrc";
        type Type = super::ServoSrc;
        type ParentType = gstreamer::Bin;
        type Interfaces = (gstreamer::URIHandler,);

        // Called once at the very beginning of instantiation of each instance and
        // creates the data structure that contains all our state
        fn with_class(klass: &Self::Class) -> Self {
            let app_src = gstreamer::ElementFactory::make("appsrc")
                .build()
                .map(|elem| elem.downcast::<gstreamer_app::AppSrc>().unwrap())
                .expect("Could not create appsrc element");

            let pad_templ = klass.pad_template("src").unwrap();
            let ghost_pad = gstreamer::GhostPad::builder_from_template(&pad_templ)
                .query_function(|pad, parent, query| {
                    ServoSrc::catch_panic_pad_function(
                        parent,
                        || false,
                        |servosrc| servosrc.query(pad, query),
                    )
                })
                .build();

            Self {
                cat: gstreamer::DebugCategory::new(
                    "servosrc",
                    gstreamer::DebugColorFlags::empty(),
                    Some("Servo source"),
                ),
                appsrc: app_src,
                srcpad: ghost_pad,
                position: Mutex::new(Default::default()),
                seeking: AtomicBool::new(false),
                seekable: AtomicBool::new(true),
                size: Mutex::new(None),
                cache: Mutex::new(SourceCache::default()),
                cache_complete: AtomicBool::new(false),
            }
        }
    }

    // The ObjectImpl trait provides the setters/getters for GObject properties.
    // Here we need to provide the values that are internally stored back to the
    // caller, or store whatever new value the caller is providing.
    //
    // This maps between the GObject properties and our internal storage of the
    // corresponding values of the properties.
    impl ObjectImpl for ServoSrc {
        // Called right after construction of a new instance
        fn constructed(&self) {
            // Call the parent class' ::constructed() implementation first
            self.parent_constructed();

            self.obj()
                .add(&self.appsrc)
                .expect("Could not add appsrc element to bin");

            let target_pad = self.appsrc.static_pad("src");
            self.srcpad.set_target(target_pad.as_ref()).unwrap();

            self.obj()
                .add_pad(&self.srcpad)
                .expect("Could not add source pad to bin");

            self.appsrc.set_caps(None::<&gstreamer::Caps>);
            self.appsrc.set_max_bytes(MAX_SRC_QUEUE_SIZE);
            self.appsrc.set_block(false);
            self.appsrc.set_format(gstreamer::Format::Bytes);
            self.appsrc
                .set_stream_type(gstreamer_app::AppStreamType::Seekable);

            self.obj()
                .set_element_flags(gstreamer::ElementFlags::SOURCE);
        }
    }

    impl GstObjectImpl for ServoSrc {}

    // Implementation of gstreamer::Element virtual methods
    impl ElementImpl for ServoSrc {
        fn metadata() -> Option<&'static gstreamer::subclass::ElementMetadata> {
            static ELEMENT_METADATA: LazyLock<gstreamer::subclass::ElementMetadata> =
                LazyLock::new(|| {
                    gstreamer::subclass::ElementMetadata::new(
                        "Servo Media Source",
                        "Source/Audio/Video",
                        "Feed player with media data",
                        "Servo developers",
                    )
                });

            Some(&*ELEMENT_METADATA)
        }

        fn pad_templates() -> &'static [gstreamer::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gstreamer::PadTemplate>> = LazyLock::new(|| {
                let caps = gstreamer::Caps::new_any();
                let src_pad_template = gstreamer::PadTemplate::new(
                    "src",
                    gstreamer::PadDirection::Src,
                    gstreamer::PadPresence::Always,
                    &caps,
                )
                .unwrap();

                vec![src_pad_template]
            });

            PAD_TEMPLATES.as_ref()
        }
    }

    // Implementation of gstreamer::Bin virtual methods
    impl BinImpl for ServoSrc {}

    impl URIHandlerImpl for ServoSrc {
        const URI_TYPE: gstreamer::URIType = gstreamer::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["servosrc"]
        }

        fn uri(&self) -> Option<String> {
            Some("servosrc://".to_string())
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            if let Ok(uri) = Url::parse(uri) &&
                uri.scheme() == "servosrc"
            {
                return Ok(());
            }
            Err(glib::Error::new(
                gstreamer::URIError::BadUri,
                format!("Invalid URI '{:?}'", uri,).as_str(),
            ))
        }
    }
}

// Public part of the ServoSrc type. This behaves like a normal
// GObject binding
glib::wrapper! {
    pub struct ServoSrc(ObjectSubclass<imp::ServoSrc>)
        @extends gstreamer::Bin, gstreamer::Element, gstreamer::Object, @implements gstreamer::URIHandler;
}

unsafe impl Send for ServoSrc {}
unsafe impl Sync for ServoSrc {}

impl ServoSrc {
    pub fn set_size(&self, size: i64) {
        self.imp().set_size(size);
    }

    pub fn set_seekable(&self, seekable: bool) {
        self.imp().set_seekable(seekable);
    }

    pub fn set_seek_offset(&self, offset: u64) -> bool {
        self.imp().set_seek_offset(self, offset)
    }

    pub fn set_seek_done(&self) {
        self.imp().set_seek_done();
    }

    pub fn push_buffer(
        &self,
        data: Vec<u8>,
    ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
        self.imp().push_buffer(self, data)
    }

    pub fn push_end_of_stream(&self) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
        self.imp().end_of_stream()
    }

    pub fn set_callbacks(&self, callbacks: gstreamer_app::AppSrcCallbacks) {
        self.imp().set_callbacks(callbacks)
    }

    /// If the byte cache is complete, point its serving cursor at `offset` and return true so
    /// the appsrc seek-data callback can skip the SeekData script round-trip. Returns false
    /// otherwise (the caller keeps the existing behavior).
    pub fn cache_serve_seek(&self, offset: u64) -> bool {
        self.imp().cache_serve_seek(offset)
    }

    /// If the byte cache is complete, feed the next chunk from it into appsrc and return true
    /// so the appsrc need-data callback can skip notifying the script. Returns false otherwise.
    pub fn cache_serve_need(&self) -> bool {
        self.imp().cache_serve_need(self)
    }
}

// Registers the type for our element, and then registers in GStreamer
// under the name "servosrc" for being able to instantiate it via e.g.
// gstreamer::ElementFactory::make().
pub fn register_servo_src() -> Result<(), glib::BoolError> {
    gstreamer::Element::register(
        None,
        "servosrc",
        gstreamer::Rank::NONE,
        ServoSrc::static_type(),
    )
}
