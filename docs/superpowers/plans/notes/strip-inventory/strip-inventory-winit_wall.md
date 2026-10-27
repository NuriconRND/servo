# Inventory: servo/examples/winit_wall/

Subsystem root: `W:\servo_multigpu-tiled-wall\components\servo\examples\winit_wall\`
Branch: `wall-strip-instrumentation`. Read-only pass; no file was edited.

## Directory contents (every file covered)

| file | lines | role | instrumentation present? |
|---|---|---|---|
| `main.rs` | 2739 | shell: args, event loop, present clock, pacing thread, render pass | **yes — the bulk of it** |
| `tile.rs` | 474 | one tile's window / context / GPU binding, `--tile-thread-spike` | only the spike harness (see cat. 4) |
| `tick_refresh_driver.rs` | 215 | `RefreshDriver` fired by the present tick | no — load-bearing frame clock |
| `vsync_refresh_driver.rs` | 113 | `DwmFlush`-blocking `RefreshDriver` | no — load-bearing frame clock |
| `crash_report.rs` | 150 | native crash stack dump (module+offset) | no — crash diagnostics, not perf |
| `device_auth_selftest.rs` | 183 | `--device-auth-selftest` state machine | no — product self-test |

The whole directory is fork code (added by `fbfb2c41ae9 feat(winit_wall): 표출 셸 기반 이식`), so the
"is it upstream?" test never rescues anything here; every verdict below is decided by
*does a rendering or timing decision read it*.

---

## Category 1 — containers added purely to hold statistics ("the mutexes")

**Finding up front: winit_wall holds no statistics `Mutex`.** Everything in this shell runs on the
main thread, so the stats containers are `Cell`/`RefCell` fields on `AppState`, not `Mutex`/`LazyLock`
statics. The two real `Mutex`es in this directory are both load-bearing frame-clock plumbing. The
static `Mutex<Option<Vec<..>>>` sample buffers the user is remembering live in
`components/paint/output_grid.rs` and `components/paint/commit_guard.rs` — a different subsystem.

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `AppState::present_ms_sum` | main.rs:292 | `Cell<f64>` field | **REMOVE** | written+read only by `note_present` (775-801); init at 2168 | feeds the `Present perf:` eprintln only |
| `AppState::present_count` | main.rs:293 | `Cell<u32>` field | **REMOVE** | same, `note_present` only; init 2169 | " |
| `AppState::present_window_start` | main.rs:294 | `Cell<Option<Instant>>` | **REMOVE** | same, `note_present` only; init 2170 | 1 s window boundary for `Present perf` |
| `AppState::pass_stats` | main.rs:303 | `RefCell<PassStats>` | **REMOVE** | borrowed only in `note_render_pass` (655, 769); init 2171 | backing store for `WALLPASS`/`WALLQUEUE`/`WALLSPLIT` |
| `AppState::main_busy` | main.rs:386 | `RefCell<MainBusy>` | **REMOVE** | borrowed in `charge_main` (849) and `report_main_busy` (1061, 1082); init 2213 | backing store for `MAINBUSY` |
| `AppState::clock_stats` | main.rs:388 | `RefCell<ClockStats>` | **REMOVE** | borrowed in `note_present_tick` 870, `note_tick_jitter` 882, `note_tick_site` 896, `note_about_to_wait` 915, `note_redraw_requested` 923, `note_redraw_suppressed` 933, `note_clock_render_pass` 939, `report_clock_stats` 956/1051, backstop bump 1576; init 2214 | backing store for `WALLCLOCK`. Every reader is itself REMOVE |
| `AppState::tick_issue_audit` | main.rs:373-375 | `RefCell<TickIssueAudit>` | **REMOVE** | borrowed only at 1651 inside `fire_present_tick`, feeding a `debug_assert!`; init 2157 | the `borrow_mut()` itself still runs in release builds — pure overhead |
| `struct MainBusy` | main.rs:403-414 | struct | **REMOVE** | only `AppState::main_busy` | 7 accumulator fields |
| `struct ClockStats` | main.rs:416-479 | struct (+63 lines of doc) | **REMOVE** | only `AppState::clock_stats` | `ticks/redraw/renders/suppressed/backstop/gaps_ms/tick_jitter_ms/last_render_at/about_to_wait/tick_by_site` |
| `struct PassStats` | main.rs:530-571 | struct | **REMOVE** | only `AppState::pass_stats` | |
| `struct PassSplit` | main.rs:573-586 | struct | **REMOVE** | constructed at 1679, filled 1736/1763/1769/1772/1776/1786/1789/1795/1824/1833, consumed only by `note_render_pass` | every field is a stopwatch result |
| `struct TickIssueAudit` + `impl` | main.rs:465-503 | struct + `record_tick` | **REMOVE** | `record_tick` called at 1652 (inside `debug_assert!`) and from the two unit tests at 2626-2640 | doc comment itself says "릴리스 빌드에서는 아예 돌지 않는다" |
| `enum MainSlot` | main.rs:527-535 | enum | **REMOVE** | only argument of `charge_main` (1660, 2326, 2444, 2453) | |
| `const MAX_CLOCK_GAP_SAMPLES` | main.rs:521-525 | const | **REMOVE** | read only at 943 in `note_clock_render_pass` | |
| `AppState::last_pacing_tick_at` | main.rs:316-318 | `Cell<Option<Instant>>` | **KEEP** | **written** inside `note_tick_site` (892-894); **read** by `pacing_is_driving()` (908-912), which gates `WakerEvent::Engine if !state.pacing_is_driving()` (2360) and `needs_pacing_backstop(..)` (2426-2432) | **TRAP.** It lives inside a `note_*` function but it is the pacing-liveness latch. If you delete `note_tick_site` wholesale the engine-wake backstop fires every wake and the clock goes back to riding engine activity — exactly the regression the 04/07_temp measurement documents |
| `enum ClockSite` | main.rs:505-520 | enum | **KEEP (reduced)** | passed through `drive_present_clock` (1096), `pump_vsync` (1132), `drive_present_clock_timer` (1557); consumed by `note_tick_site` (889) for `tick_by_site[site as usize]` **and** for the `PacingThread` latch | Only the `PacingThread` discriminant is load-bearing. The four-variant layout exists for the `tick_by` log column; it may shrink to `PacingThread` vs. everything-else (or a `bool`), but something must still tell `drive_present_clock` who called it |
| `AppState::pass_counter` | main.rs:371-372 | `Cell<u64>` | **UNCERTAIN** | read 1693, bumped 1697-1698, init 2212; only consumer is the `gfx_wall_rotate_tile_order` rotation at 1690-1699 | see cat. 4 row for `gfx_wall_rotate_tile_order` — it is a measurement aid, but deleting it also means deleting a pref defined in `components/config/prefs.rs:483,1327` |
| `Pending::callbacks: Mutex<Vec<StartFrameCallback>>` | tick_refresh_driver.rs:18-21 | `Mutex` | **KEEP** | `Pending::fire` (24-39) drains it; `TickRefreshDriver::observe_next_frame` (95-99) pushes; `TickFireHandle::fire` (114-116) | This is the frame clock's callback queue — Servo's `waiting_for_frame` is cleared from here. Not statistics |
| `Shared::state: Mutex<(Vec<..>, bool)>` + `Condvar` | vsync_refresh_driver.rs:32-36 | `Mutex`+`Condvar` | **KEEP** | `vsync_loop` (62-90), `observe_next_frame` (92-99), `Drop` (102-112) | the `DwmFlush` vsync driver. Not statistics |
| `AppState::vsync_due: Arc<AtomicBool>` | main.rs:352 | atomic | **KEEP** | set in the `observe_next_frame` closure (1145-1148), swapped in `pump_vsync` (1136) | the vsync edge itself |
| `AppState::wake_pending: Arc<AtomicBool>` | main.rs:390-398 | atomic | **KEEP** | `Waker::wake` (2508-2521) coalesces; `user_event` clears (2321) | wake coalescing, a behaviour fix, not a counter |

Category 1: **REMOVE 14 · KEEP 6 · UNCERTAIN 1**

---

## Category 2 — `note_*` / `report_*` functions and the 1 Hz rollups

All `note_*` here are `AppState` methods (there are no `emit_*` in this directory; the 1 Hz
emitters are named `report_*`).

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `AppState::note_render_pass` | main.rs:588-773 | fn (186 lines) | **REMOVE** | one caller: `render_all_tiles` at 1834-1840 (discards return) | emits `PASSEVENT` (603-644), `WALLPASS` (696-714), `WALLQUEUE` (716-734), `WALLSPLIT` (736-767); resets `pass_stats` (769-772) |
| ↳ `PASSEVENT` warn block | main.rs:596-644 | log site | **REMOVE** | inside the above; deliberately *before* the `LOG_PASS` gate | `const PASS_EVENT_MS = 5.0` goes with it |
| ↳ `static LOG_PASS: LazyLock<bool>` | main.rs:646-652 | static | **REMOVE** | read at 650 only | reads `servo_config::debug_env::LOG_PRESENT_CADENCE`. **Cross-subsystem:** `components/paint/painter.rs:85` also reads that flag, so `debug_env::LOG_PRESENT_CADENCE` and its `ALL` entry must survive until the paint side drops it too, or `components/config/tests/config_surface.rs::every_registered_flag_is_actually_read_somewhere` fails |
| `AppState::note_present` | main.rs:775-801 | fn | **REMOVE** | called at 1777 and 1796 inside `render_all_tiles` | `eprintln!("Present perf: presents_per_s=.. avg_present_ms=..")`, ungated, once/sec |
| `AppState::charge_main` | main.rs:839-867 | generic fn | **REMOVE** | 1660 (`MainSlot::Render`, wraps `render_all_tiles`), 2326 (`Spin`, wraps `spin_event_loop`), 2444 (`Spin`), 2453 (`Render`) | it returns `body()`'s value unchanged; each call site becomes the bare body |
| `AppState::note_present_tick` | main.rs:868-875 | fn | **REMOVE** | 1170 (`pump_vsync`), 1578 (`drive_present_clock_timer`) | **do not confuse with the free function `servo::note_present_tick()` at 1613** — see Traps |
| `AppState::note_tick_jitter` | main.rs:877-887 | fn | **REMOVE** | 1579 only | pushes into `ClockStats::tick_jitter_ms` → `WALLCLOCK jit_ms` |
| `AppState::note_tick_site` | main.rs:889-903 | fn | **REMOVE — with a carve-out** | 1171 (`pump_vsync`), 1580 (`drive_present_clock_timer`) | lines 896-902 (`tick_by_site` bump) are instrumentation; **lines 891-895 (`last_pacing_tick_at.set(..)` when `site == PacingThread`) are load-bearing.** Keep that as a 3-line `note_pacing_tick(site)` or inline it at both call sites |
| `AppState::note_about_to_wait` | main.rs:914-920 | fn | **REMOVE** | 2387 (`about_to_wait`) | `WALLCLOCK atw=` |
| `AppState::note_redraw_requested` | main.rs:922-930 | fn | **REMOVE** | 1659 (`fire_present_tick`, direct-render path), 2452 (`WindowEvent::RedrawRequested`) | `WALLCLOCK redraw=` |
| `AppState::note_redraw_suppressed` | main.rs:932-935 | fn | **REMOVE** | **no callers** (`#[allow(dead_code)]` at 931) | dead already |
| `AppState::note_clock_render_pass` | main.rs:936-949 | fn | **REMOVE** | 1669, first line of `render_all_tiles` | `WALLCLOCK renders=` and the `gaps_ms` distribution |
| `AppState::report_clock_stats` | main.rs:951-1058 | fn (108 lines) | **REMOVE** | 2389 (`about_to_wait`) | the `WALLCLOCK` 1 Hz line, with its percentile/`inband`/`dup`/`one`/`skip1`/`skipN` helpers |
| `AppState::report_main_busy` | main.rs:1054-1083 | fn | **REMOVE** | 2388 (`about_to_wait`) | the `MAINBUSY` 1 Hz line |
| backstop bump | main.rs:1575-1577 | stmt | **REMOVE** | inside `drive_present_clock_timer`; the `if self.vsync_stalled.get()` test has no other effect here | `WALLCLOCK backstop=`. The `vsync_stalled` flag itself is load-bearing and stays |
| `let fired_for = next;` | main.rs:1565-1568 | local | **REMOVE** | only consumer is `note_tick_jitter` at 1579 | |
| pass stopwatch locals in `render_all_tiles` | main.rs:1674 (`pass_start`), 1714/1736 (`dispatch_start`/`dispatch_ms`), 1745/1764/1799 (`tile_start`), 1761-1763/1770-1772/1787-1789 (`paint_start`), 1767-1769/1784-1786 (`current_start`), 1773-1776/1792-1795 (`present_start`), 1822-1824 (`join_start`), 1831-1833 (`flush_start`), 1675/1678 (`tile_ms`/`tile_queued_ms` vecs) | locals | **REMOVE** | every one of them terminates in `split.*` or `tile_ms[..]`, which only `note_render_pass` reads | the *operations* being timed (`make_current`, `paint_target`, `present`, `flush_deferred_dcomp_commits`) all stay; only the `Instant::now()`/`elapsed()` wrappers go |
| `let mut skipped = 0u32;` | main.rs:1680, bumped 1726, 1754 | local | **REMOVE** | passed to `note_render_pass` at 1838 and nowhere else | the `continue` next to each bump is the barrier and stays |
| `let mut dispatched_indices` | main.rs:1707-1710, pushed 1732, read 1825 | local | **REMOVE** | exists only to put join timings back into `tile_ms` | **not** `dispatched: Vec<bool>` (1705) — that one gates the second loop and is load-bearing |
| `let tile_times = in_flight.join();` | main.rs:1823-1828 | value use | **REMOVE (value only)** | `timing.render_ms` / `timing.queued_ms` → `tile_ms`/`tile_queued_ms` | **the `join()` call must stay** (it must complete before `flush_deferred_dcomp_commits`). Becomes `let _ = in_flight.join();` |
| `AppState::pacing_is_driving` | main.rs:904-912 | fn | **KEEP** | 2360 (`WakerEvent::Engine if !state.pacing_is_driving()`), 2429 (`needs_pacing_backstop`) | decides who is allowed to drive the clock — pure control flow |
| `servo::note_frame_time(CrossProcessInstant::now())` | main.rs:1604-1609 | call | **KEEP** | the engine ships it in `TickAnimation` to script; it is the page's rAF timestamp | product feature (`dom_animation_timeline_use_frame_time` / `Document::resolve_frame_time`), explicitly out of scope |
| `servo::note_present_tick()` | main.rs:1610-1613 | call | **KEEP** | `components/paint/lib.rs:97-101` → `wall_tick::note_tick_edge()` **(load-bearing: the per-edge external-present budget and the `commit_deadline()` that `gfx_present_align_commit_pct` needs)** + `output_grid::note_present_tick_now()` (instrumentation) | **TRAP.** Same name as the removable `AppState::note_present_tick`. Removing the winit_wall call would silently disable commit alignment. The instrumentation half is inside `paint` and belongs to that subsystem's inventory |
| `self.tick_fire.fire()` | main.rs:1621-1633 | call | **KEEP** | fires Servo's registered frame callbacks; clears `waiting_for_frame` | |
| `webview.on_wall_tick()` | main.rs:1650 | call | **KEEP** | issues the wall frame | its **return value** `issued` is only read by the audit `debug_assert!` → becomes `let _ = webview.on_wall_tick();` |
| `webview.commit_deferred_dcomp_at_tick()` | main.rs:1641-1643 | call | **KEEP** | flushes the deferred DComp commit at the tick head — this is the commit phase | |
| `in_flight.join()` | main.rs:1823 | call | **KEEP** | synchronisation before the commit flush | see the row above about its return value |

