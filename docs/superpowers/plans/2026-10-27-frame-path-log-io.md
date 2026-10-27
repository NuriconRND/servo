# 프레임 경로 로그 I/O 제거 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 렌더 스레드가 로그 때문에 멈추는 일이 없게 하고, ★그 사실을 런마다 증명하게★ 한다.
`-NoLog` 를 붙였다 떼는 것이 판정 지표 어디에도 나타나지 않는 상태가 목표다.

**Architecture:** 세 겹이다. (1) 소음을 먼저 지운다 — 지금 나가는 1,850 줄/s 중 1,761 줄/s 는
소비자가 하나도 없는 `info!` 이고, 이걸 끄면 뒤의 모든 비용이 20 배 줄어든다. (2) 남은 줄은
스레드 로컬 SPSC 링에 쌓고 전용 드레인 스레드 하나가 자기 파일 핸들로 내보낸다 — ★전역 큐를
쓰면 지금의 전역 stderr 락이 이름만 바뀐다.★ (3) 런이 끝날 때를 엔진이 알게 해서 강제 종료
없이 내려오게 한다. 출력 텍스트 형식은 한 글자도 안 바꾼다.

**Tech Stack:** Rust / Windows / `components/paint` / `components/servo` /
`components/servo/examples/winit_wall` / `etc/multigpu/run_wall_dist.ps1`

**Spec:** 없다. 설계 문서를 따로 두지 않고 이 문서 2 절이 그 자리를 대신한다 — 거기 적힌
수치와 반증은 2026-10-27 조사(실측 프로브 2 종 + 소스 감사 + 반증 2 건)의 결론이고, 이 계획의
모든 태스크가 그것을 근거로 삼는다. ★2 절을 읽지 않고 Task 3 이후를 수행하지 말 것.★

---

## 1. Global Constraints

- 출력 텍스트 형식은 ★불변★이다. `run_wall_dist.ps1` 분석부가 34 개 패턴으로 읽고 있고,
  그중 13 곳은 `$x.Line -match` 2 차 추출까지 한다. 바이너리 로깅은 이 계획에서 하지 않는다.
- ★드롭은 "카운터만 올림" 으로 끝내지 않는다.★ 판정 16 태그는 드롭 불가 레인에 둔다.
  `DCOMPSTAT out=` 한 줄이 날아가면 `run_wall_dist.ps1:2468` 이 그 GPU 의 COMPREFRESH 를
  "idle, not driven" 으로 ★조용히 제외★한다 — 전부 잃는 것보다 일부 잃는 것이 더 나쁘다.
- ★핫 패스 레코드는 힙 할당 0.★ `components/allocator/lib.rs:112-115` 가 Windows 에서
  `std::alloc::System` = `HeapAlloc(GetProcessHeap())` 이다. 이 바이너리에 실재하는 유일한
  프로세스 전역 직렬화 지점이므로 선택이 아니라 필수다.
