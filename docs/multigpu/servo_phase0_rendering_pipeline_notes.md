# Servo Phase 0 Rendering Pipeline Notes

## Baseline

- Servo source is cloned under `servo`.
- Local Rust/Cargo homes are isolated under ignored Servo paths:
  - `servo/.rustup`
  - `servo/.servo/cargo-home`
  - `servo/.servo/uv-cache`
- Reusable environment setup is available at `etc/multigpu/servo_env.ps1`.
- `servoshell` debug build succeeds on Windows after installing Servo's packaged GStreamer dependency.

## Verified Commands

Run from the workspace root:

```powershell
. .\etc\multigpu\servo_env.ps1
Set-Location .\servo
.\mach build -j 8
.\target\debug\servoshell.exe -f tests/html/close-on-load.html
```

Observed result:

- `.\mach build -j 8` completes post-build DLL copy.
- Direct `servoshell.exe -f tests/html/close-on-load.html` exits with status 0.
- `.\mach smoketest` currently reports `Servo exited with non-zero status 1`, while the equivalent direct binary invocation succeeds. Treat direct invocation as the Phase 0 runtime baseline until this wrapper difference is debugged.
- Interactive mouse scroll and window resize were not visually verified in this shell-only session. Recheck manually before implementing Phase 2 input-coordinate conversion.

## Entry Points

- `servo/ports/servoshell/desktop/cli.rs`
  - Parses command line arguments.
  - Chooses headed vs headless event loop.
  - Creates `App` and runs the platform event loop.

- `servo/ports/servoshell/desktop/app.rs`
  - Builds the embedder-facing `Servo` instance through `ServoBuilder`.
  - Creates the initial platform window.
  - Opens the first top-level webview.
  - Pumps Servo's browser event loop from the winit application loop.

- `servo/ports/servoshell/desktop/headed_window.rs`
  - Owns the winit window.
  - Creates `WindowRenderingContext` for direct window presentation.
  - Creates an `OffscreenRenderingContext` used for Servo page rendering before blitting into the window scene.
  - Converts winit input, resize, mouse, wheel, touch, and keyboard events into Servo embedder input events.

- `servo/components/servo/servo.rs`
  - Defines the `Servo` handle and `ServoBuilder`.
  - Initializes preferences, JS, media, constellation, paint, embedder channels, and browser orchestration.
  - This is the main coordination boundary between embedder windows and browser internals.

- `servo/components/layout/display_list/mod.rs`
  - Builds WebRender display lists from layout fragment trees.
  - This is the key source for future global scene / tile partitioning work.

- `servo/components/webgpu/wgpu_thread.rs`
  - Handles WebGPU adapter/device requests.
  - Currently requests adapters from `wgt::Backends::all()`.
  - Future GPU affinity policy for WebGPU surfaces should start here, after display topology is available.

## Multi-GPU Insertion Points

- Phase 1 topology data should first remain outside Servo as a standalone probe, then move into `servoshell` preferences or a debug command once stable.
- Phase 2 virtual viewport work belongs at the embedder/window boundary:
  - `servoshell` preferences for virtual viewport and tile config.
  - `HeadedWindow` input coordinate conversion.
  - `ServoBuilder` / webview creation path for global viewport sizing.
- Phase 3 tile partitioning should start at WebRender display-list construction:
  - preserve one global layout/display list;
  - derive per-tile clipped display lists or per-tile scene submissions.
- Phase 4 direct present requires replacing the single `HeadedWindow` presentation model with multiple platform windows or swapchain targets:
  - one tile renderer per monitor;
  - each renderer bound to a DXGI adapter/output from topology data;
  - no final single-GPU compositor in v1.

## Current Hardware Observation

`etc/multigpu/tools/topology_probe` detected:

- 2 NVIDIA RTX A4000 adapters plus Intel Graphics and Microsoft Basic Render Driver.
- 3 active monitors: `DISPLAY1`, `DISPLAY2`, `DISPLAY3`.
- DXGI reports all 3 active outputs on DXGI adapter 0.
- The second RTX A4000 is visible as a DXGI adapter but currently has no attached output.

This means the current machine can validate topology detection and 3-monitor tiling, but it is not currently wired as a true multi-GPU/multi-output wall.
