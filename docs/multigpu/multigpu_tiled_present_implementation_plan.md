# Multi-GPU Tiled Present Browser Implementation Plan

## Project Goal

硫??GPU/硫???붿뒪?뚮젅???섍꼍?먯꽌 ?섎굹??logical browser viewport瑜??щ윭 monitor region?쇰줈 ?섎늻怨? 媛?GPU媛 ?먭린 monitor ?곸뿭??吏곸젒 render/present?섎뒗 寃쎈웾 釉뚮씪?곗? ?고??꾩쓣 媛쒕컻?쒕떎.

湲곕컲 ?붿쭊? Servo濡??쒕떎. 珥덇린 紐⑺몴???쇰컲 ?뚮퉬?먯슜 釉뚮씪?곗?媛 ?꾨땲??鍮꾨뵒?ㅼ썡, 愿?쒖꽱?? 珥덇퀬?댁긽????쒕낫?쒖슜 ?꾩슜 釉뚮씪?곗? ?고??꾩씠??

## Core Architecture

```text
Servo DOM / CSS / JS
        ??Global Layout
        ??Global Display List / Scene
        ??Tiled Display Partitioner
        ??Per-GPU Renderer
 ?뚢?????????????????????р?????????????????????? ??GPU 1 ??Monitor 1  ??GPU 2 ??Monitor 2  ?? ??top-left region    ??top-right region   ?? ?쒋?????????????????????쇄?????????????????????? ??GPU 3 ??Monitor 3  ??GPU 4 ??Monitor 4  ?? ??bottom-left region ??bottom-right region?? ?붴?????????????????????닳??????????????????????        ??GPU蹂?swapchain??吏곸젒 present
```

?듭떖 ?먯튃:

- DOM, CSS, JS, layout? ?꾩껜 virtual viewport 湲곗??쇰줈 ??踰?怨꾩궛?쒕떎.
- Render? present留?GPU/monitor蹂?region?쇰줈 遺꾪븷?쒕떎.
- 理쒖쥌 ?⑹꽦 ?꾨떞 GPU???먯? ?딅뒗??
- 紐⑤뱺 renderer??媛숈? `frame_id`, `timestamp`, `scroll_offset`, `device_scale_factor`, `viewport_transform`???ъ슜?쒕떎.

---

## Phase 0. Repository And Build Baseline

### Goal

Servo 湲곕컲 媛쒕컻???쒖옉?????덈뒗 濡쒖뺄 baseline??留뚮뱺??

### Tasks

- [x] Servo fork ?먮뒗 Servo 湲곕컲 workspace瑜?以鍮꾪븳??
- [x] Windows 媛쒕컻 ?섍꼍?먯꽌 湲곕낯 minibrowser build瑜??깃났?쒗궓??
- [ ] 湲곕낯 ?섏씠吏 濡쒕뵫, ?ㅽ겕濡? resize ?숈옉???뺤씤?쒕떎.
- [x] ?꾩옱 rendering pipeline 吏꾩엯?? WebRender/wgpu ?곌껐 吏?먯쓣 臾몄꽌?뷀븳??

### Verification

- [x] clean build媛 ?깃났?쒕떎.
- [x] minibrowser媛 濡쒖뺄 HTML ?뚯씪???쒖떆?쒕떎.
- [ ] 湲곕낯 scroll/input???뺤긽 ?숈옉?쒕떎.
- [x] rendering pipeline 議곗궗 硫붾え媛 ?묒꽦?섏뼱 ?덈떎.

### Phase Result

- Status: `Completed With Manual GUI Check Pending`
- Notes:
  - Servo cloned to `servo` at revision `92c8770e5`.
  - Build environment helper added at `etc/multigpu/servo_env.ps1`.
  - `.\mach build -j 8` succeeds after local Rust 1.95.0 setup and Servo GStreamer dependency installation.
  - Direct runtime smoke command succeeds: `.\target\debug\servoshell.exe -f tests/html/close-on-load.html`.
  - Interactive scroll/resize could not be visually verified from this non-interactive shell session; verify manually before Phase 2 input-coordinate work.
  - Rendering pipeline notes saved to `docs/multigpu/servo_phase0_rendering_pipeline_notes.md`.

### Next Phase Gate

Phase 0 寃利???ぉ??紐⑤몢 ?듦낵?댁빞 Phase 1濡?吏꾪뻾?쒕떎.

---

## Phase 1. Display And GPU Topology Detection

### Goal

?ㅽ뻾 ?섍꼍??GPU, monitor, display bounds ?뺣낫瑜??섏쭛?섍퀬 logical wall layout怨?留ㅽ븨?쒕떎.

### Tasks

- [x] GPU adapter 紐⑸줉???섏쭛?쒕떎.
- [x] monitor 紐⑸줉, ?댁긽?? refresh rate, DPI, desktop position???섏쭛?쒕떎.
- [x] GPU? monitor???곌껐 愿怨꾨? 媛?ν븳 踰붿쐞?먯꽌 ?앸퀎?쒕떎.
- [x] topology dump 紐낅졊 ?먮뒗 debug page瑜?異붽??쒕떎.
- [x] wall layout ?ㅼ젙 ?뚯씪???뺤쓽?쒕떎.

湲곕낯 ?ㅼ젙 ?덉떆:

```json
{
  "virtualViewport": { "width": 7680, "height": 4320 },
  "tiles": [
    { "display": 0, "rect": [0, 0, 3840, 2160] },
    { "display": 1, "rect": [3840, 0, 3840, 2160] },
    { "display": 2, "rect": [0, 2160, 3840, 2160] },
    { "display": 3, "rect": [3840, 2160, 3840, 2160] }
  ],
  "overlapPx": 32
}
```

(2026-07-24 update) `display` is a **spatial** display index (top-left = 0, left→right then
top→bottom), resolved at window-creation time against the DXGI display topology; the GPU that
drives that display is auto-assigned automatically — there is normally no need to name a GPU at
all. The old `monitor` field name (a non-spatial, platform-dependent `winit`
`available_monitors().nth()` index) is still accepted as a **deprecated alias** for `display`.
`gpu` is now an **optional explicit override**: when present it wins over the auto-assigned
adapter, e.g. to deliberately render a tile on a GPU that does not drive its own display (useful
for cross-GPU testing, see `etc/multigpu/config/wall_layout.test_2x1_gpu1.json`). The default
(recommended) path omits `gpu` entirely and lets auto-GPU pick the adapter that drives each
tile's `display`.

### Verification

