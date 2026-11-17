/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 벽 GPU 프로브 (2026-11-17, `WALLGPU*` 줄) -- 어느 페이지 요소가 GPU 를 늦추는가, 그리고
//! Commit 이 GPU 완료를 기다리는가.
//!
//! ## 왜
//!
//! 벽 런 12(log_rotation_opacity_judder_01/12)에서 긴 DComp Commit(4~35 ms)은 디바이스마다 따로,
//! 보드 전환 때만 났다. DWM 격자에 모이지 않았고 넷이 함께 끝나지도 않았다. 그것이 GPU 완료
//! 대기인지 DWM 쪽인지는 집계 수치로 갈리지 않았다. 사용자 판단(2026-11-17): GPU 완료 대기 쪽이
//! 더 의심된다 -- ticker 단독 judder 도 결국 cache 설정 문제였다(7ff4b74aa49). 이 페이지가 한
//! 순간에 그리는 것은 많아야 이미지 하나와 FHD 30fps 비디오 하나뿐이니, 어느 요소가 GPU 를
//! 늦추는지를 본다.
//!
//! ## 두 계측
//!
//! 1. 시간 프로브(`gfx_wall_gpu_time_probe`, 런처 `-GpuTimeProbe`). WebRender 가 GPU 일을 나누는
//!    자리(`WallGpuHook`: 배치 종류마다의 `start_timer` 와 우리가 더한 `W_*` 표지)마다 미리 만든
//!    D3D11 TIMESTAMP query 를 `End` 로 꽂는다. 이름표가 곧 요소다:
//!    - `B_Image` = 이미지(`<img>`), `B_YuvImage` = 비디오(NV12), `B_Solid` = 검은 상자와 배경
//!    - `W_TileSetup` = picture-cache 타일 setup(지우기, 깊이), `target init` = 오프스크린 타깃
//!    - `W_Bind`/`W_Unbind` = DComp BeginDraw/EndDraw 와 pbuffer 생성·해제
//!    - `W_ExtLock` = 외부 이미지 잠금: 비디오 링 consume(평면 Unmap/Map = 프레임마다의 NV12
//!      업로드, 링 초기화), EGLImage 감싸기, WebGL keyed mutex. `B_YuvImage` 는 그리기 배치뿐이다
//!      (2026-11-17 리뷰로 갈랐다 -- 처음엔 이것이 `W_GpuCache` 에 잡혔다).
//!    - `W_ExtUnlock` = 외부 이미지 풀기(WebGL ReleaseSync), `W_BeginFrame` = DComp begin_frame
//!    - `W_TextureUpload` = 이미지·글리프 업로드, `W_GpuCache`, `data init` = 프레임 데이터,
//!      `W_GpuCacheClear` = GPU 캐시 재생성(드묾)
//!    - `frame` = 프레임 시작부터 첫 표지까지, `W_Truncated` = 표지 상한(160)을 넘긴 나머지
//!
//!    구간 하나의 GPU 시간은 그 표지의 타임스탬프부터 다음 표지의 타임스탬프까지다(WR 타이머와
//!    같은 평평한 의미). CPU 시간(QPC)도 같은 표지에서 함께 잰다.
//!
//! 2. 완료 프로브(`gfx_wall_gpu_fence_probe`, 런처 `-GpuFenceProbe`). DComp `end_frame` 에서
//!    Commit 임계구역 직전에 `ID3D11DeviceContext4::Signal(fence, v)` 를 하고, 전용 스레드
//!    (`WallGpuFence`)가 `SetEventOnCompletion` 으로 깨어 그 값이 끝난 순간의 QPC 를 찍는다.
//!    그 시각을 같은 디바이스의 Commit 시작·끝과 비교한다.
//!
//! ## 교란을 줄인 방법과 남은 교란
//!
//! - WR 자체 GPU 타이머(`GPU_TIME_QUERIES`)는 쓰지 않는다. ANGLE 은 GL 타이머 하나마다 D3D11
//!   query 셋을 새로 만들고(`CreateQuery`) 끝낼 때 flush 하며, WR 은 결과를 spin 으로 기다린다
//!   (`query_gl.rs::build_samples`, ANGLE `Query11.cpp`). 재려는 것을 바꾼다.
//! - 여기서는 query 를 켤 때 다 만들어 두고(슬롯 32 x 표지 160), 프레임 안에서는 `End` 만 한다.
//!   `End` 는 flush 하지 않는다. 결과는 다음 프레임들의 시작에서 `GetData(DONOTFLUSH)` 로 거둔다 --
//!   아직이면 기다리지 않고 다음에 다시 본다. 슬롯이 다 밀리면 그 프레임은 재지 않고 `skipped` 로
//!   센다. 그 원인은 둘이다: GPU 가 32 프레임 넘게 밀렸거나, 명령이 제출되지 않고 남아 있다
//!   (정지 화면처럼 프레임마다 명령이 아주 적으면 UMD 가 버퍼를 오래 들고 있다 -- 2026-11-17 로컬
//!   스모크 g4: 슬롯 8 로는 정지 페이지에서 601 중 399 를 못 쟀다. 펜스 Signal 이 있으면 0 이다).
//! - 이 거두기(`probe_us`)는 painter 스레드의 ANGLE 락 안에서 돈다 -- `GetData` 가 표지마다 한 번
//!   이다. 그 비용은 프레임마다 잰다. 링 기록은 `wall_span` 과 같다(원자 저장뿐).
//! - `Signal` 은 제출(flush)을 일으킨다(2026-11-17 로컬 스모크로 확인: 펜스를 켜면 매 프레임
//!   거둬지고 Signal 호출이 p50 약 95 us, 끄면 정지 페이지에서 프레임 명령이 수십 프레임 동안
//!   제출되지 않았다). 즉 평소에는 프레임 끝에 우리 디바이스를 제출하는 주체가 없고, 완료 프로브가
//!   그 제출을 하나 더한다. 그래서 완료 프로브는 교란이다 -- 벽에서는 시간 프로브만 켠 갈래, 펜스를
//!   더한 갈래, 펜스 대신 `-DcompAlwaysFlush`(같은 자리의 glFlush)를 더한 갈래를 견준다. 틱 머리
//!   Commit 에서는 제출을 한 주기 앞당기므로 판정에 쓰지 말 것(런처가 경고한다).
//! - 남는 해석의 함정:
//!   - GPU 타임스탬프 간격에는 그 사이에 같은 GPU 에서 돈 다른 컨텍스트(DWM 합성)의 시간도 들어간다.
//!   - 표지 사이의 명령이 아직 제출되지 않아 GPU 가 놀고 있던 시간도 들어갈 수 있다(UMD 가
//!     버퍼를 반쯤 제출한 경우). `between_us`(프레임 끝과 다음 프레임 시작 사이)와 함께 읽는다.
//!
//! ## 줄
//!
//! 10 초마다(`wall_span` 리포터):
//! - `WALLGPU` -- painter 별 프레임 GPU 시간, 프레임 사이, CPU 시간, 거두기 비용
//! - `WALLGPUTAG` -- painter 별 이름표마다의 GPU 합(비율)과 p95, CPU 합
//! - `WALLGPULONG` -- 그 painter 의 디바이스 Commit 이 길었던(4 ms 이상) 프레임과 아닌 프레임의 비교
//! - `WALLGPUDONE` -- 디바이스별: Signal 호출 비용, GPU 완료까지, 긴 Commit 의 완료 위치
//! - `WALLGPUHEAVY` -- 무거운 프레임 하나하나(ms 시각, 이름표 상위 다섯), 창당 상한
//!
//! 초마다: `WALLGPUSEC` -- painter 별 그 초의 GPU 합과 상위 이름표(전환 타임라인용).
//!
//! ## 2026-11-17 보강 (런 13 뒤, 사용자 지시 "1번 계측 보강")
//!
//! 런 13 에서 긴 Commit 은 그 프레임 GPU 일의 첫 제출(flush)이었고, 제출을 앞당겨도 늦은 렌더와
//! 육안은 같았다. Signal 뒤 GPU 완료는 p95 약 10 ms 로 우리 일(p95 약 3 ms)보다 약 7 ms 늦었다.
//! 남은 질문은 셋이다: 제출이 무엇에 막히나, 그 7 ms 는 무엇인가, bind 마다 새로 만드는 pbuffer
//! (DEPTH|STENCIL 구성)가 비용을 만드나. 그래서 더한 것:
//! - Signal 직전 펜스 완료 값(사용자 모드 읽기 하나) -- 그 순간 GPU 에서 아직 안 끝난 이전 프레임 수.
//!   `WALLGPUQUEUE` 가 그 수별로 Signal 막힘을 가른다. 이전 프레임이 밀려 있을 때만 막히면 큐 한도다.
//! - DComp bind/unbind 단계별 CPU 시간(`WALLBIND`): BeginDraw, pbuffer 생성, make current, 투명
//!   지우기, EndDraw, pbuffer 파괴. BeginDraw 텍스처 크기와 "직전 bind 와 같은 텍스처인가".
//! - `end_frame` 의 glFlush 시간(`WALLFLUSH`, `-DcompAlwaysFlush` 갈래).
//! - 어댑터 VRAM 사용량·예산(`WALLVRAM`, 리포터 스레드가 250 ms 마다 `QueryVideoMemoryInfo`).
//! - 늦은 프레임마다 한 줄(`WALLGPUFRAME`): 렌더, 우리 GPU 구간, bind 단계, flush, Signal 과 그
//!   순간의 밀린 프레임 수, Signal 뒤 GPU 완료, Commit, 상위 이름표. 리포터가 링 기록을 잇는다.

// D3D11 query·펜스 FFI 가 있어 unsafe 를 허용한다(dcomp_compositor.rs 와 같은 취지).
#![allow(unsafe_code)]

use std::cell::Cell;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use log::warn;
use webrender::WallGpuHook;
use winapi::shared::guiddef::GUID;
use winapi::shared::winerror::S_OK;
use winapi::um::d3d11::{
    D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_QUERY_DATA_TIMESTAMP_DISJOINT, D3D11_QUERY_DESC,
    D3D11_QUERY_TIMESTAMP, D3D11_QUERY_TIMESTAMP_DISJOINT, ID3D11Asynchronous, ID3D11Device,
    ID3D11DeviceContext, ID3D11Query,
};

use crate::wall_span::{self, Event, Kind, MAX_SLOTS};

/// 이름표. 앞쪽은 이 페이지에서 나올 것들, 뒤쪽은 WR 태그 전부. 모르는 것은 `other`.
pub(crate) const LABELS: [&str; 43] = [
    "frame",
    "W_TextureUpload",
    "W_NativeSurfaces",
    "W_BeginFrame",
    "W_GpuCacheClear",
    "W_ExtLock",
    "W_GpuCache",
    "data init",
    "W_TileSetup",
    "target init",
    "W_Bind",
    "W_Unbind",
    "B_Solid",
    "B_Image",
    "B_YuvImage",
    "B_Opacity",
    "B_Blend",
    "B_MixBlend",
    "B_LinearGradient",
    "TextRun",
    "Primitive",
    "Primitive (indirect)",
    "Mask (indirect)",
    "SplitComposite",
    "C_Clip",
    "C_Border",
    "C_LineDecoration",
    "C_FastLinearGradient",
    "C_LinearGradient",
    "C_Gradient",
    "C_RadialGradient",
    "C_ConicGradient",
    "Blur",
    "Blit",
    "Scale",
    "SvgFilter",
    "SvgFilterNodes",
    "Composite",
    "W_CompositeNative",
    "W_ExtSurfaces",
    "W_ExtUnlock",
    "W_Truncated",
    "other",
];
const LABEL_FRAME: u8 = 0;
const LABEL_OTHER: u8 = (LABELS.len() - 1) as u8;
/// 표지 상한을 넘긴 나머지(2026-11-17 리뷰). 그 프레임의 나머지가 마지막 이름표에 얹히지 않게 한다.
const LABEL_TRUNCATED: u8 = (LABELS.len() - 2) as u8;

pub(crate) fn label_index(label: &str) -> u8 {
    LABELS
        .iter()
        .position(|known| *known == label)
        .map_or(LABEL_OTHER, |index| index as u8)
}

/// `GpuFrame` 의 flag 비트.
const FRAME_DISJOINT: u8 = 1;
const FRAME_TRUNCATED: u8 = 2;
const FRAME_UPDATE: u8 = 4;
const FRAME_SKIPPED: u8 = 8;

