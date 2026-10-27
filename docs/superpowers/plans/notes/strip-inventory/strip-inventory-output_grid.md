# Inventory: paint/output_grid.rs

Scope: `W:\servo_multigpu-tiled-wall\components\paint\output_grid.rs` (2853 lines, 100% fork-added —
`git log --follow` bottoms out at `f77bc2b47b2 feat(paint): 출력별 vblank 격자 모듈`; there is no
upstream Servo code anywhere in this file).

**Headline for this subsystem.** The file has two unrelated things welded together. One is the
**pacing clock** (`wait_primary_vblank` → `VblankSource` → `enumerate_outputs` → `IDXGIOutput::WaitForVBlank`)
and the **common composition grid** (`COMPOSITION` + `composition_grid` + `composition_target` +
`lead_to_next_composition`) that the paint-side animation sampler reads. Both are load-bearing.
Everything else — roughly 2,100 of 2,853 lines — is the 1-second statistics machine: 23 statics,
44 `note_*`/`emit_*`/helper functions, 5 sample structs, a dedicated `OutputVBlankProbe` thread, and
10 unit tests.

**Counts:** REMOVE 84 · KEEP 24 · UNCERTAIN 4.

---

## Category 1 — statics / mutexes that exist only to hold statistics

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `PROBE_ENABLED` | output_grid.rs:33-39 | `LazyLock<bool>` | REMOVE | only reader: `COLLECT` (:50). | Master switch for the probe (`SERVO_WALL_NO_PROBE`). Gate for logging only. |
| `COLLECT` | output_grid.rs:41-51 | `pub(crate) LazyLock<bool>` | REMOVE | in-file: every `note_*`/`emit_*` first line. Out-of-file: `commit_guard.rs:168`, `commit_guard.rs:261`, `commit_guard.rs:358`, `dcomp_compositor.rs:1785` (`note_dwm_phase`). All four are themselves instrumentation. | `pub(crate)` — must die in the same commit as the three other files' gates, or they stop compiling. |
| `GRID` | output_grid.rs:83 | `Mutex<Option<HashMap<usize, OutputGrid>>>` | REMOVE | writers: `note_pacing_vblank` (:161), `probe_loop` (:2128), `forget_vanished` (:2207). readers: `grid_for_monitor` (:130) only. `grid_for_monitor`'s readers are `grid_is_fresh` (probe), `note_dcomp_stat` **after** the COLLECT gate (:1397), `emit_outphase` (:2526, :2566). | **Every reader is instrumentation.** This is the per-output vblank grid; nothing in the drawing or timing path reads it. The B2 sample path deliberately uses `COMPOSITION`, not `GRID` (see the comment at :152-156). |
| `NAMES` | output_grid.rs:85-88 | `Mutex<Option<HashMap<usize, String>>>` | REMOVE | writer: `publish_names` (:2195, probe thread only). reader: `name_for_monitor` (:1669). | See `name_for_monitor` below for the one non-statistics consumer. |
| `COMPOSITION` | output_grid.rs:290-299 | `Mutex<Option<(u64, u64)>>` | **KEEP** | writer: `note_dcomp_stat` (:1375-1377, **before** the COLLECT gate). readers: `composition_grid` (:302) → `lead_to_next_composition` (:652, load-bearing), `period_ms_for_log` (remove), `emit_outphase` (remove). | **Trap.** Looks like a stats cache; it is the grid the paint-side animation sampler lands on. |
| `SLIPS` | output_grid.rs:339 | `Mutex<Option<HashMap<usize, SlipTally>>>` | REMOVE | writer `note_sample_slip` (:490), drained by `emit_sampleslip` (:556). | `SAMPLESLIP`. |
| `PUMP_FROM_RENDER` | output_grid.rs:354 | `AtomicU64` | REMOVE | `note_pump_from_render` (:423) ← `painter.rs:5048`; drained `emit_sampleslip` (:551). | Printed in the `pump_all` field of `SAMPLESLIP`. |
| `PUMP_FROM_IDLE` | output_grid.rs:355 | `AtomicU64` | REMOVE | `note_pump_from_idle` (:430) ← `painter.rs:5049`; drained `emit_sampleslip` (:552). | — |
| `PUMP_FROM_TICK` | output_grid.rs:356 | `AtomicU64` | REMOVE | `note_pump_from_tick` (:437) ← `painter.rs:5050`; drained `emit_sampleslip` (:553). | — |
| `FRAME_IDS` | output_grid.rs:359-377 | `Mutex<Option<HashMap<PainterId, (u64,u64,HashSet<u64>)>>>` | REMOVE | writer `note_frame_id` (:383) ← `painter.rs:2585`; drained `emit_frameid` (:400). | `FRAMEID`. |
| `DCOMP_STATS` | output_grid.rs:688 | `Mutex<Option<HashMap<usize, Vec<DcompSample>>>>` | REMOVE | writer: tail of `note_dcomp_stat` (:1448); drained `emit_dcompstat` (:1459). | `DCOMPSTAT`. Sits **after** the COLLECT gate, so it is cleanly separable from the `COMPOSITION` write above it. |
| `DCOMP_STAT_FAILED` | output_grid.rs:689 | `AtomicU64` | REMOVE | `note_dcomp_stat_failed` (:1328) ← `painter.rs:4299`, `dcomp_compositor.rs:2583`, and :1345/:1356 in-file; drained `emit_dcompstat` (:1466). | — |
| `COMMITS` | output_grid.rs:691-702 | `Mutex<Option<HashMap<usize, u64>>>` | REMOVE | writer `note_commit_at` (:709) ← `dcomp_compositor.rs:2354, 3258, 3598, 5230`; reader `commit_at` (:1321) → `commit_lead_ms`/`commit_to_comp_ms`/`commit_periods` in `DCOMPSTAT` only. | — |
| `COMMIT_GAPS` | output_grid.rs:715 | `Mutex<Option<HashMap<usize, Vec<u64>>>>` | REMOVE | writer `note_commit_stamp` (:767); drained `emit_commitgap` (:792). | `COMMITGAP`. |
| `LAST_COMMIT` | output_grid.rs:717-719 | `Mutex<Option<HashMap<usize, u64>>>` | REMOVE | read+write `note_commit_stamp` (:756). | — |
| `COMMIT_STAMPS` | output_grid.rs:833-838 | `Mutex<Option<HashMap<usize, Vec<u64>>>>` | REMOVE | writer `note_commit_stamp` (:761); drained `emit_commitcycle` (:917). | `COMMITCYCLE`. |
| `BIND_SPANS` | output_grid.rs:971-980 | `Mutex<Option<HashMap<usize, Vec<(u64,u64)>>>>` | REMOVE | writer `note_bind_span` (:1004) ← `dcomp_compositor.rs:4278`; drained `emit_bindopen` (:1055). | `BINDOPEN`. |
| `TICK_QPC` | output_grid.rs:1121-1139 | `AtomicU64` | REMOVE | writer `note_present_tick_now` (:1183); reader `note_tick_to_commit` (:1215). | — |
| `PASS_RING_CAP` | output_grid.rs:1165-1167 | `const usize` | REMOVE | `note_present_tick_now` (:1187). | — |
| `PASS_RING` | output_grid.rs:1168 | `Mutex<Option<VecDeque<PassMark>>>` | REMOVE | writers `note_present_tick_now` (:1186), `note_tick_to_commit` (:1231); reader `pass_before` (:1247) → `emit_missevent`. | `MISSEVENT`. |
| `PASS_SEQ` | output_grid.rs:1169-1170 | `AtomicU64` | REMOVE | `note_present_tick_now` (:1191). | — |
| `TICK_TO_COMMIT` | output_grid.rs:1200-1201 | `Mutex<Option<HashMap<usize, Vec<f64>>>>` | REMOVE | writer `note_tick_to_commit` (:1224); drained `emit_tickcommit` (:1285). | `TICKCOMMIT`. |
| `LEADS` | output_grid.rs:1608-1609 | `Mutex<Option<HashMap<usize, Vec<f64>>>>` | REMOVE | writer `note_sample_lead` (:1617) ← `painter.rs:4330`; drained `emit_samplelead` (:1640). | `SAMPLELEAD`. The `Duration` it records is already in use by the caller; recording it changes nothing. |
| `LEAD_MISSING` | output_grid.rs:1610-1611 | `AtomicU64` | REMOVE | `note_sample_lead_missing` (:1629) ← `painter.rs:4334`; drained `emit_samplelead` (:1646). | — |
| `PACING_OUTPUT_PIN` | output_grid.rs:1722-1729 | `LazyLock<String>` (pref `gfx_wall_pacing_output`) | **UNCERTAIN** | readers: `pinned_index` (:1737) → `VblankSource::refresh` (:1818, chooses `wait_idx`), and `log_anchor` (:1848, log only). | It **changes which physical output the pacing clock blocks on**, so by the stated rules it is load-bearing. But it was introduced purely to A/B the 2026-10-26 "which output is the DWM grid on" question. Ask the user; do not remove silently. |
| `FREQ` (inside `qpc_frequency`) | output_grid.rs:92-93 | `OnceLock<Option<u64>>` | **KEEP** | see `qpc_frequency`. | — |

