# IP-wall 타일별 오디오 선택 표출 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 4개 미디어 경로(로컬 비디오/RTSP/캡처카드/WebRTC)에 오디오를 포함하고, 혼합 그리드 probe에서 타일별 `muted` 토글(가산)로 원하는 소스의 오디오를 골라 낼 수 있게 한다.

**Architecture:** 파이프라인 오디오 라우팅은 4경로 모두 이미 존재(playbin/MediaStream → autoaudiosink, `set_mute`→fakesink/autoaudiosink). 빠진 것은 (1) DOM `muted`와 (2) 캡처·WebRTC의 오디오 소스 미획득뿐. `gstwasapi2` 플러그인 한 줄로 오디오 장치 열거를 복구하고, 나머지는 혼합 그리드 probe + WebRTC 오디오 프로듀서 + 런타임 검증으로 구현한다. Servo는 autoplay-with-sound를 강제하지 않으므로(`is_allowed_to_play()`→`true`) 페이지 script `muted=false`가 제스처 없이 동작한다.

**Tech Stack:** GStreamer(gstwasapi2/wasapi2/autoaudiosink/opus), servo-media, HTML/JS(getUserMedia, RTCPeerConnection), servoshell wall.

## Global Constraints

- 작업 디렉터리: **`W:\servo_multigpu-tiled-wall`** (subst 필수; 없으면 `subst W: F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser`).
- 빌드/검사 전 PowerShell에서 `. W:\scripts\servo_env.ps1` 소싱 후 `$ErrorActionPreference='Continue'`. (PowerShell 도구가 exit 1로 깨지면 bash에서 `powershell.exe -NoProfile -Command "..."`로 우회.)
- **플러그인 변경(gstwasapi2)의 DLL 복사는 full `mach build` 필수** — `cargo build -p servoshell`은 미디어 피처 누락 + 플러그인 미복사. `windows_plugins()`가 `windows.rs.in` 각 항목을 `{plugin}.dll`로 target에 복사한다.
- release 최종 링크 lld-link 0xc0000005 시: `$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER`를 MSVC link.exe 풀경로로.
- 렌더 필요한 실행은 반드시 `--wall-all-tiles`; wall 레이아웃은 이 브랜치 스키마 필요(`config/wall_layout.local_*.json`이 파싱 실패하면 `etc/multigpu/config/wall_layout.test_2x1_samegpu.json` 또는 신규 display 스키마 `etc/multigpu/config/wall_layout.example_2x1_display.json` 사용).
- getUserMedia/WebRTC는 `--pref dom_webrtc_enabled=true` 필요(WebRTC는 `--pref dom_webrtc_transceiver_enabled=true`도).
- 백엔드 오디오 확인 로그는 `RUST_LOG=warn,servoshell=info,servo_media_gstreamer=info` 필요.
- 오디오 선택은 **가산(체크박스)**: 타일별 독립 `muted` 토글, 초기 전부 muted. 새 autoplay pref 없음. video↔audio 자동 페어링 없음(오디오는 수동 deviceId).
- 완료 조건: `cargo check -p servoshell` + `rustfmt --edition 2024 --check <touched .rs>` + `git diff --check`.
- 커밋 메시지 한국어 + 트레일러:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
  `Claude-Session: https://claude.ai/code/session_01UKn1GSL7Cq5V3ipA1eMcMt`

---

### Task 1: gstwasapi2 등록 + 오디오 장치 열거 복구 (enabler)

**Files:**
- Modify: `components/servo/gstreamer_plugin_lists/windows.rs.in`

**Interfaces:**
- Produces: servoshell 런타임에 `wasapi2` device provider 등록 → `navigator.mediaDevices.enumerateDevices()`가 audioinput(캡처카드 "Analog 0N Audio" 포함) 반환. Task 3의 캡처 오디오 전제.

- [ ] **Step 1: 플러그인 목록에 gstwasapi2 추가**

