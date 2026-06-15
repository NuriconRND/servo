# 멀티 GPU 월 WebGL/WebGPU 상태 및 인계 (2026-06-14)

three.js 예제를 멀티 GPU "월"(`--wall-layout <json> --wall-all-tiles`)에 표출하는 작업의 진행/잔여 정리.
대상: WebGPU 리타게팅(완료), WebGL keyframes(LittlestTokyo), 4K 비디오.

## 빌드 메모 (중요)
- `cmd /c ".\mach.bat build --release > log 2>&1"`, PATH에 `C:\Program Files\LLVM\bin`(lld-link) 필요.
- **CARGO_HOME 미설정 시 빌드는 전역 `~/.cargo`의 mozangle을 컴파일** (`.servo/cargo-home`가 아님).
  `etc/multigpu/servo_env.ps1`을 source하면 `CARGO_HOME=.servo\cargo-home`로 바뀜.
  → ANGLE C++ 편집은 **실제로 컴파일되는 트리**에 해야 함. 확인: `target/release/build/mozangle-*/build_script_build-*.d`.
- cc는 ANGLE `.cpp` 변경에 rerun-if-changed를 안 냄 → ANGLE 소스 편집 후엔 강제 리빌드:
  `rm -rf target/release/build/mozangle-* .fingerprint/mozangle-* deps/*mozangle*` 후 `touch` mozangle/build.rs.
  DLL(`libGLESv2.dll`/`libEGL.dll`)은 `target/release/build/mozangle-*/out/`에 생기며 exe 옆으로 수동 복사 필요.
- 한글(cp949) 콘솔에서 `mach build`는 cargo "Finished" 이후 요약의 '•' 문자로 무해한 UnicodeEncodeError를 던짐(빌드는 정상).

## ✅ 완료 (커밋됨)
- WebGPU 리타게팅 월 표출 (커밋 7c818615a, 이전 세션).
- WebGL2 텍스처/포맷 지원 (커밋 ef7b9c6aa): EXT_color_buffer_float, float 포맷,
  texSubImage2D 전송 포맷(RG/RED) 허용(`for_sub_image`), texStorage2D 크기/POT 검증 버그 수정(`for_tex_storage`).
- 멀티 GPU 월 격리 D3D11 디바이스 + cross-GPU surface 공유 + 컴포지터 안정화 (커밋 35b4c2ed7).
- 테스트 페이지/레이아웃 (커밋 b33a009f9).

검증: 듀얼/세임 GPU 월 모두 크래시 없이 안정, WebGL API 에러 0.

## 🔶 비커밋 — mozangle ANGLE 수정 (GPU 연산 분산의 핵심)
타일별 연산을 **그 모니터의 물리 GPU**에서 수행하려면 ANGLE display 캐시 키에 D3D adapter LUID가 필요.
없으면 GPU별 LUID 요청이 동일 키로 충돌→첫(GPU0) display 재사용→전부 GPU0 폴백.
(동일 모델 GPU 2개는 deviceId로도 구분 불가.)

**적용 위치**: `gfx/angle/checkout/src/libANGLE/Display.cpp` (빌드가 쓰는 mozangle 트리 = 보통 `~/.cargo/registry/src/.../mozangle-0.5.5`).
레지스트리 캐시라 git 커밋 불가 → **커밋하려면 mozangle 포크 + Cargo.toml `[patch.crates-io]`** 필요.

변경 4곳:
1. `struct ANGLEPlatformDisplay`에 `EGLAttrib luidHigh{0}; EGLAttrib luidLow{0};` 필드 추가.
2. 7-인자 생성자(luidHigh, luidLow 추가) + `tie()`에 `luidHigh, luidLow` 포함.
3. `GetDisplayFromNativeDisplay()`: `EGL_PLATFORM_ANGLE_D3D_LUID_HIGH_ANGLE`/`..._LOW_ANGLE`를 읽어
   `displayKey` 생성에 전달.
4. `Display::~Display()`의 캐시 erase 키 생성에도 동일 LUID 2개 추가(삽입/삭제 키 일치 필수, 안 그러면 dangling).

**패치/적용 자산** (레포에 커밋됨, `etc/multigpu/patches/`):
- `mozangle-0.5.5-angle-luid-display-cache.patch` — 위 4곳의 정확한 unified diff(crate 원본 대비, `git apply -p1` 검증됨).
- `apply_mozangle_angle_luid.ps1` — 빌드가 쓰는 mozangle 트리에 멱등 적용(+`-Rebuild` 시 ANGLE 강제 리빌드·DLL 복사).
- `README.md` — 비커밋 이유 / 임시 적용 / 포크+`[patch.crates-io]`로 커밋 가능하게 만드는 법.

검증 결과: nvidia-smi에서 servoshell이 **양 GPU에 등록**, 메모리/사용률 균형
(WebGL +290/+248MiB, WebGPU +606/+166, 비디오 +182/+178). compute-apps에 GPU0·GPU1 모두.

## ✅ 해결 — WebGL keyframes 검정 (2026-06-14) + 성능/화면 이슈 규명

