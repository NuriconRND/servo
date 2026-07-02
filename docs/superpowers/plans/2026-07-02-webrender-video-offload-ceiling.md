# WebRender 비디오 offload — 천장 검증 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** N개 비디오를 "한 병합 레이어"로 접으면 45~64 타일 60fps가 되는지를, 엔진 변경 없이 합성 WebGL 하네스로 측정해 C2(내부 fast-path) 착수 go/no-go를 숫자로 판정한다.

**Architecture:** 새 WebGL 하네스(`video_collapse_ceiling.html`)가 N개 quad를 단일 canvas(=단일 WebRender 레이어)에 그린다 — V1(공유 텍스처 1개, 업로드 0 = 낙관 상한), V2(타일별 텍스처 N개 매 프레임 재업로드 = 보수 상한). 기존 `video_grid_6x6_perf.html`(N개 진짜 `<video>` = N 레이어)이 Baseline B. 각각 `?dom=1`로 DOM 오버레이 혼합 케이스 측정. 기존 계측(`SERVO_LOG_PRESENT_CADENCE`)을 재사용해 present cadence + wr_update/wr_render를 수집한다.

**Tech Stack:** WebGL1 + 인라인 JS(ES module 금지, `file://` 로딩), PowerShell 런처/스윕/파서, servoshell(release), 기존 Rust 계측(env-gated, 코드 변경 없음).

## Global Constraints

- 엔진(Rust/C++) 소스 변경 **0**. 산출물은 HTML/JS + PowerShell + Markdown 뿐.
- 페이지는 인라인 `<script>`만 사용(ES module 금지) → `file:///` 로딩 유지.
- WebGL1 사용(최대 호환). 텍스처는 RGBA/UNSIGNED_BYTE, 소스 해상도 `TEX_W=1920, TEX_H=1080`.
- 계측은 기존 env 재사용: `SERVO_LOG_PRESENT_CADENCE=1` + `RUST_LOG=warn,paint=info`. 코드 변경 없음.
- 측정 머신: 합성 NVIDIA A5000 / 디스플레이 Intel iGPU @60Hz, servoshell **headed**.
- servoshell 경로: `target/release/servoshell.exe`. 페이지 로딩은 `file:///` + (백슬래시→슬래시) 절대경로 + 쿼리.
- 로그 라인: 페이지는 `console.error("[CEILING] ...")` / `console.error("[GRIDPERF] ...")` → servoshell **stderr**. 계측은 `Present cadence: ...` → 같은 stderr(`RUST_LOG` 필요).
- 그리드 매핑(cols×rows): **N=30→6×5, 40→8×5, 45→9×5, 64→8×8**. (clampInt 상한 16.)
- 커밋 메시지는 **한국어**, Claude 서명 제외.
- 검증 스모크런은 servoshell을 `Start-Process ... -PassThru`로 detached 실행 → `Start-Sleep` → `Stop-Process`, stderr 리다이렉트 로그를 `Select-String`으로 확인.

---

### Task 1: 천장 WebGL 하네스 (V1 + V2 + DOM)

**Files:**
- Create: `tests/html/video_collapse_ceiling.html`

**Interfaces:**
- Produces: 페이지 URL 파라미터 계약 — `?grid=N` | `?cols=C&rows=R`, `?mode=v1|v2`(기본 v1), `?dom=0|1`(기본 0), `?log=1`. `?log=1`일 때 매 1초 stderr에 `[CEILING] mode=<m> tiles=<n> dom=<0|1> fps=<f> maxGapMs=<g>` 출력. 초기화 시 `[CEILING] init gl=ok mode=.. tiles=.. dom=..` 1회 출력. Task 4 스윕이 이 URL 계약과 로그 포맷에 의존.

- [ ] **Step 1: 하네스 파일 작성**

Create `tests/html/video_collapse_ceiling.html`:

```html
<!doctype html>
<html lang="ko">
<head>
<meta charset="utf-8">
<title>Video Collapse Ceiling (WebGL)</title>
<style>
  :root { color-scheme: dark; }
  html, body { margin:0; width:100%; height:100%; overflow:hidden; background:#000;
    font-family: ui-monospace, SFMono-Regular, Consolas, monospace; color:#f4f4f4; }
  #gl { position:fixed; inset:0; width:100vw; height:100vh; display:block; }
  /* DOM 오버레이(?dom=1): 각 타일 위에 테두리+라벨. 캔버스 위(z=5) 합성. */
  #dom { position:fixed; inset:0; pointer-events:none; z-index:5; }
  .cap { position:absolute; box-sizing:border-box; border:2px solid rgba(0,220,255,.9);
    color:#eafcff; font-size:14px; padding:2px 4px; text-shadow:0 1px 2px #000; overflow:hidden; }
  #stats { position:fixed; left:16px; top:16px; z-index:10; padding:12px 16px;
    background:rgba(0,0,0,.78); border:1px solid rgba(255,255,255,.34);
    font-size:20px; line-height:1.4; white-space:pre; pointer-events:none; }
</style>
</head>
<body>
<canvas id="gl"></canvas>
<div id="dom"></div>
<div id="stats">loading…</div>
<script>
"use strict";
// 파라미터: ?grid=N | ?cols=C&rows=R, ?mode=v1|v2, ?dom=0|1, ?log=1
const params = new URLSearchParams(location.search);
const clampInt = (raw, fb) => { const n = parseInt(raw,10); return Number.isFinite(n) ? Math.max(1, Math.min(16, n)) : fb; };
const gridN = clampInt(params.get("grid"), 8);
const COLS = clampInt(params.get("cols"), gridN);
const ROWS = clampInt(params.get("rows"), gridN);
const TILE_COUNT = COLS * ROWS;
const MODE = params.get("mode") === "v2" ? "v2" : "v1";
const DOM = params.get("dom") === "1";
const LOG = params.get("log") === "1";
const TEX_W = 1920, TEX_H = 1080; // 각 소스 텍스처를 실제 비디오 타일 해상도로 (업로드 비용 현실화)

const canvas = document.getElementById("gl");
const stats = document.getElementById("stats");
const domLayer = document.getElementById("dom");

// 캔버스 백버퍼를 창 픽셀(devicePixelRatio 반영)에 맞춤
function resize() {
  const dpr = window.devicePixelRatio || 1;
  canvas.width = Math.round(window.innerWidth * dpr);
  canvas.height = Math.round(window.innerHeight * dpr);
}
resize();
window.addEventListener("resize", resize);

const gl = canvas.getContext("webgl", { antialias:false, depth:false, alpha:false, premultipliedAlpha:false });
if (!gl) {
  const msg = "[CEILING] ERROR webgl context is null";
  stats.textContent = msg; console.error(msg); throw new Error(msg);
}
console.error("[CEILING] init gl=ok mode=" + MODE + " tiles=" + TILE_COUNT + " dom=" + (DOM?1:0));

// --- 셰이더: 단위 사각형을 (translate,scale)로 배치 후 텍스처 샘플 ---
const VS = `
attribute vec2 aPos;      // 0..1 unit quad
uniform vec2 uTrans;      // clip-space translate (타일 좌하단)
uniform vec2 uScale;      // clip-space scale (타일 크기)
varying vec2 vUV;
void main() {
  vUV = vec2(aPos.x, 1.0 - aPos.y);
  gl_Position = vec4(uTrans + aPos * uScale, 0.0, 1.0);
}`;
const FS = `
precision mediump float;
varying vec2 vUV;
uniform sampler2D uTex;
uniform float uTint;      // 프레임마다 변해 드라이버 no-op 방지 + 타일 식별
void main() {
  vec4 c = texture2D(uTex, vUV);
  gl_FragColor = vec4(c.rgb * (0.6 + 0.4 * uTint), 1.0);
}`;
function compile(type, src) {
  const s = gl.createShader(type);
  gl.shaderSource(s, src); gl.compileShader(s);
  if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) {
    const e = "[CEILING] ERROR shader: " + gl.getShaderInfoLog(s); console.error(e); throw new Error(e);
  }
  return s;
}
const prog = gl.createProgram();
gl.attachShader(prog, compile(gl.VERTEX_SHADER, VS));
gl.attachShader(prog, compile(gl.FRAGMENT_SHADER, FS));
gl.linkProgram(prog);
if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) {
  const e = "[CEILING] ERROR link: " + gl.getProgramInfoLog(prog); console.error(e); throw new Error(e);
}
gl.useProgram(prog);
const aPos = gl.getAttribLocation(prog, "aPos");
const uTrans = gl.getUniformLocation(prog, "uTrans");
const uScale = gl.getUniformLocation(prog, "uScale");
const uTint = gl.getUniformLocation(prog, "uTint");
const uTex = gl.getUniformLocation(prog, "uTex");

const quad = new Float32Array([0,0, 1,0, 0,1, 0,1, 1,0, 1,1]);
const vbo = gl.createBuffer();
gl.bindBuffer(gl.ARRAY_BUFFER, vbo);
gl.bufferData(gl.ARRAY_BUFFER, quad, gl.STATIC_DRAW);
gl.enableVertexAttribArray(aPos);
gl.vertexAttribPointer(aPos, 2, gl.FLOAT, false, 0, 0);
gl.uniform1i(uTex, 0);
gl.activeTexture(gl.TEXTURE0);

// 소스 픽셀 버퍼 풀: 소수의 1080p RGBA 버퍼 재사용(JS 메모리 절약). 업로드 비용은
// texImage2D 호출당 발생하므로 소스를 공유해도 병목 측정엔 무방.
const POOL = 4;
const srcBufs = [];
for (let p = 0; p < POOL; p++) {
  const buf = new Uint8Array(TEX_W * TEX_H * 4);
  for (let i = 0; i < buf.length; i += 4) {
    const px = i >> 2;
    buf[i] = (px + p*60) & 255; buf[i+1] = (px >> 3) & 255; buf[i+2] = (p*63) & 255; buf[i+3] = 255;
  }
  srcBufs.push(buf);
}

function makeTex(buf) {
  const t = gl.createTexture();
  gl.bindTexture(gl.TEXTURE_2D, t);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, TEX_W, TEX_H, 0, gl.RGBA, gl.UNSIGNED_BYTE, buf);
  return t;
}
// v1: 공유 텍스처 1개(업로드 0). v2: 타일별 텍스처 N개.
let sharedTex = null, tileTex = [];
if (MODE === "v1") sharedTex = makeTex(srcBufs[0]);
else for (let i = 0; i < TILE_COUNT; i++) tileTex.push(makeTex(srcBufs[i % POOL]));

// v2: 매 프레임 각 타일 텍스처를 소스 버퍼에서 재업로드(실제 비디오 프레임 도착 모사)
function uploadFrame(frameIdx) {
  for (let i = 0; i < TILE_COUNT; i++) {
    gl.bindTexture(gl.TEXTURE_2D, tileTex[i]);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, TEX_W, TEX_H, 0, gl.RGBA, gl.UNSIGNED_BYTE, srcBufs[(i+frameIdx) % POOL]);
  }
}

// DOM 오버레이(?dom=1): 타일마다 테두리+라벨, 캔버스 위 합성
const caps = [];
if (DOM) {
  for (let i = 0; i < TILE_COUNT; i++) {
    const c = document.createElement("div");
    c.className = "cap";
    const col = i % COLS, row = (i / COLS) | 0;
    c.style.left = (col*100/COLS) + "vw"; c.style.top = (row*100/ROWS) + "vh";
    c.style.width = (100/COLS) + "vw"; c.style.height = (100/ROWS) + "vh";
    c.textContent = "TILE " + i;
    domLayer.appendChild(c); caps.push(c);
  }
}

// 타일 배치(clip space) 사전 계산
const tiles = [];
for (let i = 0; i < TILE_COUNT; i++) {
  const col = i % COLS, row = (i / COLS) | 0;
  const sx = 2/COLS, sy = 2/ROWS;
  tiles.push({ tx: -1 + col*sx, ty: 1 - (row+1)*sy, sx, sy });
}

function draw(frameIdx) {
  gl.viewport(0, 0, canvas.width, canvas.height);
  gl.clearColor(0,0,0,1); gl.clear(gl.COLOR_BUFFER_BIT);
  gl.uniform2f(uScale, tiles[0].sx, tiles[0].sy);
  for (let i = 0; i < TILE_COUNT; i++) {
    const t = tiles[i];
    gl.bindTexture(gl.TEXTURE_2D, MODE === "v1" ? sharedTex : tileTex[i]);
    gl.uniform2f(uTrans, t.tx, t.ty);
    gl.uniform1f(uTint, ((i + frameIdx) % 30) / 30);
    gl.drawArrays(gl.TRIANGLES, 0, 6);
  }
}

// fps/지터 측정 + ?log=1 로그 (video_grid_6x6_perf.html 패턴 재사용)
const startTime = performance.now();
let lastSample = startTime, rafSince = 0, lastFrameTs = startTime, maxGap = 0, jitter = 0, fps = 0, frameIdx = 0;
function tick(now) {
  rafSince++; frameIdx++;
  const gap = now - lastFrameTs; if (gap > maxGap) maxGap = gap; lastFrameTs = now;

  if (MODE === "v2") uploadFrame(frameIdx);
  draw(frameIdx);
  if (DOM) for (let i = 0; i < caps.length; i++)
    caps[i].style.borderColor = ((frameIdx+i) & 16) ? "rgba(0,220,255,.9)" : "rgba(255,140,0,.9)";

  const dt = now - lastSample;
  if (dt >= 1000) {
    fps = (rafSince*1000)/dt; jitter = maxGap;
    if (LOG) console.error(`[CEILING] mode=${MODE} tiles=${TILE_COUNT} dom=${DOM?1:0} fps=${fps.toFixed(1)} maxGapMs=${jitter.toFixed(1)}`);
    maxGap = 0; rafSince = 0; lastSample = now;
  }
  stats.textContent = [
    `collapse ceiling (WebGL)`,
    `mode  : ${MODE}  (${MODE==="v1"?"shared tex, no upload":"N tex, per-frame upload"})`,
    `grid  : ${COLS}x${ROWS} = ${TILE_COUNT}`,
    `dom   : ${DOM?"on":"off"}`,
    `rAF FPS: ${fps.toFixed(1)}`,
    `jitter(maxGap): ${jitter.toFixed(1)} ms`,
  ].join("\n");
  requestAnimationFrame(tick);
}
requestAnimationFrame(tick);
</script>
</body>
</html>
```

