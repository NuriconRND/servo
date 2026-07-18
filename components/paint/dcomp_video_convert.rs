/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! raw D3D11 YUV→RGBA 변환 패스(VideoConvertPass, Task 4 — 비디오 WR 탈출 사이클).
//! 비디오 plane 텍스처(Y/U/V 3장 또는 Y/UV 2장)를 목적지 RTV(dst_size)로
//! 스케일+색변환하는 1-draw 풀스크린 패스다. `SwapDeviceContextState`로 ANGLE의
//! 렌더 상태와 완전히 격리된 자체 `ID3DDeviceContextState`에서 그리므로, 이 패스가
//! 끝나면 ANGLE/WR 쪽 파이프라인 상태는 호출 전과 동일하게 복원된다.
//! Task 5(dcomp_compositor)가 비디오별 DComp 스왑체인 백버퍼를 rtv로 넘겨 소비한다.
#![allow(unsafe_code)]
// 이 파일은 사실상 전체가 raw D3D11/COM FFI다 — `unsafe fn` 본문 안의 개별 연산마다
// 중첩 `unsafe {}`를 요구하는 edition 2024 린트는 신호 대비 잡음이 크므로 끈다
// (dcomp_compositor.rs의 `#![allow(unsafe_code)]`와 같은 취지).
#![allow(unsafe_op_in_unsafe_fn)]

use std::ffi::CString;
use std::ptr;

use log::warn;
use paint_api::{VideoFrameLease, VideoLeaseColorRange, VideoLeaseColorSpace, VideoLeaseFormat};
use winapi::Interface;
use winapi::shared::minwindef::{FALSE, TRUE};
use winapi::um::d3d11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BLEND_DESC, D3D11_BUFFER_DESC,
    D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_WRITE,
    D3D11_CULL_NONE, D3D11_FILL_SOLID, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FLOAT32_MAX,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE_DISCARD, D3D11_RASTERIZER_DESC,
    D3D11_RENDER_TARGET_BLEND_DESC, D3D11_SAMPLER_DESC, D3D11_SDK_VERSION,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT, ID3D11BlendState,
    ID3D11Buffer, ID3D11Device, ID3D11PixelShader, ID3D11RasterizerState,
    ID3D11RenderTargetView, ID3D11Resource, ID3D11SamplerState, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11VertexShader,
};
use winapi::um::d3d11_1::{ID3D11Device1, ID3D11DeviceContext1, ID3DDeviceContextState};
use winapi::um::d3dcommon::{
    D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, ID3DBlob,
};
use winapi::um::d3dcompiler::D3DCompile;
use winapi::um::unknwnbase::IUnknown;

use crate::dcomp_compositor::ComOwned;

// Fullscreen-triangle YUV -> RGBA conversion shader (Task 4 design table). Comments in
// English per project convention for HLSL sources. Compiled twice at runtime (vs_5_0 /
// ps_5_0) from the same source string with different entry points.
const SHADER_SRC: &str = r#"
struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VSOut vs_main(uint id : SV_VertexID) {
    // Fullscreen triangle, no vertex buffer.
    VSOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    o.uv = uv;
    return o;
}

Texture2D texY : register(t0);
Texture2D texUorUV : register(t1);
Texture2D texV : register(t2);
SamplerState sampLinear : register(s0);
cbuffer ConvertParams : register(b0) {
    float4 mat_r;   // xyz = row, w unused
    float4 mat_g;
    float4 mat_b;
    float4 offs;    // xyz = (y_off, 0.5, 0.5) or (0, 0.5, 0.5)
    float4 misc;    // x = rescale, y = interleaved_uv (0/1)
};
float4 ps_main(VSOut i) : SV_Target {
    float y = texY.Sample(sampLinear, i.uv).r;
    float u, v;
    if (misc.y > 0.5) {
        float2 uv2 = texUorUV.Sample(sampLinear, i.uv).rg;
        u = uv2.x; v = uv2.y;
    } else {
        u = texUorUV.Sample(sampLinear, i.uv).r;
        v = texV.Sample(sampLinear, i.uv).r;
    }
    float3 yuv = float3(y, u, v) * misc.x - offs.xyz;
    float3 rgb = float3(dot(mat_r.xyz, yuv), dot(mat_g.xyz, yuv), dot(mat_b.xyz, yuv));
    return float4(saturate(rgb), 1.0);
}
"#;

/// D3DCompile 런타임 컴파일 헬퍼(d3dcompiler_47, OS 인박스). 성공 시 블롭을 돌려주며,
/// 호출자가 GetBufferPointer/Size로 바이트코드를 꺼내 CreateVertexShader/CreatePixelShader에
/// 넘긴 뒤 Release할 책임을 진다.
unsafe fn compile_shader(source: &str, entry: &str, target: &str) -> Option<*mut ID3DBlob> {
    let entry_c = CString::new(entry).ok()?;
    let target_c = CString::new(target).ok()?;
    let mut code: *mut ID3DBlob = ptr::null_mut();
    let mut errors: *mut ID3DBlob = ptr::null_mut();
    let hr = D3DCompile(
        source.as_ptr() as *const _,
        source.len(),
        ptr::null(),
        ptr::null(),
        ptr::null_mut(),
        entry_c.as_ptr(),
        target_c.as_ptr(),
        0,
        0,
        &mut code,
        &mut errors,
    );
    if hr < 0 || code.is_null() {
        if !errors.is_null() {
            let msg = std::slice::from_raw_parts(
                (*errors).GetBufferPointer() as *const u8,
                (*errors).GetBufferSize(),
            );
            warn!(
                "[video-convert] D3DCompile({}) failed (hr=0x{:08x}): {}",
                entry,
                hr as u32,
                String::from_utf8_lossy(msg)
            );
            (*(errors as *mut IUnknown)).Release();
        } else {
            warn!("[video-convert] D3DCompile({}) failed (hr=0x{:08x})", entry, hr as u32);
        }
        return None;
    }
    if !errors.is_null() {
        (*(errors as *mut IUnknown)).Release();
    }
    Some(code)
}

