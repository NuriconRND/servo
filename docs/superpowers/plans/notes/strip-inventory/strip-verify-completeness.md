# Adversarial completeness check of the removal inventory

Repo: `W:\servo_multigpu-tiled-wall`, branch `wall-strip-instrumentation`. Read-only pass,
2026-10-27. Nothing was edited.

Inputs checked (all seven read in full):
`strip-inventory-output_grid.md`, `-dcomp.md`, `-painter_paint.md`, `-paint_misc.md`,
`-winit_wall.md`, `-script_media.md`, `-config_surface.md`.

My job was not to repeat them. It was to find (a) anything marked REMOVE that is actually
load-bearing, (b) instrumentation no inventory lists at all, (c) ordering/precision problems
that would break the tree or mangle a file during execution.

**Headline: the inventories' classification work is sound — I found no item marked REMOVE that
is load-bearing. But they are not complete, and two of the gaps are compile breaks.**

| | count |
|---|---|
| Misclassified REMOVE (load-bearing) | **0 confirmed** · 0 suspected |
| Whole subsystems with instrumentation and **no inventory** | **2** (`components/webgl`, `components/servo`) |
| Other missed items | 3 |
| Ordering / precision problems | 9 |

---

## 1. Missed instrumentation — `components/webgl/webgl_thread.rs` (no inventory covers this crate)

The seven inventories partition `components/paint`, `components/servo/examples/winit_wall`,
`components/script`, `components/media`, `components/layout`, `components/allocator` and
`components/config`. **`components/webgl` was never assigned to anyone**, and it holds one of the
largest single instrumentation layers in the fork: three log tags (`WEBGLFANOUT`, `WEBGLLIVE`,
`WEBGLWATCHDOG`), a dedicated sampling thread, and a second reader of a `debug_env` flag that two
inventories believe has only one reader.

| symbol | file:line | kind | verdict | every caller / what it does with the value | notes |
|---|---|---|---|---|---|
| `WEBGL_PROGRESS`, `WEBGL_PHASE` | webgl_thread.rs:227-228 | `AtomicU64` / `AtomicUsize` | **REMOVE** | written at 549-550, 564-565, 1784-1785, 1811-1812, 1828-1829 (six `store`/`fetch_add` on the message hot path); read **only** by the watchdog thread at 275, 290 | the stores are on every message and every swap |
| `WEBGL_MSG_CMDS` / `_SWAPS` / `_OTHER` | webgl_thread.rs:239-241 | `AtomicU64` ×3 | **REMOVE** | written at 567-569 (one per message); read only by the watchdog at 276-278 | |
| `PHASE_IDLE/HANDLING/SWAP_LOCK/SWAP_BUFFERS/SWAP_AFTER` + `phase_name` | webgl_thread.rs:244-258 | consts + fn | **REMOVE** | `phase_name` ← watchdog :290 only | |
| `spawn_webgl_watchdog` | webgl_thread.rs:260-301 | fn spawning a thread | **REMOVE** | called once at `WebGLThread::process` :545-547, under `if *WEBGL_FANOUT_PROF` | **a thread whose entire body is `sleep(1s)` + `warn!("WEBGLWATCHDOG …")`.** This is precisely the "thread that exists to sample" shape the task names |
| `WEBGL_FANOUT_PROF` | webgl_thread.rs:335-339 | `LazyLock<bool>` | **REMOVE** | read at 545, 873, 1079, 1114, 1773, 1813, 1839, 1860, 1901, 1913, 1998, 2024 | **second reader of `debug_env::WEBGL_FANOUT_PROF` — see Ordering problem O-1** |
| `struct FanoutProfile` (14 fields) | webgl_thread.rs:341-378 | struct | **REMOVE** | only as `WebGLThread::fanout_profile` | |
| `WebGLThread::fanout_profile` (field + init) | webgl_thread.rs:429-430, init :525 | field | **REMOVE** | written 874, 1082, 1118, 1136-1137, 1774, 1826, 1844, 1902-1903, 2011-2012, 2037; read only by `maybe_emit_fanout_profile` | |
| `maybe_emit_fanout_profile` | webgl_thread.rs:1908-1963 | method | **REMOVE** | ← `WebGLThread::swap_buffers` tail :1905 | emits `WEBGLFANOUT`, resets the window |
| `Instant` stopwatch pairs around the real work | webgl_thread.rs:1079-1082, 1114-1118, 1813-1826, 1839-1844, 1860-…, 1998-2012, 2024-2037 | locals | **REMOVE** | each lands in a `fanout_profile` field | **Careful:** keep `make_context_current`, `WebGLImpl::apply`, `SwapChain::swap_buffers`, `clear_surface` and the serialize call itself — only the `WEBGL_FANOUT_PROF.then(Instant::now)` / `elapsed()` wrappers go |
| `WebGLThread::log_live_contexts` + `last_live_log` field | webgl_thread.rs:751-776; field :427-428, init :524 | method + field | **REMOVE** | called **unconditionally** at :582 in the message loop, once a second; emits `WEBGLLIVE` | **not gated by anything** — this one runs on every build, every run |
| `WEBGLRECLAIM` warns | webgl_thread.rs:1651, 1689-1692 | log sites | **REMOVE (text only)** | — | **Trap: the surrounding `maybe_reclaim_idle_contexts` (1506-…) and `released_contexts` / `context_recipes` / `last_composited` / `last_reclaim_sweep` are load-bearing VRAM reclaim.** Only the two `warn!` lines are instrumentation. Do not delete the function |

