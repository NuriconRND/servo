# Multi-GPU YUV Zero-Copy Video Plan

Date: 2026-06-10

## Goal

Reduce 4K video stutter in the Windows 2x1 multi-GPU wall path by removing the current CPU BGRA conversion and the Servo-owned per-frame `Vec<u8>` copy.

The target path is:

1. Decode H.264 into YUV420 with GStreamer.
2. Keep the decoded YUV frame as a GStreamer-owned buffer.
3. Expose YUV planes to WebRender through media external images.
4. Upload/sample YUV planes in WebRender.
5. Convert YUV to RGBA in a GPU shader during composition.

This replaces the current path:

```text
avdec_h264 -> GStreamer BGRA conversion -> appsink(BGRA)
    -> GStreamerBuffer::to_vec()
    -> VideoFrameData::Raw(Arc<Vec<u8>>)
    -> WebRender BGRA8 raw image update
    -> wall image fan-out
```

## Non-Negotiable Constraint

The YUV path must not copy decoded frame data into a Servo-owned `Vec<u8>`.

For YUV frames:

- Do not call `data.to_vec()`.
- Do not create `VideoFrameData::Raw(Arc<Vec<u8>>)` for the media frame.
- Do not convert YUV to BGRA on the CPU.
- Keep the decoded frame alive through GStreamer buffer ownership or an equivalent external-image lifetime guard.

If this cannot be satisfied for a given memory type or layout, the implementation must fall back to the existing BGRA path and log the fallback reason.

## Scope

### In Scope

- Windows 2x1 dual-GPU wall validation.
- H.264 MP4 playback through the current GStreamer media stack.
- 8-bit 4:2:0 YUV only.
- I420 as the primary target format.
- NV12 as a supported secondary format if GStreamer negotiates it cleanly.
- WebRender GPU shader conversion from YUV to RGBA.
- Multi-GPU wall fan-out of YUV plane external image references.

### Out of Scope

- GPU decode distribution across GPUs.
- 10-bit/P010/HDR.
- Remote WebRTC YUV zero-copy.
- D3D11 hardware decoder zero-copy as the first milestone.

True decoder-to-renderer GPU-resource zero-copy is tracked as a follow-up stage because it requires GStreamer D3D11 decoder output and a D3D11/DXGI-to-WebRender external texture bridge.

## Current Facts

- Current Windows/default media path requests `video/x-raw, format=BGRA` in `components/media/backends/gstreamer/render.rs`.
- `GStreamerBuffer::to_vec()` currently copies the mapped frame plane with `data.to_vec()`.
- `MediaFrameRenderer` currently turns raw media frames into `ImageFormat::BGRA8` WebRender image updates.
- `avdec_h264` advertises many output formats. `NV12` is supported, but it is not safe to assume it is the default. Current caps evidence shows `I420` first in the advertised raw format list.
- Existing 4K validation shows successful multi-GPU fan-out but visible stutter risk:
  - `4k_Sample2.mp4`: raw `3840x2160`, media fan-out present, present balance stable, but request-to-ready P95 exceeds the 60Hz frame budget.
  - `4k_3DMark.mp4`: same fan-out behavior with elevated request-to-ready latency.

## Progress Tracker

Update this table whenever a task is completed, blocked, or re-scoped.

