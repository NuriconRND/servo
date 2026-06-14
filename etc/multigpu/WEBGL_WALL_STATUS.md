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

## ⏳ 미해결 — WebGL keyframes 모델이 검정/어둡게 렌더 (다음 세션 이어서)
WebGL **API 에러는 0**이지만 모델이 검정. 분석 진행 상황:
- 기본 텍스처 경로 전부 정상: `tests/html/webgl2_texture_probe.html`, `webgl2_imgtex_probe.html`
  (texImage2D/texStorage2D/sRGB/canvas/HTMLImage/ImageBitmap 모두 샘플 정상). → 업로드는 정상.
- render-to-RGBA16F/RGBA32F 정상: `webgl2_rtt_float_probe.html` (PMREM 렌더타깃 OK). R11F_G11F_B10F만 미지원.
- keyframes는 명시적 조명이 없고 `scene.environment = pmremGenerator.fromScene(sky)`(PMREM IBL) +
  ACESFilmicToneMapping으로만 조명. 처음엔 PMREM 의심했으나—
- **결정적**: PMREM/Sky를 제거하고 단순 조명만 준 `tests/html/keyframes_simplelight.html`에서도
  모델은 **여전히 어둡고**(centerPixel≈[21,16,13]) **InvalidOperation 다발**(약 프레임당 4회).
  배경(단색)은 정상 렌더, 모델 71 meshes 로드 성공. → 원인은 PMREM이 아니라 **모델 draw/머티리얼**.
- webgl_backtrace로 InvalidOperation 추적 결과: three.module.js의 `gl.drawElements`
  (renderBufferDirect←renderObject). 즉 일부 mesh의 **drawElements가 InvalidOperation**.
  Servo `draw_elements_instanced`의 후보 지점(webglrenderingcontext.rs):
  버퍼 용량 검사(offset+count*type_size > capacity), `validate_for_draw`(attrib), `validate_framebuffer`.
  71개 중 ~4개만 실패하므로 특정 geometry/attrib 조건으로 추정.

**다음 단계 제안**:
1. `--features webgl_backtrace` 빌드로 InvalidOperation 백트레이스 재확인(이미 로깅 추가됨).
2. `draw_elements_instanced`의 각 InvalidOperation 분기에 임시 로깅을 넣어 어느 검사가 실패하는지 특정.
   (버퍼 용량/attrib validate_for_draw가 유력 — draco 디코드 후 인덱스/속성 버퍼 추적 문제 가능.)
3. 어둡게 렌더되는 mesh: drawElements는 성공하나 albedo가 어두운지 → 머티리얼/샘플러 바인딩,
   또는 sRGB 디코드/톤매핑 경로 확인. `keyframes_simplelight.html`로 격리 디버깅.
4. (참고) WebGPU 리타게팅은 cross-GPU 표출 정상, 비디오는 의도적으로 CPU 디코딩(NVDEC 미사용).

## GPU 분산 관점 요약 (사용자 질의)
- WebGL/WebGPU/비디오 모두 fan-out 표출은 정상.
- mozangle ANGLE LUID 수정 적용 시 **각 타일이 자기 모니터 GPU에서 연산**(검증됨). 미적용 시 전부 GPU0.
- 비디오 디코딩은 CPU(소프트웨어), NVDEC 미사용 — 현재 의도된 동작.
