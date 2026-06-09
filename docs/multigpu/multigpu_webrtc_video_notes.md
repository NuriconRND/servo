# Servo WebRTC And Video Multi-GPU Notes

Date: 2026-06-07

## Conclusion

Servo can process WebRTC/video content conditionally. The codebase has WebRTC DOM bindings, a GStreamer WebRTC backend, MediaStream-to-HTMLMediaElement wiring, and video frames flowing into WebRender images.

The tiled multi-GPU prototype can render this content after it becomes normal page content, but the media pipeline itself is not distributed across GPUs. In v1, WebRTC decode and media processing should be treated as one Servo/GStreamer pipeline feeding tiled WebRender renderers.

## Current Distribution Plan

The staged plan for moving video/WebRTC toward multi-GPU behavior is tracked in:

- `docs/multigpu/multigpu_webrtc_video_distribution_plan.md`

Current direction:

- First prove normal video-file render/upload fan-out across wall paint targets.
- Then validate the same tiled rendering path with WebRTC-originated video.
- Treat decode distribution as a follow-up feasibility spike, not as the first implementation step.
- Keep `canvas.captureStream()` out of the primary validation path.

Current implementation status:

- The video-file probe page exists at `tests/html/multigpu_wall_video_file_probe.html`.
- The WebRTC/getUserMedia probe page exists at `tests/html/multigpu_wall_webrtc_video_probe.html`.
- Media frame diagnostics are emitted as `Wall media frame:`.
- Paint-side wall image fan-out diagnostics are emitted as `Wall media image fanout:`.
- `wall_perf_analyzer` can summarize media counters and run the `--validate-media-wall` smoke gate.
- Deterministic no-camera WebRTC validation is available through `--pref media_capture_mocking_enabled=true`.

## Current Build State

The validation runs below used a debug `servoshell.exe` rebuilt with the GStreamer media stack:

```powershell
$env:PATH = 'C:\Program Files\LLVM\bin;' + $env:PATH
$env:PYTHONIOENCODING = 'utf-8'
.\mach.bat build --media-stack gstreamer -j 8
```

On this Windows machine:

- `C:\Program Files\LLVM\bin` was required so the build could find `lld-link.exe`.
- `PYTHONIOENCODING=utf-8` was required to avoid a CP949 `UnicodeEncodeError` during post-build output.
- Runtime validation used `RUST_LOG=info` so wall, media, and paint diagnostics were captured.

The resulting `servo/target/debug` output contains GStreamer/WebRTC runtime DLLs, including:

- `gstreamer-1.0-0.dll`
- `gstwebrtc.dll`
- `gstwebrtc-1.0-0.dll`
- `gstwebrtcnice-1.0-0.dll`
- `gstapp.dll`
- `gstapp-1.0-0.dll`
- `gstlibav.dll`

This indicates the current debug build output is packaged with the GStreamer pieces needed for WebRTC/video experiments.

## Video-File Validation Results

Date: 2026-06-09

The video-file path has been validated through V5-V7 in `multigpu_webrtc_video_distribution_plan.md`.

### V5: 1x1 Baseline

- Log: `target/multigpu_logs/v5_1x1_video_stderr_20260609_114358.log`
- Analyzer result: 671 raw media frames, 1 image add, 670 image updates, 0 panics, 0 errors.
- Result: the rebuilt GStreamer-backed `servoshell.exe` plays the video-file probe and produces media frame diagnostics.

### V6: 2x1 Dual-GPU Wall

- Log: `target/multigpu_logs/v6_2x1_video_stderr_20260609_114508.log`
- Analyzer gate: passed with `--validate-media-wall --expected-gpu 0 --expected-gpu 1`.
- Analyzer result: GPU 0/1 present activity, 723 raw media frames, 727 wall image fan-outs, 0 metadata mismatches, present spread 1, 0 panics, 0 errors.
- Result: video frame image updates are broadcast to both wall paint targets and both requested GPU paths present frames.

### V7: Visual Verification

- Screenshot: `target/multigpu_logs/v7_2x1_visual_20260609_114927.png`
- Log: `target/multigpu_logs/v7_2x1_visual_stderr_20260609_114927.log`
- Analyzer result: 278 raw media frames, 282 wall image fan-outs, 0 metadata mismatches, present spread 0, 0 panics, 0 errors.
- Visual result: the video element spans the x=1920 tile seam, the page reports `readyState` 4, playback is not paused, and no play error is shown.

The visual capture is a static screenshot. Continuous motion across tiles is inferred from the paired media frame, fan-out, metadata, barrier, and present logs rather than from a recorded video capture.

### Wildlife H.264 MP4 Validation