Category 2: **REMOVE 20 · KEEP 7 · UNCERTAIN 0**

---

## Category 3 — DWM / display OS syscalls

**Nothing in this directory queries DWM or the display purely to log.** Every OS query here either
paces the frame clock or places a window. Listed exhaustively so nobody deletes one by pattern match.

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `servo::dwm_composition_grid()` (`DwmGetCompositionTimingInfo`) | called at main.rs:1235, inside `snap_to_dwm_grid_at` (1226-1294) | syscall | **KEEP** | `snap_to_dwm_grid_at` returns the snapped `Instant`; consumers are `AppState::snap_to_dwm_grid` (1222-1224) → `drive_present_clock_timer:1572` (sets `next_present_tick`) and `spawn_pacing_thread`'s timer fallback (1377) | **TRAP (named in the task).** This is the timer-fallback path of the frame clock. It is the *only* winit_wall call site of this function — there are no logging call sites here to separate out |
| `servo::wait_primary_vblank()` (`IDXGIOutput::WaitForVBlank`) | called at main.rs:1418, inside `pace_on_vblank` (1404-1457) | blocking syscall | **KEEP** | `pace_on_vblank` → `spawn_pacing_thread`'s loop (1344-1368) → `proxy.send_event(WakerEvent::Tick)` | **TRAP (named in the task).** This *is* the frame clock |
| `QueryPerformanceCounter` / `QueryPerformanceFrequency` extern + `qpc_frequency()` | main.rs:43-63 | syscall + cached helper | **KEEP** | `qpc_frequency` read at 1238 (`snap_to_dwm_grid_at`), 1431 (`pace_on_vblank`), 1470 (`align_after_vblank`); `QueryPerformanceCounter` at 1252 and 1474 | all three consumers compute a sleep target, not a log line |
| `align_after_vblank` | main.rs:1459-1484 | fn (sleeps) | **KEEP** | `pace_on_vblank` at 1435 and 1456 | implements `-DwmAlign 1..=99`. It is an A/B knob, but it moves the tick, so it is a timing decision |
| `DwmFlush` extern + `vsync_loop` | vsync_refresh_driver.rs:23-28, 62-90 | blocking syscall + thread | **KEEP** | `DwmVsyncRefreshDriver::new` spawns it; the shell subscribes via `AppState::present_vsync` → `pump_vsync` | the `gfx_vsync_enabled` frame clock |
| `dxgi_luid_for_gpu_index(..)` LUID cross-check | tile.rs:229-237 | DXGI query → `eprintln!` warning | **KEEP** | the returned LUID is compared to `disp.luid` and only printed on mismatch | *Closest thing in this directory to "a display syscall whose result only reaches a log string"* — but it is a one-shot startup **misconfiguration** warning (wrong adapter driving a tile), not a statistic, and it is the only signal that auto-GPU binding went wrong. Flagging rather than removing |
| `enumerate_display_topology()` / `spatial_order()` + the topology dump | main.rs:1951-1984 | enumeration + `eprintln!` | **KEEP** | `spatial` drives window position, size and `gpu_index` throughout `plan_tile_windows` | the dump is a side effect of data that placement needs |
| `window.current_monitor()` + borderless-fullscreen log | tile.rs:308-321 | query + `eprintln!` | **KEEP** | decides `set_fullscreen(..)` for flip-model eligibility | behaviour, not measurement |