- [ ] **Step 2: V1 스모크 — GL 초기화 + N quad 렌더 확인**

Run (PowerShell):
```powershell
$root = "D:\2_TechReview\20260606_multigpu_browser\servo"
$exe  = "$root\target\release\servoshell.exe"
$page = "$root\tests\html\video_collapse_ceiling.html"
$url  = "file:///" + ($page -replace '\\','/') + "?grid=8&mode=v1&dom=0&log=1"
$err  = "$root\target\multigpu_logs\smoke_v1.stderr.log"
New-Item -ItemType Directory -Force (Split-Path $err) | Out-Null
$p = Start-Process $exe -ArgumentList @("--window-size","1920x1080",$url) -WorkingDirectory $root -RedirectStandardError $err -RedirectStandardOutput "$root\target\multigpu_logs\smoke_v1.stdout.log" -PassThru
Start-Sleep -Seconds 8; if(!$p.HasExited){ Stop-Process -Id $p.Id -Force }
Select-String -Path $err -Pattern "\[CEILING\] init gl=ok","\[CEILING\] mode=v1 tiles=64" | Select-Object -First 5
```
Expected: `init gl=ok mode=v1 tiles=64` 1줄 + `mode=v1 tiles=64 dom=0 fps=NN.N ...` 여러 줄 출력. (fps 값 자체는 판정 아님 — Task 5에서 측정. 여기선 렌더/로그 동작 확인.)

- [ ] **Step 3: V2 스모크 — 타일별 텍스처 매 프레임 업로드 확인**

Run:
```powershell
$url = "file:///" + ($page -replace '\\','/') + "?grid=8&mode=v2&dom=0&log=1"
$err = "$root\target\multigpu_logs\smoke_v2.stderr.log"
$p = Start-Process $exe -ArgumentList @("--window-size","1920x1080",$url) -WorkingDirectory $root -RedirectStandardError $err -RedirectStandardOutput "$root\target\multigpu_logs\smoke_v2.stdout.log" -PassThru
Start-Sleep -Seconds 8; if(!$p.HasExited){ Stop-Process -Id $p.Id -Force }
Select-String -Path $err -Pattern "\[CEILING\] mode=v2 tiles=64" | Select-Object -First 3
```
Expected: `mode=v2 tiles=64 dom=0 fps=..` 출력, `ERROR` 라인 없음.

- [ ] **Step 4: DOM 스모크 — 오버레이 혼합 확인**

Run:
```powershell
$url = "file:///" + ($page -replace '\\','/') + "?grid=8&mode=v2&dom=1&log=1"
$err = "$root\target\multigpu_logs\smoke_dom.stderr.log"
$p = Start-Process $exe -ArgumentList @("--window-size","1920x1080",$url) -WorkingDirectory $root -RedirectStandardError $err -RedirectStandardOutput "$root\target\multigpu_logs\smoke_dom.stdout.log" -PassThru
Start-Sleep -Seconds 8; if(!$p.HasExited){ Stop-Process -Id $p.Id -Force }
Select-String -Path $err -Pattern "\[CEILING\] mode=v2 tiles=64 dom=1" | Select-Object -First 3
```
Expected: `dom=1` 라인 출력, `ERROR` 없음. (창을 잠깐 육안 확인하면 캔버스 위에 청록 테두리+`TILE n` 라벨이 보임.)