Cross-reference the script inventory's own UNCERTAIN row "`WEBGLLIVE` counterpart in the WebGL
thread — not in this subsystem": that counterpart is `log_live_contexts` above. The pair can now
be removed together.

Build gate note: `servo-paint` depends on the `webgl` crate, so a break here **does** fail
`cargo test -p servo-paint --lib`. Add `cargo check -p servo-webgl` to the per-step checks anyway.

---

## 2. Missed instrumentation — `components/servo/servo.rs` (the `servo` crate itself)

The winit_wall inventory covers `components/servo/examples/winit_wall/`. Nobody covered
`components/servo/*.rs`. One item there:

| symbol | file:line | kind | verdict | every caller / what it does with the value | notes |
|---|---|---|---|---|---|
| `SPINSLOW` timing block in `Servo::spin_event_loop` | servo.rs:272-350 | 4 × `Instant::now()` + 2 counters + `warn!` | **REMOVE** | `spin_start` (273), `paint_handle_ms` (288-290), `embedder_start`/`embedder_ms` (292, 313), `updates_start`/`perform_updates_ms` (341, 343), `paint_messages` (274, 287), `embedder_messages` (275, 308), `spin_ms` (346) — **every one of them reaches only the `warn!` at 347-350** | **Careful:** this block is interleaved with the real work. Keep `paint.handle_messages(messages)` (289), the `selector.try_recv_one_message()` loop, `self.paint.borrow_mut().perform_updates()` (342), and the `messages` gather. The `paint_messages = messages.len()` line (287) and `embedder_messages += 1` (308) are the only pure-counter statements |

I swept the rest of `components/servo/` (14 files) for ALL-CAPS tags, sample statics and
`Instant::now()`: `SPINSLOW` is the only hit. `components/servo/lib.rs`'s re-exports
(`wait_primary_vblank`, `dwm_composition_grid`, `note_frame_time`, `note_present_tick`,
`schedule_wall_production`) are correctly KEEP per the other inventories.

---

## 3. Other missed items

