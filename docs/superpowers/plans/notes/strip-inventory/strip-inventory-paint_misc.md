# Inventory: paint/{commit_guard,frame_time,wall_tick,refresh_driver,web_content_animation,lib}.rs

Repository: `W:\servo_multigpu-tiled-wall`, branch `wall-strip-instrumentation`.
Read-only survey, 2026-10-27. Nothing was edited.

## Directory listing — components/paint (what this inventory covers)

| file | bytes | covered here? |
|---|---|---|
| `Cargo.toml` | 3238 | yes (no change needed) |
| `commit_guard.rs` | 22913 | **yes** |
| `comp_stats.rs` | 12838 | **yes** (not named in any other subsystem; whole file is instrumentation) |
| `dcomp_compositor.rs` | 311025 | no — other subsystem |
| `dcomp_video_convert.rs` | 44643 | yes (scanned: only `warn!` on D3D creation failure — KEEP, error reporting) |
| `frame_time.rs` | 11229 | **yes** |
| `largest_contentful_paint_calculator.rs` | 3595 | yes (scanned: upstream, no instrumentation) |
| `lib.rs` | 9351 | **yes** |
| `output_grid.rs` | 146733 | no — other subsystem (but its `COLLECT` master switch is documented below) |
| `paint.rs` | 219754 | no — other subsystem |
| `painter.rs` | 283950 | no — other subsystem (but `LOG_ANIM_STEP_SEQ` / `format_step_sequence` are referenced from my files) |
| `pinch_zoom.rs` | 6234 | yes (scanned: clean, upstream) |
| `pipeline_details.rs` | 3986 | yes (scanned: clean; holds `PipelineAnimations` by value, no stats) |
| `refresh_driver.rs` | 35320 | **yes** |
| `render_notifier.rs` | 1294 | yes (scanned: clean, upstream) |
| `screenshot.rs` | 18557 | yes (scanned: only `log::error`; `tick_should_produce` tests are mechanism) |
| `touch.rs` | 29293 | yes (scanned: only `warn!` on invalid touch state — KEEP, upstream) |
| `tracing.rs` | 3271 | yes (scanned: upstream `trace!` target macro — KEEP) |
| `wall_tick.rs` | 35498 | **yes** |
| `webrender_external_images.rs` | 20974 | **yes** (not named in any other subsystem; holds the `WEBGLEXTIMG` profiler) |
| `web_content_animation.rs` | 50634 | **yes** |
| `webview_renderer.rs` | 49925 | no — other subsystem |

---

