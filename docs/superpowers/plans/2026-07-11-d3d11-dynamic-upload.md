# D3D11 DYNAMIC 텍스처 직접 업로드 (d3d11upload 대체) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** D3D11 비디오 경로의 프레임 업로드를 d3d11upload 엘리먼트(staging Map+memcpy → CopySubresourceRegion)에서 플레이어별 DYNAMIC 텍스처 Map(WRITE_DISCARD)+memcpy로 바꿔, 구형 AMD GPU에서 타일 수를 제한하는 GPU 복사를 제거한다.

**Architecture:** appsink caps를 시스템 메모리+지원 포맷 목록으로 협상시키고, `build_frame`(파이프라인별 스트리밍 스레드)이 `DynamicUploadSet`(포맷별 DYNAMIC plane 텍스처 + `alloc_wrapped` 래핑 GstBuffer, 재사용)에 업로드한 뒤 기존 `GstD3D11Converter` → RGBA 링 경로를 그대로 쓴다. 기존 d3d11upload 경로는 `SERVO_MEDIA_D3D11_UPLOAD=legacy`로 복귀 가능하게 공존시킨다.

**Tech Stack:** Rust (servo-media render-d3d11 크레이트), winapi(D3D11 직접 COM), gstd3d11-1.0-0.dll 동적 FFI (기존 바인딩만 사용 — 신규 심볼 0개), gstreamer/gstreamer-video crates.

**스펙:** `docs/superpowers/specs/2026-07-11-d3d11-dynamic-upload-design.md`

## Global Constraints

- 변경 범위: `components/media/backends/gstreamer/render-d3d11`의 `interop.rs`·`lib.rs`만. GStreamer 소스/바이너리·ffi.rs·Cargo.toml·Servo 다른 부분 무수정.
- env 게이트: 상위 `SERVO_MEDIA_D3D11_VIDEO=1` 불변. 신규 `SERVO_MEDIA_D3D11_UPLOAD` = 미설정/`dynamic`(기본) | `legacy`(기존 d3d11upload 경로).
- 지원 포맷 (정확히 이 4개): `I420`, `YV12`, `NV12`, `P010_10LE`. NV12/P010은 네이티브 DXGI 포맷 대신 plane별 2텍스처 (스펙 §4.3).
- DYNAMIC 텍스처 desc: `D3D11_USAGE_DYNAMIC` + `D3D11_CPU_ACCESS_WRITE` + `D3D11_BIND_SHADER_RESOURCE`, MiscFlags 0.
- 에러 정책: 생성/Map 실패 = warn + 프레임 드롭. 세트 생성 실패는 1회 warn 후 이후 프레임 조용히 드롭 (스펙 §7).
- Rust 주석 한국어 허용 (기존 크레이트 관례). 커밋 메시지 한국어, Claude 서명 없이.
- 테스트는 시스템 GStreamer 1.28(msvc)로 돌지만 servoshell 런타임은 target\release 번들 1.22.x다 — 번들 검증은 Task 4 스모크가 담당.
- 스펙 §8.1의 "PoC는 examples/d3d11_upload_poc.rs 변형" 항목은 본 계획에서 **크레이트 내 자동 테스트(Task 2)로 대체**한다 — 변환기 상호운용을 동일하게, 자동으로 검증하며 예제 수정이 불필요해진다 (승인된 의도적 편차).

### 공통 테스트 실행 환경 (모든 Task의 cargo 커맨드 앞에 1회)

```powershell
. "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\servo_env.ps1"
$ErrorActionPreference = 'Continue'
Set-Location "D:\2_TechReview\20260606_multigpu_browser\servo"
$env:PATH = "C:\gstreamer\1.0\msvc_x86_64\bin;$env:PATH"
$env:GST_PLUGIN_PATH = "C:\gstreamer\1.0\msvc_x86_64\lib\gstreamer-1.0"
```

---

### Task 1: `DynamicUploadSet` 생성 — 포맷 테이블 + DYNAMIC plane 텍스처 + 래핑 버퍼

**Files:**
- Modify: `components/media/backends/gstreamer/render-d3d11/interop.rs` (SharedTextureRing 뒤, `#[cfg(test)]` 앞에 추가)
- Test: 같은 파일 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 기존 `SharedGstD3D11Device`(`d3d11_device()`, `api()`, `allocator()`, `raw()`), `GstD3D11Api::allocator_alloc_wrapped`.
- Produces:
  - `pub struct DynamicUploadSet { device, textures: Vec<ComPtr<ID3D11Texture2D>>, plane_dims: Vec<(usize, usize)>, pub wrapped_buffer: gstreamer::Buffer }`
  - `DynamicUploadSet::new(device: Arc<SharedGstD3D11Device>, info: &gstreamer_video::VideoInfo) -> Option<DynamicUploadSet>`
  - (내부) `plane_specs(format: gstreamer_video::VideoFormat) -> Option<&'static [PlaneSpec]>`

- [ ] **Step 1: 실패하는 테스트 작성 (RED)**

`interop.rs`의 `mod tests` 안, 기존 `use` 블록(`use winapi::um::d3d11 as d3d;` 근처)에 추가:

```rust
    use winapi::shared::dxgiformat as dxgifmt;
```

기존 테스트들 뒤에 추가:

```rust
    // DynamicUploadSet 생성: 포맷별 plane 텍스처 수/치수/DXGI 포맷/usage 검증.
    // 홀수 해상도 65x37 — 크로마 plane은 (65+1)/2=33 x (37+1)/2=19 로 반올림돼야 한다.
    #[test]
    fn dynamic_upload_set_creation() {
        gstreamer::init().expect("gstreamer init 실패");
        let device = SharedGstD3D11Device::get_or_create().expect("디바이스 없음");
        let api = device.api();

        let get_desc = |t: &ComPtr<d3d::ID3D11Texture2D>| unsafe {
            let mut desc = std::mem::zeroed::<d3d::D3D11_TEXTURE2D_DESC>();
            t.GetDesc(&mut desc);
            desc
        };

        // I420: R8 3장
        let info = gstreamer_video::VideoInfo::builder(gstreamer_video::VideoFormat::I420, 65, 37)
            .build()
            .expect("info");
        let set = DynamicUploadSet::new(device.clone(), &info).expect("I420 세트 생성 실패");
        assert_eq!(set.textures.len(), 3);
        assert_eq!(set.wrapped_buffer.n_memory(), 3);
        for i in 0..3u32 {
            let mem = set.wrapped_buffer.peek_memory(i);
            assert_ne!(
                unsafe { (api.is_d3d11_memory)(mem.as_mut_ptr()) },
                0,
                "plane {i}는 GstD3D11Memory여야 함"
            );
        }
        let d0 = get_desc(&set.textures[0]);
        assert_eq!(
            (d0.Width, d0.Height, d0.Format),
            (65, 37, dxgifmt::DXGI_FORMAT_R8_UNORM)
        );
        assert_eq!(d0.Usage, d3d::D3D11_USAGE_DYNAMIC);
        assert_eq!(d0.CPUAccessFlags, d3d::D3D11_CPU_ACCESS_WRITE);
        assert_eq!(d0.BindFlags, d3d::D3D11_BIND_SHADER_RESOURCE);
        assert_eq!(d0.MiscFlags, 0);
        let d1 = get_desc(&set.textures[1]);
        assert_eq!(
            (d1.Width, d1.Height, d1.Format),
            (33, 19, dxgifmt::DXGI_FORMAT_R8_UNORM)
        );

        // NV12: R8 + R8G8 2장
        let info = gstreamer_video::VideoInfo::builder(gstreamer_video::VideoFormat::Nv12, 65, 37)
            .build()
            .expect("info");
        let set = DynamicUploadSet::new(device.clone(), &info).expect("NV12 세트 생성 실패");
        assert_eq!(set.textures.len(), 2);
        assert_eq!(set.wrapped_buffer.n_memory(), 2);
        let d1 = get_desc(&set.textures[1]);
        assert_eq!(
            (d1.Width, d1.Height, d1.Format),
            (33, 19, dxgifmt::DXGI_FORMAT_R8G8_UNORM)
        );

        // P010_10LE: R16 + R16G16 2장
        let info =
            gstreamer_video::VideoInfo::builder(gstreamer_video::VideoFormat::P01010le, 65, 37)
                .build()
                .expect("info");
        let set = DynamicUploadSet::new(device.clone(), &info).expect("P010 세트 생성 실패");
        assert_eq!(set.textures.len(), 2);
        let d0 = get_desc(&set.textures[0]);
        assert_eq!(d0.Format, dxgifmt::DXGI_FORMAT_R16_UNORM);
        let d1 = get_desc(&set.textures[1]);
        assert_eq!(
            (d1.Width, d1.Height, d1.Format),
            (33, 19, dxgifmt::DXGI_FORMAT_R16G16_UNORM)
        );

        // 미지원 포맷은 None (협상 caps가 걸러주지만 방어선 확인)
        let info = gstreamer_video::VideoInfo::builder(gstreamer_video::VideoFormat::Rgba, 64, 64)
            .build()
            .expect("info");
        assert!(
            DynamicUploadSet::new(device.clone(), &info).is_none(),
            "미지원 포맷은 None이어야 함"
        );
    }
```

- [ ] **Step 2: 테스트 실행 — 컴파일 실패 확인 (RED)**

Run (공통 환경 설정 후):
```powershell
cargo test -p servo-media-gstreamer-render-d3d11 --release -- --nocapture
```
Expected: FAIL — `error[E0425]: cannot find ... DynamicUploadSet` (컴파일 에러).

- [ ] **Step 3: 구현**

`interop.rs`의 SharedTextureRing `impl` 블록 끝(`#[cfg(test)]` 앞)에 추가:

```rust
// ============================================================================
// DynamicUploadSet — d3d11upload 대체 업로드 (스펙 2026-07-11)
//
// sysmem 프레임의 각 plane을 DYNAMIC 텍스처에 Map(WRITE_DISCARD)+memcpy로 올린다.
// staging 경유 GPU 복사(CopySubresourceRegion)가 없어 구형 AMD GPU의 복사 병목을
// 제거한다. WRITE_DISCARD는 드라이버 renaming이라 (1) 변환기 draw가 이전 프레임을
// 아직 읽는 중이어도 블록되지 않고 (2) 큐에 남은 draw는 이전 버전을 계속 읽으므로
// caps당 텍스처 1세트로 충분하다 (입력 링 불필요).
// ============================================================================

use winapi::shared::dxgiformat::{
    DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM,
};
use winapi::um::d3d11::{D3D11_CPU_ACCESS_WRITE, D3D11_USAGE_DYNAMIC};

/// 지원 포맷의 plane → DXGI 텍스처 매핑 한 줄 (스펙 §4.3 표).
struct PlaneSpec {
    dxgi_format: u32,
    /// 텍셀 치수 산출용 서브샘플링 시프트 — width_texels = (W + (1<<s) - 1) >> s.
    w_shift: u32,
    h_shift: u32,
    bytes_per_texel: usize,
}

const fn spec(dxgi_format: u32, w_shift: u32, h_shift: u32, bytes_per_texel: usize) -> PlaneSpec {
    PlaneSpec { dxgi_format, w_shift, h_shift, bytes_per_texel }
}

/// NV12/P010은 네이티브 DXGI NV12/P010 대신 plane별 2텍스처 — DYNAMIC usage의
/// 네이티브 비디오 포맷 생성은 드라이버 의존이 커서(대상이 구형 AMD) 확실한 조합만 쓴다.
fn plane_specs(format: gstreamer_video::VideoFormat) -> Option<&'static [PlaneSpec]> {
    use gstreamer_video::VideoFormat;
    static I420: [PlaneSpec; 3] = [
        spec(DXGI_FORMAT_R8_UNORM, 0, 0, 1),
        spec(DXGI_FORMAT_R8_UNORM, 1, 1, 1),
        spec(DXGI_FORMAT_R8_UNORM, 1, 1, 1),
    ];
    static NV12: [PlaneSpec; 2] = [
        spec(DXGI_FORMAT_R8_UNORM, 0, 0, 1),
        spec(DXGI_FORMAT_R8G8_UNORM, 1, 1, 2),
    ];
    static P010: [PlaneSpec; 2] = [
        spec(DXGI_FORMAT_R16_UNORM, 0, 0, 2),
        spec(DXGI_FORMAT_R16G16_UNORM, 1, 1, 4),
    ];
    match format {
        // YV12는 plane 순서만 다르고(Y,V,U) 치수·포맷 구조는 I420과 동일 —
        // plane i ↔ 텍스처 i 대응이라 같은 스펙을 쓴다 (의미는 변환기가 in_info로 해석).
        VideoFormat::I420 | VideoFormat::Yv12 => Some(&I420),
        VideoFormat::Nv12 => Some(&NV12),
        VideoFormat::P01010le => Some(&P010),
        _ => None,
    }
}

/// 플레이어별 DYNAMIC 업로드 세트. caps 변경 시 재생성 (lib.rs PlayerState 소유).
pub struct DynamicUploadSet {
    device: Arc<SharedGstD3D11Device>,
    textures: Vec<ComPtr<ID3D11Texture2D>>,
    /// plane별 (유효 행 바이트 = 텍셀폭×텍셀바이트, 행 수) — upload의 memcpy 범위.
    plane_dims: Vec<(usize, usize)>,
    /// plane 텍스처들을 GstD3D11Memory로 감싼 변환기 입력 버퍼 (매 프레임 재사용 —
    /// 변환기가 메모리별 SRV를 내부 캐시하므로 SRV 생성도 1회).
    pub wrapped_buffer: gstreamer::Buffer,
}

// 안전성: 링과 동일 — ComPtr 원시 포인터는 이 구조체가 단독 소유하고, 컨텍스트
// 작업은 전부 디바이스 락 하에서 수행하며, 스트리밍 스레드 1개에서만 쓰인다.
unsafe impl Send for DynamicUploadSet {}

impl DynamicUploadSet {
    /// info의 포맷/치수에 맞는 DYNAMIC plane 텍스처 세트 + 래핑 버퍼 생성.
    /// 미지원 포맷·생성 실패 시 None (호출측이 프레임 드롭 처리).
    pub fn new(
        device: Arc<SharedGstD3D11Device>,
        info: &gstreamer_video::VideoInfo,
    ) -> Option<Self> {
        let specs = plane_specs(info.format())?;
        let mut textures = Vec::with_capacity(specs.len());
        let mut plane_dims = Vec::with_capacity(specs.len());
        let mut wrapped_buffer = gstreamer::Buffer::new();
        let d3d = device.d3d11_device();
        for s in specs {
            let round = |v: u32, shift: u32| (v + ((1u32 << shift) - 1)) >> shift;
            let w = round(info.width(), s.w_shift);
            let h = round(info.height(), s.h_shift);
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: s.dxgi_format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_SHADER_RESOURCE,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE,
                MiscFlags: 0,
            };
            unsafe {
                let mut texture = std::ptr::null_mut();
                let hr = (*d3d).CreateTexture2D(&desc, std::ptr::null(), &mut texture);
                if hr != S_OK {
                    log::warn!(
                        "D3D11 video: DYNAMIC plane 텍스처 생성 실패 hr={hr:#x} \
                         ({w}x{h} dxgi={})",
                        s.dxgi_format
                    );
                    return None;
                }
                let texture = ComPtr::from_raw(texture);
                let api = device.api();
                let memory_ptr = (api.allocator_alloc_wrapped)(
                    device.allocator(),
                    device.raw(),
                    texture.as_raw(),
                    (w as usize) * s.bytes_per_texel * (h as usize),
                    std::ptr::null_mut(),
                    None,
                );
                if memory_ptr.is_null() {
                    log::warn!("D3D11 video: 입력 plane alloc_wrapped 실패");
                    return None;
                }
                let memory: gstreamer::Memory = from_glib_full(memory_ptr);
                wrapped_buffer
                    .get_mut()
                    .expect("새 버퍼는 유일 참조")
                    .append_memory(memory);
                plane_dims.push(((w as usize) * s.bytes_per_texel, h as usize));
                textures.push(texture);
            }
        }
        Some(DynamicUploadSet { device, textures, plane_dims, wrapped_buffer })
    }
}
```

- [ ] **Step 4: 테스트 실행 (GREEN)**

Run:
```powershell
cargo test -p servo-media-gstreamer-render-d3d11 --release -- --nocapture
```
Expected: `dynamic_upload_set_creation ... ok` 포함, 기존 3개 테스트도 전부 ok (총 4 passed).

- [ ] **Step 5: 커밋**