- 로그 호출 측(`log::info!`/`warn!`)은 한 줄도 안 고친다 — 소음 제거(Task 3)로 지우는 자리만 예외.
- 테스트: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu`
- ★pref 나 debug_env 를 건드린 태스크는 `cargo test -p servo-config --test config_surface` 도 반드시.★
- 빌드: `cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release` (약 11 분)
- 빌드 전 환경 프렐류드: `$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = [Environment]::GetEnvironmentVariable('GSTREAMER_1_0_ROOT_MSVC_X86_64','User'); . W:\scripts\servo_env.ps1; $env:PATH = (Join-Path $env:GSTREAMER_1_0_ROOT_MSVC_X86_64 'bin') + ';' + $env:PATH`
- 패키징: `etc\multigpu\make_wall_dist.ps1 -Out <경로> -Force`. 런처만 바뀐 태스크는
  `run_wall_dist.ps1` 한 파일만 덮어쓰면 된다.
- ★PowerShell 5.1 에서 `cargo`/`git` 뒤에 `2>&1` 이나 `2>$null` 을 붙이지 말 것★ — 성공이 실패로 보인다.
- ★스테이징 금지★: `Cargo.lock`, `etc/multigpu/config/wall_layout.example_1x1.json`,
  `tests/html/multigpu_standard_video_*_probe.html`,
  `docs/superpowers/plans/2026-10-15-tick-animation-frame-time.md`. `git add -A` / `git add .` 금지.
- `git stash` 금지. 푸시와 파일 정리는 사용자가 한다.
- 커밋 트레일러: `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`
- 가설 하나당 커밋 하나, 메시지에 측정 수치까지.
- 코드 주석·커밋 메시지는 평어체.

---

## 2. 조사 결과 — 이 계획이 서 있는 사실

### 2.1 사용자가 건 기각 조건: ★불성립★ (방안 1 은 살아 있다)

> "파일 I/O 가 스레드 하나뿐이 아니라 프로세스 전체를 블록한다면 이 방안 역시 기각이다."

독립 프로브 둘(C/MSVC, .NET)이 같은 답을 냈다. 가해 스레드가 커널에 붙들린 동안 피해
스레드의 지터는 무부하 기준선과 구분되지 않았다.

| 구성 | 가해 스레드 1 회 최악 | 가해 블록 ms/s | ★피해 스레드 최대 갭★ |
|---|---|---|---|
| 없음(기준선) | – | – | 0.75 ms |
| `-RedirectStandardError` 파이프, 쓰기 1 | 25.40 ms | 403 | 0.87 ms |
| `-RedirectStandardError` 파이프, 쓰기 4 | 35.29 ms | – | ★0.68 ms★ |
| `cmd 2>` 상속 핸들, 쓰기 4 | 10.91 ms | – | 1.02 ms |
| `cmd 2> NUL`, 쓰기 4 | 9.34 ms | 2 | ★2.47 ms★ |

가장 ★싼★ 구성(NUL)에서 최대 갭이 가장 컸다 — 상관이 없다. 60 Hz 피해 스레드의 p50 은
C6(파이프 403 ms/s) 271.5 µs 대 C7(NUL 2 ms/s) 274.2 µs 로 같고, 차이는 p99 에서 +480 µs 뿐이다.
36 MiB 를 한 번에 쓰는 동안(66~184 ms)에도 그 창 안 최대 지터는 645 µs 였다.

★결론: 커널 파일 경로는 다른 스레드를 막지 않는다. 막는 것은 프로세스 안의 락이다★ —
`std::io::Stderr` 는 전역 `ReentrantLock` + 무버퍼라 레코드마다 전역 락 아래서 `WriteFile` 을
한 번 낸다. 렌더 스레드 넷이 거기 줄선다. 그러니 목표는 "로그를 줄이기" 가 아니라
★핫 패스에서 stderr 핸들 자체를 떼어내기★ 다.

### 2.2 ★그런데 run 10 의 e/d 비교는 교란된 비교였다★

| run 10 arm | RUST_LOG | stderr 전송로 | 쓰기당 | 블로킹 |
|---|---|---|---|---|
| e | 풀 | `-LegacyLogRedirect`(파이프) | 127 µs | 245 ms/s |
| d | `-QuietLog` | cmd 상속 핸들 | 7.1 µs | 0.8 ms/s |

★변수 둘이 동시에 바뀌었다.★ 그리고 "풀 RUST_LOG + 상속 핸들" 칸은 ★지금까지 한 번도
돌려본 적이 없다.★ 런처 기본값은 2026-10-27 부터 상속 핸들이고(`dcc378a16da`), 245 ms/s 라는
수치는 ★`-LegacyLogRedirect` 에서만★ 나온다. 기본 경로의 풀 로그는
1,850 줄/s × 7.1 µs = **13.1 ms/s = 프레임당 0.22 ms**(렌더 스레드 넷 합)다.

### 2.3 ★저더 카운터는 애초에 arm 을 가르지 않았다★

메모리가 정한 유일한 저더 판정은 `COMPREFRESH refresh_d[2]` 다. run 10 로그에서 직접 센 값:

| arm | dup% 3 런 | windows_with_dup |
|---|---|---|
| e (풀로그 · 파이프 · 245 ms/s) | 0.3736 / 0.3291 / 0.4903 | 46 / 43 / 63 |
| d (`-QuietLog` · 상속 핸들 · 0.8 ms/s) | 0.4345 / 0.4538 / 0.6240 | 57 / 56 / 56 |

★범위가 겹치고, 더 많이 막힌 쪽이 오히려 낮다.★ 운영자 기록도 같다 — "judder 발생 빈도는
D > E 이나 큰 차이는 아닌 것으로 보임".

### 2.4 ★양이 61 배 모자란다★

두 arm 을 가른 것으로 보이던 `COMMITCYCLE phase_pct` p50 80.9/82.5/83.7 대 2.3/2.5/2.6 은
주기의 78.6% = **13.1 ms/주기**다. 그런데 기본 경로에서 두 arm 의 I/O 시간 차이는
**프레임당 215 µs** 뿐이다. 설명해야 할 양이 가진 양의 61 배다(랩어라운드로 읽어도 15 배).

⇒ ★위상을 움직인 것은 I/O 대기 시간이 아니다.★ 로그량과 함께 움직이는 다른 것이다 —
포맷 CPU(1,850/s), 스케줄링·선점, 혹은 `phase_pct` 산출 자체. 그래서 Task 2 가 게이트다.

### 2.5 "최소 침습 2 종 세트" 는 ★반증되었다★ (소스 확정)

- **`BothLogger` 에 `enabled()` 게이트 추가 → 무효를 넘어 역효과.**
  `FromEmbedderLogger::enabled` = `metadata.level() <= Level::Warn`
  (`components/constellation/logging.rs:97-99`). 남는 줄이 전부 WARN 이라 ★한 번도 차단하지
  않는다.★ 게다가 `env_logger::Logger::log` 은 이미 내부에서 `self.matches(record)` 를 돌리므로
  게이트를 붙이면 통과 레코드마다 env_filter 디렉티브 스캔이 2 회가 된다.
- **`env_logger` 의 `Target::Pipe` 로 싱크 교체 → 전역 락을 안 없애고 이름만 바꾼다.**
  `WritableTarget::Pipe(Box<Mutex<dyn io::Write + Send>>)` 다. 보유 시간이 syscall 에서 memcpy 로
  줄 뿐 렌더 스레드 넷은 여전히 ★하나의 프로세스 전역 뮤텍스★에 줄선다. 게다가 color 피처의
  `adapt()` 가 Pipe 분기에서만 ★레코드마다 `Vec` 을 추가 할당★한다(Stderr 분기엔 없다).
- **종료 플러시 경로가 없다.** `impl Log for env_logger::Logger { fn flush(&self) {} }` —
  `log::logger().flush()` 가 Pipe 싱크에 닿지 않는다.
- **드롭이 무음이다.** `let _ = self.format.format(..).and_then(|_| formatter.print(..));`

### 2.6 ★제거 대상의 95.6% 는 소비자가 0 이다★

run 10 `e_01.err.log` 내부 62 초 평균, 총 1,842 줄/s:

| 자리 | 태그 | 줄/s | 소비자 |
|---|---|---|---|
| `painter.rs:2433, 2634` | Wall render start/end | 488.5 | ps1:1328 이 ★fps 정수 하나★ |
| `painter.rs:2845, 3529` | Wall frame requested/ready | 488.2 | ★없음★ |
| `dcomp_compositor.rs` 17 자리 | `[dcomp-dbg]` | 563.3 | ★없음★ |
| `paint.rs:1019,1106,2079,2607` | barrier / metadata / fan-out | 183.0 | ★없음★ |
| `painter.rs:3898` | PAINTANIMANCHOR | 17.0 | ★없음★ |
| webrender `tile_cache.rs:448`, `renderer/mod.rs:2008` | WRSLICE / WRMEM | 21.4 | ★없음★ |
| **소계** | | **1,761.4 (95.6%)** | |
| `output_grid.rs` 1 Hz 방출부 13 태그 | COMPREFRESH 외 | 42.0 | ★유지★ |
| `winit_wall` 1 Hz 롤업 | WALLPASS/MAINBUSY 등 | 6.5 | ★유지★ |

판정에 필요한 양은 ★45 줄/s · 11 KB/s★ — 전체 바이트의 1.88% 다. 그리고 그 16 태그는
★이미★ "메모리 누적 + 1 Hz 방출" 구조다(`output_grid.rs` 의 `OutputVBlankProbe`). 즉 방안 (1)의
참조 구현이 트리 안에 이미 있다.

### 2.7 종료 트리거는 ★이미 있다★

`--capture` + `--capture-sec` 가 `should_exit.set(true)`(`main.rs:812`) →
`about_to_wait` 의 `event_loop.exit()`(`main.rs:2373`) → `run_app` 정상 복귀(`main.rs:257`) →
`drop(app)` 를 탄다. `--run-for-sec` 은 그 패턴의 복제다.

그리고 런처 주석 `run_wall_dist.ps1:1286-1287` —
"Wall tile windows ignore WM_CLOSE. env_logger writes stderr unbuffered, so a force-kill does
not lose log output" — 의 ★앞 문장은 코드와 어긋나고★(`main.rs:2449` 에
`WindowEvent::CloseRequested => event_loop.exit()` 가 있고 타일 창은 title 있는 visible 창이다),
★뒷 문장은 이 계획이 버퍼를 넣는 순간 깨진다.★

### 2.8 세 방안에 대한 처분

| 방안 | 판정 | 근거 |
|---|---|---|
| (1) 메모리 큐 + 전용 flush 스레드 | ★채택★ — 단 ★전역 큐 금지★ | 기각 조건 불성립(2.1). 전역 큐 하나면(spdlog 형) 지금의 전역 stderr 락이 전역 큐 락으로 이름만 바뀌고 타일 워커 넷의 줄서기가 그대로 남는다 |
| (2) 종료 시 일괄 기록 | ★부분 채택★ | 종료 트리거(`--run-for-sec`)만 가져온다. "메모리에만 쌓기" 는 불필요 — (1)의 드레인 비용이 실측 0.38 ms/s 라 그 이상 짤 이유가 없다. 300 초 전량도 179 MB 라 용량 문제는 애초에 없었다 |
| (3) 알려진 기법 | NanoLog·Quill 의 ★스레드 로컬 SPSC + 전용 백엔드★ 구조 채택 | 호출 측 p50 6~14 ns. spdlog(271 ns)·Boost.Log(3,093 ns)는 전역 큐 하나라 기각. `ftlog` 는 MPSC 하나라 차선. ETW 는 소비 체계를 새로 만들어야 해 보류. ★레포에 이미 있는 `tracing-perfetto` 는 `Mutex<File>` 이라 현 병목과 동형 — 기각★ |

---

## 3. Review Focus

태스크의 테스트가 건드리지 않지만 사람을 물 수 있는 것들. 각 줄의 시험은 괄호 안 태스크가 진다.

- ★판정 줄 유실이 "깨끗한 런" 으로 읽힌다★ — `TILESPIKE VERDICT` · `WALLDIAG wrap` ·
  `CountOf "panicked"` 는 존재/부재 술어다. 드롭이 침묵을 만든다. (Task 4, Task 7)
- ★링 오버플로는 random 이 아니다★ — 재려는 그 스톨 순간에 찬다. 가장 중요한 창이 가장 먼저
  날아간다. (Task 4: 판정 레인 분리)
- ★강제 종료가 마지막 창을 먹는다★ — `-DurationSec` 런의 꼬리 한 flush 주기. (Task 6)
- ★드레인 스레드가 교차 NUMA 를 읽는다★ — 벽은 프로세서 그룹 둘(40+40)이고 GPU 4 장이
  NUMA 0/1 에 2:2 다. 드레인 하나가 링 넷을 비우면 절반이 교차 노드 읽기다. (Task 4: 핀 금지 +
  Task 8 에서 p99 확인)
- ★패닉 줄의 순서 역전★ — `winit_wall` 은 panic hook 을 안 걸어 기본 std 훅이 stderr 에 직접
  쓴다(`crash_report::install()` 은 네이티브 예외 전용). 링 플러시와 그 직접 쓰기의 순서가
  뒤집힐 수 있다. (Task 4: 훅 체인 앞단에서 링 먼저 비우기)

---

## 4. Tasks

### Task 1: 런처 배너 — 전송로를 런 기록에 못박는다

★엔진 재빌드 없음. `run_wall_dist.ps1` 한 파일.★ run 10 이 교란된 채 다섯 세션을 돌아다닌
직접 원인은 배너에 stderr 전송로가 arm 마다 다르다는 사실이 안 찍혔기 때문이다.

**Files:**
- Modify: `etc/multigpu/run_wall_dist.ps1` — 기동 배너(`1158` 부근의 `Write-Host` 분기)와
  분석부 머리(`1310` 직전)

**Interfaces:**
- Produces: 로그 파일 맨 앞과 콘솔 양쪽에 나가는 `RUN CONFIG` 블록. Task 7 의 거부 판정이
  이 블록의 필드를 읽는다.

- [ ] **Step 1: `RUN CONFIG` 블록을 콘솔과 분석 머리 양쪽에 찍는다**

필드 다섯: `stderr=` (`inherited-handle` / `redirect-pipe` / `nul`), `rust_log=` (전문, 생략 없이),
`duration_sec=`, `engine_md5=` (앞 12 자), `args=` (엔진에 넘긴 전체 인자열).
전송로 판정은 이미 있는 `$numaResolved -ge 0 -or -not $LegacyLogRedirect` 분기를 그대로 쓴다.

- [ ] **Step 2: 두 전송로로 짧은 런 2 회**

Run: `.\run_wall_dist.ps1 -DurationSec 5` 와 `.\run_wall_dist.ps1 -DurationSec 5 -LegacyLogRedirect`
Expected: 두 배너의 `stderr=` 가 각각 `inherited-handle` / `redirect-pipe`. `rust_log=` 는 동일.

- [ ] **Step 3: `-QuietLog` · `-NoLog` 에서도 블록이 나오는지**

Run: `.\run_wall_dist.ps1 -DurationSec 5 -QuietLog`, `… -NoLog`
Expected: `-NoLog` 에서도 콘솔에는 블록이 나온다(파일이 `NUL` 이라 파일 쪽은 없음).
`stderr=nul`.

- [ ] **Step 4: 커밋**

```bash
git add etc/multigpu/run_wall_dist.ps1
git commit -m "feat(wall): 런 배너에 stderr 전송로와 RUST_LOG 전문을 찍는다"
```

---

### Task 2: ★게이트 측정★ — 변수 하나씩 (사용자 실행, 코드 변경 없음)

> ★완료 (2026-10-27, run 11 / 12 런).★ 결과:
> `docs/superpowers/plans/notes/2026-10-27-log-arm-gate.md`
>
> **arm 은 저더를 가르지 않는다.** `dup%` 는 A 0.243/0.161/**3.889**,
> B 0.336/**3.947**/0.148, C 1.579/0.585/0.464 — 같은 arm 안의 분산이 arm 사이 차이의 25 배다.
> 가른 것은 ★기동 때 뽑히는 커밋 위상 하나★ 이고, 9/9 런에서
> `phase_pct(ms) = OUTPHASE vs_dwmvb_ms(primary) + t2c p50 + 0.12 ms` (잔차 폭 0.084 ms) 다.
> ⇒ 이 계획은 ***저더 대책이 아니라 측정 결정론화*** 다. Task 3~8 은 그대로 하되 근거가 바뀌고,
> Task 8 의 합격 판정은 아래에서 다시 썼다.

2.2~2.4 가 말하는 빈 칸을 메운다. ★이 결과가 Task 3 이후의 주장 범위를 정한다 —
작업 여부가 아니라 "저더 대책인가, 측정 위생인가" 를 정한다.★

**Files:** 없음. 실행과 집계뿐.

**Interfaces:**
- Consumes: Task 1 의 `RUN CONFIG` 블록.
- Produces: `docs/superpowers/plans/notes/2026-10-27-log-arm-gate.md` — arm × 런 표.

- [x] **Step 1: 네 arm 을 ★교대로★ 3 바퀴**

```
A = 풀 RUST_LOG + 상속 핸들      ← ★run 10 에 없는 칸★
B = 풀 RUST_LOG + -LegacyLogRedirect
C = -QuietLog  + 상속 핸들
D = -NoLog
```
순서는 `A B C D / A B C D / A B C D`. ★arm 별로 몰아 돌리지 말 것★ — 08-A 와 10-E 가 같은
구성인데 dup% 중앙값이 1.192 대 0.435 로 2.7 배 드리프트했다. 세션 간 드리프트가 arm 차이보다 컸다.
각 런 60 초, `multigpu.json`, `ticker_slice_probe.html?clip=none`, 같은 빌드.

- [x] **Step 2: 판정 지표를 ★나란히★ 집계**

주 지표는 `COMPREFRESH dup% = (refresh_d[2] + refresh_d[3]) / frames` 와 `windows_with_dup`.
부 지표로 `COMMITCYCLE phase_pct` p50 을 같은 표에 둔다. 육안 판정도 arm 라벨을 가리고 받는다.

> ★실행 시 라벨을 가리지 못했다.★ 운영자 판정은 arm 을 아는 상태에서 받았고, 그 판정
> ("D 가 3/3 로 최악, A ≈ C")은 계수기와 어긋난다(A_03 이 C_03 의 8 배). 노트의 "눈은 이
> 계수기를 안 따라갔다" 절 참고 — 가리지 못한 이상 이 불일치는 둘 중 어느 쪽 탓인지 못 가른다.

- [x] **Step 3: 두 질문에 답을 적는다**

1. ★A 가 C 와 겹치는가?★ 겹치면 — 로그량은 저더에 영향이 없다. 이 계획은 저더 대책이 아니라
   측정 위생으로 재분류하고, 저더 수사는 로깅과 무관한 곳으로 돌아간다.
2. ★A 가 B 와 겹치는가?★ 겹치면 — 교란의 정체는 전송로였고 기본값이 이미 고쳐져 있다.
   `-LegacyLogRedirect` 를 측정 런에서 금지한다.

phase_pct 만 갈리고 dup% 가 안 갈리면 ★phase_pct 는 저더 대리지표가 아니다★ 를 노트에 못박는다.

> ★이 줄은 틀렸다.★ 실제로는 phase_pct 와 dup% 가 **함께** 갈렸고, 다만 arm 을 따라 갈리지
> 않았다. phase_pct 는 쓸 수 있는 대리지표다 — 그것을 정하는 것이 로그가 아닐 뿐이다.

- [x] **Step 4: 노트 커밋**

```bash
git add docs/superpowers/plans/notes/2026-10-27-log-arm-gate.md
git commit -m "docs(wall): 로그 arm 게이트 측정 — 변수 하나씩 4 arm x 3"
```

---

### Task 3: 소음 제거 — 1,850 줄/s 를 약 90 줄/s 로

★가장 싸고 가장 큰 수정.★ 소비자 0 인 95.6% 를 pref 뒤로 보낸다. 이걸 먼저 하면 Task 4 의
링이 사실상 안 차고, 드롭 설계가 보험이 된다.

**Files:**
- Modify: `components/config/prefs.rs` — `gfx_wall_verbose_frame_log: bool` (기본 `false`)
- Modify: `components/paint/painter.rs:2433, 2634, 2845, 3529, 3898`
- Modify: `components/paint/dcomp_compositor.rs` — `[dcomp-dbg]` 17 자리
- Modify: `components/paint/paint.rs:1019, 1106, 2079, 2607`
- Modify: `vendor_local/webrender/src/tile_cache.rs:448`,
  `vendor_local/webrender/src/renderer/mod.rs:2008`
- Modify: `etc/multigpu/run_wall_dist.ps1` — `-VerboseFrameLog` 스위치, `1328` 의 fps 산출
- Test: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu`,
  `cargo test -p servo-config --test config_surface`

