# strip-PLAN — executable removal plan for the winit_wall instrumentation strip

Repo: `W:\servo_multigpu-tiled-wall`, branch `wall-strip-instrumentation`.
Merged from nine inventories + two adversarial verifications. Read-only authorship; nothing edited.

---

## 1. Scope

This plan removes every facility in the fork whose only product is a number in a log line — the
1 Hz `note_*`/`emit_*`/`report_*` aggregators and the `Mutex`/`Atomic`/`Cell` windows they drain,
the `OutputVBlankProbe` and `WEBGLWATCHDOG` sampling threads, the `Instant` stopwatches wrapped
around real work in `dcomp_compositor`/`painter`/`webgl_thread`/`media`, the `DwmGetCompositionTimingInfo`
and `DCompositionGetStatistics` call sites that feed only log fields, the `SERVO_*` gates that
switch them on, their `debug_env` registry entries and `configuration.md` documentation, the
`servo_base::interval_summary` module that formats them, and the launcher's whole analysis
section and instrumentation switches — across sixteen independently revertable commits. It
deliberately **keeps** everything a rendering or timing decision reads, even where it carries an
instrumentation name: the `IDXGIOutput::WaitForVBlank` pacing clock, `dwm_composition_grid` (the
timer-fallback arm of that clock), the `COMPOSITION` grid written ahead of the `COLLECT` gate and
the `GetFrameStatistics` call that fills it (the B2 animation-sample instant), the wall frame
barrier's `pending_frame_diagnostics`/`FrameReadyDiagnostic` plumbing, `wall_tick.rs` in full,
`commit_guard`'s device locks and resident commit workers, the rAF `frame_time` plumbing,
`paint_api::render_frame`'s per-frame admission budgets, `lock_tracked`'s mutex-poison recovery,
`LIVE_CONTEXTS`' prune, and every ANGLE GL guard. Fault injection, kill switches and
misconfiguration alarms are **not** measurement and are listed under Decisions, not removed.

---

## 1b. ★결정 확정 (2026-10-27, 사용자)★ — 6 절이 4 절을 대체한다

4 절 "Decisions needed" 는 **전부 답이 나왔다.** 실행할 때는 6 절을 보라. 요약:

| 항목 | 결정 |
|---|---|
| `-PacingOutput` / `gfx_wall_pacing_output` / `pinned_index` | ★유지★ — 계측이 아니라 동작 스위치 |
| B2 체인(`GetFrameStatistics`→`COMPOSITION`→`lead_to_next_composition`) | ★유지★ — 그리는 값을 정한다 |
| 오설정 경보(`MEDIA_D3D11_DEVICE_OWNERS` 충돌, 타일 LUID 교차검사) | ★유지★ — 4 GPU 벽에서 육안으로 못 잡는 고장 |
| 고장 주입·킬스위치 debug_env 9 개 + `gfx_wall_rotate_tile_order` | ★유지★ — 숫자를 안 찍는다 |
| `--tile-thread-spike` / TILESPIKE (474 줄) | ★제거★ — Step 17 |
| 런처 로깅 배관 | ★`-NoLog` 만 유지, 기본 `RUST_LOG=warn`★ — `-QuietLog` `-KeepRustLog` `-LegacyLogRedirect` 는 제거 |
| `vendor_local/webrender` 패치 | ★upstream 으로 되돌린다★ — Step 20 |
| `ports/servoshell` 타이밍 계층 | ★제거★ — Step 18 |
| `shared/paint` POC 예제 3 개 (2,048 줄) | ★제거★ — Step 19 |
| `check_freeze_log.ps1` 등 분석 도구 | ★제거★ — Step 16 에 포함 |

---

## 2. Ordered steps

Conventions used below:

| tag | command |
|---|---|
| **G1** | `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu` |
| **G2** | `cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu` |
| **G3** | `cargo check -p servo-canvas` |
| **G4** | `cargo check -p servo-layout -p servo-script` |
| **G5** | `cargo test -p servo-config --test config_surface` |

> **Gate-name corrections (verified against the Cargo.tomls, do not "fix" them back).**
> There is no package named `canvas`, `layout` or `script`; the packages are `servo-canvas`
> (`components/canvas/Cargo.toml:2`), `servo-layout` and `servo-script`. `cargo check -p canvas`
> and `cargo check -p layout -p script` fail with "did not match any packages".
> Also: the brief says G1 does not compile `components/webgl`. It does —
> `components/paint/Cargo.toml:56` is `webgl = { workspace = true }`, which is why the gate's own
> `--features webgl/no-wgl` resolves. G1 therefore *would* catch the `WEBGL_FANOUT_PROF` break.
> The ordering requirement in Step 2 stands regardless, because being caught by a 130-test build
> is not the same as not happening.

Run gates cheapest-first within a step (G5 → G3/G4 → G1 → G2). The full release build is ~11 min;
never reach for it to find something a crate check would have shown.

**Expected `config_surface` results.** It is 19/20 today, with
`every_env_name_read_in_sources_is_registered` RED on five unregistered `SERVO_*` names. A second
test, `every_registered_flag_is_actually_read_somewhere`, goes RED as each flag loses its last
reader and stays RED until Step 14 — this is expected and is noted per step. Both are GREEN at
Step 14 (20/20), and that is the independent success signal.

---

### Step 1 — `note_commit_failure`'s `out=` decoration and the device→monitor map

**Delete (by symbol):**
- `components/paint/dcomp_compositor.rs` — inside `note_commit_failure` (:2281-2326), the comment
  at :2300 and the two statements that build `out` (`commit_guard::monitor_for_device(device)
  .and_then(output_grid::name_for_monitor).unwrap_or_else(…)`), and the ` out={out}` field from
  the `warn!` format string.
- `components/paint/dcomp_compositor.rs:2603` — the `commit_guard::remember_device_monitor(device, monitor)`
  call inside the KEPT `note_frame_statistics`.
- `components/paint/commit_guard.rs:104-123` — `DEVICE_MONITOR`, `remember_device_monitor`,
  `monitor_for_device`.

**Keep:** `note_commit_failure` itself, its `hr >= 0` early return, the `LAST` per-device 1 s
throttle (a flood guard on a TDR storm, not a statistic), the `DCOMPOSITION_ERROR_SURFACE_BEING_RENDERED`
named branch, and the `warn!`.

**Why this position (forced).** `output_grid::publish_names` is the only writer of `NAMES`, and it
dies with `probe_loop` in Step 12. From that commit on, `out=` can only ever be `?`. The brief
requires this to be decided *before* the step that orphans `NAMES`. The decision taken here:
`note_commit_failure` is an error report (TDR / device-removed / `SURFACE_BEING_RENDERED`), not a
per-second statistic, so the report survives and only the decoration goes — which also means
`name_for_monitor` has no out-of-file caller left when Step 12 reaches it.

**Gate:** G1. **Expected:** 130/130.

---

### Step 2 — `components/webgl` instrumentation, and the `shared/paint` helper it strands

**Delete (by symbol, in `components/webgl/webgl_thread.rs` unless noted):**
1. Statement sites first: the watchdog bookkeeping in `process()` (the `*WEBGL_FANOUT_PROF` spawn
   guard, the `WEBGL_PROGRESS`/`WEBGL_PHASE` stores, and the
   `match &msg { … }.fetch_add(1, Ordering::Relaxed);` expression statement — it borrows `msg`,
   it does not consume it); the `commands += 1` bump; the serialize stopwatch in
   `clone_command_for_backend`; the lock-wait/apply stopwatch in `apply_webgl_command_to_surface`;
   the swap counter and `sync_ns`/`syncs` accumulators; the swap and clear stopwatches; the phase
   stores in `handle_swap_buffers`; the `maybe_emit_fanout_profile()` call; both switch stopwatches
   in `make_surface_current_if_needed` and `…_mut`; the `WEBGLRECLAIM` locals
   (`released`, `before`, `devices`, `mb`, `deltas`) and both `WEBGLRECLAIM` warns.
2. Then the methods: `maybe_emit_fanout_profile`, `log_live_contexts`, `spawn_webgl_watchdog`,
   `phase_name`.
3. Then the fields `WebGLThread::fanout_profile` and `WebGLThread::last_live_log` with their
   initialisers.
4. Then the types and statics: `FanoutProfile`, the crate-local `WEBGL_FANOUT_PROF`,
   `WEBGL_PROGRESS`, `WEBGL_PHASE`, `WEBGL_MSG_CMDS`/`_SWAPS`/`_OTHER`, `PHASE_IDLE`/`_HANDLING`/
   `_SWAP_LOCK`/`_SWAP_BUFFERS`/`_SWAP_AFTER`.
5. Then imports: `use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};` and
   `use servo_config::debug_env;`. **Keep** the `Arc, LazyLock`, `Duration, Instant` and
   `slice, thread` imports.
6. `components/shared/paint/rendering_context.rs:12-61` — `adapter_local_usage_bytes` (its only two
   callers are the `WEBGLRECLAIM` sites deleted above), and `dxgi1_4` from
   `components/shared/paint/Cargo.toml:53`.

**Delete by symbol, not by range, in one place in particular:** `log_live_contexts`'s doc is
lines 737-750, but lines **731-736** above it document `angle_device_key`. Delete **737-777**;
deleting 731-777 destroys the `angle_gl_lock` keying documentation.

**Keep — these sit inside or beside the deletions:** every
`let _angle_gl_guard = paint_api::angle_gl_lock(...)` (the one at :1115 is sandwiched *between*
the two stopwatch statements of T2 — a range delete drops the lock and races tile rendering on the
same ANGLE device); `clone_webgl_command` and the `cloned` body of `clone_command_for_backend`;
`SwapSync`/`WEBGL_SWAP_SYNC` and the `gl.flush()`/`gl.finish()` they select;
`swap_chain.swap_buffers` / `clear_surface`; the real switch bodies at :2003-2009 / :2029-2035;
`maybe_reclaim_idle_contexts` with `released_contexts`, `context_recipes`, `last_composited`,
`last_reclaim_sweep`, the destroy loop and the `Lost`/`Restored` notifiers; `mod fanout_clone_tests`.

**Why this position (forced).** `debug_env::WEBGL_FANOUT_PROF` has **two** readers, not one —
`webgl_thread.rs:336-339` and `paint/webrender_external_images.rs:37-40`. Step 14 deletes the
const; `components/webgl` must stop reading it first. This is the enumeration error the brief
cites; it is fixed here rather than discovered later. The `shared/paint` helper is folded in
because it becomes a `pub` dead function that produces no warning and that no later sweep of the
paint area would flag.

**Gate:** G3 (cheap, covers `paint_api`), then G1 (the only gate that compiles `servo-webgl`).
**Expected:** both clean; 130/130. Read the warning list — the workspace has no `[workspace.lints]`,
so a leftover unused import is a warning, not an error.

---

### Step 3 — `dcomp_compositor` self-contained log gates, `DWMPHASE`, and `dwm_timing()`

**Delete (by symbol, `components/paint/dcomp_compositor.rs` unless noted):**
- `[vesc-prof]`: `EscProf`, `PRESENT_SAMPLE_CAP`, `impl EscProf` (`record_present`, `maybe_flush`),
  the `esc_prof` field and its initialiser, `video_escape_prof()`, and every accumulation site
  (`2834-2843`, `3025-3033`, `3086-3096`, `3101-3110`, `3155-3166`, `3620-3622`, `5243-5246`).
  **Keep** `convert_pass.convert(...)`, the `converted` boolean, `(*swapchain.as_ptr()).Present(...)`
  and its `hr`, `cp.end_batch(...)`, and `convert_pass.begin_batch(...)` /
  `external_batch_active = true` — the last pair sits directly beside `d_batch_swaps += 1`.
- readback / validprobe: `readback_log_bound`, `readback_enabled()`, the gate in `unbind`,
  `valid_probe_enabled()` and the `[dcomp-validprobe]` block; then the now-dead `BoundTile` fields
  `tile`, `update_offset`, `dirty_size`, `tex_size` and their initialisers in `bind`.
- `[dcomp-dbg]`: `dcomp_debug()` and all 17 `log::info!` blocks, plus `ExternalStorage::frames_logged`
  and its initialiser.
- `DWMPHASE`: `DwmPhaseWindow`, `DWM_PHASE`, `note_dwm_phase_skip`, `note_dwm_phase`; then its three
  call sites — `commit_guard.rs:310`, `commit_device_guarded` (:3260) and `end_frame` (:5232-5234);
  then the now-dead `use winapi::um::profileapi::{QueryPerformanceCounter, QueryPerformanceFrequency};`
  at :57. **Do not touch line 56** — `DWM_TIMING_INFO`/`DwmGetCompositionTimingInfo` are still used
  by `composition_grid()`.