`windows.rs.in`의 배열에서 `"gstwasapi",` 다음 줄에 추가(끝 항목 콤마 주의 — 현재 마지막이 `"gstwinks"`라 콤마 없음; gstwasapi2는 중간에 삽입하므로 양쪽 콤마 유지):

```rust
[
// gst-plugins-bad
"gstwasapi",
// 오디오 입력 장치 열거(wasapi2 device provider) + 오디오 입출력 — 캡처카드 "Analog 0N Audio"
"gstwasapi2",
"gstd3d11",
// 카메라/캡처카드 소스(getUserMedia) — 1.28.4.100 커스텀 winks가 1.22.8 ks_video assert 회피
"gstmediafoundation",
"gstwinks"
]
```

- [ ] **Step 2: full mach build (플러그인 DLL 복사)**

```powershell
. W:\scripts\servo_env.ps1
$ErrorActionPreference='Continue'
cd W:\servo_multigpu-tiled-wall
.\mach build -j 8
```

Expected: exit 0. 확인: `Test-Path W:\servo_multigpu-tiled-wall\target\debug\gstwasapi2.dll` → True (mach가 복사).

- [ ] **Step 3: 오디오 장치 열거 스모크**

임시 페이지로 enumerateDevices 확인(스크래치, 미커밋):

```powershell
$probe = "W:\servo_multigpu-tiled-wall\.superpowers\sdd\enum_audio.html"
@'
<!doctype html><meta charset=utf-8><body><pre id=o>enumerating…</pre><script>
navigator.mediaDevices.enumerateDevices().then(ds=>{
  const a=ds.filter(d=>d.kind==="audioinput");
  document.getElementById("o").textContent="audioinput="+a.length+"\n"+
    a.map((d,i)=>`[${i}] "${d.label}" id=${d.deviceId||"(empty)"}`).join("\n");
  a.forEach((d,i)=>console.log(`[enum-audio] [${i}] "${d.label}" id=${d.deviceId}`));
}).catch(e=>{document.getElementById("o").textContent="FAILED "+e;console.log("[enum-audio] FAILED "+e);});
</script></body>
'@ | Set-Content -Encoding utf8 $probe
$env:RUST_LOG="warn,script=info,servo_media_gstreamer=info"
target\debug\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.test_2x1_samegpu.json --wall-all-tiles --pref dom_webrtc_enabled=true "file:///W:/servo_multigpu-tiled-wall/.superpowers/sdd/enum_audio.html" 2> ..\audio_enum_smoke.err.log
Select-String -Path ..\audio_enum_smoke.err.log -Pattern "enum-audio"
```

Expected: `audioinput=N` with N>0, 목록에 `MZ0380 PCI, Analog 0N Audio` 계열 포함(캡처카드 연결 시). 최소한 "Default Audio Capture Device"라도 나오면 provider 등록 성공. (창 닫아 종료.)

- [ ] **Step 4: Commit**

```bash
git add components/servo/gstreamer_plugin_lists/windows.rs.in
git commit -m "feat(media): gstwasapi2 등록 — 오디오 입력 장치 열거(wasapi2 provider) 복구"
```

---

### Task 2: 혼합 그리드 probe + 파일/RTSP 오디오 선택

**Files:**
- Create: `tests/html/multigpu_wall_audio_grid_probe.html`

**Interfaces:**
- Consumes: (없음 — HTML만)
- Produces: 4-타일 그리드 페이지. 파일/RTSP 타일 + 타일별 체크박스 오디오 토글. Task 3/4가 이 파일에 캡처·WebRTC 타일 로직을 채운다(placeholder 컨테이너 이미 배치).

- [ ] **Step 1: 그리드 probe 생성 (파일/RTSP 타일 + 선택 UI + 캡처/WebRTC 자리)**

`tests/html/multigpu_wall_audio_grid_probe.html` 신규:

```html
<!doctype html>
<!--
  IP-wall 스타일 타일별 오디오 선택 probe (2x2 그리드).
  4소스: 파일(?file=URL) / RTSP(?rtsp=URL) / 캡처(?capvid=<deviceId>&capaud=<deviceId>) /
  WebRTC(?signaling=ws://host:port). 각 타일에 체크박스 "🔊 audio" — 체크 시 그 <video>.muted=false.
  가산(독립): 여러 타일 동시 언뮤트 가능(OS 믹싱). 초기 전부 muted.
  Servo는 autoplay-with-sound를 강제하지 않으므로 script muted=false가 제스처 없이 동작.

  Run (wall):
    servoshell.exe --wall-layout <layout> --wall-all-tiles --pref dom_webrtc_enabled=true \
      --pref dom_webrtc_transceiver_enabled=true \
      "file:///W:/.../tests/html/multigpu_wall_audio_grid_probe.html?file=file:///W:/.../clip.mp4&rtsp=rtsp://127.0.0.1:8554/test"
-->
<html lang="en"><head><meta charset="utf-8"><title>Wall audio grid probe</title>
<style>
  html,body{margin:0;background:#0b0d12;color:#e8e8f0;font-family:system-ui,sans-serif}
  #grid{display:grid;grid-template-columns:1fr 1fr;grid-template-rows:1fr 1fr;gap:8px;
        width:100vw;height:100vh;box-sizing:border-box;padding:8px}
  .tile{position:relative;background:#111;border:2px solid #2a3350;border-radius:8px;overflow:hidden;
        display:flex;align-items:center;justify-content:center}
  .tile video{width:100%;height:100%;object-fit:contain;background:#000}
  .bar{position:absolute;left:0;top:0;right:0;display:flex;justify-content:space-between;
       align-items:center;padding:6px 10px;background:rgba(0,0,0,.6);font:14px/1.2 monospace;z-index:2}
  .bar b{color:#6cf}
  label.aud{cursor:pointer;user-select:none;padding:3px 8px;border-radius:6px;background:#233;color:#9fd}
  label.aud input{vertical-align:middle}
  .st{position:absolute;left:0;bottom:0;right:0;padding:4px 10px;background:rgba(0,0,0,.55);
      font:12px/1.3 monospace;color:#9aa;z-index:2;white-space:pre}
</style></head>
<body>
<div id="grid">
  <div class="tile" id="t-file">
    <div class="bar"><b>FILE</b><label class="aud"><input type="checkbox" data-for="file"> 🔊 audio</label></div>
    <video id="v-file" autoplay muted playsinline loop></video>
    <div class="st" id="s-file">file: idle</div>
  </div>
  <div class="tile" id="t-rtsp">
    <div class="bar"><b>RTSP</b><label class="aud"><input type="checkbox" data-for="rtsp"> 🔊 audio</label></div>
    <video id="v-rtsp" autoplay muted playsinline></video>
    <div class="st" id="s-rtsp">rtsp: idle</div>
  </div>
  <div class="tile" id="t-cap">
    <div class="bar"><b>CAPTURE</b><label class="aud"><input type="checkbox" data-for="cap"> 🔊 audio</label></div>
    <video id="v-cap" autoplay muted playsinline></video>
    <div class="st" id="s-cap">capture: idle</div>
  </div>
  <div class="tile" id="t-wrtc">
    <div class="bar"><b>WEBRTC</b><label class="aud"><input type="checkbox" data-for="wrtc"> 🔊 audio</label></div>
    <video id="v-wrtc" autoplay muted playsinline></video>
    <div class="st" id="s-wrtc">webrtc: idle</div>
  </div>
</div>
<script>
  const P = new URLSearchParams(location.search);
  const setStatus = (id,msg) => { const e=document.getElementById("s-"+id); if(e) e.textContent = id+": "+msg; };
  const clog = (id,msg) => console.log(`[audio-grid][${id}] ${msg}`);

  // --- per-tile additive audio toggle (the selection model) ---
  document.querySelectorAll("label.aud input").forEach(cb => {
    cb.addEventListener("change", () => {
      const id = cb.getAttribute("data-for");
      const v = document.getElementById("v-"+id);
      if (!v) return;
      v.muted = !cb.checked;               // checked = audible
      clog(id, "audio " + (cb.checked ? "ON (muted=false)" : "OFF (muted=true)"));
    });
  });

  // --- FILE tile: <video src> ---
  (function(){
    const url = P.get("file");
    const v = document.getElementById("v-file");
    if (!url) { setStatus("file","(no ?file= param)"); return; }
    v.src = url;
    v.addEventListener("loadedmetadata", () => setStatus("file", v.videoWidth+"x"+v.videoHeight));
    v.addEventListener("playing", () => clog("file","playing t="+v.currentTime.toFixed(2)));
    v.addEventListener("error", () => { setStatus("file","error"); clog("file","error"); });
    setInterval(()=>setStatus("file", `${v.videoWidth}x${v.videoHeight} t=${v.currentTime.toFixed(1)} muted=${v.muted}`), 1000);
  })();

  // --- RTSP tile: <video src=rtsp://> ---
  (function(){
    const url = P.get("rtsp");
    const v = document.getElementById("v-rtsp");
    if (!url) { setStatus("rtsp","(no ?rtsp= param)"); return; }
    v.src = url;
    v.addEventListener("loadedmetadata", () => setStatus("rtsp", v.videoWidth+"x"+v.videoHeight));
    v.addEventListener("playing", () => clog("rtsp","playing t="+v.currentTime.toFixed(2)));
    v.addEventListener("error", () => { setStatus("rtsp","error"); clog("rtsp","error"); });
    setInterval(()=>setStatus("rtsp", `${v.videoWidth}x${v.videoHeight} t=${v.currentTime.toFixed(1)} muted=${v.muted}`), 1000);
  })();

  // --- CAPTURE tile: filled in Task 3 ---
  window.__startCapture = null;
  if (P.get("capvid") || P.get("capaud")) {
    if (window.__startCapture) window.__startCapture(P);
    else setStatus("cap","(capture wiring pending — Task 3)");
  } else setStatus("cap","(no ?capvid=/?capaud= param)");

  // --- WEBRTC tile: filled in Task 4 ---
  window.__startWebrtc = null;
  if (P.get("signaling")) {
    if (window.__startWebrtc) window.__startWebrtc(P);
    else setStatus("wrtc","(webrtc wiring pending — Task 4)");
  } else setStatus("wrtc","(no ?signaling= param)");
</script>
</body></html>
```