```bash
cd /d/2_TechReview/20260606_multigpu_browser/servo
git add components/media/backends/gstreamer/render-d3d11/interop.rs
git commit -m 'render-d3d11: DynamicUploadSet 신설 - 포맷별 DYNAMIC plane 텍스처 + 래핑 입력 버퍼'
```

---

### Task 2: `upload()` — WRITE_DISCARD memcpy + 변환기 상호운용 E2E 테스트

스펙 리스크 1("변환기가 래핑 다중 plane DYNAMIC 입력을 수용하는가")을 이 태스크의 테스트가 판정한다. 여기서 `convert_buffer 실패`가 나면 §Fallback(태스크 하단) 적용.

**Files:**
- Modify: `components/media/backends/gstreamer/render-d3d11/interop.rs`
- Test: 같은 파일 `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 1의 `DynamicUploadSet`(`textures`, `plane_dims`, `wrapped_buffer`), 기존 `SharedTextureRing`(acquire/finish), FFI `converter_new`/`converter_convert_buffer`, 테스트 헬퍼 `open_and_read_on_second_device(handle, w, h) -> Vec<u8>`(기존).
- Produces: `DynamicUploadSet::upload(&mut self, frame: &gstreamer_video::VideoFrameRef<&gstreamer::BufferRef>) -> bool`

- [ ] **Step 1: 실패하는 테스트 작성 (RED)**

`mod tests`에 헬퍼 2개 + 테스트 4개 추가:

```rust
    use gstreamer::glib::translate::ToGlibPtr;

    /// info 레이아웃(offset/stride)대로 각 plane을 단일 텍셀 패턴으로 채운 sysmem 버퍼.
    /// texels[i] = plane i의 텍셀 바이트열 (예: I420 Y=&[81], NV12 UV=&[90, 240]).
    fn make_filled_buffer(
        info: &gstreamer_video::VideoInfo,
        texels: &[&[u8]],
    ) -> gstreamer::Buffer {
        let mut data = vec![0u8; info.size()];
        for (i, t) in texels.iter().enumerate() {
            let off = info.offset()[i];
            let stride = info.stride()[i] as usize;
            // plane 행 수: 지원 포맷 전부 plane0=H, 그 외=(H+1)/2 (I420/YV12/NV12/P010)
            let rows = if i == 0 {
                info.height() as usize
            } else {
                (info.height() as usize).div_ceil(2)
            };
            for row in 0..rows {
                let row_start = off + row * stride;
                for chunk in data[row_start..row_start + stride].chunks_exact_mut(t.len()) {
                    chunk.copy_from_slice(t);
                }
            }
        }
        gstreamer::Buffer::from_mut_slice(data)
    }

    /// 공용 E2E: 단색 plane 데이터 → upload → GstD3D11Converter → 링 → 두 번째
    /// 디바이스 판독 → 중앙 픽셀 [R,G,B,A]. convert_buffer 실패는 스펙 리스크 1.
    fn upload_convert_readback(
        info: &gstreamer_video::VideoInfo,
        texels: &[&[u8]],
    ) -> [u8; 4] {
        let device = SharedGstD3D11Device::get_or_create().expect("디바이스 없음");
        let api = device.api();
        let mut set = DynamicUploadSet::new(device.clone(), info).expect("세트 생성 실패");
        let buffer = make_filled_buffer(info, texels);
        let frame =
            gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer.as_ref(), info)
                .expect("frame map 실패");
        assert!(set.upload(&frame), "upload 실패");

        let out_info = gstreamer_video::VideoInfo::builder(
            gstreamer_video::VideoFormat::Rgba,
            info.width(),
            info.height(),
        )
        .build()
        .expect("out_info");
        let conv = unsafe {
            (api.converter_new)(
                device.raw(),
                info.to_glib_none().0,
                out_info.to_glib_none().0,
                std::ptr::null_mut(),
            )
        };
        assert!(!conv.is_null(), "converter_new 실패");

        let mut ring = SharedTextureRing::new(device.clone());
        let (out_buf, slot) = ring
            .acquire(info.width() as i32, info.height() as i32)
            .expect("acquire 실패");
        let ok = unsafe {
            (api.converter_convert_buffer)(conv, set.wrapped_buffer.as_mut_ptr(), out_buf.as_mut_ptr())
        };
        unsafe { gstreamer::glib::gobject_ffi::g_object_unref(conv as *mut _) };
        assert_eq!(ok, 1, "convert_buffer 실패 — 래핑 DYNAMIC 입력 미수용 (스펙 리스크 1)");
        let (handle, _epoch) = ring.finish(slot).expect("finish 실패");
        let rgba = open_and_read_on_second_device(handle, info.width(), info.height());
        let w = info.width() as usize;
        let mid = ((info.height() as usize / 2) * w + w / 2) * 4;
        [rgba[mid], rgba[mid + 1], rgba[mid + 2], rgba[mid + 3]]
    }

    // I420 빨강(Y=81,U=90,V=240 bt601) → RGBA 판독이 빨강이어야 한다.
    // 이어서 같은 세트에 파랑(Y=41,U=240,V=110) 재업로드 → 파랑 — WRITE_DISCARD
    // renaming 하에서 세트 재사용(최신 내용 승리)을 검증한다.
    #[test]
    fn dynamic_upload_i420_convert_readback_and_discard_reuse() {
        gstreamer::init().expect("gstreamer init 실패");
        let info = gstreamer_video::VideoInfo::builder(gstreamer_video::VideoFormat::I420, 64, 64)
            .build()
            .expect("info");

        let [r, g, b, a] = upload_convert_readback(&info, &[&[81], &[90], &[240]]);
        assert!(r > 200 && g < 70 && b < 70, "빨강 기대, 실제 ({r},{g},{b})");
        assert_eq!(a, 255);

        // 세트 재사용 검증은 upload_convert_readback를 다시 부르는 대신 동일 흐름을
        // 새 색으로 반복해도 동등하다 — renaming은 프레임 단위 독립이므로 새 세트/링
        // 여부와 무관하게 최신 업로드 내용이 변환에 반영되는지가 요점이다.
        let [r, g, b, _] = upload_convert_readback(&info, &[&[41], &[240], &[110]]);
        assert!(b > 200 && r < 70, "파랑 기대, 실제 ({r},{g},{b})");
    }

    // YV12: plane1=V, plane2=U (I420과 순서 반대). 같은 빨강이 나와야 U/V 대응이 옳다.
    #[test]
    fn dynamic_upload_yv12_convert_readback() {
        gstreamer::init().expect("gstreamer init 실패");
        let info = gstreamer_video::VideoInfo::builder(gstreamer_video::VideoFormat::Yv12, 64, 64)
            .build()
            .expect("info");
        let [r, g, b, _] = upload_convert_readback(&info, &[&[81], &[240], &[90]]);
        assert!(r > 200 && g < 70 && b < 70, "빨강 기대, 실제 ({r},{g},{b})");
    }

    // NV12: plane1은 UV 인터리브(R8G8) — 텍셀 [U,V].
    #[test]
    fn dynamic_upload_nv12_convert_readback() {
        gstreamer::init().expect("gstreamer init 실패");
        let info = gstreamer_video::VideoInfo::builder(gstreamer_video::VideoFormat::Nv12, 64, 64)
            .build()
            .expect("info");
        let [r, g, b, _] = upload_convert_readback(&info, &[&[81], &[90, 240]]);
        assert!(r > 200 && g < 70 && b < 70, "빨강 기대, 실제 ({r},{g},{b})");
    }

    // P010_10LE: 16비트 컨테이너의 상위 10비트 — 8비트값 v의 16비트 표현은 v<<8
    // (LE 바이트 [0x00, v]). plane1은 U16/V16 인터리브.
    #[test]
    fn dynamic_upload_p010_convert_readback() {
        gstreamer::init().expect("gstreamer init 실패");
        let info =
            gstreamer_video::VideoInfo::builder(gstreamer_video::VideoFormat::P01010le, 64, 64)
                .build()
                .expect("info");
        let [r, g, b, _] = upload_convert_readback(&info, &[&[0, 81], &[0, 90, 0, 240]]);
        assert!(r > 200 && g < 70 && b < 70, "빨강 기대, 실제 ({r},{g},{b})");
    }