Category 3: **REMOVE 0 · KEEP 8 · UNCERTAIN 0**

---

## Category 4 — everything else added to evaluate winit_wall performance

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `tests::tick_issue_audit_accepts_increasing_numbers_and_gaps` | main.rs:2624-2632 | `#[test]` | **REMOVE** | exercises `TickIssueAudit::record_tick` only | must go with `TickIssueAudit` or the example stops compiling under `cargo test` |
| `tests::tick_issue_audit_rejects_a_repeated_or_backwards_number` | main.rs:2634-2640 | `#[test]` | **REMOVE** | same | " |
| pacing-thread banner text | main.rs:2276-2279 | `eprintln!` string | **REMOVE (text only)** | printed once when `gfx_wall_pacing_thread` is on | says "(WALLCLOCK 의 tick_by pace 가 대부분이어야 한다)" — references a log line that will no longer exist. Keep the banner, drop the clause |
| `gfx_wall_rotate_tile_order` rotation | main.rs:371-372 (`pass_counter`), 1681-1699 (`rotate`/`offset`/counter bump), 1742 (`(offset + step) % count`), 2212 (init); pref at `components/config/prefs.rs:483,1327`; launcher switch `-RotateTileOrder` at `etc/multigpu/run_wall_dist.ps1:593-602,1046` | pref-gated behaviour knob | **UNCERTAIN** | the only consumer of `pass_counter`; its own doc (1682-1689) states its purpose is to decide whether a slow tile is "the first position" or "that painter" — i.e. it exists to disambiguate a measurement | It *does* change draw order when enabled (default `false`), so it is not pure instrumentation. Removing it also deletes a pref (different subsystem) and a launcher switch. **Recommend: remove, but as a coordinated change, not as part of the winit_wall-local edit** |
| `--tile-thread-spike` / `TILESPIKE` harness | main.rs:94-96, 141, 180 (`Config::tile_thread_spike`), 2047-2058 (`resumed` early branch), 264-268 (`App::SpikeFinished` exit code), 1896-1906 (`enum App` variant); tile.rs:332-474 (`plan_and_run_tile_thread_spike`, `run_tile_thread_spike`) | separate CLI mode, prints `create_ms`, vendor/renderer, PASS/FAIL | **UNCERTAIN** | `plan_and_run_tile_thread_spike` called only at main.rs:2048; `run_tile_thread_spike` only from it | It is a **feasibility** probe (can a worker thread own a `RenderingContext`?) that already returned its answer, not a per-frame perf counter. It does print a timing (`create_ms`). It costs nothing at runtime unless the flag is passed. **Ask the user**: dead scaffolding worth deleting, or a kept regression check? |
| `servo::config_dump::log_effective_config()` | main.rs:1922; impl `components/config/config_dump.rs:18` | startup config dump | **UNCERTAIN** | none inside winit_wall beyond the one call | Not a performance measurement, but it exists to make measurement runs reproducible. Harmless and once-per-start. Leaning KEEP; flagged because it is a logging-only facility |
| `--capture` / `--capture-sec` machinery | main.rs:81-84, 127-133, 145-150 (args), 803-837 (`maybe_capture`), 1791 (call), 2201-2207 (deadline), 2392-2400 (`about_to_wait` polling) | render-validation PNG dump | **KEEP** | `maybe_capture` called once from the primary-tile arm of `render_all_tiles`; sets `should_exit` | correctness validation, not performance measurement; the user did not ask for it |
| `crash_report::install()` | main.rs:240, crash_report.rs (whole file) | crash handler | **KEEP** | `main` at 240 | native crash stacks; unrelated to perf |
| `device_auth_selftest` (whole file + wiring) | device_auth_selftest.rs; main.rs:283-289, 1533-1555, 2374-2383 | product self-test | **KEEP** | | unrelated to perf |
| `show_console_message` | main.rs:1878-1896 | delegate | **KEEP** | Servo calls it | without it a probe page cannot say anything; it is the page's own `console`, not a shell counter |
| `notify_new_frame_ready` (empty body) | main.rs:1868-1878 | delegate | **KEEP** | Servo calls it | the emptiness is the fix — making presentation independent of a content signal. Do not "clean up" by deleting the impl |
| `FrameClock` startup banners | main.rs:2006-2026 | `eprintln!` | **KEEP** | `resumed` | states which clock was selected; operational, once per start |
| other `#[cfg(test)]` tests in main.rs | 2644-2674 (`every_clock_has_a_handle_the_tick_can_fire`), 2675-2691 (`load_status_maps_to_document_event`), 2692-2712 (`vsync_wins_over_pacing`, `pacing_only_when_the_thread_runs`, `no_pacing_thread_falls_back_to_timer`), 2713-2722 (`only_vsync_drives_the_shell_present_clock`), 2723-2738 (`backstop_only_when_pacing_was_installed_but_is_not_running`) | `#[test]` | **KEEP** | exercise `frame_callback_wiring`, `choose_frame_clock`, `FrameClock::drives_shell_present_clock`, `needs_pacing_backstop`, `document_event` | all cover load-bearing clock wiring. `backstop_only_when_pacing_was_installed_but_is_not_running` in particular guards the `last_pacing_tick_at` behaviour that cat. 1 flags as a trap |
| `tick_refresh_driver::tests` (5 tests) | tick_refresh_driver.rs:119-215 | `#[test]` | **KEEP** | exercise `fire`/`observe_next_frame`/`needs_paint_timer_heartbeat` | frame clock |
| stale doc-comment references to the removed log tags | main.rs:302 (`SERVO_LOG_PRESENT_CADENCE`), 383-385, 437-479, 521-524, 554-556, 604-606, 716-722, 736-742, 951-953, 1091-1092, 1189, 1270-1275, 1300-1313, 1563-1566, 1601-1602, 2024-2026, 2277-2279, 2402-2432, 2479-2483, 2724-2731 | comments | **REMOVE (text only)** | — | these sit inside code that is otherwise KEEP; they will point at log lines that no longer exist. Rewriting them is part of the strip, not a separate task |

