# Adversarial verification of the removal inventories

Repository: `W:\servo_multigpu-tiled-wall`, branch `wall-strip-instrumentation`.
Read-only pass, 2026-10-27. Nothing was edited.

Inventories checked (all seven):
`strip-inventory-output_grid.md`, `-dcomp.md`, `-painter_paint.md`, `-paint_misc.md`,
`-winit_wall.md`, `-script_media.md`, `-config_surface.md`.

My job was to **refute** REMOVE verdicts, not to re-derive the inventories. For every REMOVE
verdict that touches timing, the vblank clock, DWM, DXGI/DComp statistics, the frame barrier,
animation sampling or the rAF timestamp, I opened the code and looked for a surviving caller
that reads the value for a decision.

---

## 1. Headline

**I could not refute a single behaviour-critical REMOVE verdict.** The seven inventories
correctly identified every trap I went hunting for, and in several cases identified traps I
would have missed. The classification work is sound.

What they got wrong is **coverage and sequencing**, not classification:

* one whole subsystem (`components/webgl/`) and two single files
  (`components/servo/servo.rs`, `components/shared/base/interval_summary.rs`) carry fork
  instrumentation and appear in **no** inventory;
* one REMOVE verdict (`debug_env::WEBGL_FANOUT_PROF`) enumerates its callers incompletely and
  will hard-break the build if executed as written;
* two inventories disagree about *when* `output_grid::COLLECT` changes meaning, and the
  intermediate state silently turns all surviving instrumentation **on**.

---

## 2. REMOVE verdicts I attacked and could not break

Recorded so nobody re-litigates them. Each line is "I opened it, here is why the verdict holds".

