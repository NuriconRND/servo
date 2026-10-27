# Inventory: script/ + media/ + layout/ + allocator/

Repository: `W:\servo_multigpu-tiled-wall`, branch `wall-strip-instrumentation`.
Read-only survey; no file was edited.

Scope as assigned: `components/script/`, `components/media/`,
`components/layout/display_list/paint_animation.rs`, `components/allocator/tracking.rs`.
Where a removable symbol's only caller lives just outside those files (e.g.
`components/layout/display_list/mod.rs`, `components/layout/display_list/stacking_context.rs`,
`components/script/microtask.rs`, `components/script/dom/webgl/webglrenderingcontext.rs`,
`components/media/backends/gstreamer/render-d3d11/lib.rs`), the caller is listed too, because
otherwise the tree will not compile after the removal.

Method: every candidate symbol was grepped across the whole `components/` tree and every
caller inspected. Provenance was established with `git log --follow` / `git log -S` /
`git blame`, and by whether the surrounding prose is this fork's Korean + star-marked
annotation style.

Verdict counts: **REMOVE 71**, **KEEP 26**, **UNCERTAIN 6**.

---

## Category 0 — Provenance findings that change the answer up front

| item | finding |
| --- | --- |
| `components/allocator/tracking.rs` (whole file, 1-223) | **UPSTREAM Servo.** `git log --follow` gives only upstream PRs: `f1a9ceed4f8` (#38727 "allocator: Add optional heap allocation measurement tracking"), `2e950dfbf0a` (#40270), `d2c78db981a` (#39202), `eca27e0d081` (#41066), `d808cc15d09` (#41635). No fork commit touches it. It is gated behind the non-default Cargo feature `allocation-tracking` (`components/allocator/Cargo.toml:17`, `components/allocator/lib.rs:11-20`) and is MallocSizeOf plumbing, nothing to do with the wall. **KEEP (upstream).** Its `note_allocation` is a name collision with this task's `note_*` pattern, not an instance of it. |
| `components/script/dom/node/node.rs` | Nothing to remove. All 93 "Korean" grep hits are curly apostrophes in upstream spec quotations. `note_dirty_descendants` / `note_node_with_dirty_descendants` are upstream restyle plumbing. |
| `components/script/script_runtime.rs` | Nothing to remove. `note_response_urls` is upstream (`9fb01e879ba`, StreamConsumer). |
| `components/script/serviceworker_manager.rs` | Nothing to remove. `note_worker_thread` is upstream (`ed688fe2c1f`). |
| `components/script/dom/document/document_event_handler.rs` | Nothing to remove. `note_pending_input_event` is upstream (`069ad408722` #38584). |
| `components/script/dom/html/htmliframeelement.rs` | Nothing to remove. Its `note_pending_navigation` is upstream (`f174c53feea` #40100); its Korean-commented code is the `<iframe toplevel>` **feature**, not measurement. |

---

## Category 1 — Mutexes / statics added only to hold statistics

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
| --- | --- | --- | --- | --- | --- |
| `CLOCKS: LazyLock<Mutex<HashMap<PipelineId, ClockWindow>>>` + `struct ClockWindow` | `components/script/raf_clock.rs:55-67` | static Mutex of sample vectors | **REMOVE** | Only reader/writer is `note_resolved` in the same file, whose only output is `log::warn!("RAFCLOCK …")`. | Fork-only file (`00427c49054`). |
| `FRAME_TIME_PROF: LazyLock<bool>` | `components/script/raf_clock.rs:35-38` | log gate (`SERVO_FRAME_TIME_PROF`) | **REMOVE** | Read once, in `note_resolved`, to early-return. | |
| `PERIOD_MS: LazyLock<f64>` | `components/script/raf_clock.rs:45-53` | log banding constant | **REMOVE** | Passed to `summarize_intervals` for the RAFCLOCK line only. | Duplicates `refresh_driver::paint_timer_period`; nothing reads it for timing. |
| `TASK_WINDOW: thread_local RefCell<TaskWindow>` + `struct TaskWindow` | `components/script/script_thread.rs:172-188` | per-second stats window | **REMOVE** | Only `ScriptThread::note_task_duration` (1255-1312) touches it; its only output is `SCRIPTTASK`/`SCRIPTBUSY` `warn!`. | |
| `slow_task_threshold()` + `SERVO_SCRIPT_SLOW_TASK_MS` | `components/script/script_thread.rs:190-201` | log gate | **REMOVE** | Only `note_task_duration`. | |
| `LAST_WEBGL_DOM_REPORT: thread_local Cell<Option<Instant>>` | `components/script/script_thread.rs:205-206` | log throttle | **REMOVE** | Only `maybe_report_live_webgl_contexts` (1321-1332). | |
| `LAST_FORCED_GC` + `force_gc_interval()` + `SERVO_SCRIPT_FORCE_GC_SEC` | `components/script/script_thread.rs:208-221` | diagnostic GC timer | **REMOVE** | Only `maybe_force_gc` (1335-1352), which calls `JS_GC` and logs `SCRIPTGC`. | Default off (env absent ⇒ `None`), so removal is a no-op at runtime; the doc comment at 1229-1237 says outright "not a mechanism to ship enabled". |
| `rendering_update_from_timer: Arc<AtomicBool>` | `components/script/script_thread.rs:477-486` (field), `1129` (init), `1212-1216` (set in timer callback), `1396-1398` (read) | diagnostic flag | **REMOVE** | Set by the fallback-timer callback; the single read at 1396 does nothing but call `note_rendering_update_reason("fallback-timer")`. Its own doc comment says `★진단용.★`. | The surrounding `trigger_script_thread_animation.store(true, …)` at 1216 is load-bearing — keep that line. |
| `RENDERING_UPDATE_REASONS: thread_local RefCell<Vec<(&'static str, u32)>>` | `components/script/dom/document/document.rs:329-334` | tally | **REMOVE** | Written by `note_rendering_update_reason`, drained by `take_rendering_update_reasons`; only consumer of the drain is the `SCRIPTBUSY` log line (`script_thread.rs:1292-1296`). | |
| `REFLOW_STATS: thread_local Cell<(u32,u32,Duration)>` + `struct ReflowTiming` + `impl Drop` | `components/script/dom/window.rs:484-516` | per-task reflow counters | **REMOVE** | Constructed once at `window.rs:2685` (`let _reflow_timing = ReflowTiming::start(&reflow_goal);`), drained by `take_reflow_stats` (528-530) whose only caller is `note_task_duration`. | `_reflow_timing` binding at 2685 must go with it. |
| `PORT_MESSAGE_STATS: thread_local Cell<(Duration,Duration)>` | `components/script/dom/globalscope.rs:750-755` | per-task port timing | **REMOVE** | Written by `note_port_message_deserialize`/`note_port_message_dispatch`, drained by `take_port_message_stats` (779-781) whose only caller is `note_task_duration`. | Fork commit `261d99c751d`. |
| `MICROTASK_TIME: thread_local Cell<Duration>` + `struct CheckpointTiming` + `impl Drop` | `components/script/microtask.rs:82-92` | per-task microtask timing | **REMOVE** | Constructed at `microtask.rs:138`; drained by `take_microtask_time` (105-107), called from `note_task_duration` (1258) and discarded at `script_thread.rs:1820` (`let _ = take_microtask_time();`). | Both call sites must go. |
| `LIVE_MEDIA_ELEMENTS: thread_local RefCell<Vec<WeakRef<HTMLMediaElement>>>` | `components/script/dom/html/htmlmediaelement.rs:1499-1506` | live-object census | **REMOVE** | Pushed by `register_for_reporting` (1550-1552, called once at 3146); read only by `report_live_media_elements` (1519-1546) → `MEDIADOM` warn. No other reader. | Whole triple removes cleanly. |
| `LIVE_CONTEXTS: thread_local RefCell<Vec<WeakRef<WebGLRenderingContext>>>` | `components/script/dom/webgl/webglrenderingcontext.rs:179-182` | live-object census | **KEEP** | Also read by `release_contexts_of_destroyed_document` (215-240 — frees GPU resources) and `find_live_context` (245-253). | **Trap.** See Traps: the `live.retain(|c| c.is_alive())` prune lives inside `report_live_contexts`; deleting that function alone leaks dead `WeakRef` entries. |
| `TALLY: thread_local RefCell<BindTally>` + `struct BindTally` + `fn tally` | `components/layout/display_list/paint_animation.rs:72-97` | bind-outcome counters | **REMOVE** | 13 `tally(...)` call sites (370, 421, 423, 431, 436, 457, 464, 468, 594, 597, 626, 630); drained only by `log_built` (500-543) → `PAINTANIM built` warn. | Field `transform_no_rect` is never incremented — already dead. |
| `UNBOUND_STREAK`, `BOUND_PREVIOUS`, `BOUND_CURRENT`, `EDGE_BUDGET_LOST`, `EDGE_BUDGET_GAINED`, `EDGE_BUDGET_FIRST` | `components/layout/display_list/paint_animation.rs:106-135` | edge-logging state + rate budgets | **REMOVE** | Written/read only by `note_bound`, `note_unbound`, `note_gained`, `note_unbound_transform_value`, `roll_binding_edges`, `edge_line_allowed` — all log-only. | |
| `MAX_EDGE_LINES_PER_SECOND`, `PROP_OPACITY`, `PROP_TRANSFORM`, `fn property_name` | `components/layout/display_list/paint_animation.rs:100-104, 137-143` | log formatting | **REMOVE** | Only the edge-logging functions above. | |
| `sample_diagnostics: Arc<Mutex<VideoSampleDiagnostics>>` | `components/media/backends/gstreamer/player.rs:2232`, cloned 2257, used 2293 | **a Mutex that exists only to hold statistics** | **REMOVE** | Only use is `sample_diagnostics.lock().unwrap().note_sample(&sample)` at 2293. | Textbook case of the user's category 1. |
| `struct VideoSampleDiagnostics` + `impl Default` + `NEXT_VIDEO_DIAGNOSTICS_ID: AtomicU64` | `components/media/backends/gstreamer/player.rs:144-180` | sample counters | **REMOVE** | Only `note_sample`/`note_rate`. | |
| `stage_prof: Arc<Mutex<Option<SinkStageProf>>>` + `struct SinkStageProf` + `impl SinkStageProf` + `prof_on` | `components/media/backends/gstreamer/player.rs:1554-1604`, `2238-2256`, and every `mark()/took()/p.* +=` pair at 2266-2326 | **a Mutex that exists only to hold statistics** | **REMOVE** | Only output is `SINKPROF` warn in `maybe_log` (1578-1603). | The `sink_pacer` Mutex right beside it at 2235 is **load-bearing** — see Traps. |
| `LockStats` + `REGISTRY_LOCK` + `REMOVED_LOCK` + `TrackedGuard` + `lock_tracked` + `SITE_*` arrays + `site()` + `take_top_registry_sites` | `components/media/player/d3d11_ring.rs:337-500` | lock wait/hold instrumentation | **REMOVE (with a replacement helper)** | `lock_at()`/`lock_registry()`/`lock_removed()` (501-509) wrap every registry acquisition — 25 `lock_at(` sites in this file. The only *consumer* of the numbers is `media-thread/lib.rs:1261-1305` (`MEDIALOCKRATE`, `MEDIALOCKSITES`). | **Trap.** `lock_tracked` also does poison recovery (`m.lock().unwrap_or_else(\|p\| p.into_inner())`). That must survive as a plain helper, otherwise a panicking thread permanently wedges video. |
| `MEDIA_D3D11_DEVICE_OWNERS: Mutex<Vec<(usize, Option<usize>)>>` + `note_media_d3d11_device` + `conflicting_device_owner` + `mod wall_gpu_fanout_tests` | `components/media/media-thread/lib.rs:740-800` and the `#[cfg(test)]` module at ~798+ | correctness alarm, not a perf meter | **UNCERTAIN** | Called once per painter at `lib.rs:864`. Output is a single `warn!` describing a broken multi-GPU fan-out. | It *is* a fork diagnostic and deleting it changes no pixel, but it is a **misconfiguration alarm**, not a performance measurement, and it has a unit test. Recommend keeping unless the user explicitly wants alarms gone too. |
| `D3D11_IMPORT_COUNT`, `D3D11_IMPORT_TOTAL_US`, `d3d11_profile_enabled()` | `components/media/media-thread/lib.rs:639-648` | counters | **REMOVE** | Only used in the `D3D11PROF wrap` warn at 1215-1228. | |
| `RAW_VIDEO_PLANE_LOCK_COUNT`, `RAW_VIDEO_PLANE_UNLOCK_COUNT`, `RAW_VIDEO_PLANE_INFO_INTERVAL` | `components/media/media-thread/lib.rs:39-42` | counters | **REMOVE** | Only incremented/read inside the `Wall raw video plane lock/unlock` debug+info blocks at 1425-1445 and 1508-1519. | |
| `LAST_CONSUME_SHAPE: thread_local RefCell<String>` | `components/media/media-thread/lib.rs:427-433`, written in `consume_plan` (435-470), cleared at 1152 | log string builder | **REMOVE** | Read only in the `MEDILOCK`/`MEDIALOCK` slow-lock warn at 1326-1333. | The `consume_plan` body around it is load-bearing; only the `LAST_CONSUME_SHAPE.with(...)` string-building block comes out. |
| `MEDIA_LOCK_SLOW_MS: LazyLock<f64>` + `SERVO_MEDIA_LOCK_SLOW_MS` | `components/media/media-thread/lib.rs:1383-1390` | log gate | **REMOVE** | Read at 1326 and 1478, both guarding `MEDIALOCK` warns. | |
| `lock_window_*` fields (`start/calls/consumes/total_ms/consume_ms/wrap_ms`) | `components/media/media-thread/lib.rs:730-735`, init 888-893, accumulated 1251-1259, reset 1319-1324 | per-second window | **REMOVE** | Only the `MEDIALOCKRATE` warn (1269-1289). | The `stage_consume_ms` / `stage_wrap_ms` / `wrap_cached` locals (1106-1108, 1156, 1182, 1226) exist only to feed these and the slow-lock line — they go too. |
| `diag_logged_wrap_ok`, `diag_logged_wrap_fail` | `components/media/media-thread/lib.rs:666-669`, init 875-876, used 1205-1213 / 1234-1242 | one-shot log latches | **REMOVE** | Only gate `WALLDIAG wrap OK/FAIL` info lines. | The `warn!("D3D11 media: EGLImage 래핑 실패 …")` at 1243 is an error report, not a metric — keep. |
| `PROFILE_ID_SEQ`, `LAST_PROF_LOG`, `FRAMES_SINCE_LOG`, `LAST_ARRIVAL`, `LAST_PTS_MS`, `profile_enabled()`, `profile_threshold_ms()`, `RenderD3D11::profile_id`, `fn prof_log` | `components/media/backends/gstreamer/render-d3d11/lib.rs:40-70, 126, 207-249` | whole `D3D11PROF` layer | **REMOVE** | Consumed only by the `D3D11PROF …` warns at 235, 295, 304, 399, 478 and the `prof` guards at 279-287, 397, 435-480. | `profile_id` is also interpolated into the non-prof `info!("D3D11 video: plane 링 생성 … (id={})")` at ~200 — that one line must be edited, not deleted. |
| `PlayerState::drop_count` | `components/media/backends/gstreamer/render-d3d11/lib.rs:115, 148, 173, 243, 389, 401, 475, 480` | counter | **REMOVE** | Incremented at 389/475; read only by `D3D11PROF` lines at 243, 401, 480. | |
| `RingState::dropped_frames` + `D3d11PlaneRings::dropped_frames()` | `components/media/player/d3d11_ring.rs:275, 570, 629, 699-704` | counter | **UNCERTAIN** | Read only by `render-d3d11/lib.rs:243` (`D3D11PROF`), **and** asserted by unit tests at `d3d11_ring.rs:1507` and `1516`. | Removing it means deleting those two assertions (and possibly the tests). Not on the `servo-paint` build gate, but it is a real test deletion — confirm with the user. |
| `D3D11_TEXTURES_CREATED: AtomicU64` | `components/media/player/d3d11_ring.rs:52`, incremented `render-d3d11/ring_producer.rs:250` | counter | **REMOVE** | Read only by the `MEDIARINGS` warn (`media-thread/lib.rs:1313`). | Its partner `paint_api::rendering_context::D3D11_TEXTURES_RELEASED` (`components/shared/paint/rendering_context.rs:64, 1186`) is in another subsystem but has the same single reader — flag it to whoever owns `shared/paint`. |
| `mod live_counts` (`PLAYERS`, `PLAYER_INNERS`, `player_created/dropped`, `inner_created/dropped`, `snapshot`) | `components/media/player/lib.rs:24-50` | live-object census | **REMOVE** | `snapshot()` read only at `media-thread/lib.rs:1295` (`MEDIAPLAYERS` warn). The four mutators are called from the player lifecycle in `backends/gstreamer/player.rs` purely to feed it. | |
| `MediaFrameRenderer::rendered_frame_count`, `last_rendered_frame_at`, `media_frame_summary_count`, `media_frame_summary_at` | `components/script/dom/html/htmlmediaelement.rs:388-390, 421-422, 502-503` | frame counters | **REMOVE** | Fed into `log_media_frame` (621-679) only; `rendered_frame_count` is also the modulo in the summary throttle at 652. Nothing outside the log reads them. | |

---

## Category 2 — `note_*` / `emit_*` and per-1-second window aggregators

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
| --- | --- | --- | --- | --- | --- |
| `raf_clock::note_resolved` | `components/script/raf_clock.rs:74-134` | 1 s window + `RAFCLOCK`/`RAFCLOCKSEQ` warn | **REMOVE** | One caller: `Document::resolve_frame_time` (`document.rs:4615-4620`). Return value `()`. | Removing it also removes `let from_frame_time = …` (4596-4597) and `let clamped = …` (4609) in `resolve_frame_time`, which exist only as its arguments. **The rest of `resolve_frame_time` is load-bearing — see Traps.** |
| `mod raf_clock;` declaration | `components/script/lib.rs:53` | module decl | **REMOVE** | — | Delete together with the file. |
| `ScriptThread::note_task_duration` | `components/script/script_thread.rs:1240-1312` | 1 s window, `SCRIPTTASK`/`SCRIPTBUSY` warn | **REMOVE** | One caller: `script_thread.rs:2038` `Self::note_task_duration(category, task_duration);`. Return `()`. | `task_duration` itself (1884 `let start`, 2037 `let elapsed`) is **KEEP** — the upstream TTI block at 2039-2058 reads it (`> MAX_TASK_NS`, `doc.start_tti()`). |
| `ScriptThread::maybe_report_live_webgl_contexts` | `components/script/script_thread.rs:1314-1332` | 1 s throttle | **REMOVE** | One caller: `update_the_rendering` (`1379`). | Calls `report_live_contexts()` and `report_live_media_elements()`. |
| `ScriptThread::maybe_force_gc` | `components/script/script_thread.rs:1229-1352` | diagnostic forced GC | **REMOVE** | One caller: `update_the_rendering` (`1378`). | Default-off. |
| `note_rendering_update_reason` | `components/script/dom/document/document.rs:336-344` | tally bump | **REMOVE** | 11 call sites, all discarding the return: `document.rs:2998, 3011, 3015, 3020, 3024, 3028, 3032, 3036, 3040`; `script_thread.rs:1397, 2222`. | Each call sits on its own statement line directly before a `return true;` / inside a match arm — deleting the line leaves the branch intact. Re-exported through `dom/document/mod.rs:15` (`pub(crate) use self::document::*;`), imported at `script_thread.rs:134`. |
| `take_rendering_update_reasons` | `components/script/dom/document/document.rs:346-356` | tally drain | **REMOVE** | One caller: `script_thread.rs:1292-1296`, formats into the `SCRIPTBUSY` `update_reasons=[…]` field. | |
| `Document::needs_rendering_update` | `components/script/dom/document/document.rs:2979-3045` | decision function | **KEEP** | Called by the rendering-update scheduler. | Only the 9 `note_rendering_update_reason(...)` lines inside it come out; every `return true/false` stays. The big `restyle_reason` match at 2998-3005 exists **only** to pick a log string — delete the whole `note_rendering_update_reason(match …);` statement, keep `return true;`. |
| `take_reflow_stats` | `components/script/dom/window.rs:518-530` | drain | **REMOVE** | One caller: `note_task_duration` (1256). | |
| `take_port_message_stats` | `components/script/dom/globalscope.rs:770-781` | drain | **REMOVE** | One caller: `note_task_duration` (1257). | |
| `note_port_message_deserialize` / `note_port_message_dispatch` | `components/script/dom/globalscope.rs:757-768` | timing bumps | **REMOVE** | Two call sites: 1584 and 1620, plus the `Instant::now()` locals at 1581 and 1610. | The `structuredclone::read` / `MessageEvent::dispatch_jsval` calls they bracket are load-bearing — keep those. |
| `take_microtask_time` | `components/script/microtask.rs:94-107` | drain | **REMOVE** | Two callers: `note_task_duration` (1258) and the discard at `script_thread.rs:1820` (`let _ = take_microtask_time();`). | |
| `report_live_contexts` | `components/script/dom/webgl/webglrenderingcontext.rs:252-336` | `WEBGLDOM` warn | **REMOVE** | One caller: `maybe_report_live_webgl_contexts` (`script_thread.rs:1330`, imported at 144). | **Carries the `LIVE_CONTEXTS` prune.** See Traps. |
| `report_live_media_elements` | `components/script/dom/html/htmlmediaelement.rs:1519-1546` | `MEDIADOM` warn | **REMOVE** | One caller: `maybe_report_live_webgl_contexts` (`script_thread.rs:1331`). | |
| `HTMLMediaElement::register_for_reporting` | `components/script/dom/html/htmlmediaelement.rs:1549-1552` | registration | **REMOVE** | One caller: `htmlmediaelement.rs:3146`. | |
| `MediaFrameRenderer::note_media_frame_rendered` | `components/script/dom/html/htmlmediaelement.rs:610-619` | inter-frame ms | **REMOVE** | One caller: `render()` at 1105 (`let inter_frame_ms = …`), which forwards it only to `log_media_frame`. | Fork commit `be2148a68cb`. |
| `MediaFrameRenderer::log_media_frame` | `components/script/dom/html/htmlmediaelement.rs:621-679` | `Wall media frame` debug + 1-per-120 info summary | **REMOVE** | Three call sites: 854-863, 1075-1084, 1299-1308. | Each is a standalone statement. Removing it also orphans `image_update_for_log` (728, 777, 821, 945, 996, 1040, 1130, 1187, 1213, 1257) and `delete_update_count` (850, 1071, 1295) — all pure log plumbing. |
| `note_bound` / `note_unbound` / `note_gained` / `roll_binding_edges` / `edge_line_allowed` / `describe_animations` | `components/layout/display_list/paint_animation.rs:214-218, 228-285, 288-303, 351-354, 146-162, 171-211` | `PAINTANIMEDGE` warns | **REMOVE** | Internal only: call sites at 432, 437, 458, 465, 469, 470, 598, 616, 627, 631, 632, 507. All return `()`. | |
| `note_unbound_transform_value` | `components/layout/display_list/paint_animation.rs:305-348` | `PAINTANIMSTATIC` warn | **REMOVE** | One caller: `components/layout/display_list/stacking_context.rs:676-692`, inside `if animated_transform.is_none() { … }`. Return `()`. | The whole `if animated_transform.is_none() { … }` block goes; the `let style = self.style();` at 647 is still used at 695 and later, so keep it. |
| `note_stacking_context_skipped` | `components/layout/display_list/paint_animation.rs:356-372` | tally bump | **REMOVE** | One caller: `components/layout/display_list/mod.rs:556-559`, immediately before `return false;`. | Keep the `return false;`. |
| `log_built` | `components/layout/display_list/paint_animation.rs:494-543` | `PAINTANIM built` warn | **REMOVE** | One caller: `components/layout/display_list/mod.rs:332`. | **Careful:** this function also calls `roll_binding_edges()` unconditionally (507). That is fine *only because* the edge state it rolls is itself being removed — remove them in the same commit. |
| `log_document_animations` | `components/layout/display_list/paint_animation.rs:656-722` | 1 s `PAINTANIM document` warn | **REMOVE** | One caller: `components/layout/display_list/mod.rs:333-336`. | Walks the whole document animation set once a second — real cost, not just a log line. |
| `VideoSampleDiagnostics::note_rate` | `components/media/backends/gstreamer/player.rs:616-666` | 1 s `VIDEORATE` info | **REMOVE** | One caller: `note_sample` (682). | Gated by `video_rate_logging_enabled()` (`SERVO_MEDIA_VIDEO_RATE`). |
| `VideoSampleDiagnostics::note_sample` | `components/media/backends/gstreamer/player.rs:669-752` | per-sample debug + summary | **REMOVE** | One caller: `player.rs:2293`. | |
| `video_rate_logging_enabled`, `clock_time_ms`, `clock_delta_ms`, `format_optional_ms`, `VIDEO_SAMPLE_INFO_INTERVAL`, `VIDEO_SAMPLE_LATE_GAP_MS`, `VIDEO_RATE_REPORT_INTERVAL` | `components/media/backends/gstreamer/player.rs:75-76, 142, 182-205, 506-519` | helpers for the above | **REMOVE** | Used only inside `note_rate`/`note_sample`. | |
| `SinkStageProf::maybe_log` | `components/media/backends/gstreamer/player.rs:1575-1604` | 1 s `SINKPROF` warn | **REMOVE** | One caller: `player.rs:2325`. | |
| `RenderD3D11::prof_log` | `components/media/backends/gstreamer/render-d3d11/lib.rs:207-249` | 1 s `D3D11PROF` heartbeat | **REMOVE** | Called from the guarded `if prof { … }` blocks at 448 and nearby. | |
| `MEDIALOCKRATE` / `MEDIALOCKSITES` / `MEDIARINGS` / `MEDIAPLAYERS` emit block | `components/media/media-thread/lib.rs:1260-1325` | 1 s window emit | **REMOVE** | Nothing reads it; it only prints. | Largest single contiguous removal in `media/`. |
| `MEDIALOCK` slow-lock warn | `components/media/media-thread/lib.rs:1326-1334` and `1478-1484` | threshold warn | **REMOVE** | — | |
| `ring_inventory`, `removed_queue_inventory`, `group_inventory` | `components/media/player/d3d11_ring.rs:65-85` | snapshot getters | **REMOVE** | Only callers are `media-thread/lib.rs:1306-1311` (`MEDIARINGS`). | Also drop their `SITE_NAMES` entries ("group_inventory", "ring_inventory"). |
| `note_plane_lock_and_plan`, `note_plane_unlock`, `note_demand`, `note_demand_and_ring` | `components/media/player/d3d11_ring.rs:770-787, 1049-1056, 1083-1088` | **load-bearing despite the `note_` prefix** | **KEEP** | `note_plane_lock_and_plan` returns a `ConsumePlan` and mutates slot state under a never-skip commit contract; `note_plane_unlock` decrements `lock_count` and resets the compositing gate; `note_demand*` record consumer demand that drives ring allocation and return the ring id. | **Trap.** Only the `lock_at(…, "name")` wrapper inside them changes. |
| `Animations::update_for_new_timeline_value`, `start_pending_animations`, `do_post_reflow_update` | `components/script/animations.rs:157-180, 416-470, 253-...` | animation state machine | **KEEP** | — | Only the embedded `log::warn!` calls come out — see Category 4. |
| `Window::emit_timeline_marker` / `need_emit_timeline_marker` | `components/script/dom/window.rs:3396-3410`, callers 2707, 2778, `dom/event/event.rs` | upstream devtools timeline | **KEEP (upstream)** | Devtools marker plumbing, predates the fork. | Name matches the user's `emit_*` pattern but is not wall instrumentation. |
| `GlobalScope::note_cross_realm_transform_readable` / `_writable` | `components/script/dom/globalscope.rs:1466-1500` | upstream Streams spec step | **KEEP (upstream)** | Called from `readablestream.rs`/`writablestream.rs`; installs a handler. | |
| `Document::note_node_with_dirty_descendants`, `Node::note_dirty_descendants` | `components/script/dom/document/document.rs:742-…`, `components/script/dom/node/node.rs:469, 511, 825-826` | upstream restyle plumbing | **KEEP (upstream)** | | |
| `DocumentEventHandler::note_pending_input_event` | `components/script/dom/document/document_event_handler.rs:238-…` | upstream | **KEEP (upstream)** | `script_thread.rs:4044`. | |
| `ServiceWorkerManager::note_worker_thread` | `components/script/serviceworker_manager.rs:176, 694` | upstream | **KEEP (upstream)** | | |
| `note_response_urls` | `components/script/script_runtime.rs:1321` | upstream | **KEEP (upstream)** | | |
| `HTMLFormElement`→`HTMLIFrameElement::note_pending_navigation` | `components/script/dom/html/htmliframeelement.rs:763` | upstream | **KEEP (upstream)** | | |
| `AccountingAlloc::note_allocation` | `components/allocator/tracking.rs:143-152` | upstream | **KEEP (upstream)** | `lib.rs:74` (`enclosing_size_impl`). | |

---

## Category 3 — DWM / display-related OS syscalls called for statistics

**No items in this subsystem.** A tree-wide grep over `components/script/`, `components/media/`,
`components/layout/`, `components/allocator/` for `Dwm`, `DXGI`, `QueryDisplayConfig`,
`GetDeviceCaps`, `WaitForVBlank`, `D3DKMT`, `QueryPerformanceCounter`, `timeBeginPeriod` returns
only D3D11 pixel-format constants in `render-d3d11/ring_producer.rs` and desktop-duplication
capture code in `media_capture.rs` — neither is measurement. All DWM/vblank instrumentation lives
in `components/paint/` and `ports/winit_wall/`, outside this subsystem.

The one Win32-API-for-diagnostics item in reach is below.

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
| --- | --- | --- | --- | --- | --- |
| `thread_name::tag_video_streaming_thread` / `tag_audio_streaming_thread` (`SetThreadDescription`, `GetCurrentThreadId`, `THREADMAP` info line) | `components/media/backends/gstreamer/thread_name.rs:21-128` | OS calls so a CPU sampler can attribute time | **UNCERTAIN** | Callers: `player.rs:2265` (video sink callback) and `player.rs:2441` (audio). | **Trap.** The same function also calls `pin_to_node(index)` (NUMA pinning via `SetThreadGroupAffinity`, gated by pref `media_numa_pin_streaming_threads`), and the round-robin `index` comes from the *naming* counter. Pinning is a performance **control**, not a measurement. Removing the naming/`THREADMAP` parts requires keeping the per-thread index counter and the latch. Do not delete this module wholesale. |

---

## Category 4 — Other measurement facilities added for winit_wall evaluation

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
| --- | --- | --- | --- | --- | --- |
| `components/script/raf_clock.rs` (entire file, 1-134) | — | fork-only measurement module | **REMOVE** | Sole entry point `note_resolved`; sole caller `Document::resolve_frame_time`. | Added by the fork in `00427c49054`. Its only external dependency is `servo_base::interval_summary::summarize_intervals` — see "cross-subsystem" below. |
| `log::warn!("ANIMUNBIND …")` | `components/script/animations.rs:196-211` | log | **REMOVE** | Inside the `cancel_animations_for` closure in `cancel_animations_for_node`. | `set.cancel_all_animations()` below it is load-bearing. |
| `log::warn!("ANIMDROP …")` | `components/script/animations.rs:280-296` | log | **REMOVE** | Inside `do_post_reflow_update`. | `set.cancel_all_animations()` below it is load-bearing. |
| `log::warn!("ANIMSETDROP …")` | `components/script/animations.rs:363-367` | log | **REMOVE** | Inside the `sets.retain(...)` closure. | **Trap.** The `if let Some(node) = rooted_nodes.get(...) { node.dirty(NodeDamage::Style); }` in the same closure is a **later bug fix**, not part of the logging commit (`30cefa2035e2` added only the warn; the dirty came after). Keep it, keep `rooted_nodes`, keep the closure shape. |
| `log::warn!("ANIMPENDING …")` and `log::warn!("ANIMSTART …")` | `components/script/animations.rs:436-453` | logs | **REMOVE** | Inside `start_pending_animations`. | **Trap.** `git show 30cefa2035e2` proves the surrounding restructure (`if state != Pending { continue; }` … `if started_at > now { continue; }`) is **behaviour-identical to upstream** — the commit only added the two warns. Delete the warns, leave the control flow (or restore the upstream two-line form); do not delete the `continue;`. |
| `log::warn!("MEDIATEARDOWN element detached; releasing its player")` | `components/script/dom/html/htmlmediaelement.rs:3272` | log | **REMOVE** | One line inside `release_player_while_detached`. | **Trap.** `release_player_while_detached` itself (3252-3276) is a load-bearing leak fix. Remove only the one `warn!`. |
| `warn!("WEBGLDOM releasing {} context(s) …")` | `components/script/dom/webgl/webglrenderingcontext.rs:230-233` | log | **REMOVE** | Inside `release_contexts_of_destroyed_document`. | The surrounding GPU-release loop is load-bearing. |
| `warn!("SCRIPTGC forcing a full collection …")` | `components/script/script_thread.rs:1347` | log | **REMOVE** | Goes with `maybe_force_gc`. | |
| `WALLDIAG consumer-device publish` info | `components/media/media-thread/lib.rs:853-862` | log | **REMOVE** | Inside `MediaExternalImages::new`. | `D3d11PlaneRings::set_consumer_device(device)` just below is load-bearing. |
| `WALLDIAG wrap OK` / `WALLDIAG wrap FAIL` info | `components/media/media-thread/lib.rs:1203-1213`, `1232-1242` | logs | **REMOVE** | Guarded by `diag_logged_wrap_*`. | |
| `Wall raw video plane lock/unlock` debug + info blocks | `components/media/media-thread/lib.rs:1425-1446`, `1508-1520` | logs | **REMOVE** | Nothing reads them. | The `warn!("Slow raw plane unlock: …")` at 1502-1507 is also measurement — remove it and the `unlock_start`/`unlock_ms` locals. |
| `D3D11PROF wrap` warn | `components/media/media-thread/lib.rs:1215-1228` and `import_start` local at 1203 | log | **REMOVE** | — | `import_start` also feeds `stage_wrap_ms` (1226), itself removable. |
| `D3D11PROF wrap/arrgap/mapfail/drop` warns + `prof` guards | `components/media/backends/gstreamer/render-d3d11/lib.rs:279-310, 397-403, 435-482` | logs + timing locals | **REMOVE** | — | `claim_start`, `copy_start`, `pub_start`, `t_claim`, `t_copy`, `t_publish`, `bf_start` are all log-only locals. |
| `VideoSinkPacing` / `SinkPacer` / `sleep_before` | `components/media/backends/gstreamer/player.rs:2234-2235, 2277-2290`; `components/media/backends/gstreamer/render.rs` | **playback pacing** | **KEEP** | `sink_pacer.lock().unwrap().sleep_before(pts)` actually sleeps the streaming thread to hold 1x playback. | **Trap.** It is a `Arc<Mutex<…>>` created two lines from the statistics mutex and is gated by a pref, but deleting it makes every video decode at decoder speed. |
| `PaintAnimation` sampling: `horizon_seconds`, `MAX_SAMPLES`, `SAMPLE_SECONDS`, `EPSILON`, `animated_opacity`, `animated_transform_list`, `opacity_binding`, `transform_binding` | `components/layout/display_list/paint_animation.rs:38-63, 374-400, 402-492, 545-577, 579-654` | **computes what is drawn** | **KEEP** | `mod.rs:574-587` pushes the binding into the display list; `stacking_context.rs:660-674` pushes the transform animation. | **Trap.** These sit in the same file as the removals and share the `tally()` calls; strip the `tally(...)`/`note_*(...)` statements out of their bodies and leave every `return`/`Some(...)` path exactly as-is. |
| `Document::resolve_frame_time`, `Document::animation_frame_time`, `last_frame_time`, `pref!(dom_animation_timeline_use_frame_time)` | `components/script/dom/document/document.rs:4570-4625` | **product feature** (rAF timestamp) | **KEEP** | `update_the_rendering` path. | **Trap.** Only the `note_resolved(...)` call and the two locals that exist solely as its arguments come out. The monotonic clamp at 4610-4613 is behaviour, not measurement. |
| `ScriptThread::pending_frame_time`, `restore_pending_frame_time` | `components/script/script_thread.rs:470-475, 1354-1371` | **product feature** | **KEEP** | `TickAllAnimations` (2221-2228) and `update_the_rendering` (1388-…). | Korean-commented fork code, but it is the frame-time plumbing, not a meter. |
| `components/script/animations.rs::start_pending_animations_before_reflow` + `window.rs:2742-2755` call | `components/script/animations.rs:242-252`, `components/script/dom/window.rs:2742-2756` | **bug fix** | **KEEP** | Promotes `Pending` animations before the display list is built. | Korean-commented fork addition; changes what is drawn. |
| `GStreamerPlayer` thread-pinning pref path | `components/media/backends/gstreamer/thread_name.rs:69-128` | perf control | **KEEP** | See Category 3 row. | |
| `components/allocator/tracking.rs` (all) and `components/allocator/lib.rs:11-54` | — | upstream | **KEEP (upstream)** | — | |

---

## Removal order and build-breakage notes

The tree compiles after each numbered step if the step is applied whole. Steps are ordered
leaves-first so no step leaves a dangling reference.

1. **`layout` paint-animation logging** (one commit).
   - `paint_animation.rs`: delete 72-97 (`BindTally`/`TALLY`/`tally`), 100-104, 106-135, 137-143,
     146-162, 171-211, 214-218, 228-285, 288-303, 305-348, 351-354, 356-372, 494-543, 656-722.
   - In the same edit, delete every `tally(...)`, `note_unbound(...)`, `note_gained(...)`,
     `note_bound(...)` statement inside `opacity_binding` and `transform_binding`
     (lines 421, 423, 431, 432, 436, 437, 457, 458, 464, 465, 468, 469, 470, 594, 597, 598,
     616, 626, 627, 630, 631, 632). Note line 616 sits inside a `None => { … return None; }`
     arm — keep the `return None;`.
   - `display_list/mod.rs`: delete 332-336 and 553-560 (keep `return false;`).
   - `display_list/stacking_context.rs`: delete 675-693 (`if animated_transform.is_none() { … }`).
   - `use std::cell::RefCell;` at `paint_animation.rs:31` becomes unused — remove it.
   - Build check: `cargo check -p layout`.

2. **`raf_clock`** (one commit).
   - Delete `components/script/raf_clock.rs` and `components/script/lib.rs:53`.
   - `document.rs:4596-4597` (`let from_frame_time = …`), `4609` (`let clamped = …`) and
     `4615-4620` (the `note_resolved` call) go together. Nothing else changes in
     `resolve_frame_time`.
   - `servo_base::interval_summary::summarize_intervals` loses its script-side user but is still
     used by `components/paint/painter.rs` — **do not delete it from this subsystem.**

3. **`SCRIPTTASK`/`SCRIPTBUSY` and its four feeders** (must be one commit — they are mutually
   dependent).
   - `script_thread.rs`: delete 172-201 (`TASK_WINDOW`, `TaskWindow`, `slow_task_threshold`),
     1240-1312 (`note_task_duration`), 2038 (the call), 1820 (`let _ = take_microtask_time();`).
   - `window.rs`: delete 484-530 (`REFLOW_STATS`, `ReflowTiming`, `Drop`, `take_reflow_stats`)
     and 2685 (`let _reflow_timing = …`).
   - `globalscope.rs`: delete 750-781 and the four lines 1581, 1584, 1610, 1620.
   - `microtask.rs`: delete 82-107 and 138.
   - `document.rs`: delete 329-356 (`RENDERING_UPDATE_REASONS`,
     `note_rendering_update_reason`, `take_rendering_update_reasons`) and the 9 call lines
     2998-3005 (whole statement), 3011, 3015, 3020, 3024, 3028, 3032, 3036, 3040.
   - `script_thread.rs`: delete 1397-1398, 2222, 1212, 1215, and the field/init/read of
     `rendering_update_from_timer` (477-486, 1129, 1396-1398). Keep line 1216
     (`trigger_script_thread_animation.store(true, …)`).
   - Imports to prune: `script_thread.rs:134` (`note_rendering_update_reason,
     take_rendering_update_reasons`), `:139` (`take_port_message_stats`), `:147`
     (`take_reflow_stats`), `:156` (`take_microtask_time`).
   - If this is split into smaller commits, `note_task_duration` must be the **first** thing
     deleted; it is the only reader of all four feeders.

4. **Live-object censuses** (one commit).
   - `script_thread.rs`: delete 205-206, 1314-1332 (`maybe_report_live_webgl_contexts`), 1379,
     and import 144.
   - `webglrenderingcontext.rs`: delete 252-336 (`report_live_contexts`) **and** move the
     `live.retain(|context| context.is_alive())` prune into
     `release_contexts_of_destroyed_document` or into `WebGLRenderingContext::new_inherited`'s
     registration — see Traps. Also delete the `warn!("WEBGLDOM releasing …")` at 230-233.
   - `htmlmediaelement.rs`: delete 1499-1552 (`LIVE_MEDIA_ELEMENTS`,
     `report_live_media_elements`, `register_for_reporting`) and the call at 3146.

5. **Forced-GC diagnostic** (one commit, independent).
   - `script_thread.rs`: delete 208-221, 1229-1352 (`maybe_force_gc`), 1378.

6. **`animations.rs` warn lines** (one commit, independent).
   - Delete the warn statements at 196-211, 280-296, 363-367, 436-445, 447-453.
   - Keep `set.cancel_all_animations()`, `node.dirty(NodeDamage::Style)`, both `continue;`,
     and `animation.state = AnimationState::Running;`.
   - `rooted_nodes` binding at ~358 stays (the `dirty` call needs it).

7. **`htmlmediaelement` media-frame logging** (one commit).
   - Delete 388-390 (counter fields), 421-422, 502-503, 610-619
     (`note_media_frame_rendered`), 621-679 (`log_media_frame`), 130
     (`MEDIA_FRAME_INFO_INTERVAL`).
   - Delete the three `self.log_media_frame(...)` statements (854-863, 1075-1084, 1299-1308)
     and then the now-unused `image_update_for_log` assignments (728, 777, 821, 945, 996,
     1040, 1130, 1187, 1213, 1257) and `delete_update_count` bindings (850-853, 1071-1074,
     1295-1298) and `inter_frame_ms` (1105).
   - **Caution:** `rendered_frame_count` is incremented at 1103 and read at 1104; both go,
     but confirm no `#[derive]`/serialization touches the field first (none found).
   - Delete `warn!("MEDIATEARDOWN …")` at 3272 only.

8. **`media/` gstreamer sink diagnostics** (one commit).
   - `player.rs`: delete 75-76, 142-180, 182-205, 506-519, 616-752, 1554-1604, and in
     `install_video_sink_callbacks` delete 2232, 2238-2256 (`prof_on`, `stage_prof`), 2257
     (the `sample_diagnostics` clone), 2266-2270, 2277-2278, 2291-2295, 2298-2301,
     2307-2311, 2320-2326. Keep 2234-2235 and 2279-2290 (`sink_pacing` / `sink_pacer` /
     `sleep_before`) and the `render`/`weak_video_renderer`/`observer` plumbing.
   - `mark`/`took` closures (2267-2268) become unused — delete.

9. **`media/` D3D11PROF** (one commit).
   - `render-d3d11/lib.rs`: delete 40-70, 115 + 148 + 173 (`drop_count` field/init/reset),
     126 + the `profile_id` field, 207-249 (`prof_log`), 279-310, 389, 397-403, 435-482
     (keeping the non-prof body of those blocks).
   - Edit the `info!("D3D11 video: plane 링 생성 … (id={})")` line (~196-202) to drop the
     `(id={})` field and the `self.profile_id` argument — **do not delete the whole line.**
   - `ring_producer.rs`: delete 250 (`D3D11_TEXTURES_CREATED.fetch_add`) and the import at 21.
   - `d3d11_ring.rs`: delete 52 (`D3D11_TEXTURES_CREATED`).

10. **`media-thread` MEDIALOCK family** (one commit; depends on step 11 only for `LockStats`).
    - `media-thread/lib.rs`: delete 39-42, 427-470 (the `LAST_CONSUME_SHAPE` block inside
      `consume_plan` — keep the rest of `consume_plan`), 639-648, 666-669 + 875-876,
      730-735 + 888-893, 853-862, 1106-1108 (`stage_consume_ms`/`stage_wrap_ms`/`wrap_cached`),
      1152, 1156, 1182, 1203 (`import_start`), 1205-1228, 1226, 1232-1242, 1251-1259,
      1260-1334, 1383-1390, 1425-1446, 1478-1484, 1500-1520.
    - Keep `note_demand_and_ring`, `plan_or_presenting_plane`, `consume_plan`,
      `presenting_plane`, the wrap cache, the ring/wrap budgets, and
      `warn!("D3D11 media: EGLImage 래핑 실패 …")`.
    - `player/lib.rs`: delete `mod live_counts` (24-50) and its four call sites in
      `backends/gstreamer/player.rs` (search `live_counts::`).

11. **`d3d11_ring` lock instrumentation** (one commit, LAST in `media/`).
    - Delete 337-500 (`LockStats`, `REGISTRY_LOCK`, `REMOVED_LOCK`, `TrackedGuard`,
      `lock_tracked`, `take_top_registry_sites`, `SITE_COUNT`, `SITE_NAMES`, `SITE_WAIT_NS`,
      `SITE_HOLD_NS`, `SITE_HITS`, `site`), 65-85 (`removed_queue_inventory`,
      `group_inventory`, `ring_inventory`).
    - Replace `lock_at(m, "name")`, `lock_registry(m)`, `lock_removed(m)` (501-509) with a
      single `fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> { m.lock().unwrap_or_else(|p| p.into_inner()) }`
      and rewrite all **25** `lock_at(` sites to `lock_recover(`. Losing the poison recovery is
      a real regression, not a cleanup.
    - `dropped_frames` (275, 570, 629, 699-704) only if the two unit-test assertions at 1507
      and 1516 are also removed — see UNCERTAIN.

### Test files

No test in this subsystem exists solely to exercise instrumentation. Two tests *touch* removable
state:

- `components/media/player/d3d11_ring.rs:1507, 1516` — `assert_eq!(D3d11PlaneRings::dropped_frames(ring_id), 1/2)`.
  These assert a **behaviour** (frames are dropped when the ring is full) through an
  instrumentation counter. Keep the counter, or rewrite the assertions, rather than delete the
  tests.
- `components/media/media-thread/lib.rs` `mod wall_gpu_fanout_tests` (`#[cfg(test)]`, just after
  `conflicting_device_owner`) — exercises the GPU fan-out alarm. Removed only if that alarm is
  removed (UNCERTAIN).

The stated build gate `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu` does not
compile any crate in this subsystem's removal set except transitively; after each step also run
`cargo check -p layout -p script -p servo-media -p servo-media-thread -p servo-media-player`.

### Cross-subsystem hand-offs (do not delete from here)

- `servo_base::interval_summary::summarize_intervals` (`components/shared/base/interval_summary.rs`)
  — loses `raf_clock` as a consumer but `components/paint/painter.rs` still uses it.
- `servo_config::debug_env::{FRAME_TIME_PROF, MEDIA_SINK_PROF, MEDIA_VIDEO_RATE, D3D11_PROFILE}`
  (`components/config/debug_env.rs`, incl. the comment block at 344-349) — registrations become
  dead once the readers here go.
- `paint_api::rendering_context::D3D11_TEXTURES_RELEASED`
  (`components/shared/paint/rendering_context.rs:64, 1186`) — its only reader is the `MEDIARINGS`
  line removed in step 10.
- Doc-comment references to removed tags in `components/paint/frame_time.rs:102`,
  `components/paint/refresh_driver.rs:510`, `components/config/prefs.rs:124` — stale prose only.

---

## Traps

- **`Document::resolve_frame_time` (`document.rs:4570-4625`) is a product feature, not a meter.**
  The monotonic clamp, the `last_frame_time` store and the `dom_animation_timeline_use_frame_time`
  pref all decide the timestamp the page's rAF callback receives. Only `note_resolved(...)` and
  the two locals feeding it come out.
- **`d3d11_ring`'s `note_plane_lock_and_plan` / `note_plane_unlock` / `note_demand` /
  `note_demand_and_ring` are load-bearing** despite matching the `note_*` pattern exactly. The
  first returns a `ConsumePlan` under a never-skip commit contract — skipping it wedges a ring
  slot permanently. The others drive demand-based ring allocation and the compositing gate.
- **`lock_tracked` carries the mutex poison recovery.** Deleting the lock-statistics layer without
  replacing `m.lock().unwrap_or_else(|p| p.into_inner())` turns one panicking thread into a
  permanent video stall across the wall.
- **`report_live_contexts` carries the `LIVE_CONTEXTS` prune.** The
  `live.retain(|context| context.is_alive())` at `webglrenderingcontext.rs:254` is the only thing
  that drops dead `WeakRef` entries. `LIVE_CONTEXTS` itself must stay (it feeds
  `release_contexts_of_destroyed_document`, which frees GPU memory). Move the retain before
  deleting the reporter.
- **`SinkPacer` / `VideoSinkPacing::sleep_before` (`player.rs:2234-2235, 2279-2290`) is playback
  pacing**, created two lines away from the statistics mutex and wrapped in the same
  `Arc<Mutex<…>>` idiom. Deleting it makes every video decode as fast as the decoder allows.
- **The `start_pending_animations` restructure in `animations.rs:430-460` is behaviour-identical
  to upstream.** `git show 30cefa2035e2` shows the commit added only two `warn!` calls; the
  `if state != Pending { continue; }` / `if started_at > now { continue; }` shape is a rewrite of
  the original condition. Delete the warns, keep both `continue;`.
- **`node.dirty(NodeDamage::Style)` inside the `sets.retain` closure (`animations.rs:369-371`) is
  a later bug fix**, not part of the `ANIMSETDROP` logging commit. Keep it and the `rooted_nodes`
  borrow it needs.
- **`release_player_while_detached` (`htmlmediaelement.rs:3252-3276`) is a leak fix**, gated by
  `pref!(media_release_detached_player)`. Only its single `MEDIATEARDOWN` warn is instrumentation.
- **`thread_name.rs` mixes measurement with a NUMA pinning control.** The naming/`THREADMAP`
  half is instrumentation; `pin_to_node` is a performance knob gated by
  `media_numa_pin_streaming_threads`, and it consumes the index produced by the naming counter.
- **`paint_animation.rs` is half feature, half meter.** `opacity_binding`, `transform_binding`,
  `animated_opacity`, `animated_transform_list`, `horizon_seconds`, `SAMPLE_SECONDS`,
  `MAX_SAMPLES`, `EPSILON` compute what is drawn. Only the `tally(...)`/`note_*(...)` statements
  come out of their bodies.
- **`task_duration` in `script_thread.rs:1884/2037` has two readers.** `note_task_duration` is
  instrumentation; the `> MAX_TASK_NS` → `doc.start_tti()` block at 2039-2058 is upstream TTI.
  Keep `start`/`task_duration`.
- **`components/allocator/tracking.rs` is upstream Servo**, feature-gated behind
  `allocation-tracking`, and has nothing to do with the wall. Its `note_allocation` is a false
  positive for the `note_*` pattern.
- **`emit_timeline_marker` (`window.rs:3396-3410`) is upstream devtools**, the only `emit_*` in
  `script/`, and a false positive for the `emit_*` pattern.

---

## UNCERTAIN (do not guess)

| symbol | file | why |
| --- | --- | --- |
| `note_media_d3d11_device` + `conflicting_device_owner` + `MEDIA_D3D11_DEVICE_OWNERS` + `wall_gpu_fanout_tests` | `components/media/media-thread/lib.rs:740-800` | Fork-added and prints only a warning, so it matches REMOVE mechanically. But it is a **correctness alarm** for a silent multi-GPU fan-out failure (2026-08-24), not a performance measurement, and it has a unit test. Ask the user whether alarms are in scope. |
| `RingState::dropped_frames` + `D3d11PlaneRings::dropped_frames()` | `components/media/player/d3d11_ring.rs:275, 570, 629, 699-704` | Only non-test reader is a `D3D11PROF` log line, so it is removable — but two unit-test assertions (1507, 1516) use it to prove drop behaviour. Removing it means rewriting or deleting those assertions. |
| `thread_name::tag_video_streaming_thread` / `tag_audio_streaming_thread` and the `THREADMAP` line | `components/media/backends/gstreamer/thread_name.rs:21-128` | Mixed. The `SetThreadDescription` + `THREADMAP` half is measurement; `pin_to_node` is a NUMA performance control that depends on the same counter. A clean split is possible but is a rewrite, not a deletion. |
| `D3D11_TEXTURES_RELEASED` | `components/shared/paint/rendering_context.rs:64, 1186` | Outside this subsystem. Its only reader is the `MEDIARINGS` line removed here, so it becomes dead — but the decision belongs to whoever owns `shared/paint`. |
| `WEBGLLIVE` counterpart in the WebGL thread | not in this subsystem (referenced from `webglrenderingcontext.rs:189, 204, 655`) | `report_live_contexts` is explicitly designed to be read alongside `WEBGLLIVE`. Removing one half without the other leaves a dangling pair; the other half lives in the WebGL/canvas subsystem. |
| `Wall media frame` / `Wall media frame summary` log level choice | `components/script/dom/html/htmlmediaelement.rs:621-679` | The `debug!` half costs nothing when `RUST_LOG` excludes it, but the `info!` summary at 658-675 runs a modulo and an `Instant::now()` on every 120th frame. Classified REMOVE above; flagging it because it is the one item where "it is only a log line" and "it is on the per-frame path" are both true, and the user may want the removal staged separately from the counters. |