Category 4: **REMOVE 4 · KEEP 7 · UNCERTAIN 3**

---

## Totals

| verdict | count |
|---|---|
| REMOVE | 38 |
| KEEP | 28 |
| UNCERTAIN | 4 |

---

## Removal order and build-breakage notes

The build for this subsystem is **not** `cargo test -p servo-paint --lib`. winit_wall is an *example*
of the `servo` package (`components/servo/Cargo.toml`, `[package] name = "servo"`), so nothing here is
compiled by that gate. Use, in addition:

```
cargo check -p servo --example winit_wall --features webgl/no-wgl,webgpu
cargo test  -p servo --example winit_wall --features webgl/no-wgl,webgpu
```

Do the `servo-paint` gate too, because step 6 below touches a shared debug flag.

Each step below leaves the tree compiling on its own.

**Step 1 — the self-contained rollups (no cross-dependencies).**
Delete `report_main_busy` (1054-1083), `charge_main` (839-867), `enum MainSlot` (527-535),
`struct MainBusy` (403-414), `AppState::main_busy` (380-386) and its initialiser (2213).
Then unwrap the four `charge_main` call sites: 1660 → `self.render_all_tiles();`,
2326 and 2444 → `state.servo.spin_event_loop();`, 2453 → `state.render_all_tiles();`.
Drop the `report_main_busy()` call at 2388.