/// painter 가 `update()`/`render()` 직전에 세우는 프레임 꼬리표: 하위 60 비트 = 패스(틱),
/// 비트 60 = `update()` 안의 렌더(표출하지 않는 프레임).
pub(crate) type GpuFrameTag = Rc<Cell<u64>>;
pub(crate) const TAG_UPDATE: u64 = 1 << 60;

/// 2026-11-17: 8 에서 32 로 -- 위 모듈 주석의 g4(정지 페이지에서 슬롯이 밀려 2/3 를 못 쟀다).
const SLOTS: usize = 32;
const MARKS: usize = 160;

/// 완료 프로브가 켜져 있나. `end_frame` 이 매 프레임 보므로 pref 를 한 번만 읽는다.
pub(crate) static FENCE_PROBE: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| servo_config::pref!(gfx_wall_gpu_fence_probe));

/// GPU 프로브 중 하나라도 켜져 있나(2026-11-17). bind 단계·flush·VRAM 기록이 이것을 본다.
pub(crate) static ANY_PROBE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    servo_config::pref!(gfx_wall_gpu_fence_probe) || servo_config::pref!(gfx_wall_gpu_time_probe)
});

/// bind/unbind 단계(`Kind::BindStep` 의 flag). 순서는 실제 순서다.
pub(crate) const STEP_BEGIN_DRAW: u8 = 0;
pub(crate) const STEP_PBUFFER_CREATE: u8 = 1;
pub(crate) const STEP_MAKE_CURRENT: u8 = 2;
pub(crate) const STEP_EXPAND_CLEAR: u8 = 3;
pub(crate) const STEP_END_DRAW: u8 = 4;
pub(crate) const STEP_PBUFFER_DESTROY: u8 = 5;
const BIND_STEPS: [&str; 6] = [
    "begin_draw",
    "pbuffer_create",
    "make_current",
    "expand_clear",
    "end_draw",
    "pbuffer_destroy",
];

/// bind 단계 하나를 링에 쓴다. 원자 저장뿐이다.
#[inline]
pub(crate) fn record_bind_step(slot: u8, step: u8, a: u64, b: u64, c: u64) {
    wall_span::record(
        Kind::BindStep,
        slot,
        step,
        wall_span::current_tick(),
        a,
        b,
        c,
    );
}

/// BeginDraw 텍스처가 같은 서피스·같은 타일의 직전 bind 와 같은가(2026-11-17 리뷰: 서피스별로 본다 --
/// 슬라이스 둘이 번갈아 bind 되면 컴포지터 전체의 직전 값과 견주는 것은 늘 "다름" 이다).
pub(crate) const REUSE_UNKNOWN: u64 = 0;
pub(crate) const REUSE_SAME: u64 = 1;
pub(crate) const REUSE_CHANGED: u64 = 2;

/// BeginDraw 텍스처: 비트 40~41 = 재사용(`REUSE_*`), 20~39 = 폭, 0~19 = 높이. 같음은 포인터
/// 비교라, DComp 가 놓은 뒤 같은 주소에 새로 만들면 같음으로 셀 수 있다(과대).
pub(crate) fn pack_texture(width: u32, height: u32, reuse: u64) -> u64 {
    ((reuse & 3) << 40) | (((width as u64) & 0xf_ffff) << 20) | ((height as u64) & 0xf_ffff)
}

struct FrameSlot {
    disjoint: *mut ID3D11Query,
    ts: [*mut ID3D11Query; MARKS],
    label: [u8; MARKS],
    cpu: [u64; MARKS],
    n: usize,
    pending: bool,
    truncated: bool,
    tag: u64,
    frame_no: u64,
    cost_ticks: u64,
}

/// 시간 프로브. WR 렌더러가 갖고(`Renderer::set_wall_gpu_hook`), painter 스레드에서만 불린다 --
/// 그 스레드는 `render_impl` 내내 그 디바이스의 ANGLE 락을 쥐고 있다(`Painter::render`).
pub(crate) struct GpuTimeProbe {
    context: *mut ID3D11DeviceContext,
    slots: Vec<FrameSlot>,
    next: usize,
    current: Option<usize>,
    painter_slot: u8,
    tag: GpuFrameTag,
    frames: u64,
    /// 직전에 거둔 프레임: (번호, 끝 타임스탬프, 주파수).
    prev_end: Option<(u64, u64, u64)>,
}

impl GpuTimeProbe {
    /// query 를 모두 미리 만든다(타이밍 경로 밖). 하나라도 실패하면 만든 것을 놓고 `None`.
    ///
    /// # Safety
    /// `device` 는 살아 있는 ANGLE D3D11 디바이스여야 한다(AddRef 하지 않는다).
    pub(crate) unsafe fn new(
        device: *mut ID3D11Device,
        painter_slot: u8,
        tag: GpuFrameTag,
    ) -> Option<GpuTimeProbe> {
        if device.is_null() {
            return None;
        }
        let mut context: *mut ID3D11DeviceContext = std::ptr::null_mut();
        unsafe { (*device).GetImmediateContext(&mut context) };
        if context.is_null() {
            return None;
        }
        let mut probe = GpuTimeProbe {
            context,
            slots: Vec::with_capacity(SLOTS),
            next: 0,
            current: None,
            painter_slot,
            tag,
            frames: 0,
            prev_end: None,
        };
        let create = |kind| -> Option<*mut ID3D11Query> {
            let desc = D3D11_QUERY_DESC {
                Query: kind,
                MiscFlags: 0,
            };
            let mut query: *mut ID3D11Query = std::ptr::null_mut();
            let hr = unsafe { (*device).CreateQuery(&desc, &mut query) };
            (hr >= 0 && !query.is_null()).then_some(query)
        };
        for _ in 0..SLOTS {
            let Some(disjoint) = create(D3D11_QUERY_TIMESTAMP_DISJOINT) else {
                warn!("WALLGPU: TIMESTAMP_DISJOINT query 를 만들지 못했다 -- 시간 프로브 끔");
                return None;
            };
            let mut slot = FrameSlot {
                disjoint,
                ts: [std::ptr::null_mut(); MARKS],
                label: [0; MARKS],
                cpu: [0; MARKS],
                n: 0,
                pending: false,
                truncated: false,
                tag: 0,
                frame_no: 0,
                cost_ticks: 0,
            };
            for index in 0..MARKS {
                match create(D3D11_QUERY_TIMESTAMP) {
                    Some(query) => slot.ts[index] = query,
                    None => {
                        probe.slots.push(slot);
                        warn!("WALLGPU: TIMESTAMP query 를 만들지 못했다 -- 시간 프로브 끔");
                        return None;
                    },
                }
            }
            probe.slots.push(slot);
        }
        Some(probe)
    }

    #[inline]
    fn end(&self, query: *mut ID3D11Query) {
        // Safety: context 는 AddRef 한 즉시 컨텍스트, query 는 이 디바이스에서 만든 것.
        unsafe { (*self.context).End(query as *mut ID3D11Asynchronous) };
    }

    /// 다 끝난 슬롯을 오래된 것부터 거둔다. 아직인 것을 만나면 멈춘다(한 컨텍스트라 끝나는
    /// 순서가 같다). 기다리지 않는다.
    fn resolve_ready(&mut self) {
        for step in 0..SLOTS {
            let index = (self.next + step) % SLOTS;
            if !self.slots[index].pending {
                continue;
            }
            if !self.try_resolve(index) {
                break;
            }
        }
    }

    fn try_resolve(&mut self, index: usize) -> bool {
        let context = self.context;
        let slot = &self.slots[index];
        let mut disjoint = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT {
            Frequency: 0,
            Disjoint: 0,
        };
        // Safety: 위와 같다. DONOTFLUSH 라 제출을 일으키지 않는다.
        let hr = unsafe {
            (*context).GetData(
                slot.disjoint as *mut ID3D11Asynchronous,
                &mut disjoint as *mut _ as *mut _,
                std::mem::size_of::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>() as u32,
                D3D11_ASYNC_GETDATA_DONOTFLUSH,
            )
        };
        if hr < 0 {
            // 장치 오류 등. 이 슬롯은 버린다.
            self.slots[index].pending = false;
            return true;
        }
        if hr != S_OK {
            return false;
        }
        let n = slot.n;
        let mut ts = [0u64; MARKS];
        for (value, query) in ts.iter_mut().zip(slot.ts.iter()).take(n) {
            let hr = unsafe {
                (*context).GetData(
                    *query as *mut ID3D11Asynchronous,
                    value as *mut u64 as *mut _,
                    8,
                    D3D11_ASYNC_GETDATA_DONOTFLUSH,
                )
            };
            if hr != S_OK {
                return false;
            }
        }
        let slot = &mut self.slots[index];
        slot.pending = false;
        let invalid = disjoint.Disjoint != 0 || disjoint.Frequency == 0 || n < 2;
        let mut flag = 0;
        if invalid {
            flag |= FRAME_DISJOINT;
        }
        if slot.truncated {
            flag |= FRAME_TRUNCATED;
        }
        if slot.tag & TAG_UPDATE != 0 {
            flag |= FRAME_UPDATE;
        }
        let pass = slot.tag & (TAG_UPDATE - 1);
        let to_ns =
            |ticks: u64| (ticks as u128 * 1_000_000_000 / disjoint.Frequency.max(1) as u128) as u64;
        let (span_ns, between_ns) = if invalid {
            (0, u64::MAX)
        } else {
            let span = to_ns(ts[n - 1].saturating_sub(ts[0]));
            let between = match self.prev_end {
                Some((frame_no, end, freq))
                    if frame_no + 1 == slot.frame_no
                        && freq == disjoint.Frequency
                        && ts[0] >= end =>
                {
                    to_ns(ts[0] - end)
                },
                _ => u64::MAX,
            };
            (span, between)
        };
        let cpu_end = slot.cpu[n - 1];
        // 이름표 합을 먼저 구한다 -- Meta 가 뒤따를 이름표 수를 알려야 리포터가 창 경계에서 잘린
        // 묶음을 알아본다(2026-11-17 리뷰).
        let mut gpu = [0u64; LABELS.len()];
        let mut cpu = [0u64; LABELS.len()];
        let mut count = [0u64; LABELS.len()];
        if !invalid {
            for i in 0..n - 1 {
                let label = slot.label[i] as usize;
                gpu[label] += to_ns(ts[i + 1].saturating_sub(ts[i]));
                cpu[label] += slot.cpu[i + 1].saturating_sub(slot.cpu[i]);
                count[label] += 1;
            }
        }
        let labels_to_follow = count.iter().filter(|c| **c > 0).count() as u64;
        wall_span::record(
            Kind::GpuFrame,
            self.painter_slot,
            flag,
            pass,
            span_ns,
            between_ns,
            cpu_end,
        );
        wall_span::record(
            Kind::GpuFrameMeta,
            self.painter_slot,
            0,
            pass,
            wall_span::ticks_to_ns(cpu_end.saturating_sub(slot.cpu[0])),
            wall_span::ticks_to_ns(slot.cost_ticks),
            (labels_to_follow << 32) | n as u64,
        );
        if !invalid {
            for label in 0..LABELS.len() {
                if count[label] == 0 {
                    continue;
                }
                wall_span::record(
                    Kind::GpuLabel,
                    self.painter_slot,
                    0,
                    pass,
                    gpu[label],
                    wall_span::ticks_to_ns(cpu[label]),
                    ((label as u64) << 32) | count[label],
                );
            }
            self.prev_end = Some((slot.frame_no, ts[n - 1], disjoint.Frequency));
        } else {
            self.prev_end = None;
        }
        true
    }
}

impl WallGpuHook for GpuTimeProbe {
    fn frame_begin(&mut self) {
        let Some(cost_start) = wall_span::now() else {
            return;
        };
        self.resolve_ready();
        let index = self.next;
        // 건너뛴 프레임도 번호를 센다(2026-11-17 리뷰) -- 그래야 다음 프레임의 `between` 이 건너뛴
        // 프레임의 GPU 일을 "프레임 사이" 로 잘못 세지 않는다(연속성 검사가 실패해 모름이 된다).
        self.frames += 1;
        if self.slots[index].pending {
            // 32 프레임 전 것을 아직 못 거뒀다(GPU 가 밀렸거나 제출되지 않았다) -- 이 프레임은 재지 않는다.
            self.current = None;
            wall_span::record(
                Kind::GpuFrame,
                self.painter_slot,
                FRAME_SKIPPED,
                self.tag.get() & (TAG_UPDATE - 1),
                0,
                u64::MAX,
                cost_start,
            );
            return;
        }
        self.next = (index + 1) % SLOTS;
        let tag = self.tag.get();
        let frame_no = self.frames;
        let disjoint = self.slots[index].disjoint;
        let first = self.slots[index].ts[0];
        // Safety: 위와 같다.
        unsafe { (*self.context).Begin(disjoint as *mut ID3D11Asynchronous) };
        self.end(first);
        let now = wall_span::now().unwrap_or(cost_start);
        let slot = &mut self.slots[index];
        slot.label[0] = LABEL_FRAME;
        slot.cpu[0] = now;
        slot.n = 1;
        slot.truncated = false;
        slot.tag = tag;
        slot.frame_no = frame_no;
        slot.cost_ticks = now.saturating_sub(cost_start);
        self.current = Some(index);
    }

