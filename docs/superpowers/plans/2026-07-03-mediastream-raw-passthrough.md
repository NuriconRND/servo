# MediaStream raw passthrough 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `<video>`에 MediaStream(카메라/WebRTC 수신)을 표시할 때의 불필요한 VP8/Opus 재인코딩+재디코딩을 제거해, raw 프레임이 그대로 플레이어→싱크→WebRender로 흐르게 한다(화질 회복 + 지연 감소).

**Architecture:** servo-media의 MediaStream→플레이어 브릿지(`media_stream_source.rs`)가 지금 `stream.encoded()`(vp8enc)로 재인코딩하는 것을, 신규 `raw()`(이미 raw인 tail 반환)로 교체한다. 동시에 `ServoMediaStreamSrc`의 ghost pad 템플릿 caps를 RTP→raw로 바꿔 playbin3의 decodebin3가 caps 기반 오토플러그로 **디코더를 스스로 생략**하게 한다. playbin3/decodebin3/appsink/render.rs/송출 경로는 무변경.

**Tech Stack:** Rust, GStreamer(gstreamer-rs), `gstproxy`(proxysink/proxysrc), playbin3/decodebin3, Servo I420 borrowed 비디오 경로.

## Global Constraints

- 변경 파일 = `components/media/backends/gstreamer/media_stream.rs` + `components/media/backends/gstreamer/media_stream_source.rs` **둘뿐**. WebRender·playbin3·render.rs·send 경로(webrtc.rs `encoded()`) **무변경**.
- `encoded()`/`caps()`/`caps_with_payload()`/`RTP_CAPS_VP8`/`RTP_CAPS_OPUS`는 **손대지 않음**(send 경로 공유).
- 표시 경로 기본 동작 = **raw**(신규). env `SERVO_MEDIASTREAM_ENCODE_DISPLAY=1` 설정 시에만 옛 `encoded()` 경로(A/B·디버그용 escape hatch).
- 효율 불변식: raw I420가 **추가 CPU 복사·CPU 색변환 없이** borrowed로 WebRender에 도달(YUV→RGB는 셰이더). I420 패스스루 유지가 조건.
- 커밋 메시지 한국어, Claude 서명/Co-Authored-By 제외. `git add`는 변경 파일 경로만(`-A`/`.` 금지 — 무관한 WebRTC/기타 변경 존재). Rust 주석 한국어 허용.
- 빌드: `mach build --release` — `etc/multigpu/servo_env.ps1` 소싱 후 `$ErrorActionPreference='Continue'` + 프로젝트-로컬 CARGO_HOME. media 크레이트 변경은 다운스트림 재빌드(수분, BACKGROUND 실행).
- 실행/검증: `etc/multigpu/run_presenter_wall.ps1`(카메라 출력; IndexedDB/WebRTC pref + `--ignore-certificate-errors`). dev 서버(5175) + 카메라가 살아있어야 함.

## 코드 앵커 (탐색 확정)

- `media_stream.rs:123-125` `src_element()` = `elements.last()` = raw tail(`create_video_from`의 `[source, videoconvert, queue]`의 `queue`).
- `media_stream.rs:197-243` `encoded()`(vp8enc/opusenc; **불변**). `:101-121` `caps()`/`caps_with_payload()`(**불변**).
- `media_stream_source.rs:16` import `RTP_CAPS_OPUS, RTP_CAPS_VP8`. `:23-41` pad 템플릿(현재 RTP). `:55-92` `set_stream`(line 71 `stream.encoded()`). `:94-146` `setup_proxy_src`(타입-제네릭).
- send 경로: `webrtc.rs:522-587`가 `stream.encoded()`(:554,569) 사용 — **불변**.
- 싱크: `render.rs:301-320` appsink caps `video/x-raw, I420`(활성). 로그 마커: `servo_media_gstreamer::player] ... pipeline element added: element=avdec_vp8...`(재인코딩 경로에서 나타남).

---

### Task 1: raw passthrough 구현 + 객관 검증

**Files:**
- Modify: `components/media/backends/gstreamer/media_stream.rs` (add `raw()`)
- Modify: `components/media/backends/gstreamer/media_stream_source.rs` (pad 템플릿 caps + import + `set_stream` 스왑)

**Interfaces:**
- Produces: `GStreamerMediaStream::raw(&mut self) -> gstreamer::Element` (재인코딩 없이 raw tail 반환). ghost pad 템플릿이 raw caps 광고. `set_stream`이 기본 raw(env로 옛 경로 전환).