**Step 2 — `Present perf`.**
Delete `note_present` (775-801) and the three `Cell` fields (292-294) plus initialisers (2168-2170).
Remove the two calls at 1777 and 1796. The surrounding `let present_ms = present_start.elapsed()..`
locals die with step 4; until then they become unused — fold step 4 in or expect two warnings.

**Step 3 — `WALLCLOCK`, carefully.**
Order matters because of the `last_pacing_tick_at` trap:
 1. First extract the latch. Replace `note_tick_site(site)` at 1171 and 1580 with a call that *only*
    does `if matches!(site, ClockSite::PacingThread) { self.last_pacing_tick_at.set(Some(Instant::now())) }`.
    Verify `backstop_only_when_pacing_was_installed_but_is_not_running` and
    `only_vsync_drives_the_shell_present_clock` still pass.
 2. Then delete `AppState::note_present_tick` (868-875), `note_tick_jitter` (877-887),
    `note_about_to_wait` (914-920), `note_redraw_requested` (922-930),
    `note_redraw_suppressed` (932-935), `note_clock_render_pass` (936-949),
    `report_clock_stats` (951-1058), the backstop bump (1575-1577), `let fired_for` (1565-1568),
    `struct ClockStats` (416-479), `const MAX_CLOCK_GAP_SAMPLES` (521-525),
    `AppState::clock_stats` (387-388) + initialiser (2214).
 3. Remove the calls at 1170, 1578, 1579, 1659, 1669, 2387, 2389, 2452.
 4. `enum ClockSite` stays, but `#[allow(dead_code)]` on `AboutToWait` (509-513) may now be
    removable or may need to stay depending on whether you keep all four variants. `AboutToWait`
    is still *constructed* at 2431, so it is not dead; the `#[allow]` was only there because the
    variant was never constructed at the time.