| symbol | file:line | kind | verdict | why no inventory caught it | notes |
|---|---|---|---|---|---|
| `servo_base::interval_summary` — **the whole module** | `components/shared/base/interval_summary.rs`; declared `components/shared/base/lib.rs:15` | fork-added summarizer (`percentiles`, `summarize_intervals`, `format_intervals`) | **REMOVE, last** | Two inventories explicitly say "do not delete it, X still uses it" — each looking at a *different* surviving consumer. **After the full strip there are none.** | Complete consumer list, all of them already marked REMOVE: `paint/frame_time.rs:14` (`record_delivered`), `paint/paint.rs:130` (`record_painter_lock_wait`), `paint/painter.rs:4966` (`format_step_sequence`), `script/raf_clock.rs:31` (`note_resolved`). Nothing else in the tree imports it. It is the orphan the strip leaves behind; cheap, but it is 100% measurement code |
| `components/paint/Cargo.toml` winapi features `psapi` and `dxgi1_4` | Cargo.toml:~79, ~108 | build deps | **REMOVE** | the lens asked; no inventory mentions Cargo.toml | `psapi` is used only by `process_memory_mb` (`painter.rs:823`, `GetProcessMemoryInfo`); `dxgi1_4` only by `log_gpu_memory` (`painter.rs:753`, `QueryVideoMemoryInfo`). Both functions are REMOVE in painter step 4. **Keep `processthreadsapi`** — still used at `painter.rs:904` (`SetThreadPriority`) after `thread_cpu_ms` goes. **Keep `profileapi`** — `qpc_now`/`qpc_frequency` stay |
| `config_surface.rs:271-295` — resolved, not missed, but worth restating | — | test | REMOVE/rewrite | the config inventory has it | It is the **only compile break in the test tree** and it is in a package the stated gate never builds. Run `cargo test -p servo-config --test config_surface` |

### Resolved: an open question one inventory left hanging

`strip-inventory-config_surface.md` flagged `dcomp_compositor.rs:3803`
(`let work_device = if *DCOMP_BIND_PROF { … }`) as possibly a behavioural fork and said "I could
not settle it from the config side." **Settled: it is instrumentation only.** The value is an
`Option<usize>` device pointer captured solely to index `device_work(...)` for the `COMMITWORK`
counters at 4051-4053 (and nothing else reads `work_device`). Removing it is behaviour-neutral.
The dcomp inventory's verdict was right.

---

## 4. Misclassified REMOVE — none found, and here is what I checked

I verified the riskiest REMOVE verdicts by reading the code, not the inventory. Each of these
could have been a silent regression; none is.

| item | inventory verdict | what I confirmed in source |
|---|---|---|
| `output_grid::note_pacing_vblank` | REMOVE | `VblankSource::wait` (output_grid.rs:1858-1892) calls it at :1882 and then returns `Some((vblank, period))` built at :1877-1880. The clock never reads `GRID` back. Safe |
| `output_grid`'s `monitor` parameter of `lead_to_next_composition` | REMOVE after `note_sample_slip` goes | read `lead_to_next_composition` (:648-688): `monitor` appears **only** in the `note_sample_slip(...)` argument list at :681. `phase_ms`/`residue_ms` are computed only for that call. `target`, `anchor`, `now`, `freq` are untouched. Safe |
| `dcomp::CommitSite` / the `site` parameter of `raw_commit` | REMOVE | `raw_commit` (:2242-2267): `site` is used once, `COMMIT_SITES[site.index()].record(...)`, inside `if let Some(start) = start`. No branch, no ordering effect. Safe |
| `dcomp::SurfaceEntry::last_begin_texture` | REMOVE | `bind` (:4023-4053): `reused` feeds `tex_reused`/`tex_changed`/`work.note_bind(reused)` and nothing else; the pointer is inserted into a map and compared, never dereferenced. Safe |
| `dcomp` `work_device` under `DCOMP_BIND_PROF` (:3803) | REMOVE | see "Resolved" above. Safe |
| `commit_guard::GuardRole` + the `role` parameter | REMOVE | `device_guard` (:165-176): `role` reaches only `record_lock_wait(role, …)`. The `lock.lock().unwrap_or_else(|e| e.into_inner())` on both the gated and ungated paths is identical. Safe — **but keep both `unwrap_or_else` poison recoveries** |
| `script_thread::rendering_update_from_timer` | REMOVE | `update_the_rendering` (:1396-1398): `if self.rendering_update_from_timer.swap(false, …) { note_rendering_update_reason("fallback-timer"); }` — the swap's only effect is the tally. The sibling `trigger_script_thread_animation.store(true, …)` at :1216 is a different Arc and stays |
| `document::note_rendering_update_reason` call sites inside `needs_rendering_update` | REMOVE, keep every `return true` | read :2990-3045: all nine calls are standalone statements immediately above a `return true;`. The big `match restyle_reason` at :2998-3005 is the sole argument of one such call. Safe |
| `painter::note_dcomp_frame_statistics` | KEEP (reduce) | confirmed the two-armed gate at :4285-4290 and that `paint_animation_sample_at` :4318-4337 consumes `lead_to_next_composition`. The inventories' trap writeups are accurate |

