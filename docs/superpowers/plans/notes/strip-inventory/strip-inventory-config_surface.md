# Inventory: prefs / env / launcher / docs

Subsystem scope: `components/config/` (prefs.rs, debug_env.rs, config_dump.rs, removed_env.rs,
opts.rs), `components/config/tests/config_surface.rs`, `etc/multigpu/run_wall_dist.ps1`,
`etc/multigpu/make_wall_dist.ps1`, `etc/multigpu/tools/*`, and the docs that document
instrumentation switches.

Repo: `W:\servo_multigpu-tiled-wall`, branch `wall-strip-instrumentation`. READ-ONLY pass; nothing
was edited.

Build-gate note up front: the stated gate is `cargo test -p servo-paint --lib --features
webgl/no-wgl,webgpu`. **`components/config/tests/config_surface.rs` is an integration test of the
`servo-config` package and is NOT in that gate.** It will not catch a broken registry. Check it
separately with `cargo test -p servo-config --test config_surface`. Note also that
`every_env_name_read_in_sources_is_registered` is **already failing today** (unregistered `SERVO_*`
literals, see B.2) — do not read a failure there as caused by this strip.

---

## A. Prefs that exist only to gate instrumentation

**Result: none. Zero prefs in `components/config/prefs.rs` are instrumentation-only.**

I enumerated all 231 fields of `Preferences` (prefs.rs:101-1197) and grepped every fork-added
`gfx_*` / `media_*` / `dom_*` candidate for its readers. There is no `gfx_*_prof`, no
`gfx_wall_*_log`, no `*_debug`, no `*_stat` pref. Every instrumentation gate in this fork is an
**environment variable**, by deliberate design — `debug_env.rs` module doc: "운용 설정은 pref 다
… 여기 있는 것은 실패 주입·프로파일링·이분탐색용이며, 조사가 끝나면 지운다."

The prefs that *look* like instrumentation and are not are also in **Traps** below. Verification
table:

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `gfx_wall_pacing_thread` | prefs.rs:571 (decl), :1333 (default) | bool pref | KEEP | `winit_wall/main.rs:2000` picks the frame clock; `:2275` gates spawning the pacing thread | this IS the frame clock selector |
| `gfx_wall_pacing_vblank` | prefs.rs:589, :1334 | bool pref | KEEP | `winit_wall/main.rs:1334` chooses `WaitForVBlank` vs DWM-query+sleep inside the pacing loop | the pacing clock itself |
| `gfx_wall_pacing_output` | prefs.rs:613, :1335 | String pref | KEEP | `output_grid.rs:1729` (LazyLock) selects which `IDXGIOutput` the pacing thread blocks on | `output_grid.rs:2500` mentions it in a log comment too, but :1729 is the decision |
| `gfx_present_align_dwm_pct` | prefs.rs:676, :1339 | i64 pref | KEEP | `winit_wall/main.rs:1231` (`snap_to_dwm_grid_at`) and `:1335` — feeds the timer-fallback pacing path | the load-bearing `DwmGetCompositionTimingInfo` consumer |
| `gfx_present_align_commit_pct` | prefs.rs:699, :1340 | i64 pref | KEEP | `paint/lib.rs:81` (`flush_deferred_dcomp_commits`), `dcomp_compositor.rs:2204`, `painter.rs:2497` — schedules the real Commit | |
| `gfx_vsync_phase_pct` | prefs.rs:638 | i64 pref | KEEP | `winit_wall/main.rs:2010`, `:2195` — tick offset from vsync | |
| `gfx_sample_lead_periods` | prefs.rs:718 | i64 pref | KEEP | `commit_guard.rs:100` — animation sample lead; decides what is drawn | |
| `gfx_paint_side_animation_tick_divisor` | prefs.rs:539 | i64 pref | KEEP | `refresh_driver.rs:276`; `script/animations.rs:309` (comment) | memory note: divisor=1 reproduces the judder on the dev box |
| `gfx_paint_side_animation_horizon_ms` | prefs.rs:524 | i64 pref | KEEP | `layout/display_list/paint_animation.rs:51` — prediction horizon | |
| `dom_animation_timeline_use_frame_time` | prefs.rs:160, :1214 | bool pref | KEEP | `script/dom/document/document.rs:1768, 4574, 4599, 4601` — the page's rAF timestamp | product feature; explicitly in the task's KEEP list |
| `gfx_wall_rotate_tile_order` | prefs.rs:483, :1327 | bool pref | KEEP | `winit_wall/main.rs:1690` — rotates the starting tile each pass | diagnostic-*motivated* but behaviour-changing; prints no number |
| `gfx_dcomp_update_quantum_px` | prefs.rs:469 | i64 pref | KEEP | `dcomp_compositor.rs:309` — rounds the BeginDraw update rect | |
| `gfx_painter_detached_queue_limit` | prefs.rs:987 | i64 pref | KEEP | `paint.rs:1804` — detached dispatch queue cap | |
| `log_filter` | prefs.rs:1192, :1444 | String pref | KEEP | upstream Servo (`git log -S`: `8473946d11c [OH] allow setting log_filter via prefs.json (#36916)`) | upstream, not this fork |
| `dom_testperf_enabled` | prefs.rs:284 | bool pref | KEEP | upstream Servo testing pref | upstream |

---

## B. Environment variables read for instrumentation

### B.1 Registered in `servo_config::debug_env::ALL` (25 flags)