- `dwm_timing()` **and**, in the same commit, its one caller `output_grid.rs:1396` plus the two
  `DCOMPSTAT` fields it feeds (`last_vs_dwmvb_ms`, `compose_after_vblank_ms`, `output_grid.rs:1402-1410`).

**Keep:** `composition_grid()` (:1734-1761). Six of its seven callers are log lines in
`output_grid.rs`, but the seventh — `lib.rs:67` → `servo::dwm_composition_grid` →
`winit_wall/main.rs:1235` `snap_to_dwm_grid_at` — is the timer-fallback arm of the pacing clock.
It and `dwm_timing()` are two near-identical `DwmGetCompositionTimingInfo` wrappers with opposite
verdicts.

**Why this position.** All four gate families are self-contained inside one file; `DWMPHASE` is
first in order because deleting `note_dwm_phase` removes `dcomp_compositor.rs:1785`, the only
`COLLECT` reader outside `output_grid.rs`/`commit_guard.rs` — one of the four readers that must be
gone before `COLLECT` itself can die in Step 13. `dwm_timing()` must land with its `output_grid`
caller or `output_grid.rs` stops compiling.

**Gate:** G1. **Expected:** 130/130.
`every_registered_flag_is_actually_read_somewhere` (G5) goes RED from here on
(`VIDEO_ESCAPE_PROF`, `DCOMP_READBACK`, `DCOMP_VALIDPROBE`, `DCOMP_DEBUG` have no readers left);
it returns GREEN at Step 14. Do not run G5 as a pass/fail gate between here and Step 14.

---

### Step 4 — `dcomp_compositor` commit stamping, `COMMITSITE`/`COMMITWORK`, and the `DCOMPBIND` stopwatches

**Delete (by symbol):**
- `CommitOutcome` (incl. `.stamp`); `raw_commit` returns a bare `i32`; `commit_device_locked`
  returns `i32`; `end_frame`'s `commit_hr` binding stops destructuring `outcome.hr`.
- The `output_grid::commit_stamp()` call (:2258), all four `note_commit_stamp` call sites
  (:2343, :3257, :3597, :5229) and all four `note_commit_at` call sites (:2354, :3258, :3598, :5230).
- `CommitSite` + `impl` + `COMMIT_SITE_ORDER` + the `const _: ()` order assert; `CommitSlot`,
  `CommitTotals`, `COMMIT_SITES`; `DeviceWork`, `WorkSnapshot`, `DEVICE_WORK`, `DEVICE_WORK_SLOTS`,
  `find_device_work`, `device_work`; `WORK_BUCKETS`, `WORK_BUCKET_TAGS`, `work_bucket`, `WorkSlot`,
  `COMMIT_WORK`; `SLOW_COMMIT_NS`; `COMMITSITE_EMITTER`, `claim_commitsite_emitter`,
  `release_commitsite_emitter`; `emit_commitsite`. Then `raw_commit` loses its `site` parameter and
  all four call sites drop the argument; `release_all` loses :3651; `bind` loses `work_device`
  (:3803-3807) and :4051-4053; `add_surface` loses :4366-4375.
- `DCOMPBIND`: `BindProfile` and the `bind_profile` field + initialiser, `SizeSet` + impls,
  `BeginFrameSplit`, `maybe_emit_bind_profile`, `SurfaceEntry::last_begin_texture`,
  `BoundTile::begin_qpc` and the two `output_grid::qpc_now()` reads that fill/consume it, the
  `output_grid::note_bind_span` call, and every `DCOMP_BIND_PROF` stopwatch listed in the dcomp
  inventory's Category 4. `begin_frame_inner` returns `()`; `begin_frame` becomes a one-line
  forwarder; the `debug_assert!(total >= parts, …)` goes with it.
- **In this same commit**, the 17 instrumentation tests in `mod commit_probe_tests`, or the gate
  fails to compile.

**Keep:** the `DCOMP_BIND_PROF` static itself (it still has three out-of-file readers and is the
right-hand side of `COLLECT`; it dies in Step 13); `(*device).Commit()`; `commit_at_tick_locked`'s
device guard; the `Present`/`present1_partial` calls and their `ok`/`presented_ok` results;
`needs_flush` and `device.gl().flush()`; `RemoveAllVisuals()` and its `hr` check;
`self.commit_pending = true;`; `quantize_update_rect`/`UPDATE_QUANTUM_PX`, `PRESENT_SYNC_INTERVAL`,
`probe_partial_present`, `should_fast_present`, `escaped_external_count`, `withheld_frames`; and the
seven quantiser tests plus the `tile()`/`band()` helpers that live inside `mod commit_probe_tests` —
**do not delete that module wholesale**, it mixes instrumentation tests with behaviour coverage.

**Why this position.** It must follow Step 3 only for tidiness, but it must **precede** Step 13:
Step 13 deletes `DCOMP_BIND_PROF`, and every in-file reader has to be gone by then. Keeping the
static alive here means `COLLECT` evaluates to exactly the same value as before — the strip never
changes what collection is on at any commit.

**Gate:** G1. **Expected:** 130/130 minus the 17 deleted tests → **113/113**.

---

### Step 5 — `painter.rs` animation, frame-reason and render-path telemetry

**Delete (by symbol, `components/paint/painter.rs`):**
`note_animation_win`, `note_animation_ready`, `note_animation_step`, `log_paint_animation_activity`
and their four call sites; `note_frame_reason`, `note_frame_source`, `note_wall_pacer_join`,
`FrameSource`, `FrameReasonCounts`, `FRAME_REASON_PROF`, `frame_reason_prof_enabled()` and
`mod frame_reason_source_tests` **in the same commit**, plus the `frame_reason_prof_enabled()` block
at `paint.rs:2737-2743`; the `#[track_caller]` attributes on `generate_frame` /
`generate_frame_with_diagnostic_id` and `use std::panic::Location`; `check_canvas_ack_latch` and the
`WALLACKFLUSH` tail of `flush_owed_canvas_ack`; both `WALLBOOTSTRAP` log branches; the
`PAINTANIMANCHOR` block; `UpdateImagesStats`/`UPDATE_IMAGES_STATS`/`IMGUPDINNER` and both video-gap
captures; the four `render()` log blocks ("Wall render start"/"Render start", "Paint gap",
"Slow paint frame", "Wall render end"/"Render end") with their stopwatch locals; `log_gpu_memory`,
`process_memory_mb`, `thread_cpu_ms` (both cfgs of each); the `output_grid::note_frame_id` call;
`PumpSource::note`; the painter-local `LOG_PRESENT_CADENCE` static and the present-cadence
accumulator at :3466-3495; and the fields these strand (`anim_*`, `paint_animation_window_start`,
`paint_animation_skipped_busy`, `paint_animation_rode_along`, `paint_animation_held_max`,
`last_frame_by_other_source_at`, `last_frame_generated_at`, `last_video_arrived_at`,
`last_video_presented_at`, `render_count`, `last_render_end`, `canvas_ack_held_since`,
`canvas_ack_last_report`, `owed_ack_flush_count`, `owed_ack_last_report`,
`bootstrap_frames_emitted`/`_declined`/`_decline_last_report`, `present_cadence_*`,
`frame_reason_counts`, `frame_reason_window_start`) and the now-unread locals
`floats_log`, `transforms_log`, `painter_busy`, `someone_else_is_producing`.

**Keep — each sits one line from a deletion:** `last_render_started_at.set(...)` at :2461 (directly
under `note_animation_win()`; it is the clock `push_due` and `renderer_is_running` run on);
`last_gpu_memory_log_at` and the once-a-second `due` gate (it also throttles
`D3d11PlaneRings::expire_all_stale_demand`) — delete only the `log_gpu_memory` call and the
`MEDIARINGRECLAIM` warn inside it; `last_paint_animation_push_at`; `display_composite_in_flight`
and `display_composite_in_flight_since`; `wall_tick_frames_produced` and `bootstrapped_pipelines`;
`append_held_paint_values`; `PumpSource::may_schedule()` and
`schedule_production_at_next_edge()`; `flush_owed_canvas_ack`'s `ack_is_releasable` gate and
`send_owed_canvas_ack()`; `install_paint_animations`; `renderer.render(size, 0)` (drop only
`.ok().map(|results| results.stats)` and the `wr_stats` binding) and every ANGLE-lock / `make_current`
/ `prepare_for_rendering` / `renderer.update()` / `clear_background()` statement around it;
`send_pending_paint_metrics_messages_after_composite` (upstream FP/FCP/LCP).

**Keep for now:** `format_step_sequence` and `LOG_ANIM_STEP_SEQ` — they still have readers in
`frame_time.rs` and `web_content_animation.rs`; they die in Step 7.

**Why this position.** It must precede Step 7 (which deletes `format_step_sequence`): readers before
definition. It must precede Step 8, which removes the *last* `debug_env::LOG_PRESENT_CADENCE`
reader; this step removes the painter-side one, leaving `winit_wall` as the only one.

**Gate:** G1. **Expected:** 113 minus 2 (`frame_reason_source_tests`) → **111/111**.

---

### Step 6 — the frame request/ready diagnostics reduction and the `paint.rs` aggregators

**Reduce, do not delete (`painter.rs`):**
- `record_frame_request` → keep only `pending_frame_diagnostics.push_back(...)`. Remove
  `wall_request_delay_ms`, the `overlapping_frame_request_count` block and both
  "Wall frame requested"/"Frame requested" branches.
- `PendingFrameDiagnostic` → reduce to `{ wall_logical_frame_id: Option<u64> }`.
- `note_webrender_frame_ready` → keep `tick_dcomp_resize_rebuild`, the
  `!need_repaint → set_display_composite_in_flight(false)` branch, the `pending_frames` decrement,
  `pending_frame_diagnostics.pop_front()`, the `FrameReadyDiagnostic` construction and both
  `return None` guards. Remove `unexpected_frame_ready_count` + warn, the `note_animation_ready()`
  call, the `last_ready_local_frame_id` / `last_ready_wall_logical_frame_id` stores, `wait_ms` /
  `shared_request_to_ready_ms`, and both "Wall frame ready"/"Frame ready" branches.
- `FrameReadyDiagnostic` → reduce to `{ painter_id, wall_logical_frame_id, ready_at, need_repaint }`.
- Delete `next_diagnostic_frame_id` (field + fn), `last_ready_local_frame_id`,
  `last_ready_wall_logical_frame_id`, `overlapping_frame_request_count`,
  `unexpected_frame_ready_count`, `anim_ready_queue`, `anim_ready_pending_x`, and
  `Painter::wall_scroll_offsets_signature`.

**Delete (`paint.rs`):** `note_demand_tick` + its three fields + the call (keep `on_demand_wall_tick`'s
`produced` return); `log_wall_frame_metadata` + its call; `log_wall_frame_pacing_summary`,
`WallFramePacingBlockTally`, `wall_frame_pacing_coalesced_count`, `wall_frame_pacing_released_count`,
`CoalescedWallFrameRequest.coalesced_count`/`.max_pending_seen`, `WALL_FRAME_PACING_INFO_INTERVAL`;
the `PAINTDRAIN` prologue and epilogue (`drain_start`, `messages_seen`, the `kinds` tally loop and
the final `warn!`); the `IMGUPDRATE` block with `ImageUpdateStats`, `IMAGE_UPDATE_STATS`,
`DETACHED_DROPPED`; the `Wall media image fanout summary` block with
`WALL_MEDIA_IMAGE_FANOUT_COUNT` and `WALL_MEDIA_IMAGE_FANOUT_INFO_INTERVAL`; the `WALLSHOTPAUSE`
tail and its four fields; and the barrier log lines in `note_frame_ready`, `complete_barrier` and
`miss_barrier`, plus the paint.rs lines that read `diagnostic.local_frame_id` / `diagnostic.wait_ms`
(:944, :1016, :1030, :1041-1043, :1113-1115) — these must land in this same commit or `paint.rs`
will not compile against the reduced `FrameReadyDiagnostic`.

**Delete (`components/shared/paint/lib.rs:298-332`):** `PaintMessage::kind()` and its `impl` block —
its sole caller is the `kinds` histogram deleted above. **Do not touch `impl Debug for PaintMessage`
at :334**, which uses `strum`, not `kind()`.

**★ The `paint.rs` barrier range cuts a load-bearing `if/else-if` chain.** The painter_paint
inventory says "REMOVE paint.rs:1000-1032". A literal range delete leaves a dangling `} else if`
and invites someone to "fix" the braces by deleting the `complete_before_deadline` arm, which
would turn every before-deadline barrier into an after-deadline one. The current shape at
`paint.rs:998-1035` is:

