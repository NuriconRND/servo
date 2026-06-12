# Non-standard media formats: `<x-media>` and `<x-image>`

Extends the wall browser with **non-standard display targets** beyond the
RTSP `<rtsp-stream>` slice, in two categories:

1. **Non-standard video containers** — `mkv`, `mov`, `ts`, `avi`, `flv`, `wmv`
   played through a generic GStreamer-URI element `<x-media>`.
2. **Non-standard still-image formats** — `tiff`, `tga`, `exr`, `ppm`/`pgm`,
   `qoi`, `hdr`, and **JPEG XL (`jxl`)** decoded by a new `<x-image>` element.

The guiding constraint is unchanged from the RTSP work: the standard
`<video>`/`HTMLMediaElement` and `<img>` paths are **not touched**. Every
non-standard format flows through a separate, pref-gated element so its
timeline/decoder quirks can never leak into standards-compliant pages.

Branch: `nonstandard-media-formats` (forked from `rtsp-custom-element`).

---

## Phase A — `<x-media>`: non-standard video containers

`<x-media>` is the general spelling of the same element introduced for RTSP;
`<rtsp-stream>` remains an alias. Both map to `RtspStreamElement`, which feeds
`playbin3` a real URI (`StreamType::NetworkUri`) and lets GStreamer auto-plug
the demuxer + decoder. So "supporting a container" reduces to **loading the
matching demux plugin** — no new element code per container.

### What was added
- `components/servo/gstreamer_plugin_lists/common.rs.in` — added the demuxers:
  `gstavi`, `gstflv`, `gstasf` (wmv), `gstmpegpsdemux`, `gstmpegtsdemux` (ts),
  alongside the RTSP/WebRTC set (`gstrtsp`, `gstudp`, `gstsrtp`). Codecs come
  from `gstlibav`; matroska/isomp4 demuxers were already present.
- `components/script/dom/create.rs` — the runtime `LocalName` guard arm now
  matches `rtsp-stream` **or** `x-media` → `make!(RtspStreamElement)`.
- `python/servo/gstreamer.py` — added `gstmpegts` to `GSTREAMER_BASE_LIBS` so
  the `gstmpegts-1.0-0.dll` dependency of `gstmpegtsdemux` is copied.

### Gating
`<x-media>` shares the `RtspStreamElement` interface, so it is gated by
**`dom_rtsp_stream_enabled`** (not a separate pref).

### Test media (from the bundled `rtsp_testsrc.mp4`)
```
ffmpeg -y -i rtsp_testsrc.mp4 -c copy             test_media/test_media.mkv
ffmpeg -y -i rtsp_testsrc.mp4 -c copy             test_media/test_media.mov
ffmpeg -y -i rtsp_testsrc.mp4 -c copy -f mpegts   test_media/test_media.ts
ffmpeg -y -i rtsp_testsrc.mp4 -c:v mpeg4 -q:v 4   test_media/test_media.avi
ffmpeg -y -i rtsp_testsrc.mp4 -c:v libx264 -f flv test_media/test_media.flv
ffmpeg -y -i rtsp_testsrc.mp4 -c:v wmv2 -q:v 4    test_media/test_media.wmv
```

### Run
```powershell
. ..\scripts\servo_env.ps1
target\release\servoshell.exe --wall-layout ..\config\wall_layout.local_1x1.json `
  --wall-all-tiles --pref dom_rtsp_stream_enabled=true `
  tests\html\multigpu_x_media_containers_probe.html
```
Probe page: `tests/html/multigpu_x_media_containers_probe.html` (3-up grid, one
`<x-media>` per container, polls frame progress).

---

## Phase B — `<x-image>`: non-standard still images

A new replaced element that fetches `src`, decodes the bytes, uploads the
raster to WebRender, and presents it through the **same `MediaFrame` path as
`<video>`/`<rtsp-stream>`** (no `components/layout` changes — `node.media_data()`
returning `Some` is the only hook needed). `<img>` is untouched.

### What was added
- `components/script/dom/html/ximageelement.rs` — the element. Fetches via a
  `FetchResponseListener` (accumulate bytes, decode on EOF), decodes with
  `pixels::load_extended_from_memory(bytes, extension, CorsStatus::Unsafe)`,
  uploads with `paint_api.generate_image_key_blocking` + `add_image`, and
  exposes the frame through `LayoutDom::data()`. API: `src`, `width`/`height`,
  `naturalWidth`/`naturalHeight`, `complete`.