- [ ] **Step 2: 파일 오디오 자산 확인 (오디오 트랙 유무)**

```powershell
$GSTBIN="W:\servo_multigpu-tiled-wall\target\dependencies\gstreamer\1.0\msvc_X86_64\bin"
& "$GSTBIN\gst-discoverer-1.0.exe" "W:\servo_multigpu-tiled-wall\tests\Wildlife_FHD30fps_counter_10Mbitrate.mp4" 2>&1 | Select-String -Pattern "audio|Audio"
```

Expected: 오디오 스트림 라인 출력(있으면 이 파일을 `?file=`로 사용; 없으면 오디오 트랙 있는 다른 파일로 대체하고 그 경로를 보고).

- [ ] **Step 3: 파일 오디오 스모크 (월, 토글)**

```powershell
. W:\scripts\servo_env.ps1
$ErrorActionPreference='Continue'; cd W:\servo_multigpu-tiled-wall
$env:RUST_LOG="warn,servoshell=info,servo_media_gstreamer=info"
$page="file:///W:/servo_multigpu-tiled-wall/tests/html/multigpu_wall_audio_grid_probe.html?file=file:///W:/servo_multigpu-tiled-wall/tests/Wildlife_FHD30fps_counter_10Mbitrate.mp4"
target\debug\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.test_2x1_samegpu.json --wall-all-tiles "$page" 2> ..\audio_file_smoke.err.log
```