**Interfaces:**
- Produces: pref `gfx_wall_verbose_frame_log`. Task 4 의 레인 분류가 "이 pref 가 꺼진 상태에서
  남는 줄" 을 판정 레인으로 본다.

- [ ] **Step 1: pref 를 더하고 config_surface 가 ★실패★하는 것을 본다**

Run: `cargo test -p servo-config --test config_surface`
Expected: FAIL — surface 스냅샷에 새 pref 가 없다고 난다. (이 레포의 config_surface 는
pref 목록을 고정하는 시험이다. 먼저 실패를 보고 스냅샷을 갱신한다.)

- [ ] **Step 2: 호출 자리를 게이트 뒤로 옮긴다**

각 자리의 `info!(...)` 를 `if pref!(gfx_wall_verbose_frame_log) { info!(...) }` 로 감싼다.
★매크로 인자 평가까지 게이트 안으로 들어가야 한다★ — `format_args` 평가가 비용의 본체다.
webrender 두 자리는 pref 에 닿지 못하므로 ★크레이트 내부 `AtomicBool` 을 하나 두고 paint 가
기동 시 한 번 세운다★. 기존 주석은 지우지 않고 "pref 뒤로 옮긴 이유 + 2026-10-27 측정값
(이 자리 하나가 초당 N 줄)" 한 줄을 붙인다.