Registry: `components/config/debug_env.rs` (485 lines). Each flag is a `pub const DebugFlag` plus a
slot in `ALL` (:365-391) whose index must equal its `cache_index` (compile-time assertion at
:395-405).

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `D3D11_PROFILE` (`SERVO_D3D11_PROFILE`) | debug_env.rs:103-110 | Str gate | REMOVE | `media/backends/gstreamer/render-d3d11/lib.rs:63` gates stage timing + the `D3D11PROF` line; `media/media-thread/lib.rs:643` same gate in the second crate. Both only format a log line. | two crates, identical predicate |
| `D3D11_PROFILE_MS` (`SERVO_D3D11_PROFILE_MS`) | debug_env.rs:112-119 | Str (f64) | REMOVE | `render-d3d11/lib.rs:71` — log threshold only | dies with `D3D11_PROFILE` |
| `DCOMP_DEBUG` (`SERVO_DCOMP_DEBUG`) | debug_env.rs:126-132 | Presence | REMOVE | `paint/dcomp_compositor.rs:81` (`dcomp_debug()` LazyLock) gates bind/add_surface coordinate logging | hot-path gate; log only |
| `DCOMP_READBACK` (`SERVO_DCOMP_READBACK`) | debug_env.rs:134-140 | Presence | REMOVE | `paint/dcomp_compositor.rs:118` gates a `glReadPixels` readback plus log (`:1319` comment) | the readback exists only to print; it also stalls the pipeline |
| `DCOMP_VALIDPROBE` (`SERVO_DCOMP_VALIDPROBE`) | debug_env.rs:142-148 | Presence | REMOVE | `paint/dcomp_compositor.rs:129` gates the valid_rect/dirty_rect coverage log | log only |
| `LOG_PRESENT_CADENCE` (`SERVO_LOG_PRESENT_CADENCE`) | debug_env.rs:163-170 | Str gate | REMOVE | `paint/painter.rs:85-88` (LazyLock), read at `:2470`, `:2590`, `:2598`, `:3467`; accumulator fields `:274`, `:283` | drives `WALLSPLIT`/`WALLQUEUE`/present-cadence lines. **Also named as a const in `config_surface.rs:289, 293`** |
| `VIDEO_ESCAPE_PROF` (`SERVO_VIDEO_ESCAPE_PROF`) | debug_env.rs:193-200 | Presence | REMOVE | `paint/dcomp_compositor.rs:137` gates the once-a-second `[vesc-prof]` aggregate line | log only |
| `MEDIA_VIDEO_RATE` (`SERVO_MEDIA_VIDEO_RATE`) | debug_env.rs:221-231 | Str gate | REMOVE | `media/backends/gstreamer/player.rs:512` gates the per-second `VIDEORATE` line (`:629`) | log only |
| `FRAME_REASON_PROF` (`SERVO_FRAME_REASON_PROF`) | debug_env.rs:240-249 | Str gate | REMOVE | `paint/painter.rs:75-82` (LazyLock + accessor), read at `:2895`; accumulator field `:315` | emits `FRAMEREASON`. **`check_freeze_log.ps1` reads `FRAMEREASON total` as its winit_wall liveness signal** — see Traps |
| `MEDIA_SINK_PROF` (`SERVO_MEDIA_SINK_PROF`) | debug_env.rs:251-256 | Str gate | REMOVE | `media/backends/gstreamer/player.rs:2239` gates appsink stage timing (`:1556`, `:2266`) | log only |
| `WEBGL_FANOUT_PROF` (`SERVO_WEBGL_FANOUT_PROF`) | debug_env.rs:258-263 | Str gate | REMOVE | `paint/webrender_external_images.rs:37-40` (LazyLock), read at `:134`, `:140`, `:148`, `:186`, `:335` | emits `WEBGLFANOUT`/`WEBGLEXTIMG`. prefs.rs:391 and :407 only mention it in doc comments |
| `WALL_NO_PROBE` (`SERVO_WALL_NO_PROBE`) | debug_env.rs:265-276 | Presence | REMOVE | `paint/output_grid.rs:36-39` — the master switch for every `note_*`/`emit_*` in `output_grid`/`commit_guard` and for the probe thread | **exists only to A/B the instrumentation against itself.** Meaningless once the instrumentation is gone |
| `DCOMP_BIND_PROF` (`SERVO_DCOMP_BIND_PROF`) | debug_env.rs:278-283 | Str gate | REMOVE | `paint/dcomp_compositor.rs:1441-1444` (`pub(crate)` LazyLock), read at `:2243`, `:3229`, `:3273`, `:3803`; fields `:1299`, `:1312`, `:1647` | `DCOMPBIND`/`COMMITSITE`/`COMMITWORK`/`COMPREFRESH`/`TICKCOMMIT` all hang off this. **`:3803` selects `work_device` under the gate — verify that is a pure timing split, not a behaviour fork** (flagged to the paint inventory) |
| `DCOMP_SLOW_COMMIT_MS` (`SERVO_DCOMP_SLOW_COMMIT_MS`) | debug_env.rs:318-326 | Int | REMOVE | `paint/dcomp_compositor.rs:2192` — the `COMMITSITE` `over` threshold (`:1967`). `paint.rs:89` is a doc-comment mention only | only runs when `DCOMP_BIND_PROF` is on |
| `FRAME_TIME_PROF` (`SERVO_FRAME_TIME_PROF`) | debug_env.rs:332-352 | Str gate | REMOVE | `paint/frame_time.rs:108-111` (LazyLock), read at `:83`; `script/raf_clock.rs:35-38`, read at `:80` | **gate only.** The `frame_time` plumbing it measures is load-bearing — see Traps |
| `WALL_PAINTER_LOCK_PROF` (`SERVO_WALL_PAINTER_LOCK_PROF`) | debug_env.rs:358-363 | Str gate | REMOVE | `paint/paint.rs:93-97` (LazyLock) — emits `PAINTERLOCK` | log only |
| `WALL_FRAME_DELAY_TARGET_INDEX` | debug_env.rs:76-82 | Int | UNCERTAIN | `paint/paint.rs:751-753` — `WallFrameDelayInjection::from_environment` | **failure injection, not measurement.** Prints no number; it withholds a frame-ready signal to simulate a slow renderer. Fails the stated REMOVE rule. Ask the user |
| `WALL_FRAME_DELAY_AFTER` | debug_env.rs:84-89 | Int | UNCERTAIN | `paint/paint.rs:760-761` (doc at `:733`) | same injection triple |
| `WALL_FRAME_DELAY_COUNT` | debug_env.rs:91-96 | Int | UNCERTAIN | `paint/paint.rs:763-769` | same injection triple |
| `DCOMP_NO_PARTIAL_PRESENT` | debug_env.rs:150-155 | Presence | UNCERTAIN | `paint/dcomp_compositor.rs:564` — disables partial Present | kill switch; changes what is presented |
| `DCOMP_DISABLE_RESIZE_REBUILD` | debug_env.rs:172-179 | Str gate | UNCERTAIN | `paint/painter.rs:137` — disables the post-resize picture-cache rebuild | kill switch, default = feature ON |
| `DCOMP_DISABLE_RESIZE_VIRTUAL` | debug_env.rs:181-187 | Str gate | UNCERTAIN | `paint/painter.rs:150` (and `:4586`) — disables drag-time virtual-surface mode | kill switch |
| `MEDIA_DISABLE_ENOUGHDATA_BACKOFF` | debug_env.rs:208-215 | Str gate | UNCERTAIN | `media/backends/gstreamer/player.rs:498`; `script/dom/html/htmlmediaelement.rs:137` | kill switch in two crates |
| `WALL_CANVAS_ACK_SKIP` | debug_env.rs:293-303 | Int | UNCERTAIN | `paint/painter.rs:91-94` (LazyLock), used at `:1177`, `:2097`, `:2126` | deadlock *injection*; emits `WALLACKSKIP` but its purpose is to change behaviour |
| `WALL_DISABLE_CANVAS_ACK_RECOVERY` | debug_env.rs:305-312 | Str gate | UNCERTAIN | `paint/painter.rs:100-103`, read at `:2172` (doc `:2337`) | disables `flush_owed_canvas_ack`; its own doc calls it "운영에서 쓸 kill switch 이기도 하다" |

