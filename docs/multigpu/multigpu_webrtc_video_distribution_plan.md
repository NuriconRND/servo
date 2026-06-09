# WebRTC And Video Multi-GPU Distribution Plan

Date: 2026-06-09

## Goal

Move WebRTC/video work from the current "single media pipeline feeding tiled renderers" state toward measurable multi-GPU behavior.

The work is intentionally staged:

1. Prove video-file render/upload fan-out first.
2. Prove the same path with WebRTC video after the video-file path is stable.
3. Investigate whether decode itself can be split per GPU without breaking Servo's media architecture.

The first validation source is a normal video file, not `canvas.captureStream()` and not an external WebRTC sender.

## Current Architecture Baseline

- `HTMLMediaElement` creates one media player for a `<video>` element.
- The player sends decoded `VideoFrame`s to `MediaFrameRenderer`.
- `MediaFrameRenderer` converts frames into WebRender image updates.
- In wall mode, `PaintMessage::UpdateImages` is already broadcast from the source painter to every painter target registered for the logical `WebView`.
- The current Windows/default path is expected to use raw BGRA frames (`VideoFrameData::Raw`), which is safer for multi-GPU fan-out than GL texture-backed video.

This means the immediate multi-GPU opportunity is not decode distribution. The immediate opportunity is to prove that video frame image updates are fanned out and rendered by every GPU-backed tile target.

## Progress Tracker

Update this table whenever a task is completed, blocked, or re-scoped.

| ID | Status | Task | Completion Evidence |
| --- | --- | --- | --- |
| V0 | Done | Create this tracking plan and link it from the existing WebRTC/video notes. | `docs/multigpu/multigpu_webrtc_video_distribution_plan.md` |
| V1 | Done | Add `tests/html/multigpu_wall_video_file_probe.html` using an existing WPT video asset. | Added `tests/html/multigpu_wall_video_file_probe.html`; runtime load remains V5 |
| V2 | Done | Add media frame diagnostics in `MediaFrameRenderer::render()`. | `components/script/dom/html/htmlmediaelement.rs` logs `Wall media frame:` with frame id, backend, size, image key, and update type |
| V3 | Done | Add Paint-side media image fan-out diagnostics for wall mode. | `components/paint/paint.rs` logs `Wall media image fanout:` with source painter, targets, requested GPUs, and update counts |
| V4 | Done | Extend `wall_perf_analyzer` with media counters and a validation gate. | `etc/multigpu/tools/wall_perf_analyzer/analyze_wall_perf.py` reports media counters and supports `--validate-media-wall` |
| V5 | Done | Run 1x1 video-file baseline smoke. | `target/multigpu_logs/v5_1x1_video_stderr_20260609_114358.log`: 671 raw media frames, panic/error 0 |
| V6 | Done | Run 2x1 dual-GPU video-file wall smoke. | `target/multigpu_logs/v6_2x1_video_stderr_20260609_114508.log`: GPU 0/1 active, 723 raw media frames, 727 image fan-outs, metadata mismatch 0, present spread 1 |
| V7 | Done | Perform visual verification on 2x1 dual-GPU wall. | `target/multigpu_logs/v7_2x1_visual_20260609_114927.png` plus analyzer gate: video spans x=1920 seam, readyState 4, no play error, present spread 0 |
| V8 | Done | Update `multigpu_webrtc_video_notes.md` with results and limits. | Notes include V5-V7 logs, visual result, and remaining risks |
| V8a | Done | Validate the user-provided H.264 MP4 asset `tests/Wildlife_FHD30fps_counter_10Mbitrate.mp4`. | `target/multigpu_logs/wildlife_mp4_h264caps_visual_20260609_143602.png`: `canPlay mp4: maybe`, `currentSrc` points to Wildlife MP4, 1920x1080 raw frames, 302 fan-outs, present spread 0 |
| V9 | Done | Add `tests/html/multigpu_wall_webrtc_video_probe.html`. | Probe attaches `getUserMedia()` to `<video srcObject>` and spans the x=1920 tile seam |
| V10 | Done | Run 2x1 dual-GPU WebRTC/getUserMedia wall smoke. | `target/multigpu_logs/v10e_2x1_webrtc_mock_stderr_20260609_163008.log`: 611 raw media frames, 615 image fan-outs, metadata mismatch 0, present balance 1196/1196, panic/error 0 |
| V11 | Done | Update notes with WebRTC probe result and limits. | Notes include the baseline `media_frames=0` root cause, deterministic mock-capture pass, and remaining physical-camera/remote-WebRTC limits |
| V12 | Pending | Start decode distribution feasibility spike. | New or updated section comparing single decode, GStreamer tee, per-GPU decode, and WebRTC simulcast/SVC |

Status values:

- `Pending`: not started.
- `In Progress`: implementation or validation is underway.
- `Done`: completed and evidence is recorded.
- `Blocked`: cannot proceed without a named dependency or decision.
- `Deferred`: intentionally moved out of the current milestone.

## Stage 1: Video File Render/Upload Fan-Out

### Intent

