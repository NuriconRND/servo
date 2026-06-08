# Servo WebRTC And Video Multi-GPU Notes

Date: 2026-06-07

## Conclusion

Servo can process WebRTC/video content conditionally. The codebase has WebRTC DOM bindings, a GStreamer WebRTC backend, MediaStream-to-HTMLMediaElement wiring, and video frames flowing into WebRender images.

The tiled multi-GPU prototype can render this content after it becomes normal page content, but the media pipeline itself is not distributed across GPUs. In v1, WebRTC decode and media processing should be treated as one Servo/GStreamer pipeline feeding tiled WebRender renderers.

## Current Build State

The current `servo/target/debug` output contains GStreamer/WebRTC runtime DLLs, including:

- `gstreamer-1.0-0.dll`
- `gstwebrtc.dll`
- `gstwebrtc-1.0-0.dll`
- `gstwebrtcnice-1.0-0.dll`
- `gstapp.dll`
- `gstapp-1.0-0.dll`
- `gstlibav.dll`

This indicates the current debug build output is packaged with the GStreamer pieces needed for WebRTC/video experiments.

## Required Runtime Conditions

- Enable WebRTC prefs when testing:

```powershell
target\debug\servoshell.exe --pref dom_webrtc_enabled=true ...
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
- Actual per-GPU present still needs validation on target multi-GPU hardware.

## Important Limitation

`HTMLCanvasElement.captureStream()` exists in WebIDL, but the current Servo implementation is a stub-like path that creates a `MediaStreamTrack` with a new `MediaStreamId` and does not register a real GStreamer video stream.

Do not use `canvas.captureStream()` as the primary Servo WebRTC validation source. It may work in Chromium/browser sanity checks, but it is not a reliable Servo WebRTC video input.

## Recommended Next Test

Create a dedicated WebRTC/video probe page that does one of the following:

- Uses `navigator.mediaDevices.getUserMedia({ video: true })`, attaches the stream to `<video srcObject>`, and places the video across tile boundaries.
- Or connects to an external WebRTC sender and attaches the remote track from `RTCPeerConnection.ontrack` to `<video srcObject>`.

Run it with:

```powershell
target\debug\servoshell.exe --pref dom_webrtc_enabled=true --wall-layout etc\multigpu\config\wall_layout.example_3x1.json --wall-all-tiles <probe-page>
```

Collect:

- visual tile-boundary behavior for video content,
- `Wall frame metadata` scroll match logs,
- barrier complete/missed logs,
- tile present balance,
- any GStreamer/WebRTC errors.

## Development Follow-Up

- Add `tests/html/multigpu_wall_webrtc_video_probe.html`.
- Prefer real camera or external sender flow over `canvas.captureStream()`.
- Add a short analyzer/report section for video/WebRTC runs if the probe becomes stable.
- Document whether video is raw-frame backed or texture-backed on each target OS before claiming multi-GPU video support.
