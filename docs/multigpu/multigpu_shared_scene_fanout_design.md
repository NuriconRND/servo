# Multi-GPU Shared Scene Fan-Out Design

## Current State

ServoShell now supports a tiled wall scaffold:

- `--wall-layout <path>` loads a virtual viewport and tile rects.
- `--wall-tile-index <n>` runs one window as one tile.
- `--wall-all-tiles` opens one headed ServoShell window per tile.
- Each tile window can render an overlap-expanded offscreen surface and blit only its visible tile sub-rect.
- Input coordinates for headed windows are translated from tile-local device pixels to virtual viewport device pixels.
- In wall-all-tiles mode, ServoShell now creates one primary top-level `WebView` and registers secondary tile windows as additional paint targets for that same logical `WebViewId`.
- Paint broadcasts display-list, frame-tree, frame generation, font, image, viewport, animation, scroll, and LCP updates to every registered painter target for the logical `WebViewId`.

Validation so far:

- `cargo check -p servoshell` passes.
- `cargo build -p servoshell` passes.
- `cargo test -p servoshell wall_layout --lib` passes.
- An 8-second smoke run using `etc/multigpu/config/wall_layout.example_3x1.json` stayed alive and logged three planned tile windows.
- The latest 8-second smoke logged balanced headed-window presents: tile 0 = 173, tile 1 = 173, tile 2 = 173.
- The latest 8-second smoke logged balanced target repaint counts: primary = 175, secondary tile targets = 175 each.
- The latest frame diagnostics logged 3 startup `Wall frame overlap` info entries, 0 missed-frame warnings, 0 unexpected frame-ready-without-pending diagnostics, and 0 panic/error entries.

## Current Limitation

The current `--wall-all-tiles` path has moved from independent WebViews to one logical WebView with multiple paint targets, but it is still not the final direct-present multi-GPU implementation.

Remaining limitations:

- GPU adapter selection is explicit where the Windows ANGLE/no-WGL path can choose a DXGI adapter by tile `gpu`; unsupported backends still log the requested GPU and fall back to surfman's default adapter selection.
- Per-target resize and hidpi lifecycle is implemented for shared tile windows, but explicit target removal/teardown still needs a fuller API.
- Per-target frame, render, repaint, and present diagnostics are implemented, but software barrier synchronization is not implemented yet.
- Interactive visual verification on the real multi-monitor wall is still pending.
- The current headed path still presents through one redraw event that synchronously updates/paints/presents all tile windows for the shared `WebView`; Phase 5 must add explicit group-level synchronization policy.

## Target Behavior

For a wall layout with N tiles:

- One logical top-level browsing context owns DOM, JS, layout, scroll, focus, and input.
- Layout viewport is the full virtual viewport.
- Paint receives one global scene/update stream for the logical `WebView`.
- Each tile render target consumes the same scene/frame metadata.
- Each tile target applies its own:
  - render origin,
  - visible tile rect,
  - overlap-expanded render rect,
  - rendering context,
  - eventually GPU adapter / monitor binding.

## Relevant Current Code Shape

Important existing relationships:

- `WebView::new` registers one `RenderingContext` and derives one `WebViewId` from the returned `PainterId`.
- `Paint` owns a list of `Painter`s, each tied to one `RenderingContext`.
- `Painter` owns a `WebRender` document and a map of `WebViewRenderer`s.
- Many paint messages route by `webview_id.into()` to find the painter.
- `WebViewRenderer` currently tracks viewport details, viewport origin, root pipeline, scroll state, and hit testing for one `WebView` in one `Painter`.

This means the next true shared-scene step cannot just open more windows. It needs paint routing changes so one logical `WebViewId` can be rendered by more than one painter/render target.

## Proposed Implementation Path

### Step 1. Introduce Tile Render Target Metadata

Add a small paint-side struct that describes one output target:

```rust
struct TileRenderTarget {
    rendering_context: Rc<dyn RenderingContext>,
    visible_rect: DeviceIntRect,
    render_rect: DeviceIntRect,
    viewport_origin: DeviceVector2D,
    tile_index: usize,
}
```

Keep this separate from `WallLayout` so paint does not need to know ServoShell-specific config parsing.

### Step 2. Decouple WebViewId From PainterId For Wall Mode

Current code assumes `webview_id.into()` maps to the only painter for that WebView. Wall mode needs:

- one logical `WebViewId`,
- several `PainterId`s / `Painter`s,
- a routing table from `WebViewId` to all target painter ids.

Add a paint-level mapping:

```rust
HashMap<WebViewId, Vec<PainterId>>
```

For normal mode, the vector has one painter. For wall mode, it has one painter per tile.

### Step 3. Broadcast Scene Transactions