| ID | Status | Task | Completion Evidence |
| --- | --- | --- | --- |
| Y0 | Done | Capture the problem statement and zero-copy constraint in this plan. | `docs/multigpu/multigpu_yuv_zero_copy_video_plan.md` |
| Y1 | Pending | Collect clean BGRA baseline metrics with low logging overhead. | Analyzer output for Wildlife, `4k_Sample2.mp4`, and `4k_3DMark.mp4` with `RUST_LOG` reduced or sampled |
| Y2 | Done | Add YUV frame metadata and external-plane frame representation. | `VideoFrameData::Yuv` carries I420/NV12 plane metadata; YUV does not use `Arc<Vec<u8>>` |
| Y3 | Done | Add GStreamer I420/NV12 appsink negotiation. | Windows/default appsink requests I420, NV12, then BGRA in single-process mode; BGRA remains the multiprocess/force-ipc fallback |
| Y4 | Done | Implement GStreamer-backed external plane lifetime management. | `RawVideoFrameExternalImages` stores current plane owners by external ID; WebRender lock clones the owner into a per-handler locked-plane map until unlock |
| Y5 | Done | Add media external image handler support for YUV plane raw slices. | Media external handler returns `ExternalImageSource::RawData` for registered YUV planes |
| Y6 | Done | Connect YUV plane images to WebRender GPU YUV shader path. | Layout emits `push_yuv_image()` with `YuvData::PlanarYCbCr` or `YuvData::NV12` |
| Y7 | Done | Fan out YUV plane external image references across wall painters. | `4k_sample2_yuv_2x1_stderr_20260610_165457.log`: media fan-outs 918, `updates_total=2746`, requested GPUs 0 and 1 |
| Y8 | In Progress | Extend diagnostics and analyzer for YUV zero-copy. | Logs include `frame_backend=yuv_i420_external_raw`; analyzer counts YUV frame backends and split WASAPI nonfatal warnings. Lock/unlock counters and fallback counters are still pending |
| Y9 | Done | Validate 4K files on Windows 2x1 wall. | `4k_Sample2.mp4` and `4k_3DMark.mp4` both passed YUV media wall smoke validation with GPU 0/1 fan-out |
| Y10 | Pending | Validate fallback and regression media. | Wildlife MP4 and WebRTC mock capture still pass through BGRA or supported fallback path |
| Y11 | Pending | Document final results and remaining limits. | Update this plan and `multigpu_webrtc_video_notes.md` with final metrics and constraints |
| Y12 | Deferred | Investigate true GPU-resource zero-copy with GStreamer D3D11 memory. | Separate feasibility document or implementation plan |

Status values:

- `Pending`: not started.
- `In Progress`: implementation or validation is underway.
- `Done`: completed and evidence is recorded.
- `Blocked`: cannot proceed without a named dependency or decision.
- `Deferred`: intentionally moved out of the current milestone.

## Implementation Plan

### Stage 1: Baseline Without Diagnostic Distortion

- Re-run the 4K probes with frame-level diagnostics disabled or sampled.
- Keep only enough logging to compute media frame count, fan-out count, present balance, barrier latency, and panic/error count.
- Use this as the performance baseline, because `RUST_LOG=info` frame-by-frame logging can itself cause stutter.

### Stage 2: YUV Frame Model

- Replace the YUV path's dependency on `Buffer::to_vec()` with an API that can return a frame object backed by the original media buffer.
- Keep existing `Raw(Arc<Vec<u8>>)` for BGRA fallback.
- Add a YUV frame variant with:
  - format: `I420` or `NV12`,
  - frame size,
  - per-plane size,
  - per-plane stride,
  - color matrix,
  - range,
  - lifetime owner for the original GStreamer buffer or mapped memory.

The YUV variant must support lock/unlock style access without copying into Servo-owned memory.

### Stage 3: GStreamer Negotiation

- Request I420 first:

```text
video/x-raw, format=I420
```

- If I420 cannot be negotiated, request NV12:

```text
video/x-raw, format=NV12
```

- If both fail, fall back to existing BGRA:

```text
video/x-raw, format=BGRA
```

- Log the negotiated format and fallback reason.
- Do not assume `avdec_h264` defaults to NV12.

### Stage 4: WebRender External Image Path

- Do not send YUV frames as `SerializableImageData::Raw(GenericSharedMemory)`.
- Register YUV planes as media external images.
- The media external image handler should expose plane data on WebRender lock and release it on unlock.
- For I420:
  - Y plane: full resolution.
  - U plane: half width, half height.
  - V plane: half width, half height.