창이 뜨면 FILE 타일의 "🔊 audio" 체크 → 소리 확인, 해제 → 무음 확인(육안/청각). 로그 확인:

```powershell
Select-String -Path ..\audio_file_smoke.err.log -Pattern "audio-grid|audio sink|autoaudiosink|fakesink"
```

Expected: 체크 시 `[audio-grid][file] audio ON` + `GStreamer audio sink restored ... autoaudiosink`, 해제 시 `audio OFF` + `audio sink disabled ... fakesink`. 월 타일 여러 개여도 오디오 sink는 1개(중복 로그 없음). panic 0.

- [ ] **Step 4: RTSP 오디오 스모크 (오디오 있는 RTSP 소스가 있을 때)**

로컬 RTSP 서버에 오디오 포함 스트림을 올려 `?rtsp=`로 검증. 오디오 포함 RTSP 소스를 즉석에서 만들 수 있으면:

```powershell
# 별도 터미널: 오디오+비디오 RTSP (gst-rtsp-server 계열이 있으면)
# 없으면 이 스텝은 "RTSP 오디오 소스 부재로 미검증"으로 보고하고, 파일 경로로 A/V 동기만 대표 검증.
```

Expected: RTSP 소스에 오디오가 있으면 토글로 가청; 없으면 그 사실을 보고(파일 타일이 대표 검증). RTSP 타일이 video-only여도 크래시 없이 토글 no-op.

- [ ] **Step 5: Commit**

```bash
git add tests/html/multigpu_wall_audio_grid_probe.html
git commit -m "test(html): 타일별 오디오 선택 혼합 그리드 probe — 파일/RTSP 오디오 + 가산 토글"
```

---

### Task 3: 캡처카드 오디오 타일

**Files:**
- Modify: `tests/html/multigpu_wall_audio_grid_probe.html` (CAPTURE 타일 로직 채우기)
- (조건부) Modify: `components/media/backends/gstreamer/media_capture.rs` — 오디오 캡처 caps 보정이 필요할 경우에만

**Interfaces:**
- Consumes: Task 1의 오디오 장치 열거, 기존 deviceId 인프라(`getUserMedia` audio deviceId)
- Produces: 캡처 타일이 video+audio MediaStream을 표시·가청

- [ ] **Step 1: CAPTURE 타일 스크립트 작성**

probe의 `window.__startCapture = null;` 줄을 다음으로 교체:

```javascript
  // --- CAPTURE tile: getUserMedia({video:{deviceId}, audio:{deviceId}}) ---
  window.__startCapture = async function(P){
    const v = document.getElementById("v-cap");
    const constraints = { video: true, audio: true };
    const capvid = P.get("capvid"), capaud = P.get("capaud");
    if (capvid) constraints.video = { deviceId: { exact: capvid } };
    if (capaud) constraints.audio = { deviceId: { exact: capaud } };
    else if (P.get("capaud") === "") constraints.audio = true;
    try {
      const stream = await navigator.mediaDevices.getUserMedia(constraints);
      v.srcObject = stream;
      const at = stream.getAudioTracks(), vt = stream.getVideoTracks();
      setStatus("cap", `v=${vt.length} a=${at.length}` + (at[0]?` "${at[0].label}"`:""));
      clog("cap", `tracks video=${vt.length} audio=${at.length} audioLabel=${at[0]&&at[0].label}`);
    } catch(e){ setStatus("cap","getUserMedia FAILED"); clog("cap","getUserMedia FAILED "+e); }
  };
```

(위 교체로 페이지 하단의 `if (P.get("capvid") ...)` 분기가 `window.__startCapture`를 호출하게 된다 — 그 분기는 Task 2에서 이미 배치됨.)