For messages that install or update display lists, frame trees, scroll offsets, image updates, and generated frames:

- route to every painter registered for the logical `WebViewId`;
- use identical epoch/frame metadata;
- apply per-target root scene transform and clip/blit state.

Input and hit-testing should still use one authoritative target or logical viewport mapping. In v1, route input through the focused tile window and forward to the logical `WebView`.

### Step 4. Create One Logical WebView With Multiple Rendering Contexts

ServoShell wall-all-tiles should stop creating one top-level `WebView` per tile. Instead:

- create one logical top-level `WebView`;
- create one platform window/rendering context per tile;
- register additional tile render targets with Paint for that same `WebViewId`;
- make non-primary tile windows present output only, without their own DOM/script page.

This probably requires a new ServoShell concept such as `WallWindowGroup` or `TileWindowRole`:

- primary tile window owns UI focus and the logical WebView handle;
- secondary tile windows own a rendering context and event forwarding;
- all tile windows can request repaint/present.

### Step 5. Add Frame Diagnostics

Status: implemented.

Diagnostics now include:

- shared wall `logical_frame_id`,
- per-painter `local_frame_id`,
- per-tile frame request and frame-ready timing,
- per-tile render start/end timing,
- per-window repaint target timing,
- per-window headed present timing,
- startup overlap counters,
- unexpected frame-ready counters,
- tile index and monitor/GPU mapping.

These diagnostics are needed before Phase 5 synchronization work.

### Step 6. GPU Adapter Binding

Status: partially implemented.

Wall tile `gpu` mapping is propagated into `WindowRenderingContext`, exposed through the `RenderingContext` trait, and used by the Windows ANGLE/no-WGL path to choose the requested DXGI adapter. Other backends still fall back to surfman's default selection.

## Recommended Next Code Task

Start Phase 5 software barrier work:

- define a group-level frame coordinator keyed by shared wall `logical_frame_id`;
- track which target `PainterId`s have reported frame-ready for each logical frame;
- record render completion and present timing per target for that logical frame;
- add a deadline/missed-frame policy that keeps the previous frame for delayed targets;
- decide whether the current headed path can block group present directly or should first log barrier decisions while preserving the existing synchronous tile present loop.

The current diagnostics already provide the stable ids and per-target timings needed for this work.

## Progress

- Added a `Paint`-side `WebViewId -> Vec<PainterId>` target registry.
- Registered normal `WebView` creation through that registry, preserving the current single-target behavior.
- Replaced direct `webview_id.into()` routing in `Paint` message/API paths with primary-target helper methods.
- `remove_webview()` now removes a logical `WebView` from every registered painter target, which prepares the cleanup path for multi-target rendering.
- Added clone support for frame-tree, display-list payload, and image update payloads that Paint must fan out after receiving them once.
- Refactored `Painter::handle_new_display_list()` so Paint receives display-list channels once and broadcasts cloned payloads to each target painter.
- Broadcast scene/resource/frame update paths to all registered target painters.
- Added libservo `WebViewPaintTarget`, `WebView::add_paint_target()`, and `WebView::paint_target()`.
- Changed ServoShell wall-all-tiles startup so secondary tile windows reuse the primary logical WebView and register their own paint target.
- Changed shared tile repaint/update handling so new-frame and UI update notifications reach every window that contains the logical WebView.
- Propagated wall tile `gpu` mapping into `WindowRenderingContext` and `RenderingContext`.
- Added Windows ANGLE/no-WGL DXGI adapter selection for requested tile GPU indices, with logged fallback on unsupported backends.
- Hardened per-target resize/HiDPI lifecycle so secondary tile windows update their own paint target metadata without mutating the primary logical WebView.
- Added per-target frame diagnostics with separate per-painter `local_frame_id` and shared wall `logical_frame_id`.
- Changed startup pending WebRender-frame overlap diagnostics from missed-frame warnings to `Wall frame overlap` info entries.
- Changed wall-all-tiles repaint scheduling so redraw/present no longer starves non-focused tile windows when winit repeatedly delivers `RedrawRequested` to only one tile window.

Validation:

- `cargo check -p servoshell`
- `cargo build -p servoshell`
- `cargo test -p servoshell wall_layout --lib`
- touched-file `rustfmt --edition 2024 --check`
- `git diff --check`
- 8-second `--wall-all-tiles` smoke with `etc/multigpu/config/wall_layout.example_3x1.json`

Remaining before true shared-scene rendering:

- Run interactive visual verification on the real wall and confirm scroll, animation, and input stay coherent across tile boundaries.
- Add explicit per-target removal/teardown lifecycle APIs.
- Implement Phase 5 software barrier and missed-frame policy.
- Verify explicit GPU binding on real multi-GPU hardware and non-Windows backends.