---

## 5. Ordering and precision problems

**O-1 (compile break). `debug_env::WEBGL_FANOUT_PROF` has two readers, not one.**
`strip-inventory-paint_misc.md` step 4 says "`debug_env::WEBGL_FANOUT_PROF` becomes dead" after
`webrender_external_images.rs` is stripped. `strip-inventory-config_surface.md` B.1 lists its
readers as `paint/webrender_external_images.rs:37-40` only. The second reader is
**`components/webgl/webgl_thread.rs:337`**. Deleting the const at that point breaks `servo-webgl`,
and because `servo-paint` depends on `webgl`, it also breaks the stated build gate. Strip
§1 above **before** touching `debug_env.rs`.
(Verified by `grep -rnoE 'debug_env::[A-Z_0-9]+'` over `components/` + `ports/`: every other flag
has exactly the readers the config inventory lists; `FRAME_TIME_PROF`, `LOG_PRESENT_CADENCE`,
`D3D11_PROFILE` and `MEDIA_DISABLE_ENOUGHDATA_BACKOFF` each have two, all four correctly listed.)

**O-2 (brace break, silent behaviour risk). `paint.rs` barrier log removal cuts an `if/else if`
chain that carries a load-bearing assignment.**
`strip-inventory-painter_paint.md` says "REMOVE paint.rs:1000-1032 (`first_to_all_ready_ms` /
`request_to_all_ready_ms` + completed_after_deadline `info!`)" and separately "KEEP
`barrier.completed_at`". The actual shape at :998-1035 is:

```
if barrier.ready_painter_ids.len() == barrier.expected_painter_ids.len() {
    barrier.completed_at = Some(diagnostic.ready_at);          // KEEP
    if barrier.missed_deadline {                                // opener is INSIDE the range
        …two _ms locals…  info!("Wall frame barrier complete: …");
    } else if !barrier.presentation_decision_made {
        complete_before_deadline = true;                        // LOAD-BEARING, outside the range
    }
} else if !barrier.presentation_decision_made { debug!(…) }
```
A line-range delete leaves a dangling `} else if`. The correct edit is to collapse the chain to
`if !barrier.missed_deadline && !barrier.presentation_decision_made { complete_before_deadline = true; }`.
Say so in the plan, or someone will "fix" the braces by deleting the `complete_before_deadline`
arm and silently turn every before-deadline barrier into an after-deadline one.

**O-3 (file mangling). Four line ranges in `strip-inventory-winit_wall.md` are wrong, and two of
them overlap a neighbouring item.** Verified against the file:

| inventory says | actual | consequence |
|---|---|---|
| `struct TickIssueAudit` + impl = main.rs:465-503 | doc from ~472, `struct` at **487**, `impl` at **491**, ends ~505 | 465-479 is **inside `ClockStats`'s field list**. A blind delete removes `tick_by_site: [u32; 4],` and `ClockStats`'s closing brace |
| `struct ClockStats` = 416-479 | struct at **433**, body ends ~470 | over-reaches ~9 lines into `TickIssueAudit`'s doc |
| `struct PassStats` = 530-571 | doc at ~537, `struct` at **538** | 530-535 is `enum MainSlot`'s body (inventory puts MainSlot at 527-535) |
| `report_clock_stats` = 951-1058 / `report_main_busy` = 1054-1083 | `fn report_clock_stats` at **955**, `fn report_main_busy` at **1060** | 5-line overlap |