```rust
if barrier.ready_painter_ids.len() == barrier.expected_painter_ids.len() {
    barrier.completed_at = Some(diagnostic.ready_at);
    if barrier.missed_deadline {
        let first_to_all_ready_ms = …;  let request_to_all_ready_ms = …;
        info!("Wall frame barrier complete: … status=completed_after_deadline …");
    } else if !barrier.presentation_decision_made {
        complete_before_deadline = true;
    }
} else if !barrier.presentation_decision_made {
    debug!("Wall frame barrier progress: …");
}
```

**The correct collapsed form is exactly:**

```rust
if barrier.ready_painter_ids.len() == barrier.expected_painter_ids.len() {
    barrier.completed_at = Some(diagnostic.ready_at);
    if !barrier.missed_deadline && !barrier.presentation_decision_made {
        complete_before_deadline = true;
    }
}
```

The trailing `else if !barrier.presentation_decision_made { debug!(…) }` goes in full — the `debug!`
is its entire body.

**Keep:** `pending_frame_diagnostics` and the `wall_logical_frame_id` that travels request → ready →
`WallFrameCoordinator::note_frame_ready` → barrier completion (this is the barrier's key, not a
diagnostic); `WallFrameBarrier.first_ready_at` (→ `missed_deadline_at`, the 16 ms deadline) and
`.completed_at` (→ `WALL_FRAME_BARRIER_RETENTION`); `screenshot_poll_latch`;
`release_keep_previous_while_production_is_suspended` and the `run_liveness_probe` loop;
`drop_superseded_video_frame_updates` **with its `usize` return** (let-underscore the call site —
eight unit tests assert on that count); the `dispatch_detached` queue-limit check;
`target_painter_ids_for_source_painter` and the fan-out dispatch loop; the `pending_frames`
increment/decrement, `set_display_composite_in_flight`, `tick_dcomp_resize_rebuild`.

**Gate:** G1. **Expected:** 111/111 (no tests removed; `superseded_video_frame_tests` and
`wall_frame_keep_previous_tests` must still pass).

---

### Step 7 — the rest of `components/paint`: `LOCKWAIT`, `FRAMETIME`, `ANIMTICK`, `PAINTERLOCK`, `WEBGLEXTIMG`, `TIMELINESTEP`, and `format_step_sequence`

**Delete (by symbol):**
- `commit_guard.rs` — `mod tests` (whole module), **`emit_lockwait`**, `record_lock_wait`, the six
  `LOCK_*` statics, `record_fanout`, the three `FANOUT_*` statics; the instrumentation lines in
  `commit_all_at_tick` (:261, :299-301); `GuardRole`, the `role` parameter of `device_guard` and the
  instrumentation body of `device_guard` (the `COLLECT` check and the two `qpc_now()` reads) —
  **same commit**, drop the second argument at `dcomp_compositor.rs:2337`, `:3251-3253`,
  `:3590-3592`, `:4575-4577`; the imports at `:81` and `:86`; and the module-doc sentences that
  point at `LOCKWAIT`/`painter_us_max`/`fanout_us`.
  **★ Owner assignment (resolves the double claim).** `emit_lockwait` drains the `LOCK_*`/`FANOUT_*`
  statics, so it cannot outlive them, and its only caller is `output_grid.rs:2175`. **This step owns
  both** — delete `emit_lockwait` *and* the single line `crate::commit_guard::emit_lockwait();` at
  `output_grid.rs:2175`. `probe_loop` keeps compiling minus that line and dies in Step 12. The
  output_grid pass does **not** claim `emit_lockwait`.
  **Keep:** `DEVICE_LOCKS`/`device_lock` and both `lock.lock().unwrap_or_else(|e| e.into_inner())`
  poison recoveries; `WORKERS`/`DONE`/`CommitWorker`/`ensure_workers`/`commit_all_at_tick`'s dispatch
  loop, recv loop and sequential fallback; `SAMPLE_LEAD_PERIODS`.
- `frame_time.rs` — the FRAMETIME banner comment, `FRAME_TIME_PROF`, `DeliveryWindow`, `DELIVERIES`,
  `millis`, `record_delivered`; the `if *FRAME_TIME_PROF { record_delivered(...) }` at :83-85; the
  `painter_id` parameter of `frame_time_for_tick` — **same commit**, drop the first argument at
  `refresh_driver.rs:206-210` and `:292-296`; imports `:7`, `:9`, `:13`, `:14` and `Instant` from `:10`.
  **Keep** `FRAME_TIME_NANOS`, `note_frame_time`, `stored_frame_time`, `fresh_frame_time`,
  `frame_time_for_tick` and `Duration` — that is the rAF timestamp, not the log.
- `refresh_driver.rs` — the three `ANIMTICK` `warn!` blocks (:195-203, :225-232, :246-253), keeping
  the enclosing `if`/`return`/`set` statements and the `use log::warn`.
- `web_content_animation.rs` — `note_timeline_step`, `note_transit_clamp`, `note_rebase_jump`,
  `sample_transforms_at`, `note_built_at_gap`, `transit_overflow_seconds`, the nine
  `PipelineAnimations` stats/rebase fields, the seven statements they leave in
  `install_paint_animations`, `mod timeline_step_tests` (4 tests) and
  `transit_overflow_is_reported_only_past_the_limit` (1 test).
  **Keep `MAX_DISPLAY_LIST_TRANSIT`** — `animation_zero` clamps with it, which sets the animation
  origin — and `an_absurd_transit_is_clamped`.
- `webrender_external_images.rs` — `WEBGL_FANOUT_PROF`, `ExternalImageProfile`, the `profile` field
  and init, `maybe_emit_profile` and its call, and the eight timing wrappers (keeping
  `take_surface`, `create_texture`, `stage_to_painter_device`, `destroy_texture`, `lock_swap_chain`,
  `unlock_swap_chain`). **Keep `STAGE_TO_PAINTER_DEVICE`** — it swaps which GL texture WebRender gets.
- `paint.rs` — `PAINTER_LOCK_PROF`, `PAINTER_LOCK_WINDOWS`, `PainterLockWindow`,
  `record_painter_lock_wait`, and collapse the two `Threaded(threaded) if *PAINTER_LOCK_PROF` match
  arms into the plain arms below them.
- `painter.rs` — **now** `format_step_sequence`, `LOG_ANIM_STEP_SEQ` and `mod anim_step_seq_tests`
  (4 tests).

**Why this position (forced, two reasons).** (a) `format_step_sequence`/`LOG_ANIM_STEP_SEQ` have
four readers — `painter.rs:1691`, `painter.rs:1796` (gone in Step 5), `frame_time.rs:172` and
`web_content_animation.rs:624` (gone above in this step). Readers before definition, so the
definition can only go here. (b) This step removes `commit_guard`'s three `COLLECT` readers
(:168, :261, :358); with `dcomp_compositor.rs:1785` already gone in Step 3, only `output_grid`'s own
readers remain, which Steps 12-13 clear — so `COLLECT` never has to be re-expressed.

**Gate:** G1. **Expected:** 111 minus 1 (commit_guard) minus 4 (timeline_step_tests) minus 1
(transit_overflow) minus 4 (anim_step_seq_tests) → **101/101**.

---

### Step 8 — `winit_wall` pass timing, `MAINBUSY`, `Present perf`, `SPINSLOW`, `TileRenderTiming`, and the `LOG_PRESENT_CADENCE` trio