**Step 4 — `WALLPASS` / `WALLQUEUE` / `WALLSPLIT` / `PASSEVENT`.**
Delete `note_render_pass` (588-773) — which takes `LOG_PASS` and `PASS_EVENT_MS` with it —
`struct PassStats` (530-571), `struct PassSplit` (573-586), `AppState::pass_stats` (295-303)
and its initialiser (2171). Then strip `render_all_tiles` (1668-1843) down:
remove `pass_start`, `tile_ms`, `tile_queued_ms`, `split`, `skipped`, `dispatched_indices`,
every `*_start`/`elapsed()` pair, and the final `note_render_pass(..)` call (1834-1840).
Keep: `dispatched: Vec<bool>` (1705), both `paint_target_keep_previous_logical_frame` barrier
checks with their `continue`s (1722-1728, 1751-1757), `dispatch_paint_targets` (1734),
`make_current`/`paint_target`/`paint`/`present`, `maybe_capture` (1791),
`in_flight.join()` (as `let _ = ..`), and `flush_deferred_dcomp_commits()` (1832).
The `rotate`/`offset`/`pass_counter` block survives this step (see UNCERTAIN).

**Step 5 — the tick-issue audit.**
Delete `struct TickIssueAudit` + `impl` (465-503), `AppState::tick_issue_audit` (373-375) +
initialiser (2157), the `debug_assert!` block (1650-1657), and the two unit tests (2624-2640).
Change 1650 to `let _ = webview.on_wall_tick();` — **the call itself must stay**, it is what issues
the wall frame.