- [ ] **Step 3: `Wall render end` 의 fps 소비자를 끊는다**

`run_wall_dist.ps1:1328` 이 그 줄 240 줄/s 를 세어 ★페인터별 fps 정수 하나★를 만든다.
`WALLSPLIT` 의 창당 pass 수로 대체하거나, 그 한 줄만 1 Hz 롤업(`WALLFPS painter=.. fps=..`)으로
남긴다. ★둘 중 무엇을 골랐는지 커밋 메시지에 적는다.★

- [ ] **Step 4: 빌드와 테스트**

Run: 프렐류드 후 `cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release`
Run: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu`
Run: `cargo test -p servo-config --test config_surface`
Expected: 셋 다 PASS.

- [ ] **Step 5: 줄 수를 ★센다★**

60 초 풀 RUST_LOG 런(상속 핸들) 1 회. Expected: 총 줄/s 가 1,850 → 100 미만.
`-VerboseFrameLog` 를 붙이면 1,850 선으로 돌아온다. 판정 16 태그의 줄 수는 ★양쪽에서 동일★.

- [ ] **Step 6: 소비자 무손상 확인**

Run: 같은 로그로 분석부 전체.
Expected: 34 패턴 중 ★빈 절 0 개★. 특히 WALLSPLIT · WALLQUEUE · FRAMEID · FRAMETIME ·
RAFCLOCK · DCOMPSTAT(`out=`) · COMPREFRESH · TICKCOMMIT 여덟이 전부 값을 낸다.

- [ ] **Step 7: 커밋**

```bash
git add components/config/prefs.rs components/paint/painter.rs components/paint/dcomp_compositor.rs components/paint/paint.rs vendor_local/webrender/src/tile_cache.rs vendor_local/webrender/src/renderer/mod.rs etc/multigpu/run_wall_dist.ps1
git commit -m "perf(wall): 소비자 없는 info! 1,761 줄/s 를 gfx_wall_verbose_frame_log 뒤로"
```

---

### Task 4: `wall_log` — 스레드 로컬 SPSC + 전용 드레인

★`env_logger::Target::Pipe` 를 쓰지 않는다★(2.5). `log::Log` 를 직접 구현해 `BothLogger` 의
env_logger 자리를 대체한다.

**Files:**
- Create: `components/servo/wall_log.rs` (또는 `components/servo/src/wall_log.rs` — 크레이트
  레이아웃에 맞춘다)
- Modify: `components/servo/servo.rs:1127-1137 setup_logging`
- Test: `components/servo/wall_log.rs` 내부 `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 3 의 `gfx_wall_verbose_frame_log` — 꺼진 상태에서 남는 태그가 판정 레인.
- Produces:
  - `pub fn wall_log::install(filter: env_filter::Filter, path: Option<&Path>) -> Result<(), SetLoggerError>`
  - `pub fn wall_log::flush_and_shutdown()` — ★Task 6 이 `run_app` 복귀 직후 부른다★
  - 1 Hz 출력 줄 `WALLLOGDROP noise_dropped=<u64> verdict_dropped=<u64> ring_hwm_pct=<u8>`

