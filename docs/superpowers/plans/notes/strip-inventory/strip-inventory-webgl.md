# Strip inventory — `components/webgl/`

Repo: `W:\servo_multigpu-tiled-wall`, branch `wall-strip-instrumentation`.
Scope: the whole `servo-webgl` crate (`components/webgl/`), every file.
Method: every line range below was opened and read; every symbol was grepped across
`components/`, `ports/`, `tests/` and the repo root with ripgrep (no `head` truncation).
Authorship checked with `git blame` per line.

## Crate-wide survey result

| file | lines | finding |
|---|---|---|
| `components/webgl/lib.rs` | 13 | upstream, untouched — nothing to strip |
| `components/webgl/webgl_mode/mod.rs` | 7 | upstream, untouched |
| `components/webgl/webgl_mode/inprocess.rs` | 65 | upstream shape; only fork change is the `painter_surfman_details_map` plumbing (real). No instrumentation. |
| `components/webgl/webgl_limits.rs` | 261 | upstream. Zero `warn!`/`info!`/`Instant`/statics. Nothing to strip. |
| `components/webgl/webxr.rs` | 314 | upstream. Zero `warn!`/`info!`/`Instant`/statics. Nothing to strip. |
| `components/webgl/Cargo.toml` | 46 | no instrumentation-only dependency. **Do not drop any dep**: `postcard` is the fan-out clone path, `parking_lot`/`rustc-hash` are data structures. |
| `components/webgl/webgl_thread.rs` | 4541 | **all** instrumentation in this crate lives here |

Everything below is therefore in `components/webgl/webgl_thread.rs` unless stated.

---

## ★CRITICAL★ — true reader list for `debug_env::WEBGL_FANOUT_PROF`

The other inventory is **wrong**. `pub const WEBGL_FANOUT_PROF` is declared at
`components/config/debug_env.rs:258-269` and has **two** `debug_env::`-qualified readers
plus **one** registry reference. Exhaustive ripgrep of the whole repo for both
`WEBGL_FANOUT_PROF` and `SERVO_WEBGL_FANOUT_PROF`:

| # | site | what it does |
|---|---|---|
| 1 | `components/paint/webrender_external_images.rs:37-40` | `static WEBGL_FANOUT_PROF: LazyLock<bool>` — the **consumer-side** gate (`WEBGLEXTIMG`). Read at :134, :140, :148, :186, :335, :407, :452, :465. |
| 2 | **`components/webgl/webgl_thread.rs:336-339`** | `static WEBGL_FANOUT_PROF: LazyLock<bool>` — the **producer-side** gate (`WEBGLFANOUT` + the watchdog). Read at :545, :873, :1079, :1114, :1773, :1813, :1839, :1860, :1901, :1913, :1998, :2024. **This is the reader the other inventory missed.** |
| 3 | `components/config/debug_env.rs:385` | `&WEBGL_FANOUT_PROF` inside `pub const ALL: &[&DebugFlag]`, at array index 17, matching its own `cache_index: 17`. |

Non-code references (documentation / launcher, no build impact, but they go stale):
`etc/multigpu/run_wall_dist.ps1:444` (comment) and `:919` (`if ($FanoutProf) { $env:SERVO_WEBGL_FANOUT_PROF = "1" }`),
`components/config/prefs.rs:391` and `:407` (doc comments on `gfx_webgl_swap_sync` / staging prefs),
`docs/multigpu/configuration.md:308`.

**No reader in `ports/` at all.**

Consequences:
* Deleting the `debug_env.rs` const while `components/paint/webrender_external_images.rs`
  still reads it is a **hard build break**. The config const can only go after *both*
  crates are stripped.
* `debug_env::ALL` is index-ordered and a `const _: ()` assertion at
  `components/config/debug_env.rs:393+` fails compilation if any `cache_index` stops
  matching its array position. Removing entry 17 forces renumbering
  `DCOMP_BIND_PROF`(18) … `WALL_NO_PROBE`(24) down by one. That is config-crate work,
  not webgl work — flagging it so the config owner is not surprised.