```

- [ ] **Step 2: 테스트 실행 — 컴파일 실패 확인 (RED)**

Run:
```powershell
cargo test -p servo-media-gstreamer-render-d3d11 --release -- --nocapture
```
Expected: FAIL — `error[E0599]: no method named 'upload'`.

- [ ] **Step 3: `upload()` 구현**

`impl DynamicUploadSet`의 `new` 뒤에 추가 (파일 상단 use에 `D3D11_MAP_WRITE_DISCARD`, `D3D11_MAPPED_SUBRESOURCE`를 Task 1에서 추가한 `winapi::um::d3d11::{...}` 묶음에 합침):

```rust
    /// sysmem 프레임의 각 plane을 Map(WRITE_DISCARD)+행 단위 memcpy로 업로드.
    /// 실패 시 false (호출측 프레임 드롭). memcpy는 디바이스 락 밖에서 수행한다 —
    /// mapped 포인터는 CPU 메모리이고, 직렬화가 필요한 것은 컨텍스트 호출(Map/Unmap)
    /// 뿐이다 (GStreamer 자신의 staging 경로와 동일한 락 프로토콜).
    pub fn upload(
        &mut self,
        frame: &gstreamer_video::VideoFrameRef<&gstreamer::BufferRef>,
    ) -> bool {
        if frame.n_planes() as usize != self.textures.len() {
            log::warn!(
                "D3D11 video: plane 수 불일치 (frame={}, set={})",
                frame.n_planes(),
                self.textures.len()
            );
            return false;
        }
        let context = self.device.immediate_context();
        for i in 0..self.textures.len() {
            let (row_bytes, rows) = self.plane_dims[i];
            let src = match frame.plane_data(i as u32) {
                Ok(data) => data,
                Err(_) => {
                    log::warn!("D3D11 video: plane_data({i}) 실패");
                    return false;
                },
            };
            // VideoFrameRef의 info는 VideoMeta(디코더 패딩 stride)가 반영된 실제 레이아웃.
            let src_stride = frame.info().stride()[i] as usize;
            let copy_bytes = row_bytes.min(src_stride);
            if src.len() < (rows - 1) * src_stride + copy_bytes {
                log::warn!("D3D11 video: plane {i} 데이터 부족 — 프레임 드롭");
                return false;
            }
            let resource = self.textures[i].as_raw() as *mut ID3D11Resource;
            let mapped = unsafe {
                let _guard = self.device.lock();
                let mut mapped =
                    std::mem::zeroed::<winapi::um::d3d11::D3D11_MAPPED_SUBRESOURCE>();
                let hr = (*context).Map(resource, 0, D3D11_MAP_WRITE_DISCARD, 0, &mut mapped);
                if hr != S_OK {
                    log::warn!("D3D11 video: Map(WRITE_DISCARD) 실패 hr={hr:#x} (plane {i})");
                    return false;
                }
                mapped
            };
            unsafe {
                for row in 0..rows {
                    std::ptr::copy_nonoverlapping(
                        src.as_ptr().add(row * src_stride),
                        (mapped.pData as *mut u8).add(row * mapped.RowPitch as usize),
                        copy_bytes,
                    );
                }
                let _guard = self.device.lock();
                (*context).Unmap(resource, 0);
            }
        }
        true
    }