- [ ] **Step 1: 실패하는 테스트 넷을 쓴다**

```rust
#[test] fn record_path_allocates_nothing()        // 스레드 로컬 버퍼 재사용 — 2 회차 호출의 할당 카운트 0
#[test] fn noise_lane_drops_verdict_lane_does_not() // 소음 레인을 가득 채워도 판정 레인은 전부 생존
#[test] fn drain_preserves_per_thread_order()     // 스레드별 순서 보존 + 레인 간 타임스탬프 머지
#[test] fn flush_and_shutdown_is_idempotent()     // 두 번 불러도 안전, 두 번째는 no-op
```

- [ ] **Step 2: 실패를 본다**

Run: `cargo test -p servo --lib wall_log`
Expected: FAIL — `wall_log` 가 없다.

- [ ] **Step 3: 구현**

설계 제약, 전부 2 절에서 나온 것:
- ★링은 스레드 로컬 SPSC★. 전역 큐 하나 금지(2.8). head/tail 시퀀스는 128 B 패딩.
  등록만 스레드 시작 때 1 회 전역 락.
- ★레인 둘★: `verdict`(판정 16 태그 + 존재 술어 — 드롭 불가, 작아서 안 참)와
  `noise`(나머지 — 넘치면 버리고 카운터). 분류는 메시지 머리의 태그 문자열로.