- [ ] 1 GPU/1 monitor 援ъ꽦???뺥솗??異쒕젰?쒕떎.
- [x] 2 monitor ?댁긽 援ъ꽦???뺥솗??異쒕젰?쒕떎.
- [x] ?ㅼ젙 ?뚯씪??tile rect媛 virtual viewport bounds ?덉뿉 ?덈뒗吏 寃利앺븳??
- [x] monitor? tile rect 留ㅽ븨 ?ㅻ쪟瑜??щ엺???쎌쓣 ???덈뒗 硫붿떆吏濡?蹂닿퀬?쒕떎.

### Phase Result

- Status: `In Progress`
- Notes:
  - Standalone topology dump tool added at `etc/multigpu/tools/topology_probe`.
  - Example 2x2 wall config added at `etc/multigpu/config/wall_layout.example_2x2.json`.
  - Local 3x1 validation config added at `etc/multigpu/config/wall_layout.example_3x1.json`.
  - Probe output includes `wgpu` adapters, DXGI adapters/outputs, monitor bounds, and layout validation.
  - Current machine has 3 active monitors, all attached to DXGI adapter 0; second RTX A4000 has no active output.

### Next Phase Gate

?ㅼ젣 媛쒕컻 ?λ퉬?먯꽌 topology dump媛 ?뺥솗???섏삤怨? ?ㅼ젙 ?뚯씪 寃利앹씠 ?숈옉?댁빞 Phase 2濡?吏꾪뻾?쒕떎.

---

## Phase 2. Virtual Viewport And Global Scene

### Goal

釉뚮씪?곗?媛 ?щ윭 monitor瑜??섎굹????logical viewport濡?痍④툒?섎룄濡?留뚮뱺??

### Tasks

- [x] virtual viewport ?ш린瑜?wall layout ?ㅼ젙?먯꽌 ?쎈뒗??
- [x] Servo layout viewport瑜?physical monitor媛 ?꾨땲??virtual viewport 湲곗??쇰줈 ?ㅼ젙?쒕떎.
- [x] mouse 醫뚰몴瑜?monitor-local 醫뚰몴?먯꽌 virtual viewport 醫뚰몴濡?蹂?섑븳??
- [x] scroll, zoom, keyboard, pointer event瑜??꾩뿭 pipeline????踰덈쭔 ?꾨떖?쒕떎.
- [x] global display list ?먮뒗 scene snapshot??tile renderer媛 怨듭쑀?????덈뒗 ?뺥깭濡?遺꾨━?쒕떎.

### Verification

- [ ] 7680x4320 媛숈? virtual viewport 湲곗??쇰줈 layout???앹꽦?쒕떎.
- [ ] mouse click 醫뚰몴媛 ?щ컮瑜?virtual coordinate濡?蹂?섎맂??
- [ ] scroll offset??紐⑤뱺 tile???숈씪?섍쾶 ?곸슜?쒕떎.
- [ ] ?⑥씪 monitor fallback?먯꽌??湲곗〈 ?숈옉???좎??쒕떎.

### Phase Result

- Status: `Implementation Complete With Manual Verification Pending`
- Notes:
  - Phase 2 implementation started in ServoShell/libservo.
  - ServoShell now accepts `--wall-layout <path>` and `--wall-tile-index <index>`.
  - Wall layout JSON is parsed into a virtual viewport plus tile rects; invalid viewport/tile bounds fail at startup.
  - Top-level WebViews can override layout viewport size independently from the platform rendering context size.
  - Headed-window mouse, wheel, touch, and pinch coordinates are translated from tile-local device pixels into virtual viewport device pixels using the selected tile origin.
  - `ScreenGeometry` reports virtual viewport size and selected tile window rect when wall layout mode is active.
  - Manual verification page added at `tests/html/multigpu_virtual_viewport_probe.html` for viewport metrics, scroll state, and input coordinate probing.
  - One logical top-level `WebView` can now drive multiple paint targets, so scene/display-list updates are shared across tile renderers.
  - Manual GUI verification is still pending for viewport metrics, input coordinate mapping, scroll propagation, and single-monitor fallback.

### Next Phase Gate

?섎굹???꾩뿭 layout/scene??virtual viewport 湲곗??쇰줈 ?덉젙?곸쑝濡??앹꽦?섏뼱??Phase 3?쇰줈 吏꾪뻾?쒕떎.

---

## Phase 3. Tile Partitioning

### Goal

global scene??monitor蹂?tile rect濡?遺꾪븷?섍퀬, 媛?tile renderer媛 ?먭린 ?곸뿭留?洹몃━?꾨줉 ?쒕떎.

### Tasks

- [x] tile rect? `overlapPx`瑜??곸슜??render rect瑜?怨꾩궛?쒕떎.
- [x] global scene??tile render rect 湲곗??쇰줈 transform/clip?섏뿬 local context??paint?쒕떎.
- [x] DOM element媛 ?щ윭 tile??嫄몄튂硫?媛?tile?먯꽌 ?꾩슂??遺遺꾨쭔 以묐났 render?쒕떎.
- [x] fixed/sticky element媛 紐⑤뱺 愿??tile?먯꽌 媛숈? global 醫뚰몴 湲곗??쇰줈 ?뚮뜑留곷릺?꾨줉 ?쒕떎.
- [x] CSS shadow, blur, transform 寃쎄퀎 泥섎━瑜??꾪빐 overlap ?곸뿭???좎??쒕떎.

### Verification

- [ ] ???뺤쟻 HTML ?섏씠吏媛 tile 寃쎄퀎?먯꽌 ?닿툔?섏? ?딅뒗??
- [ ] fixed header媛 紐⑤뱺 monitor?먯꽌 媛숈? ?꾩튂 愿怨꾨? ?좎??쒕떎.
- [ ] sticky sidebar媛 scroll 以?寃쎄퀎?먯꽌 源⑥?吏 ?딅뒗??
- [ ] shadow/blur媛 tile 寃쎄퀎?먯꽌 ?섎━吏 ?딅뒗??

### Phase Result

- Status: `Implementation Complete With Visual Verification Pending`
- Notes:
  - Tile visible rect and overlap-expanded render rect calculation added to `ports/servoshell/wall_layout.rs`.
  - WebRender root scene now applies the selected tile origin before painting into the local rendering context.
  - Paint hit-testing compensates for the tile-origin scene transform so script/input events can continue to use virtual viewport coordinates.
  - Headed ServoShell now renders an overlap-expanded offscreen surface and blits only the visible tile sub-rect back into the platform window.
  - `--wall-all-tiles` can launch one headed ServoShell window per wall tile as a fan-out scaffold.
  - Final per-GPU direct-present fan-out is still pending in Phase 4.
  - Visual verification is still pending for static content, scrolling content, fixed/sticky elements, and shadow/blur at tile boundaries.