## Category 1 — mutexes / statics added only to hold statistics

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `DELIVERIES` | `frame_time.rs:123-126` | `static LazyLock<Mutex<HashMap<PainterId, DeliveryWindow>>>` | **REMOVE** | Only reader/writer: `record_delivered` (`frame_time.rs:138`), which pushes dt/lag samples and once a second formats the `FRAMETIME` warn line. No other reference in the tree. | Textbook case: a mutex whose entire contents become a log string. |
| `DeliveryWindow` | `frame_time.rs:113-121` | struct | **REMOVE** | Only used as the value type of `DELIVERIES`. | — |
| `FANOUT_N` / `FANOUT_SUM` / `FANOUT_MAX` | `commit_guard.rs:313-315` | `static AtomicU64` ×3 | **REMOVE** | Written by `record_fanout` (`:320-322`), drained by `emit_lockwait` (`:367-369`) into the `LOCKWAIT` warn. No other readers. | — |
| `LOCK_PAINTER_N/SUM/MAX`, `LOCK_TICK_N/SUM/MAX` | `commit_guard.rs:325-330` | `static AtomicU64` ×6 | **REMOVE** | Written by `record_lock_wait` (`:339-341`), drained by `emit_lockwait` (`:361-366`) and by the test `drain()` (`:392-400`). Both readers are themselves REMOVE. | — |
| `DEVICE_MONITOR` + `remember_device_monitor` + `monitor_for_device` | `commit_guard.rs:104-123` | `static Mutex<Option<HashMap<usize,usize>>>` + 2 fns | **UNCERTAIN** | Writer: `dcomp_compositor.rs:2603` (`remember_device_monitor(device, monitor)` at device/monitor binding). Reader: `dcomp_compositor.rs:2300`, inside `note_commit_failure`, where it is piped through `output_grid::name_for_monitor` to produce `out=` in the `[dcomp-native] … Commit failed (hr=…)` warn. | Its only consumer is an **error** log (a failed `Commit()` HRESULT), not a perf counter. If the dcomp_compositor pass keeps `note_commit_failure` (I would — it reports a real fault, and the `SURFACE_BEING_RENDERED` branch is the tripwire for "tick commit entered the render window"), this map must stay. If that log goes, these three go with it. **Do not decide this one unilaterally; it is cross-file.** |
| `DEVICE_LOCKS` + `device_lock` | `commit_guard.rs:125-142` | `static Mutex<Option<HashMap<usize, &'static Mutex<()>>>>` + fn | **KEEP** | Only caller: `device_guard` (`:166`). The returned `&'static Mutex<()>` is the per-DComp-device serialization lock. | **Trap.** This looks like "a mutex added to a winit_wall module" but it is the actual mutual exclusion between painter `end_frame`, the video fast path, the shell's tick commit and `begin_frame` self-recovery on one DComp device. Removing it is a data race on `IDCompositionDevice`. |
| `WORKERS`, `DONE`, `CommitWorker` | `commit_guard.rs:184-208` | `static Mutex<Vec<CommitWorker>>`, `static OnceLock<(Sender,Mutex<Receiver>)>`, struct | **KEEP** | Used by `ensure_workers` (`:226-247`) and `commit_all_at_tick` (`:262-293`) to dispatch the real `Commit()` to resident worker threads. | **Trap.** This is the parallel-commit fan-out, i.e. the thing that actually issues commits. The module comment records that losing the resident pool re-introduced a measured ~335 µs per-tick regression. |
| `FRAME_TIME_NANOS` | `frame_time.rs:22` | `static AtomicI64` | **KEEP** | Written by `note_frame_time` (`:38`, public, called from `winit_wall/main.rs:1609` via `servo::note_frame_time`). Read by `stored_frame_time` (`:43`) → `frame_time_for_tick` (`:81`) → the `TickAnimation` message's `frame_time` → the page's rAF timestamp. | **Trap.** Product feature (rAF timestamp plumbing), not instrumentation. |
| `NEXT_EDGE_SCHEDULED` | `wall_tick.rs:288` | `static AtomicBool` | **KEEP** | `schedule_production_at_next_edge` (`:292`) ← `lib.rs:126`, `paint.rs:3053`, `paint.rs:3158`, `painter.rs:1591`. `take_scheduled_edge` (`:297`) ← `paint.rs:2474` (gates `on_demand_wall_tick` production). | Demand-driven embedders would never produce a frame without it. |
| `TICK_SEQ` | `wall_tick.rs:318` | `static AtomicU64` | **KEEP** | `note_tick_edge` (`:322`) ← `lib.rs:98`, `paint.rs:2486`. `current_tick_seq` (`:327`) ← `painter.rs:4176`, feeding `ExternalPresentBudget::take`, which decides whether `present_external_only` runs this tick. | **Trap.** It is a counter, but a rendering decision reads it. |
| `ExternalImageProfile` + `WebGLExternalImages::profile` | `webrender_external_images.rs:42-73`, field at `:85`, init at `:111` | struct + field | **REMOVE** | All writes: `:137, 141, 151, 189, 190, 410, 455, 456, 468, 469`. Only reader: `maybe_emit_profile` (`:339-361`) → `WEBGLEXTIMG` warn. | — |
| `ORPHANED_FRONT_BUFFERS` | `webrender_external_images.rs:93`, bumped at `:162` and `:434` | `static AtomicU64` | **UNCERTAIN** | Incremented and immediately interpolated into a `warn!` about a torn-down context mid-lock. Nothing else reads it. | It is a counter inside an error path, not a per-second stat. Low value either way; I lean KEEP because deleting it means editing an error message that documents a real (`std::mem::forget`) leak path. |
| `WebGLExternalImages::logged_locked_surfaces` | `webrender_external_images.rs:83`, init `:110`, used `:196-201` | `FxHashSet<WebGLSurfaceId>` | **UNCERTAIN** | Only use: `if self.logged_locked_surfaces.insert(surface_id) { info!("WebGL external image lock routed: …") }`. Once-per-surface routing log. | Fits "measurement facility the user is not aware of", but it is a one-shot `info!`, not a window aggregate. Removing is safe; call it. |

---