- Asset: `tests/Wildlife_FHD30fps_counter_10Mbitrate.mp4`
- Initial result with fallback removed: `target/multigpu_logs/wildlife_visual_20260609_134102.png` showed `currentSrc: none`, `readyState: 0`, and no media frame diagnostics. The earlier green screen was the WebM fallback test video, not this Wildlife MP4.
- Cause: `components/media/backends/gstreamer/registry_scanner.rs` did not recognize the available H.264 decoder because the decoder advertises caps with `alignment` and `stream-format` fields.
- Fix: the H.264 decoder probe now accepts `video/x-h264, alignment=au, stream-format=avc` and `video/x-h264, alignment=au, stream-format=byte-stream`.
- Build: `.\mach.bat build --media-stack gstreamer -j 8`
- Passing screenshot: `target/multigpu_logs/wildlife_mp4_h264caps_visual_20260609_143602.png`
- Passing log: `target/multigpu_logs/wildlife_mp4_h264caps_visual_stderr_20260609_143602.log`
- Analyzer result: 298 raw media frames at 1920x1080, 302 wall image fan-outs, 0 metadata mismatches, present spread 0, 0 panics, 0 errors.
- Visual result: the actual Wildlife MP4 frame is visible across the wall video stage; page stats show `canPlay mp4: maybe`, `currentSrc` pointing to the Wildlife MP4 file, `readyState` 4, `size` 1920x1080, and no play error.

## WebRTC/getUserMedia Validation Results

Date: 2026-06-09

### Initial `media_frames=0` Result

- Log: `target/multigpu_logs/v10_2x1_webrtc_video_stderr_20260609_115915.log`
- Analyzer result: 2716 logical frames, metadata mismatch 0, barrier missed 0, present spread 1, but 0 media frames and 0 media image updates.
- Root cause: `components/script/dom/media/mediadevices.rs` resolved `getUserMedia()` with a `MediaStream` even when no video input stream was created. On this machine `ServoMedia::create_videoinput_stream(...)` returned `None`, so the probe attached a stream with no video track and `MediaFrameRenderer::render()` never ran.
- Important diagnostic distinction: the run had a few `Wall media image fanout:` lines, but no `Wall media frame:` lines. Paint currently logs all wall `UpdateImages` fan-out through that diagnostic name, so those early fan-outs were not proof of WebRTC video frames.

### Deterministic Mock-Capture Fixes

- Added `media_capture_mocking_enabled` to `components/config/prefs.rs`.
- Wired `MediaDevices::GetUserMedia()` to call `ServoMedia::set_capture_mocking(pref!(media_capture_mocking_enabled))`.
- Replaced the old GStreamer mock source with a generated BGRA `appsrc` stream in `components/media/backends/gstreamer/media_stream.rs`. The first mock attempt failed because the packaged runtime did not include `videotestsrc`.
- Removed incompatible string enum property settings from the mock WebRTC encode path: `vp8enc error-resilient` and `rtpvp8pay picture-id-mode`.

### Passing 2x1 Dual-GPU Result

- Command:

```powershell
$env:RUST_LOG = 'info'
target\debug\servoshell.exe --pref dom_webrtc_enabled=true --pref media_capture_mocking_enabled=true --wall-layout etc\multigpu\config\wall_layout.example_2x1_dualgpu.json --wall-all-tiles tests/html/multigpu_wall_webrtc_video_probe.html
```

- Log: `target/multigpu_logs/v10e_2x1_webrtc_mock_stderr_20260609_163008.log`
- Analyzer gate: passed with `--validate-media-wall --expected-gpu 0 --expected-gpu 1`.
- Analyzer result: 2347 logical frames, 2347/0 metadata matched/mismatched, 2345/0 barrier completed/missed, 0 panics, 0 errors.
- Media result: 611 raw 640x360 media frames, 1 image add, 610 image updates, 615 wall image fan-outs.
- Tile result: requested GPUs 0 and 1 were both active, with present balance 1196/1196 and spread 0.
- Representative frame evidence: `Wall media frame:` logged `frame_backend=raw size=640x360 image_update=add/update`, and Paint logged fan-out to `target_painters=[PainterId(1), PainterId(2)] requested_gpus=[Some(0), Some(1)]`.

Result: WebRTC/getUserMedia-originated video frames, using the deterministic mock-capture source, reach `<video srcObject>`, become raw WebRender image updates, and fan out to both GPU-backed wall tiles.

This validates tiled render/upload fan-out for the WebRTC MediaStream path. It does not validate distributed decode, a physical camera source, or an incoming remote `RTCPeerConnection` video track.

## Required Runtime Conditions

- Enable WebRTC prefs when testing:

```powershell
target\debug\servoshell.exe --pref dom_webrtc_enabled=true ...
```

- For deterministic no-camera WebRTC/getUserMedia validation, also enable:

```powershell
--pref media_capture_mocking_enabled=true
```

- If transceiver-specific APIs are needed, also enable:

```powershell
--pref dom_webrtc_transceiver_enabled=true
```

- `getUserMedia()` is exposed through `MediaDevices`, which is marked SecureContext in WebIDL. Prefer `https://`, `http://localhost`, or local test paths that Servo treats as secure enough for the scenario under test.

- If a future rebuild does not package GStreamer DLLs, build through the normal `mach` native media path or explicitly select the GStreamer media stack.

## Code Paths Verified