* Stripping *only* this crate is safe and self-contained: the paint-side static is a
  separate `LazyLock` with the same name in a different module.

---

## REMOVE — 31 items

Deleting changes no pixel and no timing decision. All are `NuriconRnD` fork additions
(blame-verified; dates 2026-09-01 … 2026-09-09).

| # | symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|---|
| 1 | `WEBGL_PROGRESS` | `webgl_thread.rs:227` (doc 220-226) | `static AtomicU64` | REMOVE | **Readers:** `spawn_webgl_watchdog` :275 (diffs against `last_progress` to compute `ticks_stuck`, printed). **Writers:** :550, :565, :1785, :1812, :1829 (`fetch_add(1)`). Nothing else in the repo names it. | Every reader is itself removable (#12). |
| 2 | `WEBGL_PHASE` | `webgl_thread.rs:228` | `static AtomicUsize` | REMOVE | **Readers:** `spawn_webgl_watchdog` :290 → `phase_name(...)` → printed. **Writers:** :549, :564, :1784, :1811, :1828. No other repo reference. | — |
| 3 | `WEBGL_MSG_CMDS` | `webgl_thread.rs:239` (doc 230-238) | `static AtomicU64` | REMOVE | **Reader:** :276, printed as `cmds=+N`. **Writer:** :567 via the `match &msg` select at :566-571. | — |
| 4 | `WEBGL_MSG_SWAPS` | `webgl_thread.rs:240` | `static AtomicU64` | REMOVE | **Reader:** :277, printed as `swaps=+N`. **Writer:** :568. | — |
| 5 | `WEBGL_MSG_OTHER` | `webgl_thread.rs:241` | `static AtomicU64` | REMOVE | **Reader:** :278, printed as `other=+N`. **Writer:** :569. | — |
| 6 | `PHASE_IDLE` | `webgl_thread.rs:243` | `const usize` | REMOVE | Used as `AtomicUsize::new(PHASE_IDLE)` at :228, stored at :549, matched at :251. All removable. | — |
| 7 | `PHASE_HANDLING` | `webgl_thread.rs:244` | `const usize` | REMOVE | Stored :564; matched :252. | — |
| 8 | `PHASE_SWAP_LOCK` | `webgl_thread.rs:245` | `const usize` | REMOVE | Stored :1784; matched :253. | — |
| 9 | `PHASE_SWAP_BUFFERS` | `webgl_thread.rs:246` | `const usize` | REMOVE | Stored :1811; matched :254. | — |
| 10 | `PHASE_SWAP_AFTER` | `webgl_thread.rs:247` | `const usize` | REMOVE | Stored :1828; matched :255. | — |
| 11 | `phase_name` | `webgl_thread.rs:249-258` | free `fn` | REMOVE | **One caller:** :290, inside the `WEBGLWATCHDOG` `warn!`. | — |
| 12 | `spawn_webgl_watchdog` | `webgl_thread.rs:266-301` (doc 260-265) | free `fn`, spawns a 1 Hz thread | REMOVE | **One caller:** `WebGLThread::process` :546, guarded by `if *WEBGL_FANOUT_PROF` at :545. Body only sleeps, loads the five atomics, and emits `WEBGLWATCHDOG`. | Thread is never joined and holds no resource; dropping the spawn is inert. |
| 13 | `WEBGL_FANOUT_PROF` (crate-local) | `webgl_thread.rs:336-339` (doc 335) | `static LazyLock<bool>` | REMOVE | **12 readers, all in this file, all instrumentation:** :545 (spawn watchdog), :873 (`commands += 1`), :1079, :1114, :1773, :1813, :1839, :1860, :1901, :1913 (early-return guard), :1998, :2024. **No reader outside this file** — the paint crate has its own identically named static. | Do **not** touch `components/config/debug_env.rs` from this crate's strip — see the CRITICAL section. |
| 14 | `FanoutProfile` | `webgl_thread.rs:347-380` (doc 341-346) | `struct`, 14 counters | REMOVE | Constructed at :525 and :1961; fields read only inside `maybe_emit_fanout_profile` :1928-1959. No `pub`, no other module. | Fields: `window_start, swaps, commands, applies, switches, lock_wait_ns, switch_ns, apply_ns, serialize_ns, swap_sync_ns, swap_syncs, swap_ns, clear_ns` (13 named + the derived `busy_ns` local). |
| 15 | `WebGLThread::fanout_profile` | field `webgl_thread.rs:429-430`; init `:525` | struct field | REMOVE | Mutated at :874, :1082, :1118, :1136-1137, :1774, :1826, :1844, :1902-1903, :2011-2012, :2037-2038. Read only at :1917, :1928-1959, :1961. | — |
| 16 | `WebGLThread::maybe_emit_fanout_profile` | `webgl_thread.rs:1912-1965` (doc 1908-1911) | method | REMOVE | **One caller:** `handle_swap_buffers` :1905, unconditional (the body early-returns on the gate). Body computes `devices`, `busy_ns`, emits `WEBGLFANOUT`, resets the profile. | Pure read + `warn!` + reset. |
| 17 | `WebGLThread::log_live_contexts` | `webgl_thread.rs:751-777` (its doc is **737-750**, see trap T1) | method | REMOVE | **One caller:** `handle_msg` :582, **ungated — this does run in production.** Body: reads `self.contexts` (counts `marked_for_deletion`), takes a *read* lock on `busy_webgl_context_map`, reads `context_backends.len()` and `cached_context_info.len()`, emits `WEBGLLIVE`. | **Confirmed: no prune side effect.** The only mutation in the whole body is `self.last_live_log = Some(now)` at :759. It never removes a context, never touches `released_contexts`/`context_recipes`, never calls `remove_webgl_context`. Deletion is safe; it also removes one `RwLock::read` per second from the message loop. |
| 18 | `WebGLThread::last_live_log` | field `webgl_thread.rs:427-428`; init `:524` | struct field | REMOVE | Only reader/writer is `log_live_contexts` :753-759. | — |
| 19 | watchdog bookkeeping in `process()` | `webgl_thread.rs:545-547`, `:549-550`, `:564-565`, `:566-571` | statement blocks | REMOVE | Pure stores into the removed atomics plus the watchdog spawn. | **T3:** the `match &msg {...}.fetch_add(1, ...)` at :566-571 is an *expression statement* — it only selects a static and bumps it. Deleting it does not consume `msg`; `msg` is still moved into `handle_msg` at :572. |
| 20 | commands counter | `webgl_thread.rs:873-875` | `if *WEBGL_FANOUT_PROF { self.fanout_profile.commands += 1; }` | REMOVE | Value read only by `maybe_emit_fanout_profile` :1948. | Sits between the `cached_context_info` early-return (:870-872, **real**) and the `match command` (:876, **real**). |
| 21 | serialize stopwatch | `webgl_thread.rs:1079`, `:1081-1083` inside `clone_command_for_backend` (fn 1078-1085) | `Instant` pair | REMOVE | Feeds `fanout_profile.serialize_ns`, read at :1954. | **T4:** `let cloned = clone_webgl_command(command);` at :1080 and `cloned` at :1084 are the real body — keep them. After the strip the method is a one-line wrapper; leaving the wrapper (with its now-pointless `&mut self`) is the zero-risk option, inlining it into :1098 is optional. |
| 22 | lock-wait / apply stopwatch | `webgl_thread.rs:1114`, `:1116-1120`, `:1135-1138` inside `apply_webgl_command_to_surface` (fn 1108-1139) | `Instant` pairs | REMOVE | Feed `lock_wait_ns`, `applies`, `apply_ns`; read at :1951, :1949, :1953. | **T2 — highest-risk deletion in the crate. Line 1115 `let _angle_gl_guard = paint_api::angle_gl_lock(self.angle_device_key(surface_id));` is REAL and sits *between* the two stopwatch statements.** A range delete of 1114-1120 takes the ANGLE lock with it. |
| 23 | swap counter + sync accumulators | `webgl_thread.rs:1773-1775`, `:1776-1779`, `:1868-1871`, `:1901-1904` | counters / locals | REMOVE | `sync_ns`/`syncs` locals (:1778-1779) are accumulated at :1869-1870 and folded into the profile at :1902-1903; read at :1955-1956. `swaps` read at :1945. | **T5:** the `if let Some(start) = start { sync_ns += …; syncs += 1; }` block at :1868-1871 is *inside* the `if sync != SwapSync::Off` body. The `data.gl.flush()` / `data.gl.finish()` at :1861-1867 is the `gfx_webgl_swap_sync` pixel-path switch — **KEEP**. Only the stopwatch goes. |
| 24 | swap / clear stopwatches | `webgl_thread.rs:1813`, `:1825-1827`, `:1839`, `:1843-1845` | `Instant` pairs | REMOVE | Feed `swap_ns` / `clear_ns`; read at :1957-1958. | **T6:** `swap_chain.swap_buffers(...)` (:1814-1824) and `swap_chain.clear_surface(...)` (:1840-1842) are the real swap path. The stopwatch brackets them. Delete only :1813, :1825-1827, :1839, :1843-1845. |
| 25 | phase stores in `handle_swap_buffers` | `webgl_thread.rs:1784-1785`, `:1811-1812`, `:1828-1829` | statements | REMOVE | Stores into removed atomics. | :1786 (`angle_gl_lock`) and :1787 (`make_surface_current_if_needed(...).expect(...)`) directly follow :1785 and are **real**. |
| 26 | `maybe_emit_fanout_profile()` call | `webgl_thread.rs:1905` | call | REMOVE | Last statement of `handle_swap_buffers`. | — |
| 27 | switch stopwatch (shared) | `webgl_thread.rs:1997-2001`, `:2010-2013` inside `make_surface_current_if_needed` (fn 1993-2016) | `Instant` pair | REMOVE | Feeds `switches`/`switch_ns`; read at :1950, :1952. | **T7:** :2003-2009 is the real context switch (`make_context_current` + `self.bound_context_id = Some(...)`), and :2015 `data` is the return value. The stopwatch predicate at :1998-2000 duplicates the real switch condition — removing it removes that duplication hazard too. |
| 28 | switch stopwatch (shared, mut) | `webgl_thread.rs:2022-2027`, `:2036-2039` inside `make_surface_current_if_needed_mut` (fn 2018-2042) | `Instant` pair | REMOVE | Same two fields. | Same shape as #27; :2029-2035 real, :2041 returns `data`. The disjoint-field borrow (`self.contexts` vs `self.fanout_profile`) that this relies on disappears with it — no borrow regression possible in the removal direction. |
| 29 | VRAM-delta block + `WEBGLRECLAIM released` | `webgl_thread.rs:1590`, `:1591-1594` (comment), `:1595-1609`, `:1631-1651` inside `release_context_backends` (fn 1572-1655) | locals + `warn!` | REMOVE | `released` (:1590) → only :1651. `before` (:1595-1602) → only :1632. `devices` (:1603-1609) → only :1635. `mb` (:1631) → only :1644-1645. `deltas` (:1632-1650) → only :1651. | **T8:** this removes the only two callers of `paint_api::rendering_context::adapter_local_usage_bytes` (:1599, :1639). Repo-wide that function has exactly three references: its definition at `components/shared/paint/rendering_context.rs:17` and these two. After this strip it is **dead but still `pub`**, so no build break and no `dead_code` warning — the paint/shared-paint owner should be told it can go. **T9:** :1610-1630 (the destroy loop, `context_backends.remove`, `bound_context_id = None`, `released_contexts.insert`) and :1652-1654 (the `Lost` notifier) are the real VRAM reclaim — **KEEP**. |
| 30 | `WEBGLRECLAIM restored` warn | `webgl_thread.rs:1688-1691` inside `restore_context_backends` (fn 1660-1695) | `warn!` | REMOVE | Prints `recipe.painters.len()`. Nothing reads it. | :1685-1687 (re-insert backends, clear `released_contexts`) and :1692-1694 (`Restored` notifier) are real — **KEEP**. |
| 31 | now-unused imports | `webgl_thread.rs:9`, `:49` | `use` lines | REMOVE | Line 9 `use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};` — verified: *every* `Atomic*`/`Ordering` occurrence in the file (21 sites) belongs to items #1-#5 and #19/#25. Line 49 `use servo_config::debug_env;` — verified: the only use is :337 (item #13). | **Keep line 10** `use std::sync::{Arc, LazyLock};` — `Arc` is used at :75 (`WebGLContextBusyMap`), `LazyLock` at :322 (`WEBGL_SWAP_SYNC`, KEEP). **Keep line 11** `use std::time::{Duration, Instant};` — both survive (`last_composited`, `last_reclaim_sweep`, `recv_timeout`). **Keep line 12** `use std::{slice, thread};` — `thread` is `run_on_own_thread` :534. |

---

## KEEP — 18 items

| # | symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|---|
| 1 | `SwapSync` enum | `webgl_thread.rs:312-320` (doc 303-311) | `enum` | KEEP | Produced by `WEBGL_SWAP_SYNC`; matched at :1858-1866. | Pixel/timing path. |
| 2 | `WEBGL_SWAP_SYNC` | `webgl_thread.rs:322-333` | `static LazyLock<SwapSync>` | KEEP | **One reader:** :1858 `let sync = *WEBGL_SWAP_SYNC;` → drives `gl.flush()`/`gl.finish()`. | Backed by pref `gfx_webgl_swap_sync` (`components/config/prefs.rs:392`, default `""` = off). Changes GPU completion timing — a timing decision, not a number. |
| 3 | `gl.flush()` / `gl.finish()` | `webgl_thread.rs:1858-1872` minus :1860, :1868-1871 | statements | KEEP | — | Only the surrounding `Instant` pair is instrumentation. |
| 4 | `maybe_reclaim_idle_contexts` | `webgl_thread.rs:1506-1537` (doc 1502-1505) | method | KEEP | **Two callers:** `process()` :558 (on the 500 ms `recv_timeout` path) and :563 (after every message). Calls `release_context_backends` on stale contexts. | Real VRAM reclaim. Reads pref `dom_webgl_idle_context_reclaim_ms` (`prefs.rs:975`, default `3000`). |
| 5 | `last_reclaim_sweep` | field `:405` (doc 404), init `:514`, used `:1512-1518` | field | KEEP | Rate-limits the reclaim sweep to 1/s. | Looks like a log throttle; it is not — it throttles a **destructive** sweep. |
| 6 | `released_contexts` | field `:399` (doc 398), init `:513` | field | KEEP | Read :1525, :1573, :1661; written :663, :1630, :1687. | Reclaim state. |
| 7 | `context_recipes` | field `:397` (doc 393-396), init `:512` | field | KEEP | Read :1664; written :662 and at context creation. | Needed to rebuild a released context. |
| 8 | `last_composited` | field `:403` (doc 400-402), init `:513` | field | KEEP | Written :1540-1541 (`handle_finished_rendering_to_context`); read :1527. | Drives the idle judgement. |
| 9 | `release_context_backends` | `:1572-1655` minus items R29 | method | KEEP | Callers: :671 (`ReleaseIdleContext`), :1535 (sweep). | Body destroys swap chains and GL contexts. |
| 10 | `restore_context_backends` | `:1660-1695` minus item R30 | method | KEEP | Caller: :674 (`RestoreContext`). | — |
| 11 | `handle_finished_rendering_to_context` | `:1539-1549` | method | KEEP | Caller: the `FinishedRenderingToContext` arm in `handle_msg`. | Also completes deferred deletion. |
| 12 | `context_has_busy_surfaces` | `:1551-1560` | method | KEEP | Callers :1546, :1580, :1699. | Guards destruction against in-flight WebRender use. |
| 13 | `busy_webgl_context_map` | field `:426`, type `:75` | field | KEEP | Read :766 (removable use), :1552, :1585. Also `pub` in `WebGLThreadInit` :445 and plumbed from `webgl_mode/inprocess.rs`. | Item R17 removes one *reader*, not the map. |
| 14 | `clone_webgl_command` | `:473-489` (doc 453-472) | free `fn` | KEEP | Callers: :1080 and the test module. | Correctness-critical: the serialize/deserialize round trip balances one `Arc` strong reference. Deleting it reintroduces the 2026-09-07 `memcpy` crash. |
| 15 | `mod fanout_clone_tests` | `:4443-4520` approx (`#[cfg(test)]` at 4443) | tests | KEEP | Tests `clone_webgl_command` reference counting. | Not performance instrumentation. |
| 16 | fan-out drop `warn!` | `:1212-1221` (comment 1213-1217, `warn!` 1218-1220) | `warn!` | KEEP | Fires once per dropped painter during `create_webgl_context`. | A **correctness** diagnostic ("that tile will render without this canvas"), not a counter. Same class as the serialize-failure warns at :476 and :485. |
| 17 | `_sent_time` param | `:1770` | fn param | KEEP | **upstream** — blame: `Manish Goregaokar 2020-02-19`, file `components/canvas/webgl_thread.rs`. Already `_`-prefixed. Comes from `WebGLMsg::SwapBuffers(Vec<WebGLContextId>, Option<Epoch>, u64)` at `components/shared/canvas/webgl.rs:158`. | Changing the message shape is a canvas-traits change, out of this crate. |
| 18 | `angle_device_key` + every `let _angle_gl_guard = paint_api::angle_gl_lock(...)` | `:779-789`; guards at :1115, :1146, :1611, :1786 | method / RAII guards | KEEP | Serializes WebGL work against tile rendering on the same ANGLE D3D11 device. | Dropping a guard is a 0xc0000005 class failure. Called out because guard sites sit adjacent to three separate stopwatch deletions. |

Everything else in `webgl_thread.rs` is upstream Servo: KEEP, reason **upstream**.

---

## UNCERTAIN — 2 items

| symbol | file:line-range | kind | why uncertain |
|---|---|---|---|
| `WEBGLTARGETS` warn | `webgl_thread.rs:596-602` (comment 592-595), in the `CreateContext` arm | `warn!` | It prints `target_painter_ids.len()` and the list — literally "a number being printed", which the REMOVE rule catches. But it is **not** a performance or statistics sampler: it fires once per context creation, costs nothing on any hot path, and its own comment records that its absence caused two misdiagnoses on 2026-09-03 (`log_webgpu/64`-`66`). It is also **duplicated** by the `info!` below. I cannot establish from the code whether "instrumentation" in this strip means perf samplers only or all fork-added logging — that is the user's call. Zero risk either way; it reads nothing that a rendering decision uses. |
| fan-out `info!` | `webgl_thread.rs:1227-1233` (`if target_painter_ids.len() > 1`) | `info!` | Same content as `WEBGLTARGETS` (logical context id, primary painter, target painters). Blame: `일우 남 2026-06-11` — fork, not upstream, but predates the 2026-09 instrumentation push by three months, so `git log --follow` does **not** group it with the probe commits. If exactly one of this and `WEBGLTARGETS` is kept, this is the cheaper one (`info!`, and the launcher's `RUST_LOG` has no `webgl` target so it is invisible in practice). |

---

## Removal order and build-breakage notes

Do it in this order; the tree compiles after every step.

1. **Leaf call sites first, statics last.** Remove, in one pass over `webgl_thread.rs`,
   the statement-level items R19-R30. At this point `WEBGL_PROGRESS`, `WEBGL_PHASE`,
   the three `WEBGL_MSG_*`, `fanout_profile` and `last_live_log` have no writers left.
2. **Then the method bodies:** R16 `maybe_emit_fanout_profile`, R17 `log_live_contexts`
   (range **737-777**, see T1), R12 `spawn_webgl_watchdog`, R11 `phase_name`.
3. **Then the struct fields:** R15 and R18 — delete `webgl_thread.rs:427-430` as one block
   (both fields plus both doc lines) and `:524-525` as one block (both initialisers).
   Line 426 (`busy_webgl_context_map`) and line 432 (`#[cfg(feature = "webxr")]`) are the
   neighbours; line 523 and line 526 are the ctor neighbours.
4. **Then the type and the statics:** R14 `FanoutProfile` (delete **341-380**, doc +
   `#[derive(Default)]` + struct, bounded by blank line 340 and blank line 381),
   R13 `WEBGL_FANOUT_PROF` (delete **335-339**), and the whole watchdog block
   R1-R10 (delete **220-301** — one contiguous range: blank line 219 above,
   blank line 302 below, and line 303 begins the `SwapSync` doc which is KEEP).
5. **Then the imports:** R31 — delete lines 9 and 49.
6. **Build this crate alone first:** `cargo check -p servo-webgl --features no-wgl`.
   Then the workspace. The crate has no `[lints]` section and the workspace `Cargo.toml`
   has no `[workspace.lints]`, so a missed unused import is a warning, not an error —
   which means a leftover import will **not** be caught by a successful build. Read the
   warning list.
7. **Do not touch `components/config/debug_env.rs` as part of this crate's strip.**
   The paint crate still reads `WEBGL_FANOUT_PROF`. When both crates are stripped,
   removing the const also requires renumbering `cache_index` 18-24 and the matching
   `ALL` entries, or the `const _: ()` assertion at `debug_env.rs:393+` fails to compile.
8. **Launcher/doc follow-ups (no build impact):** `etc/multigpu/run_wall_dist.ps1:444`
   and `:919` (`-FanoutProf` switch), `docs/multigpu/configuration.md:308`,
   `components/config/prefs.rs:391` and `:407` (doc comments that point at
   `swap_sync_ms` / `stage_ms` fields of a line that will no longer exist).

### Cross-crate consequences created by this strip

* `paint_api::rendering_context::adapter_local_usage_bytes`
  (`components/shared/paint/rendering_context.rs:17`) loses **both** of its callers
  (R29). It is `pub`, so nothing breaks and nothing warns — it just becomes dead code
  that no grep for "instrumentation" in the paint area will flag. Tell the paint owner.
* `WebGLMsg::SwapBuffers`'s third field (`u64` sent-time) stays unread in this crate; it
  was already unread before the strip (upstream `_sent_time`). No change.
* Nothing in `components/script/`, `components/paint/` or `ports/` calls any symbol in
  the REMOVE table. Verified by name-by-name ripgrep; the only cross-crate *mentions* of
  `WEBGLLIVE` are prose in doc comments at
  `components/script/dom/webgl/webglrenderingcontext.rs:189`, `:204`, `:655` and
  `components/script/script_thread.rs:1318` — comments only, no code dependency, but they
  will reference a log line that no longer exists.

---

## Traps

**T1 — the misattached doc comment (this is the "overlapping range" failure the brief warned about).**
Lines **731-736** are a doc comment describing `angle_device_key` (it talks about the
ANGLE D3D11 device pointer and `paint_api::angle_gl_lock`), but they are physically
attached to `fn log_live_contexts` at :751 because `log_live_contexts`'s own doc
(**737-750**) follows them with no separator. The correct deletion is **737-777**, which
leaves 731-736 sitting in front of `fn angle_device_key` at :779 — where it belonged all
along, and where it still compiles. Deleting 731-777 silently destroys the
`angle_gl_lock` key documentation. Deleting 751-777 only is also fine but leaves the
`WEBGLLIVE` doc text behind, attached to the wrong function.

**T2 — the ANGLE lock inside the stopwatch (worst case in the crate).**
`apply_webgl_command_to_surface`:
```
1114  let lock_start = WEBGL_FANOUT_PROF.then(Instant::now);          <- delete
1115  let _angle_gl_guard = paint_api::angle_gl_lock(...);            <- KEEP (RAII, must live to fn end)
1116  let apply_start = lock_start.map(|start| { ... });              <- delete 1116-1120
```
A range delete of 1114-1120 drops the lock. The WebGL thread then races tile rendering on
the same ANGLE device — the documented failure mode is an access violation in
`libGLESv2`, not a visible glitch, and it will not reproduce on a single-GPU dev box.

**T3 — the message-kind counter is an expression, not a binding.**
`webgl_thread.rs:566-571` is `match &msg { ... }.fetch_add(1, Ordering::Relaxed);`. It
borrows `msg` and discards the result. It must go, but note it is sandwiched between
`WEBGL_PROGRESS.fetch_add` (:565, also goes) and `let exit = self.handle_msg(msg, ...)`
(:572, stays and *moves* `msg`). Deleting 564-571 as one block is correct.

**T4 — `clone_command_for_backend` collapses to a wrapper.**
After removing :1079 and :1081-1083 the method is `clone_webgl_command(command)` behind a
`&mut self` it no longer needs. Leave it as-is (zero risk) or inline it at :1098 — but do
not delete the method while :1098 still calls it.

**T5 — `gfx_webgl_swap_sync` is a pixel-path switch wrapped in a stopwatch.**
In `handle_swap_buffers` the `if sync != SwapSync::Off { ... }` block at :1859-1872
contains both the real `gl.flush()`/`gl.finish()` (:1861-1867) and the timing
(:1860, :1868-1871). Keep the block and the GL calls; remove only the two `Instant`
fragments and the `sync_ns`/`syncs` locals at :1778-1779.

**T6 — `swap_buffers` and `clear_surface` are bracketed the same way.**
:1813 + :1825-1827 bracket `swap_chain.swap_buffers(...)`; :1839 + :1843-1845 bracket
`swap_chain.clear_surface(...)`. Both GL calls are the actual swap path.

**T7 — the switch stopwatch duplicates the real switch predicate.**
`make_surface_current_if_needed` (:1998-2000) and `_mut` (:2024-2026) each re-evaluate
`Some(surface_id) != self.bound_context_id && self.contexts.contains_key(&surface_id)`
purely to decide whether to start a timer, with a comment telling the reader to keep the
two copies in sync. Removing the instrumentation removes the duplication — good — but do
not let a sed on that predicate touch the real one at :2004-2006 / :2030-2032.

**T8 — removing the WEBGLRECLAIM log strips the last caller of a `pub` paint helper.**
`adapter_local_usage_bytes` goes dead but stays compiling. It will not show up as an
instrumentation symbol in the paint area's own sweep because, from inside paint, it looks
like an ordinary exported API with external callers.

**T9 — `release_context_backends` is mostly real.**
The function is 84 lines; only `released` (:1590), the comment (:1591-1594), `before`
(:1595-1602), `devices` (:1603-1609), `mb`/`deltas` (:1631-1650) and the `warn!` (:1651)
are instrumentation. :1610-1630 destroys swap chains and GL contexts and mutates
`context_backends`/`bound_context_id`/`released_contexts`; :1652-1654 sends
`WebGLContextNotification::Lost`, without which a restored canvas comes back blank
because the page is never told to redraw. The function name contains no "log"/"prof"
marker, so a name-based sweep will not protect it.

**T10 — `log_live_contexts` is ungated and on the message loop, but harmless to remove.**
It is called from `handle_msg` :582 with no env gate, i.e. it runs in production today.
Verified side-effect-free apart from its own 1 s throttle field. Removing it is a small
win (one `RwLock::read` and one `HashMap` scan per second disappear from the WebGL
thread), and no behaviour depends on it. The `Instant`/`Duration` import it uses is still
needed by the reclaim path, so do not remove line 11.

**T11 — do not confuse the two same-named statics.**
`WEBGL_FANOUT_PROF` exists as a crate-local `LazyLock<bool>` in *both*
`components/webgl/webgl_thread.rs:336` and `components/paint/webrender_external_images.rs:37`.
A repo-wide sed on that identifier will silently edit the paint crate too. Scope every
edit to `components/webgl/`.