Treat every line range in that inventory as advisory; delete by symbol, not by `sed` range. The
other six inventories' ranges spot-checked clean.

**O-4 (factual error, harmless verdict).** The winit_wall inventory says of
`AppState::tick_issue_audit`: "the `borrow_mut()` itself still runs in release builds — pure
overhead". The source says the opposite, and the source is right: the call is inside
`debug_assert!`, whose argument is not evaluated in release, and the doc at main.rs:482-485 states
"★릴리스 빌드에서는 아예 돌지 않는다★(`debug_assert!` 가 인자째 사라진다)". The REMOVE verdict
stands; the justification does not. Don't let it set an expectation of a measurable win.

**O-5. `servo_base::interval_summary` has no owner.** See §3. It should be the very last removal,
after `frame_time.rs`, `paint.rs:130`, `painter.rs:4966` and `script/raf_clock.rs`.

**O-6. Two inventories both claim `output_grid.rs:2175` (`commit_guard::emit_lockwait()`).**
output_grid step 1 deletes it as part of `probe_loop`; paint_misc step 5 says "either squash with
the output_grid pass or land `output_grid.rs:2175`'s deletion in this commit". Pick one owner
before starting, or the second commit will conflict.

**O-7. `comp_stats.rs` deletion is a two-file atomic edit with two claimants.** output_grid step 1
orphans it; paint_misc step 7 deletes it. `output_grid::emit_comp_stats` (:2246-2450) is its sole
consumer (`completed_frame_id` :2265, `sample_frame` :2304, type refs :2455-2456) and `lib.rs:135-136`
declares it. One commit, one owner.

**O-8. `COLLECT`'s re-expression and its deletion are two different plans.** dcomp step 7 says to
rewrite `output_grid.rs:51` as `COLLECT = *PROBE_ENABLED` when `DCOMP_BIND_PROF` goes; output_grid
step 5 says to delete `COLLECT` outright once its four out-of-file readers are gone. Both are
correct, in that order: dcomp step 7 must land **before** output_grid step 5, otherwise
`output_grid.rs` references a deleted `DCOMP_BIND_PROF`.