## Category 2 — `note_*` / `emit_*` / `record_*` per-1-second-window functions

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `record_delivered` | `frame_time.rs:132-196` | fn | **REMOVE** | Sole caller: `frame_time_for_tick` (`frame_time.rs:84`), inside `if *FRAME_TIME_PROF`. Return value: none. Emits `FRAMETIMESEQ` + `FRAMETIME` warns. | Also the only reason `frame_time_for_tick` takes a `painter_id`. |
| `millis` | `frame_time.rs:128-130` | fn | **REMOVE** | Callers: `record_delivered` only (`:148, 153`). | — |
| `record_lock_wait` | `commit_guard.rs:332-342` | fn | **REMOVE** | Callers: `device_guard` (`:174`) and the test at `:415-417`. Both REMOVE. | — |
| `record_fanout` | `commit_guard.rs:317-323` | fn | **REMOVE** | Sole caller: `commit_all_at_tick` (`:300`). | — |
| `emit_lockwait` | `commit_guard.rs:344-381` | `pub(crate)` fn | **REMOVE** | Sole caller: `output_grid.rs:2175` (the 1 Hz probe thread). That call site belongs to the output_grid pass and must go with it. | Emits the `LOCKWAIT` line. |
| `PipelineAnimations::note_timeline_step` | `web_content_animation.rs:576-626` | fn | **REMOVE** | Sole caller: `install_paint_animations` (`:668`). Return: none. Emits `TIMELINESTEP`. Gated on `crate::painter::LOG_ANIM_STEP_SEQ`. | Its own doc says the metric "is retired and can no longer be used for verification". |
| `PipelineAnimations::note_transit_clamp` | `web_content_animation.rs:628-660` | fn | **REMOVE** | Sole caller: `install_paint_animations` (`:692`). Emits `TRANSITCLAMP`. | Consumes `transit_clamp_count/max/window_start`. |
| `PipelineAnimations::note_rebase_jump` | `web_content_animation.rs:710-753` | fn | **REMOVE** | Sole caller: `install_paint_animations` (`:694`). Emits `PAINTANIMREBASE`. | Consumes `rebase_jumps`, `rebase_window_start`, `rebase_visible`. |
| `PipelineAnimations::sample_transforms_at` | `web_content_animation.rs:697-708` | fn | **REMOVE** | Callers: `install_paint_animations` (`:667` as `before`, `:693` as `after`) — both feed only `note_rebase_jump`. | Costs two full animation samples per display list; removing it is also a small win. |
| `note_built_at_gap` | `web_content_animation.rs:807-824` | free fn | **REMOVE** | Callers: `note_timeline_step` (`:601`) and `mod timeline_step_tests` (`:832, 840, 848, 854`). | — |
| `transit_overflow_seconds` | `web_content_animation.rs:446-462` | free fn | **REMOVE** | Callers: `install_paint_animations` (`:675`, counting into `clamped`/`max_transit` which only reach `note_transit_clamp`) and the test at `:1063, 1071, 1078`. | **Keep the constant `MAX_DISPLAY_LIST_TRANSIT` (`:36`)** — `animation_zero` (`:434`) clamps against it, which changes the animation origin. |
| `WebGLExternalImages::maybe_emit_profile` | `webrender_external_images.rs:333-367` | fn | **REMOVE** | Sole caller: `unlock` (`:471`). Emits `WEBGLEXTIMG`. | — |
| `note_frame_time` | `frame_time.rs:28-39` | `pub` fn, re-exported `lib.rs:19` | **KEEP** | `components/servo/lib.rs:49` re-exports it; `winit_wall/main.rs:1609` calls it every tick. Value lands in `FRAME_TIME_NANOS` → rAF timestamp. | **Trap.** `note_*` name, but it is the rAF frame-time feature. |
| `note_present_tick` | `lib.rs:75-101` | `pub` fn | **KEEP the function, REMOVE line 99-100** | Callers: `winit_wall/main.rs:1613`. Body is two statements: `wall_tick::note_tick_edge()` (load-bearing, drives `TICK_SEQ`) and `#[cfg(windows)] output_grid::note_present_tick_now()` (instrumentation — that fn returns early unless `*COLLECT`, and only fills `TICK_QPC`). | Delete only the second statement and the `#[cfg(windows)]` attribute above it. |
| `wall_tick::note_tick_edge` | `wall_tick.rs:322-324` | fn | **KEEP** | `lib.rs:98`, `paint.rs:2486`. | — |
| `wall_tick::schedule_production_at_next_edge`, `take_scheduled_edge` | `wall_tick.rs:291-298` | fns | **KEEP** | See Category 1 `NEXT_EDGE_SCHEDULED`. | — |

---

## Category 3 — DWM / display OS syscalls called for statistics

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `comp_stats.rs` — **the entire file** | `comp_stats.rs:1-275` | module: `DCompositionGetFrameId`, `DCompositionGetStatistics`, `DCompositionGetTargetStatistics` via `GetProcAddress`, plus `FrameIdType`, `CompositionFrameStats`, `CompositionTargetId`, `Luid`, `CompositionTargetStats`, `DcompStatsApi`, `api()`, `CompositionStats`, `TargetSample`, `FrameSample`, `MAX_TARGETS`, `completed_frame_id`, `sample_frame` | **REMOVE (whole file)** | Every caller is in `output_grid.rs`: `:2265` `completed_frame_id()`, `:2304` `sample_frame(id)`, `:2455-2456` type references, all inside `emit_comp_stats` (`:2261`), which is itself behind `if !*COLLECT { return }` and emits `COMPWALK` / `COMPREFRESH` / `DCOMPSTAT`. | Three undocumented `dcomp.dll` exports loaded only to count `refresh_count` per display. Deleting this file also removes the `GetProcAddress`/`transmute` block and the `#![allow(unsafe_code)]` exception. **Must be deleted in the same commit as `output_grid::emit_comp_stats`** or `output_grid` will not compile. |
| `mod comp_stats;` declaration | `lib.rs:135-136` | module decl (`#[cfg(windows)]`) | **REMOVE** | — | Delete with the file. |
| `note_dwm_phase()` call | `commit_guard.rs:303-310` (the call is line 310; `:303-309` is its comment) | call into `dcomp_compositor::note_dwm_phase` | **REMOVE** | `commit_all_at_tick` is the only caller of `note_dwm_phase` in the tree. The callee does `DwmGetCompositionTimingInfo` + `QueryPerformanceCounter` once per tick, computes a phase fraction and pushes it into the `DWM_PHASE` mutex for the `COMMITCYCLE` line. Nothing reads it back. | **This is a `DwmGetCompositionTimingInfo` call site that is pure logging.** Contrast with the KEEP row below — check each call site separately, as instructed. |
| `qpc_now()` / `qpc_frequency()` uses in commit_guard | `commit_guard.rs:86` (import), `:167-176`, `:261`, `:299-301`, `:318` | QPC syscalls | **REMOVE** | `:167-176` times the device-guard acquisition for `record_lock_wait`; `:261` + `:299-301` time the commit fan-out for `record_fanout`; `:318` converts ticks→µs inside `record_fanout`. All three consumers are REMOVE. | After this the whole `use crate::output_grid::{qpc_frequency, qpc_now};` import goes. |
| `output_grid::note_present_tick_now()` | call site `lib.rs:99-100` | QPC sample per tick | **REMOVE** | Only called from `note_present_tick`. Callee early-returns unless `*COLLECT`; it records `TICK_QPC` for the `TICKCOMMIT` line. | See the `note_present_tick` row in Category 2. |
| `lib.rs::wait_primary_vblank` | `lib.rs:41-61` | `pub` fn → `output_grid::wait_primary_vblank` (`IDXGIOutput::WaitForVBlank`) | **KEEP** | `components/servo/lib.rs:50` re-exports; `winit_wall/main.rs:1418` blocks the pacing thread on it. | **Trap. This is the frame clock.** Removing it stops the wall. |
| `lib.rs::dwm_composition_grid` | `lib.rs:63-73` | `pub` fn → `dcomp_compositor::composition_grid` (`DwmGetCompositionTimingInfo`, query-only) | **KEEP** | `components/servo/lib.rs:49` re-exports; `winit_wall/main.rs:1235` consumes `(vblank_qpc, period_qpc)` to snap the timer-fallback pacing clock onto the DWM grid (the `snap_to_dwm_grid_at` path). | **Trap.** Same OS call as `note_dwm_phase`, opposite verdict: here the result moves the tick, there it only prints. The doc comment at `:24-40` explains why it must stay a query and never a wait. |

