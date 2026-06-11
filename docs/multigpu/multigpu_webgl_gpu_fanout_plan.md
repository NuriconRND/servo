# Multi-GPU WebGL Fan-Out Plan

## Goal

Make WebGL canvas content render on every tile in the multi-GPU wall path.

The target path is one Servo process using `--wall-all-tiles`, one logical
`WebView`, and multiple paint targets mapped to different GPUs. WebGL should not
depend on importing a native texture produced on GPU 0 into GPU 1. Instead, each
tile GPU should have its own WebGL backend context and swap-chain, and each tile
WebRender instance should lock the WebGL texture produced on the same GPU.

## Current Problem

Current WebGL context creation uses the primary logical `WebViewId` as a single
`PainterId`. The WebGL thread creates one surfman GL context, one device, and one
swap-chain for that primary painter.

In wall mode, the scene is fanned out to multiple painters, but the WebGL
external image is still a single native texture. A secondary tile renderer on a
different GPU may try to import the primary GPU's WebGL surface. On Windows/ANGLE
this can fail with `SurfaceImportFailed(BadParameter)` or result in WebGL content
appearing only on the GPU that owns the original WebGL context.

The current defensive fix prevents process exit when import fails, but it does
not make WebGL visible on every GPU tile.

## Target Architecture

Use one logical WebGL context ID with one backend WebGL context per target
painter/GPU.

- Normal single-window WebGL keeps one backend context.
- Wall shared-WebView WebGL creates one backend context for each registered
  target painter.
- WebGL commands, resize requests, and swap-buffer requests fan out to all
  backend contexts in deterministic order.
- WebRender external image lock resolves the logical WebGL context ID plus the
  locking painter ID to the matching backend swap-chain.
- Each tile WebRender locks a same-GPU texture, avoiding cross-GPU native
  texture import for WebGL.

This distributes WebGL rendering workload by rendering the same WebGL command
stream on each tile GPU. It does not split individual WebGL draw calls across
GPUs inside one context.

## Progress Tracker

| ID | Status | Task | Notes |
| --- | --- | --- | --- |
| W1 | Done | Diagnose the current WebGL external image path. | Confirmed one logical WebGL canvas creates one primary-painter surfman context and one swap-chain. |
| W2 | Done | Choose the v1 strategy. | Use GPU-per-painter mirroring. Do not use CPU readback fallback for v1. |
| W3 | Done | Add target painter discovery for WebGL context creation. | WebGL creation queries Paint's wall target painter list; single-window fallback remains `[primary]`. |
| W4 | Done | Refactor WebGLThread to separate logical and backend contexts. | One logical context now owns backend GL context data per target painter; release build passed. |
| W5 | Done | Extend WebGL swap-chain/external-image keying. | Swap-chain and busy maps now use logical context plus locking painter; lock routing validated for both painters. |
| W6 | Done | Fan out WebGL commands, resize, swap, and removal. | Mutation commands fan out to backend contexts; response commands keep primary DOM-visible results; stress validation passed. |
| W7 | Done | Add diagnostics for backend creation and lock routing. | Backend fan-out and first successful per-surface external-image lock are logged. |
| W8 | Done | Validate on 2x1 dual-GPU wall stress page. | Release stress run confirmed backend fan-out, painter-local locks, balanced presents, and no panic/import failures. |
| W9 | Done | Record final validation and remaining risks. | Validation evidence and residual risks are recorded below. |

Status values:

- `Pending`: not started.
- `In Progress`: implementation or validation is underway.
- `Blocked`: waiting on a concrete issue before the step can continue.
- `Done`: implemented and validated for the stated scope.

## Implementation Notes

### Target Painter Discovery

Add a paint-side query path that lets WebGL context creation obtain the target
painters for a logical `WebView`.

Required behavior:

- If the `WebView` has registered wall targets, return all target painter IDs in
  their existing order.
- If no target list is registered, return the primary painter ID only.
- The returned list must be stable for the lifetime of the WebGL context in v1.

### WebGL Context Model

Keep the DOM-visible WebGL model unchanged:

- Script sees one WebGL context.
- The context keeps one logical `WebGLContextId`.
- WebGL limits and context attributes are reported from the primary backend.
- Image keys and display list references continue to use the logical context ID.

Internally, WebGLThread should map that logical context ID to backend context
entries keyed by target painter ID.

Each backend entry owns:

- target painter ID
- surfman device and GL context for that painter's requested GPU
- WebGL cached context info
- swap-chain surface for that backend
- per-backend current binding state needed by existing WebGL command handling

### External Image Locking

`WebGLExternalImages` is installed per painter/WebRender instance. It should know
the painter ID for the WebRender instance that is performing the lock.

When WebRender locks `ExternalImageId(logical_webgl_context_id)`:

1. Convert the external image ID to the logical `WebGLContextId`.
2. Combine it with the locking painter ID.
3. Lock the backend swap-chain for that pair.
4. Return the local GPU native texture to WebRender.

On lock failure, recycle the surface back to the matching backend swap-chain,
mark that backend as no longer busy, and return an invalid image for that tile
without crashing the whole wall.

### Fan-Out Behavior

The following WebGL messages should fan out to every backend for the logical
context:

- WebGL command execution
- resize
- swap buffers
- remove context
- finished rendering / unlock cleanup

The command stream order must remain identical across backends. If one backend
fails, the failure should be logged with logical context ID, painter ID, and
requested GPU. The failure should not invalidate the other backend contexts
unless the logical context must be removed.

## Validation Plan

Build checks:

```powershell
.\mach.bat build --release --media-stack gstreamer -j 8
```

Primary visual smoke:

```powershell
target\release\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.example_2x1_dualgpu.json --wall-all-tiles tests\html\multigpu_wall_stress_cases.html
```

Pass criteria:

- The WebGL canvas appears on both tile windows.
- Each tile locks a WebGL backend surface for its own painter/GPU.
- No `panic_hook` appears in stderr.
- No repeated `SurfaceImportFailed(BadParameter)` warning spam appears at
  `RUST_LOG=warn`.
- Tile presents remain balanced enough for the existing wall synchronization
  policy.

Regression checks:

- A normal single-window WebGL page creates one backend context and still
  renders.
- Existing video/YUV multi-GPU fan-out probes still use the media external image
  path and do not regress.
- The generic stress page continues running after startup and does not trigger
  the prior WebGL surface drop panic.

## Progress Update Rules

Update this document during implementation:

1. Set the current tracker row to `In Progress` before starting code changes for
   that step.
2. Add a dated `Progress Log` entry after every meaningful implementation,
   build, or runtime validation pass.
3. If a validation fails, keep the step `In Progress` or mark it `Blocked`, and
   record the failing command, log path, observed error, and next action.
4. Mark a step `Done` only after the relevant build or runtime check has passed.
5. Keep large media assets out of commits; reference local media paths only when
   needed for validation.

## Progress Log

### 2026-06-10 - Initial plan

- Confirmed current WebGL context creation sends `window.webview_id().into()` as
  a single primary `PainterId`.
- Confirmed WebGLThread creates one device/context/swap-chain for that painter.
- Confirmed wall rendering has multiple target painters, but WebGL external
  image lock currently uses only the logical WebGL context ID.
- Selected GPU-per-painter WebGL backend mirroring as the v1 strategy.
- Scope is the wall shared-WebView path first; normal single-window WebGL should
  retain one backend context.

### 2026-06-10 - Implementation started

- Started W3 target painter discovery implementation.
- The planned data source is Paint's existing `webview_painter_targets` map.
- The planned fallback for non-wall WebGL remains a single primary painter.

### 2026-06-10 - W3-W7 implementation pass

- Added a paint-side `GetWebViewPainterTargets` query and script-side WebGL
  creation now passes the target painter list into `WebGLMsg::CreateContext`.
- Added `WebGLSurfaceId`, combining logical WebGL context ID and target
  `PainterId`, for backend swap-chain and busy-state ownership.
- Refactored WebGLThread storage so one logical WebGL context can own backend GL
  contexts for each target painter.
- Updated WebGL external image lock/unlock to route by the locking painter ID,
  so each tile WebRender instance selects its local backend surface.
- Added initial WebGL command fan-out for backend contexts:
  - buffer data receivers are consumed once and applied to all backends
  - resource creation/linking runs on every backend and returns the primary
    result to script
  - mutation commands without channels are replayed to each backend through
    serialized command cloning
  - query/response commands keep primary-only DOM-visible results
- `cargo check -p servo-webgl` passed in the VS developer environment.
- Broader `cargo check` is currently blocked by an unrelated
  `zeroize` derive configuration issue in `servo-constellation-traits`.

### 2026-06-10 - W4-W9 release validation

- Added a one-shot `info` diagnostic for successful WebGL external-image lock
  routing, keyed by `WebGLSurfaceId`, so the validation log records which
  painter/WebRender instance locked which backend surface.
- Release build passed:

  ```powershell
  .\mach.bat build --release --media-stack gstreamer -j 8
  ```

- Runtime validation command:

  ```powershell
  target\release\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.example_2x1_dualgpu.json --wall-all-tiles tests\html\multigpu_wall_stress_cases.html
  ```

- Validation log:
  `target\multigpu_logs\wall_stress_webgl_fanout_stderr_20260610_232153.log`
- Confirmed WebGL backend fan-out:

  ```text
  WebGL multi-GPU backend fan-out: logical_context=WebGLContextId(1) primary_painter=PainterId(1) target_painters=[PainterId(1), PainterId(2)]
  ```

- Confirmed painter-local WebGL external-image locks:

  ```text
  WebGL external image lock routed: surface=WebGLSurfaceId { context_id: WebGLContextId(1), painter_id: PainterId(1) } painter=PainterId(1) texture=32 size=876x330
  WebGL external image lock routed: surface=WebGLSurfaceId { context_id: WebGLContextId(1), painter_id: PainterId(2) } painter=PainterId(2) texture=32 size=876x330
  ```

- Validation counters from the same run:

  ```text
  fanout=1
  lock_route=2
  surface_painter1=1
  surface_painter2=1
  gpu0_present=628
  gpu1_present=627
  gpu0_repaint=631
  gpu1_repaint=629
  barrier_missed=0
  panic_hook=0
  panic=0
  surface_import_failed=0
  clone_fail=0
  id_mismatch=0
  ```

- A longer earlier run also stayed balanced with 9,206 GPU 0 presents and
  9,205 GPU 1 presents, with no panic, no `SurfaceImportFailed`, no clone
  failure, and no backend resource ID mismatch.

## Remaining Risks

- Mirroring renders the same WebGL command stream once per tile GPU. It improves
  locality and visibility for multi-GPU wall output, but it increases total
  WebGL rendering work compared with one shared context.
- Backend GL limits may differ across GPUs. v1 reports primary backend limits;
  target wall hardware should use comparable GPUs.
- WebXR and OffscreenCanvas are not included in v1 unless they naturally reuse
  the same logical WebGL message path without extra work.
- This plan avoids application-level cross-GPU WebGL texture sharing, but it
  cannot prove that Windows DWM or the driver performs no internal copies after
  tile windows present.