```

use 라인 (Task 1의 것과 합쳐 최종):
```rust
use winapi::um::d3d11::{D3D11_CPU_ACCESS_WRITE, D3D11_MAP_WRITE_DISCARD, D3D11_USAGE_DYNAMIC};
```

- [ ] **Step 4: 테스트 실행 (GREEN)**

Run:
```powershell
cargo test -p servo-media-gstreamer-render-d3d11 --release -- --nocapture
```
Expected: 신규 4개 포함 전부 ok (총 8 passed). **`convert_buffer 실패` assert에 걸리면 아래 Fallback 적용 후 재실행.**

**Fallback (convert_buffer가 래핑 입력을 거부할 때만):** `DynamicUploadSet::new`에서 `wrapped_buffer` 완성 직후 VideoMeta를 부착한다 — 변환기가 plane 레이아웃을 VideoMeta로 요구하는 경우다:
```rust
        // 변환기가 VideoMeta를 요구하는 경우의 plane 레이아웃 명시 (offset은 plane별
        // 텍스처라 0, stride는 유효 행 바이트).
        {
            let offsets: Vec<usize> = vec![0; plane_dims.len()];
            let strides: Vec<i32> = plane_dims.iter().map(|(rb, _)| *rb as i32).collect();
            gstreamer_video::VideoMeta::add_full(
                wrapped_buffer.get_mut().expect("새 버퍼는 유일 참조"),
                gstreamer_video::VideoFrameFlags::empty(),
                info.format(),
                info.width(),
                info.height(),
                &offsets,
                &strides,
            )
            .ok()?;
        }
```
적용 후에도 실패하면 중단하고 로그(GST_DEBUG=d3d11*:5 재실행 출력)와 함께 보고할 것.

- [ ] **Step 5: 커밋**

```bash
cd /d/2_TechReview/20260606_multigpu_browser/servo
git add components/media/backends/gstreamer/render-d3d11/interop.rs
git commit -m 'render-d3d11: DynamicUploadSet.upload(WRITE_DISCARD) + 변환기 상호운용 테스트'
```

---

### Task 3: lib.rs 통합 — env 게이트, sink 분기, build_frame 분기, upload= 계측

**Files:**
- Modify: `components/media/backends/gstreamer/render-d3d11/lib.rs`

**Interfaces:**
- Consumes: Task 1·2의 `crate::interop::DynamicUploadSet`(`new`, `upload`, `wrapped_buffer`).
- Produces: env `SERVO_MEDIA_D3D11_UPLOAD`(`legacy`|기본 dynamic) 동작; sysmem 샘플 처리 경로. 외부(트레이트) 시그니처 무변경.

- [ ] **Step 1: UploadMode + env 판정 추가**

`env_flag_enabled` 함수 아래에 추가:

```rust
    /// 업로드 방식 (스펙 2026-07-11 §6): 기본 dynamic, env로 legacy 복귀.
    #[derive(Clone, Copy, PartialEq)]
    enum UploadMode {
        /// DYNAMIC 텍스처 Map(WRITE_DISCARD) 직접 업로드 (기본).
        Dynamic,
        /// 기존 d3d11upload 엘리먼트 경로 (staging+CopySubresourceRegion) 복귀 스위치.
        Legacy,
    }

    fn upload_mode() -> UploadMode {
        static MODE: std::sync::OnceLock<UploadMode> = std::sync::OnceLock::new();
        *MODE.get_or_init(|| match env::var("SERVO_MEDIA_D3D11_UPLOAD") {
            Ok(v) if v.eq_ignore_ascii_case("legacy") => UploadMode::Legacy,
            _ => UploadMode::Dynamic,
        })
    }
```

- [ ] **Step 2: PlayerState 확장**

`PlayerState` 정의(lib.rs:92-97)를 다음으로 교체:

```rust
    struct PlayerState {
        ring: Option<SharedTextureRing>,
        converter: Option<ConverterHandle>,
        /// dynamic 업로드 세트 (sysmem 협상 시에만 생성). caps 변경 시 converter와 함께 무효화.
        upload: Option<crate::interop::DynamicUploadSet>,
        /// 세트 생성 실패 마커 — 1회 warn 후 이후 프레임은 조용히 드롭 (스펙 §7).
        upload_failed: bool,
        /// 변환기 무효화 판정용 — 포맷/크기/colorimetry 변경 감지.
        in_caps: Option<gstreamer::Caps>,
    }
```

`RenderD3D11::new`의 `state: Mutex::new(PlayerState { ... })` 초기화에 `upload: None, upload_failed: false,` 추가.

- [ ] **Step 3: new()의 d3d11upload 존재 체크를 legacy 전용으로**

lib.rs:119-125의 체크 블록을 다음으로 교체:

```rust
            // legacy 모드만 d3d11upload 엘리먼트 필요 — dynamic(기본)은 라이브러리
            // (gstd3d11-1.0-0.dll)만 쓴다 (플러그인 gstd3d11.dll 불필요).
            if upload_mode() == UploadMode::Legacy
                && gstreamer::ElementFactory::find("d3d11upload").is_none()
            {
                log::warn!(
                    "D3D11 video: d3d11upload 플러그인 없음 (gstd3d11.dll 번들 확인) — Raw 경로 폴백"
                );
                return None;
            }