Prove that a decoded video frame reaches every wall paint target and is rendered/presented by the GPU assigned to that tile.

### Implementation Plan

- Add a dedicated video-file probe page at `tests/html/multigpu_wall_video_file_probe.html`.
- Reuse an existing repository video asset, preferably `tests/wpt/webgl/tests/resources/npot-video-1920x1080.mp4`, to avoid adding binary media.
- Place the `<video>` element across tile boundaries for the current 2x1, local 3x1, and target 2x2 layouts.
- Use `autoplay`, `muted`, `loop`, and `playsinline`.
- Show on-page diagnostics for `currentTime`, `readyState`, `videoWidth`, `videoHeight`, and animation frame count.
- Do not use `canvas.captureStream()`.

### Diagnostics Plan

- In `MediaFrameRenderer::render()`, log each video frame update in wall/video diagnostic mode.
- Record whether the frame is raw, GL texture, or external OES.
- Record whether the WebRender image update is an add, update, or delete.
- In Paint, log when media image updates are cloned to multiple painter targets.

### Done Criteria

- 1x1 run confirms the video plays and produces media frame logs.
- 2x1 dual-GPU run confirms both requested GPU paths are active.
- Analyzer confirms media frames and media image fan-out occurred.
- Visual check confirms the video moves continuously across the tile boundary without one tile freezing or showing stale content.

## Stage 2: WebRTC Video Probe

### Intent

After normal video file fan-out is stable, validate the same tiled rendering path with WebRTC-originated video.

### Implementation Plan

- Add `tests/html/multigpu_wall_webrtc_video_probe.html`.
- Prefer `navigator.mediaDevices.getUserMedia({ video: true })` as the first source.
- Keep external WebRTC sender support as a later option because it needs signaling and test harness decisions.
- Attach the resulting stream to `<video srcObject>`.
- Place the video across the same tile boundaries used by the video-file probe.

### Done Criteria

- Run with `--pref dom_webrtc_enabled=true`.
- The video track reaches the `<video>` element and produces media frame logs.
- Tile present counts remain balanced.
- Metadata mismatch remains 0.
- Visual result is recorded.

## Stage 3: Decode Distribution Feasibility

### Intent

Determine whether decode itself can be distributed across GPUs, and decide whether it belongs in the next milestone.

### Candidate Designs

- Single decode plus raw frame fan-out:
  - Current safest path.
  - CPU decode and frame copy cost remain centralized.
  - No cross-GPU texture sharing requirement.

- GStreamer `tee` with per-target upload branches:
  - One decode path, multiple upload/render branches.
  - May reduce per-target conversion cost if GPU upload can be isolated.
  - Needs GStreamer pipeline changes and target-specific context handling.

- Independent per-GPU decode pipelines:
  - Each GPU/tile has its own media player or decode path.
  - Avoids shared frame upload but risks desynchronized playback, higher network/demux load, and more complicated error recovery.

- WebRTC simulcast/SVC per tile:
  - Remote sender provides multiple streams/layers.
  - Tile/GPU can choose a stream or layer.
  - Requires WebRTC signaling and receiver policy changes.

### Done Criteria

- Document which candidate is viable on Windows 2x1 dual-GPU hardware.
- Document required Servo/GStreamer API changes.
- Document synchronization and failure risks.
- Choose one next implementation candidate or explicitly defer decode distribution.

## Validation Commands

Run from `servo` after loading the existing environment helper if needed.

```powershell
target\debug\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.example_1x1.json tests/html/multigpu_wall_video_file_probe.html
```

```powershell
target\debug\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.example_2x1_dualgpu.json --wall-all-tiles tests/html/multigpu_wall_video_file_probe.html
```

For deterministic WebRTC/getUserMedia validation on a machine without a usable camera source:

```powershell
target\debug\servoshell.exe --pref dom_webrtc_enabled=true --pref media_capture_mocking_enabled=true --wall-layout etc\multigpu\config\wall_layout.example_2x1_dualgpu.json --wall-all-tiles tests/html/multigpu_wall_webrtc_video_probe.html
```

```powershell
python .\etc\multigpu\tools\wall_perf_analyzer\analyze_wall_perf.py <log-file> --format markdown
```

## Progress Update Rules

When a task is completed:

1. Change its `Progress Tracker` status.
2. Add the log file, command, or manual evidence in `Completion Evidence`.
3. Add a dated entry to `Progress Log`.
4. Update `multigpu_webrtc_video_notes.md` if the result changes the current media conclusion.

When a task is blocked:

1. Mark it `Blocked`.
2. Record the exact blocker and the next decision or external dependency.
3. Do not mark a later task `Done` if it depends on the blocked task.

## Progress Log

### 2026-06-09