### B.2 Read by name literal, NOT registered (the drift the test already catches)

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `SERVO_LOG_ANIM_STEP_SEQ` | `paint/painter.rs:4969-4975` (`pub(crate) static LOG_ANIM_STEP_SEQ`); `script/raf_clock.rs:127` (inline `env::var`) | bool gate | REMOVE | readers: `painter.rs:1686`, `:1791` (`ANIMSTEPSEQ`); `paint/frame_time.rs:172` (`FRAMETIMESEQ`); `paint/web_content_animation.rs:593` (doc `:578`); `script/raf_clock.rs:127` (`RAFCLOCKSEQ`). Every reader only formats a raw-sequence log line | unregistered — `debug_env.rs:5` says so. Launcher `-AnimStepSeq` sets it. It is also **named inside the `FRAME_TIME_PROF` doc string** (debug_env.rs:348) which `configuration.md:433` duplicates verbatim |
| `SERVO_WR_SLOW_MS` | set at `run_wall_dist.ps1:917`; **no Rust reader anywhere** | dead gate | REMOVE | grep of `components/`, `ports/`, `third_party/` finds **zero** readers. Only mentions are `.superpowers/sdd/2026-09-21-per-output-commit-phase/task-3-brief.md:85` and `docs/superpowers/plans/2026-09-21-per-output-commit-phase.md:778` | **already dead**: `-WrSlowMs` sets an env nothing reads |
| `SERVO_MEDIA_LOCK_SLOW_MS` | `media/media-thread/lib.rs:1383-1386` (LazyLock) | f64 threshold | REMOVE | `media-thread/lib.rs:1326` and `:1478`: `if total_ms > *MEDIA_LOCK_SLOW_MS { warn!(...) }`; doc `:1104`. Threshold for a log line only | unregistered |
| `SERVO_SCRIPT_SLOW_TASK_MS` | `script/script_thread.rs:191-200` | i64 threshold | REMOVE | gates the `SCRIPTTASK` warn line only | unregistered |
| `SERVO_SCRIPT_FORCE_GC_SEC` | `script/script_thread.rs:212-220` | i64 interval | UNCERTAIN | `script_thread.rs:1230` (timer-driven collection) and `:1349` (`SCRIPTGC` warn). Comment reference at `script/dom/webgl/webglrenderingcontext.rs:188` | **changes behaviour** (collects on a timer instead of on pressure). Diagnostic, not measurement. Launcher `-ForceGcSec` sets it and the banner says "perf numbers from this run are not comparable" |

### B.3 Env names that are NOT this fork's instrumentation — leave alone

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `SERVO_DIAGNOSTICS` | `components/config/opts.rs:197` | opts flag | KEEP | upstream Servo; listed in `config_surface.rs:17` `UPSTREAM_OWNED` | upstream |
| `SERVO_TRACING`, `SERVO_STYLE_THREAD_STACK_SIZE_KB` | `config_surface.rs:16-20` (allowlist only) | n/a | KEEP | upstream-owned allowlist entries | upstream |
| `SAMPLING_RATE` / `SAMPLING_DURATION` | `ports/servoshell/desktop/headed_window.rs:642, 646` | upstream sampler | KEEP | upstream Servo's own sampling profiler, in servoshell not winit_wall | out of this strip's scope |
| `RUST_LOG` / `RUST_BACKTRACE` | `constellation/sandboxing.rs:260-264`, `ports/servoshell/panic_hook.rs:39` | upstream | KEEP | upstream | |
| `http_proxy` / `https_proxy` / `no_proxy` | `prefs.rs:1484-1493` | upstream | KEEP | upstream proxy env | |

### B.4 Registry infrastructure in `components/config/`

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| debug-env half of `log_effective_config` | `components/config/config_dump.rs:27-34` | startup log | REMOVE | iterates `debug_env::ALL` and prints `servo: config: debug env: …`. Callers: `winit_wall/main.rs:1922`, `ports/servoshell/desktop/app.rs:98` | compiles at any `ALL` size; delete the block when there is nothing meaningful left to report |
| pref-diff half of `log_effective_config` | `config_dump.rs:19-26` + `prefs.rs:1469` (`diff_from`) | startup log | KEEP | prints only prefs that differ from default, once, at startup | **run provenance, not a measurement.** After this strip it is the only thing in the log that says which configuration produced what the eye saw |
| `debug_env.rs` module scaffolding (`Kind`, `DebugFlag`, `ALL`, `CACHES`, `enabled`/`int`/`string`) | `components/config/debug_env.rs:42-74, 365-485` | registry | KEEP | the injection/kill-switch flags in B.1 still need it | if the user also drops the UNCERTAIN nine, the whole file goes and `components/config/lib.rs` plus the re-export at `components/servo/lib.rs:68` must be updated |
| `removed_env.rs` | `components/config/removed_env.rs` (228 lines), `blocked_by` at `:200` | migration blocklist | KEEP | blocks 20 env names that became prefs | config migration, not instrumentation |

