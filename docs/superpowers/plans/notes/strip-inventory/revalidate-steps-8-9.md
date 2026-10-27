# Re-validation of Steps 8 and 9 — read-only

Verified against the working tree at `79855e8f0ae` (steps 1-4 committed). Nothing was edited.
`components/paint/painter.rs` and `components/paint/paint.rs` were **not** read (another agent is
mid-edit); everything attributed to them below is marked UNVERIFIED.

Two reference facts established by direct count:

* `components/servo/examples/winit_wall/main.rs` is **2739 lines** — steps 1-4 did not touch it, so
  the winit_wall inventory's line numbers are still live. Where they are wrong, they were wrong
  when written, not stale.
* The SPINSLOW line numbers in `strip-inventory-servo_shared.md` are **all correct** (I checked
  every one). That inventory is trustworthy; `strip-inventory-winit_wall.md` is not.

---

## Step 8

| symbol | plan says | code shows | verdict | correct boundaries |
|---|---|---|---|---|
| `ClockStats` location | "at **433** (not 416)" | `struct ClockStats` at 433; its doc comment opens at 415 | **OK** (the correction is right) | item = **415-470** (doc 415-431, `#[derive]` 432, `struct` 433, `}` 470) |
| "465-479 is inside `ClockStats`'s field list and contains `tick_by_site`" | one claim | 465-470 *is* inside `ClockStats` (`tick_by_site` at **469**, `}` at 470); **471 is blank and 472-479 is `TickIssueAudit`'s doc comment** | **WRONG (half)** — the range straddles two items | `ClockStats` ends at 470; the next item starts at 472 |
| `TickIssueAudit` location | "at **487** (not 465)" | `struct TickIssueAudit` at 487 | **OK** | item = **472-503** (doc 472-485, derive 486, struct 487-489, blank 490, `impl` 491-503) |
| `PassStats` location | "at **538** (not 530)" | `struct PassStats` at 538 | **OK** | item = **536-570** (doc 536, derive 537, struct 538, `}` 570) |
| "530-535 is `MainSlot`'s body" | one claim | `MainSlot` = doc 527, derive 528, `enum` 529, `}` **534**; 535 is blank | **OK in substance**, off by one at the tail | `MainSlot` = **527-534** |
| `report_clock_stats` / `report_main_busy` | "at **955** and **1060**; the inventory's ranges overlap by five lines" | `fn report_clock_stats` 955, closes 1053; `fn report_main_busy` 1060, closes 1083. Inventory's 951-1058 vs 1054-1083 overlap on 1054-1058 = exactly 5 | **OK** | `report_clock_stats` = **951-1053** (doc 951-954); `report_main_busy` = **1055-1083** (doc 1055-1059) |
| `PassSplit` (plan does not correct it) | inventory: 573-586 | doc 572, derive 573, `struct` 574, `}` **585** | **WRONG in the inventory, uncorrected by the plan** | **572-585** |
| `MainBusy` (not corrected) | inventory: 403-414 | doc 403, derive 404, struct 405, `}` **413** | off by one | **403-413** |
| `note_present` (not corrected) | inventory: 775-801 | `fn note_present` 775, `}` **799**; **801-802 is `maybe_capture`'s doc comment (KEEP)** | **WRONG in the inventory, uncorrected** — deleting to 801 eats a kept function's doc | **775-799** |
| `charge_main` (not corrected) | inventory: 839-867 | 839 is `maybe_capture`'s closing `}`; doc 841-844, `fn` 845, `}` **866** | **WRONG in the inventory, uncorrected** — 839 would delete `maybe_capture`'s brace | **841-866** |
| `charge_main` call sites | "unwrap all **four** call sites" | 1660 (`MainSlot::Render`), 2326 (`Spin`), 2444 (`Spin`), 2453 (`Render`) | **OK — 4** | each becomes the bare body; `charge_main` returns `body()` unchanged |
| `AppState::main_busy` + init | "+ init" | field at **386**, doc 376-385, init at **2213** | **OK** for the plan; the inventory's removal-order "380-386" is **WRONG** (the doc starts at 376) | field item **376-386**, init 2213 |
| `note_present` + the `present_ms_*` fields | "the **three** `present_ms_*` fields + inits" | `present_ms_sum` 292, `present_count` 293, `present_window_start` 294, their comment 289-291; inits 2168/2169/2170; calls at 1777 and 1796 | **OK — 3 fields, 3 inits, 2 calls** | fields **289-294**; inits **2168-2170** |
| `note_render_pass` "takes `LOG_PASS` and `PASS_EVENT_MS` with it" | — | `const PASS_EVENT_MS` at **611**, `static LOG_PASS` at **646-650** — both local to that fn | **OK** | `note_render_pass` = **588-773**; sole caller 1834-1840 |
| `AppState::pass_stats` + init | "+ init" | field at **303**, init at **2171** | **OK** for the plan — but the inventory's removal-order "295-303" is a **TRAP**: lines **295-296 are the `--capture` comment** ("read the primary tile's framebuffer to PNG… before `present()`"), documenting `capture_path`/`capture_deadline` at 370-371, which are KEEP | delete **297-303** only; keep 295-296 |
| `render_all_tiles` locals | `pass_start`, `tile_ms`, `tile_queued_ms`, `split`, `skipped`, `dispatched_indices` and every `*_start`/`.elapsed()` pair | `pass_start` 1674; `tile_ms` 1675; `tile_queued_ms` 1678; `split` 1679; `skipped` 1680 (bumps 1726, 1754); `dispatched_indices` 1710 (push 1732, read 1825); `dispatch_start` 1714/1736; `tile_start` 1745 (reads 1764, 1799); `paint_start` 1761/1763, 1770/1772, 1787/1789; `current_start` 1767/1769, 1784/1786; `present_start` 1773/1775, 1792/1794; `join_start` 1822/1824; `flush_start` 1831/1833 | **OK — the list is complete** | `render_all_tiles` = **1668-1843** |
| `dispatched: Vec<bool>` | "Keep `dispatched: Vec<bool>` (not `dispatched_indices`)" | `dispatched` at 1706; writes at **1727** and **1731**; read at 1741 | **OK but under-specified** — see traps: `dispatched[index] = true;` at **1727 sits between `skipped += 1;` (1726) and `continue;` (1728)** | keep 1727 |
| `in_flight.join()` | "`let _ = in_flight.join();`" | 1823 `let tile_times = in_flight.join();`, 1824 `split.join_ms = …`, 1825-1828 the `tile_ms`/`tile_queued_ms` fill, inside `if let Some(in_flight) = in_flight {` (1817) … `}` (1829) | **OK** | replace 1822-1828 with `let _ = in_flight.join();`; keep the `if let` and the 1832 flush after it |
| `TileRenderTiming` sites | five bullets, but the rationale calls them "**four** sites" | outside paint.rs/painter.rs the name appears at exactly **`components/paint/lib.rs:21`** and **`components/servo/lib.rs:48`**. `winit_wall/main.rs` never names the type — it uses `timing.render_ms` / `timing.queued_ms` at 1826-1827 purely by inference | **WRONG (count, the fifth one)** — and `strip-inventory-servo_shared.md:36` says the type is referenced at `paint.rs:**344, 357, 371, 3644**`. **`:371` is outside both of the plan's paint.rs ranges** (346-361, 3636-3647); it is almost certainly `PaintTargetsInFlight::join() -> Vec<TileRenderTiming>`. The plan covers it in prose ("only its payload type becomes `()`") but not in its enumerated list, so a literal execution leaves a dangling type. UNVERIFIED (paint.rs not read). | real sites: paint.rs def + fill + `join()` signature, `paint/lib.rs:21`, `servo/lib.rs:48`, main.rs:1823-1828 = **6**, not 4 |
| `servo.rs` SPINSLOW | symbol list; reshape `let paint_handle_ms = {` → `{` and `};` → `}`; narrow the `use` to `Duration` | all confirmed exactly — see the block below | **OK with one omission** (`embedder_messages += 1;` at :308) | see below |
| `pub const LOG_PRESENT_CADENCE` | "`debug_env.rs:163-170`" | `pub const` 163, `};` **170** | **OK** | **163-170**. The section comment at 157-161 covers the following DCOMP flags too — keep it, but its "넷 다" wording goes stale |
| `ALL` entry + `cache_index` renumber | "its `&LOG_PRESENT_CADENCE` entry in `ALL`, renumbers the surviving `cache_index` values" | `ALL` has **25** entries (`cache_index` 0..24); `&LOG_PRESENT_CADENCE` is index **9** at `debug_env.rs:377` → **15** constants (ALL-positions 10..24) must be decremented | **OK, but one trap is unstated**: the constants are **not declared in `ALL` order** — `WALL_NO_PROBE` at `debug_env.rs:275` carries `cache_index: 24` but is declared *between* index 17 (`:262`) and index 18 (`:282`). A source-order renumber corrupts it. The `const _: ()` at 398-405 turns that into a compile error, as the plan says. | renumber by walking `ALL`, never the file |
| `config_surface.rs` test | "rewrites `config_surface.rs:271-295`" | `#[test]` **270**, `fn the_dead_knob_check_actually_catches_an_unread_flag` 271, `}` **294**; 295 is blank | minor off-by-one | **270-294** |
| which consts the test hard-codes | the inventory says `DCOMP_READBACK (:276, :288)`, `D3D11_PROFILE_MS (:277)` and `LOG_PRESENT_CADENCE (:289)` are "a COMPILE break the moment those three consts are deleted" | only **:288** and **:289** are real const references. `:276`/`:277` live inside the raw-string fixture `let text = r#"…"#;`, and `:280`/`:281` are string literals in `assert!(referenced.contains("…"))` | **WRONG (inventory)** — the compile-coupled set is **2** consts, not 3. Deleting `D3D11_PROFILE_MS` in Step 14 breaks nothing here. | compile-coupled: `:288` `DCOMP_READBACK`, `:289` `LOG_PRESENT_CADENCE` |
| the proposed rewrite | `DCOMP_NO_PARTIAL_PRESENT` (referenced) + `DCOMP_DISABLE_RESIZE_REBUILD` (not), "Step 14 then only removes `ALL` entries, not this test again" | both flags survive Step 14's delete list. `config_surface.rs` is listed in `REGISTRY_FILES` (`:166-170`) and excluded from `scan_sources`, so the self-reference cannot mask a dead flag. The file holds exactly **20** `#[test]`s, so the "20/20" figure is right. | **OK**, with one residue: the rewrite must also swap `DCOMP_READBACK` at **:276 and :288** (Step 14 deletes that const), and `D3D11_PROFILE_MS` at `:277`/`:281` is left naming a flag that will no longer exist — it still compiles and passes, but it is stale | — |
| `configuration.md` edit | "deletes its `###` entry in `docs/multigpu/configuration.md`" | the `###` entry is **249-252** (heading 249, blank 250, body 251, blank 252) | **OK but incomplete** | also **`configuration.md:328`** — "painter 쪽 `ANGLE_GL_LOCK` 대기는 `SERVO_LOG_PRESENT_CADENCE` 의 `angle_lock_ms` 로 본다." — a second prose reference the plan never names. (The counts at `:1` and `:201` are already stale; Step 14 owns them.) |

