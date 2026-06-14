# WebGPU 멀티 GPU 팬아웃 상태 및 인계 (2026-06-14)

WebGPU 콘텐츠(렌더+컴퓨트)를 멀티 GPU "월"의 각 타일 GPU에서 수행하도록 만드는 작업.
WebGL 팬아웃(타일별 격리 D3D11 디바이스 + 명령 재생)과 동등한 것을 WebGPU에 구현하는 것이 목표.

대상 검증 페이지: `https://threejs.org/examples/webgpu_compute_birds.html`(컴퓨트),
`webgpu_animation_retargeting`(렌더). 월 실행: `--wall-layout <json> --wall-all-tiles --pref dom_webgpu_enabled=true`.

## 핵심 배경 — WebGPU가 WebGL과 다른 이유
- **WebGL 팬아웃**: 스크립트가 `webview_painter_targets_blocking`로 타일별 painter를 받아 → WebGL 스레드가
  painter마다 별도 GL 컨텍스트(각 GPU의 격리 D3D11 디바이스)를 만들고 → **모든 WebGL 명령을 각 백엔드에 재생**.
  glow GL 호출은 상태 없는 정수 네임이라 재생이 단순. 타일 컴포지터는 자기 GPU surface(DXGI 공유)를 읽음.
- **WebGPU(기존)**: painter 인식이 전혀 없음. 단일 `wgc::global::Global`, `request_adapter`(전력선호도)→GPU0
  단일 디바이스. present는 텍스처→스테이징버퍼→**CPU 리드백**→WebRender 외부이미지. 즉 콘텐츠 렌더+컴퓨트는
  전부 GPU0, GPU1은 합성만(측정: nvidia-smi 메모리 델타 GPU0 +475~608MiB vs GPU1 +119~182MiB).