- WebRTC pref defaults:
  - `servo/components/config/prefs.rs`
  - `dom_webrtc_enabled` defaults to `false`.
  - `dom_webrtc_transceiver_enabled` defaults to `false`.

- WebRTC WebIDL:
  - `servo/components/script_bindings/webidls/RTCPeerConnection.webidl`
  - `servo/components/script_bindings/webidls/MediaDevices.webidl`
  - WebRTC APIs are gated by `Pref="dom_webrtc_enabled"`.

- GStreamer WebRTC:
  - `servo/components/media/backends/gstreamer/webrtc.rs`
  - Uses `webrtcbin`.
  - Incoming decoded video streams are converted through `GStreamerMediaStream::create_video_from(...)`.

- MediaStream to video element:
  - `servo/components/script/dom/html/htmlmediaelement.rs`
  - `SrcObject::MediaStream` tracks are passed to the player with `set_stream(...)`.
  - `PlayerEvent::VideoFrameUpdated` triggers video frame update handling.

- GStreamer video frame path on Windows:
  - `servo/components/media/backends/gstreamer/render.rs`
  - The generic path uses an `appsink` with BGRA raw frames and creates `VideoFrameData::Raw`.

- Video to WebRender image:
  - `servo/components/script/dom/html/htmlmediaelement.rs`
  - `MediaFrameRenderer` turns video frames into WebRender image data.
  - Layout reads the current video frame through `HTMLMediaData.current_frame`.

## Multi-GPU Implications

- If a WebRTC stream is attached to a `<video>` element successfully, the tiled renderer should see it as normal replaced content with a WebRender image key.
- Tile clipping, overlap, and per-window painting should apply at the WebRender/painter level.
- Decode is not per-GPU. There is no v1 workload split where each GPU decodes a different part of the WebRTC/video stream.
- Direct cross-GPU video texture sharing is not implemented and remains outside the v1 design.
- Windows currently looks safer than Unix GL-texture paths for v1 because the generic GStreamer path uses raw BGRA frames instead of passing a GL texture ID.
- The 2x1 dual-GPU video-file probe confirms per-GPU present activity for GPU 0 and GPU 1 on the current validation machine.

## Remaining Limits

- The current `Wall media image fanout:` paint-side log counts wall `UpdateImages` fan-out events. During the video-file probe, those events correlate with media image updates after playback starts, but the Paint layer does not yet tag an image update as media-only.
- The current validated path is still single decode plus raw-frame image fan-out. Decode is not distributed across GPUs.
- Static screenshot evidence does not prove sub-frame temporal sync. Use continuous media frame logs, image fan-out logs, metadata match counts, barrier completion, present balance, and optionally a screen recording for stronger temporal validation.
- WebRTC/getUserMedia video has been validated in wall mode with deterministic mock capture. A physical camera source and an incoming remote `RTCPeerConnection` video track remain unvalidated.
- `tests/Wildlife_FHD30fps_counter_10Mbitrate.mp4` is a local validation asset in the current workspace. If this probe must be portable without that file, add a committed H.264 sample or restore an explicit fallback while keeping `currentSrc` visible.

## Important Limitation

`HTMLCanvasElement.captureStream()` exists in WebIDL, but the current Servo implementation is a stub-like path that creates a `MediaStreamTrack` with a new `MediaStreamId` and does not register a real GStreamer video stream.

Do not use `canvas.captureStream()` as the primary Servo WebRTC validation source. It may work in Chromium/browser sanity checks, but it is not a reliable Servo WebRTC video input.

## Regression Commands

Run the 2x1 dual-GPU WebRTC/getUserMedia wall smoke with deterministic mock capture:

```powershell
$env:RUST_LOG = 'info'
target\debug\servoshell.exe --pref dom_webrtc_enabled=true --pref media_capture_mocking_enabled=true --wall-layout etc\multigpu\config\wall_layout.example_2x1_dualgpu.json --wall-all-tiles tests/html/multigpu_wall_webrtc_video_probe.html
```

Analyze the captured log:

```powershell
python .\etc\multigpu\tools\wall_perf_analyzer\analyze_wall_perf.py <log-file> --format markdown --validate-media-wall --expected-gpu 0 --expected-gpu 1
```

Collect:

- whether `getUserMedia()` resolves with a video track,
- whether the stream reaches the `<video>` element,
- `Wall media frame:` logs,
- `Wall media image fanout:` logs,
- `Wall frame metadata` match/mismatch counts,
- barrier complete/missed logs,
- tile present balance,
- any GStreamer/WebRTC errors.

Keep the video-file probe available as the regression fallback:

```powershell
target\debug\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.example_2x1_dualgpu.json --wall-all-tiles tests/html/multigpu_wall_video_file_probe.html
```

## Development Follow-Up

- Add a stricter `getUserMedia()` failure path or page-side assertion so a 0-video-track stream cannot be mistaken for a valid capture result.
- Validate a physical camera source when one is available on the wall machine.
- Validate an incoming remote `RTCPeerConnection` video track with explicit signaling once the harness is defined.
- Document whether video is raw-frame backed or texture-backed on each target OS before claiming multi-GPU video support.