---

## Category 2 — `note_*` / `emit_*` and the helpers, sample structs and threads that exist only for them

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `OutputGrid` (struct) | output_grid.rs:65-81 | struct | REMOVE | constructed in `note_pacing_vblank` (:162) and `probe_loop` (:2131); consumed by `grid_for_monitor`, `grid_is_fresh`, `note_dcomp_stat` (post-gate), `emit_outphase`. | `pub(crate)` but no out-of-file reference (grep across `components/` is clean). |
| `grid_for_monitor` | output_grid.rs:129-131 | fn | REMOVE | `grid_is_fresh` (:139), `note_dcomp_stat` :1397 (post-gate), `emit_outphase` :2526 & :2566. | All three readers removable. |
| `grid_is_fresh` | output_grid.rs:133-143 | fn | REMOVE | `probe_loop` (:2113). | — |
| `note_pacing_vblank` | output_grid.rs:145-173 | `pub(crate) fn` | REMOVE | `VblankSource::wait` (:1882) — the **only** caller. Its own doc says "`GRID` 의 소비자는 전부 진단이다". | **The pacing clock does not read back what this writes.** Deleting it leaves `wait()` returning the same `(vblank, period)`. |
| `DcompSample` (struct) | output_grid.rs:219-287 | struct | REMOVE | built in `note_dcomp_stat` (:1431), read in `emit_dcompstat`. | 14 fields, all printed. |
| `SlipTally` (struct) | output_grid.rs:304-337 | struct | REMOVE | `note_sample_slip`, `emit_sampleslip`. | — |
| `note_frame_id` | output_grid.rs:379-394 | `pub(crate) fn` | REMOVE | `painter.rs:2585` (passes `painter_id` + `last_ready_wall_logical_frame_id`; the value is otherwise unused there). | Caller line must go too. |
| `emit_frameid` | output_grid.rs:396-418 | fn | REMOVE | `probe_loop` (:2177). | `FRAMEID`. |
| `note_pump_from_render` | output_grid.rs:420-425 | `pub(crate) fn` | REMOVE | `painter.rs:5048` (`PumpSource::note`). | `PumpSource` itself is load-bearing (`may_schedule`); only `note()` goes. |
| `note_pump_from_idle` | output_grid.rs:427-432 | `pub(crate) fn` | REMOVE | `painter.rs:5049`. | — |
| `note_pump_from_tick` | output_grid.rs:434-439 | `pub(crate) fn` | REMOVE | `painter.rs:5050`. | — |
| `compositions_advanced` | output_grid.rs:441-484 | fn | REMOVE | `note_sample_slip` (:513) and 6 unit tests. | Pure arithmetic; classifies slips for the log. |
| `note_sample_slip` | output_grid.rs:486-539 | fn | REMOVE | `lead_to_next_composition` (:681) — **inside a load-bearing function**. | Removing this line is what makes `lead_to_next_composition`'s `monitor`, `phase_ms`, `residue_ms`, and `freq`-for-phase all unnecessary. |
| `period_ms_for_log` | output_grid.rs:539-547 | fn | REMOVE | `emit_sampleslip` (:605). | Name says it. |
| `emit_sampleslip` | output_grid.rs:548-630 | fn | REMOVE | `probe_loop` (:2171). | `SAMPLESLIP`. |
| `note_commit_at` | output_grid.rs:704-713 | `pub(crate) fn` | REMOVE | `dcomp_compositor.rs:2354` (`note_tick_commit`), `:3258`, `:3598`, `:5230`. | Writes `COMMITS`; only `commit_at` reads it. |
| `commit_stamp` | output_grid.rs:737-740 | `pub(crate) fn` | REMOVE | `dcomp_compositor.rs:2258` (inside `raw_commit`, result stored in `CommitOutcome::stamp`). | The `stamp` field of `CommitOutcome` exists only to feed `note_commit_stamp`. |
| `note_commit_stamp` | output_grid.rs:742-772 | `pub(crate) fn` | REMOVE | `dcomp_compositor.rs:2343`, `:3257`, `:3597`, `:5229`. | Feeds `LAST_COMMIT`/`COMMIT_STAMPS`/`COMMIT_GAPS` and calls `note_tick_to_commit`. |
| `emit_commitgap` | output_grid.rs:773-836 | fn | REMOVE | `probe_loop` (:2173). | `COMMITGAP`. Calls `dcomp_compositor::composition_grid()` (DWM syscall) purely for bucketing. |
| `emit_commitcycle` | output_grid.rs:840-977 | fn | REMOVE | `probe_loop` (:2174). | `COMMITCYCLE`. ~75 lines of this range are the finding-log comment block. Also a DWM syscall at :924. |
| `note_bind_span` | output_grid.rs:982-1010 | `pub(crate) fn` | REMOVE | `dcomp_compositor.rs:4278` (and the `BoundTile::begin_qpc` field at `dcomp_compositor.rs:1312` that feeds it). | — |
| `emit_bindopen` | output_grid.rs:1012-1137 | fn | REMOVE | `probe_loop` (:2176). | `BINDOPEN`. DWM syscall at :1063. |
| `PassMark` (struct) | output_grid.rs:1141-1163 | struct | REMOVE | `PASS_RING`, `pass_before`, `emit_missevent`. | — |
| `note_present_tick_now` | output_grid.rs:1172-1198 | `pub(crate) fn` | REMOVE | `lib.rs:100` inside `pub fn note_present_tick()` ← `winit_wall/main.rs:1613` and `main.rs:869/1170/1578`. | **Partial-caller trap:** `lib.rs::note_present_tick` also calls `crate::wall_tick::note_tick_edge()`, which **is** load-bearing (external-present budget + production edge). Delete only the `#[cfg(windows)] crate::output_grid::note_present_tick_now();` line at `lib.rs:99-100`, not the public function. Also: this is the only place `start_probe()` is called from. |
| `note_tick_to_commit` | output_grid.rs:1203-1244 | fn | REMOVE | `note_commit_stamp` (:764). | — |
| `pass_before` | output_grid.rs:1241-1266 | fn | REMOVE | `emit_comp_stats` (:2316). | — |
| `emit_tickcommit` | output_grid.rs:1268-1319 | fn | REMOVE | `probe_loop` (:2172). | `TICKCOMMIT`. DWM syscall at :1294. |
| `commit_at` | output_grid.rs:1320-1323 | fn | REMOVE | `note_dcomp_stat` (:1363) — in the post-gate half. | — |
| `note_dcomp_stat_failed` | output_grid.rs:1325-1330 | `pub(crate) fn` | REMOVE | `painter.rs:4299`, `dcomp_compositor.rs:2583`, in-file :1345 and :1356. | — |
| `note_dcomp_stat` | output_grid.rs:1332-1453 | `pub(crate) fn` | **UNCERTAIN** (reduce, do not delete) | `dcomp_compositor.rs:2604` (`DCompNativeCompositor::note_frame_statistics`) ← `painter.rs:4281 note_dcomp_frame_statistics` ← painter render path. | **The single most dangerous item in this file.** Lines :1344-1358 + :1372-1377 compute `period_ticks` and store `(last, period_ticks)` into `COMPOSITION` **before** the `if !*COLLECT { return; }` at :1378. That store is the **only** writer of the grid that `lead_to_next_composition` → `paint_animation_sample_at` → `painter.rs:1366 sample_at` reads, i.e. it decides **what time the paint-side animation is sampled at**. Everything from :1359-1371 and :1381-1453 is `DCOMPSTAT` only. The uncertainty is scope, not mechanism: if the user also wants the B2 `gfx_sample_lead_periods` feature gone, then this whole function, `COMPOSITION`, `composition_grid`, `composition_target`, `lead_to_next_composition`, and `GetFrameStatistics` all go together. **Ask.** |
| `emit_dcompstat` | output_grid.rs:1455-1606 | fn | REMOVE | `probe_loop` (:2178). | `DCOMPSTAT`, the longest emitter (~150 lines). |
| `note_sample_lead` | output_grid.rs:1613-1624 | `pub(crate) fn` | REMOVE | `painter.rs:4330` — records a `Duration` the caller already has and uses. | — |
| `note_sample_lead_missing` | output_grid.rs:1626-1631 | `pub(crate) fn` | REMOVE | `painter.rs:4334`. | — |
| `emit_samplelead` | output_grid.rs:1633-1665 | fn | REMOVE | `probe_loop` (:2170). | `SAMPLELEAD`. |
| `name_for_monitor` | output_grid.rs:1667-1670 | `pub(crate) fn` | REMOVE | in-file: `emit_sampleslip` (:584), `emit_dcompstat` (:1490), `emit_samplelead` (:1654). Out-of-file: **`dcomp_compositor.rs:2301`** in `note_commit_failure`. | **Caller-side consequence:** `dcomp_compositor`'s once-per-second *Commit failed* warn resolves `device → monitor → "\\.\DISPLAYn"`. Removing `NAMES` degrades that error line to `out=?`. That warn is an error path, not a 1-second statistics window, so flag it: either accept `out=?`, or re-publish names from `VblankSource::refresh` (it already enumerates them). |
| `start_probe` | output_grid.rs:1672-1682 | `pub(crate) fn` | REMOVE | `note_present_tick_now` (:1181) — the only caller in the tree. | Spawns the `OutputVBlankProbe` thread. |
| `VblankSource::log_anchor` | output_grid.rs:1826-1856 | method | REMOVE | `VblankSource::wait` (:1890). | `VBANCHOR`. Also makes `VblankSource::last_log_qpc` dead. |
| `VblankSource::last_log_qpc` (field) | output_grid.rs:1757-1758 (+ :1770 init) | field | REMOVE | `log_anchor` only. | — |
| `Output::adapter_luid` (field) | output_grid.rs:1908-1913 | field | REMOVE | written in `enumerate_outputs` (:2638-2648, via `IDXGIAdapter1::GetDesc1`); read only in `emit_comp_stats` (:2328). | Removing it removes the `GetDesc1` call and the `DXGI_ADAPTER_DESC1` import. |
| `Enumeration::names` | output_grid.rs:1956-1959 | method | REMOVE | `probe_loop` (:2088). | Used only to decide whether to re-print `announce`. |
| `throttled` | output_grid.rs:1961-1975 | fn | REMOVE | `probe_loop` (:2062, :2070). | Log-rate limiter. |
| `period_from_pair` | output_grid.rs:1977-2008 | fn | REMOVE | `probe_loop` (:2124) and 4 unit tests. | **Not used by the pacing clock** — `VblankSource::refresh` reads the period from the display mode (`nominal_period_ticks(..., None)`) precisely so it never has to wait twice. |
| `wait_two_vblanks` | output_grid.rs:2010-2025 | `unsafe fn` | REMOVE | `probe_loop` (:2118). | This is the **probe's** `WaitForVBlank` pair, not the clock's. |
| `probe_loop` | output_grid.rs:2028-2180 | fn | REMOVE | `start_probe` (:2027 spawn). | The whole `OutputVBlankProbe` thread. Lines :2165-2179 are the 1-second emitter batch, including the out-of-file `crate::commit_guard::emit_lockwait()` at :2175 (its only caller). |
| `announce` | output_grid.rs:2182-2192 | fn | REMOVE | `probe_loop` (:2090). | — |
| `publish_names` | output_grid.rs:2194-2203 | fn | REMOVE | `probe_loop` (:2093). | Only writer of `NAMES`. |
| `forget_vanished` | output_grid.rs:2205-2218 | fn | REMOVE | `probe_loop` (:2094). | Only prunes `GRID`/`NAMES`. |
| `TargetWalk` (struct) | output_grid.rs:2220-2239 | struct | REMOVE | `emit_comp_stats`. | — |
| `bump` | output_grid.rs:2241-2244 | fn | REMOVE | `emit_comp_stats` (:2342, :2348, :2375). | — |
| `emit_comp_stats` | output_grid.rs:2246-2450 | fn | REMOVE | `probe_loop` (:2169). | `COMPWALK` + `COMPREFRESH`. **This is the only consumer of the entire `components/paint/comp_stats.rs` module** (`completed_frame_id`, `sample_frame`, `FrameSample`, `TargetSample` — `DCompositionGetStatistics`). |
| `emit_missevent` | output_grid.rs:2452-2502 | fn | REMOVE | `emit_comp_stats` (:2356). | `MISSEVENT`. |
| `emit_outphase` | output_grid.rs:2514-2618 | fn | REMOVE | `probe_loop` (:2168). | `OUTPHASE`. Calls `dcomp_compositor::composition_grid()` (:2561, DWM syscall), `composition_grid()` (:2562) and `is_primary` (:2535, :2571) purely for the line. |
| `qpc_frequency` | output_grid.rs:90-101 | `pub(crate) fn` | **KEEP** | `VblankSource::refresh` (:1824), `VblankSource::wait` (:1884, :1888), `lead_to_next_composition` (:653), `probe_loop`, every emitter, plus `commit_guard.rs:86` import. | Pure `QueryPerformanceFrequency` + `OnceLock`. The clock needs it. |
| `qpc_now` | output_grid.rs:103-112 | `pub(crate) fn` | **KEEP** | `VblankSource::wait` (:1877) — the clock's timestamp; `lead_to_next_composition` (:655); plus many removable callers and `dcomp_compositor.rs:2591/3991/4276`, `commit_guard.rs:86`. | Pure `QueryPerformanceCounter`. |
| `monitor_for_hwnd` | output_grid.rs:114-126 | `pub(crate) fn` | **UNCERTAIN** | `painter.rs:4348 tile_monitor()` ← `painter.rs:4291` (`note_dcomp_frame_statistics`, supplies the `monitor` arg of `note_dcomp_stat`/`remember_device_monitor`) and `painter.rs:4327` (supplies the `monitor` arg of `lead_to_next_composition`, which uses it **only** for `note_sample_slip`). | Not itself instrumentation — a 6-line `MonitorFromWindow` wrapper with no statics and no logging. But after the strip, **neither surviving call site needs the value**: `COMPOSITION` is process-global. Whether `monitor_for_hwnd`/`tile_monitor` survive depends on whether `commit_guard::remember_device_monitor` (device→monitor, used by the Commit-failure warn) is kept. Decide with that file's owner; do not delete blind. |
| `nominal_period_ticks` | output_grid.rs:175-199 | fn | **KEEP** | `VblankSource::refresh` (:1824) — sets `self.period_qpc`, which `wait()` returns as the pacing period. Also `probe_loop` (:2129, removable). | **Load-bearing: this is the clock's period.** |
| `display_frequency_hz` | output_grid.rs:201-216 | fn | **KEEP** | `nominal_period_ticks` (:186). | `EnumDisplaySettingsW`. A display OS syscall, but its result sets the pacing period, not a log line. |
| `composition_grid` | output_grid.rs:301-303 | `pub(crate) fn` | **KEEP** | `period_ms_for_log` (remove), `lead_to_next_composition` (:652, **keep**), `emit_outphase` (:2562, remove). | Do not confuse with `dcomp_compositor::composition_grid()`, which is a different function (a `DwmGetCompositionTimingInfo` call). |
| `composition_target` | output_grid.rs:613-634 | fn | **KEEP** | `lead_to_next_composition` (:671) and 2 unit tests. | Pure arithmetic that decides the animation sample instant. |
| `lead_to_next_composition` | output_grid.rs:636-686 | `pub(crate) fn` | **KEEP** (trim) | `painter.rs:4328` → `paint_animation_sample_at` → `painter.rs:1366 let sample_at = …` → what the paint-side animation draws. | Keep lines :652-655, :671, :682-685. Delete :657-669's diagnostic phrasing as desired, and definitely delete :673-681 (`phase_ms`, `residue_ms`, `note_sample_slip`). After that, the `monitor` parameter is unused — change the signature and `painter.rs:4327`. |
| `wait_primary_vblank` | output_grid.rs:1684-1716 | `pub(crate) fn` | **KEEP** | `lib.rs:55` → `pub fn servo::wait_primary_vblank()` → `winit_wall/main.rs:1418` inside `pace_on_vblank` ← `spawn_pacing_thread` (:1342). | **This is the frame clock.** |
| `pinned_index` | output_grid.rs:1731-1744 | fn | **UNCERTAIN** | `VblankSource::refresh` (:1818). | Same judgement as `PACING_OUTPUT_PIN`: it changes which output the clock waits on (behaviour), but it exists only because of the judder A/B. |
| `VblankSource` (struct) | output_grid.rs:1746-1759 | struct | **KEEP** (drop `last_log_qpc`) | thread-local in `wait_primary_vblank` (:1712-1715). | — |
| `VblankSource::new` | output_grid.rs:1762-1771 | `const fn` | **KEEP** | thread-local initializer. | — |
| `VblankSource::refresh` | output_grid.rs:1773-1831 | method | **KEEP** | `wait()` (:1859). | Opens/validates the DXGI enumeration, picks `wait_idx`, reads the period. |
| `VblankSource::wait` | output_grid.rs:1858-1892 | method | **KEEP** (trim) | `wait_primary_vblank`. | Keep the `WaitForVBlank` at :1868, the `qpc_now()` at :1877, the period fallback at :1878-1880, and the `Some((vblank, period))`. Delete :1881-1891 (`note_pacing_vblank` + `log_anchor`). |
| `impl Drop for VblankSource` | output_grid.rs:1893-1901 | impl | **KEEP** | thread-local teardown. | — |
| `Output` (struct) | output_grid.rs:1903-1914 | struct | **KEEP** (drop `adapter_luid`) | `enumerate_outputs`, `VblankSource`, `pinned_index`, `is_primary`, and the removable probe/emitters. | — |
| `Enumeration` (struct) | output_grid.rs:1917-1925 | struct | **KEEP** | `VblankSource` + `probe_loop`. | — |
| `Enumeration::release` | output_grid.rs:1927-1936 | `unsafe fn` | **KEEP** | `VblankSource::refresh` (:1789, :1861-ish in `wait`'s failure path :1872), `Drop` (:1897), `probe_loop` (:2067). | COM teardown. |
| `Enumeration::needs_reenumerate` | output_grid.rs:1938-1954 | `unsafe fn` | **KEEP** | `VblankSource::refresh` (:1782), `probe_loop` (:2060). | `IDXGIFactory1::IsCurrent`. |
| `is_primary` | output_grid.rs:2496-2512 | fn | **KEEP** | `VblankSource::refresh` (:1810) — picks the default pacing output. Also `emit_outphase` ×3 (remove). | `GetMonitorInfoW`. Load-bearing via `refresh`. |
| `enumerate_outputs` | output_grid.rs:2620-2683 | `unsafe fn` | **KEEP** (trim `GetDesc1`) | `VblankSource::refresh` (:1792), `probe_loop` (:2069). | DXGI factory/adapter/output walk. Remove :2638-2648 (`DXGI_ADAPTER_DESC1` / `adapter_luid`) and the `adapter_luid` field init at :2676. |

---

## Category 3 — DWM / display OS syscalls, per call site

Same function, different verdicts per site. This table is call sites, already counted above under their
enclosing functions.

| syscall | call site | verdict | what the result reaches |
|---|---|---|---|
| `IDXGIOutput::WaitForVBlank` | `VblankSource::wait` output_grid.rs:1868 | **KEEP** | **The frame clock.** Blocks the pacing thread; return feeds `pace_on_vblank`. |
| `IDXGIOutput::WaitForVBlank` ×2 | `wait_two_vblanks` output_grid.rs:2017, :2021 | REMOVE | `GRID` only. |
| `QueryPerformanceFrequency` | `qpc_frequency` :96 | KEEP | clock arithmetic. |
| `QueryPerformanceCounter` | `qpc_now` :107 | KEEP | clock timestamp (plus many removable readers). |
| `EnumDisplaySettingsW` | `display_frequency_hz` :210 | KEEP | pacing period via `nominal_period_ticks`. |
| `GetMonitorInfoW` | `is_primary` :2507 | KEEP (via `refresh`) / REMOVE (via `emit_outphase`) | default pacing output / `OUTPHASE` line. |
| `MonitorFromWindow` | `monitor_for_hwnd` :121 | UNCERTAIN | see that row. |
| `CreateDXGIFactory1`, `EnumAdapters1`, `EnumOutputs`, `IDXGIOutput::GetDesc`, `IDXGIFactory1::IsCurrent` | `enumerate_outputs` / `needs_reenumerate` | KEEP | the clock's output handle. |
| `IDXGIAdapter1::GetDesc1` | `enumerate_outputs` :2642 | REMOVE | `adapter_luid` → `COMPREFRESH` line only. |
| `DwmGetCompositionTimingInfo` via `dcomp_compositor::composition_grid()` | output_grid.rs:802, :924, :1063, :1294, :2293, :2561 | REMOVE (all six) | bucket widths and phase percentages in `COMMITGAP`/`COMMITCYCLE`/`BINDOPEN`/`TICKCOMMIT`/`COMPWALK`/`OUTPHASE`. **None of these six is the pacing-timer fallback** — that one is `winit_wall/main.rs:1235 snap_to_dwm_grid_at`, which calls `servo::dwm_composition_grid()` directly and is not in this file. |
| `DwmGetCompositionTimingInfo` via `dcomp_compositor::dwm_timing()` | output_grid.rs:1396 | REMOVE | `DCOMPSTAT`'s `last_vs_dwmvb_ms` / `compose_after_vblank_ms`. Post-COLLECT-gate. |
| `IDCompositionDevice::GetFrameStatistics` | `dcomp_compositor.rs:2583` → `note_dcomp_stat` | UNCERTAIN | **Trap.** Its `lastFrameTime` + `currentCompositionRate` are what fill `COMPOSITION`. See the `note_dcomp_stat` row. |
| `DCompositionGetStatistics` (`comp_stats.rs`) | `emit_comp_stats` :2265, :2304 | REMOVE | `COMPWALK`/`COMPREFRESH`/`MISSEVENT`. Whole module becomes dead. |

---

## Category 4 — tests (build gate: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu`)

| test | line | verdict | why |
|---|---|---|---|
| `one_composition_is_one_step` | 2693-2698 | REMOVE | exercises `compositions_advanced`. |
| `several_compositions_are_counted` | 2700-2707 | REMOVE | same. |
| `the_same_grid_point_is_zero_steps` | 2709-2715 | REMOVE | same. |
| `a_wobbling_reported_rate_does_not_invent_steps` | 2717-2745 | REMOVE | same. |
| `a_backward_grid_is_negative_not_zero` | 2747-2752 | REMOVE | same. |
| `a_zero_period_counts_nothing` | 2754-2757 | REMOVE | same. |
| `a_normal_pair_yields_the_measured_period` | 2817-2827 | REMOVE | exercises `period_from_pair` (probe-only). |
| `a_pair_faster_than_240hz_falls_back_to_the_assumed_period` | 2828-2834 | REMOVE | same. |
| `a_pair_slower_than_30hz_falls_back_to_the_assumed_period` | 2836-2842 | REMOVE | same. |
| `a_backwards_pair_falls_back_to_the_assumed_period` | 2844-2851 | REMOVE | same. |
| `FREQ` / `ASSUMED` consts in `mod tests` | 2813-2816 | REMOVE | used only by the four `period_from_pair` tests. |
| `the_target_is_always_on_the_grid_and_advances_one_period` | 2758-2786 | **KEEP** | guards `composition_target`, which decides the animation sample instant. |
| `tiles_reading_the_same_composition_agree` | 2787-2796 | **KEEP** | same. |
| `nominal_period_picks_the_exact_candidate` | 2798-2811 | **KEEP** | pure arithmetic on literals; references no symbol, so it compiles either way. It documents the 60 vs 59.94 pick that `nominal_period_ticks` (kept) performs. |

`mod tests`'s `use super::{compositions_advanced, composition_target, period_from_pair};` (:2686-2689)
must be reduced to `use super::composition_target;` in the same edit, or the module will not compile.

---

## Removal order and build-breakage notes

Each step should leave the tree compiling. `cargo check -p servo-paint` after each.

**Step 1 — the probe thread and everything only it calls (pure in-file; no other crate touched).**
Delete `start_probe`, `probe_loop`, `announce`, `publish_names`, `forget_vanished`, `throttled`,
`period_from_pair`, `wait_two_vblanks`, `Enumeration::names`, `grid_is_fresh`, and the ten tests in
Category 4. **At the same moment** delete the one call into it: `#[cfg(windows)] start_probe();` at
output_grid.rs:1181 (inside `note_present_tick_now`). This orphans `commit_guard::emit_lockwait`
(output_grid.rs:2175 is its only caller) and the whole of `components/paint/comp_stats.rs`
(`emit_comp_stats` is its only consumer) — coordinate with those owners, or temporarily leave
`emit_lockwait`/`comp_stats` in place behind `#[allow(dead_code)]` and delete them in their own commit
along with `mod comp_stats;` at `lib.rs:137`.

**Step 2 — the emitters the probe used to drive, and the statics they drained.** `emit_outphase`,
`emit_comp_stats`, `emit_missevent`, `bump`, `TargetWalk`, `emit_samplelead`, `emit_sampleslip`,
`period_ms_for_log`, `emit_tickcommit`, `emit_commitgap`, `emit_commitcycle`, `emit_bindopen`,
`emit_frameid`, `emit_dcompstat`, `pass_before`, `PassMark`, `PASS_RING`, `PASS_RING_CAP`, `PASS_SEQ`,
`SlipTally`, `SLIPS`, `DcompSample`, `DCOMP_STATS`, `DCOMP_STAT_FAILED`, `LEADS`, `LEAD_MISSING`,
`TICK_TO_COMMIT`, `TICK_QPC`, `COMMIT_STAMPS`, `COMMIT_GAPS`, `LAST_COMMIT`, `COMMITS`, `BIND_SPANS`,
`FRAME_IDS`, `PUMP_FROM_*`, `compositions_advanced`, `commit_at`. Nothing outside the file reads any of
these.

**Step 3 — the `note_*` entry points, each together with its out-of-file caller line.** These break
other files the instant they go, so each must be one atomic edit:
- `note_frame_id` ⟷ `painter.rs:2584-2588`
- `note_pump_from_render/idle/tick` ⟷ `painter.rs:5046-5052` (`PumpSource::note`) and the `self.…note()`
  call that invokes it. **Keep `PumpSource` and `PumpSource::may_schedule` — they gate frame production.**
- `note_sample_lead` / `note_sample_lead_missing` ⟷ `painter.rs:4329-4335`
- `note_dcomp_stat_failed` ⟷ `painter.rs:4299`, `dcomp_compositor.rs:2583`
- `note_commit_at` ⟷ `dcomp_compositor.rs:2354, 3258, 3598, 5230`
- `note_commit_stamp` + `commit_stamp` ⟷ `dcomp_compositor.rs:2258, 2343, 3257, 3597, 5229`
  (and the now-dead `stamp` field of `CommitOutcome`)
- `note_bind_span` ⟷ `dcomp_compositor.rs:4276-4278` (and `BoundTile::begin_qpc`,
  `dcomp_compositor.rs:1312`, plus the `begin_qpc` fill at `:3990`)
- `note_present_tick_now` ⟷ `lib.rs:99-100` **only those two lines** — `pub fn note_present_tick()`
  itself stays, because `crate::wall_tick::note_tick_edge()` on the line above is load-bearing.
- `note_pacing_vblank` ⟷ output_grid.rs:1882 (in-file, inside `VblankSource::wait`)
- `name_for_monitor` + `NAMES` ⟷ `dcomp_compositor.rs:2300-2302` — decide first whether that failure
  warn keeps an output name (see the `name_for_monitor` row).
- `GRID` + `OutputGrid` + `grid_for_monitor` have no callers left after Steps 1-2 except
  `note_dcomp_stat`'s post-gate half, which goes in Step 4.

**Step 4 — reduce, do not delete, the two load-bearing functions.**
- `note_dcomp_stat`: keep the signature's `last`, `freq`, `rate_num`, `rate_den`; keep the guard at
  :1344-1347 (drop its `note_dcomp_stat_failed()` body to a bare `return`), the `period_ticks`
  computation at :1354-1358, and the `COMPOSITION` store at :1375-1377. Delete everything else, and
  drop the now-unused `monitor`, `device`, `now`, `next`, `qpc_at_read` parameters — which also lets you
  delete `dcomp_compositor.rs:2591`'s `qpc_at_read` read.
- `lead_to_next_composition`: delete :673-681 (`phase_ms`, `residue_ms`, `note_sample_slip`) and the
  `monitor` parameter; update `painter.rs:4327-4328`.

**Step 5 — `COLLECT` / `PROBE_ENABLED`, last.** `COLLECT` is `pub(crate)` and read from three other
files (`commit_guard.rs:168, 261, 358`; `dcomp_compositor.rs:1785`). It can only be deleted after those
four readers are gone, so it belongs to whichever commit finishes `commit_guard`/`dcomp_compositor`.
Also drop `WALL_NO_PROBE` from `components/config/debug_env.rs:264-276` and the `&WALL_NO_PROBE` entry
in the registry at `:386` (note the `cache_index: 24` slot).

**Step 6 — imports.** After the above, these become unused and will warn (or error under `-D warnings`):
`std::collections::{HashMap, HashSet, VecDeque}` (whole line), `std::sync::atomic::{AtomicU64, Ordering}`
(whole line), `Instant` from `std::time::{Duration, Instant}` (`lead_to_next_composition` names
`std::time::Instant` fully qualified, so keep `Duration` only), `servo_base::id::PainterId`,
`DXGI_ADAPTER_DESC1` from the `winapi::shared::dxgi` group. Keep: `std::ptr`,
`std::sync::{LazyLock, Mutex, OnceLock}`, `log::warn` (still used by `enumerate_outputs`'s
`CreateDXGIFactory1 실패`), `winapi::Interface`, and every `winuser`/`wingdi`/`profileapi` import.
The module-level `#![allow(unsafe_code)]` / `#![allow(unsafe_op_in_unsafe_fn)]` must stay —
`enumerate_outputs` and the COM teardown remain.

**Expected shape afterwards:** output_grid.rs drops from 2853 lines to roughly 600-700 — a QPC helper
pair, the display-mode period helpers, `COMPOSITION` + `composition_grid` + `composition_target` +
`lead_to_next_composition`, and the `VblankSource`/`Enumeration`/`Output` DXGI clock, plus three tests.

---

## Traps

- **`IDXGIOutput::WaitForVBlank` at output_grid.rs:1868 is the frame clock, not a probe.** There are three
  `WaitForVBlank` calls in this file. The two in `wait_two_vblanks` (:2017, :2021) are the probe and go;
  the one in `VblankSource::wait` is what `winit_wall`'s `spawn_pacing_thread` → `pace_on_vblank` blocks
  on. Deleting the module wholesale stops the wall.
- **`COMPOSITION` is written by instrumentation but read by the renderer.** The `COMPOSITION.lock()`
  store sits at output_grid.rs:1375-1377, *three lines above* the `if !*COLLECT { return; }` gate, inside
  a function called `note_dcomp_stat` whose other 100 lines are pure logging. That store is the only
  source of the grid `paint_animation_sample_at` lands the animation sample on (`painter.rs:1366`).
  The placement is deliberate — the comment at :1372-1374 records that this exact trap was already
  sprung once (design doc C5).
- **`IDCompositionDevice::GetFrameStatistics` (`dcomp_compositor.rs:2583`) is the feed for that store.**
  `painter.rs:4281 note_dcomp_frame_statistics` reads `DCOMP_BIND_PROF || SAMPLE_LEAD_PERIODS.is_some()`
  precisely so the grid still fills when profiling is off. It looks like a statistics syscall; with B2
  enabled it is a rendering input.
- **`nominal_period_ticks` / `display_frequency_hz` / `EnumDisplaySettingsW` are the pacing period.**
  A display-mode OS query, but its result becomes `VblankSource::period_qpc`, which `wait()` hands to
  the pacing thread. `period_from_pair` (the measured-from-two-vblanks path) is the probe's and goes.
- **`is_primary` / `GetMonitorInfoW` picks the default pacing output** at `VblankSource::refresh`:1810.
  Its other three call sites, all in `emit_outphase`, are log fields.
- **`enumerate_outputs` is the clock's handle source**, not the probe's. Both use it; only the probe dies.
- **`lib.rs::note_present_tick` is two calls, not one.** The `wall_tick::note_tick_edge()` on the line
  above `output_grid::note_present_tick_now()` drives the external-present budget and the production
  edge. Delete only the second line.
- **`PumpSource` is load-bearing; only `PumpSource::note()` is instrumentation.** `may_schedule()` is what
  stops the `Tick` pump from feeding production back into itself (the fix measured at 79-690× runaway
  logical frames).
- **`servo::dwm_composition_grid` / `snap_to_dwm_grid_at` are not in this file.** The timer-fallback
  `DwmGetCompositionTimingInfo` the task warns about lives in `winit_wall/main.rs:1230-1250` and reaches
  DWM through `lib.rs:64 dwm_composition_grid()` → `dcomp_compositor::composition_grid()`. All six
  `dcomp_compositor::composition_grid()` call sites *inside output_grid.rs* are log-only; removing them
  does not touch that fallback.
- **Removing this file's emitters strands two things outside it:** `commit_guard::emit_lockwait`
  (output_grid.rs:2175 is its sole caller) and all of `components/paint/comp_stats.rs`
  (`emit_comp_stats` is its sole consumer). Both must be removed, or the build will warn on dead code.
- **`COLLECT` is `pub(crate)` and load-bearing *for compilation* in three other files** until their own
  gates are stripped. It must be the last thing deleted, not the first.
