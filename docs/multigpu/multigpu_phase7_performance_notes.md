# Multi-GPU Tiled Present Phase 7 Performance Notes

Date: 2026-06-07

## Scope

This is the first repeatable performance measurement pass for the tiled-present prototype.

The current machine has three active monitors on GPU 0, so these numbers are local 1x1 and local 3x1 wall measurements. They do not validate 2 GPU/2 monitor or 4 GPU/4 monitor scaling yet.

## Tooling

Log analyzer:

```powershell
python .\etc\multigpu\tools\wall_perf_analyzer\analyze_wall_perf.py <logs> --format markdown
```

The analyzer parses:

- `Wall render end` painter render timing.
- `Wall repaint target` per-window repaint render timing.
- `Wall window present` headed-window present timing by tile.
- `Wall frame barrier complete` and `Wall frame barrier missed` synchronization timing.
- metadata mismatch, skipped repaint, panic, error, missed-frame, pending-frame, and unexpected-ready diagnostics.

## Test Content

Stress page:

```text
tests/html/multigpu_wall_stress_cases.html
```

The page includes a wide dashboard, transform/opacity/filter/blur/shadow content, fixed and sticky elements, overflow scrolling, iframe content, 2D canvas, WebGL, WebGPU fallback, and generated video.

Note: the generated video path is not a Servo WebRTC validation path. Servo's current `HTMLCanvasElement.captureStream()` implementation is stub-like, so dedicated WebRTC/video wall testing should use `getUserMedia()` or an external WebRTC sender. See `docs/multigpu/multigpu_webrtc_video_notes.md`.

## Runs

All measurements used the existing debug `target\debug\servoshell.exe`, `RUST_LOG=info`, and a 6 second smoke window unless noted.

| Run | Layout | Command shape | Log |
| --- | --- | --- | --- |
| 1x1 baseline | `etc/multigpu/config/wall_layout.example_1x1.json` | `--wall-layout ... tests/html/multigpu_wall_stress_cases.html` | `servo/servoshell_wall_perf_1x1.err.log` |
| 3x1 overlap 0 | `etc/multigpu/config/wall_layout.example_3x1_overlap0.json` | `--wall-layout ... --wall-all-tiles tests/html/multigpu_wall_stress_cases.html` | `servo/servoshell_wall_perf_3x1_overlap0.err.log` |
| 3x1 overlap 32 | `etc/multigpu/config/wall_layout.example_3x1.json` | `--wall-layout ... --wall-all-tiles tests/html/multigpu_wall_stress_cases.html` | `servo/servoshell_wall_perf_3x1_overlap32.err.log` |
| 3x1 overlap 64 | `etc/multigpu/config/wall_layout.example_3x1_overlap64.json` | `--wall-layout ... --wall-all-tiles tests/html/multigpu_wall_stress_cases.html` | `servo/servoshell_wall_perf_3x1_overlap64.err.log` |

## Summary

| Run | Logical Frames | Metadata Mismatch | Barrier Miss | Present Balance | Render P95 ms | Present P95 ms | Barrier First-to-All P95 ms | Barrier Request-to-All P95 ms |
| --- | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: |
| 1x1 baseline | 0 | 0 | 0 | `0:157` | 2.176 | 7.105 | n/a | n/a |
| 3x1 overlap 0 | 206 | 0 | 0 | `0:166, 1:166, 2:166` | 8.066 | 6.441 | 0.748 | 8.592 |
| 3x1 overlap 32 | 198 | 0 | 0 | `0:166, 1:167, 2:166` | 9.568 | 6.285 | 0.705 | 8.191 |
| 3x1 overlap 64 | 206 | 0 | 0 | `0:159, 1:160, 2:160` | 8.803 | 6.098 | 0.673 | 8.273 |

## Observations

- All 3x1 runs maintained matched scroll metadata across all wall logical frames.
- All 3x1 runs completed without barrier misses, skipped repaint targets, panic/error diagnostics, missed-frame logs, pending-frame warnings, or frame-ready-without-pending diagnostics.
- Present counts were balanced across tiles: exact balance for overlap 0 and spread 1 for overlap 32/64.
- Barrier first-to-all-ready p95 stayed below 1 ms for all measured overlap settings.
- Barrier request-to-all-ready p95 stayed around 8.2-8.6 ms for all measured overlap settings.
- The overlap 64 run had one startup/warmup outlier at `request_to_all_ready_ms=1318.902`; p95 remained 8.273 ms, so the outlier should be tracked separately from steady-state timing.
- Painter render max values include startup/warmup spikes above 450 ms in all 3x1 runs. P95 is the better current steady-state signal.

## Limitations

- These measurements used a debug build.
- All active monitors are on GPU 0, so GPU scaling and direct per-GPU present still need target hardware validation.
- GPU utilization counters were not collected in this pass.
- The 1x1 baseline uses wall-layout mode to enable comparable wall diagnostics; it does not produce wall logical frame or barrier metrics because there is only one paint target.