- [ ] **Step 2: 오디오 장치 목록 확보**

Task 1의 enum 스모크(또는 재실행)로 캡처카드 오디오 deviceId를 얻는다. 캡처카드 audio가 안 잡히면(`audioinput`에 "Analog 0N Audio" 없음) — 캡처카드 하드웨어/드라이버 문제이므로 그 사실을 보고하고, 가용한 다른 audioinput(예: "Default Audio Capture Device")으로 오디오 경로 자체를 대표 검증.

- [ ] **Step 3: 캡처 오디오 스모크**

```powershell
. W:\scripts\servo_env.ps1
$ErrorActionPreference='Continue'; cd W:\servo_multigpu-tiled-wall
$env:RUST_LOG="warn,servoshell=info,servo_media_gstreamer=info"
$vid="<캡처 video deviceId>"; $aud="<캡처 audio deviceId>"
$page="file:///W:/servo_multigpu-tiled-wall/tests/html/multigpu_wall_audio_grid_probe.html?capvid=$vid&capaud=$aud"
target\debug\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.test_2x1_samegpu.json --wall-all-tiles --pref dom_webrtc_enabled=true "$page" 2> ..\audio_cap_smoke.err.log
Select-String -Path ..\audio_cap_smoke.err.log -Pattern "audio-grid.*cap|getUserMedia:|audio sink|autoaudiosink"
```

Expected: `[audio-grid][cap] tracks video=1 audio=1`, CAPTURE 타일 토글 체크 시 가청(캡처 오디오), video와 동기. panic 0.

- [ ] **Step 4: (조건부) 오디오 캡처 caps 보정**

Step 3에서 audio 트랙은 1인데 소리가 안 나거나 negotiation이 멈추면(비디오 I420 고정과 유사한 proxy 경계 caps 거부), `media_capture.rs`의 오디오 캡처 경로에 `audio/x-raw` capsfilter를 명시적으로 삽입한다(비디오 `create_display_stream`의 I420 capsfilter 패턴 참조). **이 스텝은 Step 3이 실패할 때만 수행**하고, 필요 없으면 "caps 보정 불요"로 기록. 필요 시 `mach build` 재실행 후 Step 3 재검증.

- [ ] **Step 5: Commit**

```bash
git add tests/html/multigpu_wall_audio_grid_probe.html
# (caps 보정을 했다면) git add components/media/backends/gstreamer/media_capture.rs
git commit -m "test(html): 캡처카드 오디오 타일 — getUserMedia audio deviceId로 오디오 포함 표시"
```

---

### Task 4: WebRTC 오디오 타일

**Files:**
- Modify: `tests/html/multigpu_wall_audio_grid_probe.html` (WEBRTC 타일 로직 채우기)

**Interfaces:**
- Consumes: 기존 WebRTC 수신 체인(recvonly audio transceiver, on_incoming_stream), gst-plugins-rs 기본 시그널링
- Produces: WebRTC 타일이 오디오+비디오 트랙을 하나의 MediaStream으로 표시·가청

- [ ] **Step 1: WEBRTC 타일 스크립트 작성 (오디오 트랙 누적)**

probe의 `window.__startWebrtc = null;` 줄을 다음으로 교체(기존 수신 probe의 시그널링 클라이언트를 재사용하되, `ontrack`이 여러 번 발생하므로 트랙을 하나의 MediaStream에 누적):