---

## C. `etc/multigpu/run_wall_dist.ps1` — launcher (2715 lines)

**Every row in this section is flagged "launcher, ask before deleting."** The launcher is a separate
decision from the engine: it can keep switches that set env names the engine no longer reads (they
become inert, exactly as `-WrSlowMs` already is), and the user may want the analysis section for
reading historical logs. Stripping the engine does not require stripping this file.

### C.1 Parameters that only turn instrumentation on

Line ranges include each parameter's own comment block (the first `#` line above the declaration).

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `-D3d11Profile` | run_wall_dist.ps1:377-381 | switch | REMOVE (launcher, ask) | `:910` sets `SERVO_D3D11_PROFILE=1`; `:1079` banner | |
| `-D3d11ProfileMs` | :382-387 | int | REMOVE (launcher, ask) | `:911-914` validates and sets `SERVO_D3D11_PROFILE_MS`; `:1079` banner | |
| `-VideoRate` | :388-394 | switch | REMOVE (launcher, ask) | `:915` sets `SERVO_MEDIA_VIDEO_RATE`; `:1079` banner; analysis `:1438-1477` | |
| `-FrameReason` | :418-423 | switch | REMOVE (launcher, ask) | `:916` sets `SERVO_FRAME_REASON_PROF`; analysis `:1478-1562` | |
| `-WrSlowMs` | :424-433 | int | REMOVE (launcher, ask) | `:917` sets `SERVO_WR_SLOW_MS` — **nothing reads it**; no analysis block | already a no-op; safe to delete first and alone |
| `-SinkProf` | :434-443 | switch | REMOVE (launcher, ask) | `:918` sets `SERVO_MEDIA_SINK_PROF` | |
| `-FanoutProf` | :444-458 | switch | REMOVE (launcher, ask) | `:919` sets `SERVO_WEBGL_FANOUT_PROF`; analysis `:1563-1622`, `:1671`, `:1969` | |
| `-DcompBindProf` | :459-471 | switch | REMOVE (launcher, ask) | `:800-802` throws when `-DComp off`; `:920` sets `SERVO_DCOMP_BIND_PROF`; analysis `:1973-2120`, `:2121-2175`, `:2176-2271`, `:2461-2565`, `:2649-2715` | the single largest consumer of the analysis section |
| `-CanvasAckSkip` | :490 | int | REMOVE (launcher, ask) | `:921` sets `SERVO_WALL_CANVAS_ACK_SKIP`; analysis `:1895-1903` | the engine flag is UNCERTAIN (injection); if that stays, keep this |
| `-ForceGcSec` | :491-497 | int | REMOVE (launcher, ask) | `:922-923` sets/clears `SERVO_SCRIPT_FORCE_GC_SEC`; `:1087` banner | engine side is UNCERTAIN |
| `-NoCanvasAckRecovery` | :498-501 | switch | REMOVE (launcher, ask) | `:924` sets `SERVO_WALL_DISABLE_CANVAS_ACK_RECOVERY`; `:1898` | engine side is UNCERTAIN (kill switch) |
| `-PresentCadence` | :604-609 | switch | REMOVE (launcher, ask) | `:925` sets `SERVO_LOG_PRESENT_CADENCE`; analysis `:1764-1820`, `:1821-1859` | |
| `-AnimStepSeq` | :610-614 | switch | REMOVE (launcher, ask) | `:926` sets `SERVO_LOG_ANIM_STEP_SEQ` | |
| `-FrameTimeProf` | :615-622 | switch | REMOVE (launcher, ask) | `:927` sets `SERVO_FRAME_TIME_PROF`; analysis `:2327-2390`, `:2391-2460` | |
| `-PainterLockProf` | :623-640 | switch | REMOVE (launcher, ask) | `:928` sets `SERVO_WALL_PAINTER_LOCK_PROF`; analysis `:2566-2648` | |
| `-DcompDebug` | :647-651 | switch | REMOVE (launcher, ask) | `:785-787` warns when `-DComp off`; `:929` sets `SERVO_DCOMP_DEBUG` | |
| `-VideoEscapeProf` | :652-655 | switch | REMOVE (launcher, ask) | `:930` sets `SERVO_VIDEO_ESCAPE_PROF` | |
| `-TileThreadSpike` | :472-482 | switch | REMOVE (launcher, ask) | `:1063` appends `--tile-thread-spike`; analysis `:1675-1699` | drives `winit_wall/main.rs:142, 2059-2060` → `tile::plan_and_run_tile_thread_spike`. **A 474-line spike harness (`winit_wall/tile.rs`) hangs off it** — flagged to the winit_wall inventory |
| `-ThreadCpu` | :759 | switch | REMOVE (launcher, ask) | `:1294-1318` runs `thread_cpu_probe.exe` mid-run and writes `*.threads.txt`; `:1326-1328` warns without `-DurationSec` | |
| `-ThreadCpuWarmupSec` | :760-762 | int | REMOVE (launcher, ask) | `:1296`, `:1300`, `:1302` | dies with `-ThreadCpu` |