## Next Measurements

- Repeat the same analyzer on a release build.
- Collect 2 GPU/2 monitor and 4 GPU/4 monitor logs on target hardware.
- Add GPU utilization capture once a stable source is chosen for Windows/NVIDIA.
- Measure longer runs after the startup/warmup period to separate steady-state numbers from initialization spikes.
- Add a WebRTC/video wall probe and measure it separately from the generic stress page.

## 2026-06-08 2x1 Dual-GPU Target Run

This run used the actual 2 GPU / 2 monitor layout:

- Tile 0: monitor 0, GPU 0, `(0, 0) - (1920, 1080)`
- Tile 1: monitor 1, GPU 1, `(1920, 0) - (3840, 1080)`
- Layout: `etc/multigpu/config/wall_layout.example_2x1_dualgpu.json`
- Test content: `tests/html/multigpu_wall_sync_probe.html`
- ServoShell mode: one process, `--wall-all-tiles`, one logical `WebView` with two paint targets.

Validated log:

```text
servo/servoshell_wall_2x1_dualgpu_sync_goal_direct_20260608_161425.err.log
```

GPU utilization capture:

```text
servo/gpu_load_2x1_dualgpu_sync_goal_direct_20260608_161425.csv
```

Summary:

| Metric | Result |
| --- | ---: |
| Logical frames | 2288 |
| Metadata matched | 2288 |
| Metadata mismatched | 0 |
| Barrier completed before deadline | 2286 |
| Barrier missed | 2 |
| Skipped repaint targets | 2 |
| Panic diagnostics | 0 |
| Error diagnostics | 0 |
| Tile 0 presents | 1271 |
| Tile 1 presents | 1270 |
| Primary/GPU0 repaint targets | 1273 |
| Secondary/GPU1 repaint targets | 1272 |
| GPU0 utilization avg/max | 35.40% / 44% |
| GPU1 utilization avg/max | 32.50% / 42% |

Observations:

- The previous issue where logical frames advanced but tile window presents did not repeat was fixed by direct wall tile group repaint/present scheduling.
- Both tile windows repeatedly presented and remained balanced with spread 1.
- Both requested GPU paths were active: primary target on GPU 0 and secondary target on GPU 1.
- The two missed barriers used the expected keep-previous-frame policy. Both corresponding skipped repaint logs were policy-driven, not crashes or metadata mismatches.
- This run demonstrates application-level multi-GPU tile rendering and synchronized frame release, but it does not prove that Windows DWM or the display driver avoided every internal GPU copy.

Remaining performance work:

- Repeat this run in release mode.
- Run a longer steady-state capture and exclude startup/shutdown samples.
- Add automated p95/p99 reporting for the 2x1 target log.
- Add ETW/GPUView or PresentMon capture to validate OS compositor behavior.

## 2026-06-10 Release Generic Stress Page Smoke

This pass reran the generic object stress page on the 2x1 dual-GPU target after
the page exited immediately in release mode.

Command shape:

```powershell
target\release\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.example_2x1_dualgpu.json --wall-all-tiles tests\html\multigpu_wall_stress_cases.html
```

Observed failure before the fix:

- The release process exited when the stress page reached its WebGL canvas path.
- The stderr log showed `SurfaceImportFailed(BadParameter)` from surfman while
  importing a WebGL front-buffer surface into WebRender.
- The immediate crash path was an `unwrap()` in
  `components/shared/paint/rendering_context.rs` during
  `device.create_surface_texture(context, surface)`.
- A first defensive path that tried to destroy the failed surface still tripped
  surfman's Windows/ANGLE surface ownership check.

Fix applied:

- `RenderingContext::create_texture()` now returns the original `Surface` on
  failure instead of panicking or destroying it locally.
- `WebGLExternalImages::lock_swap_chain()` recycles the failed surface back into
  the WebGL swap-chain, decrements the busy-context counter, and marks rendering
  to that context as finished.
- The repeated nonfatal surface import diagnostic was lowered to debug level to
  avoid release log spam.

Validation:

- `.\mach.bat build --release --media-stack gstreamer -j 8` completed
  successfully after the fix.
- Relaunched log:
  `target\multigpu_logs\wall_stress_objects_2x1_release_watch_stderr_20260610_201427.log`.
- The process stayed alive past the immediate startup window; checked live as
  PID `25496` after startup and again after an additional 10 seconds.
- The new stderr did not contain `panic_hook`, `Surface drop`, or
  `SurfaceImportFailed(BadParameter)` warnings at `RUST_LOG=warn`.

Remaining risk:

- This fix prevents the whole wall process from exiting when a WebGL external
  image surface cannot be imported.
- It does not prove that the WebGL canvas content itself is correctly visible on
  every tile when surfman rejects the surface import. Visual inspection and a
  specific WebGL fallback policy are still needed.