- [ ] **Step 5: 커밋**

```powershell
git add tests/html/video_collapse_ceiling.html
git commit -m "천장 검증용 WebGL 하네스 추가 (V1/V2/DOM)"
```

---

### Task 2: Baseline B — `video_grid_6x6_perf.html`에 `?dom=1` 오버레이 추가

**Files:**
- Modify: `tests/html/video_grid_6x6_perf.html`

**Interfaces:**
- Consumes: 기존 `?grid`/`?cols`/`?rows`/`?log=1` 계약 유지.
- Produces: 새 `?dom=0|1`. `dom=1`이면 각 `<video>` 타일 위에 테두리+라벨 오버레이를 얹음(기본 동작은 `dom` 없거나 `0`이면 변화 없음).

- [ ] **Step 1: DOM 오버레이 스타일 추가**

`tests/html/video_grid_6x6_perf.html`의 `<style>` 안, `#stats { ... }` 규칙 **바로 앞**에 삽입:

```css
  /* ?dom=1 혼합 케이스: 각 비디오 타일 위 테두리+라벨 오버레이 */
  #dom { position: fixed; inset: 0; pointer-events: none; z-index: 5; }
  #dom .cap { position: absolute; box-sizing: border-box; border: 2px solid rgba(0,220,255,.9);
    color: #eafcff; font-size: 14px; padding: 2px 4px; text-shadow: 0 1px 2px #000; overflow: hidden; }
```

- [ ] **Step 2: DOM 오버레이 컨테이너 추가**

`<body>`의 `<div id="grid"></div>` **바로 다음 줄**에 삽입:

```html
<div id="dom"></div>
```

- [ ] **Step 3: 오버레이 생성 로직 추가**

`<script>` 안에서 `const STAGGER = params.get("stagger") === "1";` **다음 줄**에 삽입:

```javascript
  // ?dom=1: 각 타일 위에 DOM 테두리+라벨 오버레이를 얹어 "비디오+DOM 혼합" 케이스 측정
  const DOM = params.get("dom") === "1";
  const domLayer = document.getElementById("dom");
  if (DOM) {
    for (let i = 0; i < TILE_COUNT; i++) {
      const c = document.createElement("div");
      c.className = "cap";
      const col = i % COLS, row = (i / COLS) | 0;
      c.style.left = (col * 100 / COLS) + "vw"; c.style.top = (row * 100 / ROWS) + "vh";
      c.style.width = (100 / COLS) + "vw"; c.style.height = (100 / ROWS) + "vh";
      c.textContent = "TILE " + i;
      domLayer.appendChild(c);
    }
  }
```

- [ ] **Step 4: 오버레이를 매 프레임 살짝 갱신(실제 repaint 유발)**

`tick(now)` 함수 안, `rafSinceSample++;` **다음 줄**에 삽입:

```javascript
    if (DOM) {
      const caps = domLayer.children;
      for (let i = 0; i < caps.length; i++)
        caps[i].style.borderColor = ((rafSinceSample + i) & 16) ? "rgba(0,220,255,.9)" : "rgba(255,140,0,.9)";
    }
```

- [ ] **Step 5: 스모크 — dom 미지정 시 기존 동작 유지 + dom=1 오버레이 동작**

Run:
```powershell
$root = "D:\2_TechReview\20260606_multigpu_browser\servo"
$exe  = "$root\target\release\servoshell.exe"
$page = "$root\tests\html\video_grid_6x6_perf.html"
$env:SERVO_GSTREAMER_AVDEC_MAX_THREADS = "1"
$env:SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF = "1"
$url = "file:///" + ($page -replace '\\','/') + "?cols=6&rows=5&dom=1&log=1"
$err = "$root\target\multigpu_logs\smoke_baseline_dom.stderr.log"
$p = Start-Process $exe -ArgumentList @("--window-size","1920x1080",$url) -WorkingDirectory $root -RedirectStandardError $err -RedirectStandardOutput "$root\target\multigpu_logs\smoke_baseline_dom.stdout.log" -PassThru
Start-Sleep -Seconds 8; if(!$p.HasExited){ Stop-Process -Id $p.Id -Force }
Select-String -Path $err -Pattern "\[GRIDPERF\] tiles=30" | Select-Object -First 3
```
Expected: `[GRIDPERF] tiles=30 fps=.. playing=..` 출력. (dom=1이어도 기존 `[GRIDPERF]` 로그 정상.)

- [ ] **Step 6: 커밋**

```powershell
git add tests/html/video_grid_6x6_perf.html
git commit -m "Baseline 그리드에 dom=1 오버레이 혼합 변형 추가"
```

---

### Task 3: 천장 하네스 대화형 런처 — `run_video_collapse_ceiling.ps1`

**Files:**
- Create: `etc/multigpu/run_video_collapse_ceiling.ps1`

**Interfaces:**
- Produces: `-Mode v1|v2 -Dom 0|1 -Grid N | -Cols C -Rows R -WindowSize WxH -DurationSec S` 파라미터로 servoshell 1회 실행. `SERVO_LOG_PRESENT_CADENCE=1` + `RUST_LOG=warn,paint=info` 세팅. Task 5의 대화형(육안/PresentMon) 검증에 사용.

- [ ] **Step 1: 런처 작성**

Create `etc/multigpu/run_video_collapse_ceiling.ps1`:

```powershell
param(
    [ValidateSet("release","debug")] [string] $Profile = "release",
    [ValidateSet("v1","v2")]        [string] $Mode = "v2",
    [ValidateSet("0","1")]          [string] $Dom = "0",
    [int]    $Grid = 8,
    [int]    $Cols = 0,
    [int]    $Rows = 0,
    [string] $WindowSize = "1920x1080",
    [int]    $DurationSec = 0,
    [switch] $Detach,
    [string] $LogPrefix = "ceiling"
)
# 천장 검증 WebGL 하네스(video_collapse_ceiling.html)를 단일 servoshell 창으로 실행.
# 계측: SERVO_LOG_PRESENT_CADENCE=1 + RUST_LOG=warn,paint=info → present cadence +
#       >16ms 프레임의 wr_update/wr_render 분해가 stderr 로 남는다.
$ErrorActionPreference = "Stop"

$servoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$servoExe  = Join-Path $servoRoot "target\$Profile\servoshell.exe"
$pagePath  = Join-Path $servoRoot "tests\html\video_collapse_ceiling.html"
$logDir    = Join-Path $servoRoot "target\multigpu_logs"
$timestamp = Get-Date -Format "yyyyMMdd_HHmmss"

if (!(Test-Path $servoExe)) { throw "servoshell.exe not found: $servoExe" }
if (!(Test-Path $pagePath)) { throw "Page not found: $pagePath" }
New-Item -ItemType Directory -Path $logDir -Force | Out-Null

if ($Cols -gt 0 -or $Rows -gt 0) {
    $c = if ($Cols -gt 0) { $Cols } else { $Grid }
    $r = if ($Rows -gt 0) { $Rows } else { $Grid }
    $gridQ = "cols=$c&rows=$r"; $gridDesc = "${c}x${r} = $($c*$r) tiles"
} else {
    $gridQ = "grid=$Grid"; $gridDesc = "${Grid}x${Grid} = $($Grid*$Grid) tiles"
}
$url = "file:///" + ($pagePath -replace '\\','/') + "?$gridQ&mode=$Mode&dom=$Dom&log=1"

$env:SERVO_LOG_PRESENT_CADENCE = "1"
$env:RUST_LOG = "warn,paint=info"

$arguments = @("--window-size", $WindowSize, $url)
$stdoutLog = Join-Path $logDir "${LogPrefix}_${Mode}_dom${Dom}_${Profile}_stdout_${timestamp}.log"
$stderrLog = Join-Path $logDir "${LogPrefix}_${Mode}_dom${Dom}_${Profile}_stderr_${timestamp}.log"

Write-Host "Launching collapse-ceiling harness:"
Write-Host "  mode=$Mode dom=$Dom grid=$gridDesc window=$WindowSize"
Write-Host "  url=$url"
Write-Host "  stderr=$stderrLog"

if ($Detach) {
    $p = Start-Process -FilePath $servoExe -ArgumentList $arguments -WorkingDirectory $servoRoot `
        -RedirectStandardOutput $stdoutLog -RedirectStandardError $stderrLog -PassThru
    Write-Host "running detached. pid=$($p.Id)  stderr=$stderrLog"
} elseif ($DurationSec -gt 0) {
    $p = Start-Process -FilePath $servoExe -ArgumentList $arguments -WorkingDirectory $servoRoot `
        -RedirectStandardOutput $stdoutLog -RedirectStandardError $stderrLog -PassThru
    Start-Sleep -Seconds $DurationSec
    if (!$p.HasExited) { $p.CloseMainWindow() | Out-Null; Start-Sleep -Seconds 2 }
    if (!$p.HasExited) { Stop-Process -Id $p.Id -Force }
    Write-Host "smoke finished after $DurationSec s. stderr=$stderrLog"
} else {
    Push-Location $servoRoot
    try { & $servoExe @arguments 1> $stdoutLog 2> $stderrLog } finally { Pop-Location }
}
```

- [ ] **Step 2: 스모크 — 런처가 `[CEILING]` + `Present cadence` 둘 다 남기는지 확인**

Run:
```powershell
& "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\run_video_collapse_ceiling.ps1" -Mode v2 -Dom 1 -Grid 8 -DurationSec 10
$log = Get-ChildItem "D:\2_TechReview\20260606_multigpu_browser\servo\target\multigpu_logs\ceiling_v2_dom1_release_stderr_*.log" | Sort-Object LastWriteTime | Select-Object -Last 1
Select-String -Path $log.FullName -Pattern "\[CEILING\] mode=v2","Present cadence: painter" | Group-Object { $_.Pattern } | Select-Object Name,Count
```
Expected: `[CEILING] mode=v2` 다수 + `Present cadence: painter` 다수(각 ≥5). 둘 다 존재하면 계측·URL 계약 정상.

- [ ] **Step 3: 커밋**

```powershell
git add etc/multigpu/run_video_collapse_ceiling.ps1
git commit -m "천장 하네스 대화형 런처 추가"
```

---

### Task 4: 스윕 러너 + 로그 파서

**Files:**
- Create: `etc/multigpu/run_video_ceiling_sweep.ps1`
- Create: `etc/multigpu/tools/parse_ceiling_logs.ps1`

**Interfaces:**
- Consumes: Task 1/2의 로그 계약(`[CEILING] ... fps=<f> maxGapMs=<g>`, `[GRIDPERF] ... fps=<f> maxGapMs=<g>`), 계측(`Present cadence: painter <id> presents/s=<p> max_gap_ms=<m> pending=<n>`, `Slow paint frame: ... wr_update_ms=<u> wr_render_ms=<r>`).
- Produces: 스윕은 `$logDir/sweep_<ts>/` 아래 config별 stderr 로그 + `manifest.csv`(열: `label,mode,dom,n,cols,rows,stderr`). 파서는 manifest를 읽어 config별 **정상상태 중앙값**(rAF fps, present/s, max_gap, wr_render p50/p95, wr_update p50)을 markdown 표로 stdout 출력.

- [ ] **Step 1: 스윕 러너 작성**

Create `etc/multigpu/run_video_ceiling_sweep.ps1`:

```powershell
param(
    [ValidateSet("release","debug")] [string] $Profile = "release",
    [string] $WindowSize = "1920x1080",
    [int]    $WarmupSec = 12,
    [int]    $SteadySec = 30,
    # 부분 실행용 필터(비우면 전체). 예: -Series "v1,v2"  -Ns "45,64"
    [string] $Series = "baseline,v1,v2",
    [string] $Doms = "0,1",
    [string] $Ns = "30,40,45,64"
)
# B/V1/V2 × dom{0,1} × N{30,40,45,64} 스윕. 각 config를 detached 로 (Warmup+Steady)초 돌리고
# stderr 를 개별 로그로 캡처, manifest.csv 에 기록. present cadence 는 detached 에서도 stderr 로
# 남으므로 자동 수집 가능(단 PresentMon 물리 present 교차확인은 Task5에서 foreground 로 별도).
$ErrorActionPreference = "Stop"

$servoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$servoExe  = Join-Path $servoRoot "target\$Profile\servoshell.exe"
$ceilPage  = Join-Path $servoRoot "tests\html\video_collapse_ceiling.html"
$gridPage  = Join-Path $servoRoot "tests\html\video_grid_6x6_perf.html"
if (!(Test-Path $servoExe)) { throw "servoshell.exe not found: $servoExe" }

$timestamp = Get-Date -Format "yyyyMMdd_HHmmss"
$sweepDir  = Join-Path $servoRoot "target\multigpu_logs\sweep_$timestamp"
New-Item -ItemType Directory -Path $sweepDir -Force | Out-Null

# N → cols x rows 매핑
$gridMap = @{ 30 = @(6,5); 40 = @(8,5); 45 = @(9,5); 64 = @(8,8) }

$seriesSel = $Series.Split(",") | ForEach-Object { $_.Trim() }
$domSel    = $Doms.Split(",")   | ForEach-Object { $_.Trim() }
$nSel      = $Ns.Split(",")     | ForEach-Object { [int]$_.Trim() }

$env:SERVO_LOG_PRESENT_CADENCE = "1"
$env:RUST_LOG = "warn,paint=info"

$rows = @()
foreach ($n in $nSel) {
    if (-not $gridMap.ContainsKey($n)) { Write-Warning "no grid mapping for N=$n, skip"; continue }
    $cols = $gridMap[$n][0]; $rowsN = $gridMap[$n][1]
    foreach ($series in $seriesSel) {
        foreach ($dom in $domSel) {
            if ($series -eq "baseline") {
                $page = $gridPage
                $url  = "file:///" + ($page -replace '\\','/') + "?cols=$cols&rows=$rowsN&dom=$dom&log=1"
                # baseline 만 실제 비디오 → 디코드 정책 env 필요
                $env:SERVO_GSTREAMER_AVDEC_MAX_THREADS = "1"
                $env:SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF = "1"
                $mode = "-"
            } else {
                $page = $ceilPage
                $url  = "file:///" + ($page -replace '\\','/') + "?cols=$cols&rows=$rowsN&mode=$series&dom=$dom&log=1"
                Remove-Item Env:\SERVO_GSTREAMER_AVDEC_MAX_THREADS -ErrorAction SilentlyContinue
                Remove-Item Env:\SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF -ErrorAction SilentlyContinue
                $mode = $series
            }
            $stderr = Join-Path $sweepDir "${series}_dom${dom}_n${n}.stderr.log"
            $stdout = Join-Path $sweepDir "${series}_dom${dom}_n${n}.stdout.log"
            Write-Host "[sweep] series=$series dom=$dom n=$n ($cols x $rowsN)  -> $stderr"
            $p = Start-Process -FilePath $servoExe -ArgumentList @("--window-size",$WindowSize,$url) `
                 -WorkingDirectory $servoRoot -RedirectStandardOutput $stdout -RedirectStandardError $stderr -PassThru
            Start-Sleep -Seconds ($WarmupSec + $SteadySec)
            if (!$p.HasExited) { Stop-Process -Id $p.Id -Force }
            Start-Sleep -Seconds 1
            $rows += [pscustomobject]@{ label=$series; mode=$mode; dom=$dom; n=$n; cols=$cols; rows=$rowsN; stderr=$stderr }
        }
    }
}
$manifest = Join-Path $sweepDir "manifest.csv"
$rows | Export-Csv -NoTypeInformation -Encoding utf8 $manifest
Write-Host "sweep done. manifest=$manifest"
Write-Host "parse with: etc\multigpu\tools\parse_ceiling_logs.ps1 -Manifest `"$manifest`""
```