### Next Phase Gate

?뺤쟻 ?섏씠吏? 湲곕낯 scroll ?섏씠吏媛 tile 寃쎄퀎 artifact ?놁씠 ?쒖떆?섏뼱??Phase 4濡?吏꾪뻾?쒕떎.

---

## Phase 4. Per-GPU Renderer And Direct Present

### Goal

媛?GPU媛 ?먭린 monitor ?곸뿭??吏곸젒 render?섍퀬 ?먭린 swapchain??present?섎룄濡?留뚮뱺??

### Tasks

- [x] tile/window蹂?renderer paint target scaffold瑜??앹꽦?쒕떎.
- [x] renderer instance留덈떎 target monitor? viewport rect瑜?諛붿씤?⑺븳??
- [x] 媛?renderer媛 ?숈씪 scene snapshot怨??숈씪 frame metadata瑜??낅젰諛쏅룄濡??쒕떎.
- [x] 媛?renderer媛 target GPU adapter瑜?紐낆떆?곸쑝濡??좏깮?섎룄濡??쒕떎.
- [ ] 媛?renderer媛 ?먭린 GPU/monitor swapchain??吏곸젒 present?섎룄濡??쒕떎.
- [x] GPU 媛?texture copy??v1?먯꽌 ?ъ슜?섏? ?딅뒗??
- [x] 1 GPU/1 monitor fallback path瑜??좎??쒕떎.

### Verification

- [ ] 2 GPU/2 monitor horizontal wall?먯꽌 媛?GPU媛 ?먭린 monitor??吏곸젒 異쒕젰?쒕떎.
- [ ] 4 GPU/4 monitor 2x2 wall?먯꽌 媛?GPU媛 ?먭린 tile??異쒕젰?쒕떎.
- [ ] 理쒖쥌 ?⑹꽦 ?꾨떞 GPU ?놁씠 ?붾㈃??援ъ꽦?쒕떎.
- [ ] GPU蹂?frame time怨?present timing??濡쒓렇濡??뺤씤?????덈떎.

### Phase Result

- Status: `In Progress`
- Notes:
  - Added `--wall-all-tiles` to open one headed ServoShell window for every tile in the wall layout.
  - App startup now clones ServoShell preferences per tile, assigns a per-window `wall_tile_index`, and sizes each initial window from its tile rect.
  - Headed windows in wall-all-tiles mode are positioned on the monitor index referenced by the tile when winit can resolve that monitor.
  - Fan-out diagnostics log the tile count, per-tile monitor/GPU mapping, visible rect, and overlap-expanded render rect.
  - Automated 8-second smoke run with `etc/multigpu/config/wall_layout.example_3x1.json` stayed alive and logged three planned tile windows; interactive visual inspection is still pending.
  - Shared-scene fan-out design note added at `docs/multigpu/multigpu_shared_scene_fanout_design.md`.
  - Paint now has a `WebViewId -> Vec<PainterId>` target registry and routes current single-target WebView operations through primary-target helpers.
  - Paint display-list, frame-tree, frame generation, font, image, viewport, animation, scroll, and LCP updates are now broadcast to every painter target registered for the logical `WebViewId`.
  - libservo exposes `WebViewPaintTarget`, `WebView::add_paint_target()`, and `WebView::paint_target()` so one logical `WebView` can render into additional `RenderingContext`s.
  - `--wall-all-tiles` now creates one primary top-level `WebView`; secondary tile windows register paint targets for that same logical `WebView` instead of creating independent DOM/script/layout pipelines.
  - Shared tile windows request repaint for the same logical `WebView` and present their target-specific renderer output; GUI resize/hidpi paths avoid mutating the primary WebView from secondary paint targets.
  - Wall tile `gpu` mapping is now propagated into `WindowRenderingContext`, exposed through the `RenderingContext` trait, and reused when registering painter/WebGL surfman details.
  - Windows ANGLE/no-WGL builds now select the requested DXGI adapter index directly; unsupported backends log the requested GPU and fall back to surfman's default adapter selection.
  - Per-target resize/HiDPI lifecycle is hardened for shared tile windows:
    - libservo exposes `WebView::update_paint_target()` for one registered `WebViewPaintTarget`.
    - Paint can now resize and update viewport metadata for a specific painter target instead of only the primary target.
    - WebViewRenderer can update tile viewport origin when DPI or overlap-expanded render rect changes.
    - ServoShell GUI update keeps secondary paint target offscreen size, virtual viewport details, and tile origin synchronized without mutating the primary WebView.
  - Frame diagnostics added before Phase 5 synchronization work:
    - Paint now assigns a logical frame id to each `GenerateFrame` fan-out and passes the same id to all target painters.
    - Painter records frame request time, WebRender frame-ready wait time, render start/end timing, pending frame count, requested GPU, and a missed-frame counter.
    - WebRender frame-ready handling now guards against pending-frame underflow and logs stale/duplicate ready notifications.
    - ServoShell logs per-window repaint target timing and headed window present timing with tile/monitor/GPU mapping when wall layout mode is active.
  - Automated diagnostic smoke run passed startup/logging checks after rebuilding `target/debug/servoshell.exe`:
    - Command used `--wall-layout etc/multigpu/config/wall_layout.example_3x1.json --wall-all-tiles tests/html/multigpu_virtual_viewport_probe.html` for 8 seconds.
    - Log saved to `servoshell_wall_diag_smoke.err.log`.
    - Process stayed alive until the smoke harness stopped it.
    - Three tile plans were logged and three painters produced frame request/ready diagnostics.
    - One logical frame fan-out reached `[PainterId(1), PainterId(2), PainterId(3)]`.
    - No panic, error, or stale frame-ready-without-pending-frame diagnostics were logged.
  - Repaint scheduling follow-up resolved:
    - Wall-all-tiles repaint requests now fan out to every window that shares the logical `WebView`.
    - A headed redraw event in wall-all-tiles mode now synchronously updates/paints/presents every headed tile window for the shared `WebView`, avoiding starvation when winit repeatedly delivers `RedrawRequested` to only one tile window.
    - Repeat 8-second smoke with `etc/multigpu/config/wall_layout.example_3x1.json` logged balanced headed-window presents: tile 0 = 136, tile 1 = 136, tile 2 = 136.
    - The same run logged balanced target repaint counts: primary = 138, secondary tile targets = 138 each.
  - Frame diagnostic follow-up resolved:
    - Painter diagnostics now separate per-painter `local_frame_id` from shared wall `logical_frame_id`, so warm-up frames no longer collide with Phase 5 logical frame ids.
    - Startup frame requests that overlap older pending WebRender frames are logged as `Wall frame overlap` info diagnostics, not as missed-frame warnings.
    - Repeat 8-second smoke logged 3 startup overlaps, 0 `missed_frame_count` logs, 0 `requested frame ... still pending` warnings, and 0 unexpected frame-ready-without-pending diagnostics.
    - The same run logged balanced headed-window presents: tile 0 = 173, tile 1 = 173, tile 2 = 173.
    - The same run logged balanced target repaint counts: primary = 175, secondary tile targets = 175 each.
    - Render logs retained `logical_frame_id=Some(1)` after the shared logical frame became ready, giving Phase 5 a stable per-render diagnostic key.
  - Validation passed: `cargo check -p servoshell`; `cargo build -p servoshell`; `cargo test -p servoshell wall_layout --lib` passed 3 tests; touched-file `rustfmt --edition 2024 --check` and `git diff --check` passed.
  - Still pending: direct present to each GPU/monitor swapchain and interactive multi-monitor visual verification.
  - Immediate next work:
    - Run interactive visual verification with `etc/multigpu/config/wall_layout.example_3x1.json` and the target 2x2 wall layout, confirming all tile windows present in logs.
    - Continue Phase 5 shared `timestamp`/`scroll_offset` verification and visual sync checks.