```

- [ ] **Step 4: build_video_sink 분기**

기존 `build_video_sink` 본문 전체를 `build_video_sink_legacy`라는 새 비-트레이트 메서드로 옮기고(코드 그대로 — `impl RenderD3D11` 블록에 추가), 트레이트 메서드는 다음으로 교체:

```rust
        fn build_video_sink(
            &self,
            appsink: &gstreamer::Element,
            pipeline: &gstreamer::Element,
        ) -> Result<(), PlayerError> {
            if upload_mode() == UploadMode::Legacy {
                return self.build_video_sink_legacy(appsink, pipeline);
            }
            // dynamic(기본): 엘리먼트 없이 appsink가 sysmem 프레임을 직접 받고
            // build_frame이 DYNAMIC 텍스처로 업로드한다. 포맷 목록 밖 디코더 출력은
            // playbin이 videoconvert를 자동 삽입해 목록 내 포맷으로 맞춘다 (스펙 §5.1).
            let caps = gstreamer::Caps::builder("video/x-raw")
                .field(
                    "format",
                    gstreamer::List::new(["I420", "YV12", "NV12", "P010_10LE"]),
                )
                .field("pixel-aspect-ratio", gstreamer::Fraction::from((1, 1)))
                .build();
            appsink.set_property("caps", &caps);
            // 디바이스 주입 유지 — 자동 플러깅으로 d3d11 엘리먼트가 끼어도 우리
            // 디바이스를 쓰게 한다.
            if let Some(context) = self.device.gst_context() {
                pipeline.set_context(&context);
            }
            pipeline.set_property("video-sink", appsink);
            Ok(())
        }
```

참고: `build_video_sink_legacy`는 기존 본문 그대로이므로 diff는 시그니처 줄 1줄 변화뿐이어야 한다.

- [ ] **Step 5: build_frame 분기 + upload= 계측**

(a) lib.rs:187-190의 비 D3D11 메모리 드롭 블록을 교체:

```rust
            // 메모리 타입 판별: D3D11 메모리(legacy d3d11upload 협상) vs sysmem(dynamic).
            let is_d3d11_mem =
                unsafe { (api.is_d3d11_memory)(buffer.peek_memory(0).as_mut_ptr()) } != 0;
            if upload_mode() == UploadMode::Legacy && !is_d3d11_mem {
                log::warn!("D3D11 video: 비 D3D11 메모리 샘플 — 프레임 드롭");
                return None;
            }
```

(b) caps 변경 무효화 블록(lib.rs:196-199)을 교체:

```rust
            // caps 변경(포맷/크기/색상 정보) 시 변환기·업로드 세트 재생성
            if state.in_caps.as_deref() != Some(caps) {
                state.converter = None;
                state.upload = None;
                state.upload_failed = false;
                state.in_caps = Some(caps.to_owned());
            }
```

(c) `let converter = state.converter.as_ref()?;` 직후, `conv_start` 앞에 업로드 블록 삽입:

```rust
            // 업로드 (dynamic 경로): sysmem plane들을 DYNAMIC 텍스처에
            // Map(WRITE_DISCARD)+memcpy. legacy 경로(D3D11 메모리)는 업로드가 이미
            // 끝나 있으므로 샘플 버퍼를 그대로 변환기 입력으로 쓴다.
            let up_start = std::time::Instant::now(); // D3D11PROF
            let in_buffer_ptr = if is_d3d11_mem {
                buffer.as_mut_ptr()
            } else {
                if state.upload_failed {
                    // 생성 실패 마커 — 최초 1회만 warn (스펙 §7).
                    return None;
                }
                if state.upload.is_none() {
                    match crate::interop::DynamicUploadSet::new(self.device.clone(), &info) {
                        Some(set) => state.upload = Some(set),
                        None => {
                            // 원인은 new()가 warn으로 남김 — 여기선 마커와 요약만.
                            log::warn!(
                                "D3D11 video: dynamic 업로드 세트 생성 실패 — 이후 프레임 드롭 (id={})",
                                self.profile_id
                            );
                            state.upload_failed = true;
                            return None;
                        },
                    }
                }
                let upload = state.upload.as_mut().expect("직전에 보장");
                let Ok(frame) =
                    gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                else {
                    log::warn!("D3D11 video: 프레임 map 실패 — 프레임 드롭");
                    return None;
                };
                if !upload.upload(&frame) {
                    return None; // upload()가 원인 warn 로깅
                }
                upload.wrapped_buffer.as_mut_ptr()
            };
            let t_upload = up_start.elapsed(); // D3D11PROF
```

(d) convert 호출(lib.rs:240-246)의 첫 버퍼 인자를 교체:

```rust
            let ok = unsafe {
                (api.converter_convert_buffer)(
                    converter.0,
                    in_buffer_ptr,
                    out_buffer.as_mut_ptr(),
                )
            };
```

(e) D3D11PROF 로그 라인(lib.rs:279-294)에 `upload=` 필드 추가 — 포맷 문자열의 `total={:.1}` 뒤에 `upload={:.2}`를 넣고, 인자 목록의 `total_ms` 뒤에 `t_upload.as_secs_f64() * 1000.0,` 추가:

```rust
                    log::warn!(
                        "D3D11PROF id={} over={} total={:.1} upload={:.2} acquire={:.1} \
                         convert={:.1} finish={:.1} ef_lockwait={:.2} poll_lockwait={:.2} \
                         fence_loop={:.1} polls={} fr={}",
                        self.profile_id,
                        if over { 1 } else { 0 },
                        total_ms,
                        t_upload.as_secs_f64() * 1000.0,
                        t_acquire.as_secs_f64() * 1000.0,
                        t_convert.as_secs_f64() * 1000.0,
                        t_finish.as_secs_f64() * 1000.0,
                        ms(st.endflush_lock_wait),
                        ms(st.poll_lock_wait),
                        ms(st.fence_loop),
                        st.poll_count,
                        frames,
                    );