**Delete by symbol, never by line range.** The winit_wall inventory's ranges are wrong in four
places and two of them overlap a neighbouring struct: `ClockStats` is at **433** (not 416),
`TickIssueAudit` at **487** (not 465 — 465-479 is inside `ClockStats`'s field list and contains
`tick_by_site`), `PassStats` at **538** (not 530 — 530-535 is `MainSlot`'s body),
`report_clock_stats` at **955** and `report_main_busy` at **1060** (the inventory's ranges overlap
by five lines). Locate each item by `fn`/`struct`/`enum`/`const` name and delete to its own closing
brace.

**Delete:**
- `components/servo/servo.rs` (`SPINSLOW`): `spin_start`, `paint_messages`, `embedder_messages`,
  `embedder_start`/`embedder_ms`, `updates_start`/`perform_updates_ms`, `spin_ms`, the `if spin_ms
  > 30.0 { warn!("SPINSLOW …") }` block and the two rationale comments; reshape
  `let paint_handle_ms = {` → `{` and `};` → `}`; narrow `use std::time::{Duration, Instant};` to
  `Duration`. **Keep `paint.handle_messages(messages)`, the `try_recv_one_message` loop and
  `self.paint.borrow_mut().perform_updates()`.**
- `winit_wall/main.rs`: `report_main_busy`, `charge_main` (unwrap all four call sites to the bare
  body), `MainSlot`, `MainBusy`, `AppState::main_busy` + init; `note_present` and the three
  `present_ms_*` fields + inits; `note_render_pass` (which takes `LOG_PASS` and `PASS_EVENT_MS`
  with it), `PassStats`, `PassSplit`, `AppState::pass_stats` + init; and inside `render_all_tiles`
  the locals `pass_start`, `tile_ms`, `tile_queued_ms`, `split`, `skipped`, `dispatched_indices`
  and every `*_start`/`.elapsed()` pair.
  **Keep:** `dispatched: Vec<bool>` (not `dispatched_indices`), both
  `paint_target_keep_previous_logical_frame` barrier checks **with their `continue`s** (the
  `skipped` bumps sit beside them), `dispatch_paint_targets`, `make_current`/`paint_target`/
  `paint`/`present`, `maybe_capture`, `flush_deferred_dcomp_commits()`, and `in_flight.join()` —
  as `let _ = in_flight.join();`, because the join must complete before the commit flush.
- `TileRenderTiming` (cross-crate, one commit): `paint.rs:346-361`, the `dispatched_at`/`started`/
  `queued_ms` fill at `paint.rs:3636-3647`, the re-export at `components/paint/lib.rs:21`, the
  re-export at `components/servo/lib.rs:48`, and `winit_wall/main.rs:1823`'s use of the value.
  **`PaintTargetsInFlight::join()` stays** — it is the barrier that stops render passes overlapping;
  only its payload type becomes `()`.
- **The `LOG_PRESENT_CADENCE` trio, in this commit and no other:** `winit_wall`'s `LOG_PASS` is the
  last remaining reader of `servo_config::debug_env::LOG_PRESENT_CADENCE` (the painter-side one died
  in Step 5). So this commit also deletes `pub const LOG_PRESENT_CADENCE`
  (`debug_env.rs:163-170`), its `&LOG_PRESENT_CADENCE` entry in `ALL`, renumbers the surviving
  `cache_index` values, deletes its `###` entry in `docs/multigpu/configuration.md`, and rewrites
  `config_surface.rs:271-295` (`the_dead_knob_check_actually_catches_an_unread_flag`), which
  hard-codes the const. Rewrite it **once, against flags that survive the whole strip** — use
  `DCOMP_NO_PARTIAL_PRESENT` (referenced in the synthetic text) and `DCOMP_DISABLE_RESIZE_REBUILD`
  (not referenced), so the `assert_eq!` reads
  `assert_eq!(dead, vec!["SERVO_DCOMP_DISABLE_RESIZE_REBUILD"]);`. Step 14 then only removes `ALL`
  entries, not this test again.

**Why this position (forced).** A `pub use` of a deleted item is a hard error on the default build
path, so `TileRenderTiming`'s four sites are one commit. The `LOG_PRESENT_CADENCE` const, its `ALL`
entry and `config_surface.rs:271-295` are compile-coupled to each other and to both crate-level
readers; this is the first commit at which the last reader can go, so it is the only commit where
the whole trio can land without leaving something broken or leaving a registered-but-unread flag.
The `cache_index` renumber is safe to do twice: `debug_env.rs`'s `const _: ()` assertion makes a
stale index a compile error, not a silent bug.

**Gate:** G5 (cheap; **20/20 is not expected yet** — `every_env_name_read_in_sources_is_registered`
is still RED on the remaining unregistered names and `every_registered_flag_is_actually_read_somewhere`
is still RED on the flags stranded in Steps 3-7; what this gate must show is that
`the_dead_knob_check_actually_catches_an_unread_flag` **compiles and passes**), then G1 (101/101),
then G2.

---

### Step 9 — `winit_wall` `WALLCLOCK` and the tick-issue audit

**Order inside the step is forced by the pacing latch:**
1. **First extract the latch.** `AppState::note_tick_site` (main.rs:889-903) contains both
   instrumentation (the `tick_by_site` bump) and `last_pacing_tick_at.set(Some(Instant::now()))`
   when `site == ClockSite::PacingThread`. Replace the two call sites (:1171 in `pump_vsync`,
   :1580 in `drive_present_clock_timer`) with a three-line `note_pacing_tick(site)` that does
   **only** the latch, or inline it. `pacing_is_driving()` reads that latch and gates
   `WakerEvent::Engine if !state.pacing_is_driving()` and `needs_pacing_backstop(..)`; deleting
   `note_tick_site` wholesale makes `pacing_is_driving()` permanently false and re-opens the
   engine-wake backstop on every wake.
2. Then delete by symbol: `AppState::note_present_tick`, `note_tick_jitter`, `note_about_to_wait`,
   `note_redraw_requested`, `note_redraw_suppressed` (already caller-less), `note_clock_render_pass`,
   `report_clock_stats`, the backstop bump at :1575-1577, `let fired_for = next;`, `struct ClockStats`,
   `const MAX_CLOCK_GAP_SAMPLES`, `AppState::clock_stats` + init; `struct TickIssueAudit` + its
   `impl`, `AppState::tick_issue_audit` + init, the `debug_assert!` block, and the two unit tests
   `tick_issue_audit_accepts_increasing_numbers_and_gaps` /
   `tick_issue_audit_rejects_a_repeated_or_backwards_number`. Change the audited call to
   `let _ = webview.on_wall_tick();` — **the call issues the wall frame and must stay.**
3. Then the stale doc-comment text that cites now-dead log tags, and the clause
   "(WALLCLOCK 의 tick_by pace 가 대부분이어야 한다)" in the pacing-thread banner (keep the banner).

**Keep:** `ClockSite` (something must still tell `drive_present_clock` who called it; only the
`PacingThread` discriminant is load-bearing, and `AboutToWait` is still constructed at :2431);
`pacing_is_driving`; `servo::note_frame_time(...)` at :1609 and `servo::note_present_tick()` at
:1613 (the latter clocks `wall_tick::note_tick_edge()` — deleting it makes `commit_deadline()` and
`gfx_present_align_commit_pct` no-ops with no compile error); `self.tick_fire.fire()`;
`webview.commit_deferred_dcomp_at_tick()`; `vsync_stalled`/`vsync_tick_pending`/`vsync_tick_at`/
`vsync_due`/`vsync_armed`; `spawn_pacing_thread`, `pace_on_vblank`, `align_after_vblank`,
`snap_to_dwm_grid_at` and `qpc_frequency` in full; `notify_new_frame_ready`'s empty body;
`--capture`; `crash_report`; `device_auth_selftest`; the `FrameClock` startup banners; and the five
remaining `#[cfg(test)]` clock-wiring tests, in particular
`backstop_only_when_pacing_was_installed_but_is_not_running`, which guards step 1 above.

**Note on expectations (the inventory's justification is wrong, the verdict is not).** The claim
that `tick_issue_audit.borrow_mut()` "still runs in release builds" is false: it is the argument of
`debug_assert!`, which is compiled out. Removing it buys nothing measurable; remove it as dead
weight, not as a win.

**Gate:** G2 first (cheap relative to G1 here), then
`cargo test -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu` to confirm the
five surviving clock tests pass.

---

### Step 10 — `components/layout` and `components/script`

**Delete:**
- `layout/display_list/paint_animation.rs` — `BindTally`/`TALLY`/`tally`, `MAX_EDGE_LINES_PER_SECOND`,
  `PROP_OPACITY`, `PROP_TRANSFORM`, `property_name`, `UNBOUND_STREAK`, `BOUND_PREVIOUS`,
  `BOUND_CURRENT`, `EDGE_BUDGET_LOST`/`_GAINED`/`_FIRST`, `edge_line_allowed`, `describe_animations`,
  `note_bound`, `note_unbound`, `note_gained`, `note_unbound_transform_value`, `roll_binding_edges`,
  `note_stacking_context_skipped`, `log_built`, `log_document_animations`, and every `tally(...)`/
  `note_*(...)` statement inside `opacity_binding` and `transform_binding`; `use std::cell::RefCell;`.
  **Keep every `return`/`Some(...)`/`return None;` path**, `horizon_seconds`, `MAX_SAMPLES`,
  `SAMPLE_SECONDS`, `EPSILON`, `animated_opacity`, `animated_transform_list`, `opacity_binding`,
  `transform_binding`.
- `layout/display_list/mod.rs:332-336` and `:553-560` (keep `return false;`);
  `layout/display_list/stacking_context.rs:675-693` (the whole `if animated_transform.is_none() { … }`;
  keep `let style = self.style();` at :647).
- `script/raf_clock.rs` (whole file) and `script/lib.rs:53`; in `document.rs::resolve_frame_time`
  the `note_resolved(...)` call and the two locals that exist only as its arguments. **Keep the
  monotonic clamp, `last_frame_time`, `animation_frame_time` and
  `dom_animation_timeline_use_frame_time`** — that is the page's rAF timestamp.
- `script_thread.rs` — `TASK_WINDOW`, `TaskWindow`, `slow_task_threshold`, `note_task_duration` and
  its call, `let _ = take_microtask_time();`, `maybe_report_live_webgl_contexts` and its call,
  `LAST_WEBGL_DOM_REPORT`, `maybe_force_gc` + `LAST_FORCED_GC` + `force_gc_interval` + the `SCRIPTGC`
  warn and their call, the `rendering_update_from_timer` field/init/read, and the four stranded
  imports. **Keep `start`/`task_duration` at :1884/:2037** — the upstream `> MAX_TASK_NS` →
  `doc.start_tti()` block reads them — and **keep
  `trigger_script_thread_animation.store(true, …)`**.
- `window.rs` — `REFLOW_STATS`, `ReflowTiming`, its `Drop`, `take_reflow_stats`, and the
  `let _reflow_timing = …` binding. **Keep `emit_timeline_marker`** (upstream devtools).
- `globalscope.rs` — `PORT_MESSAGE_STATS`, `note_port_message_deserialize`,
  `note_port_message_dispatch`, `take_port_message_stats` and the four call lines (keeping
  `structuredclone::read` and `MessageEvent::dispatch_jsval`).
- `microtask.rs` — `MICROTASK_TIME`, `CheckpointTiming`, its `Drop`, `take_microtask_time` and the
  construction site.
- `document.rs` — `RENDERING_UPDATE_REASONS`, `note_rendering_update_reason`,
  `take_rendering_update_reasons` and the nine call statements inside `needs_rendering_update`
  (keep every `return true;`; the big `match restyle_reason` is the sole argument of one of them and
  goes with it). **Do not touch `Document::rendering_update_reasons` /
  `add_rendering_update_reason` / `clear_rendering_update_reasons`** — near-identical names, upstream
  decision state.
- `webglrenderingcontext.rs` — `report_live_contexts` and the `WEBGLDOM releasing …` warn.
  **★ Move the `live.retain(|context| context.is_alive())` prune out of `report_live_contexts`
  into `release_contexts_of_destroyed_document` (or into registration) before deleting it** —
  it is the only thing that drops dead `WeakRef` entries, and `LIVE_CONTEXTS` itself must survive
  because `release_contexts_of_destroyed_document` frees GPU resources through it.
- `htmlmediaelement.rs` — `LIVE_MEDIA_ELEMENTS`, `report_live_media_elements`,
  `register_for_reporting` and its call; `MediaFrameRenderer::note_media_frame_rendered`,
  `log_media_frame` and its three call sites, the four counter fields,
  `MEDIA_FRAME_INFO_INTERVAL`, and the now-unused `image_update_for_log`, `delete_update_count`,
  `inter_frame_ms`; the single `MEDIATEARDOWN` warn at :3272 (**keep
  `release_player_while_detached`**, a leak fix).
- `animations.rs` — the `ANIMUNBIND`, `ANIMDROP`, `ANIMSETDROP`, `ANIMPENDING` and `ANIMSTART`
  `warn!`s only. **Keep `set.cancel_all_animations()`, `node.dirty(NodeDamage::Style)` and
  `rooted_nodes`, both `continue;` statements and `animation.state = AnimationState::Running;`** —
  `git show 30cefa2035e2` proves the surrounding restructure is behaviour-identical to upstream and
  that the commit added only the warns.

**Keep wholesale, reason "upstream":** `components/allocator/tracking.rs` (incl. `note_allocation`),
`Node::note_dirty_descendants`, `Document::note_node_with_dirty_descendants`,
`note_pending_input_event`, `note_worker_thread`, `note_response_urls`, `note_pending_navigation`,
`GlobalScope::note_cross_realm_transform_readable`/`_writable`.

**Gate:** G4. **Expected:** clean. (G1 is unaffected — `servo-paint` does not depend on
`servo-script`.) Run G2 at the end of the step as a cross-check that the example still links.

---

### Step 11 — `components/media`

**Delete:**
- `backends/gstreamer/player.rs` — `sample_diagnostics` and its clone, `VideoSampleDiagnostics`,
  `NEXT_VIDEO_DIAGNOSTICS_ID`, `note_sample`, `note_rate`, `video_rate_logging_enabled`,
  `clock_time_ms`, `clock_delta_ms`, `format_optional_ms`, `VIDEO_SAMPLE_INFO_INTERVAL`,
  `VIDEO_SAMPLE_LATE_GAP_MS`, `VIDEO_RATE_REPORT_INTERVAL`; `stage_prof`, `prof_on`, `SinkStageProf`,
  `maybe_log`, the `mark`/`took` closures and every `p.* +=` pair; the `live_counts::` call sites;
  the `MEDIATEARDOWN` warns at :3056 and :3085.
  **★ Keep `sink_pacing` / `sink_pacer` / `sleep_before`** — created two lines from the statistics
  mutex, in the same `Arc<Mutex<…>>` idiom; it is what holds 1× playback. Keep the
  `render`/`weak_video_renderer`/`observer` plumbing.
- `backends/gstreamer/media_stream.rs:356` and `media_stream_source.rs:153` — the two
  `MEDIATEARDOWN` log lines only; the surrounding drop/detach logic is load-bearing.
- `backends/gstreamer/render-d3d11/lib.rs` — `PROFILE_ID_SEQ`, `LAST_PROF_LOG`, `FRAMES_SINCE_LOG`,
  `LAST_ARRIVAL`, `LAST_PTS_MS`, `profile_enabled()`, `profile_threshold_ms()`,
  `RenderD3D11::profile_id`, `prof_log`, `PlayerState::drop_count`, and every `D3D11PROF` warn with
  its `prof` guard and timing locals. **Edit** the `info!("D3D11 video: plane 링 생성 … (id={})")`
  line to drop the `(id={})` field — do not delete the line.
- `player/lib.rs` — `mod live_counts`.
- `media-thread/lib.rs` — `RAW_VIDEO_PLANE_LOCK_COUNT`/`_UNLOCK_COUNT`/`_INFO_INTERVAL` and their
  debug+info blocks, the `LAST_CONSUME_SHAPE` string-builder inside `consume_plan`,
  `D3D11_IMPORT_COUNT`/`_TOTAL_US`/`d3d11_profile_enabled()`, `diag_logged_wrap_ok`/`_fail` and the
  two `WALLDIAG wrap` lines, the `WALLDIAG consumer-device publish` info, the six `lock_window_*`
  fields and the `stage_consume_ms`/`stage_wrap_ms`/`wrap_cached` locals, `MEDIA_LOCK_SLOW_MS` and
  both `MEDIALOCK` slow-lock warns, the `Slow raw plane unlock` warn with its locals, and the whole
  `MEDIALOCKRATE`/`MEDIALOCKSITES`/`MEDIARINGS`/`MEDIAPLAYERS` emit block.
  **★ Two log lines the script_media inventory's ranges miss, and a trap attached to them:** delete
  `info!("MEDILOCKDEFER …")` at `:913-918` and `info!("MEDIAWRAPDEFER …")` at `:1048-1053` —
  **the statements only.** The counters they print (`ring_init_frame`/`done`/`deferred`,
  `wrap_init_frame`/`done`/`deferred`) implement the per-render-frame admission budgets in
  `may_initialize_ring_now` and `roll_wrap_budget_frame` and are **load-bearing**.
  **Keep** `note_demand_and_ring`, `plan_or_presenting_plane`, `consume_plan`, `presenting_plane`,
  the wrap cache, the ring/wrap budgets, `D3d11PlaneRings::set_consumer_device`, and
  `warn!("D3D11 media: EGLImage 래핑 실패 …")`.
- `player/d3d11_ring.rs` — `LockStats`, `REGISTRY_LOCK`, `REMOVED_LOCK`, `TrackedGuard`,
  `lock_tracked`, `take_top_registry_sites`, `SITE_COUNT`/`SITE_NAMES`/`SITE_WAIT_NS`/`SITE_HOLD_NS`/
  `SITE_HITS`, `site`, `ring_inventory`, `removed_queue_inventory`, `group_inventory`,
  `D3D11_TEXTURES_CREATED` (and its `fetch_add` at `render-d3d11/ring_producer.rs:250` + import).
  **★ Replace, do not just delete, the lock helpers.** `lock_at`/`lock_registry`/`lock_removed`
  carry `m.lock().unwrap_or_else(|p| p.into_inner())`. Introduce
  `fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> { m.lock().unwrap_or_else(|p| p.into_inner()) }`
  and rewrite all **25** `lock_at(` sites to `lock_recover(`. Losing the poison recovery turns one
  panicking thread into a permanent video stall across the wall.
  **Keep** `note_plane_lock_and_plan`, `note_plane_unlock`, `note_demand`, `note_demand_and_ring` —
  they match the `note_*` pattern exactly and are all load-bearing.
- `components/shared/paint/rendering_context.rs` — the `D3D11_TEXTURES_RELEASED` static and the
  `fetch_add` at `:1186` (its sole reader was the `MEDIARINGS` line deleted above).
  **Leave the `unsafe { … Release(); }` at `:1187-1190` alone.**

**Gate:** G1 (covers `media`/`servo-media`), then G2 (the only gate that compiles the gstreamer
backend, via `--features media-gstreamer`). **Expected:** 101/101 and clean.

---

### Step 12 — `output_grid`: the probe thread, every emitter, and `comp_stats.rs`

**Delete (by symbol, `components/paint/output_grid.rs`):**
`start_probe` and the `#[cfg(windows)] start_probe();` line inside `note_present_tick_now`;
`probe_loop`; `announce`; `publish_names`; `forget_vanished`; `throttled`; `period_from_pair`;
`wait_two_vblanks`; `Enumeration::names`; `grid_is_fresh`; `grid_for_monitor`; `OutputGrid`;
`GRID`; `NAMES`; `name_for_monitor`; `emit_outphase`, `emit_comp_stats`, `emit_missevent`, `bump`,
`TargetWalk`, `emit_samplelead`, `emit_sampleslip`, `period_ms_for_log`, `emit_tickcommit`,
`emit_commitgap`, `emit_commitcycle`, `emit_bindopen`, `emit_frameid`, `emit_dcompstat`,
`pass_before`, `PassMark`, `compositions_advanced`, `commit_at`; and the statics they drain —
`SLIPS`, `PUMP_FROM_RENDER/IDLE/TICK`, `FRAME_IDS`, `DCOMP_STATS`, `DCOMP_STAT_FAILED`, `COMMITS`,
`COMMIT_GAPS`, `LAST_COMMIT`, `COMMIT_STAMPS`, `BIND_SPANS`, `TICK_QPC`, `PASS_RING_CAP`,
`PASS_RING`, `PASS_SEQ`, `TICK_TO_COMMIT`, `LEADS`, `LEAD_MISSING`, `DcompSample`, `SlipTally`.
Delete the ten probe/arithmetic tests and reduce `mod tests`'s
`use super::{compositions_advanced, composition_target, period_from_pair};` to
`use super::composition_target;` **in the same edit**, or the test module will not compile.

**★ Owner assignment (resolves the second double claim).** **This step owns
`components/paint/comp_stats.rs`.** `output_grid::emit_comp_stats` is its sole consumer
(`completed_frame_id`, `sample_frame`, `FrameSample`, `TargetSample`), so delete the whole 274-line
file **and** `mod comp_stats;` at `components/paint/lib.rs:135-136` in this commit. The paint_misc
pass does not claim it. Note for the record: this removes the ability to read
`COMPREFRESH refresh_d[2]`, which the project memory records as the current judder verdict metric.
That is the stated intent of the strip — restarting from naked-eye observation — but it should be
said out loud before the commit lands.

**Keep:** the three behaviour tests `the_target_is_always_on_the_grid_and_advances_one_period`,
`tiles_reading_the_same_composition_agree`, `nominal_period_picks_the_exact_candidate`;
`qpc_frequency`, `qpc_now`, `nominal_period_ticks`, `display_frequency_hz`, `COMPOSITION`,
`composition_grid`, `composition_target`, `lead_to_next_composition`, `wait_primary_vblank`,
`VblankSource` and its `new`/`refresh`/`wait`/`Drop`, `Output`, `Enumeration`,
`Enumeration::release`, `Enumeration::needs_reenumerate`, `is_primary`, `enumerate_outputs`.
**The `IDXGIOutput::WaitForVBlank` at `VblankSource::wait` is the frame clock** — only the two in
`wait_two_vblanks` go.

**Why this position.** The emitters are what read `GRID`/`NAMES`/the sample statics, so they must go
before those statics; and `NAMES` can only die after Step 1 settled `note_commit_failure`. The
`COLLECT` definition stays untouched here, so collection remains exactly as off-by-default as it is
today.

**Gate:** G1. **Expected:** 101 minus 10 → **91/91**.

---

### Step 13 — `output_grid`'s `note_*` entry points, the two reductions, and `COLLECT`/`PROBE_ENABLED`/`DCOMP_BIND_PROF`

**Delete (each with its out-of-file caller line, same commit):**
- `note_pacing_vblank` ⟷ the call in `VblankSource::wait`; `VblankSource::log_anchor` and
  `last_log_qpc` ⟷ the call in `wait`. Keep the `WaitForVBlank`, the `qpc_now()`, the period
  fallback and the `Some((vblank, period))`.
- `note_frame_id` ⟷ `painter.rs:2584-2588`.
- `note_pump_from_render/idle/tick` ⟷ `painter.rs:5046-5052`.
- `note_sample_lead` / `note_sample_lead_missing` ⟷ `painter.rs:4330` / `:4334`
  (keep `Some(anchor + lead)` and `None`).
- `note_dcomp_stat_failed` ⟷ `painter.rs:4299` and `dcomp_compositor.rs:2583`; collapse the match to
  `if let Ok(compositor) = shared.try_borrow() { compositor.note_frame_statistics(monitor); }`.
- `note_commit_at`, `commit_stamp`, `note_commit_stamp`, `note_tick_to_commit`, `note_bind_span` —
  their call sites went in Step 4, so these are now caller-less.
- `note_present_tick_now` ⟷ `components/paint/lib.rs:99-100` — **delete only those two lines**
  (`#[cfg(windows)]` + the call). `pub fn note_present_tick()` itself stays, because
  `crate::wall_tick::note_tick_edge()` on the line above drives the external-present budget and
  `commit_deadline()`.
- `note_sample_slip` ⟷ the call inside `lead_to_next_composition`.
- `Output::adapter_luid`, the `IDXGIAdapter1::GetDesc1` block in `enumerate_outputs` and the
  `DXGI_ADAPTER_DESC1` import.

**Reduce, do not delete:**
- `note_dcomp_stat` → keep the signature's `last`/`freq`/`rate_num`/`rate_den`, the guard (drop its
  `note_dcomp_stat_failed()` body to a bare `return`), the `period_ticks` computation and the
  `COMPOSITION` store. Delete everything else and the now-unused `monitor`/`device`/`now`/`next`/
  `qpc_at_read` parameters, and the `qpc_at_read` read at `dcomp_compositor.rs:2591`.
  **★ This store is the only writer of the grid that `composition_grid()` →
  `lead_to_next_composition` → `painter::paint_animation_sample_at` reads to choose the instant the
  page's animation is sampled at. It sits three lines above the `COLLECT` gate on purpose.**
- `lead_to_next_composition` → delete `phase_ms`, `residue_ms`, the `note_sample_slip` call and the
  `monitor` parameter; update `painter.rs:4327-4328`. Keep the `anchor`/`qpc_now()` adjacency and
  the `composition_target` call.

**★ Then, in this same commit, retire the master switch without ever flipping it.**
`COLLECT` is `*PROBE_ENABLED && *dcomp_compositor::DCOMP_BIND_PROF` — collection is **off** unless
`SERVO_DCOMP_BIND_PROF` is set. The dcomp inventory proposed re-expressing it as `*PROBE_ENABLED`
when `DCOMP_BIND_PROF` dies; that would make collection **on by default** for every intervening
commit and confound exactly the by-eye comparison this strip exists to restart. **Do not do that.**
By this point every reader is gone: `dcomp_compositor.rs:1785` (Step 3), `commit_guard.rs:168/261/358`
(Step 7), `output_grid.rs:2163` and the ~28 in-file `note_*`/`emit_*` gates (Steps 12 and above).
So delete, together: `COLLECT`, `PROBE_ENABLED`, the `WALL_NO_PROBE` read, and
`dcomp_compositor::DCOMP_BIND_PROF`; and **rewrite `painter.rs:4285-4290`** from

```rust
if !*crate::dcomp_compositor::DCOMP_BIND_PROF &&
    crate::commit_guard::SAMPLE_LEAD_PERIODS.is_none()
{ return; }
```

to

```rust
if crate::commit_guard::SAMPLE_LEAD_PERIODS.is_none() { return; }
```

Writing a bare `return;`, or deleting the call, kills the B2 animation grid silently — the in-file
comment records that this exact mistake was made once already (design doc C5).

Finally prune `output_grid.rs`'s now-unused imports: `HashMap`/`HashSet`/`VecDeque`,
`AtomicU64`/`Ordering`, `Instant` (keep `Duration`), `servo_base::id::PainterId`. Keep `std::ptr`,
`LazyLock`/`Mutex`/`OnceLock`, `log::warn`, `winapi::Interface` and the `winuser`/`wingdi`/
`profileapi` imports, and the module-level `#![allow(unsafe_code)]`.

**Expected shape after this step:** `output_grid.rs` drops from 2853 lines to roughly 600-700.

**Gate:** G1 then G2. **Expected:** 91/91 and clean.
`every_registered_flag_is_actually_read_somewhere` is now RED on everything that remains in `ALL`
from the removed set; Step 14 fixes it.

---

### Step 14 — `debug_env` registry, the unregistered env names, `config_dump` and the docs

**Delete, all three edits per flag in one pass and renumber `cache_index` once:**
`D3D11_PROFILE`, `D3D11_PROFILE_MS`, `DCOMP_DEBUG`, `DCOMP_READBACK`, `DCOMP_VALIDPROBE`,
`VIDEO_ESCAPE_PROF`, `MEDIA_VIDEO_RATE`, `FRAME_REASON_PROF`, `MEDIA_SINK_PROF`,
`WEBGL_FANOUT_PROF`, `WALL_NO_PROBE`, `DCOMP_BIND_PROF`, `DCOMP_SLOW_COMMIT_MS`, `FRAME_TIME_PROF`,
`WALL_PAINTER_LOCK_PROF` — for each: the `pub const` block, its `&NAME` entry in `ALL`, and the
renumbered `cache_index`. `LOG_PRESENT_CADENCE` is already gone (Step 8). The `const _: ()`
assertion at `debug_env.rs:395-405` turns any stale index into a compile error, so renumber all
fifteen and compile once.

**Also in this commit:**
- `components/config/config_dump.rs:27-34` — the debug-env half of `log_effective_config`.
  **Keep the pref-diff half** (`:19-26` + `prefs.rs:1469`): after this strip it is the only record in
  the log of which configuration produced what the eye saw.
- `docs/multigpu/configuration.md` — exactly the `###` entry of each deleted flag, the stale-notice
  line at `:200`, and the counts in the heading at `:201` and the title at `:1`. The doc tests
  `every_knob_appears_in_the_configuration_doc` and `the_doc_repeats_the_knob_descriptions_verbatim`
  compare after `split_whitespace().join(" ")`, so line wrapping is free but wording is not: if you
  edit a *surviving* flag's `doc` string, make the identical edit here.
- `docs/multigpu/configuration_manual.md:302-332` (section 6) and the `debug env:` sentence in
  section 2. No test reads this file.
- The remaining unregistered `SERVO_*` literals: `SERVO_MEDIA_LOCK_SLOW_MS` and
  `SERVO_SCRIPT_SLOW_TASK_MS` were removed in Steps 11 and 10; `SERVO_LOG_ANIM_STEP_SEQ` in Step 7;
  `SERVO_WR_SLOW_MS` is launcher-only (Step 16, and nothing in `components/` ever read it).
  Confirm with a grep that none remains.

**Gate:** G5 first (seconds, and it is the test this step exists to turn green), then G1, G2, G4.
**Expected: `config_surface` 20/20** — `every_env_name_read_in_sources_is_registered` goes from RED
to GREEN, and `every_registered_flag_is_actually_read_somewhere` returns to GREEN. This is the
independent success signal for the whole strip. G1 91/91.

---

### Step 15 — the engine orphans: `interval_summary` and the stranded winapi features

**Delete:**
- `components/shared/base/interval_summary.rs` (whole file, 185 lines, incl. its 8 unit tests) and
  `pub mod interval_summary;` at `components/shared/base/lib.rs:15`.
  **Why last (forced).** It has exactly four consumers, all removed above:
  `paint/frame_time.rs:14` (Step 7), `paint/paint.rs:130` (Step 7), `paint/painter.rs:4966`
  (Step 7, inside `format_step_sequence`), `script/raf_clock.rs:31` (Step 10). Three inventories
  each said "do not delete it, another file still uses it", each looking at a different consumer —
  which is how it ends up shipped as dead code. It is a `pub mod` in a shared crate, so nothing
  warns about it.
- `components/paint/Cargo.toml` — the winapi features `"psapi"` (used only by
  `process_memory_mb`, Step 5) and `"dxgi1_4"` (used only by `log_gpu_memory`, Step 5).
  **★ Keep `"processthreadsapi"`** — it has three use sites and the third, `SetThreadPriority` at
  `painter.rs:904`, is functional; after the strip the feature *looks* orphaned by inspection of the
  two obvious sites and is not. **Keep `"profileapi"`** (`qpc_now`/`qpc_frequency`), **`"dwmapi"`**
  (`composition_grid`), **`"winbase"`** (`THREAD_PRIORITY_ABOVE_NORMAL`) and the whole `dcomp`
  transitive closure — winapi 0.3.9 features are empty arrays and the closure must stay intact.

**Note:** pulling a winapi feature while a `use` of its module remains gives an unresolved-module
error that reads like a toolchain problem. Do this only after a clean G1.

**Gate:** G3 and G4 first (cheap; `servo-base` is in both graphs), then G1. **Expected:** 91/91.

---

### Step 16 — the launcher, its tools, and the packaging

Not compiled; nothing in the tree depends on it. **`etc/multigpu/run_wall_dist.ps1` is UTF-8 with
BOM** — an editor that strips the BOM makes PowerShell 5.1 decode it as cp949, and a Korean comment
line then swallows the following newline and the parser fails somewhere unrelated with
`Unexpected token '}'`. Verify after every edit with `Get-Command .\run_wall_dist.ps1 -Syntax`
(silence means it parsed). Edit **bottom-up** so the line numbers below stay valid.

1. The whole analysis section, `:1345-2715` (24 parser blocks, 1371 lines — just over half the
   file). Per the user's confirmed scope this includes the `RUNCONFIG` append at `:1346-1368`, the
   `$runConfig`/`$engineMd5`/`$stderrTransport` provenance at `:1140-1169` and the RUN CONFIG
   banner. The pref-diff half of `config_dump::log_effective_config` (kept in Step 14) becomes the
   replacement record of what a run was configured with.
2. The `-ThreadCpu` sampling block `:1292-1318`, then the **unconditional** `thread_cpu_probe`
   block `:1254-1289` — the latter costs every run a three-second sleep and an extra process and is
   gated by no switch at all. Highest-value launcher removal.
3. Banner lines `:1088`, `:1087`, and the instrumentation half of `:1079` — **a partial line**; the
   `immediate_composite=`, `dcomp=`, `sample_lead=`, `pacing_*` parts name real prefs and stay.
4. The env-assignment block `:910-930`. **★ Keep `:905-907`, the `SERVO_*` wipe** — it is what makes
   `removed_env`'s startup blocking work and what makes shell-set diagnostic env invisible to the
   engine.
5. The pairing guards `:983-986`, `:800-802`, `:785-787`.
6. Parameter declarations bottom-up: `-ThreadCpuWarmupSec`, `-ThreadCpu`, `-VideoEscapeProf`,
   `-DcompDebug`, `-PainterLockProf`, `-FrameTimeProf`, `-AnimStepSeq`, `-PresentCadence`,
   `-NoCanvasAckRecovery`, `-ForceGcSec`, `-CanvasAckSkip`, `-TileThreadSpike`, `-DcompBindProf`,
   `-FanoutProf`, `-SinkProf`, `-WrSlowMs`, `-FrameReason`, `-VideoRate`, `-D3d11ProfileMs`,
   `-D3d11Profile`, `-LegacyLogRedirect`, `-QuietLog`, `-KeepRustLog`, `-NoLog`.
   `-WrSlowMs` can go first and alone — nothing in `components/` ever read `SERVO_WR_SLOW_MS`.
   **Keep `-Pref` passthrough** (`:1060`) — the escape hatch that makes every removed switch
   re-creatable by hand — and the `RUST_LOG` token validator (`:959-981`), a correctness guard.
   `-NoLog` and the default `RUST_LOG` assignment are flagged under Decisions.
   **Keep `-PacingOutput`** (`:247-254`, `:1001`) — see Decisions.
7. `etc/multigpu/make_wall_dist.ps1` — the `thread_cpu_probe` build+copy, the
   `probe_machine_topology.ps1` and `run_frame_time_ab.ps1` copies, and the `-ThreadCpu` usage hint.
   **Keep the probe-page copies** (`wall_anim_jitter_probe.html`, `ticker_slice_probe.html`,
   `raf_clock_probe.html`, `wall_paint_animation_probe.html`) — those are the naked-eye probes the
   user is restarting from; only the switch recipes in their comment blocks go stale.
8. `etc/multigpu/tools/` — delete `run_frame_time_ab.ps1`, `thread_cpu_probe/`, `topology_probe/`,
   `probe_machine_topology.ps1`, `wall_perf_analyzer/`, `parse_ceiling_logs.ps1` and the two sweep
   drivers that feed it (`run_video_ceiling_sweep.ps1`, `run_video_collapse_ceiling.ps1`).
   **Keep** `measure_decode_only.ps1`, `serve_http.ps1`, `verify_gpu_fanout.ps1`,
   `verify_ndebug_webgl.ps1`, `wall_mouse_hittest_probe.ps1`, `wall_synth_mouse_wiggle.ps1`,
   `run_single_capture.ps1`, `run_three_wall_capture.ps1`.
   `check_freeze_log.ps1` dies on its own once `WALLTICKRATE`/`FRAMEREASON`/`ANIMSTEP` are gone —
   flagged under Decisions.
9. **Keep all of `docs/ai-notes.md`, `docs/multigpu/wall_*.md`, `etc/multigpu/*_STATUS.md`,
   `docs/superpowers/**` and `.superpowers/sdd/**`** — they are history, not a switch reference.

**Gate:** `Get-Command .\run_wall_dist.ps1 -Syntax` and the same for `make_wall_dist.ps1`; then
G5 to confirm the config-surface suite is still 20/20 (the launcher is not scanned by it, so this
is a no-change check).

---

## 3. Kept and why

**Frame clock (winit_wall + paint)**
- `IDXGIOutput::WaitForVBlank` in `VblankSource::wait`, `wait_primary_vblank`, `pace_on_vblank`,
  `spawn_pacing_thread` — this is the clock the wall runs on; two other `WaitForVBlank` calls in the
  same file are the probe's and go.
- `composition_grid()` / `servo::dwm_composition_grid` / `snap_to_dwm_grid_at` — the timer-fallback
  arm of that clock; six of its seven callers are log lines, the seventh moves the tick.
- `nominal_period_ticks`, `display_frequency_hz`, `EnumDisplaySettingsW` — the pacing period.
- `is_primary` / `GetMonitorInfoW` at `VblankSource::refresh` — picks the default pacing output.
- `enumerate_outputs`, `Enumeration::release`/`needs_reenumerate`, `qpc_now`, `qpc_frequency`.
- `align_after_vblank` (`-DwmAlign`) — an A/B knob that moves the tick, so a timing decision.
- `AppState::last_pacing_tick_at` + `pacing_is_driving()` — the latch inside `note_tick_site`;
  without it the engine-wake backstop fires on every wake.
- `servo::note_present_tick()` → `wall_tick::note_tick_edge()` — the tick edge behind
  `ExternalPresentBudget` and `commit_deadline()`.
- `tick_refresh_driver`'s callback `Mutex` and `vsync_refresh_driver`'s `Mutex`+`Condvar`.

**Paint-side animation (B2)**
- `COMPOSITION` and its write inside `note_dcomp_stat`, deliberately ahead of the `COLLECT` gate;
  `composition_grid`, `composition_target`, `lead_to_next_composition`.
- `DCompNativeCompositor::note_frame_statistics` / `IDCompositionDevice::GetFrameStatistics` —
  named as a removal candidate in the brief; in this tree it is the feed for that grid.
- `painter::note_dcomp_frame_statistics` (reduced to the `SAMPLE_LEAD_PERIODS` arm),
  `paint_animation_sample_at`, `tile_monitor()`, `commit_guard::SAMPLE_LEAD_PERIODS`.
- `last_paint_animation_push_at`, `last_render_started_at`, `append_held_paint_values`,
  `install_paint_animations`, and `web_content_animation`'s whole sampling path incl.
  `MAX_DISPLAY_LIST_TRANSIT`.

**Wall frame barrier and production**
- `pending_frame_diagnostics` / `PendingFrameDiagnostic` / `FrameReadyDiagnostic` (reduced) — the
  request→ready frame-id correlation the barrier runs on, despite the name.
- `WallFrameBarrier.first_ready_at` (→ the 16 ms deadline) and `.completed_at` (→ retention).
- `wall_tick.rs` in full — zero removals; every counter there is read by a decision.
- `PumpSource::may_schedule`, `wall_tick_frames_produced`, `bootstrapped_pipelines`,
  `display_composite_in_flight(_since)`, `screenshot_poll_latch`, `PaintTargetsInFlight::join()`.

**DComp correctness**
- `commit_guard::DEVICE_LOCKS`/`device_lock`/`device_guard` and both poison recoveries;
  `WORKERS`/`DONE`/`commit_all_at_tick` (the resident commit pool, worth a measured ~335 µs/tick).
- `raw_commit`'s `Commit()`, `commit_at_tick_locked`'s guard, `quantize_update_rect` +
  `UPDATE_QUANTUM_PX` and their seven tests, `PRESENT_SYNC_INTERVAL`, `probe_partial_present`,
  `should_fast_present`, `escaped_external_count`, `withheld_frames`, `RemoveAllVisuals()`,
  `commit_pending`, `needs_flush`/`gl().flush()`.
- `note_commit_failure` (reduced) — a TDR / `SURFACE_BEING_RENDERED` error report, not a statistic.

**rAF / script**
- `frame_time.rs`'s `FRAME_TIME_NANOS`/`note_frame_time`/`stored_frame_time`/`fresh_frame_time`/
  `frame_time_for_tick`; `Document::resolve_frame_time` and its monotonic clamp;
  `ScriptThread::pending_frame_time`; `TickAllAnimations`' `CrossProcessInstant`.
- `start`/`task_duration` in `script_thread.rs` — the upstream TTI block reads them.
- `Document::rendering_update_reasons` / `add_` / `clear_` — upstream decision state, near-identical
  name to the removed fork tally.
- `start_pending_animations_before_reflow`, `node.dirty(NodeDamage::Style)` in the `sets.retain`
  closure, both `continue;` in `start_pending_animations`.

**GPU resource lifetime**
- `webgl_thread::maybe_reclaim_idle_contexts` with `released_contexts`/`context_recipes`/
  `last_composited`/`last_reclaim_sweep` and the destroy/restore bodies;
  `script`'s `LIVE_CONTEXTS` and `release_contexts_of_destroyed_document` (prune moved, not deleted);
  `release_player_while_detached`; `D3d11PlaneRings::expire_all_stale_demand` and the
  `last_gpu_memory_log_at` gate that throttles it; `drop_superseded_video_frame_updates` with its
  `usize` return.
- `paint_api::render_frame` (`begin_render_frame`/`current_render_frame`) — the denominator of
  `media_ring_init_max_per_frame` and the wrap budget.
- `may_initialize_ring_now` / `roll_wrap_budget_frame` and their `ring_init_*`/`wrap_init_*`
  counters — admission control printed next to the thing it gates.
- `d3d11_ring::note_plane_lock_and_plan` / `note_plane_unlock` / `note_demand` /
  `note_demand_and_ring`, and `lock_recover`'s poison recovery.

**Pixel-path switches that read like knobs**
- `WEBGL_SWAP_SYNC`/`gfx_webgl_swap_sync` and the `gl.flush()`/`gl.finish()` it selects;
  `STAGE_TO_PAINTER_DEVICE`; `gfx_paint_side_animation_tick_divisor`;
  `gfx_paint_side_animation_horizon_ms`; `gfx_present_align_dwm_pct`; `gfx_present_align_commit_pct`;
  `gfx_vsync_phase_pct`; `gfx_wall_pacing_thread`/`_vblank`; `gfx_painter_detached_queue_limit`.
  **No pref in `components/config/prefs.rs` is removed by this plan** — if a later diff deletes a
  `gfx_*` pref, something was misclassified.

**Locks and error paths**
- Every `paint_api::angle_gl_lock` guard (three of them sit adjacent to deleted stopwatches);
  `ANGLE_GL_EXCLUSION`, `ANGLE_GL_DEVICE_LOCKS`, `AngleGlGuard`.
- `report_unknown_external_image` and its `SEEN` rate-limiter; `set_dcomp_mode`'s one-shot warn;
  `wall_layout.rs`'s configuration warnings; `dxgi_luid_for_gpu_index`'s LUID-mismatch eprintln;
  `clone_webgl_command` (its round trip balances one `Arc` strong ref);
  the fan-out drop `warn!` in `create_webgl_context`.
- `crash_report`, `device_auth_selftest`, `--capture`, `removed_env::check_or_exit`,
  `notify_new_frame_ready`'s deliberately empty body, `schedule_wall_production` (a dead but
  intentional public door, not a probe), `config_dump`'s pref-diff half.