**Step 6 — the shared debug flag (cross-subsystem; coordinate).**
After step 4 winit_wall no longer references `servo_config::debug_env::LOG_PRESENT_CADENCE`.
`components/paint/painter.rs:85` still does, so `components/config/debug_env.rs:163-166,377`
must stay for now and `config_surface::every_registered_flag_is_actually_read_somewhere`
still passes. **If the paint-side strip removes the last reader, that flag, its `ALL` entry, and
the launcher's `-PresentCadence` switch (`etc/multigpu/run_wall_dist.ps1:609,925`) must be deleted
in the same commit** or that test fails. Note `config_surface.rs:289-293`
(`the_dead_knob_check_actually_catches_an_unread_flag`) uses `LOG_PRESENT_CADENCE` *as a literal
example of an unread flag*; deleting the constant breaks that test too and it must be rewritten
to use another flag.

**Step 7 — out-of-tree consumers (not a build break, but the work is not done without it).**
`etc/multigpu/run_wall_dist.ps1` parses `WALLSPLIT` (1764-1819), `WALLQUEUE` (1821-1860) and
references `WALLPASS`/`WALLCLOCK`/`MAINBUSY` in its analysis prose and `-PresentCadence` warning
(1818). Those sections will silently report nothing. Also `components/config/prefs.rs` doc comments
(455, 490, 550, 655) and several `components/paint/*` doc comments cite these tags; they become
stale references. None of this blocks compilation.