---

## Category 4 — other measurement facilities (switches, log lines, profile scaffolding, tests)

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `FRAME_TIME_PROF` | `frame_time.rs:107-111` | `static LazyLock<bool>`, reads `SERVO_FRAME_TIME_PROF` | **REMOVE** | Sole reader: `frame_time_for_tick:83`. | The env flag itself is declared in `components/config/debug_env.rs:332-334` and listed at `:390`; a twin lives in `components/script/raf_clock.rs:33-36`. Those are other subsystems — flag that the `debug_env` entry becomes dead once both sides go. |
| FRAMETIME banner comment block | `frame_time.rs:89-105` | comment | **REMOVE** | — | Describes the removed block only. |
| `frame_time_for_tick`'s `painter_id` parameter | `frame_time.rs:76-87` | fn signature | **REMOVE the parameter, KEEP the function** | Callers: `refresh_driver.rs:206-210` and `refresh_driver.rs:292-296`, both passing `self.painter_id`. The value is used only by `record_delivered`. | After the param is dropped, `use servo_base::id::PainterId;` (`frame_time.rs:13`) goes too. `AnimationRefreshDriverObserver::painter_id` (`refresh_driver.rs:160`) **stays** — it is also read at `:187` for the fan-out ownership test. |
| frame_time imports | `frame_time.rs:7` (`HashMap`), `:9` (`LazyLock, Mutex`), `:13` (`PainterId`), `:14` (`interval_summary::{percentiles, summarize_intervals}`), `Instant` in `:10` | imports | **REMOVE** | — | `Duration` on `:10` stays (`max_age`). `servo_base::interval_summary` is still used by `paint.rs:130`, `painter.rs:4966`, `script/raf_clock.rs:31` — do not delete the shared module from here. |
| `GuardRole` enum + the `role` parameter of `device_guard` | `commit_guard.rs:144-157`, `:165` | enum + param | **REMOVE** | `device_guard` call sites that pass it: `dcomp_compositor.rs:2337` (`GuardRole::Tick`), `:3251-3253`, `:3590-3592`, `:4575-4577` (all `GuardRole::Painter`). Inside `device_guard` the value flows only to `record_lock_wait` (`:174`), which buckets the wait time into the painter or tick histogram. The doc at `:144-150` says so outright: *"`role` 은 대기 시간을 어느 통에 넣을지만 정한다"*. | **Cross-file:** the four `dcomp_compositor` call sites must lose their second argument in the same commit. |
| `device_guard` instrumentation body | `commit_guard.rs:166-176` | the `COLLECT` check, two `qpc_now()` reads, the `record_lock_wait` call | **REMOVE** | — | The function must keep `let lock = device_lock(device); lock.lock().unwrap_or_else(|e| e.into_inner())`. |
| `commit_all_at_tick` instrumentation lines | `commit_guard.rs:261`, `:299-301`, `:303-310` | locals + calls | **REMOVE** | — | Keep the dispatch loop, the `recv` loop, the sequential fallback, and the `note_tick_commit` loop at `:305-307` **unless** the dcomp_compositor pass removes `note_tick_commit` (it is `output_grid::note_commit_at` + `note_commit_failure`; the first half is instrumentation, the second is error reporting). Coordinate. |
| `mod tests` in commit_guard (`drain`, `lock_wait_is_counted_per_acquirer_role`) | `commit_guard.rs:383-430` | test module | **REMOVE (whole module)** | Exercises `record_lock_wait` and the six `LOCK_*` statics only. | 1 test. Named for the build gate `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu`. |
| `ANIMTICK stop-requested` log | `refresh_driver.rs:195-203` | `if self.animating.get() { warn!(…) }` | **REMOVE the inner `if` block** | — | Keep the enclosing `if !webview_renderer.animating() { … return false; }`. |
| `ANIMTICK start` log | `refresh_driver.rs:225-232` | comment + `warn!` | **REMOVE** | — | Keep `self.animating.set(true); true` that follows. |
| `ANIMTICK stop` log | `refresh_driver.rs:246-253` | `if self.animating.get() { warn!(…) }` | **REMOVE the inner `if` block** | — | Keep `self.animating.set(false); return false;`. |
| `PipelineAnimations` stats fields | `web_content_animation.rs:171-183` (`built_at_previous`, `built_at_gaps`, `built_at_window_start`, `transit_clamp_window_start`, `transit_clamp_count`, `transit_clamp_max`) | struct fields | **REMOVE** | Touched only by `note_timeline_step` and `note_transit_clamp`. | — |
| `PipelineAnimations` rebase fields | `web_content_animation.rs:188-204` (`rebase_jumps`, `rebase_window_start`, `rebase_visible`) | struct fields | **REMOVE** | Touched only by `note_rebase_jump`. `rebase_visible` is write-only in the whole tree (`:738-739` is its sole mention besides the declaration). | — |
| `mod timeline_step_tests` | `web_content_animation.rs:826-856` | test module (4 tests) | **REMOVE (whole module)** | Tests `note_built_at_gap` only. | 4 tests: `first_sample_yields_no_gap`, `reports_gap_in_milliseconds`, `drops_samples_that_go_backwards`, `zero_gap_is_a_real_sample`. |
| `transit_overflow_is_reported_only_past_the_limit` | `web_content_animation.rs:1052-1079` | test | **REMOVE (this test only)** | Tests `transit_overflow_seconds`. | The sibling test `an_absurd_transit_is_clamped` (`:1081-1094`) **stays** — it tests `animation_zero`'s clamp, which is behaviour. |
| `ActivePaintAnimation::complete` | `web_content_animation.rs:278-284`, assigned `:688`, set in test helper `:886` | struct field | **UNCERTAIN** | Written at install time from `PaintAnimation::complete` (`components/shared/paint/display_list.rs:1136`). **Never read anywhere in the tree.** Its own doc says *"Not used to decide whether to keep playing"*. | Dead diagnostic state, but it is not a counter and removing it touches a shared display-list type's consumer. Low priority; decide with the shared/display-list owner. |
| `WEBGL_FANOUT_PROF` + every timing wrapper | `webrender_external_images.rs:35-40`, `:134-138`, `:140-142`, `:148-152`, `:186-191`, `:407-410`, `:452-457`, `:465-470`, `:471` | `static LazyLock<bool>` reading `SERVO_WEBGL_FANOUT_PROF`, plus `Instant::now()` wrappers | **REMOVE** | The gate is read only at those sites; every accumulated value reaches only `maybe_emit_profile`. | Keep the functional bodies inside each wrapper: `swap_chain.take_surface()`, `create_texture`, `stage_to_painter_device`, `destroy_texture`, `lock_swap_chain`, `unlock_swap_chain`. `debug_env::WEBGL_FANOUT_PROF` in `components/config/debug_env.rs` becomes dead. |
| `STAGE_TO_PAINTER_DEVICE` (`gfx_webgl_stage_to_painter_device`) | `webrender_external_images.rs:22-24`, used `:185` | pref gate | **KEEP** | Selects whether WR gets the cross-device texture or a staged copy on the painter device. | **Trap.** The comment calls it "진단용, 기본 off", but it changes which texture is handed to WebRender — that is a pixel-path switch, not a log gate. |
| `SAMPLE_LEAD_PERIODS` (`gfx_sample_lead_periods`) | `commit_guard.rs:88-102` | pref gate, `LazyLock<Option<u64>>` | **KEEP** | `painter.rs:4287` (part of the gate that decides whether to call `note_dcomp_frame_statistics` at all) and `painter.rs:4319` (`paint_animation_sample_at` — the lead used to pick **which instant the animation is sampled at**). | **Trap.** This is the B2 paint-side lead prediction: it changes the value drawn. See also the Traps section — it creates a live dependency on a DComp statistics call. |
| `gfx_paint_side_animation_tick_divisor` | `refresh_driver.rs:276-289` | pref read per frame | **KEEP** | Decides which animating webviews get a `TickAnimation` this frame. | **Trap.** It is the paint-side animation tick divisor — a feature, and the one the memory note `dev-machine-cannot-reproduce-wall-judder` is about. |
| `paint_timer_period`, `frame_time_max_age` | `refresh_driver.rs:496-528` | fns | **KEEP** | `frame_time_max_age` ← `refresh_driver.rs:209, 295`. `paint_timer_period` ← `refresh_driver.rs:413, 499, 551, 659, 671, 676, 703` and **`frame_time.rs:165`** (the latter is inside `record_delivered`, so that one reference disappears). | Also the free-running timer grid; `TimerRefreshDriver`, `Heartbeat`, and both `refresh_driver` tests are mechanism — KEEP. |
| `lib.rs::schedule_wall_production` | `lib.rs:103-127` | `pub` fn | **KEEP** | Re-exported at `components/servo/lib.rs:49`. No in-tree callers (the doc at `:107` says so); the three internal schedule points call `wall_tick::schedule_production_at_next_edge` directly. | Public embedder API, not instrumentation. Leave it. |
| `wall_tick.rs` in full | `wall_tick.rs:1-605` | module | **KEEP (zero removals)** | `TileOutcome`, `KeepPreviousReason`, `tile_may_generate`, `decline_may_arm_keep_previous`, `ack_is_releasable`, `wall_frame_arms_keep_previous`, `tick_should_produce`, `bootstrap_frame_allowed`, `ExternalPresentBudget`, `TICK_SEQ`, `NEXT_EDGE_SCHEDULED` — consumers at `paint.rs:57, 2258, 2474, 2486, 2644, 2655, 3053, 3158`, `painter.rs:159-160, 351, 2175, 2247, 3698, 3956, 4176, 1591`, `screenshot.rs:276, 362, 390`, `lib.rs:98, 126`. | **Trap.** The whole module reads like bookkeeping (counters, budgets, outcome enums) and is entirely decision logic. Its 19 tests all stay. |