- ★레코드 경로 힙 할당 0★ — 스레드 로컬 재사용 `Vec<u8>` 에 포맷하고 링에 복사.
  포맷은 호출 스레드에서(지금 env_logger 도 그렇고, 락 밖이다). 출력 바이트열은
  ★현행 env_logger 기본 포맷과 한 글자도 다르지 않아야 한다★.
- 타임스탬프는 호출 측에서 QPC raw 만, 변환은 드레인에서.
- 드레인 스레드 하나: `BELOW_NORMAL`, ★렌더 NUMA 노드에 핀하지 말 것★, 10 ms 주기 또는
  링 50%. ★자기 전용 `File` 핸들만 쓰고 `std::io::stderr()` 는 절대 건드리지 않는다★ —
  건드리면 전역 `ReentrantLock` 으로 원위치다.
- 1 Hz 로 `WALLLOGDROP` 한 줄을 드레인 스레드가 직접 낸다(드롭 0 이어도 낸다 — Task 7 이
  그 줄의 ★부재★도 거부 사유로 쓴다).
- 패닉: 훅 체인 앞단에서 링을 먼저 비운다(std 기본 훅의 직접 stderr 쓰기와 순서 역전 방지).

- [ ] **Step 4: 테스트 통과**

Run: `cargo test -p servo --lib wall_log`
Expected: PASS 4/4.

- [ ] **Step 5: `setup_logging` 결선**

`EnvLoggerBuilder::from_env(env).build()` 자리를 `wall_log::install(...)` 로. ★`filter` 계산과
`set_max_level` 의 의미는 그대로 둔다★ — `max(env_logger, con_logger)` 가
`RUST_LOG=off` 에서도 WARN 을 살려 두는 현재 동작은 Task 5 에서 다룬다.

- [ ] **Step 6: 바이트 동일성 확인**

같은 `RUST_LOG`, 같은 페이지로 60 초 런 2 회(구 싱크 / 새 싱크).
Expected: 판정 16 태그 줄의 ★바이트열이 타임스탬프를 빼고 동일★. 분석부 34 패턴 출력 동일.

- [ ] **Step 7: 레코드당 비용을 ★잰다★ (추정 금지)**

`log!` 호출 자리를 QPC 로 감싸 렌더 스레드 넷이 살아 있는 벽에서 직접 측정, 1 Hz 히스토그램.
Expected: ★p50 ≤ 1 µs, p99 ≤ 10 µs, max ≤ 1 ms★, 렌더 스레드의 로깅 총 점유 ≤ 1 ms/s.
★max 가 16.67 ms 를 넘는 표본이 하나라도 나오면 실패★ — 링 용량이나 레인 분류로 되돌아간다.

- [ ] **Step 8: 커밋**

```bash
git add components/servo/wall_log.rs components/servo/servo.rs
git commit -m "perf(wall): 스레드 로컬 SPSC 링 + 전용 드레인 — 레코드당 p50 <측정값>"
```

---

### Task 5: `FromEmbedderLogger` 를 `winit_wall` 에서 뺀다

★`enabled()` 게이트가 아니라 미설치★(2.5). 게이트는 한 번도 차단하지 않는다.

**Files:**
- Modify: `components/servo/servo.rs:1127-1137 setup_logging`
- Modify: `components/config/prefs.rs` — 또는 `setup_logging` 에 인자 하나
- Test: `cargo test -p servo-config --test config_surface`

**Interfaces:**
- Consumes: Task 4 의 `wall_log::install`.
- Produces: `setup_logging` 이 constellation 로거 설치 여부를 받는 형태.

- [ ] **Step 1: 잃는 것이 없음을 ★코드로★ 확인하고 기록**