| verdict under attack | inventory | what I checked | holds? |
|---|---|---|---|
| `note_pacing_vblank` REMOVE (output_grid.rs:145-173) | output_grid | `VblankSource::wait` (output_grid.rs:1858-1892) is the only caller. Removing the call leaves `Some((vblank, period))` untouched — the function writes `GRID` and returns `()`. It also early-returns on `!*COLLECT`, so it is already a no-op in production runs. Every `GRID` reader (`grid_for_monitor` → `grid_is_fresh`, `note_dcomp_stat` post-gate, `emit_outphase`) is itself REMOVE. | **holds** |
| `log_anchor` + `last_log_qpc` REMOVE | output_grid | Called once from `wait()` at :1890, writes nothing but a `warn!`. | **holds** |
| `wait_two_vblanks` / `period_from_pair` REMOVE while `WaitForVBlank` KEEP | output_grid | Three `WaitForVBlank` sites: :1868 (`VblankSource::wait`, the clock), :2017 and :2021 (`wait_two_vblanks`, probe only). `VblankSource::refresh` reads the period from `nominal_period_ticks(..., None)` — it never measures, by design (comment at :1818-1824 states that measuring would cost a whole tick). | **holds** |
| `COMPOSITION` KEEP / `note_dcomp_stat` reduce-not-delete | output_grid + dcomp | Verified at output_grid.rs:1372-1379: the `COMPOSITION.lock()` store sits **three lines above** `if !*COLLECT { return; }`, with the comment `★격자 갱신은 계측 게이트 밖이다★`. `composition_grid()` (:301) → `lead_to_next_composition` (:636) → `painter::paint_animation_sample_at` (painter.rs:4303-4342) → `sample_at` → `update_paint_animations`. This is the single most dangerous item in the strip and **both** inventories flagged it. | **holds** |
| `lead_to_next_composition` KEEP(trim) | output_grid | `note_sample_slip` at :681 is the only consumer of `phase_ms`/`residue_ms`/`monitor`; the returned `(anchor, lead)` is what the painter uses. Deleting lines :673-681 is safe; the `anchor`/`qpc_now()` adjacency at :652-655 must not be disturbed (the comment says so, and `snap_to_dwm_grid_at` has the same shape). | **holds** |
| `dwm_timing()` REMOVE but `composition_grid()` KEEP | dcomp | Confirmed two distinct `DwmGetCompositionTimingInfo` wrappers. `composition_grid` has one load-bearing caller — `lib.rs:67` → `servo::dwm_composition_grid` → `winit_wall/main.rs:1235` inside `snap_to_dwm_grid_at`, which sets `next_present_tick` (main.rs:1572) — and six log-only callers in `output_grid.rs`. `dwm_timing`'s sole caller is `output_grid.rs:1396`, post-COLLECT-gate, feeding two printed fields. | **holds** |
| `note_frame_statistics` / `GetFrameStatistics` KEEP | dcomp | painter.rs:4285-4290 reads `!*DCOMP_BIND_PROF && SAMPLE_LEAD_PERIODS.is_none()` → return. The second arm is the feature. Verified verbatim in source. | **holds** |
| `AppState::note_tick_site` REMOVE-with-carve-out | winit_wall | Verified main.rs:889-903: lines 891-894 set `last_pacing_tick_at`, which `pacing_is_driving()` (:904-912) reads, which gates `WakerEvent::Engine if !state.pacing_is_driving()` (:2360) and `needs_pacing_backstop` (:2426-2432). Deleting the function wholesale is a real regression; the inventory's step-3.1 "extract the latch first" is the correct order. | **holds, and the carve-out is mandatory** |
| `note_render_pass` + all stopwatch locals REMOVE, `in_flight.join()` KEEP | winit_wall | Verified main.rs:1700-1845. `dispatched: Vec<bool>` (:1705) gates the second loop; `dispatched_indices` (:1710) only maps join timings into `tile_ms`. `skipped` bumps sit beside the barrier `continue`s at :1726 and :1754. `join()` must still run before `flush_deferred_dcomp_commits()` (:1832). All as described. | **holds** |
| `painter_busy` / `someone_else_is_producing` die with the two counters | painter_paint | Verified painter.rs:1482-1502. Both locals have exactly one reader each — `paint_animation_skipped_busy` and `paint_animation_rode_along`. **Secondary check:** `painter_busy` calls `self.renderer_behind()`, which has the 2-second force-release side effect. Removing this call site is safe because `run_liveness_probe` (painter.rs:2306, `let _ = self.renderer_behind();`) and the tick path (painter.rs:3954) still call it every tick — the file's own comment at :2296-2306 says the probe exists precisely so the liveness call does not depend on any other path. | **holds** |
| `pending_frame_diagnostics` / `FrameReadyDiagnostic` KEEP(reduced) | painter_paint | Verified painter.rs:3497-3515 and 3561-3568 → paint.rs:933-1005. `diagnostic.wall_logical_frame_id` is the **barrier key** (`self.barriers.contains_key(&logical_frame_id)`), and `ready_at` sets `barrier.first_ready_at`, which `missed_deadline_at` turns into the 16 ms deadline. Named "diagnostic", is the barrier. | **holds — do not let the name fool a later editor** |
| `last_gpu_memory_log_at` KEEP | painter_paint | Verified painter.rs:2396-2421: the same once-a-second `due` gate throttles `D3d11PlaneRings::expire_all_stale_demand`. Removing the `Cell` makes the ring reclaim run every frame or never (the comment records 321 rings / 3.8 GB). | **holds** |
| `last_render_started_at` KEEP | painter_paint | Verified painter.rs:2460-2461 (sits one line under `note_animation_win()`), read at :1419 (`rendered_since_push`), :1443 (`renderer_is_running`), :1485. Those decide whether animation values are pushed. | **holds** |
| `PumpSource::note` REMOVE, `may_schedule` KEEP | painter_paint | Verified painter.rs:5040-5052. `may_schedule` is `!matches!(self, PumpSource::Tick)`; `note` is a three-arm dispatch into `output_grid::note_pump_from_*`. Cleanly separable. | **holds** |
| `commit_guard::device_guard` body REMOVE, `device_lock` KEEP | paint_misc | Verified commit_guard.rs:165-176: `if !*COLLECT { return lock.lock()… }` is already the production path. Only the `role` argument and the two `qpc_now()` reads go. **Call-site count verified: exactly four** (`dcomp_compositor.rs:2337, 3251, 3590, 4575`), matching the inventory. | **holds** |
| `commit_guard::WORKERS`/`DONE`/`commit_all_at_tick` KEEP | paint_misc | Verified commit_guard.rs:250-310. The dispatch loop, the `recv` loop and the sequential fallback issue the real `Commit()`. Only `:261`, `:299-301` (the `record_fanout` stopwatch) and `:310` (`note_dwm_phase`) are instrumentation. | **holds** |
| `frame_time.rs` core KEEP, three lines REMOVE | paint_misc | Verified frame_time.rs:75-87: the instrumentation is `if *FRAME_TIME_PROF { record_delivered(...) }` plus the `painter_id` parameter. `delivered` is returned regardless and becomes the rAF timestamp. | **holds** |
| `wall_tick.rs` zero removals | paint_misc | `grep -nE "warn!|info!|debug!" components/paint/wall_tick.rs` returns **nothing**. The module contains no log line at all; every counter (`TICK_SEQ`, `NEXT_EDGE_SCHEDULED`) is read by a decision. | **holds** |
| `DCOMP_BIND_PROF` at dcomp_compositor.rs:3803 is a pure timing split | dcomp (config_surface raised doubt) | **Resolved.** Verified :3802-3807 and :4050-4053: `work_device` is `Some(ptr)` only when the gate is on, and its single use is `work_device.and_then(device_work).note_bind(reused)` at :4051. It never selects a different device for any real operation. config_surface's Trap "verify that line; I could not settle it" is now settled — it is **not** behavioural. | **config_surface's doubt dismissed** |
| `paint.rs::note_demand_tick` REMOVE, `produced` KEEP | painter_paint | Verified paint.rs:2419-2424: `on_demand_wall_tick` computes `produced` from `on_demand_wall_tick_inner()`, passes `produced.is_some()` to the logger, and returns `produced`. | **holds** |
| `hold_the_wall_while_a_screenshot_is_taken` tail REMOVE, head KEEP | painter_paint | Verified paint.rs:2316-2357: `release_keep_previous_while_production_is_suspended` and the `run_liveness_probe` loop precede the counters. `released` becomes unused. | **holds** |
| dcomp `unbind` stopwatch REMOVE, EndDraw/destroy KEEP | dcomp | Verified dcomp_compositor.rs:4242-4322: `EndDraw`, `destroy_render_pbuffer`, `Release` are plainly separable from `end_ns`/`destroy_start`/`bind_device`. | **holds** |