### C.2 Logging-plumbing parameters (named in the user's request)

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `-NoLog` | :328-335 | switch | REMOVE (launcher, ask) | `:872-874` contradiction check; `:878` sets `$LogPath="NUL"`; `:1088` banner; `:1147-1149` `$stderrTransport`; `:1306`, `:1315` thread-log naming | **the only way to run with no log file.** Removing it forces a log on every run — consider keeping it even if everything else goes |
| `-KeepRustLog` | :336 | switch | REMOVE (launcher, ask) | `:935-937` contradiction check with `-QuietLog`; `:951` else-branch picks the default `RUST_LOG` | |
| `-QuietLog` | :337-354 | switch | REMOVE (launcher, ask) | `:935`, `:938-950` sets `RUST_LOG="error,paint=warn,script::raf_clock=warn,winit_wall=info"` | exists to keep exactly the judder counters and drop everything else — **dies with those counters** |
| `-LegacyLogRedirect` | :355-376 | switch | REMOVE (launcher, ask) | `:1149`, `:1173` choose `Start-Process -RedirectStandardError` (pipe) vs `cmd 2>` (inherited handle) | memory note: the 127µs write cost is `-LegacyLogRedirect`-only; "logs hide the judder" was disproven |
| default `RUST_LOG` assignment | :951-958 | assignment | UNCERTAIN | sets `warn,paint=info,media=info,winit_wall=info,…` unconditionally (rationale in the file header, :19-24) | after the strip there is far less to log; what the right default becomes is the user's call |
| `RUST_LOG` token validator | :959-981 | validation | KEEP | warns when a `RUST_LOG` token is not a level (a bare token silently raises max_level to Trace) | a correctness guard, not instrumentation; costs nothing |
| `media=` presence warning | :983-986 | warning | REMOVE (launcher, ask) | warns that the `WALLDIAG` lines will be missing | guards an analysis table only |

### C.3 Launcher body regions

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `SERVO_*` instrumentation env block | :904-931 (the assignments are :910-930) | assignment block | REMOVE (launcher, ask) | the complete set of `$env:SERVO_…` instrumentation assignments | **lines :905-907 (the `SERVO_*` wipe) must stay** — memory note `run-wall-dist-wipes-servo-env`; it is what makes `removed_env` startup blocking work |
| `-DcompBindProf` / `-DcompDebug` pairing guards | :785-787, :800-802 | validation | REMOVE (launcher, ask) | throw/warn when the gate cannot produce output | dies with the switches |
| instrumentation banner line | :1079 | `Write-Host` | REMOVE (launcher, ask) | prints `d3d11_profile=… video_rate=… threshold=…`; the `immediate_composite=` part names a real pref and must be kept | **partial line** — do not delete the whole line |
| force-GC banner line | :1087 | `Write-Host` | REMOVE (launcher, ask) | | |
| `-NoLog` banner line | :1088 | `Write-Host` | REMOVE (launcher, ask) | | |
| `$stderrTransport` / `$engineMd5` / `$runConfig` | :1140-1169 | run provenance | KEEP | `:1166` console, `:1358-1362` appends `RUNCONFIG …` lines into the log, `:1368` reprints | run identity, not measurement; keeps naked-eye A/B honest. Trim only the `$stderrTransport` branches tied to removed switches |
| **unconditional `thread_cpu_probe` sampling** | :1254-1289 | probe + 3s sleep | REMOVE (launcher, ask) | runs `thread_cpu_probe.exe --pid … --duration 1` three seconds into **every** run (no switch gates it), parses `group N` lines, warns about processor group 0 | **this costs every run a 3-second sleep and an extra process.** Highest-value launcher removal |
| `-ThreadCpu` sampling block | :1292-1318 | probe | REMOVE (launcher, ask) | see `-ThreadCpu` above | |
| **whole analysis section** | **:1345-2715 (end of file)** | log parser | REMOVE (launcher, ask) | begins at `if (!(Test-Path $LogPath)) { exit 0 }` (:1345). 24 blocks: markers :1370-1383, per-tile rate :1385-1399, WALLDIAG :1400-1410, D3D11PROF :1411-1437, VIDEORATE :1438-1477, FRAMEREASON :1478-1562, WEBGLFANOUT :1563-1622, WEBGLWATCHDOG :1623-1674, TILESPIKE :1675-1699, SURFIMPORT :1700-1763, WALLSPLIT :1764-1820, WALLQUEUE :1821-1859, WALLACK :1860-1904, WEBGLEXTIMG :1905-1972, DCOMPBIND :1973-2120, COMMITSITE :2121-2175, COMMITWORK :2176-2271, FRAMEID :2272-2326, FRAMETIME :2327-2390, RAFCLOCK :2391-2460, COMPREFRESH :2461-2565, PAINTERLOCK :2566-2648, TICKCOMMIT :2649-2715 | **1371 lines, just over half the file.** The `RUNCONFIG` append at :1346-1368 sits inside this range but is provenance — keep it if `$runConfig` is kept |
| `-Pref` passthrough | :1060 | pass-through | KEEP | `foreach ($p in $Pref) { $argList += @("--pref", $p) }` | the escape hatch that makes every removed switch re-creatable by hand |

---

