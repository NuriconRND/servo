# Standard `<img>` / `<video>` media-type dispatch

Branch: `standard-tag-media-dispatch` (off `nonstandard-media-formats`).

## Goal

Earlier work added non-standard media support through **separate** custom elements
(`<x-image>`, `<x-media>`/`<rtsp-stream>`). This feature instead teaches the
**standard** `<img>` / `<video>` elements to detect the media type and dispatch,
so that a page authored with plain standard markup works in both worlds:

- **Standard browser**: an unsupported format / source fails and the page's
  standard fallback (broken-image `onerror`, `<picture>` fallback, or `<video>`
  inline fallback content + `MEDIA_ERR_SRC_NOT_SUPPORTED`) is shown — i.e.
  graceful degradation.
- **Multi-GPU wall browser**: the same markup decodes/plays via the existing
  GStreamer / extended-decoder infrastructure.

All new behavior is **pref-gated, default off**. The existing `<x-image>` /
`<x-media>` / `<rtsp-stream>` elements are kept alongside (standard tags are the
primary path; custom elements remain an explicit opt-in).

## Changes

New prefs (`components/config/prefs.rs`, all default `false`):

- `dom_image_extended_formats_enabled`
- `dom_video_extended_containers_enabled`
- `dom_video_network_uri_enabled`

### `<img>` — extended decode (`dom_image_extended_formats_enabled`)

- `components/net/image_cache.rs` — `decode_bytes_sync` gains an `extension`
  hint and, under the pref, routes raster decode through
  `pixels::load_extended_from_memory` instead of `load_from_memory`. The extended
  function delegates browser-standard formats to `load_from_memory`, so
  PNG/JPEG/WebP/GIF/BMP/ICO are unaffected. The extension is derived from the
  resource URL (needed only for magic-byte-less formats such as TGA). Undecodable
  data still returns `None` → the existing broken-image / `error` fallback.
- `components/script/dom/html/htmlimageelement.rs` — `is_supported_image_mime_type`
  additionally accepts `EXTENDED_IMAGE_MIME_TYPES` under the pref, so
  `<picture><source type="image/jxl">` (etc.) can be negotiated.

### `<video>` — container + rtsp dispatch

- Containers (`dom_video_extended_containers_enabled`): the `<source type>`
  filter and `CanPlayType()` in `htmlmediaelement.rs` accept
  `EXTENDED_CONTAINER_MIME_TYPES` (mkv/avi/wmv/ts/flv/mov) even when the backend's
  `can_play_type` is conservative. Playback itself uses the normal AppSrc →
  playbin3 path (auto-demux from the loaded plugin set). Note Matroska is already
  reported by the upstream registry scanner, so mkv plays regardless of the pref.
- rtsp (`dom_video_network_uri_enabled`): in `create_media_player`, a
  `Resource::Url` with an `rtsp`/`rtsps` scheme is created as a
  `StreamType::NetworkUri` player with the URI passed through (reusing the
  `<rtsp-stream>` wiring); `resource_fetch_algorithm` then **skips** the Servo
  AppSrc fetch for that scheme (the GStreamer player pulls the stream itself).
- Fallback: unchanged — `queue_dedicated_media_source_failure_steps` sets
  `MEDIA_ERR_SRC_NOT_SUPPORTED` and the `<video>` inline children render.

## Test pages

`tests/html/multigpu_standard_img_extended_probe.html` and
`tests/html/multigpu_standard_video_extended_probe.html` — standard markup with
fallbacks, reusing the `servo/test_media/` assets generated for the `<x-image>` /
`<x-media>` probes. Both support `?autoclose=MS` for headless/automated runs.
Both use a flexbox grid sized in viewport units so every tile stays inside the
wall resolution (Servo does not support `display:grid`).

### Thumbnail fallback for standard browsers

So a standard browser shows a **thumbnail** (not just text) for media it cannot
decode, each non-standard asset has a pre-generated 320×180 JPEG thumbnail
(`*.thumb.jpg` in `test_media/`; the standard browser can decode it, the
non-standard original it cannot). Pure-authoring, no engine change:

- **`<img>`**: `<picture><source type="image/x-exr" srcset="photo.exr"><img
  src="photo.thumb.jpg"></picture>` — standard browser skips the unknown typed
  source and loads the thumbnail `<img>`; the wall browser accepts the typed
  source and decodes the real image. Verified in Servo both ways (real 640×360 vs
  thumbnail 320×180).
- **`<video>`**: `<video poster="clip.thumb.jpg">` — real standard browsers show
  the poster when the source can't play; the wall browser hides it once playback
  starts. Servo does **not** render `poster` on a source error (and does not fire
  the `error` event for a rejected `<source type>`), so the probe also overlays
  the thumbnail `<img>` by polling `readyState` — that covers Servo's pref-off
  "standard" preview. Verified: failed clips show the 320×180 thumbnail overlay;
  playing clips show none.

## Verification (2026-06-15, debug build, `--features media-gstreamer`)

Run in wall mode (the present loop keeps `setInterval` alive; a plain single
window starves timers). Diagnostics use `console.error` so they surface via
`log::log!` on stderr regardless of buffering.

- `<img>` pref ON → tiff/tga/exr/ppm/pgm/qoi/hdr/jxl all `decoded=true 640x360`,
  plus the `<picture>` JPEG-XL negotiation. pref OFF → all `decoded=false`
  (onerror fallback); `<picture>` falls back to its PNG `<img>`.