```javascript
  // --- WEBRTC tile: gst-plugins-rs default signalling, audio+video ---
  window.__startWebrtc = function(P){
    const v = document.getElementById("v-wrtc");
    const SIGNALING = P.get("signaling") || "ws://127.0.0.1:8443";
    let ws, pc, sessionId=null, consuming=false;
    const inbound = new MediaStream();     // accumulate audio+video tracks here
    const wlog = (m)=>{ clog("wrtc", m); setStatus("wrtc", m); };
    const send = (o)=>{ try{ ws.send(JSON.stringify(o)); }catch(e){ wlog("send fail "+e); } };
    ws = new WebSocket(SIGNALING);
    ws.onopen = ()=> wlog("ws open");
    ws.onerror = ()=> wlog("ws error");
    ws.onmessage = async (ev)=>{
      let m; try{ m=JSON.parse(ev.data); }catch(e){ return; }
      switch(m.type){
        case "welcome": send({type:"setPeerStatus",roles:["Listener"]}); send({type:"list"}); break;
        case "list": if(!consuming && m.producers && m.producers.length) start(m.producers[0].id); break;
        case "peerStatusChanged": if(!consuming && m.roles && m.roles.includes("Producer")) start(m.peerId); break;
        case "sessionStarted": sessionId=m.sessionId; break;
        case "startSession": sessionId=m.sessionId||sessionId; if(m.offer) await offer(m.offer); break;
        case "peer":
          if(m.sdp) await offer(m.sdp);
          else if(m.ice && pc){ try{ await pc.addIceCandidate({candidate:m.ice.candidate,sdpMLineIndex:m.ice.sdpMLineIndex}); }catch(e){} }
          break;
      }
    };
    function start(pid){ consuming=true; wlog("startSession -> "+pid); mkpc(); send({type:"startSession",peerId:pid}); }
    function mkpc(){
      pc = new RTCPeerConnection();
      pc.ontrack = (e)=>{
        inbound.addTrack(e.track);          // accumulate: audio AND video land in one stream
        v.srcObject = inbound;
        const a=inbound.getAudioTracks().length, vt=inbound.getVideoTracks().length;
        wlog(`ontrack ${e.track.kind} (v=${vt} a=${a})`);
      };
      pc.onicecandidate = (e)=>{ if(e.candidate && sessionId) send({type:"peer",sessionId,ice:{candidate:e.candidate.candidate,sdpMLineIndex:e.candidate.sdpMLineIndex}}); };
    }
    async function offer(sdp){
      try{ if(!pc) mkpc();
        await pc.setRemoteDescription({type:sdp.type||"offer",sdp:sdp.sdp});
        const ans=await pc.createAnswer(); await pc.setLocalDescription(ans);
        send({type:"peer",sessionId,sdp:{type:"answer",sdp:pc.localDescription.sdp}});
        wlog("sent answer");
      }catch(e){ wlog("offer/answer fail "+e); }
    }
  };
```

- [ ] **Step 2: 오디오+비디오 프로듀서 실행 확인**

WebRTC 오디오 검증에는 오디오를 송출하는 프로듀서가 필요하다. `webrtcsink`에 오디오+비디오를 함께 넣는 파이프라인:

```powershell
# 시그널링 서버(gst-plugins-rs 기본, :8443)
F:\20260609_SDWall_BrowserTest\20260612_Webrtc\VF_PRSignallingServer.exe
# 오디오+비디오 프로듀서
gst-launch-1.0 videotestsrc is-live=true ! videoconvert ! webrtcsink name=ws signaller::uri=ws://127.0.0.1:8443 `
  audiotestsrc is-live=true wave=ticks ! audioconvert ! audioresample ! ws.