**Cargo features note.** `#[cfg(target_os = "windows")]` guards `vsync_refresh_driver`,
`split_plan_for_own_thread` and the spike. None of the removals above are inside a `cfg`, so there is
no asymmetric-compile risk, but re-check `#[allow(dead_code)]` attributes after each step —
several were added to silence exactly the warnings this strip will start producing elsewhere.

---

## Traps

* **`spawn_pacing_thread` (1325-1393) / `pace_on_vblank` (1404-1457) / `align_after_vblank`
  (1459-1484) / `snap_to_dwm_grid_at` (1226-1294) are the frame clock.** They are dense with
  measurement *prose* (dup counts, jitter, phase percentages, log references) and contain not a
  single sample write. Nothing inside them is removable. Specifically: `snap_to_dwm_grid_at`'s
  `DwmGetCompositionTimingInfo` call at 1235 feeds `next_present_tick`, and
  `pace_on_vblank`'s `WaitForVBlank` at 1418 is what wakes the pacing thread.
* **`AppState::last_pacing_tick_at` is written inside `note_tick_site`.** It is the only
  instrumentation-shaped function in this file with a load-bearing side effect. Deleting
  `note_tick_site` wholesale makes `pacing_is_driving()` permanently false, which re-opens the
  engine-wake backstop on every wake — the exact beat-against-the-engine regression documented at
  main.rs:2330-2358.
* **Two different `note_present_tick`.** `AppState::note_present_tick` (868) is a counter and goes.
  The free function `servo::note_present_tick()` called at 1613 clocks the wall-tick edge in
  `paint::wall_tick` and is what makes `commit_deadline()` (and therefore
  `gfx_present_align_commit_pct`) work at all. Deleting it makes every tile fall back to an
  immediate commit — a silent judder regression with no compile error.
* **`servo::note_frame_time(..)` at 1609 is the page's rAF timestamp**, not a probe.
* **`webview.on_wall_tick()` at 1650 looks like it exists for the audit** because its return value
  is only read by a `debug_assert!`. The call produces the wall frame; only the value is dead.
* **`in_flight.join()` at 1823 looks like a timing wrapper.** The `join_start`/`join_ms` around it
  are instrumentation, but the join must happen before `flush_deferred_dcomp_commits()`.
* **`dispatched: Vec<bool>` (1705) vs `dispatched_indices` (1710).** Near-identical names; the first
  controls which tiles the second loop skips, the second only maps join timings into the stats array.
* **`skipped` bumps sit next to the barrier `continue`s** (1726, 1754). The counter goes, the
  `continue` stays.
* **`notify_new_frame_ready` with an empty body (1868-1878) is deliberate.** It is not a stub left
  over from instrumentation; filling it in or deleting the impl restores content-driven presentation.
* **`vsync_stalled` / `vsync_tick_pending` / `vsync_tick_at` / `vsync_due` / `vsync_armed` are
  clock state**, not counters, even though `WALLCLOCK backstop` reports on them.
* **`tick_refresh_driver.rs`'s `Mutex` is a callback queue, not a sample buffer.** It is the one
  `Mutex` a grep for "Mutex in a winit_wall module" will surface first.
* **`--capture` is correctness validation, not performance measurement.** It writes a PNG and exits.