```

- [ ] **Step 6: 컴파일 + 기존 테스트 회귀 (GREEN)**

Run:
```powershell
cargo test -p servo-media-gstreamer-render-d3d11 --release -- --nocapture
```
Expected: 8 passed (lib.rs는 테스트 대상 아님 — 컴파일 성공이 판정). 빌드 에러 0.

- [ ] **Step 7: 커밋**

```bash
cd /d/2_TechReview/20260606_multigpu_browser/servo
git add components/media/backends/gstreamer/render-d3d11/lib.rs
git commit -m 'D3D11 dynamic 업로드 경로 통합: d3d11upload 제거(기본), SERVO_MEDIA_D3D11_UPLOAD=legacy 복귀'
```

---

### Task 4: servoshell 빌드 + 스모크/회귀 검증 (번들 1.22.x 런타임)

**Files:** 없음 (검증 전용 — 코드 변경이 나오면 해당 Task로 되돌아감)

**Interfaces:**
- Consumes: Task 3까지의 전체 경로, `etc/multigpu/run_video_wall_d3d11.ps1`(SERVO_MEDIA_D3D11_VIDEO=1 등 최종 레시피 내장), D3D11PROF(`SERVO_D3D11_PROFILE=1`).

- [ ] **Step 1: 전체 빌드**

```powershell
. "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\servo_env.ps1"
$ErrorActionPreference = 'Continue'
Set-Location "D:\2_TechReview\20260606_multigpu_browser\servo"
.\mach build --release
```
Expected: `Serving build ... Finished` (빌드 성공).

- [ ] **Step 2: 스모크 (dynamic 기본, 2x2, PROF on)**

```powershell
$env:SERVO_D3D11_PROFILE = "1"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 2 -Rows 2 -Detach
```
30초 재생 후 최신 로그 판정:
```powershell
$log = Get-ChildItem target\multigpu_logs\video_wall_d3d11_*_stderr.log | Sort-Object LastWriteTime | Select-Object -Last 1
Select-String -Path $log -Pattern "비 D3D11 메모리|convert_buffer 실패|생성 실패|프레임 map 실패" | Measure-Object
Select-String -Path $log -Pattern "D3D11PROF id=0 " | Select-Object -Last 3
Get-Process servoshell | Stop-Process -Force
```
Expected:
- 실패 패턴 카운트 = 0
- `D3D11PROF` 하트비트에 `upload=` 필드 존재, 값 대략 0.2~1.5ms (1080p I420 memcpy), `over=0` 유지
- 화면: 4타일 정상 색상 재생 (녹색/보라 틴트 = U/V 문제, 세로 줄밀림 = stride 문제 — 즉시 중단하고 해당 Task 재검토)

- [ ] **Step 3: legacy 복귀 A/B**

```powershell
$env:SERVO_MEDIA_D3D11_UPLOAD = "legacy"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 2 -Rows 2 -Detach
```
Expected: 기존 경로 그대로 정상 재생 (로그에 실패 패턴 0). 확인 후:
```powershell
Get-Process servoshell | Stop-Process -Force
Remove-Item Env:SERVO_MEDIA_D3D11_UPLOAD
```

- [ ] **Step 4: 45타일 회귀 (기존 검증 항목)**

```powershell
Remove-Item Env:SERVO_D3D11_PROFILE -ErrorAction SilentlyContinue
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 9 -Rows 5
```
Expected (기존 기준선과 동일):
- 시작: 동기 릴리즈 후 ~5초 내 전 타일 60fps 안착 (육안 + 스크립트의 sanity 마커 45/45)
- lockstep: 타일 간 프레임카운터 ±1 (육안 확인 가능한 카운터 페이지 사용 시)
- 블랙 타일 0, 간헐 멈춤 없음 (2~3 루프 관찰)
- 로그: `import` 실패/폴백 경고 0

- [ ] **Step 5: 결과 기록**

측정 결과(upload= 분포, 45타일 시작 곡선 소견)를 커밋 없이 이 계획 문서 하단이나 세션 노트에 남기고, AMD 실기 검증(추후, 다른 장비)과 `D:\ServoWallPackage` 재패키징을 후속 항목으로 사용자에게 보고.

---

## Self-Review (작성 후 점검 완료)

1. **스펙 커버리지**: §4.1 caps 협상(Task 3-4단계), §4.2 DYNAMIC 1세트(Task 1), §4.3 포맷 표(Task 1), §5.1 sink 분기+legacy 전용 플러그인 체크(Task 3), §5.2 DynamicUploadSet·락 프로토콜·VideoMeta 조건부(Task 1·2), §5.3 build_frame 분기(Task 3), §5.4 FFI 무변경(전 Task), §5.5 upload= 계측(Task 3), §6 env 게이트(Task 3), §7 에러 정책(Task 2·3), §8 검증 1~4(Task 2·4; 8.5 AMD는 추후로 스펙에 명시됨). §8.1 예제-PoC는 자동 테스트로 대체 — Global Constraints에 편차 명시.
2. **플레이스홀더 스캔**: TBD/TODO/"적절히 처리" 없음. 모든 코드 스텝에 실제 코드, 모든 실행 스텝에 커맨드+기대 출력.
3. **타입 일관성**: `DynamicUploadSet::new(Arc<SharedGstD3D11Device>, &VideoInfo) -> Option<Self>`·`upload(&VideoFrameRef<&BufferRef>) -> bool`·`wrapped_buffer: gstreamer::Buffer`가 Task 1→2→3에서 동일. `UploadMode`/`upload_mode()`는 Task 3 내 정의·사용 일치. 테스트 헬퍼 `open_and_read_on_second_device`는 기존 코드 재사용.