- For NV12:
  - Y plane: full resolution.
  - UV plane: half width, half height with interleaved UV samples.

If WebRender API 0.68 does not expose the required YUV primitive through Servo, add the missing WebRender API connection before attempting a custom workaround.

### Stage 5: Wall Fan-Out

- Fan out plane external image references, not BGRA image buffers.
- Ensure each tile WebRender instance can lock/read the same frame planes safely.
- Hold frame lifetime until all target painters have completed lock/unlock for the frame.
- If safe shared access cannot be guaranteed, block YUV zero-copy for that memory type and use BGRA fallback.

### Stage 6: Diagnostics

Add diagnostics that can answer these questions without heavy per-frame logging by default:

- Which backend rendered the frame: `raw_bgra`, `yuv_i420_external_raw`, `yuv_nv12_external_raw`.
- Was any Servo-owned copy performed for the frame.
- Which format was negotiated by GStreamer.
- How many plane image locks/unlocks occurred.
- Whether the frame fell back to BGRA and why.
- Whether wall fan-out reached GPU 0 and GPU 1.

## Acceptance Criteria

The milestone is complete only when all of the following are true:

- 4K H.264 MP4 playback negotiates I420 or NV12.
- YUV frames do not call `data.to_vec()` or create `Arc<Vec<u8>>` frame payloads.
- CPU YUV-to-BGRA conversion is not used on the YUV path.
- WebRender performs YUV-to-RGBA conversion on the GPU.
- Both wall tiles continue to present on requested GPUs 0 and 1.
- Metadata mismatch remains 0.
- Panic/error diagnostics remain 0.
- `Barrier request-to-all-ready` P95 improves over the BGRA baseline for 4K samples.
- Visible stutter is reduced.
- BGRA fallback remains functional.

## Validation Commands

Use the existing 2x1 wall layout:

```powershell
target\debug\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.example_2x1_dualgpu.json --wall-all-tiles tests/html/multigpu_wall_video_4k_sample2_probe.html
```

```powershell
target\debug\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.example_2x1_dualgpu.json --wall-all-tiles tests/html/multigpu_wall_video_4k_3dmark_probe.html
```

Analyze the captured log:

```powershell
python .\etc\multigpu\tools\wall_perf_analyzer\analyze_wall_perf.py <log-file> --format markdown --validate-media-wall --expected-gpu 0 --expected-gpu 1
```

## Follow-Up: True GPU-Resource Zero-Copy

The first milestone removes Servo-owned CPU copies but may still use CPU-decoded YUV memory that WebRender uploads to GPU textures.

True GPU-resource zero-copy requires a different path:

```text
hardware decode -> D3D11/DXGI texture -> WebRender external texture -> GPU YUV shader
```

The current GStreamer dependency package contains `gstd3d11.dll`, so this is worth a separate feasibility spike. That work must validate:

- GStreamer D3D11 decoder availability.
- Decoder output as D3D11 texture memory.
- ANGLE/WebRender ability to sample the shared texture.
- Multi-GPU adapter compatibility.
- Cross-adapter texture sharing or per-tile decode/upload policy.

## Progress Log

### 2026-06-10

