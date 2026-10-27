# Strip inventory — `components/servo/*.rs` + `components/shared/*` + Cargo.toml features

Repo: `W:\servo_multigpu-tiled-wall`, branch `wall-strip-instrumentation`, HEAD `414c77e7e19`.
Read-only pass. No code edited.

**Method.** Upstream baseline is the local branch `study/servo-base` (`92c8770e55d`, pure upstream
Servo). Every verdict below is backed by `git diff study/servo-base HEAD -- <file>`; anything that
the diff does not touch is upstream and is marked KEEP/upstream without further argument. Every
candidate symbol was grepped across the whole repo (`components/`, `ports/`, `tests/`,
`vendor_local/`, `third_party/`, `etc/`) and every caller is listed. All line numbers below were
read out of the current working tree with `awk`/`sed` and re-verified against a second read.

---

## A. `components/servo/` (excluding `examples/winit_wall/`)

The **entire** fork delta to `components/servo/*.rs` is: the SPINSLOW block (commit `b7d176e3fd4`),
`Servo::on_demand_wall_tick` + doc comments (commit `37bb6a06726`), the `WebViewPaintTarget` /
viewport-override / wall-tick API on `webview.rs`, and re-export widening in `lib.rs`.
`webview.rs` contains **no** instrumentation — every added symbol there is a rendering API.

### A.1 REMOVE

| symbol | file:line-range | kind | verdict | every caller / reader | notes |
|---|---|---|---|---|---|
| `spin_start` | `components/servo/servo.rs:273` | `let` local (`Instant`) | REMOVE | read once at `servo.rs:346` (`spin_ms`) | nothing else |
| `paint_messages` | `components/servo/servo.rs:274` (decl), `:287` (assign) | `let mut` counter | REMOVE | read once, in the `warn!` at `servo.rs:348-350` | `messages.len()` is otherwise unused |
| `embedder_messages` | `components/servo/servo.rs:275` (decl), `:308` (`+= 1`) | `let mut` counter | REMOVE | read once, in the `warn!` at `servo.rs:348-350` | — |
| `paint_handle_ms` + its block form | `components/servo/servo.rs:276` (`let paint_handle_ms = {`), `:288` (`let start = Instant::now();`), `:290` (`start.elapsed()…`), `:291` (`};`) | block turned into an expression | REMOVE | read once, in the `warn!` at `servo.rs:348-350` | **This is the one edit that is not a pure line delete.** Lines 277–286 and 289 (`paint.handle_messages(messages);`) must survive. Restore to the upstream shape: bare `{` at 276, `}` at 291 — see §Removal order |
| `embedder_start` / `embedder_ms` | `components/servo/servo.rs:292`, `:313` | `let` locals | REMOVE | `embedder_ms` read once, in the `warn!` at `:348-350` | — |
| `updates_start` / `perform_updates_ms` | `components/servo/servo.rs:341`, `:343` | `let` locals | REMOVE | `perform_updates_ms` read once, in the `warn!` at `:348-350` | `self.paint.borrow_mut().perform_updates();` at `:342` must survive |
| `spin_ms` + the SPINSLOW gate | `components/servo/servo.rs:346-351` | `let` + `if` + `warn!` | REMOVE | nothing reads `spin_ms` outside the gate | the whole `if` block goes |
| the SPINSLOW rationale comment | `components/servo/servo.rs:268-272` | comment | REMOVE | — | describes only the measurement |
| the 30 ms-threshold comment | `components/servo/servo.rs:344-345` | comment | REMOVE | — | — |
| `Instant` in the import | `components/servo/servo.rs:9` | `use std::time::{Duration, Instant};` | REMOVE (narrow to `Duration`) | `Instant` appears in `components/servo/` at exactly four sites: `servo.rs:273, 288, 292, 341` — all four are in the list above. `Duration` is still used | **build-breaking if forgotten**: `unused_imports` is `deny` in this workspace's CI profile |
| `TileRenderTiming` (re-export only) | `components/servo/lib.rs:48` | `pub use` item | REMOVE *iff* `paint::TileRenderTiming` goes | re-export readers: **none outside the crate graph**. The type itself is referenced at `components/paint/paint.rs:344, 357, 371, 3644` and `components/paint/lib.rs:21` only. `grep -rn 'TileRenderTiming\|queued_ms\|render_ms' ports/` → only `ports/servoshell/window.rs`'s *own* unrelated `render_ms` local | see Trap T-2 — `PaintTargetsInFlight::join()` is a **barrier**, not instrumentation |
| `note_frame_time` (re-export only) | `components/servo/lib.rs:49` | `pub use` item | REMOVE *iff* `paint::frame_time::note_frame_time` goes | exactly one caller in the repo: `components/servo/examples/winit_wall/main.rs:1609` | see Trap T-3 — `frame_time.rs` is **not** wholly instrumentation |