**O-9. The build gate is wrong for most of this work, and three inventories say so separately.**
Consolidated: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu` compiles
`components/paint` and (transitively) `components/webgl`, `components/shared/*`,
`components/media`, `components/script`, `components/layout`. It does **not** build
`components/servo/examples/winit_wall`, and it does **not** run
`components/config/tests/config_surface.rs`. Per step, also run:

```
cargo check -p servo --example winit_wall --features webgl/no-wgl,webgpu
cargo test  -p servo --example winit_wall --features webgl/no-wgl,webgpu
cargo test  -p servo-config --test config_surface
cargo check -p servo-webgl -p layout -p script
```
Expect `every_env_name_read_in_sources_is_registered` to be **red before you start** (five
unregistered `SERVO_*` literals) and to go green once B.2 of the config inventory lands.

---

## 6. Coverage map — what is now accounted for, and what is still unswept

Swept by me, tree-wide, for: ALL-CAPS log tags in `warn!/info!/debug!/trace!`,
`static X: LazyLock|Mutex|RwLock|OnceLock|Atomic` holding samples, named `thread::Builder` spawns,
`QueryPerformanceCounter` / `Instant::now()` captures reaching only a log, `#[cfg(feature)]`
measurement blocks, and `probe`/`prof` modules.

| area | status |
|---|---|
| `components/paint/*` | covered by the inventories; `webview_renderer.rs` independently verified **clean** (only two upstream `warn!` on hit-test and IPC failure) |
| `components/webgl` | **gap — §1** |
| `components/servo/*.rs` | **gap — §2** (`SPINSLOW` only) |
| `components/shared/base/interval_summary.rs` | **gap — §3** (orphaned, not misclassified) |
| `components/shared/paint/*` | swept: `ANGLE_GL_EXCLUSION`/`ANGLE_GL_DEVICE_LOCKS` are real locks (KEEP); `report_unknown_external_image`'s `SEEN` (lib.rs:905) is a bounded error-report limiter, not a window stat (KEEP); `D3D11_TEXTURES_RELEASED` (rendering_context.rs:64, 1186) becomes dead with `MEDIARINGS` — already flagged UNCERTAIN by the script/media inventory, and its partner `D3D11_TEXTURES_CREATED` is REMOVE there |
| `components/webgpu`, `components/canvas`, `components/constellation`, `components/net`, `components/fonts`, `components/storage`, `components/devtools` | swept: no fork instrumentation. The `VELLO`/`CREATE`/`SELECT` ALL-CAPS hits are an error path and SQL |
| `components/profile`, `components/shared/profile` | upstream Servo's own time/memory profiler — out of scope, correctly untouched by every inventory |
| `ports/servoshell` | swept: nothing fork-instrumentation. `components/servo/examples/winit_minimal.rs` clean |
| `components/media/backends/gstreamer/player.rs:988 GstSyncGroupWatchdog` | **not instrumentation** — a 30 s timeout that releases held pipelines. Correctly absent from the inventory; listed here so nobody deletes it by the "watchdog thread" pattern |

---

## 7. Traps to add to the master list

These are on top of the (good) trap sections the seven inventories already carry.

* **`webgl_thread::maybe_reclaim_idle_contexts` (1506-…) is VRAM reclaim, not a probe.** It is
  called from the 500 ms `recv_timeout` arm (:558) and after every message (:563), it sits next to
  the `WEBGLRECLAIM` warns, and it owns `released_contexts` / `context_recipes` / `last_composited`
  / `last_reclaim_sweep` — all of which read like census state. Only the two `warn!` lines go.
* **`webgl_thread::log_live_contexts` is ungated.** Unlike everything else in §1 it is not behind
  `WEBGL_FANOUT_PROF`; it runs on every build. It is also the counterpart the script inventory's
  `report_live_contexts` row says it cannot see.
* **`Document::rendering_update_reasons` (document.rs:628) is not the fork's tally.** It is
  upstream `Cell<RenderingUpdateReason>`, read at :3019 and :3142 to **decide** whether a rendering
  update is needed, written by `add_rendering_update_reason` (:3866) and cleared by
  `clear_rendering_update_reasons` (:3871, called from `script_thread.rs:1466`). The fork's
  instrumentation tally is the thread-local **`RENDERING_UPDATE_REASONS`** (:329-334) and the free
  functions `note_rendering_update_reason` / `take_rendering_update_reasons`. Near-identical names,
  opposite verdicts. No inventory calls this out.
* **`servo.rs`'s `paint.handle_messages(messages)` and `perform_updates()` are wrapped in the
  `SPINSLOW` stopwatch.** The stopwatch goes; both calls and the `messages` gather stay.
* **`components/paint/Cargo.toml`: keep `processthreadsapi` and `profileapi`.** Only `psapi` and
  `dxgi1_4` become unused. `processthreadsapi` survives `thread_cpu_ms` because of
  `SetThreadPriority` at painter.rs:904; `profileapi` survives because `qpc_now`/`qpc_frequency` do.

---

## 8. Verdict

The seven inventories are **safe on classification and unsafe on completeness**. I could not find
a single REMOVE that is load-bearing — the traps that matter (the `WaitForVBlank` clock, the
`COMPOSITION` write ahead of the `COLLECT` gate, `GetFrameStatistics` as the B2 grid, the
`composition_grid` vs `dwm_timing` split, `last_pacing_tick_at` inside `note_tick_site`,
`PumpSource::may_schedule`, `wall_tick_frames_produced`, `DEVICE_LOCKS`, `SinkPacer`,
`lock_tracked`'s poison recovery, `LIVE_CONTEXTS`'s prune, `quantize_update_rect`'s tests) are all
caught and correctly argued. What is missing is one unassigned crate holding a sampling thread and
three log tags, one unassigned file holding `SPINSLOW`, an orphaned shared module, two stale
Cargo features, and a handful of line ranges in the winit_wall inventory that would mangle the
file if executed literally. Fix O-1 and O-2 before any commit lands; fix O-3 by deleting symbols
rather than ranges.