- `components/script_bindings/webidls/XImageElement.webidl` — gated interface.
- `components/script/dom/create.rs` — guard arm `x-image` → `make!(XImageElement)`.
- `components/script/dom/node/node.rs`, `element/element.rs`,
  `virtualmethods.rs` — wire `XImageElement` into `media_data()`, presentational
  hints, and the vtable.
- `components/config/prefs.rs` — **`dom_x_image_enabled`** (default `false`).

### B1 — `image`-crate extended decoders
`pixels::load_extended_from_memory` (a new entry point that bypasses the
standard `<img>` allowlist) tries, in order: the standard allowlisted decoders,
then JPEG XL (see B2), then the `image` crate with content-guessing and an
**extension-hint fallback** (`ImageFormat::from_extension`) for formats with no
magic bytes such as TGA. Workspace `image` features were extended to include
`dds`, `exr`, `ff`, `hdr`, `pnm`, `qoi`, `tga`, `tiff`.

### B2 — JPEG XL via `jxl-oxide`
The `image` crate has no JXL decoder, so `jxl-oxide` (pure Rust) is used:
- `is_jxl()` sniffs the raw codestream (`FF 0A`) and the ISOBMFF JXL signature
  box.
- `decode_jxl()` renders frame 0 (`JxlImage::render_frame(0)` →
  `Render::image_all_channels()`), flattens the interleaved f32 samples
  (1/2/3/4+ channels → RGBA8), and reuses the shared
  `raster_from_rgba8_dynamic_image()` helper.
- Wired ahead of the `image`-crate path in `load_extended_from_memory`.

> jxl-oxide API note: in 0.11.x the framebuffer accessor is
> `Render::image_all_channels() -> FrameBuffer` (with `width()`/`height()`/
> `channels()`/`buf() -> &[f32]`), **not** `Render::image()`.

### Gating
`<x-image>` is gated by **`dom_x_image_enabled`** (default `false`).

### Test images (from a frame of `rtsp_testsrc.mp4`)
```
ffmpeg -y -i rtsp_testsrc.mp4 -frames:v 1 test_media/frame.png
ffmpeg -y -i test_media/frame.png test_media/test_image.tiff
ffmpeg -y -i test_media/frame.png test_media/test_image.tga
ffmpeg -y -i test_media/frame.png test_media/test_image.exr
ffmpeg -y -i test_media/frame.png test_media/test_image.ppm
ffmpeg -y -i test_media/frame.png test_media/test_image.pgm
ffmpeg -y -i test_media/frame.png test_media/test_image.qoi
ffmpeg -y -i test_media/frame.png test_media/test_image.hdr
ffmpeg -y -i test_media/frame.png test_media/test_image.jxl   # needs ffmpeg --enable-libjxl
```
(DDS has no ffmpeg encoder; use ImageMagick `magick frame.png test_image.dds`.)

### Run
```powershell
. ..\scripts\servo_env.ps1
target\release\servoshell.exe --pref dom_x_image_enabled=true `
  tests\html\multigpu_x_image_formats_probe.html
```
Probe page: `tests/html/multigpu_x_image_formats_probe.html` (4-up grid, one
`<x-image>` per format, logs `X_IMAGE <ext>: complete=<bool> WxH`).

> Run trap: launch with `Start-Process` (or run it interactively and close the
> window to exit). Wrapping a windowed servoshell run in a `*>`-redirected
> background shell can terminate the process immediately with an empty log.
> Don't set `RUST_LOG=error` for this probe — the `X_IMAGE` lines are page
> `console.log` output at info level and get suppressed.

---

## Verification status

- **Containers** — `mkv`, `mov`, `ts`, `avi`, `flv`, `wmv` all demux + decode
  through `<x-media>`.
- **Images** — probe reports `complete=true 640x360` for all 8 extended
  formats including `X_IMAGE jxl: complete=true 640x360`; no panics.

Build/format gate (project convention):
```powershell
cargo check -p servo-pixels
cargo build -p servoshell      # or: .\mach build --release
rustfmt --edition 2024 --check --config unstable_features=true `
  --config binop_separator=Back --config imports_granularity=Module `
  --config group_imports=StdExternalCrate <touched .rs files>
```

## Non-goals / notes
- Test assets under `test_media/` are gitignored — regenerate via the ffmpeg
  commands above.
- Audio, seeking, and wall fan-out tuning for `<x-media>`/`<x-image>` are out
  of scope for this slice (single logical WebView; fan-out reuses the existing
  shared-scene path).
- `<img>` and `<video>`/`HTMLMediaElement` behavior is unchanged.