### `components/servo/servo.rs` SPINSLOW — exact boundaries; nothing load-bearing interleaved

Read line by line. Everything is inside `fn spin_event_loop` (`fn` at **263**).

* **268-272** rationale comment 1 → delete
* **273** `let spin_start = Instant::now();` → delete
* **274** `let mut paint_messages = 0usize;` → delete
* **275** `let mut embedder_messages = 0usize;` → delete
* **276** `let paint_handle_ms = {` → reshape to `{`
* **277-286** the `try_recv` drain → **KEEP**
* **287** `paint_messages = messages.len();` → delete
* **288** `let start = Instant::now();` → delete
* **289** `paint.handle_messages(messages);` → **KEEP**
* **290** `start.elapsed().as_secs_f64() * 1000.0` → delete (the block's tail expression; afterwards the block evaluates to `()`)
* **291** `};` → reshape to `}`
* **292** `let embedder_start = Instant::now();` → delete
* **308** `embedder_messages += 1;` → delete. ★**The plan never names this line.** It names the symbol but gives only the declaration at 275; leaving 308 behind is a hard compile error.★
* **313** `let embedder_ms = …;` → delete
* **341** `let updates_start = Instant::now();` → delete
* **342** `self.paint.borrow_mut().perform_updates();` → **KEEP**
* **343** `let perform_updates_ms = …;` → delete
* **344-345** rationale comment 2 (the 30 ms threshold) → delete
* **346** `let spin_ms = …;` → delete
* **347-351** `if spin_ms > 30.0 { warn!("SPINSLOW …") }` → delete
* **9** `use std::time::{Duration, Instant};` → narrow to `Duration`. Confirmed: `Instant` occurs in this file at exactly **273, 288, 292, 341** — all four deleted. `Duration` survives at **884**. `warn!` survives at **283**, so that import stays.

Nothing load-bearing is interleaved other than 277-286, 289 and 342, which the plan already calls
out. Both rationale comments describe only the measurement. `SPINSLOW` has no reader anywhere else
in the repo — launcher, docs and tests are all clean.

---

## Step 9

| symbol | plan says | code shows | verdict | correct boundaries |
|---|---|---|---|---|
| `note_tick_site` | "main.rs:**889-903**" | doc **888**, `fn` 889, `}` **901**; 902 is blank and **903 is `pacing_is_driving`'s doc comment (KEEP)** | **WRONG** — 902-903 eats the top of the kept function's doc | **888-901** |
| the latch | "contains … `last_pacing_tick_at.set(Some(Instant::now()))` when `site == ClockSite::PacingThread`" | **890-891** comment; **892-895** `if matches!(site, ClockSite::PacingThread) { self.last_pacing_tick_at.set(Some(std::time::Instant::now())); }` (the `.set(..)` wraps across 893-894); **896-900** the instrumentation (`clock_stats.borrow_mut()`, `window_start.get_or_insert_with(..)`, `tick_by_site[site as usize] += 1`) | **OK in substance**; the inventory's "891-895 load-bearing / 896-902" is off at both ends | latch = **892-895**; instrumentation = **896-900** |
| "a **three-line** `note_pacing_tick(site)`" | 3 lines | the latch is 4 lines as currently formatted (3 if `.set(..)` goes on one line) | **cosmetic off-by-one** | minimal extraction: `fn note_pacing_tick(&self, site: ClockSite) { if matches!(site, ClockSite::PacingThread) { self.last_pacing_tick_at.set(Some(std::time::Instant::now())); } }` — or inline the `if` at both call sites |
| who reads the latch | "`pacing_is_driving()` reads that latch and gates `WakerEvent::Engine if !state.pacing_is_driving()` and `needs_pacing_backstop(..)`" | field `last_pacing_tick_at` at **318** (doc 316-317), init **2185**, written **893**, read **909** inside `pacing_is_driving` (**903-912**). `pacing_is_driving()` called at exactly **2360** and **2429** | **OK — complete: 1 writer, 1 reader, 2 call sites** | `pacing_is_driving` = 903-912; the backstop call is 2427-2430, free fn `needs_pacing_backstop` at **2572** |
| the two call sites to replace | ":1171 in `pump_vsync`, :1580 in `drive_present_clock_timer`" | `self.note_tick_site(site);` at **1171** and **1580** — exactly two | **OK — 2** | both functions keep `site` in use afterwards, so neither `site` parameter is stranded |
| `backstop_only_when_pacing_was_installed_but_is_not_running` "guards step 1 above" | — | that test (**2728-2738**) calls `needs_pacing_backstop(bool, bool)`, a **free function of two booleans** (def 2572). It never builds an `AppState` and never touches `last_pacing_tick_at` | **WRONG** — ★**no test in the tree covers the latch write.** If the extraction silently drops the `.set(..)`, every test still passes while `pacing_is_driving()` is permanently false and the engine-wake backstop re-opens on every wake★ | verify the latch by inspection, not by the gate. `only_vsync_drives_the_shell_present_clock` (2716-2726) also tests only `FrameClock::drives_shell_present_clock` |
| `AppState::note_present_tick` | delete | **868-875** (doc 868), callers **1170**, **1578** | **OK** | 868-875 |
| `note_tick_jitter` | delete | **877-886** (doc 877-880), caller **1579** | **OK** | 877-886 |
| `note_about_to_wait` | delete | **914-920**, caller **2387** | **OK** | 914-920 |
| `note_redraw_requested` | delete | **922-928**, callers **1659** and **2452** | **OK — 2 callers** | 922-928 |
| `note_redraw_suppressed` | "(already caller-less)" | **930-934** — doc 930, `#[allow(dead_code, reason = "2단계 분기 1 에서만 쓴다")]` at **931**, fn 932-934. Zero callers repo-wide | **OK** | **930-934** (the inventory's 932-935 misses the `#[allow]` and overshoots into a blank line) |
| `note_clock_render_pass` | delete | **936-949** (doc 936), caller **1669**, the first line of `render_all_tiles` | **OK** | 936-949 |
| `report_clock_stats` | delete | **951-1053**, caller **2389**. Body is pure logging plus `*stats = ClockStats::default()` with `last_render_at` carried over — no effect outside `clock_stats` | **OK** | 951-1053 |
| the backstop bump | "at :1575-1577" | 1575 `if self.vsync_stalled.get() {`, 1576 `self.clock_stats.borrow_mut().backstop += 1;`, 1577 `}` | **OK** | 1575-1577. `vsync_stalled` itself is read at 1556 and 1584 and stays |
| `let fired_for = next;` | one line | at **1567** (the inventory says 1565-1568); its rationale comment is 1564-1566; only consumer is 1579 | **OK (plan)**, inventory range wrong | delete **1564-1567** |
| `struct ClockStats` | delete | — | **OK** | **415-470** |
| `const MAX_CLOCK_GAP_SAMPLES` | delete | const at **525**, doc 521-524; read at exactly one site, **943** | **OK — 1 reader** | **521-525** |
| `AppState::clock_stats` + init | delete | doc **387**, field **388**, init **2214** | **OK** | 387-388 / 2214 |
| `struct TickIssueAudit` + its `impl` | delete | — | **OK** | **472-503** (doc 472-485, derive 486, struct 487-489, impl 491-503) |
| `AppState::tick_issue_audit` + init | delete | doc 373-374, field **375**, init **2157** | **OK** | 373-375 / 2157 |
| the `debug_assert!` block | delete; call becomes `let _ = webview.on_wall_tick();` | **1649** `if let Some(webview) = webview {`, **1650** `let issued = webview.on_wall_tick();`, **1651** `let mut audit = self.tick_issue_audit.borrow_mut();`, **1652-1657** `debug_assert!(…)`, **1658** `}` | **OK** | delete **1651-1657**; rewrite 1650 |
| the two unit tests | `tick_issue_audit_accepts_…` / `tick_issue_audit_rejects_…` | doc **2624-2625**, `#[test]` 2626, fn **2627-2632**; blank 2633; `#[test]` 2634, fn **2635-2640** | **OK — 2** | **2624-2641** |
| "the **five** remaining `#[cfg(test)]` clock tests" | 5 | `mod tests` (opens **2619**) holds **9** `#[test]`s: 2627, 2635, 2649, 2676, 2694, 2701, 2708, 2717, 2729. Removing the two audit tests leaves **7**. The inventory listed **five line ranges**, one of which (2692-2712) holds three tests; the plan read the range count as a test count — the same mistake as "17 instrumentation tests" | **WRONG — this is the next miscount** | survivors in main.rs: `every_clock_has_a_handle_the_tick_can_fire`, `load_status_maps_to_document_event`, `vsync_wins_over_pacing`, `pacing_only_when_the_thread_runs`, `no_pacing_thread_falls_back_to_timer`, `only_vsync_drives_the_shell_present_clock`, `backstop_only_when_pacing_was_installed_but_is_not_running` = **7**. The whole example target: main.rs 9 + `tick_refresh_driver.rs` 5 + `device_auth_selftest.rs` 6 = **20 now → 18 after Step 9** |
| `ClockSite` KEEP (reduced) | "only `PacingThread` is load-bearing, and `AboutToWait` is still constructed at :2431" | `enum ClockSite` **505-519**; constructed at **2314** (`WakerEvent::Tick` → `PacingThread`), **2315** (`Engine` → `UserEvent`), **2431** (`AboutToWait`), **2445** (`WindowEvent`). All four are constructed, so none goes dead | **OK** | after the strip the only read is `matches!(site, ClockSite::PacingThread)` at 892. The explicit `= 0/1/2/3` discriminants and the `#[allow(dead_code)]` at 509-513 can go; the variant cannot |
| `servo::note_frame_time(...)` at :1609 | KEEP | **1609**, inside `fire_present_tick` (`fn` at **1603**) | **OK** | — |
| `servo::note_present_tick()` at :1613 | KEEP | **1613**; re-exported at `components/servo/lib.rs:49`; `paint/lib.rs:97-101` is the definition | **OK** | distinct from `AppState::note_present_tick` at 869 — the name collision trap is real |
| the pacing-thread banner clause | "the clause `(WALLCLOCK 의 tick_by pace 가 대부분이어야 한다)` … keep the banner" | banner = **2276-2279** (`eprintln!(` 2276, string 2277-2278, `);` 2279); the clause is on **2278**, and **2277 ends in a `\` line continuation** | **OK**, plus a mechanical detail the plan omits | delete 2278 **and** strip the trailing `\` from 2277, or the string literal breaks |
| "the stale doc-comment text that cites now-dead log tags" | unenumerated | the inventory's list is mostly inside items being deleted anyway | **acceptable as a hint** | the ones that **survive** the deletions and need in-place edits: **1091-1092** (`drive_present_clock` doc, cites `MAINBUSY window_ms=3239`), **1270-1275** (inside `snap_to_dwm_grid_at`, cites `WALLCLOCK per_interval skip1` / `ticks/s`), **1300-1313** (`spawn_pacing_thread` doc, cites `about_to_wait` counts and `gap_ms p95`), **2024-2026** (FrameClock startup banner), **2425-2426** (the `needs_pacing_backstop` call-site comment, cites `WALLCLOCK tick_by atw`), **2724-2731** (`backstop_…` test doc), **main.rs:302** (names `SERVO_LOG_PRESENT_CADENCE`, but that whole comment dies with `pass_stats`) |

### The frame clock: which lines inside it are instrumentation

**Answer: none.** All four functions were read end to end; not one contains a counter, a sample
write, or a statistics log.

| fn | boundaries | instrumentation inside |
|---|---|---|
| `snap_to_dwm_grid_at` | doc **1226-1228**, `fn` **1229**, `}` **1279** | **zero executable lines.** `servo::dwm_composition_grid()` at **1235**, `qpc_frequency()` at **1238**, `QueryPerformanceCounter` at **1252** and the `anchor` at **1250** all feed the returned `Instant`. Only strippable text: the 1259-1273 rationale comment, which cites `WALLCLOCK per_interval skip1`. (The body is indented 8 spaces up to 1276 and 4 at 1277-1278 — pre-existing cosmetic oddity, leave it.) |
| `spawn_pacing_thread` | doc **1282-1324**, `fn` **1325**, `}` **1392** | **zero.** The single `log::warn!` at **1390** fires only when the thread fails to spawn — an error report, KEEP. The doc at 1300-1313 is measurement prose and may be rewritten; `AppState::snap_to_dwm_grid_at` is called at **1377** and `proxy.send_event(WakerEvent::Tick)` at 1346 and 1386. |
| `pace_on_vblank` | doc **1404-1412**, `fn` **1413**, `}` **1457** | **zero.** `servo::wait_primary_vblank()` at **1418**. The `qpc_frequency()` read at 1434-1437 computes `vblank_period`, which the `period * 2 <= vblank_period * 3` **decision** at 1438 consumes. The dup-count arithmetic at 1420-1433 is comment only. |
| `align_after_vblank` | doc **1459-1465**, `fn` **1466**, `}` **1484** | **zero.** `qpc_frequency()` 1470, `QueryPerformanceCounter` 1474, `thread::sleep` 1483 — that sleep *is* `-DwmAlign`. |
| `dwm_composition_grid` / `wait_primary_vblank` re-exports | `components/servo/lib.rs:49-50` | KEEP — one caller each (1235 / 1418) |

### `LOG_PRESENT_CADENCE` — the complete coupled set

Verified by repo-wide grep, excluding `painter.rs`/`paint.rs`.

**Compile-coupled — must land in one commit:**
1. `components/config/debug_env.rs:163-170` — the `pub const`
2. `components/config/debug_env.rs:377` — `&LOG_PRESENT_CADENCE,` in `ALL` (position 9 of 25)
3. `components/config/debug_env.rs` — `cache_index` on the **15** constants at `ALL` positions 10..24 (★not in source order★ — `WALL_NO_PROBE` at `:275` carries index 24 but is declared between index 17 at `:262` and index 18 at `:282`)
4. `components/config/tests/config_surface.rs:289` — `&servo_config::debug_env::LOG_PRESENT_CADENCE`
5. `components/config/tests/config_surface.rs:293` — `assert_eq!(dead, vec!["SERVO_LOG_PRESENT_CADENCE"]);`
6. `components/servo/examples/winit_wall/main.rs:647` — inside `static LOG_PASS` (646-650), itself inside `note_render_pass`
7. UNVERIFIED: `components/paint/painter.rs:85` — the plan asserts Step 5 already removed it. **Confirm before starting Step 8.** If it survives, Step 8 breaks `every_registered_flag_is_actually_read_somewhere` from the other direction.

**Test-coupled but not compile-coupled:** `every_registered_flag_is_actually_read_somewhere`
(`config_surface.rs:254-268`). `walk_rust_files` scans every `.rs` under the repo root except
`target/`, `.git/` and `third_party/stylo/`; `scan_sources` additionally skips the three
`REGISTRY_FILES` (`debug_env.rs`, `removed_env.rs`, `config_surface.rs`). So `winit_wall/main.rs`
**is** scanned and `config_surface.rs` is **not** — which is exactly why the plan's rewrite cannot
self-mask a dead flag.

**Text-only:** `docs/multigpu/configuration.md:249-252` (named by the plan) **and `:328`** (not
named); `docs/multigpu/configuration_manual.md:61, 310, 325` (Step 14);
`etc/multigpu/run_wall_dist.ps1:604, 925` plus the `-PresentCadence` switch (Step 16);
`main.rs:302`; plus historical notes under `docs/` and `.superpowers/`, and three stale copies
under `target/wall_dist*/` (build output — ignore).

---

## New load-bearing traps (not in the plan or the inventories)

1. **`main.rs:1727` — `dispatched[index] = true;` is sandwiched inside the instrumentation.**
   The dispatch-loop barrier branch is `{ skipped += 1; dispatched[index] = true; continue; }`
   (1726-1728). The plan says "keep both barrier checks with their `continue`s (the `skipped` bumps
   sit beside them)" but never mentions that a third, load-bearing statement sits between the bump
   and the `continue`. Dropping 1727 silently re-paints every barrier-skipped tile in the second
   loop — a pixel/timing regression with **no compile error**.

2. **`main.rs:1765` — a `continue;` the plan's keep-list does not cover.**
   The `let Some(rendering_context) = tile.rendering_context.as_ref() else { … }` arm (1760-1766)
   ends `tile_ms[tile_index] = tile_start.elapsed()…;` (1764) then `continue;` (1765). The plan
   names only the two `paint_target_keep_previous_logical_frame` `continue`s. This one is a
   compile error if dropped (a `let…else` block must diverge), so it is loud, not silent — but it
   is a third `continue` entangled with a deleted stopwatch.

3. **`main.rs:295-296` — the `--capture` comment sits above the `pass_stats` field.**
   The winit_wall inventory's removal step 4 gives `AppState::pass_stats (295-303)`. Lines 295-296
   document `capture_path` / `capture_deadline` (KEEP, declared 110 lines further down at 370-371),
   not `pass_stats`. Deleting the range as written loses the only explanation of why the capture
   read happens before `present()`.

4. **`main.rs:2277` ends in a `\` string continuation.** Deleting only the WALLCLOCK clause on
   2278 leaves a dangling continuation.

5. **`debug_env.rs` declaration order ≠ `ALL` order.** `cache_index: 24` is declared at `:275`,
   between 17 and 18. Any `cache_index` renumber must be driven off `ALL`, not off a file walk.

6. **Nothing covers the `last_pacing_tick_at` write.** `backstop_only_when_pacing_was_installed_but_is_not_running`
   tests a pure boolean function and cannot detect a dropped latch. Step 9's "order is forced by
   the pacing latch" is right, but its stated verification is not a verification.

## Instrumentation these two steps do not account for

* **`docs/multigpu/configuration.md:328`** — a second `SERVO_LOG_PRESENT_CADENCE` prose reference,
  outside the `###` entry Step 8 names. Step 14 does not name it either.
* **`components/paint/paint.rs:371`** (per `strip-inventory-servo_shared.md:36`, UNVERIFIED) — a
  `TileRenderTiming` occurrence outside both of Step 8's two enumerated paint.rs ranges; it is the
  `join()` return type the plan only describes in prose.
* **`components/servo/servo.rs:308`** `embedder_messages += 1;` — the one SPINSLOW statement the
  plan's symbol list omits (the servo_shared inventory does list it at `:308`).
* **`main.rs:1189`** (per the inventory's stale-comment row) was not individually re-checked; the
  surviving stale comments I did enumerate are listed in the Step 9 table.