- [ ] **Step 1: `raw()` 메서드 추가**

`components/media/backends/gstreamer/media_stream.rs`에서 `encoded()`가 끝나는 `}`(line 243) **다음 줄**에 삽입:
```rust
    /// 표시(display) 경로용 소스. MediaStream은 이미 raw(videoconvert 뒤 queue)이므로
    /// 재인코딩 없이 그 tail 엘리먼트를 그대로 반환한다. `encoded()`(송출용)와 달리
    /// 파이프라인에 새 엘리먼트를 추가하지 않는다. send 경로는 계속 `encoded()`를 쓴다.
    pub fn raw(&mut self) -> gstreamer::Element {
        self.src_element()
    }
```

- [ ] **Step 2: ghost pad 템플릿 caps를 raw로 변경**

`components/media/backends/gstreamer/media_stream_source.rs`의 `AUDIO_SRC_PAD_TEMPLATE`/`VIDEO_SRC_PAD_TEMPLATE`(line 23-41)를 아래로 교체:
```rust
    static AUDIO_SRC_PAD_TEMPLATE: LazyLock<gstreamer::PadTemplate> = LazyLock::new(|| {
        // raw 오디오를 그대로 흘려 playbin3/decodebin3가 디코더를 끼우지 않게 한다.
        let caps = gstreamer::Caps::builder("audio/x-raw").build();
        gstreamer::PadTemplate::new(
            "audio_src",
            gstreamer::PadDirection::Src,
            gstreamer::PadPresence::Sometimes,
            &caps,
        )
        .expect("Could not create audio src pad template")
    });

    static VIDEO_SRC_PAD_TEMPLATE: LazyLock<gstreamer::PadTemplate> = LazyLock::new(|| {
        // raw 비디오를 그대로 흘려 playbin3/decodebin3가 디코더를 끼우지 않게 한다.
        let caps = gstreamer::Caps::builder("video/x-raw").build();
        gstreamer::PadTemplate::new(
            "video_src",
            gstreamer::PadDirection::Src,
            gstreamer::PadPresence::Sometimes,
            &caps,
        )
        .expect("Could not create video src pad template")
    });
```

- [ ] **Step 3: import 정리**

같은 파일 line 16을 아래로 교체(이제 `RTP_CAPS_*` 미사용):
```rust
use crate::media_stream::GStreamerMediaStream;
```
(빌드에서 `RTP_CAPS_OPUS`/`RTP_CAPS_VP8`가 이 파일 다른 곳에서 여전히 쓰인다고 나오면 그 항목은 import에 남길 것 — 현재 탐색상 템플릿이 유일 사용처.)

- [ ] **Step 4: `set_stream`을 raw 기본으로 스왑(env 게이트)**

같은 파일 `set_stream`의 line 71:
```rust
            let last_element = stream.encoded().map_err(|_| PlayerError::SetStreamFailed)?;
```
을 아래로 교체:
```rust
            // 표시 경로: 재인코딩(vp8enc/opusenc) 없이 raw tail을 그대로 proxysink로 흘린다.
            // SERVO_MEDIASTREAM_ENCODE_DISPLAY 설정 시에만 옛 encoded() 경로(A/B·디버그용).
            let last_element = if std::env::var("SERVO_MEDIASTREAM_ENCODE_DISPLAY").is_ok() {
                stream.encoded().map_err(|_| PlayerError::SetStreamFailed)?
            } else {
                stream.raw()
            };
```

- [ ] **Step 5: 빌드**

Run (PowerShell, BACKGROUND — 수분):
```powershell
. "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\servo_env.ps1"
$ErrorActionPreference='Continue'
Set-Location "D:\2_TechReview\20260606_multigpu_browser\servo"
.\mach build --release
```
Expected: `Finished` (에러 없음). `RTP_CAPS_*` unused-import 경고가 나오면 Step 3처럼 정리.

- [ ] **Step 6: 객관 검증 — 플레이어 파이프라인에서 재인코딩/재디코드 소멸**