---

## 3. REMOVE verdicts that are wrong as written

### 3.1 `debug_env::WEBGL_FANOUT_PROF` — caller list is incomplete (build break)

`strip-inventory-config_surface.md` B.1 states the readers are
`paint/webrender_external_images.rs:37-40` only, and its removal order step 3 says to delete the
`pub const`, the `ALL` entry and renumber once the reader is gone.

There is a **second reader**:

```
components/webgl/webgl_thread.rs:337:    debug_env::string(&debug_env::WEBGL_FANOUT_PROF)
```

(verified by `grep -rn "debug_env::WEBGL_FANOUT_PROF" components/ ports/`.) Executing
config_surface step 3 as written leaves `components/webgl` referencing a deleted const — a hard
compile error in a crate the stated build gate (`cargo test -p servo-paint`) does not build, so
it will not be caught until a full build.

Not a silent behaviour regression, but it falsifies the "every caller" column, which is the
column the whole exercise rests on.

### 3.2 `output_grid::name_for_monitor` / `NAMES` REMOVE — degrades an error path, not just a log

Flagged by the output_grid inventory itself, restated here because it is the one REMOVE whose
consequence outlives the strip: `publish_names` is the **only** writer of `NAMES` and dies with
`probe_loop` in step 1. From that commit on, `dcomp_compositor::note_commit_failure`'s
`out=` field is permanently `?` — even if `note_commit_failure` is kept (it is UNCERTAIN in two
inventories). That is a Commit-failure / TDR report, not a statistic. Decide the
`note_commit_failure` question **before** step 1, not after.

Everything else I checked that was marked REMOVE is genuinely removable.

---

## 4. Instrumentation the inventories did not list at all

These are the expensive gaps: code that matches the user's four categories exactly and that no
inventory owns, so a strip executed from these seven files leaves it in place (or, worse, leaves
it in place while deleting the flag it reads).

### 4.1 `components/webgl/webgl_thread.rs` — an entire uncovered subsystem

No inventory covers `components/webgl/`. Verified contents:

