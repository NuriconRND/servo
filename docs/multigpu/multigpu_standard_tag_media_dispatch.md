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

## Verification (2026-06-15, debug build, `--features media-gstreamer`)

Run in wall mode (the present loop keeps `setInterval` alive; a plain single
window starves timers). Diagnostics use `console.error` so they surface via
`log::log!` on stderr regardless of buffering.

- `<img>` pref ON → tiff/tga/exr/ppm/pgm/qoi/hdr/jxl all `decoded=true 640x360`,
  plus the `<picture>` JPEG-XL negotiation. pref OFF → all `decoded=false`
  (onerror fallback); `<picture>` falls back to its PNG `<img>`.
- `<video>` containers pref ON → avi/wmv/ts/flv/mov `canPlay="maybe"`,
  `readyState=4`, playing. pref OFF → `canPlay=""`, not playing → inline fallback.
- rtsp pref ON → log shows `stream_type=NetworkUri network_uri=Some("rtsp://…")`
  and `playbin3 uri = rtsp://…` (live playback needs an rtsp server; the engine
  dispatch is the same path already proven for `<rtsp-stream>`). pref OFF →
  `MEDIA_ERR_SRC_NOT_SUPPORTED` → fallback.
- 3×1 wall regression (no new prefs): `scroll_offsets=matched`,
  `Wall frame barrier complete`, balanced presents, 0 panics.

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