- [ ] **Step 2: 파서 작성**

Create `etc/multigpu/tools/parse_ceiling_logs.ps1`:

```powershell
param(
    [Parameter(Mandatory=$true)] [string] $Manifest,
    [int] $DropFirst = 10   # 워밍업으로 앞 N개 초당 샘플 버림
)
# 스윕 manifest 를 읽어 config별 정상상태 중앙값 표(markdown)를 stdout 으로 출력.
$ErrorActionPreference = "Stop"
function Median($xs) {
    $s = @($xs | Where-Object { $_ -ne $null } | Sort-Object)
    if ($s.Count -eq 0) { return $null }
    return $s[[int][math]::Floor($s.Count/2)]
}
function Pct($xs, $p) {
    $s = @($xs | Where-Object { $_ -ne $null } | Sort-Object)
    if ($s.Count -eq 0) { return $null }
    $idx = [int][math]::Floor(($s.Count-1) * $p); return $s[$idx]
}

$configs = Import-Csv $Manifest
$fmt = { param($v,$d=1) if ($null -eq $v) { "-" } else { [math]::Round($v,$d) } }

"| series | dom | N | rAF fps | present/s | max_gap ms | wr_render p50 | wr_render p95 | wr_update p50 |"
"|---|---|---|---|---|---|---|---|---|"
foreach ($c in $configs) {
    if (!(Test-Path $c.stderr)) { "| $($c.label) | $($c.dom) | $($c.n) | (no log) | | | | | |"; continue }
    $lines = Get-Content $c.stderr

    # 페이지 rAF fps: [CEILING] 또는 [GRIDPERF] ... fps=NN.N maxGapMs=NN.N
    $fps = @(); $gap = @()
    foreach ($ln in ($lines | Select-String -Pattern "\[(CEILING|GRIDPERF)\].* fps=([0-9.]+) maxGapMs=([0-9.]+)")) {
        $m = [regex]::Match($ln.Line, "fps=([0-9.]+) maxGapMs=([0-9.]+)")
        if ($m.Success) { $fps += [double]$m.Groups[1].Value; $gap += [double]$m.Groups[2].Value }
    }
    # present cadence: Present cadence: painter .. presents/s=NN.N max_gap_ms=NN.NN pending=N
    $pps = @()
    foreach ($ln in ($lines | Select-String -Pattern "Present cadence: painter .* presents/s=([0-9.]+)")) {
        $m = [regex]::Match($ln.Line, "presents/s=([0-9.]+)")
        if ($m.Success) { $pps += [double]$m.Groups[1].Value }
    }
    # slow-frame 분해: Slow paint frame: .. wr_update_ms=NN.NN wr_render_ms=NN.NN
    $wu = @(); $wr = @()
    foreach ($ln in ($lines | Select-String -Pattern "wr_update_ms=([0-9.]+) wr_render_ms=([0-9.]+)")) {
        $m = [regex]::Match($ln.Line, "wr_update_ms=([0-9.]+) wr_render_ms=([0-9.]+)")
        if ($m.Success) { $wu += [double]$m.Groups[1].Value; $wr += [double]$m.Groups[2].Value }
    }
    # 워밍업 제거(초당 샘플 계열)
    if ($fps.Count -gt $DropFirst) { $fps = $fps[$DropFirst..($fps.Count-1)] }
    if ($gap.Count -gt $DropFirst) { $gap = $gap[$DropFirst..($gap.Count-1)] }
    if ($pps.Count -gt $DropFirst) { $pps = $pps[$DropFirst..($pps.Count-1)] }

    $row = "| {0} | {1} | {2} | {3} | {4} | {5} | {6} | {7} | {8} |" -f `
        $c.label, $c.dom, $c.n,
        (& $fmt (Median $fps)), (& $fmt (Median $pps)), (& $fmt (Median $gap)),
        (& $fmt (Median $wr)), (& $fmt (Pct $wr 0.95)), (& $fmt (Median $wu))
    $row
}
```

- [ ] **Step 3: 엔드투엔드 스모크 — 축소 스윕 1개 config → 파서 표 1행**

Run:
```powershell
$sweep = "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\run_video_ceiling_sweep.ps1"
& $sweep -Series "v2" -Doms "0" -Ns "64" -WarmupSec 4 -SteadySec 6
$man = Get-ChildItem "D:\2_TechReview\20260606_multigpu_browser\servo\target\multigpu_logs\sweep_*\manifest.csv" | Sort-Object LastWriteTime | Select-Object -Last 1
& "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\tools\parse_ceiling_logs.ps1" -Manifest $man.FullName -DropFirst 2
```
Expected: markdown 표 헤더 + `| v2 | 0 | 64 | <fps> | <present/s> | ... |` 1행. fps/present 셀이 `-`가 아니라 숫자면 파싱 정상.

- [ ] **Step 4: 커밋**

```powershell
git add etc/multigpu/run_video_ceiling_sweep.ps1 etc/multigpu/tools/parse_ceiling_logs.ps1
git commit -m "천장 스윕 러너 + 로그 파서 추가"
```

---

### Task 5: 검증 실행 + 결과 문서 + go/no-go + 메모리 갱신

**Files:**
- Create: `etc/multigpu/WEBRENDER_VIDEO_OFFLOAD_STATUS.md`
- Modify: `C:\Users\ilwoonam75\.claude\projects\D--2-TechReview-20260606-multigpu-browser\memory\webrender-video-offload.md`
- Modify: `C:\Users\ilwoonam75\.claude\projects\D--2-TechReview-20260606-multigpu-browser\memory\MEMORY.md`

**Interfaces:**
- Consumes: Task 4 스윕/파서 산출물.
- Produces: 채워진 결과 표 + go/no-go 판정 문장 + 다음 단계. 메모리 `webrender-video-offload`에 검증 결과 반영.

- [ ] **Step 1: 전체 스윕 실행 (약 18분: 24 config × ~42s)**

Run (창을 다른 앱으로 가리지 말 것 — servoshell 이 occluded 되면 present 가 왜곡됨):
```powershell
& "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\run_video_ceiling_sweep.ps1"
```
Expected: `sweep done. manifest=...sweep_<ts>\manifest.csv`. 24개 stderr 로그 생성.

- [ ] **Step 2: 파싱해 결과 표 생성**

Run:
```powershell
$man = Get-ChildItem "D:\2_TechReview\20260606_multigpu_browser\servo\target\multigpu_logs\sweep_*\manifest.csv" | Sort-Object LastWriteTime | Select-Object -Last 1
& "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\tools\parse_ceiling_logs.ps1" -Manifest $man.FullName | Tee-Object -Variable table
$table
```
Expected: 24행 markdown 표. 이 표를 Step 3 문서에 붙여넣는다.

- [ ] **Step 3: PresentMon 물리 present 교차확인(핵심 config만)**

`64@60fps` 판정에 걸리는 대표 config(baseline dom1 n64, v1 dom1 n64, v2 dom1 n64)를 런처로 **foreground** 실행하고 PresentMon 으로 물리 present cadence 확인(원격이 아닌 물리 모니터에서):
```powershell
# 예: v2 dom1 64타일을 30초 foreground 로 띄우고 그 사이 다른 창에서 PresentMon 캡처
& "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\run_video_collapse_ceiling.ps1" -Mode v2 -Dom 1 -Cols 8 -Rows 8 -DurationSec 30
# 별도 관리자 콘솔: D:\PresentMon-2.3.1-x64.exe -process_name servoshell.exe -output_file presentmon_v2_n64.csv (30초)
```
Expected: PresentMon 평균 present fps 가 파서의 present/s 와 대체로 일치(±수 fps). 큰 괴리면 rAF-side 측정이 아닌 물리 present 를 정본으로 채택하고 문서에 명기.

- [ ] **Step 4: 결과 문서 작성 — `etc/multigpu/WEBRENDER_VIDEO_OFFLOAD_STATUS.md`**

Create with the parsed table pasted into `## 결과` and the verdict filled from the go/no-go rules:

```markdown
# WebRender 비디오 offload — 천장 검증 결과

- 날짜: 2026-07-02
- 스펙: `docs/superpowers/specs/2026-07-02-webrender-video-offload-ceiling-design.md`
- 계획: `docs/superpowers/plans/2026-07-02-webrender-video-offload-ceiling.md`
- 머신: 합성 A5000 / 디스플레이 Intel iGPU @60Hz, servoshell release, headed

## 무엇을 쟀나
- Baseline B: `video_grid_6x6_perf.html` = N개 진짜 `<video>` (N WebRender 레이어)
- V1: `video_collapse_ceiling.html` 공유 텍스처 1개 N quad (업로드 0 = 낙관 상한)
- V2: 타일별 텍스처 N개 매 프레임 texImage2D (업로드 포함 = 보수 상한)
- 각 × dom{0,1} × N{30,40,45,64}. 계측: SERVO_LOG_PRESENT_CADENCE + RUST_LOG=warn,paint=info.

## 결과
<!-- Step 2 파서 표 붙여넣기 -->

<!-- 있으면 Step 3 PresentMon 교차확인 요약 -->

## 판정 (go/no-go)
<!-- 아래 규칙으로 한 문장 결론:
  - V2가 64@60fps(present/s ≥ ~58) 유지            → C2 착수 강하게 정당화
  - V1만 되고 V2 안 됨                              → 업로드가 관건, C2의 zero-copy 평면 재사용이 결정적 → 착수
  - V1도 45@60fps 못 감                             → 접기론 실패 → 접근 A(네이티브 오버레이) 재검토
  - baseline 이 이미 목표 근처                       → 병목 재평가
-->

## 관찰 / 주의
- V1 vs V2 간극 = 순수 업로드 비용(실제 C2는 YUV borrowed 재사용이라 이보다 가벼움 → V2는 보수 상한).
- dom0 vs dom1 간극 = DOM 오버레이 합성 비용(작아야 정상; 메모리상 DOM 은 잘 확장).
- baseline 의 wr_render 가 V1/V2 대비 큰지 확인 → "레이어당 O(N)" 병목의 직접 증거.

## 다음 단계
<!-- 판정에 따라: C2 설계 세션 / 접근 A 재검토 / 병목 재조사 중 하나를 명시 -->
```