| symbol | lines | what it is |
|---|---|---|
| `WEBGL_PROGRESS`, `WEBGL_PHASE` | 227-228 | atomics written at :549-550, :564-565, :1784-1785, :1811-1812, :1828-1829 purely so an external thread can see liveness |
| `WEBGL_MSG_CMDS` / `_SWAPS` / `_OTHER` | 239-241 | per-message-kind counters, written at :567-569 |
| `PHASE_*` consts + `phase_name` | 243-258 | log formatting |
| `spawn_webgl_watchdog` | 266-300 | **a dedicated thread that sleeps 1 s in a loop and emits `WEBGLWATCHDOG`**. Spawned at :546 under `*WEBGL_FANOUT_PROF`. Textbook "a thread that exists to sample". |
| `log_live_contexts` + `last_live_log` field | 427-428, 751-777 | emits `WEBGLLIVE` once a second. **Called unconditionally at :582 on every WebGL message** — not gated by any flag. Verified it contains no prune or other side effect (unlike its script-side cousin `report_live_contexts`). |
| `WEBGL_FANOUT_PROF` | 336-339 | the second reader of the debug_env flag (see 3.1) |
| `FanoutProfile` + `fanout_profile` field + `maybe_emit_fanout_profile` | 429-430, 525, 1908-1965 | emits `WEBGLFANOUT`; ~15 `Instant` stopwatch sites at :873-874, 1079-1082, 1114-1118, 1136-1137, 1773-1774, 1813, 1826, 1839-1844, 1860, 1901-1903, 1998-2012, 2024-2038 |

Load-bearing neighbours in the same file that must **not** be swept up: `WEBGL_SWAP_SYNC` /
`gfx_webgl_swap_sync` (:322-333, selects flush/finish after swap) and
`maybe_reclaim_idle_contexts` / `WEBGLRECLAIM` (:1506-1690 — the warn at :1651 is a log, the
release around it frees GPU contexts and VRAM).

### 4.2 `components/servo/servo.rs` — the `SPINSLOW` spin-timing harness

No inventory covers `components/servo/servo.rs` (only `components/servo/lib.rs` re-exports are
mentioned). `spin_event_loop` carries a full timing harness:

```
273  let spin_start = Instant::now();
274  let mut paint_messages = 0usize;
275  let mut embedder_messages = 0usize;
276  let paint_handle_ms = { ... };   287  paint_messages = messages.len();   288  let start = Instant::now();
292  let embedder_start = Instant::now();   308  embedder_messages += 1;   313  let embedder_ms = ...
341  let updates_start = Instant::now();    343  let perform_updates_ms = ...
346  let spin_ms = ...;  347  if spin_ms > 30.0 { warn!("SPINSLOW ...") }
```

Every value terminates in the one `warn!`. This runs on the embedder's hottest loop (the
winit_wall shell calls `spin_event_loop` hundreds of times a second — see
`winit_wall/main.rs:2326, 2444`).

### 4.3 `components/shared/base/interval_summary.rs` — the orphan nobody claims

185 lines, fork-added, module doc states outright that it exists so `FRAMETIME` and `RAFCLOCK`
use the same bands and percentiles. Its complete consumer list:

```
components/paint/frame_time.rs:14      (record_delivered — REMOVE)
components/paint/paint.rs:130          (record_painter_lock_wait — REMOVE)
components/paint/painter.rs:4966       (format_step_sequence — REMOVE)
components/script/raf_clock.rs:31      (whole file — REMOVE)
```

**After the strip it has zero consumers.** Three inventories mention it and all three say *do
not delete it from here, another file still uses it* (paint_misc §imports, script_media
§cross-subsystem, painter_paint step 9). The result is that no inventory removes it. Delete the
file and `components/shared/base/lib.rs:15` in the last commit of the strip.

### 4.4 `components/shared/paint/` — two unclaimed symbols

* `PaintMessage::kind()` (`components/shared/paint/lib.rs:298-...`, ~40 match arms). Its doc
  comment says it is "used only to count what spilled when one drain was slow". Sole caller is
  the `PAINTDRAIN` tally in `paint.rs:3833-3846`, which is REMOVE. painter_paint says
  "coordinate with that file"; nobody owns it.
* `D3D11_TEXTURES_RELEASED` (`components/shared/paint/rendering_context.rs:64, 1186`). Sole
  reader is the `MEDIARINGS` line (`media-thread/lib.rs:1315`), which is REMOVE. script_media
  explicitly hands it off to "whoever owns shared/paint"; nobody owns it.