### Next Phase Gate

GPU蹂?direct present媛 ?ㅼ젣 multi-monitor ?섍꼍?먯꽌 ?숈옉?댁빞 Phase 5濡?吏꾪뻾?쒕떎.

---

## Phase 5. Frame Synchronization

### Goal

?щ윭 GPU媛 媛숈? logical frame???뚮뜑留곹븯怨?媛?ν븳 ?숈떆??present?섎룄濡??숆린?뷀븳??

### Tasks

- [x] main coordinator媛 留?frame `frame_id`瑜?諛쒗뻾?쒕떎.
- [x] 紐⑤뱺 renderer媛 媛숈? `frame_id`, `timestamp`, `scroll_offset`???ъ슜?쒕떎.
- [x] renderer蹂?render completion ?곹깭瑜??섏쭛?쒕떎.
- [x] present ??software barrier瑜??붾떎.
- [x] deadline???볦튇 renderer???댁쟾 ?꾨젅?꾩쓣 ?좎??쒕떎.
- [x] missed frame, render latency, present latency瑜?湲곕줉?쒕떎.

### Verification

- [ ] animation??monitor 寃쎄퀎?먯꽌 媛숈? ?쒓컙 湲곗??쇰줈 ?吏곸씤??
- [ ] scroll 以?紐⑤뱺 monitor媛 媛숈? offset???좎??쒕떎.
- [x] ??renderer媛 ?먮젮?몃룄 ?꾩껜 ?깆씠 crash?섏? ?딅뒗??
- [x] missed frame ?뺤콉??濡쒓렇濡??뺤씤?쒕떎.

### Phase Result