## D. `etc/multigpu/make_wall_dist.ps1` and `etc/multigpu/tools/`

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `thread_cpu_probe` build + copy | make_wall_dist.ps1:~120-135 (warnings at `:123`, `:133`) | packaging | REMOVE (launcher, ask) | packages the probe that `-ThreadCpu` and the unconditional block at run_wall_dist:1255 need | |
| `probe_machine_topology.ps1` copy | make_wall_dist.ps1:249 | packaging | REMOVE (launcher, ask) | ships the topology probe into the dist | |
| `run_frame_time_ab.ps1` copy | make_wall_dist.ps1:~245 | packaging | REMOVE (launcher, ask) | ships the A/B driver | |
| probe-page copies and their comment blocks | make_wall_dist.ps1:~150-230 (`:194` ANIMJITTER, `:207` `-DcompBindProf` recipe, `:218` `-FrameTimeProf -AnimStepSeq` recipe) | packaging | UNCERTAIN | copies `wall_anim_jitter_probe.html`, `ticker_slice_probe.html`, `raf_clock_probe.html`, `wall_paint_animation_probe.html` and others into the dist | these are **naked-eye probe pages** — exactly what the user wants to restart from. Keep the pages; only the switch recipes inside the comments go stale |
| usage hint naming `-ThreadCpu` | make_wall_dist.ps1:284 | `Write-Host` | REMOVE (launcher, ask) | | |
| `tools/run_frame_time_ab.ps1` | whole file (`:32-33` set `PresentCadence`, `DcompBindProf`, `AnimStepSeq`) | analysis driver | REMOVE (launcher, ask) | drives `run_wall_dist.ps1` with three instrumentation switches | breaks the moment those switches go |
| `tools/thread_cpu_probe/` | whole directory | probe binary | REMOVE (launcher, ask) | only consumers are `-ThreadCpu` and the unconditional block | |
| `tools/topology_probe/` and `tools/probe_machine_topology.ps1` | whole directory / file | probe | REMOVE (launcher, ask) | machine-shape reporting, used only to interpret perf numbers | |
| `tools/wall_perf_analyzer/` | whole directory | log analyser | REMOVE (launcher, ask) | parses the same log tags the analysis section does | |
| `tools/parse_ceiling_logs.ps1` | whole file | log analyser | REMOVE (launcher, ask) | consumed by `run_video_ceiling_sweep.ps1` and `run_video_collapse_ceiling.ps1` | those two sweep scripts go with it |
| `etc/multigpu/check_freeze_log.ps1` | whole file (untracked, `??` in git status) | log analyser | UNCERTAIN | cross-reads `WALLTICKRATE produced` + `edges`, `FRAMEREASON total`, `ANIMSTEP dx_px p50` | **the user's own anti-self-deception tool** (memory: `walltickrate-zero-is-ambiguous`). It dies automatically when those three tags die. Ask before deleting — a naked-eye replacement may be wanted first |
| `tools/measure_decode_only.ps1` | `:394` mentions `-D3d11ProfileMs` | baseline tool | KEEP | a decode-ceiling baseline, independent of wall instrumentation | only a stale comment reference |
| `tools/serve_http.ps1`, `verify_gpu_fanout.ps1`, `verify_ndebug_webgl.ps1`, `wall_mouse_hittest_probe.ps1`, `wall_synth_mouse_wiggle.ps1`, `run_single_capture.ps1`, `run_three_wall_capture.ps1` | whole files | support | KEEP | not performance instrumentation | |
| `run_video_ceiling_sweep.ps1`, `run_video_collapse_ceiling.ps1` | whole files | sweep drivers | UNCERTAIN | both feed `parse_ceiling_logs.ps1` | video-decode ceiling sweeps, not wall judder; outside the stated four categories |

---

## E. Docs

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `docs/multigpu/configuration.md` section "조사용 환경변수 21 개" | configuration.md:200-441 | reference | REMOVE | **`config_surface.rs:614` (`every_knob_appears_in_the_configuration_doc`) and `:636` (`the_doc_repeats_the_knob_descriptions_verbatim`) read this file and require one `###` entry per flag in `debug_env::ALL`, with the body text equal to the flag's `doc` field after whitespace squeeze** | delete exactly the entries whose flags go; keep the entries for flags that stay. Flag + entry in the same commit keeps both tests green |
| `configuration.md` title | configuration.md:1 | heading | REMOVE | `# 설정 노브 전량 (pref 34 + 조사용 env 21)` — the counts must be updated | no test reads it, and it is already wrong (ALL has 25) |
| `configuration.md` stale-notice line | configuration.md:200 | `>` note | REMOVE | the `★낡음…★` line immediately above the env section | goes with the section |
| `configuration.md` pref sections and CLI/trap sections | configuration.md:69-199, 443-489 | reference | KEEP | pref tables, wall CLI flags, known traps. `every_migrated_pref_appears_in_the_configuration_doc` (`config_surface.rs:621`) requires every `removed_env::REMOVED` pref **and** old env name to appear here | do not touch |
| `docs/multigpu/configuration_manual.md` section 6 "조사용 환경변수 (15 개)" | configuration_manual.md:302-332 | how-to | REMOVE | recipe table for `SERVO_LOG_PRESENT_CADENCE`, `SERVO_D3D11_PROFILE`, `SERVO_VIDEO_ESCAPE_PROF`, `SERVO_DCOMP_DEBUG`, `SERVO_DCOMP_READBACK`, `SERVO_WALL_FRAME_DELAY_*` | no test reads this file |
| `configuration_manual.md` section 2 "지금 무슨 설정으로 도는지" | configuration_manual.md:54-74 | how-to | KEEP | documents the `config_dump` startup line | keep the pref half; drop the `debug env:` sentence if `config_dump.rs:27-34` goes |
| `configuration_manual.md` sections 4 and 7 | configuration_manual.md:110-257, 334-349 | how-to | KEEP | pref recipes and a symptom table | check section 7 for rows that point at removed env names |
| `docs/ai-notes.md` | whole file | history / memory | KEEP | heavy references to `SERVO_VIDEO_ESCAPE_PROF`, `vesc-prof` and others | **history, not a switch reference.** Deleting it destroys the record of why things are as they are |
| `docs/multigpu/wall_animation_end_black_frame.md`, `wall_output_animation_frontend_handoff.md` | whole files | investigation notes | KEEP | reference `-DcompBindProf` and friends | historical records |
| `etc/multigpu/WEBRENDER_VIDEO_OFFLOAD_STATUS.md`, `WEBGL_WALL_STATUS.md`, `WEBGPU_FANOUT_STATUS.md` | whole files | status notes | KEEP | reference instrumentation switch names | historical |
| `docs/superpowers/**`, `.superpowers/sdd/**` | 6+ files reference `DcompBindProf` | plans / specs | KEEP | project history | out of scope |
| `docs/COMMAND_LINE_ARGS.md`, `docs/debugging.md` | — | upstream docs | KEEP | grep found **no** fork instrumentation references | nothing to do |

---

## F. Tests