/// draw 직후 per-convert SRV들을 Release한다(null은 건너뜀). Safety: 각 non-null 포인터는
/// `create_srv`가 돌려준, 아직 Release되지 않은 SRV여야 한다.
unsafe fn release_srvs(srvs: &[*mut ID3D11ShaderResourceView; 3]) {
    for &s in srvs {
        if !s.is_null() {
            (*(s as *mut IUnknown)).Release();
        }
    }
}

/// 비디오별 DComp 스왑체인 백버퍼로 YUV plane을 스케일+색변환하는 1-draw 패스.
/// `device`는 ANGLE 하부 D3D11 디바이스(비소유 — 호출자가 수명 보장; plane SRV 생성에
/// 쓰인다). 나머지 필드는 전부 `new()` 1회 생성 후 재사용되는 파이프라인 자원.
/// `ComOwned`(Send/Sync 아님)를 담고 있으므로 이 구조체도 렌더러 스레드 전용이다.
///
/// plane SRV는 캐시하지 않고 convert()마다 새로 만들고 draw 직후 Release한다. 링 plane
/// 텍스처는 `D3D11_USAGE_DYNAMIC`이라 소비 슬롯이 다시 present될 때마다 그 사이의
/// `Map(WRITE_DISCARD)`로 백킹 할당이 rename(교체)된다 — 이전 백킹에 대해 만든 SRV를
/// 캐시해 재사용하면 rename 뒤 그 SRV가 이미 해제된 rename 버퍼를 가리켜(dangling), NVIDIA
/// D3D11 드라이버가 커맨드 제출 시 그 메모리를 역참조해 AV로 죽는다(규모=동시 비디오 수에
/// 비례해 빨리 발현). ANGLE의 native 경로가 안정적인 이유도 매 draw 뷰를 다시 유도하기
/// 때문 — fresh-per-convert가 그와 동치다.
pub(crate) struct VideoConvertPass {
    device: *mut ID3D11Device,
    vs: ComOwned<ID3D11VertexShader>,
    ps: ComOwned<ID3D11PixelShader>,
    sampler: ComOwned<ID3D11SamplerState>,
    cbuffer: ComOwned<ID3D11Buffer>,
    blend_state: ComOwned<ID3D11BlendState>,
    raster_state: ComOwned<ID3D11RasterizerState>,
    /// SwapDeviceContextState로 활성화할 격리 상태(FeatureLevel 11_0,
    /// EmulatedInterface=ID3D11Device — ANGLE의 D3D10 비호환은 우리와 무관).
    context_state: ComOwned<ID3DDeviceContextState>,
    /// begin_batch가 연 배치 상태. `Some(prev)` = 배치 활성(prev = begin_batch의
    /// SwapDeviceContextState가 AddRef해 돌려준 직전 ANGLE 상태 — end_batch에서 복원 후
    /// Release). 활성인 동안 convert()는 상태 스왑을 생략하고 per-draw 자원만 재바인딩한다
    /// (정적 파이프라인 상태는 begin_batch에서 1회 바인딩). 렌더러 스레드 전용이라 raw 포인터
    /// 보관 OK. `None` = 비활성(유닛 테스트/단독 호출은 항상 이 상태 — convert가 자체 스왑).
    batch: Option<*mut ID3DDeviceContextState>,
}

impl VideoConvertPass {
    /// ANGLE 하부 D3D11 디바이스로 1회 생성. D3DCompile 런타임 컴파일 + 파이프라인 자원
    /// 일체를 준비한다. 실패(QI/컴파일/자원 생성 HRESULT 실패)면 None — 호출측(Task 5)이
    /// warn-once 후 external surface 경로를 스킵한다.
    ///
    /// Safety: `device`는 유효하고 살아있는 `ID3D11Device`여야 하며, 이 함수가 반환하는
    /// `VideoConvertPass`가 살아있는 동안 유효해야 한다(비소유 참조).
    pub(crate) unsafe fn new(device: *mut ID3D11Device) -> Option<VideoConvertPass> {
        // ID3D11Device1 QI는 CreateDeviceContextState 전용 — 사용 즉시 Drop(Release)해도
        // 충분하다(Create* 계열은 베이스 ID3D11Device로 그대로 호출 가능).
        let device1 = {
            let mut raw: *mut ID3D11Device1 = ptr::null_mut();
            let hr = (*device)
                .QueryInterface(&ID3D11Device1::uuidof(), &mut raw as *mut _ as *mut _);
            if hr < 0 || raw.is_null() {
                warn!(
                    "[video-convert] QI ID3D11Device1 failed (hr=0x{:08x}); video escape unavailable",
                    hr as u32
                );
                return None;
            }
            ComOwned::from_raw(raw)?
        };

        let vs_blob = compile_shader(SHADER_SRC, "vs_main", "vs_5_0")?;
        let vs = {
            let mut raw: *mut ID3D11VertexShader = ptr::null_mut();
            let hr = (*device).CreateVertexShader(
                (*vs_blob).GetBufferPointer(),
                (*vs_blob).GetBufferSize(),
                ptr::null_mut(),
                &mut raw,
            );
            (*(vs_blob as *mut IUnknown)).Release();
            if hr < 0 || raw.is_null() {
                warn!("[video-convert] CreateVertexShader failed (hr=0x{:08x})", hr as u32);
                return None;
            }
            ComOwned::from_raw(raw)?
        };

        let ps_blob = compile_shader(SHADER_SRC, "ps_main", "ps_5_0")?;
        let ps = {
            let mut raw: *mut ID3D11PixelShader = ptr::null_mut();
            let hr = (*device).CreatePixelShader(
                (*ps_blob).GetBufferPointer(),
                (*ps_blob).GetBufferSize(),
                ptr::null_mut(),
                &mut raw,
            );
            (*(ps_blob as *mut IUnknown)).Release();
            if hr < 0 || raw.is_null() {
                warn!("[video-convert] CreatePixelShader failed (hr=0x{:08x})", hr as u32);
                return None;
            }
            ComOwned::from_raw(raw)?
        };

        let sampler = {
            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MipLODBias: 0.0,
                MaxAnisotropy: 1,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                BorderColor: [0.0; 4],
                MinLOD: 0.0,
                MaxLOD: D3D11_FLOAT32_MAX,
            };
            let mut raw: *mut ID3D11SamplerState = ptr::null_mut();
            let hr = (*device).CreateSamplerState(&desc, &mut raw);
            if hr < 0 || raw.is_null() {
                warn!("[video-convert] CreateSamplerState failed (hr=0x{:08x})", hr as u32);
                return None;
            }
            ComOwned::from_raw(raw)?
        };