### 4.5 `components/media/media-thread/lib.rs` — two log sites outside every listed range

script_media step 10 enumerates line ranges that do **not** include:

* `:913-918` — `info!("MEDIALOCKDEFER frame={} initialized={} deferred={}")`
* `:1048-1053` — `info!("MEDIAWRAPDEFER frame={} wrapped={} deferred={}")`

**Trap attached:** the counters these lines print (`ring_init_frame/done/deferred`,
`wrap_init_frame/done/deferred`) are **load-bearing** — they implement the per-render-frame ring
and wrap budgets in `may_initialize_ring_now` (:906-925) and `roll_wrap_budget_frame`
(:1043-1058), gated by `media_ring_init_max_per_frame`. Anyone grepping `MEDIALOCK*` and
deleting the surrounding state will remove a real admission-control gate. Remove the two `info!`
statements only.

### 4.6 `MEDIATEARDOWN` sites in the gstreamer backend

script_media removes the one `MEDIATEARDOWN` in `htmlmediaelement.rs:3272` and lists no others.
Four more exist, each with a timing or count field:

```
components/media/backends/gstreamer/player.rs:3056   "MEDIATEARDOWN disposed … ms={:.0}"
components/media/backends/gstreamer/player.rs:3085   "MEDIATEARDOWN player drop …"
components/media/backends/gstreamer/media_stream.rs:356        "MEDIATEARDOWN stream drop …"
components/media/backends/gstreamer/media_stream_source.rs:153 "MEDIATEARDOWN detaching …"
```

Each sits inside real teardown code (the surrounding drop/detach logic is load-bearing). Lowest
priority of the gaps, but they are unlisted and they are this fork's measurement prose.

### 4.7 Tag census used to find the above

For the record, so the next pass can be checked mechanically — every fork log tag in the tree and
the file that emits it (`grep -rnoE '"[A-Z][A-Z0-9]{4,}[A-Z0-9]? ' --include=*.rs components/ ports/`).
Tags with **no owning inventory**: `WEBGLFANOUT`, `WEBGLLIVE`, `WEBGLWATCHDOG`, `WEBGLRECLAIM`
(components/webgl), `SPINSLOW` (components/servo/servo.rs), `MEDIALOCKDEFER`, `MEDIAWRAPDEFER`
(media-thread), `MEDIATEARDOWN` ×4 (gstreamer backend). Everything else maps to one of the seven
inventories. `COMPSTATS` (`output_grid.rs:2268`) is inside `emit_comp_stats` and is covered.

---

## 5. Ordering problems between the seven plans

### 5.1 `COLLECT` silently inverts in the middle of the strip (highest risk)

`output_grid.rs:50-51` is

```rust
static COLLECT: LazyLock<bool> =
    LazyLock::new(|| *PROBE_ENABLED && *crate::dcomp_compositor::DCOMP_BIND_PROF);
```

i.e. today collection is **off by default** (it needs `SERVO_DCOMP_BIND_PROF`).

* `strip-inventory-dcomp.md` **step 7** deletes `DCOMP_BIND_PROF` and instructs:
  *"`output_grid.rs:51` … must be re-expressed (probably `COLLECT = *PROBE_ENABLED`)"*.
* `strip-inventory-output_grid.md` **step 5** says `COLLECT` is the **last** thing deleted.

If dcomp step 7 lands before output_grid step 5 — which is what both orderings imply, since
output_grid defers `COLLECT` to "whichever commit finishes dcomp_compositor" — then for the
duration of the intervening commits `COLLECT` is `*PROBE_ENABLED`, which is **`true` unless
`SERVO_WALL_NO_PROBE` is set**. Every surviving `note_*`, the `OutputVBlankProbe` thread, the
`device_guard` QPC pair and the `DWMPHASE` DWM round-trip all switch **on** in default runs.

The user is doing naked-eye observation between commits. An intermediate build that quietly
turns the whole probe layer on is exactly the kind of confounded comparison the memory note
`log-redirect-blocks-the-engine` records. **Fix:** when `DCOMP_BIND_PROF` goes, set
`COLLECT = false` (or delete the three remaining readers in the same commit), never
`*PROBE_ENABLED`.

### 5.2 The `servo-paint` gate does not compile four of the seven subsystems