`LogEntry` 의 유일한 소비자는 `constellation.rs` 의 32 칸 `handled_warnings` 이고 트리 어디서도
읽지 않는다. `Panic` 분기는 `event_loop_id == None` 에서 즉시 return 하는데 페인트 스레드는
항상 `None` 이다. ★확인 결과를 커밋 메시지에 파일:줄로 적는다.★ devtools 소비자가 있으면
이 태스크는 여기서 멈추고 ruling 을 남긴다.

- [ ] **Step 2: servoshell 경로는 건드리지 않는다**

`set_logger`(`servo.rs:1349` 부근, `FromScriptLogger`)는 그대로. 가르는 것은 임베더 쪽
`setup_logging` 하나다.

- [ ] **Step 3: `set_max_level` 변화를 기록한다**

con_logger 의 `Warn` 이 빠지면 `filter` 가 env_logger 단독이 되어 ★`RUST_LOG=off` 가 실제로
off 가 된다.★ 이득이지만 `-NoLog` 런의 동작이 바뀌므로 런처 배너에 `max_level=` 을 더한다.

- [ ] **Step 4: 테스트와 측정**

Run: `cargo test -p servo-config --test config_surface`
Run: 60 초 풀 로그 런. Expected: WARN 141/s 경로의 `ReentrantMutex` + 힙 2 + send 가 사라진다 —
Task 4 Step 7 의 히스토그램에서 WARN 태그의 p99 가 INFO 태그 수준으로 내려온다.

- [ ] **Step 5: 커밋**

```bash
git add components/servo/servo.rs components/config/prefs.rs
git commit -m "perf(wall): winit_wall 에서 FromEmbedderLogger 미설치 — WARN 141/s 의 전역 ReentrantMutex 제거"
```

---

### Task 6: `--run-for-sec` + 런처 `Wait-Process`

버퍼를 넣은 순간 `run_wall_dist.ps1:1286-1288` 의 전제가 깨진다(2.7). 이 태스크 없이 Task 4 를
출하하지 않는다.

**Files:**
- Modify: `components/servo/examples/winit_wall/main.rs` — `Config`, `AppState:370` 부근,
  `about_to_wait:2373` 부근, `run_app` 복귀 직후 `main.rs:257`
- Modify: `etc/multigpu/run_wall_dist.ps1:1284-1289`

**Interfaces:**
- Consumes: `wall_log::flush_and_shutdown()` (Task 4).
- Produces: 엔진 인자 `--run-for-sec <f64>`, 로그 줄 `MEASUREEND qpc=<u64>`.

- [ ] **Step 0: ★공짜 검증부터★ — WM_CLOSE 가 정말 무시되는가**

Run: `-DurationSec` 런에서 `Stop-Process` 대신 `$proc.CloseMainWindow(); $proc.WaitForExit(15000)` 1 회.
Expected: 불명 — 이것이 질문이다. 내려오면 런처 주석(`:1286`)이 틀린 것이고 Task 6 의 엔진
변경 없이도 깨끗한 종료가 공짜로 생긴다. ★결과를 ruling 으로 남긴다.★ 안 내려오면 Step 1 로.

- [ ] **Step 1: 기한을 `capture_deadline` 과 같은 자리에 더한다**

`Config.run_for_sec: Option<f64>` → `AppState.run_deadline: Option<Instant>`(`main.rs:2207` 과
같은 자리) → `about_to_wait` 맨 앞에서 `should_exit.set(true)` + `MEASUREEND` 레코드.
★`ControlFlow::Poll` 로 바꾸지 말 것★ — capture 경로가 그렇게 하는데 그대로 베끼면 측정 런이
busy-poll 이 된다.

- [ ] **Step 2: 플러시는 `Drop` 이 아니라 ★명시 호출★**

`event_loop.run_app(&mut app)?` 복귀 직후, `drop(app)` ★전★에 `wall_log::flush_and_shutdown()`.
`main.rs:260`·`269` 의 `std::process::exit` 두 곳이 `Drop` 을 건너뛰므로 `Drop` 에 넣으면 안 된다.

- [ ] **Step 3: 런처 배선**

`-DurationSec` 이면 `--run-for-sec` 을 엔진에 넘기고 `Wait-Process -Timeout ($DurationSec + 15)`.
★`Stop-Process -Force` 는 지우지 않고 백스톱으로 남긴다★ — 발동하면 배너에
`TAIL_LOST=1` 과 경고를 찍는다. `1286-1287` 의 낡은 주석은 새 사실로 고친다.

- [ ] **Step 4: 꼬리 생존 확인**

Run: `-DurationSec 60` 3 회.
Expected: ★마지막 1 초 창의 COMPREFRESH · COMMITCYCLE 이 파일에 있다, 3/3.★
`MEASUREEND` 가 마지막 창 뒤에 있고 `TAIL_LOST=0`.

- [ ] **Step 5: 커밋**

```bash
git add components/servo/examples/winit_wall/main.rs etc/multigpu/run_wall_dist.ps1
git commit -m "feat(wall): --run-for-sec 으로 측정 런을 엔진이 끝낸다 — 강제 종료는 백스톱으로"
```

---

### Task 7: 무결성 자동 거부 — 경고가 아니라 거부

판정 줄 유실이 "깨끗한 런" 으로 읽히는 것을 막는다.

**Files:**
- Modify: `etc/multigpu/run_wall_dist.ps1` 분석부 머리

**Interfaces:**
- Consumes: Task 1 의 `RUN CONFIG`, Task 4 의 `WALLLOGDROP`, Task 6 의 `MEASUREEND` / `TAIL_LOST`.

- [ ] **Step 1: 거부 술어 넷**