### A.2 KEEP

| symbol | file:line-range | kind | verdict | every caller / reader | notes |
|---|---|---|---|---|---|
| `Servo::on_demand_wall_tick` | `components/servo/servo.rs:1123-1125` (fn; doc `1105-1122`) | pub fn | KEEP | `components/servo/tests/common/mod.rs:95` (inside `ServoTest::spin`, right after `spin_event_loop()` at `:94`). Without it `ServoTest::spin` produces one bootstrap frame and then nothing (measured: 19 timeouts → 0) | frame production, not measurement |
| `Servo::setup_logging`, `BothLogger`, `FromEmbedderLogger` wiring | `servo.rs:1127-1138`, `1327-1349`, `1351-1356` | fn / struct / impl | KEEP — **upstream** | unchanged vs `study/servo-base` | the task listed these as suspects; the diff shows zero fork delta |
| tracing-perfetto layer | `ports/servoshell/lib.rs:59-110`, feature at `ports/servoshell/Cargo.toml:58,85`, workspace dep `Cargo.toml:262` | optional tracing layer | KEEP — **upstream** | unchanged vs `study/servo-base` | off by default (cargo feature `tracing-perfetto`) |
| all of `components/servo/webview.rs` fork additions — `WebViewPaintTarget` (`:84-88`), `on_wall_tick` (`:699`), `on_demand_wall_tick` (`:709`), `primary_paint_target`, `add_paint_target`, `add_paint_target_on_own_thread`, `dispatch_paint_targets` (`:786`), `paint_target`, `flush_deferred_dcomp_commits`, `commit_deferred_dcomp_at_tick`, `paint_target_keep_previous_logical_frame`, `update_paint_target` (`:830`), `viewport_size_override` (`:1185`), `viewport_origin_override` (`:1194`) | `webview.rs` | API | KEEP | winit_wall + servoshell | every one of these changes pixels or timing. The whole `webview.rs` fork delta was read against `study/servo-base`: it contains **no** timing or counter code |
| `components/servo/examples/winit_minimal.rs` `spin_event_loop_and_wall_tick` | `winit_minimal.rs:49-79`, call sites `:140`, `:151` | fn | KEEP | this example | without it the example renders one frame and freezes |
| `dwm_composition_grid`, `wait_primary_vblank` (re-exports) | `components/servo/lib.rs:49-50` | `pub use` | KEEP | `winit_wall/main.rs:1235` and `:1418` respectively | both feed the pacing clock's sleep target — a timing decision |
| `note_present_tick` (re-export) | `components/servo/lib.rs:49` | `pub use` | KEEP | `winit_wall/main.rs:1613` (and `main.rs:869`/`1170`/`1578` via the shell's own wrapper) | **not removable.** `paint::note_present_tick` calls `wall_tick::note_tick_edge()`, which is the tick-edge counter that `ExternalPresentBudget` and `commit_deadline()` are built on. Only its second statement (`output_grid::note_present_tick_now()`) is instrumentation, and that lives in a covered area |
| `schedule_wall_production` (re-export) | `components/servo/lib.rs:49` | `pub use` | KEEP (dead API, not instrumentation) | **zero callers repo-wide** — only the definition at `components/paint/lib.rs:125-127` and a mention in a `webview.rs:703` doc comment; `paint/lib.rs:121-124` documents the emptiness itself | it is an unused public door for demand-driven embedders, not a probe. Deleting it is an API decision, not a strip decision |
| `removed_env` (re-export) | `components/servo/lib.rs:68` | `pub use` | KEEP | `winit_wall/main.rs:251`, `ports/servoshell/desktop/cli.rs:42`, `components/config/tests/config_surface.rs` (7 sites) | `check_or_exit()` **exits the process** on a retired env var — behaviour, not a print |
| `config_dump` (re-export) | `components/servo/lib.rs:68` | `pub use` | UNCERTAIN → see §Uncertain | `winit_wall/main.rs:1922`, `ports/servoshell/desktop/app.rs:98` | — |
| `PaintTargetsInFlight`, `RenderingContextFactory`, `WebRenderDebugOption`, `DisplayTopology`, `dxgi_luid_for_gpu_index`, `enumerate_display_topology`, `spatial_order`, `wall_args`, `wall_layout`, `WebViewPaintTarget` re-exports | `components/servo/lib.rs:47-56`, `:91` | `pub use` | KEEP | winit_wall / servoshell | multi-GPU placement and dispatch |

---

## B. `components/shared/`

### B.1 REMOVE

| symbol | file:line-range | kind | verdict | every caller / reader | notes |
|---|---|---|---|---|---|
| **whole module** `interval_summary` | `components/shared/base/interval_summary.rs:1-185` (entire file) | module | REMOVE *iff* all four readers go | `components/paint/painter.rs:4966` (`format_intervals`, inside `pub(crate) fn format_step_sequence`, `painter.rs:4965-4967`, whose only consumer is the `ANIMSTEPSEQ` log line); `components/paint/paint.rs:130` (`percentiles`, feeds only the `PAINTERLOCK` `warn!` at `paint.rs:132-137`); `components/paint/frame_time.rs:14,167,168` (`summarize_intervals`+`percentiles`, feed only the `FRAMETIME` log); `components/script/raf_clock.rs:31,111` (`summarize_intervals`, feeds only the `RAFCLOCK` `warn!` at `raf_clock.rs:114-125`). **Four readers, all in covered areas, all log-only.** | file is self-contained: `format_intervals`, `Percentiles`, `percentiles`, `Intervals`, `summarize_intervals`, plus 8 unit tests. Nothing else in `shared/base` touches it |
| `pub mod interval_summary;` | `components/shared/base/lib.rs:15` | mod decl | REMOVE with the file | — | single line; neighbours are `pub mod id;` (14) and `pub mod print_tree;` (16) |
| `PaintMessage::kind()` and its `impl` block | `components/shared/paint/lib.rs:298-332` (`impl PaintMessage {` at 298 … `}` at 332; doc at 299-300, fn 301-331) | inherent impl | REMOVE | **exactly one caller repo-wide**: `components/paint/paint.rs:3842` (`let kind = message.kind();`) inside the `kinds` histogram at `paint.rs:3838-3846`, which is read only by that function's slow-drain log | `impl Debug for PaintMessage` starts at `lib.rs:334` and uses `strum`'s `Into<&'static str>`, **not** `kind()` — do not delete it by accident |
| the `kinds` histogram that consumes it | `components/paint/paint.rs:3838-3846` (`drain_start`, `messages_seen`, `kinds` vec + loop) | locals | REMOVE (covered area — listed here only to close the caller chain) | `kinds`/`messages_seen`/`drain_start` are read only by the drain-summary log later in `handle_messages` | flagged so the paint-area owner and this one do not each assume the other does it |
| `D3D11_TEXTURES_RELEASED` | `components/shared/paint/rendering_context.rs:63-65` (doc 63, static 64-65) | `pub static AtomicU64` | REMOVE *iff* its one reader goes | writer: `rendering_context.rs:1186` (`fetch_add` inside `release_d3d11_texture`, fn at `:1178-1195`). reader: **exactly one** — `components/media/media-thread/lib.rs:1315-1316`, inside the `MEDIARINGS` `warn!` (`lib.rs:1309-1317`). Also named in a comment at `components/media/backends/gstreamer/render-d3d11/ring_producer.rs:249` (comment only, no code) | **the `Release()` call at `rendering_context.rs:1187-1189` is functional and must stay.** Only line 1186 and the two comment lines 1184-1185 come out |
| `adapter_local_usage_bytes` | `components/shared/paint/rendering_context.rs:12-61` (doc 12-15, `cfg_attr` 16, fn 17-61) | `pub fn` | REMOVE *iff* both readers go | **TWO readers, both in `components/webgl/webgl_thread.rs`** — `:1599` (building the `before` vec) and `:1639` (building `after` inside `deltas`). Both feed only the `WEBGLRECLAIM` `warn!` at `webgl_thread.rs:1651`. **`components/webgl/` is in no covered inventory** — see §D | this is the same shape as the `WEBGL_FANOUT_PROF` miscount the task warns about; the second reader is inside a closure and is easy to miss |
| `WEBGLRECLAIM` instrumentation block (the caller side) | `components/webgl/webgl_thread.rs:1590` (`let released`), `:1591-1602` (comment + `before`), `:1603-1609` (`devices`), `:1631-1651` (`mb`, `deltas`, `warn!`) | locals + log | REMOVE (uncovered area — listed to close the chain) | nothing outside the `warn!` reads `released`, `before`, `devices`, `mb`, `deltas` | the release loop at `:1610-1627` and `self.context_backends.remove` / `released_contexts.insert` at `:1628-1630` are **functional** |

### B.2 KEEP

| symbol | file:line-range | kind | verdict | every caller / reader | notes |
|---|---|---|---|---|---|
| `paint_api::render_frame` (`begin_render_frame`, `current_render_frame`) | `components/shared/paint/render_frame.rs:1-31` (whole file) | thread-local frame counter | **KEEP** | writer `components/paint/painter.rs:2428`; readers `components/media/media-thread/lib.rs:911` (`may_initialize_ring_now` — gates `media_ring_init_max_per_frame`) and `:1044` (`roll_wrap_budget_frame` — resets the wrap budget) | **highest-risk false positive in my area.** It reads exactly like a frame-number probe but it is the denominator of two per-frame admission budgets. Deleting it makes ring init and texture wrapping unbounded per frame |
| `angle_gl_lock` (`:93`), `ANGLE_GL_EXCLUSION` (`:129`), `ANGLE_GL_DEVICE_LOCKS` (`:132-133`), `AngleGlGuard` (`:137-141`) | `components/shared/paint/lib.rs:54-141` | per-device lock | KEEP | `components/webgl/webgl_thread.rs` (`angle_gl_lock` at the surface loop) and paint call sites | the `log::warn!` inside `angle_gl_lock` fires once per D3D11 device and is the only evidence the keying resolved; it is a correctness canary, not a counter. Removing the lock would reintroduce 0xC0000005 |
| `report_unknown_external_image` | `components/shared/paint/lib.rs:896-920` (doc 896-901, fn 902-920) | fn + `AtomicU32 SEEN` | KEEP | `lib.rs:946` (lock) and `lib.rs:991` (unlock) | it is an **error-path** report on a path that used to panic the main thread. Not performance/statistics instrumentation. The `SEEN` counter exists to rate-limit, not to report a number |
| `set_dcomp_mode` / `DCOMP_MODE_INVALID_WARNED` `eprintln!` | `components/shared/paint/rendering_context.rs:148-181` | config normalisation + one-shot warning | KEEP | `winit_wall/main.rs`, `ports/servoshell` | the `Once` exists to stop a per-frame log flood; the normalisation itself is behaviour |
| `wall_layout.rs` `eprintln!`s | `components/shared/paint/wall_layout.rs:228-243, 302-…` | config warnings | KEEP | CLI parse | they report that a user-requested layout did not happen |
| `rendering_context.rs` `warn!`/`info!` at `:472, 476, 486, 1048, 1107, 1147, 1283-1298, 1737, 1756, 1761, 2021` | — | error/diagnostic logs | KEEP | — | all failure-path messages; none compute a statistic |
| `shared/paint/display_list.rs`, `wall_args.rs`, `wall_layout.rs` | — | fork modules | KEEP | — | grepped for `Instant`/`Atomic`/`elapsed`/`env::var`: no perf instrumentation |
| `shared/embedder/lib.rs` `needs_paint_timer_heartbeat` | `:295-305` | trait method | KEEP | paint timer thread | drives whether paint emits wakeups — pure behaviour |
| `shared/layout/lib.rs` `MediaFrameYuvFormat` / `MediaFrameYuvImage` / `MediaFrame::image_keys` / `ReflowRequest::animation_frame_time` | `:234-287`, `:744-749` | types / field | KEEP | layout + display list + script | `animation_frame_time` sets the animation segment origin; removing it shifts animation phase |
| `shared/script/lib.rs` `TickAllAnimations(…, CrossProcessInstant)` | `:246-250` | message payload | KEEP | script thread | the frame time animations are ticked *for* |
| `shared/constellation/*`, `shared/canvas/webgl.rs`, `shared/webgpu/*` | — | fork deltas | KEEP | — | grepped: no timing/counter code |
| `shared/base/*` other than `interval_summary` | — | — | KEEP — **upstream** | — | `cross_process_instant`, `generic_channel`, `id`, `print_tree`, `rope`, `text`, `threadpool` all unchanged vs base |

---

## C. Cargo.toml — dependencies and features that exist only for instrumentation

| item | file:line-range | kind | verdict | every reader of the feature | notes |
|---|---|---|---|---|---|
| `"psapi"` | `components/paint/Cargo.toml:108` | winapi feature | REMOVE *iff* `painter.rs::process_memory_mb` goes | **one** use site repo-wide in this crate: `components/paint/painter.rs:823` (`use winapi::um::psapi::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};`) inside `fn process_memory_mb` (`painter.rs:821-842`, with a non-Windows stub at `:843`) | `painter.rs` is a covered area. The feature line is in my area; the trigger is theirs |
| `"dxgi1_4"` | `components/paint/Cargo.toml:91` | winapi feature | REMOVE *iff* `painter.rs::log_gpu_memory` goes | **one** use site in this crate: `components/paint/painter.rs:753` inside `fn log_gpu_memory` (`painter.rs:750-815`, non-Windows stub at `:816`); the `QueryVideoMemoryInfo` calls are at `:784` and `:786` | `"dxgi"` and `"dxgi1_2"` are used elsewhere in paint and stay |
| `"dxgi1_4"` | `components/shared/paint/Cargo.toml:53` | winapi feature | REMOVE *iff* `adapter_local_usage_bytes` goes | **one** use site in non-example code: `components/shared/paint/rendering_context.rs:22`. Examples under `components/shared/paint/examples/` use the separate dev-dependency winapi block (`Cargo.toml:63-108`), which does **not** list `dxgi1_4` | **this one was not in the brief.** The brief named only `components/paint/Cargo.toml`; `shared/paint` has its own `dxgi1_4` for the same reason |
| `"processthreadsapi"` | `components/paint/Cargo.toml:106` | winapi feature | **KEEP** | three use sites: `painter.rs:665` (`GetCurrentThread`/`GetThreadTimes` — instrumentation), `painter.rs:822` (`GetCurrentProcess` — instrumentation), **`painter.rs:904` (`GetCurrentThread`/`SetThreadPriority` — functional)** | keeping it is correct, and the reason is the third site only. If someone later strips `thread_cpu_ms` and `process_memory_mb` they must not then conclude the feature is unused |
| `"profileapi"` | `components/paint/Cargo.toml:107` | winapi feature | **KEEP** | `components/paint/dcomp_compositor.rs:57`, `components/paint/output_grid.rs:58` (QPC) | QPC underpins the pacing grid |
| `"dwmapi"` | `components/paint/Cargo.toml:87` | winapi feature | **KEEP** | `components/paint/dcomp_compositor.rs:56` (`DwmGetCompositionTimingInfo`) — feeds `composition_grid()` → `servo::dwm_composition_grid()` → the shell's sleep target | timing decision |
| `"winbase"` | `components/paint/Cargo.toml:115` | winapi feature | **KEEP** | `painter.rs:905` (`THREAD_PRIORITY_ABOVE_NORMAL`) | pairs with `SetThreadPriority` |
| `"d3dcompiler"` and the rest of the dcomp transitive closure | `components/paint/Cargo.toml:70-123` | winapi features | **KEEP** | `dcomp_video_convert.rs:39` etc. | winapi 0.3.9 features are empty arrays; the closure must stay intact or winapi does not compile |
| `time = { workspace = true }` | `components/paint/Cargo.toml:53` | dep (fork-added) | **KEEP** | `components/paint/frame_time.rs:47, 68` (inside the functional `stored_frame_time`/`fresh_frame_time`), `components/paint/web_content_animation.rs:996+` | looks strippable with the FRAMETIME log; it is not |
| `windows-sys` (Win32_Foundation / Diagnostics_Debug / Kernel / LibraryLoader) | `components/servo/Cargo.toml:169-176` | target dep (fork-added) | KEEP (conditional) | only `components/servo/examples/winit_wall/crash_report.rs` | a **crash** reporter (module+offset on an AV), not perf/statistics instrumentation. It becomes an unused dep only if `crash_report.rs` is deleted — that is a winit_wall-area call |
| `sdwall_device_auth` dev-dep | `components/servo/Cargo.toml:190` | dev-dep | KEEP | `components/servo/tests/` | functional |
| `gleam` → media-thread; `log` → media/player; `jxl-oxide` → pixels; `ipc-channel` → shared/canvas; `zeroize_derive` → shared/constellation; `postcard`/`servo-config`/`ipc-channel`/`no-wgl` → webgl; `bincode`/`wgpu-hal`/`windows` → webgpu; `paint_api` → servoshell | respective Cargo.tomls | deps | KEEP | — | checked each diff hunk; all functional, none instrumentation-only |
| `[patch.crates-io] webrender = { path = "vendor_local/webrender" }` | `Cargo.toml:426-431` | workspace patch | **REMOVE — but it is nobody's area yet.** See §D | the patch's own comment: *"로컬 계측 사본 … 느린 프레임에 한 줄(`WRSLOW`)을 더한 것뿐이다. 원인이 잡히면 되돌린다"* ("local instrumented copy … it only adds one `WRSLOW` line on slow frames; revert when the cause is found") | nine commits touch `vendor_local/webrender`, every one of them a probe (`wr(local): …`, `diag: WRRATE …`). Dropping the patch reverts to crates.io webrender 0.68.0 |
| `[patch.crates-io] surfman = { path = "third_party/surfman" }` and the `[patch."…/stylo"]` block | `Cargo.toml:432`, `:477-499` | workspace patches | KEEP | — | surfman carries multi-GPU/DComp changes; stylo carries a CSS-animation panic fix. Functional |

---

## D. SWEEP — directories under `components/` and `ports/` and who covered them

Covered so far: `paint/output_grid.rs`; `paint/dcomp_compositor.rs`; `paint/painter.rs`+`paint.rs`;
`paint/{commit_guard,frame_time,wall_tick,refresh_driver,web_content_animation,lib}.rs`;
`servo/examples/winit_wall/`; `script/`+`media/`+`layout/`+`allocator/`; `config` prefs+debug_env+launcher;
and this report (`servo/*.rs`, `shared/*`, Cargo.toml features).

### D.1 Directories with fork changes that **no inventory covers** — and that **do** hold instrumentation

| area | what is there | evidence |
|---|---|---|
| **`components/paint/comp_stats.rs`** (274 lines, whole file) | Pure telemetry module: `DCompositionGetFrameId` / `GetStatistics` / `GetTargetStatistics` delay-loaded via `GetProcAddress` to ask when a composited frame actually hit each panel. Its own header calls it "텔레메트리". | Consumers: **only** `components/paint/output_grid.rs:2265, 2304, 2455, 2456` (all inside `fn emit_comp_stats`, `output_grid.rs:2261-…`, called from `:2168`) and the `mod comp_stats;` decl at `components/paint/lib.rs:136`. Both are covered files — but the 274-line module itself was never inventoried, so a covered-area delete of `emit_comp_stats` leaves it orphaned (and `lib.rs:136` would then fail to compile if the file is deleted without the decl, or warn as dead code if the decl stays). |
| **`components/paint/webrender_external_images.rs`** (+438 vs base) | `WEBGL_FANOUT_PROF` gate (`:37-39`), a `profile` accumulator struct (`:42-…`), `ORPHANED_FRONT_BUFFERS: AtomicU64` (`:93`), per-call `Instant` probes at `:134, 148, 186, 407, 452, 465`, a 1-second window flush at `:333-…`. | **This is the file that makes `debug_env::WEBGL_FANOUT_PROF` a two-reader symbol.** Readers: `components/paint/webrender_external_images.rs:38` **and** `components/webgl/webgl_thread.rs:337`. Exactly the miscount the brief cites. Neither file is in a covered area. |
| **`components/webgl/`** (3 files changed) | `webgl_thread.rs` carries a large `SERVO_WEBGL_FANOUT_PROF` apparatus: gate `:336-339`, accumulator `:341`, field doc `:429`, gated branches/`Instant`s at `:545, 873, 1079, 1114, 1773, 1813, 1839, 1860, 1901`, window flush `:1908-…`, switch probes `:1998, 2024`; plus the `WEBGLRECLAIM` block (`:1590-1651`) that is the only consumer of `shared/paint`'s `adapter_local_usage_bytes`. | — |
| **`ports/servoshell/`** (12 files, +1535 vs base) | `window.rs:231-284` — `render_start`/`render_ms`/`present_start`/`present_ms` and the `Wall repaint target: …` `info!` / `Repaint target: …` `debug!` pair, all four locals log-only (`webview.paint_target()`, `webview.paint()`, `flush_deferred_dcomp_commits()`, `rendering_context.present()` between them are functional). `desktop/gui.rs:690-692` present timing. `desktop/app.rs:72, 345` `Instant` captures, and `app.rs:98` `config_dump::log_effective_config()`. Also `desktop/vsync_refresh_driver.rs` (+113, functional) and `running_app_state.rs` (+253). | — |
| **`vendor_local/webrender/`** (outside `components/`/`ports/`, reached via the root `[patch.crates-io]`) | A vendored WebRender 0.68.0 whose *entire* fork delta is instrumentation: `WRSLOW` breakdown at `src/renderer/mod.rs:1911`, resolve counting at `:5116`. Nine commits, all titled `wr(local)`/`diag`/`probe`. | The patch comment says explicitly it is to be reverted once the cause is found. |
| **`etc/multigpu/tools/{thread_cpu_probe,topology_probe}`** (outside `components/`/`ports/`) | Two standalone probe binaries added by the fork (new workspace-adjacent Cargo.tomls). | Not engine code; mention only so they are not forgotten. |

### D.2 Directories with fork changes that no inventory covers, but that **do not** hold perf instrumentation

`components/script_bindings` (build-script timing at `build.rs:16,41` is upstream),
`components/webgpu` (+`shared_present.rs`, error logs only), `components/devtools` (upstream
timeline actor), `components/pixels` (jxl decode), `components/net` (upstream cache/HTTP timing),
`components/fonts` (upstream shaping timer at `font.rs:463-481`), `components/constellation`,
`components/malloc_size_of`, `components/sdwall_device_auth` (timeouts, not probes),
`components/paint/{dcomp_video_convert,screenshot,webview_renderer,tracing}.rs` (grepped clean).

### D.3 Directories unchanged vs upstream — KEEP wholesale, reason "upstream"

`components/`: `background_hang_monitor`, `bluetooth`, `canvas`, `default-resources`,
`deny_public_fields`, `dom_struct`, `geometry`, `hyper_serde`, `jstraceable_derive`,
`malloc_size_of` (1 trivial hunk), `metrics`, `profile`, `servo_tracing`, `storage`, `timers`,
`url`, `wakelock`, `webdriver_server`, `webxr`, `xpath`.
`components/shared/`: `background_hang_monitor`, `bluetooth`, `devtools`, `fonts`, `net`,
`profile`, `storage`, `webxr`.

---

## Removal order and build-breakage notes

Order matters because a `static`/`fn`/re-export is only removable after its last reader.

1. **`components/servo/servo.rs` SPINSLOW — self-contained, do it first.**
   The only non-trivial step is turning the paint-drain block back into a statement block:
   line 276 `let paint_handle_ms = {` → `{`; delete 287 (`paint_messages = …`), 288
   (`let start = …`), 290 (`start.elapsed()…`); line 291 `};` → `}`. **Keep line 289
   (`paint.handle_messages(messages);`) and lines 277-286.** Then delete 268-275, 292, 308, 313,
   341, 343-351, and narrow the import at line 9 to `use std::time::Duration;`.
   `cargo check -p servo` alone catches a mistake here.

2. **`PaintMessage::kind()`** — delete the caller first (`components/paint/paint.rs:3838-3846`),
   then `components/shared/paint/lib.rs:298-332`. Reversing the order leaves `paint` not
   compiling. Do **not** touch `impl Debug for PaintMessage` at `lib.rs:334-338`.

3. **`interval_summary`** — four readers must go first, in any order:
   `components/script/raf_clock.rs`, `components/paint/frame_time.rs`,
   `components/paint/paint.rs:130`, `components/paint/painter.rs:4966`. Only then delete
   `components/shared/base/interval_summary.rs` and `components/shared/base/lib.rs:15`.
   `painter.rs:4966` is wrapped in `format_step_sequence`, so the paint owner must remove that
   wrapper too, not just the call.

4. **`D3D11_TEXTURES_RELEASED`** — remove the `MEDIARINGS` `warn!`
   (`components/media/media-thread/lib.rs:1309-1317`) first, then
   `rendering_context.rs:1186` (the `fetch_add`) and `:63-65` (the static).
   **Leave `rendering_context.rs:1187-1190` (the `unsafe { … Release(); }`) alone.**

5. **`adapter_local_usage_bytes`** — remove both `components/webgl/webgl_thread.rs` call sites
   (`:1599` and `:1639`) together with `released`/`before`/`devices`/`mb`/`deltas`/the
   `WEBGLRECLAIM` `warn!`, then `rendering_context.rs:12-61`, then `"dxgi1_4"` from
   `components/shared/paint/Cargo.toml:53`.

6. **Cargo features last, and only after a clean `cargo check` of the crate.** winapi 0.3.9
   features gate module visibility; pulling `"psapi"`/`"dxgi1_4"` while a `use` remains gives an
   unresolved-module error that reads like a toolchain problem, not an edit mistake.

7. **`components/servo/lib.rs` re-exports** (`TileRenderTiming` at `:48`, `note_frame_time` at
   `:49`) come out only after `components/paint/lib.rs` stops exporting them. A `pub use` of a
   gone item is a hard error, and `components/servo` is on the default-member build path, so this
   breaks `cargo build` for everyone, not just the wall.

8. The workspace `[patch.crates-io] webrender` line is a **Cargo.lock-affecting** change: dropping
   it re-resolves webrender from crates.io. Do it in its own commit, and expect a full rebuild.

---

## Traps

- **T-1 — `PaintTargetsInFlight::join()` is a barrier, not a probe.** Its doc is written entirely
  about the timings it returns (`paint.rs:366-371`), which makes it read as instrumentation. But
  `paint.rs:339-342` says the opposite: dropping the token without joining lets the next pass
  overlap the previous one. On a 4-GPU wall that is torn tiles, not a missing log line. Only the
  **payload type** `TileRenderTiming` (`paint.rs:357-362`) and its two fields are instrumentation;
  the channel and the `recv()` must stay (the message type can become `()`).

- **T-2 — `paint_api::render_frame` is a budget denominator.** `begin_render_frame` /
  `current_render_frame` (`shared/paint/render_frame.rs`) look like a frame-number probe. They are
  read by `media-thread/lib.rs:911` to enforce `media_ring_init_max_per_frame` and by `:1044` to
  roll the texture-wrap budget. Delete them and both admission limits become unbounded per frame —
  exactly the 190 ms bloated frame the module header describes. KEEP.

- **T-3 — `frame_time.rs` is half functional.** `note_frame_time` (and the `servo::note_frame_time`
  re-export) look like a pure probe because the file also owns the `FRAMETIME` log. But
  `stored_frame_time()` (`frame_time.rs:42`) feeds `frame_time_for_tick`, read by
  `refresh_driver.rs:206` and `:292` — that is the timestamp handed to animations. Removing
  `note_frame_time` without checking makes `stored_frame_time()` permanently `None` and silently
  changes animation phase on every tile.

- **T-4 — `note_present_tick` is two things in one function.** `paint/lib.rs:97-101` calls
  `wall_tick::note_tick_edge()` (functional: the tick-edge counter behind `ExternalPresentBudget`
  and `commit_deadline()`) **and** `output_grid::note_present_tick_now()` (instrumentation). Its
  own doc states that not calling it makes `gfx_present_align_commit_pct` a no-op and sends every
  tile to the immediate-commit fallback. The `servo::note_present_tick` re-export must stay.

- **T-5 — `processthreadsapi` has three use sites, only one of which survives.** Two are
  instrumentation (`GetThreadTimes` at `painter.rs:665`, `GetCurrentProcess` at `:822`) and one is
  functional (`SetThreadPriority` at `:904`). After the strip the feature looks orphaned by
  inspection of the two obvious sites. It is not.

- **T-6 — `shared/paint` has its own `dxgi1_4`.** The brief named only `components/paint/Cargo.toml`.
  `components/shared/paint/Cargo.toml:53` carries `dxgi1_4` for `adapter_local_usage_bytes`, and
  the `examples/` dev-dependency winapi block (`:63-108`) is a *separate* feature set that does
  **not** include it — so the two must be reasoned about independently.

- **T-7 — `report_unknown_external_image` and the `angle_gl_lock` warning are not statistics.**
  Both are one-shot/rate-limited messages on paths that previously panicked or silently
  mis-keyed. `report_unknown_external_image` replaced an `.expect()` that killed the main thread
  and froze the whole wall. Stripping them loses the only signal that a real regression returned.

- **T-8 — `debug_env::WEBGL_FANOUT_PROF` has two readers, in two uncovered crates.** This is the
  recorded past failure, still live: `components/webgl/webgl_thread.rs:337` and
  `components/paint/webrender_external_images.rs:38`. Neither file is in any covered inventory.

- **T-9 — `schedule_wall_production` has zero callers but is not instrumentation.** `paint/lib.rs:125`
  documents this. Removing it with the probes would be an unrelated API deletion.

- **T-10 — `components/paint/comp_stats.rs` must not be deleted while `paint/lib.rs:136` declares it,
  and must not be left behind when `output_grid::emit_comp_stats` goes.** It sits exactly in the
  seam between two covered areas and was inventoried by neither.

---

## Uncertain

| symbol | file | why |
|---|---|---|
| `config_dump` (`servo_config::config_dump`, re-exported at `components/servo/lib.rs:68`) | `components/config/config_dump.rs`; callers `components/servo/examples/winit_wall/main.rs:1922` and `ports/servoshell/desktop/app.rs:98` | It prints the effective pref set once at startup and changes no pixel, so by the stated rule it is REMOVE. But it is a *configuration* dump, not a performance/statistics counter, and with the probes gone it becomes the only record of which prefs a run actually used — which is what naked-eye observation needs to be reproducible. Its home crate (`components/config`) was a covered area; the re-export and the `ports/servoshell` caller were not. Decide the module's fate in the config area and make the `lib.rs:68` re-export follow. |
| `components/shared/paint/examples/{d3d11_dyn_poc,dcomp_native_poc,dcomp_partial_present_poc}.rs` (2,048 lines) and the dev-dependency winapi block `components/shared/paint/Cargo.toml:56-108` | shared/paint | Standalone proof-of-concept binaries, fork-added, never linked into the engine — they change no pixel in any build. They are not instrumentation *of* the wall, they are experiments *about* it, so "deleting it changes no pixel" does not settle it. They cost only `cargo build --examples` time. Leaning KEEP; needs a human call. |
| `components/paint/Cargo.toml` winapi features `"d3d9types"`, `"excpt"`, `"ktmtypes"`, `"vadefs"`, `"vcruntime"`, `"wtypes"` etc. | components/paint | They exist to close winapi 0.3.9's transitive-module graph for `dcomp`, not because any `use` names them. Whether any become droppable after `psapi`/`dxgi1_4` go cannot be settled by grep — only by removing them one at a time and compiling. Do not guess. |
| `ORPHANED_FRONT_BUFFERS` (`components/paint/webrender_external_images.rs:93`, incremented at `:162` and `:434`) | components/paint (uncovered file) | Both increments feed `warn!` lines, so it reads REMOVE — but I did not verify whether either branch *also* performs a leak-avoiding action that the counter is merely narrating. Belongs to whoever takes `webrender_external_images.rs`. |