## 핵심 설계 결정 (중요)
1. **ID 충돌 제약**: wgpu-core의 `IdentityValues`는 레지스트리별로 내부 할당(`alloc`/`None`)과
   외부 제공 ID(`mark_as_used`/`Some(id)`) 혼용 시 **패닉**("Mix of internally allocated and
   externally provided IDs"). Servo는 항상 스크립트가 외부 ID를 제공 → secondary 디바이스에 `None`을
   넘길 수 없음.
2. **해결책 = GPU당 별도 `Global` 인스턴스.** 각 Global은 독립 레지스트리라 **동일한 스크립트 ID를
   그대로 재사용** 가능 → **ID 변환 테이블 불필요**. 명령 재생이 "각 Global에 같은 메시지를 같은 인자로
   디스패치"로 단순화됨. (스크립트/DOM 코드는 전혀 손대지 않음 — 모든 로직이 wgpu 스레드에 국한.)
3. **어댑터→물리 GPU(LUID) 매칭** (포크 불필요, wgpu-core/hal 공개 API만):
   - secondary Global에서 `enumerate_adapters(Backends::DX12)` (어댑터 레지스트리는 **내부 할당 전용**이라
     외부 ID와 혼용 안 됨 → 패닉 회피)
   - `global.adapter_as_hal::<wgc::api::Dx12>(id).raw_adapter().GetDesc1().AdapterLuid` 로 LUID 읽기
   - LUID는 `(HighPart: i32, LowPart: u32)` 정수쌍 → **동일 모델 GPU 2장도 구분** (WebGL의 ANGLE LUID와 동일 원리)
   - webgpu 크레이트에 새 의존성 추가 불필요(외부 타입의 inherent 메서드/필드 접근만 사용).
4. **N-GPU 일반화**: 2개로 한정하지 않음. 모든 DX12 어댑터를 열거해 primary와 다른 **고유 LUID마다**
   secondary를 만듦 (N개 GPU → N-1개 secondary).
5. **게이트**: pref `dom_webgpu_multigpu_fanout`(기본 false). 꺼져 있으면 secondary 0개 → 기존 동작과 100% 동일.

## ✅ 구현 완료 (이번 세션, 비커밋 — 사용자 커밋 보류 관례)
모두 `components/webgpu/wgpu_thread.rs` (+ pref, + RenderCommand Clone):

- **Phase 1 — 디바이스 팬아웃 골격**:
  - `SecondaryGpu { global, target_luid, adapter_id, poller }` 구조 + `WGPU`에 `multigpu_fanout`/
    `fanout_initialized`/`secondary_gpus` 필드.
  - `build_instance_descriptor()`(primary/secondary 공통), `adapter_luid()`(LUID 읽기),
    `ensure_secondary_gpus()`(첫 RequestDevice 시 GPU 발견+secondary Global/어댑터 생성).
  - `RequestDevice`에서 각 secondary Global에 **같은 device_id/queue_id로 미러 디바이스 생성**.
  - `DropDevice`/`DestroyDevice` 미러.
- **Phase 2 — 명령 재생** (secondary Global에 동일 ID로 디스패치, 에러는 무시 — secondary 출력은
  아직 present 안 하므로 페이지 화면은 절대 손상되지 않음):
  - 리소스 생성: CreateBuffer/Texture/TextureView/Sampler/BindGroup/BindGroupLayout/PipelineLayout/
    Compute·RenderPipeline/ShaderModule/QuerySet/CommandEncoder, Compute·RenderGetBindGroupLayout.
  - 커맨드: CommandEncoderFinish, Copy(B2B/B2T/T2B/T2T), ResolveQuerySet.
  - 컴퓨트 패스: Begin/SetPipeline/SetBindGroup/Dispatch/DispatchIndirect/End
    (`secondary_compute_passes: HashMap<id, Vec<ComputePass>>` + `replay_secondary_compute` 헬퍼).
  - 렌더 패스: Begin/RenderPassCommand/End (`secondary_render_passes` + `replay_secondary_render`,
    `RenderCommand`에 `Clone` 추가해 GPU별 복제).
  - 큐: Submit(+secondary poller wake로 maintain), WriteBuffer, WriteTexture, UnmapBuffer
    (mapped_at_creation 버퍼 데이터 채우고 unmap — 안 하면 secondary 커맨드버퍼가 "버퍼 매핑됨"으로 검증 실패).
  - Drop 전종류 미러 (리소스 누수 방지).

빌드: 전체 릴리스 빌드 성공. (cp949 콘솔의 mach 요약 '•' UnicodeEncodeError는 빌드 완료 후 무해.)

## ✅ 런타임 검증 완료 (2026-06-14, 2×A4000 월 + compute_birds)
`--pref dom_webgpu_multigpu_fanout=true`로 실측: **present 63fps(60fps vsync) 유지 + 두 A4000 모두
컴퓨트(util ~39%/39%) + GPU1 메모리 ~691MiB 안정**. 런타임에서 3개 버그를 잡음:
1. **primary가 Vulkan에 잡힘** (`Backends::PRIMARY`가 Vulkan 우선 → adapter LUID=None → primary 제외 실패)
   → `build_instance_descriptor`가 팬아웃 시 `Backends::DX12` 강제.
2. **WARP 포함 4개로 팬아웃 → 2fps** (DXGI DX12 열거가 2×A4000 + WARP(소프트웨어) + 가상 어댑터 반환)
   → primary LUID 필수화 + `device_type == DiscreteGpu`만 + primary 제외 → secondary 정확히 1개.
3. **GPU1 메모리 누수 ~8.4MB/frame** (캔버스 텍스처는 `GPUTexture.destroy()`→DestroyTexture로 해제되는데
   미미러였음) → DestroyTexture/DestroyBuffer도 secondary에 미러 + poller wake.

미재생 항목(저우선): RenderBundleEncoderFinish(인코더 non-Clone), BufferMapAsync(스크립트 리드백 전용),
ValidateTextureDescriptor(검증용 더미). compute_birds/retargeting은 렌더번들 미사용.

## 🔲 Phase 3 — GPU-direct present (CPU-readback per-tile는 폐기)
**CPU 리드백 구조에선 타일별 present가 무의미** — 두 GPU가 동일 프레임을 렌더하므로 GPU1 리드백을 써도
픽셀이 동일하고 리드백만 1→2회로 늘어 손해. **진짜 "WebGL처럼" = GPU-direct**(CPU 우회): 각 타일
컴포지터가 자기 GPU의 렌더 텍스처를 DXGI 공유 핸들로 직접 샘플(NativeTexture). 실현가능성 100% 확인
(포크 불필요): `device_as_hal::<Dx12>().raw_device()`+`CreateCommittedResource(HEAP_FLAG_SHARED)`+
`CreateSharedHandle`(windows 0.62=wgpu-hal과 동일), `texture_from_raw`+`create_texture_from_hal`(자체 ID는
`wgpu_core::id::Id::zip`로 예약범위), 공유 펜스(`D3D12_FENCE_FLAG_SHARED`), 컴포지터는 `OpenSharedResource1`
→D3D11→`EGL_D3D_TEXTURE_ANGLE`→GL(WebGL cross-GPU surface 수정과 동일 원리). 마일스톤:
- **M1** primary global: 공유 present 텍스처 생성 + 매프레임 캔버스→복사 + 핸들 export + 펜스, 외부이미지맵에 플러밍.
- **M2** primary painter `WebGpuExternalImages`(painter별)가 핸들 임포트→NativeTexture, 펜스 대기 (tile0 GPU-direct, CPU 리드백 0 검증). **최대 미지수=ANGLE 임포트+교차 디바이스 동기화**.
- **M3** secondary globals로 확장 + painter LUID 연결(`paint_api::rendering_context::dxgi_luid_for_gpu_index` 이미 추가됨) → tile1 GPU-direct.

## 검증 방법
1. 빌드 후 `dom_webgpu_multigpu_fanout=true`로 월 실행:
   `servoshell.exe --wall-layout etc/multigpu/config/example_2x1_dualgpu.json --wall-all-tiles \
    --pref dom_webgpu_enabled=true --pref dom_webgpu_multigpu_fanout=true <url>`
2. 로그(RUST_LOG=info)에서 `WebGPU multi-GPU fan-out: primary adapter LUID = ...`,
   `initialized secondary GPU (LUID ...)`, `N secondary GPU(s) ready`,
   `mirrored device ... onto secondary GPU` 확인.
3. `nvidia-smi -l 1`로 servoshell 컴퓨트 앱이 **두 GPU 모두**에서 메모리/util 사용하는지 확인
   (Phase 2 성공 기준). `etc/multigpu/tools/verify_gpu_fanout.ps1 -Pref dom_webgpu_enabled=true` 활용 가능.
4. **안전성**: pref off거나 비-Windows면 secondary 0개 → 기존 동작 동일. secondary 에러는 전부 무시되어
   페이지 출력 불변.