        let cbuffer = {
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<[[f32; 4]; 5]>() as u32,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE,
                MiscFlags: 0,
                StructureByteStride: 0,
            };
            let mut raw: *mut ID3D11Buffer = ptr::null_mut();
            let hr = (*device).CreateBuffer(&desc, ptr::null(), &mut raw);
            if hr < 0 || raw.is_null() {
                warn!("[video-convert] CreateBuffer(cbuffer) failed (hr=0x{:08x})", hr as u32);
                return None;
            }
            ComOwned::from_raw(raw)?
        };

        let blend_state = {
            let mut rt: D3D11_RENDER_TARGET_BLEND_DESC = std::mem::zeroed();
            rt.BlendEnable = FALSE;
            rt.RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL as u8;
            let mut desc: D3D11_BLEND_DESC = std::mem::zeroed();
            desc.AlphaToCoverageEnable = FALSE;
            desc.IndependentBlendEnable = FALSE;
            desc.RenderTarget[0] = rt;
            let mut raw: *mut ID3D11BlendState = ptr::null_mut();
            let hr = (*device).CreateBlendState(&desc, &mut raw);
            if hr < 0 || raw.is_null() {
                warn!("[video-convert] CreateBlendState failed (hr=0x{:08x})", hr as u32);
                return None;
            }
            ComOwned::from_raw(raw)?
        };

        let raster_state = {
            let desc = D3D11_RASTERIZER_DESC {
                FillMode: D3D11_FILL_SOLID,
                CullMode: D3D11_CULL_NONE,
                FrontCounterClockwise: FALSE,
                DepthBias: 0,
                DepthBiasClamp: 0.0,
                SlopeScaledDepthBias: 0.0,
                DepthClipEnable: TRUE,
                ScissorEnable: FALSE,
                MultisampleEnable: FALSE,
                AntialiasedLineEnable: FALSE,
            };
            let mut raw: *mut ID3D11RasterizerState = ptr::null_mut();
            let hr = (*device).CreateRasterizerState(&desc, &mut raw);
            if hr < 0 || raw.is_null() {
                warn!("[video-convert] CreateRasterizerState failed (hr=0x{:08x})", hr as u32);
                return None;
            }
            ComOwned::from_raw(raw)?
        };

        let context_state = {
            let feature_levels = [D3D_FEATURE_LEVEL_11_0];
            let mut chosen: D3D_FEATURE_LEVEL = 0;
            let mut raw: *mut ID3DDeviceContextState = ptr::null_mut();
            let hr = (*device1.as_ptr()).CreateDeviceContextState(
                0,
                feature_levels.as_ptr(),
                feature_levels.len() as u32,
                D3D11_SDK_VERSION,
                &ID3D11Device::uuidof(),
                &mut chosen,
                &mut raw,
            );
            if hr < 0 || raw.is_null() {
                warn!("[video-convert] CreateDeviceContextState failed (hr=0x{:08x})", hr as u32);
                return None;
            }
            ComOwned::from_raw(raw)?
        };

        Some(VideoConvertPass {
            device,
            vs,
            ps,
            sampler,
            cbuffer,
            blend_state,
            raster_state,
            context_state,
            batch: None,
        })
    }

    /// texture(usize, AddRef 유지 중인 ID3D11Texture2D 핸들)의 SRV를 새로 만든다(캐시 금지 —
    /// 위 구조체 주석 참고: DYNAMIC rename으로 캐시 SRV가 dangling→드라이버 AV). 호출자가
    /// draw 직후 Release한다. Safety: texture는 살아있는 ID3D11Texture2D를 가리켜야 한다(lease 계약).
    unsafe fn create_srv(&self, texture: usize) -> Option<*mut ID3D11ShaderResourceView> {
        let mut raw: *mut ID3D11ShaderResourceView = ptr::null_mut();
        let hr = (*self.device).CreateShaderResourceView(
            texture as *mut ID3D11Texture2D as *mut ID3D11Resource,
            ptr::null(),
            &mut raw,
        );
        if hr < 0 || raw.is_null() {
            warn!("[video-convert] CreateShaderResourceView failed (hr=0x{:08x})", hr as u32);
            return None;
        }
        Some(raw)
    }

    /// 배치를 연다(Task: external present 컨텍스트-상태 스왑 배칭). 우리 격리 상태로 1회
    /// 스왑하고 직전 ANGLE 상태를 보관한 뒤, 배치 동안 변하지 않는 정적 파이프라인 상태
    /// (셰이더/샘플러/블렌드/래스터/토폴로지)를 1회 바인딩한다. 이후 같은 프레임의 convert()
    /// 들은 스왑·정적 바인딩을 생략하고 per-draw 자원(RTV/뷰포트/SRV/cbuffer)만 재바인딩한다
    /// → 프레임당 스왑 2N회(convert마다 in/out)가 2회(begin/end)로 준다.
    ///
    /// ★SAFETY(치명): 배치가 열린 동안 이 스레드에서 ANGLE/GL 호출이 절대 돌면 안 된다 —
    /// 우리 ID3DDeviceContextState가 활성이라 ANGLE의 GL→D3D11 상태 설정이 우리 상태 객체를
    /// 오염시키고 ANGLE 상태 캐시가 어긋난다. 호출측(dcomp_compositor)이 이 불변식을 보장한다
    /// (WR passes-루프 타일 GL '전'에 end_batch — start_compositing에서 닫음).
    ///
    /// Safety: `context`는 이 패스를 생성한 디바이스의 살아있는 즉시 컨텍스트(1)여야 한다.
    pub(crate) unsafe fn begin_batch(&mut self, context: *mut ID3D11DeviceContext1) {
        if self.batch.is_some() {
            // 이미 활성 — 이중 스왑 금지(방어적).
            return;
        }
        // prev_state는 SwapDeviceContextState가 AddRef해 돌려준 직전(ANGLE) 상태.
        // end_batch에서 복원 후 Release한다.
        let mut prev_state: *mut ID3DDeviceContextState = ptr::null_mut();
        (*context).SwapDeviceContextState(self.context_state.as_ptr(), &mut prev_state);
        self.bind_static(context);
        self.batch = Some(prev_state);
    }

    /// 배치를 닫는다(begin_batch와 짝). 직전 ANGLE 상태로 복원하고 참조를 정리한다. 배치가
    /// 열려 있지 않으면 no-op(멱등). ★이 호출 이후 비로소 ANGLE/GL 호출이 안전하다.
    ///
    /// Safety: `context`는 begin_batch에 넘긴 바로 그 즉시 컨텍스트(1)여야 한다.
    pub(crate) unsafe fn end_batch(&mut self, context: *mut ID3D11DeviceContext1) {
        let Some(prev_state) = self.batch.take() else {
            return;
        };
        // discarded는 우리 context_state를 다시 AddRef해 돌려준 것 — 마스터 참조는
        // self.context_state가 이미 갖고 있으므로 이 추가 참조를 Release한다.
        let mut discarded: *mut ID3DDeviceContextState = ptr::null_mut();
        (*context).SwapDeviceContextState(prev_state, &mut discarded);
        if !discarded.is_null() {
            (*(discarded as *mut IUnknown)).Release();
        }
        if !prev_state.is_null() {
            (*(prev_state as *mut IUnknown)).Release();
        }
    }

    /// 배치 동안 불변인 정적 파이프라인 상태(토폴로지/VS/PS/샘플러/래스터/블렌드)를 바인딩한다.
    /// begin_batch(배치 1회) 또는 비배치 convert(draw마다)에서 호출. per-draw 자원(SRV/cbuffer/
    /// 뷰포트/RTV)은 여기 없다 — draw_locked가 매 draw 바인딩한다.
    unsafe fn bind_static(&self, context: *mut ID3D11DeviceContext1) {
        let sampler_ptr = self.sampler.as_ptr();
        (*context).IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        (*context).VSSetShader(self.vs.as_ptr(), ptr::null(), 0);
        (*context).PSSetShader(self.ps.as_ptr(), ptr::null(), 0);
        (*context).PSSetSamplers(0, 1, &sampler_ptr);
        (*context).RSSetState(self.raster_state.as_ptr());
        (*context).OMSetBlendState(self.blend_state.as_ptr(), &[1.0, 1.0, 1.0, 1.0], 0xffff_ffff);
    }

    /// lease의 plane들을 rtv(dst_size)로 스케일+색변환 1-draw.
    ///
    /// 배치 활성(begin_batch 후)이면 상태 스왑을 생략하고 per-draw 자원만 재바인딩한다.
    /// 비활성(유닛 테스트/단독 호출)이면 오늘과 동일하게 자체적으로 SwapDeviceContextState로
    /// ANGLE 상태를 격리(draw 전 스왑, 후 복원)한다 — 호출 전후 ANGLE/WR 상태 불변.
    ///
    /// Safety: `context`는 이 패스를 생성한 디바이스의 살아있는 즉시 컨텍스트(1)여야
    /// 하고, `rtv`는 dst_width x dst_height 크기의 살아있는 렌더 타겟 뷰여야 한다.
    pub(crate) unsafe fn convert(
        &mut self,
        context: *mut ID3D11DeviceContext1,
        lease: &VideoFrameLease,
        rtv: *mut ID3D11RenderTargetView,
        dst_width: u32,
        dst_height: u32,
    ) -> bool {
        if self.batch.is_some() {
            // 배치 활성: begin_batch가 이미 우리 상태 활성화 + 정적 상태 바인딩을 마쳤다.
            // 스왑·정적 바인딩 생략, per-draw 자원만.
            return self.draw_locked(context, lease, rtv, dst_width, dst_height, false);
        }

        // 비배치: 우리 격리 상태로 스왑 — prev_state는 SwapDeviceContextState가 AddRef해
        // 돌려준 ANGLE(또는 기본) 상태. 참조 카운트 계약: 아래에서 반드시 한 번 Release한다.
        let mut prev_state: *mut ID3DDeviceContextState = ptr::null_mut();
        (*context).SwapDeviceContextState(self.context_state.as_ptr(), &mut prev_state);

        let ok = self.draw_locked(context, lease, rtv, dst_width, dst_height, true);

        // ANGLE 상태로 복원. discarded는 우리가 방금까지 쓰던 context_state를 다시
        // AddRef해 돌려준 것 — self.context_state가 이미 마스터 참조를 갖고 있으므로
        // 이 추가 참조도 반드시 Release한다.
        let mut discarded: *mut ID3DDeviceContextState = ptr::null_mut();
        (*context).SwapDeviceContextState(prev_state, &mut discarded);
        if !discarded.is_null() {
            (*(discarded as *mut IUnknown)).Release();
        }
        if !prev_state.is_null() {
            (*(prev_state as *mut IUnknown)).Release();
        }
        ok
    }

    /// convert()의 실제 draw 본문(우리 컨텍스트 상태가 활성인 동안에만 호출되어야 함).
    /// `bind_static_state`=true면 정적 파이프라인 상태도 바인딩한다(비배치 경로 — 매 draw).
    /// false면 정적 상태는 begin_batch가 이미 바인딩했다고 가정하고 per-draw 자원만 바인딩한다.
    unsafe fn draw_locked(
        &mut self,
        context: *mut ID3D11DeviceContext1,
        lease: &VideoFrameLease,
        rtv: *mut ID3D11RenderTargetView,
        dst_width: u32,
        dst_height: u32,
        bind_static_state: bool,
    ) -> bool {
        let params = yuv_rgb_params(lease.format, lease.color_space, lease.color_range);
        let cbuf_data = params.to_cbuffer();

        let mut mapped: D3D11_MAPPED_SUBRESOURCE = std::mem::zeroed();
        let hr = (*context).Map(
            self.cbuffer.as_ptr() as *mut ID3D11Resource,
            0,
            D3D11_MAP_WRITE_DISCARD,
            0,
            &mut mapped,
        );
        if hr < 0 {
            warn!("[video-convert] cbuffer Map failed (hr=0x{:08x})", hr as u32);
            return false;
        }
        ptr::copy_nonoverlapping(
            cbuf_data.as_ptr() as *const u8,
            mapped.pData as *mut u8,
            std::mem::size_of_val(&cbuf_data),
        );
        (*context).Unmap(self.cbuffer.as_ptr() as *mut ID3D11Resource, 0);

        // plane SRV는 이 draw 전용으로 새로 만들고(캐시 금지 — 구조체 주석의 DYNAMIC rename
        // dangling 참고) draw 뒤 unbind + Release한다. 실패 시 그때까지 만든 것만 정리한다.
        let plane_count = lease.plane_count.min(3);
        let mut srvs: [*mut ID3D11ShaderResourceView; 3] = [ptr::null_mut(); 3];
        for (i, slot) in srvs.iter_mut().enumerate().take(plane_count) {
            let Some(plane) = lease.planes[i] else {
                warn!(
                    "[video-convert] plane {} missing (plane_count={})",
                    i, lease.plane_count
                );
                release_srvs(&srvs);
                return false;
            };
            let Some(srv) = self.create_srv(plane.texture) else {
                release_srvs(&srvs);
                return false;
            };
            *slot = srv;
        }

        let cbuf_ptr = self.cbuffer.as_ptr();

        // 정적 파이프라인 상태(토폴로지/셰이더/샘플러/래스터/블렌드)는 비배치 경로에서만 매 draw
        // 바인딩한다. 배치 경로는 begin_batch가 1회 바인딩했으므로 생략(never varies).
        if bind_static_state {
            self.bind_static(context);
        }
        // per-draw 자원(SRV/cbuffer/뷰포트/RTV)은 배치·비배치 공통으로 매 convert 재바인딩한다.
        (*context).PSSetShaderResources(0, srvs.len() as u32, srvs.as_ptr());
        (*context).PSSetConstantBuffers(0, 1, &cbuf_ptr);
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: dst_width as f32,
            Height: dst_height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        (*context).RSSetViewports(1, &viewport);
        (*context).OMSetRenderTargets(1, &rtv, ptr::null_mut());
        (*context).Draw(3, 0);

        // draw 제출 후 SRV를 unbind하고 Release한다. Draw가 참조한 SRV는 D3D11 런타임이
        // GPU 완료까지 실제 파기를 지연하므로(deferred destruction) 즉시 Release가 안전하다.
        let nulls: [*mut ID3D11ShaderResourceView; 3] = [ptr::null_mut(); 3];
        (*context).PSSetShaderResources(0, srvs.len() as u32, nulls.as_ptr());
        release_srvs(&srvs);
        true
    }
}