- Created the staged WebRTC/video multi-GPU distribution plan.
- Set the first implementation target to normal video-file render/upload fan-out.
- Kept decode distribution as a follow-up feasibility spike instead of mixing it with first video validation.
- Added the video-file wall probe page using the existing WPT `npot-video-1920x1080.mp4` asset.
- Added `Wall media frame:` diagnostics in `MediaFrameRenderer::render()`.
- Added `Wall media image fanout:` diagnostics in Paint when image updates are cloned to multiple wall targets.
- Extended `wall_perf_analyzer` with media frame/fan-out counters and the `--validate-media-wall` smoke gate.
- Static checks passed: `python -m py_compile`, analyzer `--help`, analyzer media regex sample, and `git diff --check`.
- `cargo check -p servo-script -p servo-paint` did not complete in the current shell because `lld-link.exe` is not available on `PATH`.
- Initial V5 run with the existing `servoshell.exe` produced wall render/present logs but 0 media frames because the binary was not built with `media-gstreamer`.
- Rebuilt with `media-gstreamer` using `.\mach.bat build --media-stack gstreamer -j 8` after adding `C:\Program Files\LLVM\bin` to `PATH` for `lld-link.exe` and setting `PYTHONIOENCODING=utf-8` for post-build output.
- V5 1x1 video-file smoke passed: `target/multigpu_logs/v5_1x1_video_stderr_20260609_114358.log` reported 671 raw media frames, 1 add, 670 updates, panic/error 0.
- V6 2x1 dual-GPU smoke passed: `target/multigpu_logs/v6_2x1_video_stderr_20260609_114508.log` reported GPU 0/1 present activity, 723 raw media frames, 727 image fan-outs, metadata mismatch 0, and present spread 1.
- V7 visual verification passed for the static capture: `target/multigpu_logs/v7_2x1_visual_20260609_114927.png` shows the video element crossing the x=1920 seam with readyState 4 and no play error. The paired log reported present spread 0 and media fan-out. Temporal motion sync is inferred from continuous media frame/fan-out logs, not from a video recording.
- Updated `multigpu_webrtc_video_notes.md` with the V5-V7 results and remaining limits.
- Expanded the progress tracker so Stage 2 WebRTC probe work is tracked before the decode distribution feasibility spike.
- User-provided `tests/Wildlife_FHD30fps_counter_10Mbitrate.mp4` initially showed no Wildlife video with fallback removed: `target/multigpu_logs/wildlife_visual_20260609_134102.png` reported `currentSrc: none`, `readyState: 0`, and no media frame diagnostics. The earlier green screen was the WebM fallback test video, not the Wildlife MP4.
- Fixed H.264 MP4 source selection by updating `components/media/backends/gstreamer/registry_scanner.rs` to recognize decoder caps with `alignment` and `stream-format` fields.
- Rebuilt with `.\mach.bat build --media-stack gstreamer -j 8`.
- Wildlife MP4 2x1 dual-GPU validation passed after the caps fix: `target/multigpu_logs/wildlife_mp4_h264caps_visual_20260609_143602.log` reported 298 raw 1920x1080 media frames, 302 image fan-outs, metadata mismatch 0, present spread 0, panic/error 0. Screenshot: `target/multigpu_logs/wildlife_mp4_h264caps_visual_20260609_143602.png`.
- Added the WebRTC/getUserMedia wall probe page at `tests/html/multigpu_wall_webrtc_video_probe.html`.
- Initial WebRTC/getUserMedia wall run `target/multigpu_logs/v10_2x1_webrtc_video_stderr_20260609_115915.log` produced 0 media frames. Source inspection showed `MediaDevices::GetUserMedia()` resolves a `MediaStream` even when `create_videoinput_stream()` returns `None`; on this machine no host video capture stream was created, so the page attached a stream with no video track. The run's early Paint fan-out lines were not video evidence because no `Wall media frame:` diagnostics were present.
- Added a deterministic runtime path by wiring the new `media_capture_mocking_enabled` pref to `ServoMedia::set_capture_mocking()`.
- First mock-capture attempt `target/multigpu_logs/v10b_2x1_webrtc_mock_stderr_20260609_152449.log` proved the pref was active but failed because the packaged GStreamer runtime did not include `videotestsrc`.
- Replaced the mock video source with a generated BGRA `appsrc` stream so no optional GStreamer test-source plugin is required.
- Removed incompatible string enum property settings from the mock WebRTC encode path: `vp8enc error-resilient` and `rtpvp8pay picture-id-mode`. Both caused runtime panics on this GStreamer build and are not required for the wall fan-out validation.
- WebRTC/getUserMedia mock-capture 2x1 dual-GPU validation passed: `target/multigpu_logs/v10e_2x1_webrtc_mock_stderr_20260609_163008.log` reported 611 raw 640x360 media frames, 1 image add, 610 image updates, 615 image fan-outs to painter targets with requested GPUs `[Some(0), Some(1)]`, metadata mismatch 0, barrier missed 0, present balance 1196/1196, panic/error 0.
- Updated `multigpu_webrtc_video_notes.md` with the WebRTC probe root cause, passing evidence, and remaining limits.

## Current Assumptions

- Initial target platform is Windows.
- Initial target wall is the existing 2x1 dual-GPU layout.
- Initial video path should use raw frames unless diagnostics show the runtime is using GL texture-backed video.
- Cross-GPU texture sharing remains out of scope for this stage.
- Hardware genlock remains out of scope.