- Created this progress-tracked plan.
- Recorded the user requirement that the YUV path must avoid Servo-owned CPU frame copies.
- Confirmed that the current BGRA path uses `data.to_vec()` and `VideoFrameData::Raw(Arc<Vec<u8>>)` for raw frames.
- Confirmed that the installed GStreamer dependency tree includes `gstd3d11.dll`, making true GPU-resource zero-copy a plausible follow-up investigation.
- Kept D3D11 hardware decode out of the first milestone to avoid mixing two large changes: no Servo-owned copy for CPU-decoded YUV and true decoder texture sharing.
- Implemented `VideoFrameData::Yuv` with I420/NV12 plane metadata and borrowed plane access through the existing frame buffer owner.
- Changed the Windows/default GStreamer appsink negotiation to prefer I420, then NV12, then BGRA when running in single-process mode.
- Added a single-process media external image registry that lets WebRender lock YUV planes as `ExternalImageSource::RawData` without copying them into Servo-owned `Vec<u8>`.
- Extended layout media frames and image fragments so video can emit WebRender `push_yuv_image()` display items.
- Updated the wall performance analyzer to count `yuv_i420_external_raw` and `yuv_nv12_external_raw` media frames.
- Verified compile/link after cleanup with Visual Studio environment, `CC=cl.exe`, `CXX=cl.exe`, and `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER=link.exe`: Cargo reached `Finished dev profile`. `mach` then returned exit code 1 in the Windows post-build DLL-copy step because Python stdout used CP949 and could not encode a Unicode bullet.
- Verified `4k_Sample2.mp4` on the Windows 2x1 wall using `target\multigpu_logs\4k_sample2_yuv_2x1_stderr_20260610_165457.log`:
  - `frame_backend=yuv_i420_external_raw`, size `3840x2160`.
  - Media frames yuv_i420/yuv_nv12: `914 / 0`.
  - Media fan-outs: `918`, `updates_total=2746`.
  - Present balance: tile 0 `1723`, tile 1 `1723`, spread `0`.
  - Metadata matched/mismatched: `3471 / 0`.
  - Barrier completed/missed: `3469 / 0`.
  - Panic/error diagnostics: `0 / 0`; nonfatal media warnings: `3`.
  - Barrier request-to-all-ready P95: `21.843ms` in info-level logging.
- Verified `4k_3DMark.mp4` on the Windows 2x1 wall using `target\multigpu_logs\4k_3dmark_yuv_2x1_final_stderr_20260610_174533.log`:
  - `frame_backend=yuv_i420_external_raw`, size `3840x2160`.
  - Media frames yuv_i420/yuv_nv12: `1034 / 0`.
  - Media fan-outs: `1038`, `updates_total=3106`.
  - Present balance: tile 0 `919`, tile 1 `919`, spread `0`.
  - Metadata matched/mismatched: `1822 / 0`.
  - Barrier completed/missed: `1820 / 1`.
  - Panic/error diagnostics: `0 / 0`; nonfatal media warnings: `1`.
  - Barrier request-to-all-ready P95: `26.180ms` in info-level logging.
- Added a no-loop one-shot probe for full-duration release validation: `tests/html/multigpu_wall_video_4k_3dmark_once_probe.html`.
- Verified release build: `.\mach.bat build --release --media-stack gstreamer -j 8` succeeded with `PYTHONIOENCODING=utf-8`, `CC=cl.exe`, `CXX=cl.exe`, and `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER=link.exe`.
- Ran `4k_3DMark.mp4` through one full release playback duration using `target\release\servoshell.exe` and `target\multigpu_logs\4k_3dmark_yuv_2x1_release_once_stderr_20260610_180606.log`:
  - Source duration from GStreamer discoverer: `0:03:37.586000000`, 3840x2160 H.264 High Profile, 60/1 fps.
  - Runtime window: `245s`, with the no-loop probe stopped after the source duration plus margin.
  - `frame_backend=yuv_i420_external_raw`, size `3840x2160`.
  - Media frames yuv_i420/yuv_nv12: `13317 / 0`.
  - Media fan-outs: `13322`, `updates_total=39956`.
  - Present balance: tile 0 `12691`, tile 1 `12691`, spread `0`.
  - Metadata matched/mismatched: `25534 / 0`.
  - Barrier completed/missed: `25532 / 25`.
  - Skipped repaint targets: `7`.
  - Panic/error diagnostics: `0 / 0`; nonfatal media warnings: `17`.
  - Barrier request-to-all-ready P95: `20.541ms` in info-level logging.
  - Painter render P95: `3.125ms`; repaint render P95: `4.192ms`; window present P95: `10.925ms`.