카메라 출력을 raw(기본)로 실행:
```powershell
& "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\run_presenter_wall.ps1" -Layout "etc\multigpu\config\wall_layout.example_1x1.json" -Detach
Start-Sleep -Seconds 12
$log = Get-ChildItem "D:\2_TechReview\20260606_multigpu_browser\servo\target\multigpu_logs\presenter_output_wall_release_stderr_*.log" | Sort-Object LastWriteTime | Select-Object -Last 1
"=== raw(default): player pipeline avdec_vp8 / vp8enc 있어야 없음 ==="
Select-String -Path $log.FullName -Pattern "servo_media_gstreamer::player.*element added.*avdec_vp8","vp8enc" | Select-Object -First 5
"=== 렌더 동작(프레임 흐름) ==="
Select-String -Path $log.FullName -Pattern "Wall render end|VideoFrameUpdated|new-sample" | Select-Object -Last 3
# 창 종료
Get-Process servoshell -ErrorAction SilentlyContinue | Stop-Process -Force
```
Expected: `avdec_vp8`(player)와 `vp8enc` 매치가 **0** (재인코딩·디코드#2 소멸), 렌더는 계속 동작(프레임 흐름 로그 존재). (참고: webrtc 수신측 decodebin의 디코드#1은 별개 파이프라인이라 남을 수 있음 — 여기서 보는 건 `servo_media_gstreamer::player` 카테고리.)

- [ ] **Step 7: A/B 대조 — env로 옛 경로면 avdec_vp8 재등장**

```powershell
$env:SERVO_MEDIASTREAM_ENCODE_DISPLAY = "1"
& "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\run_presenter_wall.ps1" -Layout "etc\multigpu\config\wall_layout.example_1x1.json" -Detach
Start-Sleep -Seconds 12
$log = Get-ChildItem "D:\2_TechReview\20260606_multigpu_browser\servo\target\multigpu_logs\presenter_output_wall_release_stderr_*.log" | Sort-Object LastWriteTime | Select-Object -Last 1
Select-String -Path $log.FullName -Pattern "servo_media_gstreamer::player.*element added.*avdec_vp8" | Select-Object -First 3
Get-Process servoshell -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-Item Env:\SERVO_MEDIASTREAM_ENCODE_DISPLAY
```
Expected: 이번엔 `avdec_vp8`(player) 매치가 **존재**(옛 재인코딩 경로 복원) → 스왑이 실제로 경로를 바꾼다는 A/B 확증.

- [ ] **Step 8: 커밋**
```powershell
git add components/media/backends/gstreamer/media_stream.rs components/media/backends/gstreamer/media_stream_source.rs
git commit -m "MediaStream 표시 경로 raw passthrough: 불필요한 VP8/Opus 재인코딩 제거"
```

---

### Task 2: 회귀 + 증상 검증 + 메모리 갱신

**Files:**
- Modify: `C:\Users\ilwoonam75\.claude\projects\D--2-TechReview-20260606-multigpu-browser\memory\presenter-webrtc-wall.md` (근본원인+수정 기록)

**Interfaces:**
- Consumes: Task 1의 raw passthrough. Produces: 검증 결과 + 메모리 기록. (코드 변경 없음.)

- [ ] **Step 1: 회귀 — 오디오/수신/파일비디오/프리뷰**

카메라 출력 실행(raw 기본) 후 로그로 확인:
```powershell
& "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\run_presenter_wall.ps1" -Layout "etc\multigpu\config\wall_layout.example_1x1.json" -Detach
Start-Sleep -Seconds 12
$log = Get-ChildItem "D:\2_TechReview\20260606_multigpu_browser\servo\target\multigpu_logs\presenter_output_wall_release_stderr_*.log" | Sort-Object LastWriteTime | Select-Object -Last 1
"=== 오디오 싱크 살아있나(wasapisink) ==="
Select-String -Path $log.FullName -Pattern "wasapisink|audio" | Select-Object -Last 3
"=== WebRTC 수신 connected + 렌더 지속 ==="
Select-String -Path $log.FullName -Pattern "PC state: connected|Wall render end" | Select-Object -Last 3
"=== 신규 에러/패닉 없나(cert/moz/webkit 제외) ==="
Select-String -Path $log.FullName -Pattern "ERROR|panic|Failed to negotiate|not-negotiated|reject" | Where-Object { $_ -notmatch "certificate|moz-|webkit-|getReceivers" } | Select-Object -First 10
Get-Process servoshell -ErrorAction SilentlyContinue | Stop-Process -Force
```
Expected: 오디오 싱크 동작, WebRTC connected + 렌더 지속, **`not-negotiated`/caps 협상 실패 없음**(raw caps 협상 성공 신호). getReceivers는 선재(무관). `not-negotiated`가 나오면 → 리스크 ① (pad 템플릿/caps) 재점검: 템플릿 caps를 `gstreamer::Caps::new_any()`로 완화 시도.

- [ ] **Step 2: 증상 검증(사용자 육안, 핸드오프)**

Servo 네이티브 창은 컨트롤러가 못 봄 → **사용자가** raw(기본) 실행 화면을 Chrome과 비교:
- 움직일 때 **블록 아티팩트 사라졌는지**(화질 회복).
- **delay 줄었는지**(선택: 카메라를 스톱워치/시계에 대고 화면-실제 오프셋 비교; A/B는 `SERVO_MEDIASTREAM_ENCODE_DISPLAY=1`로 옛 경로와 같은 세션 비교).
Expected: 아티팩트 개선 + 지연 감소를 사용자가 확인. (이게 최종 수용 기준.)

- [ ] **Step 3: 메모리 갱신**

`memory/presenter-webrtc-wall.md`에 추가: "카메라 화질 저하/지연 근본원인 = servo-media가 MediaStream 표시 시 vp8enc 재인코딩→avdec_vp8 재디코딩(`media_stream_source.rs:71` `encoded()`). **수정: 표시 경로 raw passthrough**(신규 `raw()`+ghost pad raw caps, env `SERVO_MEDIASTREAM_ENCODE_DISPLAY`로 옛 경로 A/B). send 경로 무변경. 커밋 <sha>." (실제 sha·검증결과로 채움 — 플레이스홀더 금지.) `MEMORY.md` hook도 갱신.

- [ ] **Step 4: (조건부) env 게이트 처리**

사용자 육안 확인(Step 2)이 개선을 확정하면: env 게이트를 **디버그 escape hatch로 유지**(Servo 관례, zero-cost)하거나 스펙대로 제거. **기본 권고 = 유지**(향후 회귀 디버깅에 유용). 제거를 택하면 Task 1 Step 4의 `if/else`를 `let last_element = stream.raw();`로 단순화 후 별도 커밋. (이 스텝은 사용자 확인 전엔 진행하지 않음.)

---

## Self-Review

**Spec coverage:**
- §3 변경 3곳(raw()/set_stream 스왑/pad caps) → Task 1 Step 1-4. ✓
- §4 효율(I420 borrowed 무복사) → 아키텍처상 자동 성립; Task 2 Step 1의 `not-negotiated` 부재로 I420 패스스루 간접 확인. ✓
- §6 리스크: ① pad 템플릿 caps → Task 1 Step 2(+Task 2 Step 1의 완화 fallback). ② decodebin3 raw-bypass → Task 1 Step 6(avdec_vp8 소멸=디코더 생략 확증). ③ I420 패스스루 → Task 2 Step 1. ④ 오디오 → Task 1 Step 2(audio 템플릿) + Task 2 Step 1(wasapisink). ⑤ blast radius → Task 2 Step 1(수신/오디오/렌더). ⑥ send 무변경 → 전 태스크 webrtc.rs 미변경. ✓
- §6 검증: dot 덤프 대신 **동등한 로그-기반 element-added 검증**(avdec_vp8/vp8enc 소멸) 채택(더 신뢰 가능, 버그를 처음 찾은 그 마커) + A/B + 육안. ✓
- §7 비목표(send/GL/RTCP/다중스트림) → 계획에서 안 다룸. ✓

**Placeholder scan:** Task 2 Step 3의 `<sha>`/검증결과는 실행 시 실측으로 채우는 지점(규칙 명시). Step 4는 사용자 확인 조건부(미완 아님, 명시적 게이트). TBD/TODO 없음. ✓

**Type 일관성:** `raw(&mut self) -> gstreamer::Element`가 Task 1 Step 1 정의 ↔ Step 4 호출부 일치(둘 다 Element 반환, `encoded()`는 Result라 `?`/`.map_err` 유지). env 이름 `SERVO_MEDIASTREAM_ENCODE_DISPLAY` 전 스텝 동일. pad 템플릿 caps `audio/x-raw`/`video/x-raw` 일관. ✓

## 리스크 노트(실행 시)
- **가장 큰 미확정 = Task 1 Step 6의 avdec_vp8 소멸 여부.** 소멸하면 decodebin3 raw-bypass 성립 = fix 성공. 안 소멸하거나 `not-negotiated`면 → pad 템플릿 caps를 `Caps::new_any()`로 바꿔 재시도(리스크 ①), 그래도 안 되면 decodebin3가 raw를 안 받는 것 → 스펙 §6-② 재검토(플레이어측에 raw 강제 경로 필요할 수 있음, 범위 재산정).