- Status: `In Progress`
- Notes:
  - Phase 4 diagnostics now provide a shared wall `logical_frame_id` and per-painter `local_frame_id`.
  - Per-target frame request, frame-ready, render, repaint target, and headed-window present timings are available in wall layout mode.
  - Startup WebRender frame overlap is now logged as `Wall frame overlap` info instead of missed-frame warnings.
  - Latest 8-second `--wall-all-tiles` smoke with `etc/multigpu/config/wall_layout.example_3x1.json` logged balanced presents across all three tile windows and no missed-frame or unexpected frame-ready diagnostics.
  - Paint now has a wall frame coordinator keyed by shared `logical_frame_id`.
  - Multi-target `GenerateFrame` fan-out registers expected `PainterId` targets before requesting WebRender frames.
  - WebRender frame-ready diagnostics now return painter id, local frame id, logical frame id, wait time, and repaint requirement to Paint.
  - The wall frame coordinator records per-target readiness, logs `Wall frame barrier complete` when every target reaches the same logical frame, and logs `Wall frame barrier missed` when the deadline expires before every target is ready.
  - Current software barrier deadline is 16 ms after the first target reports the logical frame ready.
  - Barrier decisions are now wired into tile repaint/present:
    - Multi-target WebRender frame-ready notifications no longer set repaint flags immediately.
    - The wall frame coordinator releases repaint only after every expected target reaches the logical frame before the deadline.
    - If the deadline expires, ready targets are released and missing targets are marked `keep_previous_frame` for that logical frame.
    - ServoShell checks the selected `WebViewPaintTarget` before child WebView repaint; delayed targets skip child render/present so late content is not painted into the current present group.
    - Late completion after a missed barrier is logged with `policy=keep-previous-frame-for-delayed-targets` and does not release the delayed target for the missed frame.
  - Validation passed after barrier/present wiring:
    - `rustfmt --edition 2024 --check components\paint\paint.rs components\paint\painter.rs components\servo\webview.rs ports\servoshell\window.rs`
    - `git diff --check`
    - `cargo check -p servoshell`
    - `cargo build -p servoshell`
    - `cargo test -p servoshell wall_layout --lib` passed 3 tests.
  - Automated 8-second smoke run with `etc/multigpu/config/wall_layout.example_3x1.json` passed:
    - Command used `--wall-layout etc\multigpu\config\wall_layout.example_3x1.json --wall-all-tiles tests/html/multigpu_virtual_viewport_probe.html`.
    - Log saved to `servo\servoshell_wall_barrier_smoke.err.log`.
    - One barrier completed before deadline with ready `3/3`.
    - No barrier misses, skipped repaint targets, panics, error diagnostics, missed-frame logs, pending-frame warnings, or frame-ready-without-pending diagnostics were logged.
    - Balanced headed-window presents: tile 0 = 243, tile 1 = 243, tile 2 = 243.
    - Balanced target repaint counts: primary = 245, secondary tile targets = 490 total.
  - Shared frame metadata proof added:
    - Paint now logs `Wall frame metadata` for multi-target logical frames, comparing per-target scroll-tree snapshots before fan-out.
    - Per-painter frame request/ready diagnostics now include shared wall request timing (`shared_request_delay_ms` and `shared_request_to_ready_ms`).
    - Metadata logs mark `timestamp_source=single-script-update`, reflecting the one logical ScriptThread update that feeds all paint targets.
  - Automatic scroll/animation sync probe added at `tests/html/multigpu_wall_sync_probe.html`.
  - Automated 8-second sync smoke run with `etc/multigpu/config/wall_layout.example_3x1.json` passed:
    - Command used `--wall-layout etc\multigpu\config\wall_layout.example_3x1.json --wall-all-tiles tests/html/multigpu_wall_sync_probe.html`.
    - Log saved to `servo\servoshell_wall_sync_smoke.err.log`.
    - 280 wall logical frames were generated.
    - 280 `Wall frame metadata` entries reported `scroll_offsets=matched` across all three paint targets.
    - 280 wall frame barriers completed before deadline with ready `3/3`.
    - No metadata mismatches, barrier misses, skipped repaint targets, panics, error diagnostics, missed-frame logs, pending-frame warnings, or frame-ready-without-pending diagnostics were logged.
    - Balanced headed-window presents: tile 0 = 145, tile 1 = 145, tile 2 = 145.
    - Balanced target repaint counts: primary = 147, secondary tile targets = 294 total.
  - Sync smoke retest notes:
    - First rerun log saved to `servo\servoshell_wall_sync_smoke_rerun.err.log`.
    - First rerun generated 473 logical frames and 473 matched metadata entries.
    - First rerun logged one startup barrier miss at `logical_frame_id=2`: ready `1/3`, missing `[PainterId(1), PainterId(2)]`, then `completed_after_deadline` with ready `3/3`.
    - The missed startup frame still reported `scroll_offsets=matched`; no metadata mismatch, skipped repaint, panic, or error was logged.
    - First rerun headed-window presents were balanced: tile 0 = 234, tile 1 = 234, tile 2 = 234.
    - Second rerun log saved to `servo\servoshell_wall_sync_smoke_rerun2.err.log`.
    - Second rerun generated 468 logical frames, 468 matched metadata entries, and 468 barrier completions with no barrier misses.
    - Second rerun headed-window presents were near-balanced: tile 0 = 231, tile 1 = 232, tile 2 = 231.
    - Interpretation: shared metadata/scroll sync remained stable; one non-reproduced startup/warmup barrier miss should be tracked separately as startup scheduling jitter.
  - Visible manual-check run:
    - Visible ServoShell launched with `--wall-layout etc\multigpu\config\wall_layout.example_3x1.json --wall-all-tiles tests/html/multigpu_wall_sync_probe.html`.
    - Log saved to `servo\servoshell_wall_visible.err.log`.
    - Runtime log confirmed all three tile windows presenting: tile 0, tile 1, and tile 2.
    - Runtime log continued to report `Wall frame metadata ... scroll_offsets=matched`.
    - Visible process was terminated after the manual-check launch; no `servoshell` process remained afterward.
    - Manual visual pass/fail observation was not recorded, so Phase 5 visual verification remains pending.
  - Delayed-renderer failure injection added:
    - Paint can now simulate a delayed wall target by setting `SERVO_WALL_FRAME_DELAY_TARGET_INDEX` to a zero-based paint target index.
    - Optional `SERVO_WALL_FRAME_DELAY_AFTER` selects the first logical frame id eligible for injection.
    - Optional `SERVO_WALL_FRAME_DELAY_COUNT` selects how many logical frames to inject.
    - The injected target's frame-ready diagnostic is withheld from the wall barrier, forcing the normal missed-deadline path without crashing or killing the renderer.
    - Injected keep-previous state is retained until one repaint query observes and logs the skipped target, so the failure-policy path is testable in a fast animation loop.
  - Delayed-renderer injection smoke passed:
    - Command used `SERVO_WALL_FRAME_DELAY_TARGET_INDEX=2`, `SERVO_WALL_FRAME_DELAY_AFTER=21`, `SERVO_WALL_FRAME_DELAY_COUNT=1`, `--wall-layout etc\multigpu\config\wall_layout.example_3x1.json --wall-all-tiles tests/html/multigpu_wall_sync_probe.html`.
    - Log saved to `servo\servoshell_wall_delay_injection_smoke_repaint2.err.log`.
    - Injection scheduled `logical_frame_id=21` for delayed `PainterId(3)`.
    - The delayed target readiness was withheld from the barrier.
    - Barrier missed with ready `2/3`, missing `[PainterId(3)]`, `need_repaint=true`, and policy `keep-previous-frame-for-delayed-targets`.
    - Keep-previous skip observation was armed and consumed for `PainterId(3)`.
    - ServoShell logged `Wall repaint target skipped` for `logical_frame_id=21` with policy `keep-previous-frame`.
    - No metadata mismatch, panic, error diagnostics, frame-ready-without-pending diagnostics, or pending-frame warnings were logged.
    - Headed-window presents remained balanced: tile 0 = 226, tile 1 = 226, tile 2 = 226.
  - Post-injection no-injection regression smoke passed:
    - Log saved to `servo\servoshell_wall_sync_smoke_post_delay_injection.err.log`.
    - 440 wall logical frames were generated.
    - 440 `Wall frame metadata` entries reported `scroll_offsets=matched`.
    - No delay-injection logs, metadata mismatches, barrier misses, skipped repaint targets, panics, error diagnostics, frame-ready-without-pending diagnostics, or pending-frame warnings were logged.
    - Headed-window presents remained balanced: tile 0 = 223, tile 1 = 223, tile 2 = 223.
  - Validation passed after shared metadata diagnostics:
    - `rustfmt --edition 2024 --check components\paint\paint.rs components\paint\painter.rs`
    - `git diff --check`
    - `cargo check -p servoshell` after loading `etc\multigpu\servo_env.ps1`
    - `cargo build -p servoshell` after loading `etc\multigpu\servo_env.ps1`
    - `cargo test -p servoshell wall_layout --lib` passed 3 tests.
  - Validation passed after delayed-renderer injection changes:
    - `rustfmt --edition 2024 --check components\paint\paint.rs components\paint\painter.rs`
    - `git diff --check`
    - `cargo check -p servoshell` after loading `etc\multigpu\servo_env.ps1`
    - `cargo build -p servoshell` after loading `etc\multigpu\servo_env.ps1`
    - `cargo test -p servoshell wall_layout --lib` passed 3 tests.
  - Still pending: interactive scroll/animation visual verification, direct per-GPU swapchain validation on real multi-GPU output hardware, and Phase 6 stress pages.

### Next Phase Gate

scroll/animation/input??monitor 寃쎄퀎?먯꽌 ?쒓컖?곸쑝濡??숆린?붾릺?댁빞 Phase 6?쇰줈 吏꾪뻾?쒕떎.