/// cbuffer 파라미터(Step 3 상수표 기반). `to_cbuffer()`가 HLSL cbuffer 레이아웃(5×float4,
/// mat_r/mat_g/mat_b/offs/misc 순서)으로 패킹한다.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ConvertParams {
    pub y_coef: f32,
    pub rv: f32,
    pub gu: f32,
    pub gv: f32,
    pub bu: f32,
    pub y_off: f32,
    pub rescale: f32,
    pub interleaved_uv: bool,
}

impl ConvertParams {
    pub(crate) fn to_cbuffer(&self) -> [[f32; 4]; 5] {
        [
            [self.y_coef, 0.0, self.rv, 0.0],                                        // mat_r
            [self.y_coef, self.gu, self.gv, 0.0],                                    // mat_g
            [self.y_coef, self.bu, 0.0, 0.0],                                        // mat_b
            [self.y_off, 0.5, 0.5, 0.0],                                             // offs
            [self.rescale, if self.interleaved_uv { 1.0 } else { 0.0 }, 0.0, 0.0],   // misc
        ]
    }
}

/// space/range별 YUV->RGB 행렬 계수 + rescale(포맷별 비트심도 정규화). WR yuv.glsl /
/// A-dyn ColorDepth 페어링과 동일 의미론(Step 3 상수표).
pub(crate) fn yuv_rgb_params(
    format: VideoLeaseFormat,
    space: VideoLeaseColorSpace,
    range: VideoLeaseColorRange,
) -> ConvertParams {
    let (y_coef, rv, gu, gv, bu, y_off) = match (space, range) {
        (VideoLeaseColorSpace::Rec601, VideoLeaseColorRange::Limited) => {
            (1.16438, 1.59603, -0.39176, -0.81297, 2.01723, 16.0 / 255.0)
        },
        (VideoLeaseColorSpace::Rec709, VideoLeaseColorRange::Limited) => {
            (1.16438, 1.79274, -0.21325, -0.53291, 2.11240, 16.0 / 255.0)
        },
        (VideoLeaseColorSpace::Rec2020, VideoLeaseColorRange::Limited) => {
            (1.16438, 1.67867, -0.18733, -0.65042, 2.14177, 16.0 / 255.0)
        },
        (VideoLeaseColorSpace::Rec601, VideoLeaseColorRange::Full) => {
            (1.0, 1.402, -0.34414, -0.71414, 1.772, 0.0)
        },
        (VideoLeaseColorSpace::Rec709, VideoLeaseColorRange::Full) => {
            (1.0, 1.5748, -0.18733, -0.46813, 1.8556, 0.0)
        },
        (VideoLeaseColorSpace::Rec2020, VideoLeaseColorRange::Full) => {
            (1.0, 1.4746, -0.16455, -0.57135, 1.8814, 0.0)
        },
    };
    let rescale: f32 = match format {
        VideoLeaseFormat::I420 | VideoLeaseFormat::Nv12 => 1.0,
        VideoLeaseFormat::I420_10 => 65535.0 / 1023.0,
        VideoLeaseFormat::P010 => 65535.0 / 65472.0,
    };
    let interleaved_uv = matches!(format, VideoLeaseFormat::Nv12 | VideoLeaseFormat::P010);
    ConvertParams { y_coef, rv, gu, gv, bu, y_off, rescale, interleaved_uv }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paint_api::VideoLeasePlane;
    use winapi::shared::dxgiformat::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R8G8_UNORM,
        DXGI_FORMAT_R8_UNORM,
    };
    use winapi::shared::dxgitype::DXGI_SAMPLE_DESC;
    use winapi::um::d3d11::{
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
        D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice,
        ID3D11DeviceContext,
    };
    use winapi::um::d3dcommon::D3D_DRIVER_TYPE_WARP;

    // WARP 디바이스 생성 → DYNAMIC plane 텍스처 생성/기록 → convert → STAGING readback.
    // GPU 무관(CI-safe) — Microsoft의 순수 소프트웨어 래스터라이저를 사용한다.

    fn warp_device() -> (*mut ID3D11Device, *mut ID3D11DeviceContext1) {
        unsafe {
            let mut device: *mut ID3D11Device = ptr::null_mut();
            let mut context: *mut ID3D11DeviceContext = ptr::null_mut();
            let feature_levels = [D3D_FEATURE_LEVEL_11_0];
            let hr = D3D11CreateDevice(
                ptr::null_mut(),
                D3D_DRIVER_TYPE_WARP,
                ptr::null_mut(),
                0,
                feature_levels.as_ptr(),
                feature_levels.len() as u32,
                D3D11_SDK_VERSION,
                &mut device,
                ptr::null_mut(),
                &mut context,
            );
            assert!(hr >= 0, "WARP D3D11CreateDevice failed (hr=0x{:08x})", hr as u32);
            let mut context1: *mut ID3D11DeviceContext1 = ptr::null_mut();
            let hr = (*context)
                .QueryInterface(&ID3D11DeviceContext1::uuidof(), &mut context1 as *mut _ as *mut _);
            assert!(hr >= 0, "QI ID3D11DeviceContext1 failed (hr=0x{:08x})", hr as u32);
            (*(context as *mut IUnknown)).Release();
            (device, context1)
        }
    }

    /// dev의 즉시 컨텍스트로 DYNAMIC 텍스처를 Map(WRITE_DISCARD)해 raw 바이트를 채운다.
    unsafe fn fill_dynamic(dev: *mut ID3D11Device, tex: *mut ID3D11Texture2D, bytes_per_pixel: usize, w: u32, h: u32, fill: &[u8]) {
        let mut ctx: *mut ID3D11DeviceContext = ptr::null_mut();
        (*dev).GetImmediateContext(&mut ctx);
        let mut mapped: D3D11_MAPPED_SUBRESOURCE = std::mem::zeroed();
        let hr = (*ctx).Map(tex as *mut ID3D11Resource, 0, D3D11_MAP_WRITE_DISCARD, 0, &mut mapped);
        assert!(hr >= 0, "Map failed (hr=0x{:08x})", hr as u32);
        for y in 0..h as usize {
            let row = (mapped.pData as *mut u8).add(y * mapped.RowPitch as usize);
            for x in 0..w as usize {
                let dst = row.add(x * bytes_per_pixel);
                for b in 0..bytes_per_pixel {
                    *dst.add(b) = fill[b];
                }
            }
        }
        (*ctx).Unmap(tex as *mut ID3D11Resource, 0);
        (*(ctx as *mut IUnknown)).Release();
    }

    fn make_plane_r8(dev: *mut ID3D11Device, w: u32, h: u32, fill: u8) -> *mut ID3D11Texture2D {
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_SHADER_RESOURCE,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE,
                MiscFlags: 0,
            };
            let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
            let hr = (*dev).CreateTexture2D(&desc, ptr::null(), &mut tex);
            assert!(hr >= 0, "CreateTexture2D(R8) failed (hr=0x{:08x})", hr as u32);
            fill_dynamic(dev, tex, 1, w, h, &[fill]);
            tex
        }
    }

    fn make_plane_r16(dev: *mut ID3D11Device, w: u32, h: u32, fill: u16) -> *mut ID3D11Texture2D {
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R16_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_SHADER_RESOURCE,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE,
                MiscFlags: 0,
            };
            let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
            let hr = (*dev).CreateTexture2D(&desc, ptr::null(), &mut tex);
            assert!(hr >= 0, "CreateTexture2D(R16) failed (hr=0x{:08x})", hr as u32);
            let le = fill.to_le_bytes();
            fill_dynamic(dev, tex, 2, w, h, &le);
            tex
        }
    }

    fn make_plane_rg8(dev: *mut ID3D11Device, w: u32, h: u32, fill: (u8, u8)) -> *mut ID3D11Texture2D {
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_SHADER_RESOURCE,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE,
                MiscFlags: 0,
            };
            let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
            let hr = (*dev).CreateTexture2D(&desc, ptr::null(), &mut tex);
            assert!(hr >= 0, "CreateTexture2D(RG8) failed (hr=0x{:08x})", hr as u32);
            fill_dynamic(dev, tex, 2, w, h, &[fill.0, fill.1]);
            tex
        }
    }

    /// 변환 결과를 담을 8x8 B8G8R8A8 렌더 타겟(뷰 + 백킹 텍스처).
    fn make_render_target(dev: *mut ID3D11Device, w: u32, h: u32) -> (*mut ID3D11RenderTargetView, *mut ID3D11Texture2D) {
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: winapi::um::d3d11::D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
            let hr = (*dev).CreateTexture2D(&desc, ptr::null(), &mut tex);
            assert!(hr >= 0, "CreateTexture2D(RTV) failed (hr=0x{:08x})", hr as u32);
            let mut rtv: *mut ID3D11RenderTargetView = ptr::null_mut();
            let hr = (*dev).CreateRenderTargetView(tex as *mut ID3D11Resource, ptr::null(), &mut rtv);
            assert!(hr >= 0, "CreateRenderTargetView failed (hr=0x{:08x})", hr as u32);
            (rtv, tex)
        }
    }

    /// rtv_tex의 중앙 픽셀을 STAGING readback으로 읽어 (R,G,B,A) 순서로 반환한다
    /// (백킹 포맷은 BGRA이므로 내부에서 스위즐한다).
    fn readback_center(
        dev: *mut ID3D11Device,
        ctx: *mut ID3D11DeviceContext1,
        rtv_tex: *mut ID3D11Texture2D,
        w: u32,
        h: u32,
    ) -> [u8; 4] {
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ,
                MiscFlags: 0,
            };
            let mut staging: *mut ID3D11Texture2D = ptr::null_mut();
            let hr = (*dev).CreateTexture2D(&desc, ptr::null(), &mut staging);
            assert!(hr >= 0, "CreateTexture2D(staging) failed (hr=0x{:08x})", hr as u32);
            (*ctx).CopyResource(staging as *mut ID3D11Resource, rtv_tex as *mut ID3D11Resource);
            let mut mapped: D3D11_MAPPED_SUBRESOURCE = std::mem::zeroed();
            let hr = (*ctx).Map(staging as *mut ID3D11Resource, 0, D3D11_MAP_READ, 0, &mut mapped);
            assert!(hr >= 0, "Map(staging) failed (hr=0x{:08x})", hr as u32);
            let cx = (w / 2) as usize;
            let cy = (h / 2) as usize;
            let row = (mapped.pData as *const u8).add(cy * mapped.RowPitch as usize);
            let px = row.add(cx * 4);
            let (b, g, r, a) = (*px, *px.add(1), *px.add(2), *px.add(3));
            (*ctx).Unmap(staging as *mut ID3D11Resource, 0);
            (*(staging as *mut IUnknown)).Release();
            [r, g, b, a]
        }
    }

    fn assert_close(actual: [u8; 4], expected_rgb: [u8; 3], tol: i32) {
        for i in 0..3 {
            let diff = actual[i] as i32 - expected_rgb[i] as i32;
            assert!(
                diff.abs() <= tol,
                "channel {} expected {} got {} (full={:?})",
                i,
                expected_rgb[i],
                actual[i],
                actual
            );
        }
    }

    fn lease3(
        y: *mut ID3D11Texture2D,
        u: *mut ID3D11Texture2D,
        v: *mut ID3D11Texture2D,
        format: VideoLeaseFormat,
        space: VideoLeaseColorSpace,
        range: VideoLeaseColorRange,
    ) -> VideoFrameLease {
        VideoFrameLease {
            ring_id: 0,
            planes: [
                Some(VideoLeasePlane { texture: y as usize, width: 4, height: 4 }),
                Some(VideoLeasePlane { texture: u as usize, width: 4, height: 4 }),
                Some(VideoLeasePlane { texture: v as usize, width: 4, height: 4 }),
            ],
            plane_count: 3,
            format,
            color_space: space,
            color_range: range,
            frame_seq: 1,
        }
    }

    #[test]
    fn convert_i420_bt709_limited_white_and_black() {
        let (dev, ctx1) = warp_device();
        let mut pass = unsafe { VideoConvertPass::new(dev) }.expect("VideoConvertPass::new failed on WARP");
        let (rtv, rtv_tex) = make_render_target(dev, 8, 8);

        // White: Y=235, U=V=128 (BT.709 limited).
        let y = make_plane_r8(dev, 4, 4, 235);
        let u = make_plane_r8(dev, 4, 4, 128);
        let v = make_plane_r8(dev, 4, 4, 128);
        let lease = lease3(y, u, v, VideoLeaseFormat::I420, VideoLeaseColorSpace::Rec709, VideoLeaseColorRange::Limited);
        let ok = unsafe { pass.convert(ctx1, &lease, rtv, 8, 8) };
        assert!(ok);
        let px = readback_center(dev, ctx1, rtv_tex, 8, 8);
        assert_close(px, [255, 255, 255], 3);

        // Black: Y=16, U=V=128 (neutral chroma).
        let y_black = make_plane_r8(dev, 4, 4, 16);
        let lease_black = lease3(y_black, u, v, VideoLeaseFormat::I420, VideoLeaseColorSpace::Rec709, VideoLeaseColorRange::Limited);
        let ok = unsafe { pass.convert(ctx1, &lease_black, rtv, 8, 8) };
        assert!(ok);
        let px = readback_center(dev, ctx1, rtv_tex, 8, 8);
        assert_close(px, [0, 0, 0], 3);
    }

    #[test]
    fn convert_i420_bt601_limited_red() {
        let (dev, ctx1) = warp_device();
        let mut pass = unsafe { VideoConvertPass::new(dev) }.expect("VideoConvertPass::new failed on WARP");
        let (rtv, rtv_tex) = make_render_target(dev, 8, 8);

        let y = make_plane_r8(dev, 4, 4, 81);
        let u = make_plane_r8(dev, 4, 4, 90);
        let v = make_plane_r8(dev, 4, 4, 240);
        let lease = lease3(y, u, v, VideoLeaseFormat::I420, VideoLeaseColorSpace::Rec601, VideoLeaseColorRange::Limited);
        let ok = unsafe { pass.convert(ctx1, &lease, rtv, 8, 8) };
        assert!(ok);
        let px = readback_center(dev, ctx1, rtv_tex, 8, 8);
        assert_close(px, [255, 0, 0], 3);
    }

    #[test]
    fn convert_nv12_interleaved_uv() {
        let (dev, ctx1) = warp_device();
        let mut pass = unsafe { VideoConvertPass::new(dev) }.expect("VideoConvertPass::new failed on WARP");
        let (rtv, rtv_tex) = make_render_target(dev, 8, 8);

        let y = make_plane_r8(dev, 4, 4, 81);
        let uv = make_plane_rg8(dev, 4, 4, (90, 240));
        let lease = VideoFrameLease {
            ring_id: 0,
            planes: [
                Some(VideoLeasePlane { texture: y as usize, width: 4, height: 4 }),
                Some(VideoLeasePlane { texture: uv as usize, width: 4, height: 4 }),
                None,
            ],
            plane_count: 2,
            format: VideoLeaseFormat::Nv12,
            color_space: VideoLeaseColorSpace::Rec601,
            color_range: VideoLeaseColorRange::Limited,
            frame_seq: 1,
        };
        let ok = unsafe { pass.convert(ctx1, &lease, rtv, 8, 8) };
        assert!(ok);
        let px = readback_center(dev, ctx1, rtv_tex, 8, 8);
        assert_close(px, [255, 0, 0], 3);
    }

    #[test]
    fn convert_i420_10_rescale_white() {
        let (dev, ctx1) = warp_device();
        let mut pass = unsafe { VideoConvertPass::new(dev) }.expect("VideoConvertPass::new failed on WARP");
        let (rtv, rtv_tex) = make_render_target(dev, 8, 8);

        // R16 raw Y=940, U=V=512 (10-bit limited white); rescale=65535/1023 corrects the
        // R16_UNORM normalization (/65535) back to a 10-bit fraction (/1023).
        let y = make_plane_r16(dev, 4, 4, 940);
        let u = make_plane_r16(dev, 4, 4, 512);
        let v = make_plane_r16(dev, 4, 4, 512);
        let lease = lease3(y, u, v, VideoLeaseFormat::I420_10, VideoLeaseColorSpace::Rec709, VideoLeaseColorRange::Limited);
        let ok = unsafe { pass.convert(ctx1, &lease, rtv, 8, 8) };
        assert!(ok);
        let px = readback_center(dev, ctx1, rtv_tex, 8, 8);
        assert_close(px, [255, 255, 255], 3);
    }

    #[test]
    fn params_matrix_selection() {
        let p = yuv_rgb_params(VideoLeaseFormat::I420, VideoLeaseColorSpace::Rec709, VideoLeaseColorRange::Limited);
        assert!((p.rv - 1.79274).abs() < 1e-4);
        assert!((p.rescale - 1.0).abs() < 1e-6);
        let p10 = yuv_rgb_params(VideoLeaseFormat::I420_10, VideoLeaseColorSpace::Rec709, VideoLeaseColorRange::Limited);
        assert!((p10.rescale - 64.0616).abs() < 1e-3);
    }

    // 배치 경로(begin_batch → convert×N → end_batch)의 결과 픽셀이 비배치 경로와 동일한지,
    // 그리고 end_batch가 상태를 복원해 이후 비배치 convert가 여전히 정상인지 검증한다.
    #[test]
    fn convert_batched_matches_unbatched() {
        let (dev, ctx1) = warp_device();
        let mut pass = unsafe { VideoConvertPass::new(dev) }.expect("VideoConvertPass::new failed on WARP");
        let (rtv, rtv_tex) = make_render_target(dev, 8, 8);

        // White(BT.709 limited): Y=235, U=V=128.
        let yw = make_plane_r8(dev, 4, 4, 235);
        let uw = make_plane_r8(dev, 4, 4, 128);
        let vw = make_plane_r8(dev, 4, 4, 128);
        let white = lease3(yw, uw, vw, VideoLeaseFormat::I420, VideoLeaseColorSpace::Rec709, VideoLeaseColorRange::Limited);
        // Red(BT.601 limited): Y=81, U=90, V=240.
        let yr = make_plane_r8(dev, 4, 4, 81);
        let ur = make_plane_r8(dev, 4, 4, 90);
        let vr = make_plane_r8(dev, 4, 4, 240);
        let red = lease3(yr, ur, vr, VideoLeaseFormat::I420, VideoLeaseColorSpace::Rec601, VideoLeaseColorRange::Limited);

        // 배치 오픈 후 같은 배치에서 두 번 convert(두 번째는 정적 상태 재바인딩 없이).
        unsafe { pass.begin_batch(ctx1) };

        let ok = unsafe { pass.convert(ctx1, &white, rtv, 8, 8) };
        assert!(ok);
        assert_close(readback_center(dev, ctx1, rtv_tex, 8, 8), [255, 255, 255], 3);

        let ok = unsafe { pass.convert(ctx1, &red, rtv, 8, 8) };
        assert!(ok);
        assert_close(readback_center(dev, ctx1, rtv_tex, 8, 8), [255, 0, 0], 3);

        unsafe { pass.end_batch(ctx1) };

        // 배치 종료 후 비배치 convert도 정상(상태 복원 확인).
        let ok = unsafe { pass.convert(ctx1, &white, rtv, 8, 8) };
        assert!(ok);
        assert_close(readback_center(dev, ctx1, rtv_tex, 8, 8), [255, 255, 255], 3);
    }
}
