/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 부분 Present PoC (스펙 Task 1). ★HALT 게이트: G2★
//!
//! DXGI FLIP_SEQUENTIAL 합성 스왑체인에서 "부분 Present"(승격된 flip 스왑체인을 유지한 채,
//! 이전에 표시된 버퍼에서 정체(stale) 영역만 복사해 채우고 Present1로 dirty rect만 갱신)가
//! 가능한지 스탠드얼론으로 검증한다. GL/ANGLE/surfman은 전혀 쓰지 않는다 — 순수 D3D11 +
//! DirectComposition. 렌더는 `ClearRenderTargetView` + `CopySubresourceRegion`만 사용(방향
//! 문제 없음, top-left/bottom-left 규약과 무관).
//!
//! 4게이트:
//!   G1: `IDXGIFactory2::CreateSwapChainForComposition` — 640x480 BGRA8 BufferCount=2
//!       FLIP_SEQUENTIAL, DComp 비주얼에 SetContent + Commit.
//!   G2 (★핵심 미지수★): 프레임 A(빨강 전체) Present 후 `GetBuffer(1)`이 성공하고, 그 텍스처를
//!       `CopySubresourceRegion`의 **소스**로 사용해 `GetBuffer(0)`(새 back buffer)에 복사가
//!       실제로 성공하는가. 스테이징 리드백으로 픽셀이 빨강인지 확인. hr 실패 또는 픽셀
//!       불일치 = FAIL → 즉시 종료 코드 2(부분 Present 워크스트림 HALT).
//!   G3: 부분 Present — 프레임 B: 좌반을 파랑 보조 텍스처에서 `CopySubresourceRegion`으로
//!       복사(우반은 G2의 catch-up 복사로 채워진 빨강 유지), `Present1`을 좌반 320x480
//!       dirty rect로 호출. 화면 판정(GetDC(null)+GetPixel): 좌반=파랑, 우반=빨강.
//!   G4: 로테이션 의미론 — 프레임 C(초록 전체) Present 후 `GetBuffer(0)` 스테이징 리드백이
//!       "프레임 B 시점의 버퍼"(파랑+빨강 조합, BufferCount=2 교대 가정)인지 확인. 실측이
//!       이 가정과 다르면 그 자체는 태스크 실패가 아니다 — 측정된 로테이션을 정확히
//!       기록만 한다(Task 4의 stale 부기 "반대 버퍼" 가정을 이 실측에 맞춘다). GetBuffer/Map
//!       자체가 API 레벨에서 실패(읽기 불가/무의미)하는 경우만 진짜 FAIL로 취급한다.
//!
//! 구현 세부(브리프): 각 프레임 사이(A→B, B→C 전환) `sleep(200ms)` + DComp `Commit()`
//! 케이던스. 화면/버퍼 샘플 직전에는 추가 300ms 대기(DWM 합성 반영 여유). 게이트 스코프
//! 안의 모든 실패 분기는 "G{n} FAIL: <이유>"를 출력하고 반환한다.
//!
//! 실행 (PowerShell):
//!   . .\etc\multigpu\servo_env.ps1; $ErrorActionPreference='Continue'
//!   cargo run --release -p servo-paint-api --example dcomp_partial_present_poc --features no-wgl
//!
//! Windows 전용(D3D11/DXGI/DirectComposition은 이 플랫폼에만 존재). `no-wgl`은 이 예제
//! 자체에는 불필요(ANGLE/surfman을 전혀 안 씀)하지만 이 크레이트의 표준 예제 실행 관례를
//! 따라 그대로 전달해도 무해하다.
//!
//! 무결성 규칙(★HALT★): 게이트를 통과시키기 위한 임의 우회 금지. 진짜 플랫폼/API 실패(예:
//! GetBuffer(1)을 복사 소스로 못 씀 = G2)를 억지 판정 강등으로 덮지 않는다. PoC 자체 버그
//! 디버깅은 정상 과정이나, 명세대로 실패하면 FAIL로 보고한다.

#[cfg(target_os = "windows")]
fn main() {
    std::process::exit(poc::run());
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "dcomp_partial_present_poc: Windows 전용 PoC입니다(D3D11/DXGI/DirectComposition). \
         이 빌드에서는 스킵합니다."
    );
    std::process::exit(1);
}

#[cfg(target_os = "windows")]
mod poc {
    use std::mem;
    use std::ptr;
    use std::time::Duration;