- [ ] **Step 5: 메모리 갱신**

`memory/webrender-video-offload.md` 본문 끝에 검증 결과 요약(핵심 숫자 + 판정 + 결과 문서 경로)을 추가하고, `MEMORY.md`의 해당 줄 hook 을 "천장 검증 완료: <판정 한 줄>"로 갱신. (실제 숫자·판정으로 채움 — 플레이스홀더 금지.)

- [ ] **Step 6: 커밋**

```powershell
git add etc/multigpu/WEBRENDER_VIDEO_OFFLOAD_STATUS.md
git commit -m "WebRender 비디오 offload 천장 검증 결과 및 go/no-go 판정"
```
(메모리 파일은 `.claude` 밖 사용자 홈이라 이 저장소 커밋에 포함되지 않음 — Write 로 저장만.)

---

## Self-Review

**Spec coverage:**
- §5.1 합성 WebGL 하네스 V1/V2/dom → Task 1. ✓
- §5.2 Baseline B dom 변형 → Task 2. ✓
- §5.3 측정 프로토콜(env, N 스윕, present cadence, PresentMon 교차확인) → Task 3(런처)+Task 4(스윕/파서)+Task 5(실행/PresentMon). ✓
- §5.4 결과 문서 + 메모리 갱신 → Task 5. ✓
- §6 판정 기준 → Task 5 Step 4 문서의 go/no-go 규칙에 그대로 반영. ✓
- §7 리스크(V2 과대평가/디코드 천장) → 결과 문서 "관찰/주의"에서 baseline wr_render 대조 + V1/V2 간극 해석으로 다룸. ✓
- §8 비목표(엔진 구현 없음) → Global Constraints "엔진 변경 0" + 전 Task 가 HTML/PS/MD 만. ✓

**Placeholder scan:** 결과 문서(Task 5 Step 4)의 `<!-- -->`는 실행 시 실측 숫자로 채우는 지점이며, 채우는 규칙(go/no-go 값 포함)을 명시함 — 계획 자체의 미완이 아님. 그 외 TBD/TODO 없음. ✓

**Type/계약 일관성:** 로그 라인 계약이 전 Task 일치 — 페이지 `[CEILING]/[GRIDPERF] ... fps=.. maxGapMs=..`(Task1/2 생성 ↔ Task4 파서 정규식), 계측 `Present cadence: painter .. presents/s=.. max_gap_ms=.. pending=..` 및 `wr_update_ms=.. wr_render_ms=..`(painter.rs 실제 포맷 ↔ Task4 파서). manifest.csv 열(`label,mode,dom,n,cols,rows,stderr`)이 스윕 생성 ↔ 파서 소비 일치. 그리드 매핑(30→6×5 등)이 스윕과 스펙 동일. ✓