| symbol | file:line-range | kind | verdict | every caller and what it does with the value | notes |
|---|---|---|---|---|---|
| `the_dead_knob_check_actually_catches_an_unread_flag` | config_surface.rs:271-295 | test | REMOVE or rewrite | **hard-codes `servo_config::debug_env::DCOMP_READBACK` (:276, :288), `D3D11_PROFILE_MS` (:277) and `LOG_PRESENT_CADENCE` (:289), and asserts `dead == vec!["SERVO_LOG_PRESENT_CADENCE"]` (:293)** | **a COMPILE break the moment those three consts are deleted.** Rewrite against two surviving flags, or delete it with them |
| `every_registered_flag_is_actually_read_somewhere` | config_surface.rs:255-269 | test | KEEP | scans all `.rs` for `debug_env::<IDENT>` references | **the safety net for this strip**: it fails if you delete a reader and leave the flag. Run it |
| `every_knob_appears_in_the_configuration_doc` | config_surface.rs:614-619 | test | KEEP | requires every surviving flag to have a `configuration.md` entry | run after editing the doc |
| `the_doc_repeats_the_knob_descriptions_verbatim` | config_surface.rs:636-651 | test | KEEP | requires the doc text to equal the flag's `doc` field verbatim | **the strictest doc coupling.** If you edit a surviving flag's `doc` string (for example to drop its `SERVO_LOG_ANIM_STEP_SEQ` mention) you must make the identical edit in `configuration.md` |
| `the_doc_check_actually_catches_a_missing_entry` | config_surface.rs:653-667 | test | KEEP | uses the *string* `"SERVO_DCOMP_READBACK"` (:657, :661), not a const reference, so it compiles either way | it fails trivially if `ALL` ever becomes empty (`missing` would be empty and `assert!(!missing.is_empty())` fires) |
| `every_env_name_read_in_sources_is_registered` | config_surface.rs:210-253 | test | KEEP | scans for `"SERVO_…"` literals; allowlists `UPSTREAM_OWNED` plus `PENDING_PREF_MIGRATION` (currently empty) | **already red today** on `SERVO_LOG_ANIM_STEP_SEQ`, `SERVO_MEDIA_LOCK_SLOW_MS`, `SERVO_SCRIPT_SLOW_TASK_MS`, `SERVO_WR_SLOW_MS`, `SERVO_SCRIPT_FORCE_GC_SEC`. Removing B.2 turns it green — a free win |
| remaining `config_surface.rs` tests (`gfx_defaults_…` :328, `video_defaults_…` :365, `media_defaults_…` :417, `every_migrated_env_is_listed_as_removed` :500, and the rest) | config_surface.rs:297-612 | tests | KEEP | pref defaults and removed-env migration | untouched by this strip |
| `prefs.rs` inline tests (`the_single_frame_producer_knob_stays_gone` :1578, `the_dcomp_update_quantum_defaults_to_the_whole_tile` :1596) | prefs.rs:1563-1603 | unit tests | KEEP | pref default assertions | in `-p servo-config --lib`, not in the paint gate |

**No test in this subsystem exercises instrumentation only**, other than
`the_dead_knob_check_actually_catches_an_unread_flag`. The config-surface suite is a *registry
consistency* suite and stays useful at a smaller registry.

---

## Removal order and build-breakage notes

Nothing in this subsystem can go first — **`debug_env.rs` is the leaf, not the root.**
`every_registered_flag_is_actually_read_somewhere` enforces the opposite direction: a flag deleted
while a reader survives is a compile error; a reader deleted while the flag survives is a test
failure.

1. **Launcher first, and only with the user's go-ahead.** `etc/multigpu/run_wall_dist.ps1` is not
   compiled and nothing depends on it. Delete bottom-up so the line numbers above stay valid:
   1. analysis section `:1345-2715` (keep `:1346-1368` if `$runConfig` is kept),
   2. `-ThreadCpu` block `:1292-1318`, then the unconditional probe block `:1254-1289`,
   3. banner lines `:1088`, `:1087`, and the instrumentation half of `:1079`,
   4. env block `:910-930` — **keep the `SERVO_*` wipe at `:905-907`**,
   5. pairing guards `:983-986`, `:800-802`, `:785-787`,
   6. parameter declarations bottom-up: `:760-762`, `:759`, `:652-655`, `:647-651`, `:623-640`,
      `:615-622`, `:610-614`, `:604-609`, `:498-501`, `:491-497`, `:490`, `:472-482`, `:459-471`,
      `:444-458`, `:434-443`, `:424-433`, `:418-423`, `:388-394`, `:382-387`, `:377-381`, and if the
      user wants them `:355-376`, `:337-354`, `:336`, `:328-335`.

   The file is **UTF-8 with BOM** (header :3-18): an editor that strips the BOM makes PowerShell 5.1
   decode it as cp949, and a Korean comment line then swallows the following newline. Verify after
   every edit with `Get-Command .\run_wall_dist.ps1 -Syntax` — silence means it parsed.
   `-WrSlowMs` can go first and alone, because nothing reads its env.

2. **Then the engine readers** (other subagents' subsystems: `paint/output_grid.rs`,
   `commit_guard.rs`, `painter.rs`, `dcomp_compositor.rs`, `frame_time.rs`,
   `webrender_external_images.rs`, `web_content_animation.rs`, `paint.rs`, `script/raf_clock.rs`,
   `media/*`). Each `LazyLock` gate — `painter.rs:75-105`, `dcomp_compositor.rs:1441`,
   `frame_time.rs:108`, `raf_clock.rs:35`, `paint.rs:93`, `webrender_external_images.rs:37`,
   `output_grid.rs:36` — is the last reference to its `debug_env::` const. Delete gate and const in
   the **same commit**.

3. **Then `debug_env.rs`, per flag, all three edits together or the build breaks:**
   - the `pub const` block,
   - its `&NAME` entry in `ALL` (:365-391),
   - **renumber every surviving flag's `cache_index` to its new position in `ALL`.** The const block
     at `:395-405` asserts `ALL[i].cache_index == i` at compile time, so a stale index is a hard
     compile error rather than a silent bug. This is the most error-prone step — do all 16 removals
     in one pass and renumber once.

4. **Then `config_dump.rs:27-34`**, if what remains in `ALL` is no longer worth printing.

5. **Then the unregistered names** (B.2): `SERVO_LOG_ANIM_STEP_SEQ` (`painter.rs:4969-4975` and
   `raf_clock.rs:127`, readers at `painter.rs:1686, 1791`, `frame_time.rs:172`,
   `web_content_animation.rs:593`), `SERVO_MEDIA_LOCK_SLOW_MS`
   (`media-thread/lib.rs:1326, 1383-1386, 1478`), `SERVO_SCRIPT_SLOW_TASK_MS`
   (`script_thread.rs:191-200`), `SERVO_WR_SLOW_MS` (launcher only). These are registry-free, so
   they break nothing inside `components/config/`.