    use winapi::Interface;
    use winapi::shared::dxgi::{DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, IDXGIAdapter, IDXGIDevice};
    use winapi::shared::dxgi1_2::{
        DXGI_ALPHA_MODE_IGNORE, DXGI_PRESENT_PARAMETERS, DXGI_SCALING_STRETCH,
        DXGI_SWAP_CHAIN_DESC1, IDXGIFactory2, IDXGISwapChain1,
    };
    use winapi::shared::dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM;
    use winapi::shared::dxgitype::{DXGI_SAMPLE_DESC, DXGI_USAGE_RENDER_TARGET_OUTPUT};
    use winapi::shared::minwindef::{FALSE, HINSTANCE, LPARAM, LRESULT, TRUE, UINT, WPARAM};
    use winapi::shared::windef::{HDC, HWND, POINT, RECT};
    use winapi::um::d3d11::{
        D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
        ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Resource, ID3D11Texture2D,
    };
    use winapi::um::d3dcommon::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0,
        D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
    };
    use winapi::um::dcomp::{
        DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
    };
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::unknwnbase::IUnknown;
    use winapi::um::wingdi::GetPixel;
    use winapi::um::winuser::{
        BringWindowToTop, ClientToScreen, CreateWindowExW, DefWindowProcW, DestroyWindow,
        DispatchMessageW, GetDC, PeekMessageW, RegisterClassW, ReleaseDC, SetForegroundWindow,
        SetProcessDPIAware, ShowWindow, TranslateMessage, UpdateWindow, MSG, PM_REMOVE, SW_SHOW,
        WNDCLASSW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
    };

    /// 창(=DComp 합성 스왑체인 타깃) client 크기·위치. WS_POPUP(테두리 없음)이라
    /// client == 창 크기.
    const WIN_X: i32 = 100;
    const WIN_Y: i32 = 100;
    const WIN_W: i32 = 640;
    const WIN_H: i32 = 480;
    const HALF_W: i32 = WIN_W / 2;

    // 샘플 좌표(경계선 x=320에서 충분히 떨어뜨려 DWM 경계 처리 아티팩트 회피).
    const LEFT_PT: (i32, i32) = (160, 240);
    const RIGHT_PT: (i32, i32) = (480, 240);

    // RGBA(ClearRenderTargetView 입력 순서: R,G,B,A — 텍스처 바이트 순과 무관).
    const RED_RGBA: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
    const GREEN_RGBA: [f32; 4] = [0.0, 1.0, 0.0, 1.0];
    const BLUE_RGBA: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

    // B8G8R8A8 메모리 바이트 순([B,G,R,A])의 기대값(스테이징 리드백 비교용).
    const RED_BGRA: [u8; 4] = [0x00, 0x00, 0xFF, 0xFF];
    const GREEN_BGRA: [u8; 4] = [0x00, 0xFF, 0x00, 0xFF];
    const BLUE_BGRA: [u8; 4] = [0xFF, 0x00, 0x00, 0xFF];

    // COLORREF(0x00BBGGRR, GetPixel 반환값) 형태의 기대값(화면 판정용).
    const RED_CREF: u32 = 0x0000_00FF;
    const BLUE_CREF: u32 = 0x00FF_0000;

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: UINT,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    /// 화면에 보이는 topmost 팝업 창을 만든다(테두리 없음 → client == 창 크기).
    /// GetPixel 판독이 가려지지 않으려면 창이 전경에 떠 있어야 하므로 WS_EX_TOPMOST.
    fn create_visible_window() -> HWND {
        unsafe {
            let hinstance: HINSTANCE = GetModuleHandleW(ptr::null());
            let class_name: Vec<u16> = "ServoDCompPartialPresentPoc\0".encode_utf16().collect();
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: ptr::null_mut(),
                hCursor: ptr::null_mut(),
                hbrBackground: ptr::null_mut(),
                lpszMenuName: ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };
            let atom = RegisterClassW(&wc);
            assert!(atom != 0, "RegisterClassW 실패");
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST,
                class_name.as_ptr(),
                class_name.as_ptr(),
                WS_POPUP | WS_VISIBLE,
                WIN_X,
                WIN_Y,
                WIN_W,
                WIN_H,
                ptr::null_mut(),
                ptr::null_mut(),
                hinstance,
                ptr::null_mut(),
            );
            assert!(!hwnd.is_null(), "CreateWindowExW 실패");
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
            BringWindowToTop(hwnd);
            SetForegroundWindow(hwnd);
            hwnd
        }
    }

    /// 큐에 쌓인 창 메시지를 모두 처리한다(창 realize/전경 반영).
    fn pump_messages(hwnd: HWND) {
        unsafe {
            let mut msg: MSG = mem::zeroed();
            while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// HRESULT(FAILED = 음수) 검사 헬퍼.
    fn hr_check(hr: i32, label: &str) -> Result<(), String> {
        if hr < 0 {
            Err(format!("{label} 실패 (hr=0x{:08x})", hr as u32))
        } else {
            Ok(())
        }
    }

    /// COLORREF(0x00BBGGRR) 채널당 ±24 허용 비교(DWM 컬러 변환 여유).
    fn colors_close(got: u32, want: u32) -> bool {
        let ch = |c: u32, s: u32| (c >> s) & 0xFF;
        let d = |a: u32, b: u32| (a as i32 - b as i32).abs();
        d(ch(got, 0), ch(want, 0)) <= 24 &&
            d(ch(got, 8), ch(want, 8)) <= 24 &&
            d(ch(got, 16), ch(want, 16)) <= 24
    }

    /// B8G8R8A8 스테이징 리드백([B,G,R,A]) 채널당 ±24 허용 비교(알파는 무시 — IGNORE
    /// 모드라 화면 판정과 무관하고, GPU 직접 리드백도 클리어값 그대로라 굳이 안 봐도 됨).
    fn bgra_close(got: [u8; 4], want: [u8; 4]) -> bool {
        let d = |a: u8, b: u8| (a as i32 - b as i32).abs();
        d(got[0], want[0]) <= 24 && d(got[1], want[1]) <= 24 && d(got[2], want[2]) <= 24
    }

    /// client 좌표 (sx, sy)의 화면 픽셀을 GetPixel로 읽는다.
    unsafe fn sample_client_pixel(hwnd: HWND, dc: HDC, sx: i32, sy: i32) -> u32 {
        let mut origin = POINT { x: 0, y: 0 };
        unsafe {
            ClientToScreen(hwnd, &mut origin);
            GetPixel(dc, origin.x + sx, origin.y + sy)
        }
    }

    /// COM 포인터의 RAII 래퍼. Drop에서 IUnknown::Release를 호출한다(모든 COM vtable은
    /// IUnknown 3메서드로 시작하므로 어떤 인터페이스 포인터든 IUnknown으로 재해석해
    /// Release 가능 — dcomp_native_poc.rs와 동일 관례). 이 래퍼 덕분에 이 파일의 모든
    /// 조기 반환(`?`/`return Err(..)`) 경로에서도 그 시점까지 획득한 COM 레퍼런스가
    /// 자동으로 해제된다(수동 release 나열 누락 위험 제거).
    struct ComPtr<T>(*mut T);

    impl<T> ComPtr<T> {
        fn as_raw(&self) -> *mut T {
            self.0
        }
    }

    impl<T> Drop for ComPtr<T> {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    (*(self.0 as *mut IUnknown)).Release();
                }
            }
        }
    }

    /// D3D11_USAGE_STAGING 텍스처로 CopyResource → Map(READ)해 (x, y) 픽셀을 읽는다.
    /// 반환은 B8G8R8A8 메모리 바이트 순 [B, G, R, A]. G2/G4 공용.
    unsafe fn read_pixel(
        device: *mut ID3D11Device,
        context: *mut ID3D11DeviceContext,
        tex: *mut ID3D11Texture2D,
        x: u32,
        y: u32,
    ) -> Result<[u8; 4], String> {
        let mut desc: D3D11_TEXTURE2D_DESC = unsafe { mem::zeroed() };
        unsafe {
            (*tex).GetDesc(&mut desc);
        }
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: desc.Width,
            Height: desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: desc.SampleDesc,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ,
            MiscFlags: 0,
        };
        let mut staging_raw: *mut ID3D11Texture2D = ptr::null_mut();
        hr_check(
            unsafe { (*device).CreateTexture2D(&staging_desc, ptr::null(), &mut staging_raw) },
            "CreateTexture2D(staging)",
        )?;
        let staging = ComPtr(staging_raw);
        unsafe {
            (*context).CopyResource(staging.as_raw() as *mut ID3D11Resource, tex as *mut ID3D11Resource);
        }
        let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { mem::zeroed() };
        let map_hr = unsafe {
            (*context).Map(
                staging.as_raw() as *mut ID3D11Resource,
                0,
                D3D11_MAP_READ,
                0,
                &mut mapped,
            )
        };
        if map_hr < 0 {
            // `staging`은 이 함수 반환 시 자동 Drop → Release(위 ComPtr 참조).
            return Err(format!("Map(staging) 실패 (hr=0x{:08x})", map_hr as u32));
        }
        let mut px = [0u8; 4];
        unsafe {
            let row = mapped.pData as *const u8;
            let offset = (y as isize) * (mapped.RowPitch as isize) + (x as isize) * 4;
            ptr::copy_nonoverlapping(row.offset(offset), px.as_mut_ptr(), 4);
            (*context).Unmap(staging.as_raw() as *mut ID3D11Resource, 0);
        }
        Ok(px)
    }

    /// width x height 단색 D3D11_USAGE_DEFAULT + BIND_RENDER_TARGET 텍스처를 만들어
    /// ClearRenderTargetView로 채운다(G3의 좌반 보조 텍스처용).
    unsafe fn make_solid_texture(
        device: *mut ID3D11Device,
        context: *mut ID3D11DeviceContext,
        width: u32,
        height: u32,
        rgba: [f32; 4],
    ) -> Result<ComPtr<ID3D11Texture2D>, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex_raw: *mut ID3D11Texture2D = ptr::null_mut();
        hr_check(
            unsafe { (*device).CreateTexture2D(&desc, ptr::null(), &mut tex_raw) },
            "CreateTexture2D(helper)",
        )?;
        let tex = ComPtr(tex_raw);
        let mut rtv_raw: *mut ID3D11RenderTargetView = ptr::null_mut();
        hr_check(
            unsafe {
                (*device).CreateRenderTargetView(tex.as_raw() as *mut ID3D11Resource, ptr::null(), &mut rtv_raw)
            },
            "CreateRenderTargetView(helper)",
        )?;
        let rtv = ComPtr(rtv_raw);
        unsafe {
            (*context).ClearRenderTargetView(rtv.as_raw(), &rgba);
            (*context).Flush();
        }
        // `rtv`는 여기서 스코프 종료로 자동 Drop → Release.
        Ok(tex)
    }

    /// GetBuffer(index)를 ID3D11Texture2D로 가져온다.
    unsafe fn get_buffer(
        swapchain: *mut IDXGISwapChain1,
        index: u32,
    ) -> Result<ComPtr<ID3D11Texture2D>, String> {
        let mut tex_raw: *mut ID3D11Texture2D = ptr::null_mut();
        hr_check(
            unsafe {
                (*swapchain).GetBuffer(
                    index,
                    &ID3D11Texture2D::uuidof(),
                    &mut tex_raw as *mut _ as *mut _,
                )
            },
            &format!("GetBuffer({index})"),
        )?;
        Ok(ComPtr(tex_raw))
    }

    /// 종료 코드 결정을 위한 실패 분류. Halt2 = G2 FAIL(브리프 규정: 즉시 종료 코드 2).
    /// General = 그 외 진짜 실패(G1 부트스트랩 또는 G3 HRESULT/화면 판정).
    enum PocError {
        Halt2(String),
        General(String),
    }

    /// 게이트 스코프 내 실패 공통 처리(플랜 요구: 게이트 스코프 안의 **모든** 실패 분기는
    /// "G{n} FAIL: <이유>" 라인을 찍고 반환한다). General 계열(G1/G3/G4 스코프)용.
    fn gate_fail(gate: u32, msg: String) -> PocError {
        println!("G{gate} FAIL: {msg}");
        PocError::General(format!("G{gate} FAIL: {msg}"))
    }

    /// G2 스코프 내 실패 공통 처리: "G2 FAIL: <이유>" 출력 후 Halt2(브리프 규정: 즉시
    /// 종료 코드 2). 프레임 A 준비(렌더/Present)도 브리프 G2 정의("프레임 A(빨강 전체
    /// 클리어) Present 후…")의 스코프이므로 이 분류를 쓴다 — G1을 통과한 스왑체인에
    /// 렌더/Present조차 안 되면 부분 Present 실현성 검증 자체가 불가능하므로 HALT
    /// 의미론과도 부합한다.
    fn g2_fail(msg: String) -> PocError {
        println!("G2 FAIL: {msg}");
        PocError::Halt2(msg)
    }

    pub fn run() -> i32 {
        env_logger::init();
        unsafe {
            SetProcessDPIAware();
        }

        let hwnd = create_visible_window();
        pump_messages(hwnd);

        let result = unsafe { run_poc(hwnd) };
        pump_messages(hwnd);

        let exit_code = match result {
            Ok(()) => {
                println!("\n결과: PoC 실행 완료(위 G1~G4 판정 참조).");
                0
            },
            Err(PocError::Halt2(msg)) => {
                eprintln!("\n결과: G2 FAIL — {msg}");
                eprintln!(
                    "★HALT: G2(FLIP_SEQUENTIAL 합성 스왑체인에서 GetBuffer(1)을 복사 소스로 \
                     사용하는 것) 실패 — 부분 Present 워크스트림 폐기. Task 4 스킵, Task 5가 \
                     유일 경로. 컨트롤러에 보고."
                );
                2
            },
            Err(PocError::General(msg)) => {
                eprintln!("\n결과: FAIL — {msg}");
                1
            },
        };

        unsafe {
            DestroyWindow(hwnd);
        }
        exit_code
    }

    unsafe fn run_poc(hwnd: HWND) -> Result<(), PocError> {
        // --- D3D11 디바이스(BGRA 지원) ---
        let feature_levels = [
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ];
        let mut device_raw: *mut ID3D11Device = ptr::null_mut();
        let mut context_raw: *mut ID3D11DeviceContext = ptr::null_mut();
        let mut feature_level: D3D_FEATURE_LEVEL = unsafe { mem::zeroed() };
        hr_check(
            unsafe {
                D3D11CreateDevice(
                    ptr::null_mut(),
                    D3D_DRIVER_TYPE_HARDWARE,
                    ptr::null_mut(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    feature_levels.as_ptr(),
                    feature_levels.len() as u32,
                    D3D11_SDK_VERSION,
                    &mut device_raw,
                    &mut feature_level,
                    &mut context_raw,
                )
            },
            "D3D11CreateDevice",
        )
        .map_err(PocError::General)?;
        // 이 지점부터 `device`/`context`는 RAII — 이후 어떤 조기 반환 경로를 타도 Release됨.
        let device = ComPtr(device_raw);
        let context = ComPtr(context_raw);

        // --- DXGI 팩토리(디바이스의 어댑터 체인에서 획득 — CreateSwapChainForComposition
        //     권장 경로) + DComp 디바이스(같은 IDXGIDevice 재사용) ---
        let mut dxgi_device_raw: *mut IDXGIDevice = ptr::null_mut();
        hr_check(
            unsafe {
                (*device.as_raw())
                    .QueryInterface(&IDXGIDevice::uuidof(), &mut dxgi_device_raw as *mut _ as *mut _)
            },
            "QI IDXGIDevice",
        )
        .map_err(PocError::General)?;
        let dxgi_device = ComPtr(dxgi_device_raw);

        let mut adapter_raw: *mut IDXGIAdapter = ptr::null_mut();
        hr_check(
            unsafe { (*dxgi_device.as_raw()).GetAdapter(&mut adapter_raw) },
            "GetAdapter",
        )
        .map_err(PocError::General)?;
        let adapter = ComPtr(adapter_raw);

        let mut factory2_raw: *mut IDXGIFactory2 = ptr::null_mut();
        hr_check(
            unsafe {
                (*adapter.as_raw())
                    .GetParent(&IDXGIFactory2::uuidof(), &mut factory2_raw as *mut _ as *mut _)
            },
            "adapter.GetParent(IDXGIFactory2)",
        )
        .map_err(PocError::General)?;
        let factory2 = ComPtr(factory2_raw);

        let mut dcomp_raw: *mut IDCompositionDevice = ptr::null_mut();
        hr_check(
            unsafe {
                DCompositionCreateDevice(
                    dxgi_device.as_raw(),
                    &IDCompositionDevice::uuidof(),
                    &mut dcomp_raw as *mut _ as *mut _,
                )
            },
            "DCompositionCreateDevice",
        )
        .map_err(PocError::General)?;
        let dcomp = ComPtr(dcomp_raw);

        // --- G1: CreateSwapChainForComposition + DComp 비주얼 + Commit ---
        let desc1 = DXGI_SWAP_CHAIN_DESC1 {
            Width: WIN_W as u32,
            Height: WIN_H as u32,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: FALSE,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };
        let mut swapchain_raw: *mut IDXGISwapChain1 = ptr::null_mut();
        let g1_hr = unsafe {
            (*factory2.as_raw()).CreateSwapChainForComposition(
                device.as_raw() as *mut IUnknown,
                &desc1,
                ptr::null_mut(),
                &mut swapchain_raw,
            )
        };
        if g1_hr < 0 || swapchain_raw.is_null() {
            let msg = if g1_hr < 0 {
                format!("CreateSwapChainForComposition 실패 (hr=0x{:08x})", g1_hr as u32)
            } else {
                "CreateSwapChainForComposition hr=S_OK인데 포인터가 null".to_string()
            };
            println!("G1 FAIL: {msg}");
            return Err(PocError::General(format!("G1 FAIL: {msg}")));
        }
        let swapchain = ComPtr(swapchain_raw);

        // 이하 DComp 타깃/비주얼/SetContent/Commit은 브리프가 G1 정의에 포함시킨
        // 단계들("visual.SetContent(swapchain) + Commit") — 실패 시 전부 "G1 FAIL:" 출력.
        let mut target_raw: *mut IDCompositionTarget = ptr::null_mut();
        hr_check(
            unsafe { (*dcomp.as_raw()).CreateTargetForHwnd(hwnd, TRUE, &mut target_raw) },
            "CreateTargetForHwnd",
        )
        .map_err(|m| gate_fail(1, m))?;
        let target = ComPtr(target_raw);

        let mut root_raw: *mut IDCompositionVisual = ptr::null_mut();
        hr_check(unsafe { (*dcomp.as_raw()).CreateVisual(&mut root_raw) }, "CreateVisual(root)")
            .map_err(|m| gate_fail(1, m))?;
        let root = ComPtr(root_raw);
        hr_check(unsafe { (*target.as_raw()).SetRoot(root.as_raw()) }, "SetRoot")
            .map_err(|m| gate_fail(1, m))?;

        let mut visual_raw: *mut IDCompositionVisual = ptr::null_mut();
        hr_check(unsafe { (*dcomp.as_raw()).CreateVisual(&mut visual_raw) }, "CreateVisual(content)")
            .map_err(|m| gate_fail(1, m))?;
        let visual = ComPtr(visual_raw);
        hr_check(
            unsafe { (*visual.as_raw()).SetContent(swapchain.as_raw() as *mut IUnknown) },
            "SetContent(swapchain)",
        )
        .map_err(|m| gate_fail(1, m))?;
        hr_check(
            unsafe { (*root.as_raw()).AddVisual(visual.as_raw(), TRUE, ptr::null_mut()) },
            "AddVisual",
        )
        .map_err(|m| gate_fail(1, m))?;
        hr_check(unsafe { (*dcomp.as_raw()).Commit() }, "Commit(G1)")
            .map_err(|m| gate_fail(1, m))?;
        println!(
            "G1 PASS: CreateSwapChainForComposition {WIN_W}x{WIN_H} BGRA8 BufferCount=2 \
             FLIP_SEQUENTIAL/SCALING_STRETCH/ALPHA_IGNORE + SetContent + Commit"
        );

        // --- 프레임 A: 전체 빨강 → Present (브리프 G2 정의 "프레임 A Present 후…"의
        //     준비 단계 = G2 스코프 — 실패 시 "G2 FAIL:" 출력 + Halt2) ---
        {
            let buf0 = unsafe { get_buffer(swapchain.as_raw(), 0) }
                .map_err(|m| g2_fail(format!("프레임 A 준비 — {m}")))?;
            let mut rtv_raw: *mut ID3D11RenderTargetView = ptr::null_mut();
            hr_check(
                unsafe {
                    (*device.as_raw()).CreateRenderTargetView(
                        buf0.as_raw() as *mut ID3D11Resource,
                        ptr::null(),
                        &mut rtv_raw,
                    )
                },
                "CreateRenderTargetView(frameA)",
            )
            .map_err(|m| g2_fail(format!("프레임 A 준비 — {m}")))?;
            let rtv = ComPtr(rtv_raw);
            unsafe {
                (*context.as_raw()).ClearRenderTargetView(rtv.as_raw(), &RED_RGBA);
                (*context.as_raw()).Flush();
            }
            drop(rtv);
            hr_check(unsafe { (*swapchain.as_raw()).Present(1, 0) }, "Present(frameA)")
                .map_err(|m| g2_fail(format!("프레임 A 준비 — {m}")))?;
            // `buf0`는 이 블록 종료로 자동 Drop.
        }

        // 프레임 전환 케이던스(브리프 구현 세부: "각 프레임 사이 sleep(200ms) + Commit") —
        // 프레임 A → 프레임 B(G2 작업) 전환. 이 Commit은 게이트 판정 대상이 아닌 케이던스
        // 단계이므로(브리프의 게이트 정의 밖 구현 세부) 실패 시 게이트 접두사 없이 일반
        // 실패로 보고한다.
        std::thread::sleep(Duration::from_millis(200));
        hr_check(unsafe { (*dcomp.as_raw()).Commit() }, "Commit(프레임 A→B 전환)")
            .map_err(PocError::General)?;
        pump_messages(hwnd);

        // --- G2: GetBuffer(1)(=프레임 A 이력) → CopySubresourceRegion 소스로 GetBuffer(0)에
        //     복사 → 스테이징 리드백으로 빨강 확인 ---
        let buf1 = match unsafe { get_buffer(swapchain.as_raw(), 1) } {
            Ok(t) => t,
            Err(msg) => return Err(g2_fail(format!("GetBuffer(1) 실패 — {msg}"))),
        };
        // 진단: GetBuffer(1) 자체가 프레임 A 내용을 갖고 있는지 직접 확인(복사 이전).
        let buf1_direct = unsafe {
            read_pixel(device.as_raw(), context.as_raw(), buf1.as_raw(), RIGHT_PT.0 as u32, RIGHT_PT.1 as u32)
        };
        match &buf1_direct {
            Ok(px) => println!(
                "  [진단] GetBuffer(1) 직접 리드백(프레임 A 이력): got={px:02x?} want~{RED_BGRA:02x?}"
            ),
            Err(msg) => println!("  [진단] GetBuffer(1) 직접 리드백 실패: {msg}"),
        }

        let buf0_epoch2 = match unsafe { get_buffer(swapchain.as_raw(), 0) } {
            Ok(t) => t,
            Err(msg) => return Err(g2_fail(format!("GetBuffer(0) 실패(복사 대상) — {msg}"))),
        };
        unsafe {
            (*context.as_raw()).CopySubresourceRegion(
                buf0_epoch2.as_raw() as *mut ID3D11Resource,
                0,
                0,
                0,
                0,
                buf1.as_raw() as *mut ID3D11Resource,
                0,
                ptr::null(),
            );
            (*context.as_raw()).Flush();
        }
        let g2_left = unsafe {
            read_pixel(device.as_raw(), context.as_raw(), buf0_epoch2.as_raw(), LEFT_PT.0 as u32, LEFT_PT.1 as u32)
        };
        let g2_right = unsafe {
            read_pixel(device.as_raw(), context.as_raw(), buf0_epoch2.as_raw(), RIGHT_PT.0 as u32, RIGHT_PT.1 as u32)
        };
        drop(buf1);
        let g2_pass = matches!(&g2_left, Ok(px) if bgra_close(*px, RED_BGRA)) &&
            matches!(&g2_right, Ok(px) if bgra_close(*px, RED_BGRA));
        println!(
            "  GetBuffer(0) post-copy readback: left={g2_left:02x?} right={g2_right:02x?} want~{RED_BGRA:02x?}"
        );
        if !g2_pass {
            return Err(g2_fail(
                "CopySubresourceRegion(GetBuffer(1) -> GetBuffer(0)) 결과가 빨강이 아님 — \
                 GetBuffer(1)을 복사 소스로 사용한 결과가 기대와 다름"
                    .to_string(),
            ));
        }
        println!("G2 PASS: GetBuffer(1) 읽기 가능 + CopySubresourceRegion 소스로 사용해 GetBuffer(0)에 복사 성공");

        // --- G3: 부분 Present — 좌반만 파랑으로 갱신(buf0_epoch2는 G2에서 이미 전체 빨강) ---
        let helper = match unsafe {
            make_solid_texture(device.as_raw(), context.as_raw(), HALF_W as u32, WIN_H as u32, BLUE_RGBA)
        } {
            Ok(t) => t,
            Err(msg) => {
                return Err(gate_fail(3, format!("보조 텍스처 생성 실패 — {msg}")));
            },
        };
        unsafe {
            (*context.as_raw()).CopySubresourceRegion(
                buf0_epoch2.as_raw() as *mut ID3D11Resource,
                0,
                0,
                0,
                0,
                helper.as_raw() as *mut ID3D11Resource,
                0,
                ptr::null(),
            );
            (*context.as_raw()).Flush();
        }
        drop(helper);
        drop(buf0_epoch2);

        let mut dirty_rect = RECT {
            left: 0,
            top: 0,
            right: HALF_W,
            bottom: WIN_H,
        };
        let present_params = DXGI_PRESENT_PARAMETERS {
            DirtyRectsCount: 1,
            pDirtyRects: &mut dirty_rect,
            pScrollRect: ptr::null_mut(),
            pScrollOffset: ptr::null_mut(),
        };
        let present1_hr = unsafe { (*swapchain.as_raw()).Present1(0, 0, &present_params) };
        if present1_hr < 0 {
            println!("G3 FAIL: Present1(dirty rect) 실패 (hr=0x{:08x})", present1_hr as u32);
            return Err(PocError::General(format!(
                "Present1(dirty rect) 실패 (hr=0x{:08x})",
                present1_hr as u32
            )));
        }

        std::thread::sleep(Duration::from_millis(300));
        pump_messages(hwnd);
        let dc = unsafe { GetDC(ptr::null_mut()) };
        let screen_left = unsafe { sample_client_pixel(hwnd, dc, LEFT_PT.0, LEFT_PT.1) };
        let screen_right = unsafe { sample_client_pixel(hwnd, dc, RIGHT_PT.0, RIGHT_PT.1) };
        unsafe {
            ReleaseDC(ptr::null_mut(), dc);
        }
        println!(
            "  화면 판정: left=0x{screen_left:06x}(want~0x{BLUE_CREF:06x}=blue) right=0x{screen_right:06x}(want~0x{RED_CREF:06x}=red)"
        );
        let g3_pass = colors_close(screen_left, BLUE_CREF) && colors_close(screen_right, RED_CREF);
        if !g3_pass {
            println!("G3 FAIL: 화면 픽셀이 기대(좌=파랑/우=빨강)와 불일치");
            return Err(PocError::General(
                "G3 FAIL: Present1 dirty rect 부분 갱신이 화면에 기대대로 반영되지 않음"
                    .to_string(),
            ));
        }
        println!("G3 PASS: Present1(dirty rect 320x480) 부분 갱신이 화면에 반영됨(좌=파랑/우=빨강)");

        // 프레임 전환 케이던스(브리프 구현 세부): 프레임 B → 프레임 C 전환.
        // (G3 화면 판정은 위에서 이미 완료 — 여기는 다음 프레임으로 넘어가기 전 케이던스.)
        std::thread::sleep(Duration::from_millis(200));
        hr_check(unsafe { (*dcomp.as_raw()).Commit() }, "Commit(프레임 B→C 전환)")
            .map_err(PocError::General)?;
        pump_messages(hwnd);

        // --- 프레임 C: 전체 초록 → Present(로테이션 관찰용) ---
        let buf0_epoch3 = match unsafe { get_buffer(swapchain.as_raw(), 0) } {
            Ok(t) => t,
            Err(msg) => {
                println!("G4 FAIL: 프레임 C용 GetBuffer(0) 실패(unreadable) — {msg}");
                return Err(PocError::General(format!(
                    "G4: 프레임 C용 GetBuffer(0) 실패 — {msg}"
                )));
            },
        };
        {
            let mut rtv_raw: *mut ID3D11RenderTargetView = ptr::null_mut();
            hr_check(
                unsafe {
                    (*device.as_raw()).CreateRenderTargetView(
                        buf0_epoch3.as_raw() as *mut ID3D11Resource,
                        ptr::null(),
                        &mut rtv_raw,
                    )
                },
                "CreateRenderTargetView(frameC)",
            )
            .map_err(|m| gate_fail(4, format!("프레임 C 준비 — {m}")))?;
            let rtv = ComPtr(rtv_raw);
            unsafe {
                (*context.as_raw()).ClearRenderTargetView(rtv.as_raw(), &GREEN_RGBA);
                (*context.as_raw()).Flush();
            }
            // `rtv`는 이 내부 블록 종료로 자동 Drop.
        }
        drop(buf0_epoch3);
        let present_c_hr = unsafe { (*swapchain.as_raw()).Present(1, 0) };
        if present_c_hr < 0 {
            println!("G4 FAIL: 프레임 C Present 실패(unreadable) (hr=0x{:08x})", present_c_hr as u32);
            return Err(PocError::General(format!(
                "G4: 프레임 C Present 실패 (hr=0x{:08x})",
                present_c_hr as u32
            )));
        }
        std::thread::sleep(Duration::from_millis(300));
        pump_messages(hwnd);

        // --- G4: 프레임 C Present 후 GetBuffer(0) 스테이징 리드백 — 로테이션 의미론 관찰 ---
        let buf0_post_c = match unsafe { get_buffer(swapchain.as_raw(), 0) } {
            Ok(t) => t,
            Err(msg) => {
                println!("G4 FAIL: 프레임 C 이후 GetBuffer(0) 실패(unreadable) — {msg}");
                return Err(PocError::General(format!(
                    "G4: 프레임 C 이후 GetBuffer(0) 실패 — {msg}"
                )));
            },
        };
        let g4_left = unsafe {
            read_pixel(device.as_raw(), context.as_raw(), buf0_post_c.as_raw(), LEFT_PT.0 as u32, LEFT_PT.1 as u32)
        };
        let g4_right = unsafe {
            read_pixel(device.as_raw(), context.as_raw(), buf0_post_c.as_raw(), RIGHT_PT.0 as u32, RIGHT_PT.1 as u32)
        };
        drop(buf0_post_c);
        match (&g4_left, &g4_right) {
            (Ok(l), Ok(r)) => {
                println!("  프레임 C Present 후 GetBuffer(0) 리드백: left={l:02x?} right={r:02x?}");
                let matches_frame_b = bgra_close(*l, BLUE_BGRA) && bgra_close(*r, RED_BGRA);
                let matches_frame_c = bgra_close(*l, GREEN_BGRA) && bgra_close(*r, GREEN_BGRA);
                let matches_frame_a = bgra_close(*l, RED_BGRA) && bgra_close(*r, RED_BGRA);
                if matches_frame_b {
                    println!(
                        "G4 PASS: GetBuffer(0)이 '프레임 B 시점의 버퍼'(파랑+빨강)를 보유 — \
                         BufferCount=2 교대(2-프레임 전 버퍼 재사용) 가정과 일치"
                    );
                } else {
                    let observed = if matches_frame_c {
                        "프레임 C 내용(초록 단색) — 즉 GetBuffer(0)가 방금 Present한 버퍼 자체를 \
                         가리킴(단순 재사용/미교대 로테이션)"
                    } else if matches_frame_a {
                        "프레임 A 내용(빨강 단색) — 2단계가 아닌 다른 교대 주기"
                    } else {
                        "위 어느 가설과도 불일치 — 값 자체는 읽혔으나(API 실패 아님) 패턴 불명"
                    };
                    println!(
                        "G4 FAIL: 로테이션 의미론이 예상(프레임 B 파랑+빨강)과 다름 — 실측: {observed}. \
                         (참고: 이 값 자체가 읽기 불가/무의미한 것은 아니므로 태스크 자체의 하드 실패는 \
                         아님 — Task 4에서 부기 방향을 이 실측에 맞춘다.)"
                    );
                }
            },
            _ => {
                println!(
                    "G4 FAIL: GetBuffer(0) 리드백 자체가 실패(unreadable) — left={g4_left:?} right={g4_right:?}"
                );
            },
        }

        // 정리: device/context/dxgi_device/adapter/factory2/dcomp/swapchain/target/root/visual은
        // 전부 ComPtr(RAII) — 이 함수가 어떤 경로로 반환되든(성공/조기 Err 포함) 여기서
        // 스코프 종료 시 자동으로 Release된다. 명시적 release 나열이 필요 없다.
        Ok(())
    }
}