```

Expected: 프로듀서가 audio+video 트랙을 publish(로그에 오류 없음). webrtcsink의 audio pad 이름/문법이 다르면 gst-plugins-rs 문서 형식으로 조정하고 실제 사용한 명령을 보고.

- [ ] **Step 3: WebRTC 오디오 스모크**

```powershell
. W:\scripts\servo_env.ps1
$ErrorActionPreference='Continue'; cd W:\servo_multigpu-tiled-wall
$env:RUST_LOG="warn,servoshell=info,servo_media_gstreamer=info"
$page="file:///W:/servo_multigpu-tiled-wall/tests/html/multigpu_wall_audio_grid_probe.html?signaling=ws://127.0.0.1:8443"
target\debug\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.test_2x1_samegpu.json --wall-all-tiles --pref dom_webrtc_enabled=true --pref dom_webrtc_transceiver_enabled=true "$page" 2> ..\audio_wrtc_smoke.err.log
Select-String -Path ..\audio_wrtc_smoke.err.log -Pattern "audio-grid.*wrtc|ontrack|audio sink|autoaudiosink"
```

Expected: `[audio-grid][wrtc] ontrack audio (v=1 a=1)` (video·audio 둘 다), WEBRTC 타일 토글 체크 시 가청, video와 동기. panic 0. (오디오 트랙이 안 오면 프로듀서 SDP에 audio m-line이 있는지, recvonly transceiver가 붙는지 로그로 진단.)

- [ ] **Step 4: Commit**

```bash
git add tests/html/multigpu_wall_audio_grid_probe.html
git commit -m "test(html): WebRTC 오디오 타일 — audio+video 트랙 누적 표시(recvonly audio transceiver)"
```

---

### Task 5: 통합 월 스모크 + 검증 기록

**Files:** 산출물 `audio_*_smoke.err.log` (검증 후 삭제 가능)

- [ ] **Step 1: 정적 검증**

```powershell
. W:\scripts\servo_env.ps1
$ErrorActionPreference='Continue'; cd W:\servo_multigpu-tiled-wall
# (엔진 .rs를 건드린 경우에만) rustfmt --edition 2024 --check <touched .rs>
git diff --check
cargo check -p servoshell
```

Expected: diff 무출력, check 성공. (Task 3에서 media_capture.rs를 안 건드렸으면 rustfmt 대상 없음.)

- [ ] **Step 2: 4소스 혼합 통합 스모크 (가산 선택 + 월 1회)**

시그널링 서버 + 오디오 프로듀서를 띄운 상태로, 파일+RTSP(가능하면)+캡처(가능하면)+WebRTC를 한 번에:

```powershell
$env:RUST_LOG="warn,servoshell=info,servo_media_gstreamer=info"
$q="file=file:///W:/servo_multigpu-tiled-wall/tests/Wildlife_FHD30fps_counter_10Mbitrate.mp4&signaling=ws://127.0.0.1:8443"
# 캡처가 가용하면 &capvid=<id>&capaud=<id> 추가
$page="file:///W:/servo_multigpu-tiled-wall/tests/html/multigpu_wall_audio_grid_probe.html?$q"
target\debug\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.test_2x1_samegpu.json --wall-all-tiles --pref dom_webrtc_enabled=true --pref dom_webrtc_transceiver_enabled=true "$page" 2> ..\audio_integration_smoke.err.log
```

검증(육안/청각 + 로그):
- 각 타일 토글을 개별로 켜고 끄며 해당 소스 오디오만 제어됨(가산: 파일+WebRTC 동시 켜면 둘 다 들림).
- 여러 타일 언뮤트 시에도 오디오 sink 로그가 논리적으로 소스당 1개(월 타일 수만큼 중복 아님).
- A/V 동기 이상 없음, panic 0.

```powershell
Select-String -Path ..\audio_integration_smoke.err.log -Pattern "audio-grid|audio sink (restored|disabled)|panic"
```

- [ ] **Step 3: 스펙에 검증 결과 추기 + 커밋**

`docs/superpowers/specs/2026-07-24-wall-audio-per-tile-selection-design.md` 말미에 검증 결과(장치 열거 복구/파일·RTSP·캡처·WebRTC 각 가청 여부/가산 믹싱/월 1회/미검증 항목) 한국어 6-10줄로 추기:

```bash
git add docs/superpowers/specs/2026-07-24-wall-audio-per-tile-selection-design.md
git commit -m "docs: 타일별 오디오 선택 표출 검증 결과 기록"
```