6. **Then the docs, in lockstep with step 3.** In `configuration.md:200-441`, delete exactly the
   `###` entry of each deleted flag. If you edit a *surviving* flag's `doc` string, make the
   identical edit here — `the_doc_repeats_the_knob_descriptions_verbatim` compares after
   `split_whitespace().join(" ")`, so line wrapping is free but wording is not. Update the count in
   the heading at `:201` and the title at `:1`. Then `configuration_manual.md:302-332`.

7. **Then `config_surface.rs:271-295`.** Rewrite `the_dead_knob_check_actually_catches_an_unread_flag`
   against two flags that still exist, or delete it alongside the three consts it names. This is the
   only compile break in the test tree.

8. **Verification.** The stated gate `cargo test -p servo-paint --lib --features
   webgl/no-wgl,webgpu` covers **none** of this. Add `cargo test -p servo-config` (the `prefs.rs`
   unit tests) and `cargo test -p servo-config --test config_surface` (the registry and doc-drift
   tests). Expect `every_env_name_read_in_sources_is_registered` to go from red to green once step 5
   lands.

**Order summary:** launcher (independent) → engine readers → `debug_env` consts + `ALL` +
`cache_index` renumber (one commit) → `config_dump` → unregistered env → docs → `config_surface.rs`.
Steps 3 and 6 must land together or the doc tests go red in between. Memory rule
`probe-changes-get-their-own-commit` maps here to one coherent group per commit.

---

## Traps

- **`DwmGetCompositionTimingInfo` is not all instrumentation.** `gfx_present_align_dwm_pct`
  (prefs.rs:676) feeds `snap_to_dwm_grid_at` at `winit_wall/main.rs:1231` and the clock selection at
  `:1335`. That call site IS the timer-fallback pacing clock. Other call sites of the same OS
  function only feed `DWMPHASE`/`OUTPHASE` log lines. Judge each call site, never the function.
- **`IDXGIOutput::WaitForVBlank`, `gfx_wall_pacing_vblank` and `gfx_wall_pacing_output` are the frame
  clock**, not a probe. `output_grid.rs:1729` reads `gfx_wall_pacing_output` to choose which physical
  output to block on — deleting it does not quiet a log, it unanchors the wall.
- **`SERVO_FRAME_TIME_PROF` is a logging gate; `frame_time` is a product feature.** Delete
  `paint/frame_time.rs:83, 107-111` and `script/raf_clock.rs:33-38, 80` (the `FRAMETIME`/`RAFCLOCK`
  emitters), **not** the frame-time plumbing that reaches `dom_animation_timeline_use_frame_time` and
  `Document::resolve_frame_time` (`document.rs:1768, 4574, 4599, 4601`). They share a name and a
  file; they are not the same thing.
- **`SERVO_WALL_NO_PROBE` reads like an off-switch worth keeping. It is not.** It exists only to A/B
  *the instrumentation against itself* in one binary (debug_env.rs:265-276). Once the instrumentation
  is gone it gates nothing.
- **`SERVO_DCOMP_BIND_PROF` at `dcomp_compositor.rs:3803` chooses `work_device` based on the gate**
  (`let work_device = if *DCOMP_BIND_PROF { … }`). If that branch picks a *different device* rather
  than merely timing the same one, the gate has a behavioural side effect and removing it is not
  neutral. Verify that line; I could not settle it from the config side.
- **`FRAMEREASON` is load-bearing for the user's own freeze check.** `check_freeze_log.ps1` uses
  `FRAMEREASON total` as the winit_wall production-rate signal, cross-read against
  `WALLTICKRATE edges` and `ANIMSTEP dx_px`, precisely because `produced=0` means both "fixed" and
  "frozen" (memory: `walltickrate-zero-is-ambiguous`). Removing `SERVO_FRAME_REASON_PROF` removes the
  only automated way to tell a frozen run from a clean one. Offer a naked-eye substitute first.
- **`-TileThreadSpike` is not a logging switch.** It makes `winit_wall` run a context-creation spike
  on worker threads and exit (`main.rs:2059-2060`, `tile.rs`, 474 lines). It is a test harness, not a
  measurement of a running wall, and it belongs to the winit_wall decision.
- **Failure injection and kill switches are not measurement.** Nine registered flags
  (`WALL_FRAME_DELAY_*` x3, `DCOMP_NO_PARTIAL_PRESENT`, `DCOMP_DISABLE_RESIZE_REBUILD`,
  `DCOMP_DISABLE_RESIZE_VIRTUAL`, `MEDIA_DISABLE_ENOUGHDATA_BACKOFF`, `WALL_CANVAS_ACK_SKIP`,
  `WALL_DISABLE_CANVAS_ACK_RECOVERY`) plus `SERVO_SCRIPT_FORCE_GC_SEC` print nothing on their own;
  they change behaviour on purpose. They fail the stated REMOVE rule, so they are UNCERTAIN.
- **`run_wall_dist.ps1:905-907` wipes all `SERVO_*` before anything is set.** Do not delete it while
  trimming the env block two lines below. It is what makes `removed_env`'s startup blocking work and
  what makes shell-set diagnostic env invisible to the engine (memory: `run-wall-dist-wipes-servo-env`).
- **`config_dump::log_effective_config`'s pref half is run provenance, not a measurement.** It prints
  only prefs that differ from default, once, at startup. After this strip it is the only thing in the
  log that says which configuration produced what the eye saw. Keep it.
- **`run_wall_dist.ps1` is UTF-8 with BOM.** Any edit that drops the BOM makes PowerShell 5.1 decode
  it as cp949; a Korean comment line then eats the following newline and the parser fails somewhere
  unrelated with `Unexpected token '}'`.
- **`every_env_name_read_in_sources_is_registered` is red before you start.** Four unregistered names
  (plus `SERVO_SCRIPT_FORCE_GC_SEC`) already fail it. Do not hunt a regression you did not cause; it
  turns green when B.2 is removed.
- **There is no pref to remove.** If a later step produces a diff that deletes a `gfx_*` pref,
  something was misclassified — every one of them is read by a rendering or timing decision.