**Upstream, untouched:** `components/allocator/tracking.rs`, `components/profile` and
`shared/profile`, `setup_logging`/`BothLogger`/`FromEmbedderLogger`, the `tracing-perfetto` layer,
`Window::emit_timeline_marker`, all upstream `note_*` in `script/`, and every directory listed as
unchanged against `study/servo-base`.

---

## 4. Decisions needed

| # | question | recommendation | cost if wrong |
|---|---|---|---|
| D-1 | **`-PacingOutput` / `gfx_wall_pacing_output` / `output_grid::PACING_OUTPUT_PIN` + `pinned_index`.** It selects which physical `IDXGIOutput` the pacing thread blocks on, so it is a timing decision by the stated rule — but it exists only because of the 2026-10-26 "which output is the DWM grid on" A/B. | **Keep all three** (pref, launcher switch, `pinned_index`). The brief already rules `-PacingOutput` out of scope; keep the engine side consistent with it. | If removed: the wall is silently pinned to the primary output and that A/B cannot be re-run without re-adding code. If kept: one unused string pref and ~20 lines. |
| D-2 | **The nine fault-injection / kill-switch `debug_env` flags** (`WALL_FRAME_DELAY_{TARGET_INDEX,AFTER,COUNT}`, `DCOMP_NO_PARTIAL_PRESENT`, `DCOMP_DISABLE_RESIZE_REBUILD`, `DCOMP_DISABLE_RESIZE_VIRTUAL`, `MEDIA_DISABLE_ENOUGHDATA_BACKOFF`, `WALL_CANVAS_ACK_SKIP`, `WALL_DISABLE_CANVAS_ACK_RECOVERY`) plus `SERVO_SCRIPT_FORCE_GC_SEC`, and the code they drive (`WallFrameDelayInjection`, `canvas_ack_skips_left`/`consume_canvas_ack_skip`). They print nothing; they change behaviour on purpose. | **Keep**, and keep their `configuration.md` entries so the doc tests stay green. They fail the stated REMOVE rule (they are not "exists only so a number can be printed"). `SERVO_SCRIPT_FORCE_GC_SEC` is the weakest — it is default-off but changes collection timing; remove it with the `SCRIPTGC` warn if you want it gone. | If removed: you lose the reproduction tools for the barrier-delay and canvas-ack-deadlock bugs, and `paint.rs:4583-4613` (`a_suspended_tick_leaves_the_injected_skip_observation_alone`) must be deleted. If kept: ~400 lines of inert code and 9 registry entries survive. |
| D-3 | **Scope of B2** — `gfx_sample_lead_periods` / `SAMPLE_LEAD_PERIODS` / `note_frame_statistics` / `GetFrameStatistics` / `COMPOSITION` / `composition_grid` / `composition_target` / `lead_to_next_composition` / `paint_animation_sample_at` / `monitor_for_hwnd` + `tile_monitor`. This plan keeps the whole chain. If you also want the paint-side lead prediction gone, all of it goes in one extra step. | **Keep.** It changes what is drawn, so by the stated rule it is not instrumentation — and it is the one thing in this strip that fails silently. | If removed by mistake: `-SampleLead` degrades to "sample at now" on every tile — a pixel-level change with no error, no log and no test failure, which is the worst possible outcome for a naked-eye restart. If kept unwanted: `GetFrameStatistics` keeps being called once per tile per frame and ~250 lines survive. |
| D-4 | **`--tile-thread-spike` / `TILESPIKE`** — `winit_wall/main.rs:94-96, 141, 180, 264-268, 1896-1906, 2047-2058` and `tile.rs:332-474` (474 lines), plus the launcher `-TileThreadSpike`. A separate CLI mode that answers "can a worker thread own a `RenderingContext`?", prints `create_ms` and exits. | **Remove**, as its own commit after Step 9. It is a feasibility probe that already returned its answer and costs nothing at runtime, so this is a tidiness call, not a safety one. | If removed and wanted back: it is a self-contained block, recoverable from git. If kept: 474 lines of a mode nobody runs. |
| D-5 | **`gfx_wall_rotate_tile_order`** — `prefs.rs:483,1327`, `winit_wall/main.rs:1681-1699` + `pass_counter`, launcher `-RotateTileOrder`. Its own doc says it exists to decide whether a slow tile is "the first position" or "that painter" — i.e. to disambiguate a measurement — but it genuinely changes draw order when enabled (default `false`). | **Keep.** It prints no number, and removing it deletes a pref and a launcher switch in exchange for nothing at the default. | If removed: a pref disappears from `prefs.rs` (and this plan's claim that no pref is removed becomes false, weakening that invariant as a check). If kept: `pass_counter` survives as its only consumer. |
| D-6 | **Misconfiguration alarms:** `media-thread`'s `note_media_d3d11_device` / `conflicting_device_owner` / `MEDIA_D3D11_DEVICE_OWNERS` + `mod wall_gpu_fanout_tests`, and `tile.rs:229-237`'s LUID cross-check eprintln. Fork-added, print only a warning, change no pixel. | **Keep both.** They are alarms for a silent multi-GPU fan-out failure (2026-08-24) and for the wrong adapter driving a tile — exactly the class of bug a 4-GPU wall cannot see by eye. | If removed: a mis-bound GPU produces a wall that looks nearly right and nothing says why. If kept: one static, one warn, one unit test. |
| D-7 | **The launcher's logging plumbing** — `-NoLog`, `-QuietLog`, `-KeepRustLog`, `-LegacyLogRedirect` and the default `RUST_LOG` assignment at `:951-958`. The brief puts all four in scope. | Remove `-QuietLog`, `-KeepRustLog` and `-LegacyLogRedirect`; **keep `-NoLog`** (the only way to run with no log file at all, which is what naked-eye observation wants) and set the default `RUST_LOG` to `warn`. Note the memory record: the 127 µs write cost was `-LegacyLogRedirect`-only, and "logs hide the judder" was disproven. | If `-NoLog` is removed: every run writes a log file you did not ask for. If the default `RUST_LOG` is left as-is: a mostly-empty but still-noisy log, since the `paint=info`/`media=info` targets lose almost all their content. |
| D-8 | **`vendor_local/webrender` `[patch.crates-io]`** (root `Cargo.toml:426-431`). The patch's own comment says it is a local instrumented copy that adds one `WRSLOW` line on slow frames and should be reverted once the cause is found; nine commits touch it, all titled `wr(local)`/`diag`/`probe`. Not in any inventory's step list. | **Remove, in its own commit, after Step 16.** Read the nine commits first to confirm none carries a non-instrumentation change. | If removed carelessly: a real vendored fix is silently reverted along with `WRSLOW`. If kept: the wall keeps running a non-upstream WebRender whose only delta is a probe. It is a `Cargo.lock`-affecting change — expect a full ~11 min rebuild. |

---

## 5. Unresolved

1. **`cargo check -p canvas` and `cargo check -p layout -p script` do not resolve.** No package is
   named `canvas`, `layout` or `script`; the packages are `servo-canvas`, `servo-layout`,
   `servo-script` (the bare names are `[lib] name` values). Substituted throughout as G3/G4.
2. **The brief's claim that the `servo-paint` gate does not compile `components/webgl` is false.**
   `components/paint/Cargo.toml:56` has `webgl = { workspace = true }`. G1 does compile
   `servo-webgl`, which is why its own `--features webgl/no-wgl` resolves. The Step 2-before-Step 14
   ordering is kept anyway.
3. **`ORPHANED_FRONT_BUFFERS`** (`webrender_external_images.rs:93`, bumped at `:162` and `:434`) and
   **`WebGLExternalImages::logged_locked_surfaces`** (`:83`). Both feed only a log, but I could not
   establish whether either increment site also performs a leak-avoiding action the counter merely
   narrates. Left in place by Step 7; decide separately.
4. **`ActivePaintAnimation::complete`** (`web_content_animation.rs:278-284`) — written at install
   time from `PaintAnimation::complete`, never read anywhere in the tree. Dead diagnostic state, but
   removing it touches a shared display-list type's consumer. Left in place.
5. **`RingState::dropped_frames` / `D3d11PlaneRings::dropped_frames()`** — its only non-test reader
   is a `D3D11PROF` line removed in Step 11, but two unit-test assertions
   (`d3d11_ring.rs:1507`, `:1516`) use it to prove drop behaviour. Step 11 keeps the counter and the
   assertions; rewriting them to observe the behaviour directly is a separate task.
6. **`thread_name.rs`** (`tag_video_streaming_thread` / `tag_audio_streaming_thread`) mixes
   `SetThreadDescription` + the `THREADMAP` line (measurement) with `pin_to_node` (a NUMA
   performance control gated by `media_numa_pin_streaming_threads`) that consumes the index the
   naming counter produces. A clean split is a rewrite, not a deletion. Left untouched.
7. **`WITHHOLD_WARN_FRAMES`'s warn** (`dcomp_compositor.rs:4861-4867`), **`WEBGLTARGETS`**
   (`webgl_thread.rs:596-602`) and the duplicate fan-out `info!` (`:1227-1233`). Each is a one-shot
   operator-visible notice rather than a per-second statistic; whether "instrumentation" here means
   perf samplers only or all fork-added logging is the user's call. Left in place; zero risk either
   way. **Do not touch `withheld_frames` itself** — it drives `DEMOTE_AFTER_WITHHOLD`.
8. **`components/shared/paint/examples/{d3d11_dyn_poc,dcomp_native_poc,dcomp_partial_present_poc}.rs`**
   (2,048 lines) and the dev-dependency winapi block at `shared/paint/Cargo.toml:56-108`.
   Experiments *about* the wall, never linked into the engine. "Deleting it changes no pixel" does
   not settle them. Leaning keep.
9. **`components/paint/Cargo.toml` winapi features `"d3d9types"`, `"excpt"`, `"ktmtypes"`,
   `"vadefs"`, `"vcruntime"`, `"wtypes"`** and similar. They close winapi 0.3.9's transitive-module
   graph rather than being named by a `use`. Whether any become droppable after `psapi`/`dxgi1_4`
   go can only be settled by removing them one at a time and compiling. Do not guess.
10. **`config_dump` as a whole.** Step 14 keeps the pref-diff half as the run-provenance record that
    replaces the launcher's removed RUN CONFIG banner. If the user wants that gone too, the
    `components/servo/lib.rs:68` re-export and the `ports/servoshell/desktop/app.rs:98` caller go
    with it.
11. **`ports/servoshell`** carries its own small timing layer (`window.rs:231-284`'s
    `render_start`/`render_ms`/`present_start`/`present_ms` and the `Wall repaint target:` /
    `Repaint target:` pair, `desktop/gui.rs:690-692`, `desktop/app.rs:72, 345`). No inventory
    covered it and it is outside the stated winit_wall scope. Not in any step above; flag whether
    servoshell should be stripped in the same pass.
12. **`check_freeze_log.ps1`** (untracked) dies automatically once `WALLTICKRATE`, `FRAMEREASON` and
    `ANIMSTEP` are gone. Per the memory note `walltickrate-zero-is-ambiguous`, `produced=0` means
    both "fixed" and "frozen", and this script was the only automated way to tell them apart.
    Step 16 does not delete it; it simply stops working. A naked-eye substitute (does the ticker
    page still move?) should exist before the first post-strip measurement run.


---

## 6. 결정에 따라 추가된 단계 (17~20)

이 넷은 1b 의 결정으로 생긴 단계다. 1~16 뒤에 이 순서로 붙는다.

### Step 17 — `--tile-thread-spike` / TILESPIKE

`components/servo/examples/winit_wall/tile.rs` 의 TILESPIKE 블록(약 474 줄), `main.rs` 의
배선, 런처의 `-TileThreadSpike`. ★자립 블록이라 Step 9 뒤 어디서든 되지만, winit_wall 을
두 번 건드리지 않도록 Step 9 직후에 둔다.★ 타일 스레드 자체는 건드리지 않는다 — 이것은
"타일을 스레드로 돌릴 수 있는가" 를 재려고 만든 CLI 모드이고 이미 답이 나왔다.

게이트: `cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu`

### Step 18 — `ports/servoshell` 타이밍 계층

`ports/servoshell/desktop/window.rs:231-284` 의 `render_start`/`render_ms`/`present_start`/
`present_ms` 와 `"Wall repaint target:"` / `"Repaint target:"` 쌍, `desktop/gui.rs:690-692`,
`desktop/app.rs:72` 와 `:345`. ★인벤토리가 커버하지 않은 영역이므로 지우기 전에 각 심볼의
호출자를 다시 전수 확인한다.★ `ports/servoshell/lib.rs:67` 의 `PerfettoLayer` 는 업스트림
tracing 통합이므로 이 단계에서 건드리지 않는다(필요하면 별건으로).

게이트: `cargo check -p servoshell` (패키지명 확인 후), 그다음
`cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu`

### Step 19 — `shared/paint` POC 예제 3 개

`components/shared/paint/examples/d3d11_dyn_poc.rs`, `dcomp_native_poc.rs`,
`dcomp_partial_present_poc.rs` (합계 2,048 줄)와 `components/shared/paint/Cargo.toml:56-108`
의 dev-dependency winapi 블록. 엔진에 링크되지 않으므로 픽셀에 영향이 없다.
★dev-dependency 를 지울 때 `[[example]]` 선언이 남지 않게 같이 지운다.★

게이트: `cargo check -p servo-paint`, `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu`

### Step 20 — `vendor_local/webrender` 패치 되돌리기

루트 `Cargo.toml:426-431` 의 `[patch.crates-io]`. ★먼저 `wr(local)`/`diag`/`probe` 커밋
9 개를 읽어 계측 외 변경이 섞였는지 확인한다★ — 섞여 있으면 그 변경만 따로 떼어
upstream webrender 위에 다시 올리거나, 이 단계를 보류하고 사용자에게 보고한다.
Cargo.lock 이 바뀌므로 약 11 분 풀빌드가 따른다. ★`Cargo.lock` 은 스테이징 금지 목록에
있으므로 이 단계의 커밋에 포함하지 않는다 — 사용자에게 알리고 사용자가 처리한다.★

게이트: `cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release`
(이 단계만 풀빌드가 필요하다)

---

## 7. 실행 규약

- ★단계마다 커밋 하나.★ 커밋 메시지에 그 단계가 지운 것과 게이트 결과를 적는다.
- ★`git add -A` / `git add .` 금지.★ 파일을 하나씩 스테이징한다. 절대 스테이징 금지:
  `Cargo.lock`, `etc/multigpu/config/wall_layout.example_1x1.json`,
  `tests/html/multigpu_standard_video_extended_probe.html`,
  `tests/html/multigpu_standard_video_rtsp_probe.html`,
  `docs/superpowers/plans/2026-10-15-tick-animation-frame-time.md`.
- ★`git stash` 금지.★ 푸시는 사용자가 한다.
- ★PowerShell 5.1 에서 `cargo`/`git` 뒤에 `2>&1`/`2>$null` 금지.★
- ★winit_wall 은 심볼 단위로 지운다 — 줄 범위 금지.★ 인벤토리의 네 범위가 이웃 구조체를
  침범한다(verify-completeness O-3).
- 빌드 전 `target` 용량 확인(메모리 `check-target-size-every-build`).
- 기준선: `servo-paint --lib` 130/130, `config_surface` 19/20(미등록 env 5 개).
  ★Step 14 뒤 `config_surface` 는 20/20 이어야 한다 — 독립 성공 신호다.★

---

## 8. ★재검증 정정 (2026-10-27, Steps 8~11)★ — 실행 전 반드시 읽을 것

Steps 1~5 에서 계획서 집계 오류가 **다섯 단계 연속** 나왔다
(`[dcomp-dbg]` 17/16, `dwm_timing` 소비자 2/3, 테스트 17/15, `raw_commit` 호출처 4/3,
게이트 111/113). 그래서 Step 5 실행과 병렬로 Steps 8~11 을 읽기 전용으로 재검증했다.
결과: **계획서 오류 28 건, 하중 함정 12 건, 누락 계측 14 건.**

전문:
`docs/superpowers/plans/notes/strip-inventory/revalidate-steps-8-9.md`,
`.../revalidate-steps-10-11.md`.

### 8.1 ★조용한 회귀를 낳는 것 — 컴파일러가 안 잡는다★

| 위치 | 문제 |
|---|---|
| `winit_wall/main.rs:1727` | **이 제거 전체에서 가장 위험한 한 줄.** `dispatched[index] = true;` 가 계측 `skipped += 1;`(1726)과 하중 `continue;`(1728) **사이에** 끼어 있다. 떨구면 barrier 가 건너뛴 타일을 두 번째 루프가 전부 다시 칠한다. 픽셀과 타이밍 회귀인데 컴파일 에러가 없다. |
| `layout/display_list/mod.rs:560` | 계획서는 "553-560 삭제, `return false;` 는 유지" 라고 적었는데 **560 이 바로 그 `return false;`** 다. 그대로 지우면 건너뛰어야 할 stacking context 가 생성 경로로 떨어진다. 올바른 범위 553-559. |
| `d3d11_ring.rs` 락 헬퍼 | 계획서의 `lock_registry` 는 **존재하지 않는다.** 셋째는 `fn lock<T>`(:504, 호출 540·545, CONSUMER_DEVICE). 이걸 놓치면 `.unwrap()` 로 바꾸기 쉬운데, 그러면 패닉 스레드 하나가 CONSUMER_DEVICE 를 오염시켜 **모든 타일의 비디오가 영구 정지**한다. 독 복구는 `lock_tracked`(:446)에 있고, 교체 대상은 **28 곳**(lock_at 23 + lock 2 + lock_removed 3). |
| `winit_wall/main.rs:893` | `last_pacing_tick_at` 래치를 **어느 테스트도 덮지 않는다.** 계획서가 가드라고 지명한 `backstop_only_when_pacing_was_installed_but_is_not_running` 은 `needs_pacing_backstop(bool, bool)` 자유 함수만 부른다. 추출을 망쳐도 게이트는 전부 초록이고 `pacing_is_driving()` 만 영구히 false 가 된다. |
| `stacking_context.rs:675` | 675 는 `});` — `let animated_transform = ...` 바인딩의 닫는 토큰이다(그 값은 701 에서 읽는다). 올바른 범위 676-693. |
| `animations.rs:196-211, 280-296` | ANIMUNBIND·ANIMDROP 는 각각 `if !set.animations.is_empty() { ... }` 안의 **유일한 문장**이다. "warn 만 삭제" 하면 빈 if 가 남는다. 래퍼를 같이 지우고 그 아래 `set.cancel_all_animations();` 를 남길 것. |

### 8.2 빌드가 깨지는 것 — 시끄럽지만 단계가 멈춘다

- `note_rendering_update_reason` 호출처는 **9 개가 아니라 11 개** — `script_thread.rs:1397`(fallback-timer)과 `:2222`(renderer-tick)가 계획서에 없다.
- `rendering_update_from_timer` 는 3 사이트가 아니라 **5** — `:1212` 와 `:1215` 누락.
- `script_thread.rs` 의 "stranded imports 넷" 은 **다섯 줄이고 셋은 전체 삭제가 아니라 트림** — `:139`·`:147`·`:156` 에서 `GlobalScope`·`Window`·`Microtask`/`MicrotaskQueue` 는 살아야 한다.
- `servo.rs:308` 의 `embedder_messages += 1;` 이 SPINSLOW 심볼 목록에 없다.
- `paint.rs:371` 의 `TileRenderTiming`(`PaintTargetsInFlight::join()` 반환형)이 Step 8 범위 밖이다.
- `main.rs:2277` 은 `\` 문자열 연속으로 2278 까지 이어진다. 2278 만 지우면 문자열이 닫히지 않는다.
- `d3d11_ring.rs` 인벤토리 게터 범위는 65-85 가 아니라 **59-92**(94 는 `SLOT_COUNT`). 85 에서 자르면 `ring_inventory` 가 반토막 난다.
- `render-d3d11/lib.rs` 의 `profile_id` 를 읽는 로그가 한 줄이 아니라 **넷**(142, 200-203, 348-352, 391-394). 뒤 셋은 진짜 실패 보고라 지우면 안 된다.
- `player.rs:3055-3058` 의 MEDIATEARDOWN warn 은 `started`(3047)·`id`·`stream_type`(3044)의 **유일한 독자**다.
- `microtask.rs:11` 의 `use std::time::{Duration, Instant};` 가 고아가 된다(계획서 미기재).
- `player.rs:7` 은 `AtomicU64` 만 떼내는 **트림**(AtomicBool·Ordering 생존), `render-d3d11/lib.rs:25` 는 줄 전체 삭제.
- `debug_env.rs` 의 선언 순서와 `ALL` 순서가 한 군데 어긋난다(`:275` 가 cache_index 24 인데 17 과 18 사이에 선언). 파일 순서로 renumber 하면 `const _: ()`(:398-405)가 터진다.

### 8.3 게이트 기대값 정정

- **Step 9** — 계획서가 "남는 clock 테스트 다섯" 이라 했으나, `main.rs mod tests` 에는 `#[test]` 가 **아홉** 있다(인벤토리가 섬 다섯은 **줄 범위** 수였고 그중 하나에 테스트가 셋 들어 있다). example 타겟 전체: **20 -> 18**.
- `config_surface.rs` 의 상수 결합은 셋이 아니라 **둘**(`DCOMP_READBACK` :288, `LOG_PRESENT_CADENCE` :289). `:276`·`:277` 은 raw-string 픽스처 안이라 상수 참조가 아니다. 항목 범위도 271-295 가 아닌 **270-294**.

### 8.4 올바른 심볼 경계 (인벤토리 범위가 틀린 곳)

`note_tick_site` 889-901(래치 892-895, 계측 896-900; **903 은 살려야 하는 `pacing_is_driving` 의 doc 머리**) /
`note_present` 775-799(801-802 는 `maybe_capture` 의 doc) /
`charge_main` 841-866(839 는 `maybe_capture` 의 닫는 괄호) /
`PassSplit` 572-585 / `AppState::main_busy` doc 은 376 부터 /
`note_redraw_suppressed` 930-934 /
`AppState::pass_stats` 는 **297-303** 만(295-296 은 KEEP 인 `capture_path`/`capture_deadline` 의 설명).

### 8.5 기타 정정

- 계획서의 `MEDILOCKDEFER` 는 오타 — 실제 태그는 **`MEDIALOCKDEFER`**(914-917). 그대로 grep 하면 0 건이 나와 "이미 끝났다" 로 오독한다.
- `ring_init_deferred` / `wrap_init_deferred` 는 **admission control 이 아니다** — 진짜 예산은 `ring_init_done`·`wrap_init_done` 를 `max_per_frame` 와 비교하는 :923·:952 다. (남겨도 무해하다.)
- `LIVE_CONTEXTS` prune(`webglrenderingcontext.rs:257`)의 올바른 이사 위치는 `register_for_reporting`(640-642) — Vec 이 커지는 유일한 자리이고 이미 `borrow_mut()` 를 쇥고 있다.
- `htmlmediaelement.rs` 의 고아 집합이 더 크다 — `image_key_for_log`, `frame_backend`(1109-1128 매치 + 두 함수의 파라미터 여섯 + 호출 인자).
- `summarize_intervals` 의 남는 소비자는 `painter.rs` 가 아니라 **`frame_time.rs`** 다.
- `configuration.md` 의 `SERVO_LOG_PRESENT_CADENCE` 참조는 둘(### 항목 249-252, 산문 :328).