① `WALLLOGDROP` 줄이 없거나 `verdict_dropped ≠ 0` ② 판정 16 태그 각각의 창 수가
`floor(DurationSec)` 과 ±2 를 넘게 어긋남 ③ `DCOMPSTAT out=` 이 ★출력 4 개 전부★가 아님
④ `TAIL_LOST=1`.

- [ ] **Step 2: 걸리면 ★저더 수치 출력 자체를 거부★**

경고를 찍고 계속 가지 않는다. `RUN REJECTED: <사유>` 한 줄을 내고 dup% · phase_pct 절을
아예 출력하지 않는다. 다른 절(WEBGL·TILESPIKE 등)은 그대로 낸다.

- [ ] **Step 3: 음성 시험**

Run: 로그 파일에서 `DCOMPSTAT out=\\.\DISPLAY3` 줄을 전부 지운 사본으로 분석부.
Expected: `RUN REJECTED: DCOMPSTAT 출력 3/4` — dup% 절이 ★안 나온다.★
(현재 코드는 그 GPU 를 "idle, not driven" 으로 조용히 빼고 계속 간다.)

- [ ] **Step 4: 커밋**

```bash
git add etc/multigpu/run_wall_dist.ps1
git commit -m "feat(wall): 무결성 술어 4 개 — 걸리면 저더 수치 출력을 거부한다"
```

---

### Task 8: 수렴 검증 — 이 계획의 합격 판정

**Files:**
- Create: `docs/superpowers/plans/notes/2026-10-27-log-io-convergence.md`

- [ ] **Step 1: A / C / D 를 교대로 3 바퀴**

Task 2 와 같은 조건, 같은 페이지, 같은 빌드. B(`-LegacyLogRedirect`)는 측정 런에서 제외한다.

- [ ] **Step 2: 합격 판정**

★run 11 을 받아 다시 썼다.★ 옛 판정("dup% 3 arm 의 범위가 서로 겹친다", "phase_pct p50
중앙값이 서로 5 점 이내")은 **쓸 수 없다**: 범위는 arm 과 무관하게 이미 겹치고(A 0.161~3.889),
phase_pct 는 기동 추첨이라 arm 을 바꾸지 않아도 80 점씩 뛴다. 두 지표 다 통과·불통과가
로그 작업이 아니라 추첨 운에 달린다.

판정은 ★위상을 공변량으로 묶은 뒤★ 본다. 각 런에 `OUTPHASE vs_dwmvb_ms`(primary)를 같이 적고:

- ★`vs_dwmvb` 가 안전대(≥ +0.9 ms)에 든 런만 모아서 3 arm 의 `dup%` 가 서로 겹친다.★
  안전대 밖 런은 버리지 말고 표에 남기되 판정에서 뺀다(뺀 사실과 개수를 노트에 적는다).
- ★`vs_dwmvb` 의 분포가 arm 에 따라 달라지지 않는다.★ run 11 에서 C 암만 +0.10/+0.10/+0.42 로
  뭉쳤다 — n=3 의 우연일 수 있으나, 기동 부하가 DWM 격자 재위상에 영향을 준다면 그것은
  ***로그 작업이 실제로 움직일 수 있는 유일한 지점*** 이므로 여기서 확인한다.
- 육안 판정을 ★arm 라벨 없이★ 받았을 때 순위가 랜덤. (Task 2 에서 가리지 못했다. 여기서는
  런 파일명을 먼저 섞고 판정을 받는다.)
- 모든 런에서 `verdict_dropped == 0`, `TAIL_LOST == 0`.

- [ ] **Step 3: ★불합격 해석이 더 중요하다★**

Task 2 가 이미 절반을 답했다 — ★위상을 움직인 것은 I/O 가 아니다.★ 그러므로 이 계획의
정상 출구는 "저더가 줄었다" 가 아니라 ***판정 줄이 유실 없이 나오고, 로그 구성이 어떤
판정 지표에도 나타나지 않는다*** 이다. 그것을 달성하면 합격이고, 저더 수사는 다음 질문으로
넘어간다: ***왜 엔진 기동이 패널 vblank 와 DWM 격자의 어긋남을 런마다 다시 뽑는가,
그리고 그것을 안전대에 고정할 수 있는가.***

- [ ] **Step 4: 노트 커밋**

```bash
git add docs/superpowers/plans/notes/2026-10-27-log-io-convergence.md
git commit -m "docs(wall): 로그 I/O 제거 후 수렴 검증 — <합격/불합격과 수치>"
```

---

## 5. 하지 않는 것

- ★바이너리 로깅★ — 텍스트 대 바이너리 압축비가 1.4 배뿐이고(이미 1 Hz 요약이라 양이 작다),
  ps1 분석부 34 패턴 + 내부 regex 13 곳을 다시 써야 한다. 동기는 용량이 아니라 포맷 비용이었는데
  그 비용은 Task 3 의 소음 제거로 20 배 줄어든다.
- ★ETW★ — 호출당 161~218 ns 로 쓸 만하지만 소비 체계(wpa/tracelog)를 새로 만들어야 하고
  현행 분석부와 접점이 없다. Task 8 이 불합격이고 더 낮은 바닥이 필요하면 그때 연다.
- ★mmap 싱크★ — `TerminateProcess` 생존이 실측(2/2)되었으나 Task 6 이 강제 종료 자체를
  없애므로 필요가 사라진다. 운영(무한) 런에서 요청 시 덤프가 필요해지면 그때.
- ★`-LegacyLogRedirect` 제거★ — 남겨 둔다. 다만 Task 1 의 배너와 Task 7 의 거부가 그것이
  측정 런에 섞이는 것을 막는다.