The stated gate `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu` does not build
`components/webgl`, `components/servo` (the `winit_wall` example), `components/config`'s
integration tests, `components/script`, `components/media` or `components/layout`. Three
inventories say so individually; nothing collects it. The strip needs, at minimum:

```
cargo test  -p servo-paint --lib --features webgl/no-wgl,webgpu
cargo check -p servo --example winit_wall --features webgl/no-wgl,webgpu
cargo check -p layout -p script -p servo-media -p servo-media-thread -p servo-media-player
cargo check -p canvas            # pulls in components/webgl
cargo test  -p servo-config --test config_surface
```

Without the `canvas`/`webgl` check, problem 3.1 is invisible until a full release build.

### 5.3 `emit_lockwait` and `comp_stats.rs` are each owned by two plans

`output_grid.rs:2175` is the sole caller of `commit_guard::emit_lockwait`, and
`output_grid::emit_comp_stats` is the sole consumer of all of `components/paint/comp_stats.rs`.
output_grid step 1 orphans both; paint_misc steps 5 and 7 delete both. Both plans describe the
coupling, but neither claims the commit. Assign one owner or the tree sits on `dead_code`
warnings (fatal under `-D warnings`).

### 5.4 `name_for_monitor` ordering (see 3.2)

output_grid step 1 (`probe_loop`/`publish_names`) must not land before the
`note_commit_failure` decision in dcomp step 9, or the Commit-failure warn loses its output name
in between with no way to tell.

### 5.5 `LOG_PRESENT_CADENCE` has two readers in two crates

winit_wall step 6 and config_surface step 3 both say this, from opposite sides
(`winit_wall/main.rs:647` and `painter.rs:86`). `config_surface.rs:271-295`
(`the_dead_knob_check_actually_catches_an_unread_flag`) hard-codes the const and asserts
`dead == vec!["SERVO_LOG_PRESENT_CADENCE"]`, so the flag, both readers and that test must land
together. Correctly identified by both; listed here because it is the only three-way coupling.

### 5.6 `format_step_sequence` / `LOG_ANIM_STEP_SEQ` three-file knot

`painter.rs:4941-4975` defines them; `frame_time.rs:172` and `web_content_animation.rs:593,624`
read them. paint_misc steps 1 and 3 remove the two readers; painter_paint step 9 removes the
definitions. Order is readers → definition, and both plans say so. No conflict, listed for
completeness.

---

## 6. Additional traps found during verification

Not present in any inventory's Traps section:

* **`log_live_contexts` (webgl_thread.rs:582) is called on every WebGL message, ungated.**
  Unlike `WEBGLFANOUT`, it does not sit behind `WEBGL_FANOUT_PROF`. It is a once-a-second
  `warn!` plus two map walks on the WebGL thread's hot path.
* **`spawn_webgl_watchdog` is a sampling thread, the only one outside `output_grid::probe_loop`.**
  The user's category-4 ("any other measurement facility, even ones the user is not aware of")
  covers it squarely.
* **`may_initialize_ring_now` / `roll_wrap_budget_frame` counters are admission control, not
  statistics** (see 4.5). They have the exact shape of a per-window tally and are printed next
  to the thing they gate.
* **`interval_summary` is where the strip will "finish" while still shipping dead code.** It
  will not warn — it is a `pub mod` in a shared crate, so nothing flags it as unused.
* **`config_surface.rs::every_registered_flag_is_actually_read_somewhere` scans *all* `.rs` for
  `debug_env::<IDENT>`**, which means it would have caught 3.1 — but it is not in the stated
  build gate. Add it (see 5.2); it is the cheapest safety net available for this strip.

---

## 7. Bottom line for the executor

Execute the seven inventories as written **with these five changes**:

1. Add a `components/webgl/webgl_thread.rs` pass (4.1) and a `components/servo/servo.rs` pass
   (4.2) before touching `debug_env.rs`.
2. Fix the `WEBGL_FANOUT_PROF` caller list (3.1) — two readers, not one.
3. When `DCOMP_BIND_PROF` dies, write `COLLECT = false`, never `*PROBE_ENABLED` (5.1).
4. Settle `note_commit_failure` before output_grid step 1 (3.2 / 5.4).
5. Run the full command set in 5.2 after every commit, not just the `servo-paint` gate.

And schedule a final cleanup commit for the orphans in 4.3 and 4.4, which no plan currently owns.