- `<video>` containers pref ON → avi/wmv/ts/flv/mov **play to completion**
  (`adv=true`, `currentTime` advances; all 6 reach 240+ frames simultaneously in
  a 2×1 wall). pref OFF → `canPlay=""` → inline fallback. Requires the playback
  fix below.
- standard `<video>` (mp4/webm, no pref) also **plays** (`currentTime` advances)
  with the fix — previously it froze for every format.
- rtsp pref ON → routed to a `NetworkUri`/playbin3 player; with a live rtsp
  server it plays end-to-end (verified: 240+ YUV frames at ~25fps via
  `gst-validate-rtsp-server-1.0`). pref OFF → `MEDIA_ERR_SRC_NOT_SUPPORTED` →
  fallback.
- 3×1 wall regression (no new prefs): `scroll_offsets=matched`,
  `Wall frame barrier complete`, balanced presents, 0 panics.

## Fix: standard `<video>` sustained-playback regression

Originally the standard `<video>` element **froze after the first frame** for
*every* source/format (mp4/webm/mkv/…), in both wall and normal mode, while the
custom `<x-media>` element played fine. Root cause was in
`HTMLMediaElement::update_media_state` (`htmlmediaelement.rs`): the pause branch
guard was just `is_playing`, so once the element was potentially-playing **and**
the player was running (both true), control fell through to that branch and
called `player.pause()` on every invocation — pausing the GStreamer pipeline
mid-playback. State logs showed `play() → Playing → pause() → Paused` then a
permanent stall (the decoder produced ~3 preroll frames and halted).

The fix gates the branch on `!is_potentially_playing() && is_playing`, and only
pauses for *genuine* reasons (`paused`/`ended`/`error`) — not a transient
`is_blocked_media_element()` (a readyState dip), which would deadlock this
backend (pausing halts frame production, so readyState never recovers). After the
fix, standard `<video>` plays all formats; the probe tracks `currentTime`
(`adv=`/`t=`) to confirm real frame advancement (`readyState`/`paused` alone do
not catch a freeze).

## Status: non-standard `<video>` playback verified

**Playback itself is verified working** for standard `<video>` across standard
formats (mp4/webm) and the non-standard containers (mkv/avi/wmv/ts/flv/mov), in
1×1/2×1/3×1 walls, plus `rtsp://` end-to-end. This closes the playback
verification. One follow-up remains below: looping of some streaming containers.

## Known limitation & proposals: looping of streaming containers

`loop` is not reliable for every container. Verified behavior (long runs, >2
loop cycles, via the `seeking`/`seeked`/`ended` events):

| container | loop |
|---|---|
| mkv, avi, mov | loops reliably (∞) |
| ts, flv | loops 0–1× then freezes at `t=0` |
| wmv | never loops (freezes at `t=0` on the first loop) |

Root cause (not intentional, not a Servo logic bug): HTML `loop` replays by
`seek(0)` (`end_of_playback_in_forwards_direction` → `seek`,
`htmlmediaelement.rs`). The failing containers fire the `seeking` event but never
`seeked` — i.e. the **GStreamer seek-to-0 never completes (no SeekDone)** and the
element is stuck in `seeking`/`t=0`. Seeking through the standard `<video>`
**ServoSrc (appsrc *push* source)** is unreliable for streaming containers that
lack a robust seek index: MPEG-TS/FLV complete the seek non-deterministically
(hence "loops once then stops"); ASF/WMV effectively never complete it. Matroska/
AVI/ISO-BMFF have seek indices, so they loop fine. (`<x-media>` uses filesrc,
which seeks better, but it is a live-stream element with no loop.)

Proposals to resolve (not yet implemented — playback works, only re-looping of
these three containers is affected):

1. **loop-via-reload** — on end of stream, re-run the resource load (re-create
   the player) instead of `seek(0)`. Robust for every container, but adds a brief
   gap (pipeline teardown+setup) and a re-fetch between loops, including for the
   containers that currently loop seamlessly.
2. **seek-timeout fallback** — keep `seek(0)` (no gap for mkv/avi/mov/mp4/webm),
   but if `seeked` does not arrive within a short timeout, fall back to a reload
   for that element. Surgical (only the failing containers reload), but more
   complex to implement and tune.

Until one is implemented, prefer mkv/avi/mov (or standard mp4/webm) for content
that must loop on the wall.

## Build gotcha

`cargo build -p servoshell --features media-gstreamer` does **not** copy the
GStreamer plugin DLLs listed in `gstreamer_plugin_lists/common.rs.in` into
`target\debug` (only `mach build` runs `package_gstreamer_dlls`). Any missing
plugin makes GStreamer init fail with `ErrorLoadingPlugins`, which is **fatal**
(`components/servo/servo.rs` → `std::process::exit(1)`). Either run `mach build`,
or manually copy the required plugins from
`target\dependencies\gstreamer\1.0\msvc_X86_64\lib\gstreamer-1.0\`
(gstavi/gstflv/gstasf/gstmpegpsdemux/gstmpegtsdemux/gstsrtp) **and** the base lib
`gstmpegts-1.0-0.dll` from the bundle `bin\` (required by `gstmpegtsdemux`).