    fn mark(&mut self, label: &'static str) {
        let Some(index) = self.current else {
            return;
        };
        let n = self.slots[index].n;
        if self.slots[index].truncated {
            return;
        }
        // 마지막 칸은 frame_end 몫이고, 그 앞 칸은 상한을 넘긴 나머지(`W_Truncated`) 몫이다.
        if n >= MARKS - 2 {
            let query = self.slots[index].ts[n];
            self.end(query);
            let now = wall_span::now().unwrap_or(0);
            let slot = &mut self.slots[index];
            slot.label[n] = LABEL_TRUNCATED;
            slot.cpu[n] = now;
            slot.n = n + 1;
            slot.truncated = true;
            return;
        }
        let query = self.slots[index].ts[n];
        self.end(query);
        let now = wall_span::now().unwrap_or(0);
        let slot = &mut self.slots[index];
        slot.label[n] = label_index(label);
        slot.cpu[n] = now;
        slot.n = n + 1;
    }

    fn frame_end(&mut self) {
        let Some(index) = self.current.take() else {
            return;
        };
        let n = self.slots[index].n;
        let query = self.slots[index].ts[n];
        let disjoint = self.slots[index].disjoint;
        self.end(query);
        // Safety: 위와 같다.
        unsafe { (*self.context).End(disjoint as *mut ID3D11Asynchronous) };
        let now = wall_span::now().unwrap_or(0);
        let slot = &mut self.slots[index];
        slot.label[n] = LABEL_OTHER;
        slot.cpu[n] = now;
        slot.n = n + 1;
        slot.pending = true;
    }
}