### Instrumentation master switch and `SERVO_*` env vars read in `components/paint`

The master switch is **`output_grid::COLLECT`** (`output_grid.rs:50-51`):

```
static COLLECT: LazyLock<bool> = LazyLock::new(|| *PROBE_ENABLED && *dcomp_compositor::DCOMP_BIND_PROF);
static PROBE_ENABLED: LazyLock<bool> = LazyLock::new(|| !debug_env::enabled(&debug_env::WALL_NO_PROBE));
```

i.e. collection is on when `SERVO_WALL_NO_PROBE` is unset **and** `SERVO_DCOMP_BIND_PROF` is set. It is defined in another subsystem's file, but **three of its readers are in my files** and all three are REMOVE: `commit_guard.rs:168`, `:261`, `:358`. The fourth outside reader is `dcomp_compositor.rs:1785`, plus ~28 sites in `output_grid.rs`.

Every `SERVO_*` env var read from `components/paint`, with the file that reads it (mine marked ★):

| env var | read at | purpose | verdict |
|---|---|---|---|
| ★`SERVO_FRAME_TIME_PROF` | `frame_time.rs:107-111` | gates `FRAMETIME` / `FRAMETIMESEQ` | REMOVE |
| ★`SERVO_LOG_ANIM_STEP_SEQ` | read in `painter.rs:4975`; consumed in ★`web_content_animation.rs:593` and ★`frame_time.rs:172` | gates `TIMELINESTEP`, `FRAMETIMESEQ`, `ANIMSTEPSEQ` | REMOVE (definition is painter.rs's to delete; my two readers go first) |
| ★`SERVO_WEBGL_FANOUT_PROF` | `webrender_external_images.rs:36-39` | gates `WEBGLEXTIMG` | REMOVE |
| `SERVO_WALL_NO_PROBE` | `output_grid.rs:38` | master switch half | other subsystem |
| `SERVO_DCOMP_BIND_PROF` | `dcomp_compositor.rs:1442` | master switch half | other subsystem |
| `SERVO_WALL_PAINTER_LOCK_PROF` | `paint.rs:94` | lock profiling | other subsystem |
| `SERVO_FRAME_REASON_PROF`, `SERVO_LOG_PRESENT_CADENCE`, `SERVO_WALL_CANVAS_ACK_SKIP`, `SERVO_WALL_DISABLE_CANVAS_ACK_RECOVERY`, `SERVO_DCOMP_DISABLE_RESIZE_*`, `SERVO_WR_PICTURE_TILE_SIZE` | `painter.rs:76-166` | mixed | other subsystem |
| `SERVO_DCOMP_DEBUG`, `SERVO_DCOMP_READBACK`, `SERVO_DCOMP_VALIDPROBE`, `SERVO_VIDEO_ESCAPE_PROF`, `SERVO_DCOMP_NO_PARTIAL_PRESENT`, `SERVO_DCOMP_SLOW_COMMIT_MS` | `dcomp_compositor.rs:78-2192` | mixed | other subsystem |
| `SERVO_WALL_FRAME_DELAY_{TARGET_INDEX,AFTER,COUNT}` | `paint.rs:751-764` | fault injection, not measurement | other subsystem |
| `SERVO_REFRESH_TIMER_HZ` | ★`refresh_driver.rs:533` — **mentioned in a comment only**, the live read is the `gfx_refresh_hz` pref | — | no action |

---

## Removal order and build-breakage notes

Each step below leaves the tree compiling on its own, **except where it says it must be squashed with another subsystem's step**. Per the project rule "가설 하나당 커밋 하나", these are already one coherent change each.

**Step 1 — `frame_time.rs` (self-contained except for two argument lists).**
Delete `:89-196` (banner, `FRAME_TIME_PROF`, `DeliveryWindow`, `DELIVERIES`, `millis`, `record_delivered`), then `:83-85` inside `frame_time_for_tick`, then the `painter_id` parameter at `:77`, then imports `:7`, `:9`, `:13`, `:14` and `Instant` from `:10`. **In the same commit** fix the two call sites `refresh_driver.rs:206-210` and `:292-296` to drop the first argument. `frame_time.rs`'s own `mod tests` (`:198-267`) compiles unchanged — none of those six tests touch the removed code.

**Step 2 — `refresh_driver.rs` ANIMTICK lines.** Delete the three `warn!` blocks (`:195-203`, `:225-232`, `:246-253`). No signature changes; `use log::warn` stays (used at `:218`, `:304`, `:522`).

**Step 3 — `web_content_animation.rs`.** Delete, in this order so nothing dangles: the three `note_*` fns (`:576-626`, `:628-660`, `:710-753`), then `sample_transforms_at` (`:697-708`), then `note_built_at_gap` (`:807-824`), then `transit_overflow_seconds` (`:446-462`), then the nine `PipelineAnimations` fields (`:171-183`, `:188-204`), then the seven statements in `install_paint_animations` (`:665-668`, `:670-671`, `:675-678`, `:692-694`), then `mod timeline_step_tests` (`:826-856`) and the single test at `:1052-1079`. `install_paint_animations` reduces to: `received_at`/`now`, `prune_held`, the `map` into `ActivePaintAnimation`. **Keep `MAX_DISPLAY_LIST_TRANSIT`** — `animation_zero:434` clamps with it. After this the `crate::painter::{LOG_ANIM_STEP_SEQ, format_step_sequence}` references from this file are gone, which is a precondition for the painter.rs pass deleting them.

**Step 4 — `webrender_external_images.rs`.** Delete `WEBGL_FANOUT_PROF` (`:35-40`), `ExternalImageProfile` (`:42-73`), the `profile` field (`:85`) and its init (`:111`), `maybe_emit_profile` (`:333-367`) and its call (`:471`), then the eight timing wrappers. Unused imports after this: `std::time::{Duration, Instant}` (`:8`) — check `Duration` has no other use, and `servo_config::debug_env` (`:17`) if nothing else in the file uses it.

**Step 5 — `commit_guard.rs` + its cross-file argument lists (one commit).**
a. Delete `mod tests` (`:383-430`), `emit_lockwait` (`:344-381`), `record_lock_wait` (`:332-342`), the six `LOCK_*` statics (`:325-330`), `record_fanout` (`:317-323`), the three `FANOUT_*` statics (`:313-315`).
b. Delete `commit_guard.rs:261`, `:299-301` from `commit_all_at_tick`.
c. Delete `GuardRole` (`:144-157`), the `role` parameter (`:165`), and the instrumentation body (`:166-176`). **Same commit:** drop the second argument at `dcomp_compositor.rs:2337`, `:3251-3253`, `:3590-3592`, `:4575-4577`.
d. Delete the import `use crate::output_grid::{qpc_frequency, qpc_now};` (`:86`) and `use std::sync::atomic::{AtomicU64, Ordering};` (`:81`).
e. Trim the module doc (`:1-78`) where it describes `LOCKWAIT`/`painter_us_max`/`fanout_us` — the phase table and the "가드 안에서 하는 일은 `Commit()` 하나" rule are design rationale worth keeping; the sentences that point at a now-deleted log line are not.
**Breakage if done alone:** `output_grid.rs:2175` calls `emit_lockwait()`. Either squash with the output_grid pass or land `output_grid.rs:2175`'s deletion in this commit.

**Step 6 — `lib.rs` line 99-100.** Delete `#[cfg(windows)] crate::output_grid::note_present_tick_now();` from `note_present_tick`. Self-contained.

**Step 7 — `comp_stats.rs` deletion (must be squashed with the output_grid pass).**
Delete the file and `lib.rs:135-136`. This breaks `output_grid.rs:2265, 2304, 2455-2456` immediately, so it cannot land before (or separately from) the removal of `output_grid::emit_comp_stats` and the `TargetWindow`/`COMPWALK`/`COMPREFRESH` machinery around `output_grid.rs:2168-2530`. **Coordinate with whoever owns `output_grid.rs`.** Note for the user: `COMPREFRESH refresh_d[2]` is the judder counter recorded in memory as the current verdict metric — removing `comp_stats.rs` removes the ability to measure it. That is the stated intent (back to naked-eye observation), but it is worth saying out loud before the commit lands.

**Step 8 — the `commit_guard.rs:310` `note_dwm_phase()` call.** Delete with the dcomp_compositor pass that deletes `note_dwm_phase` itself; deleting only the call site is harmless (leaves a `dead_code` warning on the callee) but deleting only the callee breaks my file.

**Deferred / needs a decision:** `DEVICE_MONITOR` + `remember_device_monitor` + `monitor_for_device` (Step 5 candidate, but only if `dcomp_compositor::note_commit_failure` goes), `ActivePaintAnimation::complete`, `ORPHANED_FRONT_BUFFERS`, `logged_locked_surfaces`.

### Test accounting for `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu`

Removed with their instrumentation:
- `commit_guard.rs::tests::lock_wait_is_counted_per_acquirer_role` (and the `drain` helper) — whole `mod tests`, `:383-430`.
- `web_content_animation.rs::timeline_step_tests` — whole module, 4 tests, `:826-856`.
- `web_content_animation.rs::tests::transit_overflow_is_reported_only_past_the_limit` — `:1052-1079`.

Kept and must still pass unchanged:
- `frame_time.rs::tests` — 6 tests (`nothing_stored_is_none`, `a_recent_value_is_used`, `a_stale_value_is_dropped`, `exactly_max_age_is_still_fresh`, `a_value_slightly_in_the_future_is_used`, `store_and_read_round_trip`).
- `wall_tick.rs::tests` — 19 tests, all mechanism.
- `refresh_driver.rs::tests` — 2 tests (`a_driver_without_a_clock_of_its_own_gets_woken_by_the_paint_timer`, `a_driver_with_its_own_clock_gets_no_heartbeat`).
- `web_content_animation.rs::tests` — the remaining 9 tests, including `an_absurd_transit_is_clamped` and `a_built_at_in_the_future_does_not_panic`.

---

## Traps — looks like instrumentation, is load-bearing

1. **`lib.rs::wait_primary_vblank` (`:41-61`) is the frame clock.** `IDXGIOutput::WaitForVBlank` via `output_grid::wait_primary_vblank`, blocked on by `winit_wall/main.rs:1418`. Deleting it stops the wall. Its own doc explicitly contrasts itself with the "never add a wait" rule for producer threads.
2. **`lib.rs::dwm_composition_grid` (`:63-73`) is a `DwmGetCompositionTimingInfo` call that moves the clock.** `winit_wall/main.rs:1235` snaps the timer-fallback pacing clock to the returned grid. The *same OS call* at `dcomp_compositor::note_dwm_phase` only prints. Judge call sites, never functions.
3. **`wall_tick::TICK_SEQ` / `current_tick_seq` is a counter that gates rendering.** `painter.rs:4176` feeds it to `ExternalPresentBudget::take`, which decides whether `present_external_only` fires this tick. A counter read by a decision is not instrumentation.
4. **`frame_time.rs`'s core four (`FRAME_TIME_NANOS`, `note_frame_time`, `stored_frame_time`, `fresh_frame_time`, `frame_time_for_tick`) are the rAF timestamp feature**, not the `FRAMETIME` log. The log is bolted onto `frame_time_for_tick` at exactly one line (`:83-85`) plus one parameter. Everything above line 88 stays.
5. **`commit_guard::DEVICE_LOCKS` / `device_lock` / `device_guard` is real mutual exclusion on `IDCompositionDevice`.** Only the `role` argument and the timing around the `lock()` are instrumentation.
6. **`commit_guard::WORKERS` / `DONE` / `worker_loop` / `ensure_workers` / `commit_all_at_tick` issue the actual commits.** The module history records that losing the resident pool cost a measured ~335 µs (4 × ~80 µs thread spawns) per tick.
7. **`commit_guard::SAMPLE_LEAD_PERIODS` changes what is drawn.** It is the B2 lead used by `painter::paint_animation_sample_at` (`painter.rs:4319`) to choose the instant the animation is sampled at. **And it creates a live dependency on a DComp statistics call:** `painter.rs:4286-4289` deliberately runs `note_dcomp_frame_statistics` when *either* `DCOMP_BIND_PROF` is on *or* `SAMPLE_LEAD_PERIODS` is set, because that call is what fills the shared composition grid B2 reads. So "`note_dcomp_frame_statistics` is a stats call, delete it" is wrong while `gfx_sample_lead_periods` exists. Whoever strips `painter.rs`/`output_grid.rs` must either keep that path or remove B2 in the same step.
8. **`refresh_driver`'s `gfx_paint_side_animation_tick_divisor` (`:276-289`) is the paint-side animation tick divisor** — a feature, and the knob behind the memory note "개발 장비도 divisor=1 이면 벽 저더를 재현한다". Not a log gate.
9. **`refresh_driver::Heartbeat` (`:380-494`) is a clock, not a probe.** `armed`/`waiting_for_frame` with their four `SeqCst` accesses are a correctness argument written out in the comment at `:461-487`; dropping the heartbeat freezes `FrameClock::Timer` embedders.
10. **`web_content_animation::MAX_DISPLAY_LIST_TRANSIT` (`:36`) survives `transit_overflow_seconds`.** The constant is the clamp inside `animation_zero:434`, which sets the animation origin. Only the *overflow reporter* goes.
11. **`web_content_animation`'s `PaintAnimation` sampling path** — `ActivePaintAnimation::running`/`sample`/`sample_final`/`ends_at`, `segment_progress`, `find_segment`, `HeldPaintValues`, `prune_held`, `append_held_paint_values`, `animation_zero` — computes the drawn value. Only the `note_*`/window fields wrapped around it are instrumentation.
12. **`wall_tick.rs` has zero removals.** Outcome enums, budgets and predicates all read like bookkeeping; every one of them is a rendering or liveness decision. In particular `tile_may_generate`'s `renderer_behind` argument is a liveness probe whose 2-second force-release only runs on the decline path (measured 2026-08-31: losing the call killed two of four painters for 44 s).
13. **`webrender_external_images::STAGE_TO_PAINTER_DEVICE`** is commented "진단용, 기본 off" but swaps which GL texture WebRender receives. It is a pixel-path switch.
14. **`dcomp_compositor::note_commit_failure` is error reporting, not a statistic**, and it is the only consumer of `commit_guard::DEVICE_MONITOR`. Its `SURFACE_BEING_RENDERED` branch is the tripwire for "a tick commit entered the render window" — the exact failure mode in the memory note `dcomp-v1-commit-rejects-open-surface`.