---

## Phase 6. Web Content Stress Cases

### Goal

?ㅼ젣 釉뚮씪?곗? 肄섑뀗痢좎뿉??tile 遺꾩궛 ?뚮뜑留곸쓽 correctness? ?쒓퀎瑜?寃利앺븳??

### Tasks

- [x] ???dashboard HTML ?뚯뒪???섏씠吏瑜?留뚮뱺??
- [x] CSS transform, opacity, filter, blur, shadow stress case瑜?留뚮뱺??
- [x] fixed/sticky/overflow/iframe/canvas ?뚯뒪?몃? 留뚮뱺??
- [x] WebGL/WebGPU canvas媛 tile 寃쎄퀎??嫄몄튂??寃쎌슦瑜?寃利앺븳??
- [x] video element媛 tile 寃쎄퀎??嫄몄튂??寃쎌슦瑜?寃利앺븳??

### Verification

- [ ] static dashboard媛 2x2 wall?먯꽌 ?뺤긽 ?쒖떆?쒕떎.
- [ ] animation怨?transform??tile 寃쎄퀎?먯꽌 ?딄린吏 ?딅뒗??
- [ ] canvas媛 tile 寃쎄퀎?먯꽌 clip/render?쒕떎.
- [ ] unsupported case??紐낇솗??fallback ?먮뒗 ?쒗븳 ?ы빆?쇰줈 ?쒖떆?쒕떎.

### Phase Result

- Status: `Implementation Complete With Visual Verification Pending`
- Notes:
  - Comprehensive Phase 6 stress page added at `tests/html/multigpu_wall_stress_cases.html`.
  - The page covers a wide static dashboard, CSS transform/opacity/filter/blur/shadow content, fixed and sticky elements, an overflow scroller, an iframe, 2D canvas, WebGL canvas, WebGPU availability/fallback, and a video element driven by `canvas.captureStream()` when available.
  - The stress content uses explicit x/y boundary markers for the local 3x1 wall and the target 2x2 wall, with animated and media elements crossing tile boundaries.
  - Browser sanity check through local HTTP confirmed the page loads, animation frames advance, auto-scroll runs, WebGL is available, WebGPU availability is reported, and the generated video path works in the browser used for the check.
  - Important media limitation discovered after the initial Phase 6 smoke: Servo's `HTMLCanvasElement.captureStream()` implementation is currently stub-like and should not be used as the primary Servo WebRTC validation input. The Phase 6 generated-video case is useful for browser/page sanity, but it does not prove Servo WebRTC video handling.
  - Automated 8-second ServoShell smoke run passed with `etc/multigpu/config/wall_layout.example_3x1.json`:
    - Command used `--wall-layout etc\multigpu\config\wall_layout.example_3x1.json --wall-all-tiles tests/html/multigpu_wall_stress_cases.html`.
    - Log saved to `servo\servoshell_wall_stress_smoke.err.log`.
    - Process stayed alive until the smoke harness stopped it.
    - 290 wall logical frames were generated.
    - 290 `Wall frame metadata` entries reported `scroll_offsets=matched`.
    - 218 wall frame barriers completed before deadline; no barrier misses were logged.
    - No skipped repaint targets, metadata mismatches, panics, error diagnostics, missed-frame logs, pending-frame warnings, or frame-ready-without-pending diagnostics were logged.
    - Headed-window presents were balanced: tile 0 = 248, tile 1 = 248, tile 2 = 248.
  - Still pending: interactive visual verification on the local 3x1 wall and the target 2x2 multi-GPU wall, especially shadow/blur continuity, iframe/media clipping, and WebGL/video boundary behavior.

### Next Phase Gate

二쇱슂 stress case 寃곌낵媛 臾몄꽌?붾릺怨? v1 吏???쒖쇅 踰붿쐞媛 ?뺤젙?섏뼱??Phase 7濡?吏꾪뻾?쒕떎.

---

## Phase 7. Performance Measurement And Tuning

### Goal

?⑥씪 GPU ?鍮?multi-GPU tiled present 援ъ“???깅뒫 ?대뱷怨?蹂묐ぉ??痢≪젙?쒕떎.

### Tasks

- [x] ?⑥씪 GPU/?⑥씪 monitor baseline??痢≪젙?쒕떎.
- [ ] 2 GPU/2 monitor ?깅뒫??痢≪젙?쒕떎.
- [ ] 4 GPU/4 monitor ?깅뒫??痢≪젙?쒕떎.
- [ ] GPU蹂?render time, CPU scene build time, frame barrier wait time??湲곕줉?쒕떎.
- [x] tile蹂?workload imbalance瑜?遺꾩꽍?쒕떎.
- [x] overlapPx 媛믪뿉 ?곕Ⅸ ?덉쭏/?깅뒫 tradeoff瑜?痢≪젙?쒕떎.

### Verification

- [x] ?숈씪 肄섑뀗痢?湲곗? frame time 鍮꾧탳?쒓? ?묒꽦?쒕떎.
- [ ] GPU蹂?utilization 濡쒓렇媛 ?섏쭛?쒕떎.
- [x] barrier wait time??蹂묐ぉ?몄? ?뺤씤?쒕떎.
- [ ] 沅뚯옣 overlapPx 湲곕낯媛믪씠 ?뺤젙?쒕떎.

### Phase Result

- Status: `In Progress`
- Notes:
  - Phase 7 performance notes added at `docs/multigpu/multigpu_phase7_performance_notes.md`.
  - Wall performance analyzer added at `etc/multigpu/tools/wall_perf_analyzer/analyze_wall_perf.py`.
  - Local 1x1 baseline config added at `etc/multigpu/config/wall_layout.example_1x1.json`.
  - Local 3x1 overlap comparison configs added at `etc/multigpu/config/wall_layout.example_3x1_overlap0.json` and `etc/multigpu/config/wall_layout.example_3x1_overlap64.json`.
  - Automated 6-second stress-page measurements completed:
    - 1x1 baseline: `servo\servoshell_wall_perf_1x1.err.log`.
    - 3x1 overlap 0: `servo\servoshell_wall_perf_3x1_overlap0.err.log`.
    - 3x1 overlap 32: `servo\servoshell_wall_perf_3x1_overlap32.err.log`.
    - 3x1 overlap 64: `servo\servoshell_wall_perf_3x1_overlap64.err.log`.
  - Local 3x1 results had 0 metadata mismatches, 0 barrier misses, 0 skipped repaint targets, and balanced or near-balanced tile present counts.
  - Measured 3x1 steady-state p95 values:
    - overlap 0: render p95 8.066 ms, present p95 6.441 ms, barrier request-to-all-ready p95 8.592 ms.
    - overlap 32: render p95 9.568 ms, present p95 6.285 ms, barrier request-to-all-ready p95 8.191 ms.
    - overlap 64: render p95 8.803 ms, present p95 6.098 ms, barrier request-to-all-ready p95 8.273 ms.
  - Current local recommendation remains overlap 32 px because it preserves the v1 artifact guard band while staying in the same p95 timing range as 0/64 px.
  - Still pending: release-build measurements, GPU utilization capture, 2 GPU/2 monitor and 4 GPU/4 monitor target hardware measurements, CPU scene-build timing, and longer steady-state runs that exclude startup/warmup spikes.
  - WebRTC/video multi-GPU investigation documented at `docs/multigpu/multigpu_webrtc_video_notes.md`.
  - Current media conclusion: Servo can process WebRTC/video conditionally through GStreamer and `<video srcObject>`, but v1 does not distribute media decode across GPUs. The tiled renderer can handle the resulting video element as page content if the media stream reaches WebRender successfully.