impl Drop for GpuTimeProbe {
    fn drop(&mut self) {
        // Safety: 렌더러 deinit 안(EGL terminate 전)에서 렌더러와 함께 사라진다.
        unsafe {
            for slot in &self.slots {
                for query in slot.ts.iter().filter(|q| !q.is_null()) {
                    (**query).Release();
                }
                (*slot.disjoint).Release();
            }
            (*self.context).Release();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 완료 프로브 (ID3D11Fence)
// ---------------------------------------------------------------------------------------------

// winapi 0.3.9 에는 D3D11.4 펜스 인터페이스가 없다. vtable 순번을 손으로 세지 않고 winapi 의
// vtable 구조체 크기에서 계산한다(시험이 그 값을 고정한다). 순서는 windows 0.62.2 의
// `ID3D11Device5_Vtbl`/`ID3D11DeviceContext4_Vtbl`/`ID3D11Fence_Vtbl` 와 같다.
const PTR: usize = std::mem::size_of::<usize>();
/// ID3D11Device1 까지 50 칸 + Device2 4 + Device3 11 + Device4 2 + OpenSharedFence 1.
const DEVICE5_CREATE_FENCE: usize =
    std::mem::size_of::<winapi::um::d3d11_1::ID3D11Device1Vtbl>() / PTR + 4 + 11 + 2 + 1;
/// ID3D11DeviceContext1 까지 134 칸 + Context2 10 + Context3 3.
const CONTEXT4_SIGNAL: usize =
    std::mem::size_of::<winapi::um::d3d11_1::ID3D11DeviceContext1Vtbl>() / PTR + 10 + 3;
/// ID3D11DeviceChild 7 칸 + CreateSharedHandle 1.
const FENCE_GET_COMPLETED: usize =
    std::mem::size_of::<winapi::um::d3d11::ID3D11DeviceChildVtbl>() / PTR + 1;
const FENCE_SET_EVENT: usize = FENCE_GET_COMPLETED + 1;

const IID_ID3D11_DEVICE5: GUID = GUID {
    Data1: 0x8ffde202,
    Data2: 0xa0e7,
    Data3: 0x45df,
    Data4: [0x9e, 0x01, 0xe8, 0x37, 0x80, 0x1b, 0x5e, 0xa0],
};
const IID_ID3D11_DEVICE_CONTEXT4: GUID = GUID {
    Data1: 0x917600da,
    Data2: 0xf58c,
    Data3: 0x4c33,
    Data4: [0x98, 0xd8, 0x3e, 0x15, 0xb3, 0x90, 0xfa, 0x24],
};
const IID_ID3D11_FENCE: GUID = GUID {
    Data1: 0xaffde9d1,
    Data2: 0x1df7,
    Data3: 0x4bb7,
    Data4: [0x8a, 0x34, 0x0f, 0x46, 0x25, 0x1d, 0xab, 0x80],
};

type CreateFenceFn =
    unsafe extern "system" fn(*mut c_void, u64, u32, *const GUID, *mut *mut c_void) -> i32;
type SignalFn = unsafe extern "system" fn(*mut c_void, *mut c_void, u64) -> i32;
type GetCompletedFn = unsafe extern "system" fn(*mut c_void) -> u64;
type SetEventFn = unsafe extern "system" fn(*mut c_void, u64, *mut c_void) -> i32;

/// # Safety
/// `object` 는 살아 있는 COM 객체, `index` 는 그 인터페이스의 vtable 칸이어야 한다.
unsafe fn vtable_fn<F: Copy>(object: *mut c_void, index: usize) -> F {
    unsafe {
        let vtable = *(object as *const *const usize);
        std::mem::transmute_copy(&*vtable.add(index))
    }
}

/// # Safety
/// `object` 는 살아 있는 COM 객체.
unsafe fn com_release(object: *mut c_void) {
    if object.is_null() {
        return;
    }
    unsafe { (*(object as *mut winapi::um::unknwnbase::IUnknown)).Release() };
}

/// 디바이스 슬롯마다 waiter 가 보는 칸. 펜스 포인터는 뮤텍스 아래에서만 만진다 -- painter 가
/// 해제할 때 waiter 가 쓰던 중이면 안 된다. 그 뮤텍스는 waiter 와 해제(종료 때 한 번)만 잡는다.
struct FenceEntry {
    fence: Mutex<usize>,
    event: AtomicUsize,
    fresh: AtomicBool,
}

static FENCES: [FenceEntry; MAX_SLOTS] = [const {
    FenceEntry {
        fence: Mutex::new(0),
        event: AtomicUsize::new(0),
        fresh: AtomicBool::new(false),
    }
}; MAX_SLOTS];
static WAITER_STARTED: AtomicBool = AtomicBool::new(false);
static FENCE_REMOVED: AtomicU64 = AtomicU64::new(0);

/// 완료 프로브. DComp 컴포지터가 갖고, painter 스레드에서만 `signal` 한다.
pub(crate) struct GpuFence {
    slot: u8,
    context4: *mut c_void,
    fence: *mut c_void,
    next: u64,
}

impl GpuFence {
    /// 펜스를 만들고 waiter 에 등록한다(타이밍 경로 밖, 첫 `end_frame` 에서 한 번).
    ///
    /// # Safety
    /// `device`/`context` 는 살아 있는 ANGLE D3D11 디바이스와 그 즉시 컨텍스트.
    pub(crate) unsafe fn new(
        device: *mut ID3D11Device,
        context: *mut ID3D11DeviceContext,
        dcomp_device: usize,
    ) -> Option<GpuFence> {
        if device.is_null() || context.is_null() {
            return None;
        }
        let slot = wall_span::device_slot(dcomp_device);
        unsafe {
            let mut device5: *mut c_void = std::ptr::null_mut();
            let hr =
                (*device).QueryInterface(&IID_ID3D11_DEVICE5, &mut device5 as *mut _ as *mut _);
            if hr < 0 || device5.is_null() {
                warn!(
                    "WALLGPU: dev{slot} ID3D11Device5 없음(hr=0x{:08x}) -- 완료 프로브 끔",
                    hr as u32
                );
                return None;
            }
            let mut context4: *mut c_void = std::ptr::null_mut();
            let hr = (*context).QueryInterface(
                &IID_ID3D11_DEVICE_CONTEXT4,
                &mut context4 as *mut _ as *mut _,
            );
            if hr < 0 || context4.is_null() {
                com_release(device5);
                warn!(
                    "WALLGPU: dev{slot} ID3D11DeviceContext4 없음(hr=0x{:08x}) -- 완료 프로브 끔",
                    hr as u32
                );
                return None;
            }
            let create: CreateFenceFn = vtable_fn(device5, DEVICE5_CREATE_FENCE);
            let mut fence: *mut c_void = std::ptr::null_mut();
            let hr = create(device5, 0, 0, &IID_ID3D11_FENCE, &mut fence);
            com_release(device5);
            if hr < 0 || fence.is_null() {
                com_release(context4);
                warn!(
                    "WALLGPU: dev{slot} CreateFence 실패(hr=0x{:08x}) -- 완료 프로브 끔",
                    hr as u32
                );
                return None;
            }
            let entry = &FENCES[slot as usize];
            if entry.event.load(Ordering::Acquire) == 0 {
                // 자동 리셋 이벤트. 프로세스 수명이다(슬롯 넷).
                let event = winapi::um::synchapi::CreateEventW(
                    std::ptr::null_mut(),
                    0,
                    0,
                    std::ptr::null(),
                );
                if event.is_null() {
                    com_release(fence);
                    com_release(context4);
                    warn!("WALLGPU: dev{slot} 이벤트를 만들지 못했다 -- 완료 프로브 끔");
                    return None;
                }
                entry.event.store(event as usize, Ordering::Release);
            }
            *entry.fence.lock().unwrap_or_else(|e| e.into_inner()) = fence as usize;
            entry.fresh.store(true, Ordering::Release);
            start_waiter();
            log::info!("WALLGPU: dev{slot} 완료 프로브 켬 (ID3D11Fence)");
            Some(GpuFence {
                slot,
                context4,
                fence,
                next: 0,
            })
        }
    }

    /// 그 프레임의 마지막 GPU 명령 뒤에 값을 하나 올린다. 링 기록 하나 -- 락·할당 없음.
    pub(crate) fn signal(&mut self) {
        let Some(before) = wall_span::now() else {
            return;
        };
        // 2026-11-17 보강: Signal 직전의 완료 값 -- 그 순간 GPU 에서 아직 안 끝난 이전 프레임 수를
        // 리포터가 구한다(`WALLGPUQUEUE`). 모니터드 펜스라 사용자 모드 읽기다.
        let completed_before = self.completed();
        self.next += 1;
        // Safety: context4 와 fence 는 이 객체가 쥔 참조.
        let hr = unsafe {
            let signal: SignalFn = vtable_fn(self.context4, CONTEXT4_SIGNAL);
            signal(self.context4, self.fence, self.next)
        };
        let after = wall_span::now().unwrap_or(before);
        wall_span::record(
            Kind::GpuSignal,
            self.slot,
            u8::from(hr < 0),
            self.next,
            before,
            after,
            // (Signal 직전 완료 값 + 1) << 32 | 틱 하위 32 비트. 0 = 모름.
            ((completed_before.wrapping_add(1) & 0xffff_ffff) << 32)
                | (wall_span::current_tick() & 0xffff_ffff),
        );
    }
}

impl GpuFence {
    /// 지금까지 끝난 값(2026-11-17 리뷰). 모니터드 펜스라 사용자 모드 읽기다. Commit 앞뒤에서 읽어
    /// waiter 의 깨움 지연과 무관하게 "Commit 이 끝날 때 GPU 가 끝났나" 를 판정한다.
    pub(crate) fn completed(&self) -> u64 {
        // Safety: 이 객체가 쥔 펜스.
        unsafe {
            let get: GetCompletedFn = vtable_fn(self.fence, FENCE_GET_COMPLETED);
            get(self.fence)
        }
    }
}

impl Drop for GpuFence {
    fn drop(&mut self) {
        // waiter 가 이 펜스를 더 만지지 않게 칸을 비운 뒤 놓는다.
        let entry = &FENCES[self.slot as usize];
        let mut guard = entry.fence.lock().unwrap_or_else(|e| e.into_inner());
        if *guard == self.fence as usize {
            *guard = 0;
        }
        drop(guard);
        // Safety: 이 객체가 쥔 참조. 컴포지터 release_all(EGL terminate 전)에서 불린다.
        unsafe {
            com_release(self.fence);
            com_release(self.context4);
        }
    }
}

fn start_waiter() {
    if WAITER_STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name(String::from("WallGpuFence"))
        .spawn(waiter);
    if let Err(error) = spawned {
        warn!("WALLGPU: 완료 waiter 스레드를 띄우지 못했다: {error}");
    }
}

/// 칸 하나를 다음 값에 건다. 뮤텍스 아래에서 부른다.
///
/// # Safety
/// `fence` 는 살아 있는 ID3D11Fence(칸의 뮤텍스가 지킨다).
unsafe fn arm(fence: usize, event: usize) -> Option<u64> {
    unsafe {
        let get: GetCompletedFn = vtable_fn(fence as *mut c_void, FENCE_GET_COMPLETED);
        let completed = get(fence as *mut c_void);
        if completed == u64::MAX {
            // 장치 제거.
            return None;
        }
        let set: SetEventFn = vtable_fn(fence as *mut c_void, FENCE_SET_EVENT);
        let hr = set(fence as *mut c_void, completed + 1, event as *mut c_void);
        (hr >= 0).then_some(completed)
    }
}

fn waiter() {
    use winapi::um::winbase::WAIT_OBJECT_0;
    // 깨움 시각이 곧 완료 시각의 근사다. painter 는 ABOVE_NORMAL 이고(비디오 시작 때 디코더가 CPU 를
    // 넘친다, painter.rs) 이 스레드는 거의 늘 막혀 있으므로, 가장 높은 우선순위로 둔다(2026-11-17 리뷰).
    // Safety: 현재 스레드 의사 핸들.
    let raised = unsafe {
        winapi::um::processthreadsapi::SetThreadPriority(
            winapi::um::processthreadsapi::GetCurrentThread(),
            winapi::um::winbase::THREAD_PRIORITY_TIME_CRITICAL as i32,
        )
    };
    if raised == 0 {
        warn!("WALLGPU: 완료 waiter 우선순위를 올리지 못했다 -- 깨움 지연이 판정에 섞일 수 있다");
    }
    let mut armed_at = [0u64; MAX_SLOTS];
    loop {
        let mut handles = [std::ptr::null_mut::<c_void>(); MAX_SLOTS];
        let mut which = [0usize; MAX_SLOTS];
        let mut n = 0;
        for (index, entry) in FENCES.iter().enumerate() {
            let event = entry.event.load(Ordering::Acquire);
            if event == 0 {
                continue;
            }
            if entry.fresh.swap(false, Ordering::AcqRel) {
                let guard = entry.fence.lock().unwrap_or_else(|e| e.into_inner());
                if *guard != 0 {
                    // Safety: 칸의 뮤텍스 아래.
                    let _ = unsafe { arm(*guard, event) };
                    armed_at[index] = wall_span::now().unwrap_or(0);
                }
            }
            handles[n] = event as *mut c_void;
            which[n] = index;
            n += 1;
        }
        if n == 0 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            continue;
        }
        // Safety: 살아 있는 이벤트 핸들들(프로세스 수명).
        let result = unsafe {
            winapi::um::synchapi::WaitForMultipleObjects(
                n as u32,
                handles.as_ptr() as *const _,
                0,
                250,
            )
        };
        let woke = result.wrapping_sub(WAIT_OBJECT_0) as usize;
        if woke >= n {
            // 시간 초과(새 칸을 걸러 다시 돈다) 또는 오류.
            continue;
        }
        let at = wall_span::now().unwrap_or(0);
        let index = which[woke];
        let entry = &FENCES[index];
        let guard = entry.fence.lock().unwrap_or_else(|e| e.into_inner());
        if *guard == 0 {
            continue;
        }
        // Safety: 칸의 뮤텍스 아래.
        match unsafe { arm(*guard, entry.event.load(Ordering::Acquire)) } {
            Some(completed) => {
                wall_span::record(
                    Kind::GpuDone,
                    index as u8,
                    0,
                    completed,
                    at,
                    armed_at[index],
                    0,
                );
                armed_at[index] = wall_span::now().unwrap_or(0);
            },
            None => {
                FENCE_REMOVED.fetch_add(1, Ordering::Relaxed);
            },
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 리포트
// ---------------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------------
// VRAM 표본 (2026-11-17 보강)
// ---------------------------------------------------------------------------------------------

/// 디바이스 슬롯마다 IDXGIAdapter3(AddRef 해 둔 것, 프로세스 수명).
static ADAPTERS: [AtomicUsize; MAX_SLOTS] = [const { AtomicUsize::new(0) }; MAX_SLOTS];

/// 그 디바이스의 어댑터를 VRAM 표본에 등록한다(첫 `end_frame` 에서 한 번, 타이밍 경로 밖).
///
/// # Safety
/// `device` 는 살아 있는 ANGLE D3D11 디바이스.
pub(crate) unsafe fn register_adapter(device: *mut ID3D11Device, dcomp_device: usize) {
    use winapi::Interface;
    use winapi::shared::dxgi::{IDXGIAdapter, IDXGIDevice};
    use winapi::shared::dxgi1_4::IDXGIAdapter3;
    if device.is_null() {
        return;
    }
    let slot = wall_span::device_slot(dcomp_device) as usize;
    unsafe {
        let mut dxgi: *mut IDXGIDevice = std::ptr::null_mut();
        if (*device).QueryInterface(&IDXGIDevice::uuidof(), &mut dxgi as *mut _ as *mut _) < 0
            || dxgi.is_null()
        {
            return;
        }
        let mut adapter: *mut IDXGIAdapter = std::ptr::null_mut();
        let hr = (*dxgi).GetAdapter(&mut adapter);
        (*dxgi).Release();
        if hr < 0 || adapter.is_null() {
            return;
        }
        let mut adapter3: *mut IDXGIAdapter3 = std::ptr::null_mut();
        let hr =
            (*adapter).QueryInterface(&IDXGIAdapter3::uuidof(), &mut adapter3 as *mut _ as *mut _);
        (*adapter).Release();
        if hr < 0 || adapter3.is_null() {
            warn!("WALLGPU: dev{slot} IDXGIAdapter3 없음 -- VRAM 표본 끔");
            return;
        }
        if ADAPTERS[slot]
            .compare_exchange(0, adapter3 as usize, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            (*adapter3).Release();
        }
    }
}

/// 표본 하나: (QPC, 슬롯, 로컬 사용, 로컬 예산, 비로컬 사용) 바이트.
type VramSample = (u64, u8, u64, u64, u64);

/// 리포터가 링을 비울 때마다(250 ms) 부른다. 커널 호출이지만 리포터 스레드다.
pub(crate) fn sample_vram(state: &mut ReportState, now_qpc: u64) {
    use winapi::shared::dxgi1_4::{
        DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL,
        DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3,
    };
    for (slot, adapter) in ADAPTERS.iter().enumerate() {
        let adapter = adapter.load(Ordering::Acquire) as *mut IDXGIAdapter3;
        if adapter.is_null() {
            continue;
        }
        let mut local: DXGI_QUERY_VIDEO_MEMORY_INFO = unsafe { std::mem::zeroed() };
        let mut nonlocal: DXGI_QUERY_VIDEO_MEMORY_INFO = unsafe { std::mem::zeroed() };
        // Safety: 프로세스 수명 동안 쥔 어댑터.
        let ok = unsafe {
            (*adapter).QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut local) >= 0
                && (*adapter).QueryVideoMemoryInfo(
                    0,
                    DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL,
                    &mut nonlocal,
                ) >= 0
        };
        if ok {
            state.vram.push((
                now_qpc,
                slot as u8,
                local.CurrentUsage,
                local.Budget,
                nonlocal.CurrentUsage,
            ));
        }
    }
}

/// 초마다 디바이스별 VRAM 줄. 표본은 실시간으로 쌓였으므로 이번 창의 것을 모두 낸다.
fn vram_lines(
    samples: &[VramSample],
    wall_second: impl Fn(u64) -> String,
    second_of: impl Fn(u64) -> u64,
) -> Vec<String> {
    let mut by: std::collections::BTreeMap<(u64, u8), (u64, u64, u64, u64, u64)> =
        Default::default();
    for &(qpc, slot, usage, budget, nonlocal) in samples {
        let e = by
            .entry((second_of(qpc), slot))
            .or_insert((qpc, u64::MAX, 0, budget, 0));
        e.1 = e.1.min(usage);
        e.2 = e.2.max(usage);
        e.3 = e.3.min(budget);
        e.4 = e.4.max(nonlocal);
    }
    const MB: u64 = 1 << 20;
    by.into_iter()
        .map(|((_, slot), (first, min, max, budget, nonlocal))| {
            format!(
                "WALLVRAM at={} dev={slot} local_mb={}..{} budget_mb={} nonlocal_max_mb={}",
                wall_second(first),
                min / MB,
                max / MB,
                budget / MB,
                nonlocal / MB
            )
        })
        .collect()
}

/// 무거운 프레임 문턱과 창당 상한.
const HEAVY_SPAN_US: u64 = 8_000;
const LONG_COMMIT_US: u64 = 4_000;
const HEAVY_MAX: usize = 120;
/// 프레임 끝(또는 Signal)과 그 뒤 Commit 시작 사이의 최대 간격(그 안이어야 짝으로 본다).
const COMMIT_PAIR_MAX_US: u64 = 50_000;
/// 창 끝에서 이 안쪽의 프레임 묶음과 Signal 은 다음 창으로 넘긴다(짝이 다음 창에 올 수 있다).
const DEFER_US: u64 = 250_000;
/// 이보다 오래 넘어온 불완전 묶음은 기다리지 않는다(painter 가 멈춘 경우).
const STALE_US: u64 = 20_000_000;
/// 초 단위 줄은 이만큼 지난 초부터 낸다(늦게 거둬지는 프레임을 기다린다).
const SECOND_HOLD: u64 = 2;

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn p50_p95_max(mut values: Vec<u64>) -> String {
    if values.is_empty() {
        return String::from("-");
    }
    values.sort_unstable();
    format!(
        "{}/{}/{}",
        percentile(&values, 0.5),
        percentile(&values, 0.95),
        values.last().copied().unwrap_or(0)
    )
}

fn is_frame_kind(kind: Kind) -> bool {
    matches!(kind, Kind::GpuFrame | Kind::GpuFrameMeta | Kind::GpuLabel)
}

fn is_commit_kind(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::InframeCommit | Kind::TickCommit | Kind::RecoverCommit | Kind::ExternalCommit
    )
}

/// `to_us` 로 us 를 QPC 틱으로 되돌린다(선형).
fn ticks_for_us(to_us: impl Fn(u64) -> u64, us: u64) -> u64 {
    let probe = 10_000_000u64;
    let per = to_us(probe).max(1);
    (us as u128 * probe as u128 / per as u128) as u64
}

/// 한 프레임을 리포터가 모은 것. 시간은 ns 로 들고 있다가 찍을 때 나눈다(2026-11-17 리뷰).
#[derive(Clone, Debug, Default)]
pub(crate) struct Frame {
    pub painter: u8,
    pub pass: u64,
    pub flag: u8,
    pub span_ns: u64,
    pub between_ns: Option<u64>,
    pub cpu_end: u64,
    pub cpu_ns: u64,
    pub cost_ns: u64,
    /// (이름표, GPU ns, CPU ns)
    pub labels: Vec<(u8, u64, u64)>,
    pub commit_us: Option<u64>,
}

impl Frame {
    fn timed(&self) -> bool {
        self.flag & (FRAME_SKIPPED | FRAME_DISJOINT) == 0
    }

    fn presented(&self) -> bool {
        self.flag & FRAME_UPDATE == 0
    }
}

/// Commit 하나(자리와 무관하게): 디바이스, 가드를 잡은 뒤의 시작, 끝, 그리고 in-frame 이면
/// 완료 프로브가 Commit 앞뒤에서 읽은 펜스 완료 값(+1, 하위 32 비트).
#[derive(Clone, Copy, Debug)]
struct Commit {
    device: u8,
    start: u64,
    end: u64,
    fence: Option<(u64, u64)>,
}

fn commits_of<'a>(events: impl Iterator<Item = &'a Event>) -> Vec<Commit> {
    let mut commits: Vec<Commit> = events
        .filter_map(|e| match e.kind {
            Kind::InframeCommit => Some(Commit {
                device: e.id,
                start: e.a,
                end: e.b,
                fence: (e.c != 0 && (e.c >> 32) != 0 && (e.c & 0xffff_ffff) != 0)
                    .then(|| ((e.c >> 32) - 1, (e.c & 0xffff_ffff) - 1)),
            }),
            // 틱·복구·비디오 fast-path 는 가드를 잡은 뒤(b)부터 끝(c)까지.
            Kind::TickCommit | Kind::RecoverCommit | Kind::ExternalCommit => Some(Commit {
                device: e.id,
                start: e.b,
                end: e.c,
                fence: None,
            }),
            _ => None,
        })
        .collect();
    commits.sort_by_key(|c| c.start);
    commits
}

/// 프레임들을 모은다. 한 painter 의 기록은 한 스레드 링에서 Frame, Meta, Label... 순서로 나온다.
pub(crate) fn frames_of(events: &[Event]) -> Vec<Frame> {
    let mut frames: Vec<Frame> = Vec::new();
    let mut open: [Option<usize>; MAX_SLOTS] = [None; MAX_SLOTS];
    for e in events {
        let painter = (e.id as usize).min(MAX_SLOTS - 1);
        match e.kind {
            Kind::GpuFrame => {
                frames.push(Frame {
                    painter: e.id,
                    pass: e.seq,
                    flag: e.flag,
                    span_ns: e.a,
                    between_ns: (e.b != u64::MAX).then_some(e.b),
                    cpu_end: e.c,
                    ..Default::default()
                });
                open[painter] = (e.flag & FRAME_SKIPPED == 0).then_some(frames.len() - 1);
            },
            Kind::GpuFrameMeta => {
                if let Some(index) = open[painter] {
                    frames[index].cpu_ns = e.a;
                    frames[index].cost_ns = e.b;
                }
            },
            Kind::GpuLabel => {
                if let Some(index) = open[painter] {
                    frames[index].labels.push(((e.c >> 32) as u8, e.a, e.b));
                }
            },
            _ => {},
        }
    }
    frames
}

/// 이번 창에서 분석할 것과 다음 창으로 넘길 것을 가른다(2026-11-17 리뷰). 프레임 묶음은 Frame 과
/// 그 뒤의 Meta·Label 을 한 덩어리로 다룬다. 넘기는 것:
/// - 창 끝 `defer_after` 뒤에 끝난 프레임 묶음과 Signal -- 짝(Commit, 완료)이 다음 창에 온다.
/// - 아직 다 오지 않은 묶음(Meta 가 알린 이름표 수보다 적다) -- 링을 비우는 순간 잘렸다.
/// Commit 과 완료 기록은 넘기지 않는다(다음 창이 되돌아보기로 본다).
pub(crate) fn split_carry(
    events: Vec<Event>,
    defer_after: u64,
    stale_before: u64,
) -> (Vec<Event>, Vec<Event>) {
    struct Group {
        events: Vec<Event>,
        cpu_end: u64,
        skipped: bool,
        announced: Option<u64>,
        labels: u64,
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut open: [Option<usize>; MAX_SLOTS] = [None; MAX_SLOTS];
    let mut analyze = Vec::new();
    let mut carry = Vec::new();
    for e in events {
        let painter = (e.id as usize).min(MAX_SLOTS - 1);
        match e.kind {
            Kind::GpuFrame => {
                groups.push(Group {
                    events: vec![e],
                    cpu_end: e.c,
                    skipped: e.flag & FRAME_SKIPPED != 0,
                    announced: None,
                    labels: 0,
                });
                open[painter] = Some(groups.len() - 1);
            },
            Kind::GpuFrameMeta | Kind::GpuLabel => {
                // 묶음이 없는 조각(창보다 오래된 것)은 버린다.
                if let Some(index) = open[painter] {
                    let group = &mut groups[index];
                    if e.kind == Kind::GpuFrameMeta {
                        group.announced = Some(e.c >> 32);
                    } else {
                        group.labels += 1;
                    }
                    group.events.push(e);
                }
            },
            Kind::GpuSignal if e.b > defer_after => carry.push(e),
            _ => analyze.push(e),
        }
    }
    for group in groups {
        let complete = group.skipped || group.announced.is_some_and(|n| group.labels >= n);
        let late = group.cpu_end > defer_after;
        let stale = group.cpu_end < stale_before;
        if (late || !complete) && !stale {
            carry.extend(group.events);
        } else {
            analyze.extend(group.events);
        }
    }
    (analyze, carry)
}

/// 초 단위 누적(창을 넘어 합친다).
#[derive(Clone, Debug)]
pub(crate) struct SecAcc {
    first: u64,
    frames: u64,
    sum_ns: u64,
    max_ns: u64,
    per_label: [u64; LABELS.len()],
}

/// 리포터가 창 사이에 들고 가는 것(2026-11-17 리뷰).
#[derive(Default)]
pub(crate) struct ReportState {
    carry: Vec<Event>,
    lookback: Vec<Event>,
    seconds: std::collections::BTreeMap<(u64, u8), SecAcc>,
    /// 2026-11-17 보강: 이번 창의 VRAM 표본.
    vram: Vec<VramSample>,
}

/// 리포터가 10 초마다 부른다. `window` 는 이번 창의 모든 기록, `now_qpc` 는 지금.
pub(crate) fn report(
    state: &mut ReportState,
    t_secs: u64,
    window: &[Event],
    slot_map: &[Event],
    now_qpc: u64,
    to_us: impl Fn(u64) -> u64 + Copy,
    wall: impl Fn(u64) -> String,
    wall_second: impl Fn(u64) -> String,
    second_of: impl Fn(u64) -> u64,
) -> Vec<String> {
    let mut gathered = std::mem::take(&mut state.carry);
    gathered.extend(
        window
            .iter()
            .filter(|e| {
                is_frame_kind(e.kind)
                    || is_commit_kind(e.kind)
                    || is_side_kind(e.kind)
                    || matches!(e.kind, Kind::GpuSignal | Kind::GpuDone)
            })
            .copied(),
    );
    let defer_after = now_qpc.saturating_sub(ticks_for_us(to_us, DEFER_US));
    let stale_before = now_qpc.saturating_sub(ticks_for_us(to_us, STALE_US));
    let (analyze, carry) = split_carry(gathered, defer_after, stale_before);
    let mut lines = report_lines(t_secs, &analyze, &state.lookback, slot_map, to_us, &wall);
    // 2026-11-17 보강: 짝을 찾을 때는 되돌아보기 + 이번 창 + 다음 창으로 넘긴 것까지 본다(세지는
    // 이번 창 것만).
    let lookup: Vec<Event> = state
        .lookback
        .iter()
        .chain(analyze.iter())
        .chain(carry.iter())
        .copied()
        .collect();
    lines.extend(bind_lines(t_secs, &analyze, to_us));
    lines.extend(flush_lines(t_secs, &analyze, to_us));
    lines.extend(queue_lines(t_secs, &analyze, &lookup, to_us));
    lines.extend(frame_lines(
        t_secs, &analyze, &lookup, slot_map, to_us, &wall,
    ));
    // VRAM: 진행 중인 초는 다음 창으로 넘긴다(2026-11-17 리뷰 -- 창 경계의 초가 둘로 나뉘었다).
    let current = second_of(now_qpc);
    let (done_samples, open_samples): (Vec<VramSample>, Vec<VramSample>) = state
        .vram
        .iter()
        .partition(|sample| second_of(sample.0) < current);
    lines.extend(vram_lines(&done_samples, &wall_second, &second_of));
    state.vram = open_samples;

    // 초 단위: 누적하고, 충분히 지난 초만 낸다.
    for frame in frames_of(&analyze).iter().filter(|f| f.timed()) {
        let key = (second_of(frame.cpu_end), frame.painter);
        let acc = state.seconds.entry(key).or_insert(SecAcc {
            first: frame.cpu_end,
            frames: 0,
            sum_ns: 0,
            max_ns: 0,
            per_label: [0; LABELS.len()],
        });
        acc.first = acc.first.min(frame.cpu_end);
        acc.frames += 1;
        acc.sum_ns += frame.span_ns;
        acc.max_ns = acc.max_ns.max(frame.span_ns);
        for &(label, gpu, _) in &frame.labels {
            acc.per_label[label as usize] += gpu;
        }
    }
    let cutoff = second_of(now_qpc).saturating_sub(SECOND_HOLD);
    let ready: Vec<(u64, u8)> = state
        .seconds
        .keys()
        .filter(|(second, _)| *second < cutoff)
        .copied()
        .collect();
    for key in ready {
        if let Some(acc) = state.seconds.remove(&key) {
            lines.push(second_line(key.1, &acc, &wall_second));
        }
    }

    state.lookback = analyze
        .iter()
        // 2026-11-17 리뷰: Signal 도 남긴다(한 창 늦게 분석되는 프레임의 짝). 세는 것은 이번 창 것만이다.
        .filter(|e| {
            is_commit_kind(e.kind)
                || is_side_kind(e.kind)
                || matches!(e.kind, Kind::GpuDone | Kind::GpuSignal)
        })
        .copied()
        .collect();
    state.carry = carry;
    lines
}

/// 짝짓기에만 쓰는 곁 기록(2026-11-17 보강): 렌더 구간, bind 단계, flush.
fn is_side_kind(kind: Kind) -> bool {
    matches!(kind, Kind::Render | Kind::BindStep | Kind::Flush)
}

fn us_set(values: Vec<u64>) -> String {
    p50_p95_max(values)
}

/// 디바이스별 bind/unbind 단계(`WALLBIND`).
fn bind_lines(t_secs: u64, events: &[Event], to_us: impl Fn(u64) -> u64 + Copy) -> Vec<String> {
    let mut lines = Vec::new();
    for device in 0..MAX_SLOTS as u8 {
        let mine: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == Kind::BindStep && e.id == device)
            .collect();
        if mine.is_empty() {
            continue;
        }
        let begins: Vec<&&Event> = mine.iter().filter(|e| e.flag == STEP_BEGIN_DRAW).collect();
        let reuse = |kind: u64| begins.iter().filter(|e| (e.c >> 40) & 3 == kind).count();
        let (same, changed, unknown) = (
            reuse(REUSE_SAME),
            reuse(REUSE_CHANGED),
            reuse(REUSE_UNKNOWN),
        );
        let mut sizes: Vec<(u64, u64)> = begins
            .iter()
            .map(|e| ((e.c >> 20) & 0xf_ffff, e.c & 0xf_ffff))
            .collect();
        sizes.sort_unstable();
        // 가장 흔한 크기.
        let mut mode = (0, 0, 0usize);
        let mut i = 0;
        while i < sizes.len() {
            let j = sizes[i..].iter().take_while(|s| **s == sizes[i]).count();
            if j > mode.2 {
                mode = (sizes[i].0, sizes[i].1, j);
            }
            i += j;
        }
        let mut line = format!(
            "WALLBIND t={t_secs}s w=10s dev={device} binds={} texture_same/changed/unknown={same}/{changed}/{unknown} tex={}x{}({})",
            begins.len(),
            mode.0,
            mode.1,
            mode.2
        );
        for (step, name) in BIND_STEPS.iter().enumerate() {
            let values: Vec<u64> = mine
                .iter()
                .filter(|e| e.flag == step as u8)
                .map(|e| to_us(e.b.saturating_sub(e.a)))
                .collect();
            if values.is_empty() {
                continue;
            }
            line.push_str(&format!(" {name}_us={}:{}", values.len(), us_set(values)));
        }
        lines.push(line);
    }
    lines
}

/// 디바이스별 glFlush(`WALLFLUSH`).
fn flush_lines(t_secs: u64, events: &[Event], to_us: impl Fn(u64) -> u64 + Copy) -> Vec<String> {
    let mut lines = Vec::new();
    for device in 0..MAX_SLOTS as u8 {
        let values: Vec<u64> = events
            .iter()
            .filter(|e| e.kind == Kind::Flush && e.id == device)
            .map(|e| to_us(e.b.saturating_sub(e.a)))
            .collect();
        if values.is_empty() {
            continue;
        }
        let long = values.iter().filter(|v| **v >= LONG_COMMIT_US).count();
        lines.push(format!(
            "WALLFLUSH t={t_secs}s w=10s dev={device} n={} dur_us={} long={long}",
            values.len(),
            us_set(values)
        ));
    }
    lines
}

/// Signal 순간 GPU 에서 아직 안 끝난 이전 프레임 수. 모르면 None.
fn pending_at_signal(signal: &Event) -> Option<u64> {
    let packed = signal.c >> 32;
    (packed != 0).then(|| {
        (signal.seq & 0xffff_ffff)
            .saturating_sub(1)
            .saturating_sub(packed - 1)
    })
}

/// 디바이스별: Signal 순간의 밀린 프레임 수와 Signal 막힘(`WALLGPUQUEUE`).
fn queue_lines(
    t_secs: u64,
    events: &[Event],
    lookup: &[Event],
    to_us: impl Fn(u64) -> u64 + Copy,
) -> Vec<String> {
    let _ = lookup;
    let mut lines = Vec::new();
    for device in 0..MAX_SLOTS as u8 {
        let signals: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == Kind::GpuSignal && e.id == device)
            .collect();
        if signals.is_empty() {
            continue;
        }
        // 밀린 수 0/1/2/3+ 마다 (전체, 막힘 4 ms 이상, Signal 시간들)
        let mut buckets: [(u64, u64, Vec<u64>); 4] = Default::default();
        let mut unknown = 0;
        for signal in &signals {
            let call = to_us(signal.b.saturating_sub(signal.a));
            let Some(pending) = pending_at_signal(signal) else {
                unknown += 1;
                continue;
            };
            let bucket = &mut buckets[(pending as usize).min(3)];
            bucket.0 += 1;
            bucket.1 += (call >= LONG_COMMIT_US) as u64;
            bucket.2.push(call);
        }
        let mut line = format!("WALLGPUQUEUE t={t_secs}s w=10s dev={device} unknown={unknown}");
        for (pending, (all, blocked, calls)) in buckets.into_iter().enumerate() {
            line.push_str(&format!(
                " pending{}{}={all}(blocked={blocked},sig_us={})",
                pending,
                if pending == 3 { "+" } else { "" },
                us_set(calls)
            ));
        }
        lines.push(line);
    }
    lines
}

/// 늦은 프레임 문턱(`WALLGPUFRAME`).
const FRAME_RENDER_LATE_US: u64 = 16_667;
const FRAME_AFTER_LATE_US: u64 = 12_000;
const FRAME_LINES_MAX: usize = 200;

/// 늦은 프레임마다 한 줄(`WALLGPUFRAME`). 프레임은 이번 창 것만, 짝은 `lookup` 에서 찾는다.
///
/// 2026-11-17 리뷰: 짝은 (종류, id) 별로 시각순 색인을 한 번 만들어 이분 탐색으로 찾는다(프레임마다
/// 전체를 훑지 않는다). flush 와 Signal 은 그 프레임의 렌더 구간 안(렌더가 없으면 같은 painter 의
/// 다음 프레임 끝 전)에서만 찾는다 -- 이 프레임이 flush 하지 않았을 때 다음 프레임 것을 잡지 않게.
fn frame_lines(
    t_secs: u64,
    events: &[Event],
    lookup: &[Event],
    slot_map: &[Event],
    to_us: impl Fn(u64) -> u64 + Copy,
    wall: impl Fn(u64) -> String,
) -> Vec<String> {
    use std::collections::HashMap;
    let device_of = |painter: u8| {
        slot_map
            .iter()
            .find(|e| e.kind == Kind::SlotMap && e.id == painter)
            .map(|e| e.a as u8)
    };
    let within = |from: u64, at: u64| at >= from && to_us(at - from) <= COMMIT_PAIR_MAX_US;
    let mut index: HashMap<(u8, u8), Vec<&Event>> = HashMap::new();
    for e in lookup.iter().filter(|e| {
        matches!(
            e.kind,
            Kind::Render | Kind::GpuSignal | Kind::Flush | Kind::GpuDone | Kind::BindStep
        )
    }) {
        index.entry((e.kind as u8, e.id)).or_default().push(e);
    }
    for list in index.values_mut() {
        list.sort_by_key(|e| e.a);
    }
    let empty: Vec<&Event> = Vec::new();
    let list = |kind: Kind, id: u8| index.get(&(kind as u8, id)).unwrap_or(&empty);
    let mut commits: HashMap<u8, Vec<Commit>> = HashMap::new();
    for c in commits_of(lookup.iter()) {
        commits.entry(c.device).or_default().push(c);
    }
    let no_commits: Vec<Commit> = Vec::new();

    let frames = frames_of(events);
    // 같은 painter 의 다음 프레임 끝(렌더가 없을 때 짝의 상한).
    let mut ends: HashMap<u8, Vec<u64>> = HashMap::new();
    for f in frames.iter().filter(|f| f.timed() && f.presented()) {
        ends.entry(f.painter).or_default().push(f.cpu_end);
    }
    for v in ends.values_mut() {
        v.sort_unstable();
    }

    let mut rows: Vec<(u64, u64, String)> = Vec::new();
    for frame in frames.iter().filter(|f| f.timed() && f.presented()) {
        let Some(device) = device_of(frame.painter) else {
            continue;
        };
        let renders = list(Kind::Render, frame.painter);
        let at = renders.partition_point(|e| e.a <= frame.cpu_end);
        let render = (at > 0)
            .then(|| renders[at - 1])
            .filter(|r| r.b >= frame.cpu_end);
        let render_us = render.map(|r| to_us(r.b.saturating_sub(r.a)));
        let upper = match render {
            Some(r) => r.b,
            None => ends
                .get(&frame.painter)
                .and_then(|v| v.iter().find(|end| **end > frame.cpu_end).copied())
                .unwrap_or(u64::MAX),
        };
        let first_after = |kind: Kind| {
            let v = list(kind, device);
            let i = v.partition_point(|e| e.a < frame.cpu_end);
            v.get(i)
                .copied()
                .filter(|e| within(frame.cpu_end, e.a) && e.a <= upper)
        };
        let signal = first_after(Kind::GpuSignal);
        let flush = first_after(Kind::Flush);
        let done = signal.and_then(|s| {
            let v = list(Kind::GpuDone, device);
            let i = v.partition_point(|d| d.a < s.a);
            v[i..].iter().copied().find(|d| d.seq >= s.seq)
        });
        let commit_from = signal
            .map(|s| s.b)
            .or(flush.map(|f| f.b))
            .unwrap_or(frame.cpu_end);
        let device_commits = commits.get(&device).unwrap_or(&no_commits);
        let i = device_commits.partition_point(|c| c.start < commit_from);
        let commit = device_commits
            .get(i)
            .filter(|c| within(commit_from, c.start));
        let sig_us = signal.map(|s| to_us(s.b.saturating_sub(s.a)));
        let after_us = match (signal, done) {
            (Some(s), Some(d)) => Some(to_us(d.a.saturating_sub(s.b))),
            _ => None,
        };
        let flush_us = flush.map(|f| to_us(f.b.saturating_sub(f.a)));
        let commit_us = commit.map(|c| to_us(c.end.saturating_sub(c.start)));
        let late = render_us.unwrap_or(0) > FRAME_RENDER_LATE_US
            || sig_us.unwrap_or(0) >= LONG_COMMIT_US
            || flush_us.unwrap_or(0) >= LONG_COMMIT_US
            || commit_us.unwrap_or(0) >= LONG_COMMIT_US
            || after_us.unwrap_or(0) >= FRAME_AFTER_LATE_US;
        if !late {
            continue;
        }
        // 그 렌더 안의 bind 단계 합.
        let mut steps = [0u64; BIND_STEPS.len()];
        let mut binds = 0;
        if let Some(r) = render {
            let v = list(Kind::BindStep, device);
            let i = v.partition_point(|e| e.a < r.a);
            for e in v[i..]
                .iter()
                .take_while(|e| e.a <= r.b)
                .filter(|e| e.b <= r.b)
            {
                steps[(e.flag as usize).min(BIND_STEPS.len() - 1)] +=
                    to_us(e.b.saturating_sub(e.a));
                binds += (e.flag == STEP_BEGIN_DRAW) as u64;
            }
        }
        let mut labels = frame.labels.clone();
        labels.sort_by_key(|(_, g, _)| std::cmp::Reverse(*g));
        let top: Vec<String> = labels
            .iter()
            .take(3)
            .map(|(l, g, _)| format!("{}:{}", LABELS[*l as usize].replace(' ', "_"), g / 1000))
            .collect();
        let opt = |v: Option<u64>| v.map_or(String::from("-"), |v| v.to_string());
        let row = format!(
            "WALLGPUFRAME at={} p{} dev={device} pass={} render_us={} gpu_span_us={} between_us={} \
             cpu_us={} binds={binds} bind_us={}/{}/{}/{} unbind_us={}/{} flush_us={} sig_us={} \
             pending_at_sig={} gpu_after_sig_us={} after_minus_span_us={} commit_us={} top_gpu_us={}",
            wall(frame.cpu_end),
            frame.painter,
            frame.pass,
            opt(render_us),
            frame.span_ns / 1000,
            frame
                .between_ns
                .map_or(String::from("-"), |v| (v / 1000).to_string()),
            frame.cpu_ns / 1000,
            steps[STEP_BEGIN_DRAW as usize],
            steps[STEP_PBUFFER_CREATE as usize],
            steps[STEP_MAKE_CURRENT as usize],
            steps[STEP_EXPAND_CLEAR as usize],
            steps[STEP_END_DRAW as usize],
            steps[STEP_PBUFFER_DESTROY as usize],
            opt(flush_us),
            opt(sig_us),
            opt(signal.and_then(pending_at_signal)),
            opt(after_us),
            after_us.map_or(String::from("-"), |a| {
                (a as i64 - (frame.span_ns / 1000) as i64).to_string()
            }),
            opt(commit_us),
            if top.is_empty() {
                String::from("-")
            } else {
                top.join(",")
            },
        );
        rows.push((frame.cpu_end, render_us.unwrap_or(0), row));
    }
    let dropped = rows.len().saturating_sub(FRAME_LINES_MAX);
    if dropped > 0 {
        rows.sort_by_key(|(_, render, _)| std::cmp::Reverse(*render));
        rows.truncate(FRAME_LINES_MAX);
    }
    rows.sort_by_key(|(at, _, _)| *at);
    let mut lines: Vec<String> = rows.into_iter().map(|(_, _, row)| row).collect();
    if dropped > 0 {
        lines.push(format!(
            "WALLGPUFRAME t={t_secs}s dropped={dropped} (창당 상한 {FRAME_LINES_MAX}, 렌더가 긴 것만 남겼다)"
        ));
    }
    lines
}

fn second_line(painter: u8, acc: &SecAcc, wall_second: impl Fn(u64) -> String) -> String {
    let mut order: Vec<usize> = (0..LABELS.len())
        .filter(|l| acc.per_label[*l] > 0)
        .collect();
    order.sort_by_key(|l| std::cmp::Reverse(acc.per_label[*l]));
    let top: Vec<String> = order
        .iter()
        .take(4)
        .map(|l| {
            format!(
                "{}:{:.1}",
                LABELS[*l].replace(' ', "_"),
                acc.per_label[*l] as f64 / 1e6
            )
        })
        .collect();
    format!(
        "WALLGPUSEC at={} p{painter} frames={} gpu_ms={:.1} max_us={} top_ms={}",
        wall_second(acc.first),
        acc.frames,
        acc.sum_ns as f64 / 1e6,
        acc.max_ns / 1000,
        if top.is_empty() {
            String::from("-")
        } else {
            top.join(",")
        },
    )
}

/// 10 초 창의 줄들. `events` 는 이번에 분석할 것, `lookback` 은 이전 창의 Commit 과 완료(짝짓기용).
pub(crate) fn report_lines(
    t_secs: u64,
    events: &[Event],
    lookback: &[Event],
    slot_map: &[Event],
    to_us: impl Fn(u64) -> u64 + Copy,
    wall: impl Fn(u64) -> String,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut frames = frames_of(events);
    let commits = commits_of(lookback.iter().chain(events.iter()));
    let device_of = |painter: u8| {
        slot_map
            .iter()
            .find(|e| e.kind == Kind::SlotMap && e.id == painter)
            .map(|e| e.a as u8)
    };
    // 표출하는 프레임마다 그 뒤 첫 Commit(같은 디바이스). `update()` 안의 렌더는 Commit 이 없다.
    for frame in frames.iter_mut() {
        if !frame.timed() || !frame.presented() {
            continue;
        }
        let Some(device) = device_of(frame.painter) else {
            continue;
        };
        frame.commit_us = commits
            .iter()
            .find(|c| {
                c.device == device
                    && c.start >= frame.cpu_end
                    && to_us(c.start - frame.cpu_end) <= COMMIT_PAIR_MAX_US
            })
            .map(|c| to_us(c.end.saturating_sub(c.start)));
    }

    let painters: std::collections::BTreeSet<u8> = frames.iter().map(|f| f.painter).collect();
    for &painter in &painters {
        let mine: Vec<&Frame> = frames.iter().filter(|f| f.painter == painter).collect();
        let timed: Vec<&&Frame> = mine.iter().filter(|f| f.timed()).collect();
        let render = timed.iter().filter(|f| f.presented()).count();
        let skipped = mine.iter().filter(|f| f.flag & FRAME_SKIPPED != 0).count();
        let disjoint = mine
            .iter()
            .filter(|f| f.flag & FRAME_SKIPPED == 0 && f.flag & FRAME_DISJOINT != 0)
            .count();
        let truncated = mine
            .iter()
            .filter(|f| f.flag & FRAME_TRUNCATED != 0)
            .count();
        let span_sum_ns: u64 = timed.iter().map(|f| f.span_ns).sum();
        let cost: Vec<u64> = timed.iter().map(|f| f.cost_ns).collect();
        let cost_mean_us = cost
            .iter()
            .sum::<u64>()
            .checked_div(cost.len() as u64 * 1000)
            .unwrap_or(0);
        lines.push(format!(
            "WALLGPU t={t_secs}s w=10s p{painter} dev={} frames={render}/{} skipped={skipped} \
             disjoint={disjoint} trunc={truncated} span_us={} between_us={} cpu_us={} \
             probe_us={cost_mean_us}/{} busy={:.1}%",
            device_of(painter).map_or(String::from("-"), |d| d.to_string()),
            timed.len() - render,
            p50_p95_max(timed.iter().map(|f| f.span_ns / 1000).collect()),
            p50_p95_max(
                timed
                    .iter()
                    .filter_map(|f| f.between_ns)
                    .map(|n| n / 1000)
                    .collect()
            ),
            p50_p95_max(timed.iter().map(|f| f.cpu_ns / 1000).collect()),
            cost.iter().max().copied().unwrap_or(0) / 1000,
            span_sum_ns as f64 / 1e8,
        ));

        // 이름표: GPU 합 순. (이름표, 프레임별 GPU ns, CPU ns 합)
        let mut per_label: Vec<(u8, Vec<u64>, u64)> = Vec::new();
        for frame in &timed {
            for &(label, gpu, cpu) in &frame.labels {
                match per_label.iter_mut().find(|(l, _, _)| *l == label) {
                    Some(entry) => {
                        entry.1.push(gpu);
                        entry.2 += cpu;
                    },
                    None => per_label.push((label, vec![gpu], cpu)),
                }
            }
        }
        let total: u64 = per_label
            .iter()
            .map(|(_, g, _)| g.iter().sum::<u64>())
            .sum();
        per_label.sort_by_key(|(_, g, _)| std::cmp::Reverse(g.iter().sum::<u64>()));
        let mut tag_line = format!("WALLGPUTAG t={t_secs}s w=10s p{painter}");
        for (label, gpu, cpu) in &per_label {
            let sum: u64 = gpu.iter().sum();
            let mut sorted = gpu.clone();
            sorted.sort_unstable();
            tag_line.push_str(&format!(
                " {}={:.1}ms({:.0}%)/p95={}us/cpu={:.1}ms",
                LABELS[*label as usize].replace(' ', "_"),
                sum as f64 / 1e6,
                if total == 0 {
                    0.0
                } else {
                    sum as f64 * 100.0 / total as f64
                },
                percentile(&sorted, 0.95) / 1000,
                *cpu as f64 / 1e6,
            ));
        }
        lines.push(tag_line);

        // 긴 Commit 이 뒤따른 프레임과 아닌 프레임(표출 프레임만).
        let (long, short): (Vec<&&Frame>, Vec<&&Frame>) = timed
            .iter()
            .filter(|f| f.presented() && f.commit_us.is_some())
            .partition(|f| f.commit_us.unwrap_or(0) >= LONG_COMMIT_US);
        let label_p50_us = |set: &[&&Frame], label: u8| {
            let mut values: Vec<u64> = set
                .iter()
                .map(|f| {
                    f.labels
                        .iter()
                        .filter(|(l, _, _)| *l == label)
                        .map(|(_, g, _)| g)
                        .sum::<u64>()
                })
                .collect();
            values.sort_unstable();
            percentile(&values, 0.5) / 1000
        };
        let mut top = String::new();
        for (label, _, _) in per_label.iter().take(6) {
            top.push_str(&format!(
                "{}{}:{}/{}us",
                if top.is_empty() { "" } else { "," },
                LABELS[*label as usize].replace(' ', "_"),
                label_p50_us(&long, *label),
                label_p50_us(&short, *label),
            ));
        }
        lines.push(format!(
            "WALLGPULONG t={t_secs}s w=10s p{painter} long={} short={} unpaired={} \
             span_long_us={} span_short_us={} cpu_long_us={} cpu_short_us={} \
             top_long/short_p50={}",
            long.len(),
            short.len(),
            timed
                .iter()
                .filter(|f| f.presented() && f.commit_us.is_none())
                .count(),
            p50_p95_max(long.iter().map(|f| f.span_ns / 1000).collect()),
            p50_p95_max(short.iter().map(|f| f.span_ns / 1000).collect()),
            p50_p95_max(long.iter().map(|f| f.cpu_ns / 1000).collect()),
            p50_p95_max(short.iter().map(|f| f.cpu_ns / 1000).collect()),
            if top.is_empty() {
                String::from("-")
            } else {
                top
            },
        ));
    }

    lines.extend(fence_lines(t_secs, events, lookback, &commits, to_us));

    // 무거운 프레임 하나하나.
    let mut heavy: Vec<&Frame> = frames
        .iter()
        .filter(|f| {
            f.timed()
                && (f.span_ns / 1000 >= HEAVY_SPAN_US || f.commit_us.unwrap_or(0) >= LONG_COMMIT_US)
        })
        .collect();
    let dropped = heavy.len().saturating_sub(HEAVY_MAX);
    if dropped > 0 {
        heavy.sort_by_key(|f| std::cmp::Reverse((f.span_ns / 1000).max(f.commit_us.unwrap_or(0))));
        heavy.truncate(HEAVY_MAX);
    }
    heavy.sort_by_key(|f| f.cpu_end);
    for frame in heavy {
        let mut labels = frame.labels.clone();
        labels.sort_by_key(|(_, g, _)| std::cmp::Reverse(*g));
        let top: Vec<String> = labels
            .iter()
            .take(5)
            .map(|(l, g, c)| {
                format!(
                    "{}:{}/{}us",
                    LABELS[*l as usize].replace(' ', "_"),
                    g / 1000,
                    c / 1000
                )
            })
            .collect();
        lines.push(format!(
            "WALLGPUHEAVY at={} p{} pass={} kind={}{} span_us={} between_us={} cpu_us={} \
             commit_us={} top_gpu/cpu={}",
            wall(frame.cpu_end),
            frame.painter,
            frame.pass,
            if frame.presented() {
                "render"
            } else {
                "update"
            },
            if frame.flag & FRAME_TRUNCATED != 0 {
                " trunc=1"
            } else {
                ""
            },
            frame.span_ns / 1000,
            frame
                .between_ns
                .map_or(String::from("-"), |v| (v / 1000).to_string()),
            frame.cpu_ns / 1000,
            frame.commit_us.map_or(String::from("-"), |v| v.to_string()),
            top.join(","),
        ));
    }
    if dropped > 0 {
        lines.push(format!(
            "WALLGPUHEAVY t={t_secs}s dropped={dropped} (창당 상한 {HEAVY_MAX}, 큰 것만 남겼다)"
        ));
    }
    let removed = FENCE_REMOVED.load(Ordering::Relaxed);
    if removed > 0 {
        lines.push(format!("WALLGPUDONE t={t_secs}s device_removed={removed}"));
    }
    lines
}

/// 디바이스별 완료 프로브 줄.
///
/// 긴 Commit 마다 GPU 가 언제 끝났는지를 셋으로 가른다: Commit 시작 전(before), Commit 이 도는
/// 동안(during = Commit 이 GPU 완료를 기다렸을 수 있다), Commit 이 돌아온 뒤(after = Commit 은
/// GPU 를 기다리지 않았다). in-frame Commit 이면 Commit 앞뒤에서 직접 읽은 펜스 완료 값으로
/// 가르고(`by_read`), 아니면 waiter 의 깨움 시각으로 가른다. 깨움 시각은 실제 완료보다 늦다 --
/// 그 지연을 `wake_lag_us` 로 낸다(완료 값으로는 Commit 끝까지 끝났는데 깨움이 그 뒤인 경우).
fn fence_lines(
    t_secs: u64,
    events: &[Event],
    lookback: &[Event],
    commits: &[Commit],
    to_us: impl Fn(u64) -> u64 + Copy,
) -> Vec<String> {
    let mut lines = Vec::new();
    let slack = ticks_for_us(to_us, 500);
    for device in 0..MAX_SLOTS as u8 {
        let signals: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == Kind::GpuSignal && e.id == device)
            .collect();
        if signals.is_empty() {
            continue;
        }
        let mut dones: Vec<&Event> = lookback
            .iter()
            .chain(events.iter())
            .filter(|e| e.kind == Kind::GpuDone && e.id == device)
            .collect();
        dones.sort_by_key(|e| e.a);
        let mut call = Vec::new();
        let mut after_sig = Vec::new();
        let (mut before, mut during, mut after, mut long_n, mut by_read) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
        let mut done_after_commit = Vec::new();
        let mut wake_lag = Vec::new();
        let mut failed = 0;
        let mut matched = 0;
        let mut unpaired = 0;
        for signal in &signals {
            call.push(to_us(signal.b.saturating_sub(signal.a)));
            failed += (signal.flag & 1) as u64;
            let done = dones
                .iter()
                .find(|d| d.seq >= signal.seq && d.a >= signal.a);
            if let Some(done) = done {
                matched += 1;
                after_sig.push(to_us(done.a.saturating_sub(signal.b)));
            }
            let Some(commit) = commits.iter().find(|c| {
                c.device == device
                    && c.start >= signal.b
                    && to_us(c.start - signal.b) <= COMMIT_PAIR_MAX_US
            }) else {
                unpaired += 1;
                continue;
            };
            let value = signal.seq & 0xffff_ffff;
            // 펜스 읽기로 본 위치(있으면). 0 = before, 1 = during, 2 = after.
            let read = commit.fence.map(|(at_start, at_end)| {
                if at_start >= value {
                    0
                } else if at_end >= value {
                    1
                } else {
                    2
                }
            });
            if let (Some(read), Some(done)) = (read, done) {
                if read <= 1 && done.a > commit.end {
                    wake_lag.push(to_us(done.a - commit.end));
                }
            }
            let dur = to_us(commit.end.saturating_sub(commit.start));
            if dur < LONG_COMMIT_US {
                continue;
            }
            long_n += 1;
            let place = match (read, done) {
                (Some(read), _) => {
                    by_read += 1;
                    read
                },
                (None, Some(done)) if done.a < commit.start => 0,
                (None, Some(done)) if done.a <= commit.end + slack => 1,
                (None, Some(_)) => 2,
                (None, None) => continue,
            };
            match place {
                0 => before += 1,
                1 => during += 1,
                _ => {
                    after += 1;
                    if let Some(done) = done {
                        done_after_commit.push(to_us(done.a.saturating_sub(commit.end)));
                    }
                },
            }
        }
        lines.push(format!(
            "WALLGPUDONE t={t_secs}s w=10s dev={device} sig={} fail={failed} unpaired={unpaired} \
             sig_call_us={} done={matched} gpu_after_sig_us={} long={long_n} by_read={by_read} \
             long_done_before={before} long_done_during={during} long_done_after={after} \
             gpu_after_long_commit_us={} wake_lag_us={}",
            signals.len(),
            p50_p95_max(call),
            p50_p95_max(after_sig),
            p50_p95_max(done_after_commit),
            p50_p95_max(wake_lag),
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: Kind, id: u8, flag: u8, seq: u64, a: u64, b: u64, c: u64) -> Event {
        Event {
            kind,
            id,
            flag,
            seq,
            a,
            b,
            c,
        }
    }

    fn frame(
        painter: u8,
        pass: u64,
        span_ns: u64,
        cpu_end: u64,
        labels: &[(&str, u64)],
    ) -> Vec<Event> {
        let mut out = vec![
            ev(Kind::GpuFrame, painter, 0, pass, span_ns, u64::MAX, cpu_end),
            ev(
                Kind::GpuFrameMeta,
                painter,
                0,
                pass,
                1_000,
                1_000,
                ((labels.len() as u64) << 32) | (labels.len() as u64 + 1),
            ),
        ];
        for (label, ns) in labels {
            out.push(ev(
                Kind::GpuLabel,
                painter,
                0,
                pass,
                *ns,
                0,
                ((label_index(label) as u64) << 32) | 1,
            ));
        }
        out
    }

    #[test]
    fn vtable_slots_match_the_windows_crate_layout() {
        // windows 0.62.2: ID3D11Device5::CreateFence 68, ID3D11DeviceContext4::Signal 147,
        // ID3D11Fence::GetCompletedValue 8 / SetEventOnCompletion 9.
        assert_eq!(DEVICE5_CREATE_FENCE, 68);
        assert_eq!(CONTEXT4_SIGNAL, 147);
        assert_eq!(FENCE_GET_COMPLETED, 8);
        assert_eq!(FENCE_SET_EVENT, 9);
    }

    #[test]
    fn labels_resolve_and_unknown_is_other() {
        assert_eq!(LABELS[label_index("B_YuvImage") as usize], "B_YuvImage");
        assert_eq!(LABELS[label_index("target init") as usize], "target init");
        assert_eq!(LABELS[LABEL_TRUNCATED as usize], "W_Truncated");
        assert_eq!(label_index("no such tag"), LABEL_OTHER);
        assert!(LABELS.len() < 64);
    }

    #[test]
    fn long_commit_is_placed_by_fence_reads_then_by_wake_time() {
        // QPC = us.
        let to_us = |t: u64| t;
        let map = [ev(Kind::SlotMap, 0, 0, 0, 0, 0, 0)];
        let read = |at_start: u64, at_end: u64| ((at_start + 1) << 32) | (at_end + 1);
        let events = vec![
            // 1: 읽기로 during(시작 때 0, 끝날 때 1). 깨움은 Commit 끝 590 us 뒤 -> wake_lag.
            ev(Kind::GpuSignal, 0, 0, 1, 1000, 1002, 7),
            ev(Kind::InframeCommit, 0, 0, 7, 1010, 9010, read(0, 1)),
            ev(Kind::GpuDone, 0, 0, 1, 9600, 0, 0),
            // 2: 읽기로 before(시작 때 이미 2).
            ev(Kind::GpuSignal, 0, 0, 2, 20_000, 20_001, 8),
            ev(Kind::GpuDone, 0, 0, 2, 20_005, 0, 0),
            ev(Kind::InframeCommit, 0, 0, 8, 20_010, 26_000, read(2, 2)),
            // 3: 읽기 없음 -> 깨움 시각으로 after, Commit 끝 1000 us 뒤.
            ev(Kind::GpuSignal, 0, 0, 3, 40_000, 40_001, 9),
            ev(Kind::InframeCommit, 0, 0, 9, 40_010, 46_000, 0),
            ev(Kind::GpuDone, 0, 0, 3, 47_000, 0, 0),
        ];
        let lines = report_lines(10, &events, &[], &map, to_us, |_| String::from("x"));
        let done = lines.iter().find(|l| l.starts_with("WALLGPUDONE")).unwrap();
        assert!(done.contains("long=3 by_read=2"), "{done}");
        assert!(done.contains("long_done_before=1"), "{done}");
        assert!(done.contains("long_done_during=1"), "{done}");
        assert!(done.contains("long_done_after=1"), "{done}");
        assert!(
            done.contains("gpu_after_long_commit_us=1000/1000/1000"),
            "{done}"
        );
        assert!(done.contains("wake_lag_us=590/590/590"), "{done}");
    }

    #[test]
    fn labels_are_summed_per_painter_and_joined_to_the_next_commit() {
        let to_us = |t: u64| t;
        let map = [ev(Kind::SlotMap, 1, 0, 0, 2, 5760, 0)];
        let mut events = frame(
            1,
            5,
            9_000_000,
            100,
            &[("B_Image", 6_000_000), ("B_YuvImage", 3_000_000)],
        );
        events.push(ev(Kind::InframeCommit, 2, 0, 5, 150, 6150, 0));
        // `update()` 안의 렌더는 Commit 과 짝짓지 않는다.
        events.push(ev(Kind::GpuFrame, 1, FRAME_UPDATE, 5, 1_000, u64::MAX, 120));
        let lines = report_lines(10, &events, &[], &map, to_us, |_| String::from("t"));
        let tag = lines.iter().find(|l| l.starts_with("WALLGPUTAG")).unwrap();
        assert!(tag.contains("B_Image=6.0ms(67%)"), "{tag}");
        assert!(tag.contains("B_YuvImage=3.0ms(33%)"), "{tag}");
        let long = lines.iter().find(|l| l.starts_with("WALLGPULONG")).unwrap();
        assert!(long.contains("long=1 short=0 unpaired=0"), "{long}");
        let heavy = lines
            .iter()
            .find(|l| l.starts_with("WALLGPUHEAVY"))
            .unwrap();
        assert!(
            heavy.contains("kind=render")
                && heavy.contains("span_us=9000")
                && heavy.contains("commit_us=6000"),
            "{heavy}"
        );
    }

    #[test]
    fn late_and_incomplete_frame_groups_are_carried_to_the_next_window() {
        let mut events = frame(0, 1, 1_000, 100, &[("B_Solid", 1_000)]);
        // 늦게 끝난 프레임(창 끝 근처).
        events.extend(frame(1, 2, 1_000, 9_900, &[("B_Image", 1_000)]));
        // 잘린 묶음: Meta 는 이름표 둘을 알렸는데 하나만 왔다.
        let mut cut = frame(
            2,
            3,
            1_000,
            500,
            &[("B_Image", 1_000), ("B_YuvImage", 1_000)],
        );
        cut.pop();
        events.extend(cut);
        events.push(ev(Kind::GpuSignal, 0, 0, 1, 9_950, 9_951, 0));
        events.push(ev(Kind::GpuSignal, 0, 0, 2, 5_000, 5_001, 0));
        let (analyze, carry) = split_carry(events, 9_000, 0);
        assert_eq!(frames_of(&analyze).len(), 1, "{analyze:?}");
        assert_eq!(frames_of(&carry).len(), 2, "{carry:?}");
        assert_eq!(
            carry.iter().filter(|e| e.kind == Kind::GpuSignal).count(),
            1
        );
        assert_eq!(
            analyze.iter().filter(|e| e.kind == Kind::GpuSignal).count(),
            1
        );
    }

    #[test]
    fn pending_frames_at_signal_come_from_the_read_before_signal() {
        // 값 10 을 신호하기 직전에 7 까지 끝났다 -> 8, 9 가 밀려 있다.
        let signal = ev(Kind::GpuSignal, 0, 0, 10, 0, 0, ((7u64 + 1) << 32) | 5);
        assert_eq!(pending_at_signal(&signal), Some(2));
        let unknown = ev(Kind::GpuSignal, 0, 0, 10, 0, 0, 5);
        assert_eq!(pending_at_signal(&unknown), None);
        let to_us = |t: u64| t;
        let events = vec![
            ev(Kind::GpuSignal, 1, 0, 10, 0, 9000, ((7u64 + 1) << 32) | 1),
            ev(
                Kind::GpuSignal,
                1,
                0,
                11,
                20_000,
                20_100,
                ((10u64 + 1) << 32) | 2,
            ),
        ];
        let lines = queue_lines(10, &events, &[], to_us);
        assert!(lines[0].contains("pending0=1(blocked=0"), "{}", lines[0]);
        assert!(lines[0].contains("pending2=1(blocked=1"), "{}", lines[0]);
    }

    #[test]
    fn bind_steps_report_texture_reuse_and_step_times() {
        let to_us = |t: u64| t;
        let events = vec![
            ev(
                Kind::BindStep,
                2,
                STEP_BEGIN_DRAW,
                1,
                0,
                300,
                pack_texture(5760, 2160, REUSE_UNKNOWN),
            ),
            ev(Kind::BindStep, 2, STEP_PBUFFER_CREATE, 1, 300, 1300, 0),
            ev(
                Kind::BindStep,
                2,
                STEP_BEGIN_DRAW,
                2,
                2000,
                2200,
                pack_texture(5760, 2160, REUSE_SAME),
            ),
            ev(
                Kind::BindStep,
                2,
                STEP_BEGIN_DRAW,
                3,
                4000,
                4200,
                pack_texture(5760, 2160, REUSE_CHANGED),
            ),
        ];
        let line = &bind_lines(10, &events, to_us)[0];
        assert!(
            line.contains("binds=3 texture_same/changed/unknown=1/1/1 tex=5760x2160(3)"),
            "{line}"
        );
        assert!(
            line.contains("pbuffer_create_us=1:1000/1000/1000"),
            "{line}"
        );
    }

    #[test]
    fn a_late_frame_row_joins_render_bind_signal_done_and_commit() {
        let to_us = |t: u64| t;
        let map = [ev(Kind::SlotMap, 1, 0, 0, 3, 5760, 0)];
        let mut events = frame(1, 7, 3_000_000, 10_000, &[("W_Bind", 1_000_000)]);
        let lookup = vec![
            ev(Kind::Render, 1, 0, 7, 1_000, 30_000, 0),
            ev(
                Kind::BindStep,
                3,
                STEP_BEGIN_DRAW,
                7,
                2_000,
                2_500,
                pack_texture(1, 1, REUSE_UNKNOWN),
            ),
            ev(Kind::BindStep, 3, STEP_PBUFFER_CREATE, 7, 2_500, 4_500, 0),
            ev(
                Kind::GpuSignal,
                3,
                0,
                5,
                12_000,
                22_000,
                ((3u64 + 1) << 32) | 7,
            ),
            ev(Kind::InframeCommit, 3, 0, 7, 22_010, 22_300, 0),
            ev(Kind::GpuDone, 3, 0, 5, 30_000, 0, 0),
        ];
        events.extend(lookup.iter().copied());
        let lines = frame_lines(10, &events, &lookup, &map, to_us, |_| String::from("T"));
        assert_eq!(lines.len(), 1, "{lines:?}");
        let row = &lines[0];
        for want in [
            "p1 dev=3 pass=7 render_us=29000",
            "binds=1 bind_us=500/2000/0/0",
            "sig_us=10000 pending_at_sig=1 gpu_after_sig_us=8000 after_minus_span_us=5000",
            "commit_us=290",
        ] {
            assert!(row.contains(want), "{want} not in {row}");
        }
    }

    #[test]
    fn a_frame_does_not_borrow_the_next_frames_flush() {
        let to_us = |t: u64| t;
        let map = [ev(Kind::SlotMap, 0, 0, 0, 0, 0, 0)];
        // 프레임 1 은 렌더가 30 ms(늦음)인데 flush 하지 않았다. 다음 렌더(40 ms~)의 flush 를 잡으면 안 된다.
        let mut events = frame(0, 1, 1_000_000, 20_000, &[("B_Image", 1_000_000)]);
        let lookup = vec![
            ev(Kind::Render, 0, 1, 1, 0, 30_000, 0),
            ev(Kind::Render, 0, 1, 2, 40_000, 45_000, 0),
            ev(Kind::Flush, 0, 0, 2, 44_000, 44_500, 0),
        ];
        events.extend(lookup.iter().copied());
        let lines = frame_lines(10, &events, &lookup, &map, to_us, |_| String::from("T"));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("flush_us=- "), "{}", lines[0]);
    }

    #[test]
    fn seconds_are_merged_across_windows_and_held_until_old_enough() {
        let to_us = |t: u64| t;
        let second_of = |q: u64| q / 1_000_000;
        let wall_second = |q: u64| format!("s{}", q / 1_000_000);
        let mut state = ReportState::default();
        let mut window = frame(0, 1, 2_000_000, 3_100_000, &[("B_Image", 2_000_000)]);
        window.extend(frame(0, 2, 1_000_000, 9_900_000, &[("B_Image", 1_000_000)]));
        let first = report(
            &mut state,
            10,
            &window,
            &[],
            10_000_000,
            to_us,
            |_| String::new(),
            wall_second,
            second_of,
        );
        let sec3 = first
            .iter()
            .find(|l| l.starts_with("WALLGPUSEC at=s3"))
            .unwrap();
        assert!(sec3.contains("frames=1 gpu_ms=2.0"), "{sec3}");
        assert!(!first.iter().any(|l| l.starts_with("WALLGPUSEC at=s9")));
        // 다음 창: 넘어온 프레임과 같은 초의 새 프레임이 한 줄로 합쳐진다.
        let window = frame(0, 3, 1_000_000, 9_950_000, &[("B_Image", 1_000_000)]);
        let second = report(
            &mut state,
            20,
            &window,
            &[],
            20_000_000,
            to_us,
            |_| String::new(),
            wall_second,
            second_of,
        );
        let sec9: Vec<&String> = second
            .iter()
            .filter(|l| l.starts_with("WALLGPUSEC at=s9"))
            .collect();
        assert_eq!(sec9.len(), 1, "{second:?}");
        assert!(sec9[0].contains("frames=2 gpu_ms=2.0"), "{}", sec9[0]);
    }
}