### 근본 원인 = WebGL2 정점-속성 검증 버그 (검정의 진짜 원인)
InvalidOperation은 `draw_elements_instanced`가 아니라 **그 전에** 실행되는
`WebGL2RenderingContext::DrawElements` → `validate_vertex_attribs_for_draw`
(components/script/dom/webgl/webgl2renderingcontext.rs)에서 발생.
이 함수가 프로그램 속성의 base type을 정점 배열의 **스토리지** `type_`와 비교하는데 두 가지 버그:
1. enabled array일 때 스토리지 enum을 그대로 base type으로 사용. 하지만 `vertexAttribPointer`는
   스토리지가 normalized/정수여도 셰이더엔 **항상 FLOAT**로 공급(UNSIGNED_SHORT/BYTE 등). 스토리지
   enum은 FLOAT/INT/UINT 그룹에 없어 → 오탐 INVALID_OPERATION → LittlestTokyo의 일부 mesh
   (USHORT로 저장된 `vec4`) 약 4/71개가 조용히 드롭 → "검정".
2. base-type 그룹표에 행렬 타입(FLOAT_MAT2/3/4 …)이 누락 → InstancedMesh의 `mat4` 속성
   (PMREM의 RoomEnvironment 등, 4개 float vec4로 공급)도 거부.

**수정**: 스토리지-타입-as-base-type 로직을 `candidate_base_types`로 교체
(비활성→제너릭 속성 타입; 활성+normalized/float/half→FLOAT; 활성+비정규화 정수→[FLOAT, INT/UINT]
permissive — Servo는 vertexAttribPointer/IPointer를 구분 저장 안 함, IPointer가 Pointer로 포워딩됨)
+ `glsl_attrib_scalar_base_type()` 헬퍼(FLOAT_VEC*/FLOAT_MAT*→FLOAT, INT_*→INT, UINT_*→UINT).
검증: InvalidOperation 120→0, 단일창·듀얼GPU월 모두 모델 **밝게 정상 렌더**(maxRGB~247;
centerPixel [21,16,13]은 모델 어두운 틈에 걸린 것일 뿐 — "어둡다"는 오판이었고 실제론 mesh가
드롭돼 안 그려졌던 것). 단순조명/PMREM 양쪽 OK. **미커밋(사용자 hold).**

### "월이 매우 느림" = `--features webgl_backtrace` 빌드 부작용 (월/cross-GPU 버그 아님)
`send_with_fallibility`(모든 WebGL 명령, webglrenderingcontext.rs:~433)가 명령마다
`capture_webgl_backtrace()` 호출 → feature ON이면 명령마다 `Backtrace::new()`+`format!`
(전체 Rust 심볼화)+JS 스택 캡처. LittlestTokyo는 프레임당 수천 명령 → ~2.6s/frame(≈0.4fps).
**해상도/타일수/GPU수와 무관**하게 동일(평범한 1920창, 1x1, same-GPU 2타일, dual-GPU 모두 2.6s)
→ 월이 원인이 아님이 입증됨. **feature 없는 `mach build --release`는 빈 구조체 반환 →
3840x1080 듀얼GPU월에서 60fps(vsync)**. 첫 프레임 ~1.3s는 일회성 ANGLE 셰이더 컴파일.
**교훈: 성능 작업/데모는 webgl_backtrace 없이 빌드. webgl_backtrace는 InvalidOperation 디버깅 전용.**
(프로브의 프레임별 `gl.finish()`도 파이프라인을 직렬화해 측정치를 부풀림 — 빼고 측정.)

### "왼쪽 모니터 좌상단만 보임" = 테스트 페이지 버그 (Servo 아님)
`--wall-all-tiles`는 가상 뷰포트(3840x1080) 전체를 보는 **공유 webview 1개**. 진단용 프로브가
`renderer.setSize(960,600)` **고정** → 좌상단만 채움. `window.innerWidth/innerHeight`(+resize)로
크기를 잡으면 월 전체를 채움(검증: window.innerWidth=3840). 데모 페이지: `tests/html/keyframes_wall.html`.

### 참고: WebGL2 활성화 pref
WebGL2는 기본 OFF — `--pref dom_webgl2_enabled=true` 없거나 host가 www.servoexperiments.com이
아니면 `getContext('webgl2')`가 null → three.js(WebGL2 전용)가 "Error creating WebGL context".
127.0.0.1/threejs.org는 pref 필요. 단일창 실행/캡처 헬퍼: `etc/multigpu/tools/run_single_capture.ps1`.

### 남은(미수정) latent 버그
`gl.getVertexAttrib/getBufferParameter/getProgramParameter`를 특정 상태에서 호출 시 Servo 패닉
(`assertion failed: self.is_double()`, mozjs jsval.rs:503). `keyframes_drawprobe.html`로 재현.

## GPU 분산 관점 요약 (사용자 질의)
- WebGL/WebGPU/비디오 모두 fan-out 표출은 정상.
- mozangle ANGLE LUID 수정 적용 시 **각 타일이 자기 모니터 GPU에서 연산**(검증됨). 미적용 시 전부 GPU0.
- 비디오 디코딩은 CPU(소프트웨어), NVDEC 미사용 — 현재 의도된 동작.