### Next Phase Gate

?깅뒫 痢≪젙 寃곌낵瑜?湲곕컲?쇰줈 v1 湲곕낯 ?ㅼ젙怨?沅뚯옣 ?섎뱶?⑥뼱 援ъ꽦???뺤젙?섏뼱??Phase 8濡?吏꾪뻾?쒕떎.

---

## Phase 8. V1 Hardening

### Goal

v1 ?쒖뿰怨??대? 寃利앹뿉 ?꾩슂???덉젙?깆쓣 ?뺣낫?쒕떎.

### Tasks

- [ ] invalid topology ?ㅼ젙??????ㅻ쪟 泥섎━瑜??뺣━?쒕떎.
- [ ] monitor unplug/reorder ?곹솴??fallback ?뺤콉???뺤쓽?쒕떎.
- [ ] renderer crash ?먮뒗 GPU device loss ?곹솴??蹂듦뎄 ?뺤콉???뺤쓽?쒕떎.
- [ ] logs? diagnostics瑜??뺣━?쒕떎.
- [ ] v1 limitations 臾몄꽌瑜??묒꽦?쒕떎.

### Verification

- [ ] ?섎せ???ㅼ젙 ?뚯씪?먯꽌 紐낇솗???ㅻ쪟媛 異쒕젰?쒕떎.
- [ ] monitor 媛쒖닔媛 遺議깊븷 ??fallback ?먮뒗 醫낅즺 ?뺤콉???쇨??쒕떎.
- [ ] GPU renderer ?ㅽ뙣媛 ?꾩껜 process crash濡??댁뼱吏吏 ?딅룄濡?泥섎━?쒕떎.
- [ ] v1 demo ?덉감 臾몄꽌媛 ?묒꽦?쒕떎.

### Phase Result

- Status: `Not Started`
- Notes:

### V1 Completion Criteria

- [ ] 2x2 4K monitor wall?먯꽌 ?섎굹??8K virtual webpage瑜??쒖떆?쒕떎.
- [ ] 媛?GPU媛 ?먭린 monitor??吏곸젒 present?쒕떎.
- [ ] scroll, animation, input??monitor 寃쎄퀎?먯꽌 ?쒓컖?곸쑝濡??숆린?붾맂??
- [ ] 理쒖쥌 ?⑹꽦 ?꾨떞 GPU ?놁씠 ?숈옉?쒕떎.
- [ ] tile 寃쎄퀎??shadow, blur, antialiasing artifact媛 overlap 湲곕낯媛믪쑝濡??덉슜 ?섏?源뚯? 以꾩뼱?좊떎.
- [ ] topology, frame timing, missed frame diagnostics瑜??뺤씤?????덈떎.

---

## V1 Explicit Non-Goals

- Chromium compatibility ?섏????꾩쟾??web compatibility
- GPU 媛?texture sharing 湲곕컲 workload stealing
- ?⑥씪 monitor?먯꽌 ?щ윭 GPU媛 ??swapchain??怨듬룞 present?섎뒗 援ъ“
- hardware genlock 蹂댁옣
- DRM/protected video 理쒖쟻??- OS overlay plane 理쒖쟻??- 紐⑤뱺 WebGPU/WebGL edge case ?꾩쟾 吏??

Additional media non-goals for v1:

- Per-GPU WebRTC/video decode distribution.
- Cross-GPU video texture sharing.
- DRM/protected video and OS overlay-plane optimization.
- Treating `canvas.captureStream()` as a complete Servo WebRTC video test source.

## Reboot Resume Notes - 2026-06-07

### Current Work State

- Main working directory: `D:\2_TechReview\20260606_multigpu_browser`.
- Servo repository: `D:\2_TechReview\20260606_multigpu_browser\servo`.
- Current implementation status:
  - Phase 6 stress content implemented and ServoShell 3x1 smoke passed.
  - Phase 7 local debug performance pass completed for 1x1 and 3x1 overlap 0/32/64.
  - Phase 8 hardening has not started.
  - WebRTC/video capability review is documented, but a dedicated Servo WebRTC/video wall probe still needs to be added.

### Added Or Updated Artifacts

- `tests/html/multigpu_wall_stress_cases.html`
- `etc/multigpu/config/wall_layout.example_1x1.json`
- `etc/multigpu/config/wall_layout.example_3x1_overlap0.json`
- `etc/multigpu/config/wall_layout.example_3x1_overlap64.json`
- `etc/multigpu/tools/wall_perf_analyzer/analyze_wall_perf.py`
- `docs/multigpu/multigpu_phase7_performance_notes.md`
- `docs/multigpu/multigpu_webrtc_video_notes.md`

### Useful Existing Logs

- `servo/servoshell_wall_stress_smoke.err.log`
- `servo/servoshell_wall_perf_1x1.err.log`
- `servo/servoshell_wall_perf_3x1_overlap0.err.log`
- `servo/servoshell_wall_perf_3x1_overlap32.err.log`
- `servo/servoshell_wall_perf_3x1_overlap64.err.log`

### Verified Before Reboot

- `python -m py_compile etc\multigpu\tools\wall_perf_analyzer\analyze_wall_perf.py`
- `python -m json.tool` on the new wall layout JSON files.
- `git diff --check` in `servo`.
- Local browser sanity check for `tests/html/multigpu_wall_stress_cases.html`.
- ServoShell 3x1 stress smoke had 0 metadata mismatches, 0 barrier misses, and balanced tile presents.

### Recommended Next Commands

From repository root:

```powershell
cd D:\2_TechReview\20260606_multigpu_browser\servo
git status --short
python .\etc\multigpu\tools\wall_perf_analyzer\analyze_wall_perf.py servoshell_wall_perf_3x1_overlap32.err.log --format markdown
```

To rerun the current 3x1 stress smoke manually:

```powershell
target\debug\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.example_3x1.json --wall-all-tiles tests/html/multigpu_wall_stress_cases.html
```

To start the next development item:

```text
Add tests/html/multigpu_wall_webrtc_video_probe.html and test it with --pref dom_webrtc_enabled=true.
```

### Next Priority

1. Add a dedicated Servo WebRTC/video wall probe that does not rely on `canvas.captureStream()`.
2. Start Phase 8 hardening: invalid topology errors, monitor mismatch fallback, renderer/device-loss policy, diagnostics cleanup.
3. Run release-build performance measurements.
4. Validate direct per-GPU present and performance on 2 GPU/2 monitor or 4 GPU/4 monitor target hardware.

## Current Defaults

- Engine: Servo
- Initial OS: Windows
- Initial wall layout: 2x2
- Initial monitor recommendation: same resolution, same refresh rate
- Initial overlap: 32 px
- Missed frame policy: keep previous frame for the delayed tile
- Cross-GPU copy: disabled in v1

## Update - 2026-06-08 2x1 Dual-GPU Target Validation

### Target Hardware Layout

- GPU count in use: 2
- Display layout:
  - Tile 0 / GPU 0 / monitor 0: `(0, 0) - (1920, 1080)`
  - Tile 1 / GPU 1 / monitor 1: `(1920, 0) - (3840, 1080)`
- Layout file: `etc/multigpu/config/wall_layout.example_2x1_dualgpu.json`
- Test path that matches the project goal: one ServoShell process with `--wall-all-tiles`.
- Non-goal test path: separate ServoShell processes per tile. That path can make both monitors move, but it does not validate one logical page, shared scene fan-out, or synchronized frame presentation.

### Implementation Adjustments

- Wall-layout headed windows are now borderless, fixed-size tile windows, without using fullscreen.
- Wall-layout startup sizes each window from the selected tile rect.
- The desktop embedder now tracks animating `WebView`s and keeps the Servo event loop spinning while animation is active.
- Wall-all-tiles repaint release now directly repaints/presents the headed tile group on the main thread instead of relying only on later OS `RedrawRequested` delivery.
- The validated path remains one logical `WebView` with multiple paint targets. Primary target renders on requested GPU 0; secondary target renders on requested GPU 1.

### Latest Validation Run

Command shape:

```powershell
target\debug\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.example_2x1_dualgpu.json --wall-all-tiles tests/html/multigpu_wall_sync_probe.html
```

Latest log:

```text
servo\servoshell_wall_2x1_dualgpu_sync_goal_direct_20260608_161425.err.log
```

Summary:

- Logical wall frames: `2288`
- Metadata matched/mismatched: `2288 / 0`
- Barrier completed before deadline / missed: `2286 / 2`
- Repaint skips: `2`, both from the missed-barrier keep-previous-frame policy.
- Panic/error diagnostics: `0 / 0`
- Tile window presents:
  - Tile 0 / GPU 0: `1271`
  - Tile 1 / GPU 1: `1270`
- Repaint targets:
  - Primary / requested GPU 0: `1273`
  - Secondary / requested GPU 1: `1272`
- Requested GPU diagnostics:
  - `requested_gpu=Some(0)`: `14252`
  - `requested_gpu=Some(1)`: `14249`
- NVIDIA utilization samples during the run:
  - GPU 0: avg `35.40%`, max `44%`
  - GPU 1: avg `32.50%`, max `42%`

Interpretation:

- The shared logical scene is fan-out rendered to two paint targets.
- The two paint targets use the same logical frame metadata and matched scroll offsets.
- The software barrier usually releases both targets together.
- When a target misses the deadline, the delayed target keeps the previous frame; the run did not show mixed-current-frame presentation.
- Both GPU paths are active and both tile windows present repeatedly.

### What Is Currently Proven

- One logical Servo page can drive multiple tile paint targets.
- Each tile renderer receives the same logical frame id and matched frame metadata.
- The Windows ANGLE path selects the requested DXGI adapter indices for GPU 0 and GPU 1.
- The application-level tiled renderer does not intentionally perform cross-GPU texture sharing or cross-GPU workload stealing.
- The current software synchronization policy prevents presenting a delayed tile's late frame as if it belonged to the current synchronized group.
- Borderless fixed-size tile windows avoid native border/titlebar crossing the GPU/display boundary.

### What Is Not Yet Proven

- OS/DWM/driver-level composition may still copy or migrate surfaces internally. The current Servo logs cannot prove absence of all OS-level GPU copies.
- The result is not hardware genlock. Present timing is software-barrier based.
- The result has been validated on the 2x1 dual-GPU setup, not yet on the intended 2x2 4K wall.
- Release-build performance is not measured for this 2x1 dual-GPU target.
- Long steady-state runs are still needed to quantify barrier miss rate and jitter.
- Visual pass/fail for seam continuity, blur/shadow overlap, iframe/media clipping, and WebGL/video boundary behavior still needs manual confirmation on the target wall.

### Remaining Work

1. Capture ETW/GPUView or PresentMon evidence to determine whether Windows DWM or the driver performs GPU-to-GPU copies after Servo presents each tile window.
2. Run a longer 2x1 dual-GPU sync test, excluding startup and shutdown, and record barrier miss rate, p95/p99 render time, p95/p99 present time, and GPU utilization.
3. Repeat the same validation with a release build.
4. Run the Phase 6 stress page on the 2x1 dual-GPU hardware and record visual seam results.
5. Validate the intended 2x2 4K wall layout once hardware is available.
6. Add an automated log gate for target validation:
   - metadata mismatch must be `0`
   - panic/error diagnostics must be `0`
   - tile present spread must stay within a configured threshold
   - skipped repaint must be explained by missed-barrier keep-previous policy
   - requested GPU counts must include every configured tile GPU
7. Harden topology errors: missing monitor, monitor reorder, invalid GPU index, mixed DPI, and monitor unplug/replug.
8. Define device-loss and renderer-failure policy for one tile without crashing the whole wall.
9. Add a dedicated WebRTC/video wall probe that does not rely on `canvas.captureStream()`.
10. Clean up diagnostics and separate startup/warmup spikes from steady-state metrics in the analyzer.
