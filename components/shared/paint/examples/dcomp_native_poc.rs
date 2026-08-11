/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! DComp Native Compositor PoC 게이트 (스펙 §8-1). ★HALT 게이트★
//!
//! WR(WebRender)의 Native Compositor를 DirectComposition으로 구현하기 전에, 핵심 가정
//! 4개를 스탠드얼론으로 검증한다:
//!   G1: ANGLE의 D3D11 디바이스로 DComp 디바이스/타깃/루트 비주얼 생성 가능
//!   G2: `IDCompositionVirtualSurface::BeginDraw`가 준 텍스처를 EGL pbuffer로 래핑 가능
//!       (최대 리스크)
//!   G3: GL로 그린 내용이 Commit 후 화면에 보임 (Commit 경로 성립)
//!   G4: top-left 방향 정합 (좌상에 그린 게 화면 좌상에)
//!
//! 각 게이트 PASS/FAIL을 출력하고, 4/4 PASS일 때만 exit 0.
//!
//! 실행 (PowerShell):
//!   . .\etc\multigpu\servo_env.ps1; $ErrorActionPreference='Continue'
//!   cargo run --release -p servo-paint-api --example dcomp_native_poc --features no-wgl
//!
//! Windows + `no-wgl`(ANGLE) 빌드 전용 — DComp 인터롭 메서드
//! (`angle_d3d11_device_ptr`/`create_render_pbuffer_from_d3d_texture`/
//! `make_render_pbuffer_current`/`window_hwnd`)가 이 조합에서만 실제 구현을 갖는다.
//!
//! ★필수 전제: ANGLE의 기본 창 서피스는 `EGL_DIRECT_COMPOSITION_ANGLE=TRUE`로 만들어져
//! ANGLE이 HWND에 자체 DComp 타깃을 붙인다. 그 상태로 우리가 `CreateTargetForHwnd`를
//! 호출하면 `DCOMPOSITION_ERROR_WINDOW_ALREADY_COMPOSED(0x88980800)`로 실패한다
//! (타깃은 (hwnd, topmost)당 1개). 그래서 이 PoC는 surfman 창 서피스를 "평범한 HWND
//! 서피스"로 만들도록 `surfman::set_dcomp_native_compositor(true)`(구
//! `SERVO_COMPOSITOR_DCOMP=1`, Task 1 opt-out — 3 상태 문자열 파싱은
//! `paint_api::rendering_context::DcompMode::parse` 한 곳으로 모였고, surfman은 그 결과인
//! 불리언만 받는다. 이 PoC는 pref 계층을 거치지 않으므로 그 불리언을 직접 주입한다)를
//! run() 진입 즉시, WindowRenderingContext 생성 전에 호출한다. 이는 실제 WR 네이티브
//! 컴포지터가 쓰는 구성과 동일하다(우회가 아니라 설계된 경로).
//!
//! 무결성 규칙(★HALT★): 게이트를 통과시키기 위한 임의 우회 금지. 특히 진짜 플랫폼/API
//! 실패(예: pbuffer 래핑 불가 = G2, 방향 정합 불가 = G4)를 억지 y-flip/판정 강등으로
//! 덮지 않는다. PoC 자체 버그(오버로드 이름/좌표 산식/flush 누락) 디버깅은 정상 과정이나,
//! 명세대로 실패하면 FAIL로 보고한다.

#[cfg(all(target_os = "windows", feature = "no-wgl"))]
fn main() {
    std::process::exit(poc::run());
}

#[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
fn main() {
    eprintln!(
        "dcomp_native_poc: Windows + `no-wgl`(ANGLE) 조합 전용 PoC입니다 — DComp 인터롭 \
         메서드가 cfg(all(target_os = \"windows\", feature = \"no-wgl\"))로 게이트되어 다른 \
         빌드 설정에서는 전부 None/no-op입니다. 이 빌드에서는 스킵합니다."
    );
    std::process::exit(1);
}

#[cfg(all(target_os = "windows", feature = "no-wgl"))]
mod poc {
    use std::mem;
    use std::ptr;

    use dpi::PhysicalSize;
    use euclid::Size2D;
    use gleam::gl;
    use paint_api::rendering_context::{RenderingContext, WindowRenderingContext};
    use raw_window_handle::{
        DisplayHandle, RawWindowHandle, Win32WindowHandle, WindowHandle,
    };
    use winapi::Interface;
    use winapi::shared::dxgi::IDXGIDevice;
    use winapi::shared::dxgi1_2::DXGI_ALPHA_MODE_IGNORE;
    use winapi::shared::dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM;
    use winapi::shared::minwindef::{HINSTANCE, LPARAM, LRESULT, TRUE, UINT, WPARAM};
    use winapi::shared::windef::{HWND, POINT, RECT};
    use winapi::um::d3d11::{D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11Texture2D};
    use winapi::um::dcomp::{
        DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget,
        IDCompositionVirtualSurface, IDCompositionVisual,
    };
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::unknwnbase::IUnknown;
    use winapi::um::wingdi::GetPixel;
    use winapi::um::winuser::{
        BringWindowToTop, ClientToScreen, CreateWindowExW, DefWindowProcW, DestroyWindow,
        DispatchMessageW, GetClientRect, GetDC, PeekMessageW, RegisterClassW, ReleaseDC,
        SetForegroundWindow, SetProcessDPIAware, ShowWindow, TranslateMessage, UpdateWindow, MSG,
        PM_REMOVE, SW_SHOW, WNDCLASSW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
    };

    /// 창(=DComp 서피스) 요청 크기. 실제 판정은 GetClientRect로 얻는 실측 client 크기를
    /// 사용한다(DPI 안전).
    const SIZE: i32 = 512;

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
            let class_name: Vec<u16> = "ServoDCompNativePoc\0".encode_utf16().collect();
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
                100,
                100,
                SIZE,
                SIZE,
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

    /// COLORREF(0x00BBGGRR) 채널당 ±16 허용 비교(DWM 컬러 변환 여유).
    fn colors_close(got: u32, want: u32) -> bool {
        let ch = |c: u32, s: u32| (c >> s) & 0xFF;
        let d = |a: u32, b: u32| (a as i32 - b as i32).abs();
        d(ch(got, 0), ch(want, 0)) <= 16 &&
            d(ch(got, 8), ch(want, 8)) <= 16 &&
            d(ch(got, 16), ch(want, 16)) <= 16
    }

    /// 창의 실측 client 크기(물리 픽셀).
    fn client_size(hwnd: HWND) -> (i32, i32) {
        unsafe {
            let mut rc: RECT = mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            (rc.right - rc.left, rc.bottom - rc.top)
        }
    }

    /// client 좌표 (sx, sy)의 화면 픽셀을 GetPixel로 읽는다.
    unsafe fn sample_client_pixel(hwnd: HWND, dc: winapi::shared::windef::HDC, sx: i32, sy: i32) -> u32 {
        let mut origin = POINT { x: 0, y: 0 };
        unsafe {
            ClientToScreen(hwnd, &mut origin);
            GetPixel(dc, origin.x + sx, origin.y + sy)
        }
    }

    pub fn run() -> i32 {
        env_logger::init();

        // ★ANGLE 기본 창 서피스는 자체 DComp 타깃을 HWND에 붙이므로(§헤더 주석), Task 1
        // opt-out을 켜서 surfman이 평범한 HWND 서피스를 만들게 한다. 반드시
        // WindowRenderingContext 생성(=surfman create_window_surface가 이 값을 봄) 전에
        // 주입해야 한다. 이 PoC는 servo_config/paint_api의 pref 파싱 계층 없이 도는 유일한
        // 경로라, `gfx.dcomp.mode` 문자열이 아니라 surfman의 저수준 불리언 게이트를 직접
        // 켠다(리뷰 결과 — 3 상태 파싱은 paint_api로 옮겼고, surfman은 더 이상 그 파싱을
        // 모른다. 이 PoC는 애초에 Hybrid/SurfaceOnly 구분과 무관하므로 그 구분 없이 곧장
        // true를 주입한다. 옛 SERVO_COMPOSITOR_DCOMP=1과 동일한 효과).
        surfman::set_dcomp_native_compositor(true);

        // DPI 가상화로 client 픽셀 좌표가 스케일되지 않도록 프로세스를 DPI-aware로.
        unsafe {
            SetProcessDPIAware();
        }

        let hwnd = create_visible_window();
        pump_messages(hwnd);

        let display_handle = DisplayHandle::windows();
        let raw = {
            let nz = std::num::NonZeroIsize::new(hwnd as isize).expect("HWND가 0");
            RawWindowHandle::Win32(Win32WindowHandle::new(nz))
        };
        let window_handle = unsafe { WindowHandle::borrow_raw(raw) };

        let rc = match WindowRenderingContext::new(
            display_handle,
            window_handle,
            PhysicalSize::new(SIZE as u32, SIZE as u32),
        ) {
            Ok(rc) => rc,
            Err(error) => {
                eprintln!(
                    "WindowRenderingContext::new 실패: {error:?} — 하드웨어 ANGLE 디바이스를 \
                     얻지 못함 (RUST_LOG=warn로 재실행해 EGL/DXGI 진단 확인)"
                );
                unsafe {
                    DestroyWindow(hwnd);
                }
                return 1;
            },
        };

        if let Err(error) = rc.make_current() {
            eprintln!("make_current 실패: {error:?}");
            unsafe {
                DestroyWindow(hwnd);
            }
            return 1;
        }

        let result = unsafe { run_poc(&rc, hwnd) };
        pump_messages(hwnd);

        let exit_code = match result {
            Ok(()) => {
                println!("\n결과: 4/4 PASS — DComp Native Compositor 핵심 가정 검증 완료.");
                0
            },
            Err(error) => {
                eprintln!("\n결과: FAIL — {error}");
                eprintln!("★HALT: 이후 태스크 진행 금지. 위 게이트 출력을 첨부해 방향 문의.");
                1
            },
        };

        unsafe {
            DestroyWindow(hwnd);
        }
        exit_code
    }

    unsafe fn run_poc(rc: &WindowRenderingContext, hwnd: HWND) -> Result<(), String> {
        // 실측 client 크기 사용(WS_POPUP + DPI-aware라 SIZE와 같아야 하지만 안전하게).
        let (cw, ch) = client_size(hwnd);
        if cw <= 1 || ch <= 1 {
            return Err(format!("client 크기가 비정상: {cw}x{ch}"));
        }
        println!("창 client 크기: {cw}x{ch} (요청 {SIZE}x{SIZE})");

        // --- G1: DComp 디바이스 + 타깃 + 루트 비주얼 ---
        let hwnd_from_rc = rc
            .window_hwnd()
            .ok_or("window_hwnd()가 None — WindowRenderingContext에 HWND 없음")?;
        if hwnd_from_rc != hwnd as usize {
            return Err(format!(
                "window_hwnd() 불일치: rc={hwnd_from_rc:#x} vs 실제 {:#x}",
                hwnd as usize
            ));
        }
        let d3d = rc
            .angle_d3d11_device_ptr()
            .ok_or("angle_d3d11_device_ptr()가 None — ANGLE D3D11 디바이스 없음")?
            as *mut ID3D11Device;

        let mut dxgi: *mut IDXGIDevice = ptr::null_mut();
        hr_check(
            unsafe {
                (*d3d).QueryInterface(
                    &IDXGIDevice::uuidof(),
                    &mut dxgi as *mut _ as *mut _,
                )
            },
            "QI IDXGIDevice",
        )?;

        let mut dcomp: *mut IDCompositionDevice = ptr::null_mut();
        hr_check(
            unsafe {
                DCompositionCreateDevice(
                    dxgi,
                    &IDCompositionDevice::uuidof(),
                    &mut dcomp as *mut _ as *mut _,
                )
            },
            "DCompositionCreateDevice",
        )?;

        let mut target: *mut IDCompositionTarget = ptr::null_mut();
        hr_check(
            unsafe { (*dcomp).CreateTargetForHwnd(hwnd, TRUE, &mut target) },
            "CreateTargetForHwnd",
        )?;

        let mut root: *mut IDCompositionVisual = ptr::null_mut();
        hr_check(unsafe { (*dcomp).CreateVisual(&mut root) }, "CreateVisual(root)")?;
        hr_check(unsafe { (*target).SetRoot(root) }, "SetRoot")?;
        println!("G1 PASS: DComp device/target/root visual (hwnd={hwnd:?})");

        // --- 가상 서피스 + 콘텐츠 비주얼 ---
        let mut vsurf: *mut IDCompositionVirtualSurface = ptr::null_mut();
        hr_check(
            unsafe {
                (*dcomp).CreateVirtualSurface(
                    cw as u32,
                    ch as u32,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_ALPHA_MODE_IGNORE,
                    &mut vsurf,
                )
            },
            "CreateVirtualSurface",
        )?;
        let mut visual: *mut IDCompositionVisual = ptr::null_mut();
        hr_check(unsafe { (*dcomp).CreateVisual(&mut visual) }, "CreateVisual(content)")?;
        hr_check(
            unsafe { (*visual).SetContent(vsurf as *mut IUnknown) },
            "SetContent",
        )?;
        hr_check(
            unsafe { (*root).AddVisual(visual, TRUE, ptr::null_mut()) },
            "AddVisual",
        )?;

        // --- G2 + 4분면 렌더 ---
        let gl = rc.gleam_gl_api();
        unsafe { render_quadrants(rc, &gl, vsurf, cw, ch, true) }?;

        // --- G3/G4: Commit + 화면 픽셀 판독 ---
        hr_check(unsafe { (*dcomp).Commit() }, "Commit(quadrants)")?;
        pump_messages(hwnd);
        std::thread::sleep(std::time::Duration::from_millis(500));
        pump_messages(hwnd);

        let (hw, hh) = (cw / 2, ch / 2);
        let (qx, qy) = (hw / 2, hh / 2);
        let dc = unsafe { GetDC(ptr::null_mut()) };
        let tl = unsafe { sample_client_pixel(hwnd, dc, qx, qy) };
        let tr = unsafe { sample_client_pixel(hwnd, dc, hw + qx, qy) };
        let bl = unsafe { sample_client_pixel(hwnd, dc, qx, hh + qy) };
        let br = unsafe { sample_client_pixel(hwnd, dc, hw + qx, hh + qy) };
        unsafe {
            ReleaseDC(ptr::null_mut(), dc);
        }

        // COLORREF = 0x00BBGGRR: red=0x0000FF, green=0x00FF00, blue=0xFF0000, white=0xFFFFFF
        let expect = [
            (tl, 0x0000_00FFu32, "TL(좌상)=red"),
            (tr, 0x0000_FF00u32, "TR(우상)=green"),
            (bl, 0x00FF_0000u32, "BL(좌하)=blue"),
            (br, 0x00FF_FFFFu32, "BR(우하)=white"),
        ];
        let mut match_count = 0;
        let mut orient_ok = true;
        for (got, want, label) in expect {
            let pass = colors_close(got, want);
            if pass {
                match_count += 1;
            } else {
                orient_ok = false;
            }
            println!(
                "  pixel {label}: got=0x{got:06x} want=0x{want:06x} {}",
                if pass { "OK" } else { "MISMATCH" }
            );
        }

        // G3: 무엇이든 화면에 합성됐는가(최소 한 분면이 기대색과 근접).
        if match_count == 0 {
            // G3 실패 진단: 전체 단색(red) 채움으로 Commit 경로 자체를 분리 검증.
            eprintln!(
                "  [진단] 4분면 판독 전무 매치 → 전체 단색(red) 채움 재시도로 Commit 경로 분리 검증"
            );
            let _ = unsafe { diagnose_full_fill(rc, &gl, dcomp, vsurf, hwnd, cw, ch) };
            release_com(vsurf, visual, root, target, dcomp, dxgi);
            return Err(
                "G3 FAIL: nothing composited to screen (4분면 어느 것도 기대색과 불일치)".into(),
            );
        }
        println!("G3 PASS: DComp commit visible on screen ({match_count}/4 분면 매치)");

        // G4: 네 분면 모두 기대 위치의 기대색.
        if !orient_ok {
            release_com(vsurf, visual, root, target, dcomp, dxgi);
            return Err(
                "G4 FAIL: orientation mismatch (위 픽셀 로그 참조 — y-flip 가정 재검토 필요)"
                    .into(),
            );
        }
        println!("G4 PASS: top-left orientation confirmed");

        release_com(vsurf, visual, root, target, dcomp, dxgi);
        Ok(())
    }

    /// vsurf에 4분면(좌상 R, 우상 G, 좌하 B, 우하 W)을 그린다(top-left 논리 좌표).
    /// `quadrants=false`면 전체를 red 단색으로 채운다(G3 분리 진단용).
    unsafe fn render_quadrants(
        rc: &WindowRenderingContext,
        gl: &std::rc::Rc<dyn gl::Gl>,
        vsurf: *mut IDCompositionVirtualSurface,
        cw: i32,
        ch: i32,
        quadrants: bool,
    ) -> Result<(), String> {
        let update = RECT {
            left: 0,
            top: 0,
            right: cw,
            bottom: ch,
        };
        let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
        let mut offset = POINT { x: 0, y: 0 };
        hr_check(
            unsafe {
                (*vsurf).BeginDraw(
                    &update,
                    &ID3D11Texture2D::uuidof(),
                    &mut tex as *mut _ as *mut _,
                    &mut offset,
                )
            },
            "BeginDraw",
        )?;

        // BeginDraw 텍스처의 실제 크기(아틀라스일 수 있음)로 pbuffer를 만든다.
        let mut desc: D3D11_TEXTURE2D_DESC = unsafe { mem::zeroed() };
        unsafe {
            (*tex).GetDesc(&mut desc);
        }
        let pbuf = rc
            .create_render_pbuffer_from_d3d_texture(
                tex as usize,
                Size2D::new(desc.Width as i32, desc.Height as i32),
            )
            .ok_or_else(|| {
                // G2 실패: pbuffer 래핑 불가 = 설계 재검토 필요(★HALT★).
                let _ = unsafe { (*vsurf).EndDraw() };
                unsafe {
                    (*(tex as *mut IUnknown)).Release();
                }
                "G2 FAIL: pbuffer wrap failed (create_render_pbuffer_from_d3d_texture=None; \
                 RUST_LOG=warn stderr의 EGL 에러 참조)"
                    .to_string()
            })?;
        if quadrants {
            println!(
                "G2 PASS: BeginDraw texture wrapped as pbuffer (offset=({},{}), tex={}x{})",
                offset.x, offset.y, desc.Width, desc.Height
            );
        }

        if !rc.make_render_pbuffer_current(pbuf) {
            rc.destroy_render_pbuffer(pbuf);
            let _ = unsafe { (*vsurf).EndDraw() };
            unsafe {
                (*(tex as *mut IUnknown)).Release();
            }
            return Err("make_render_pbuffer_current 실패".into());
        }

        // pbuffer가 default framebuffer(0). scissor+clear로 분면을 칠한다.
        gl.bind_framebuffer(gl::FRAMEBUFFER, 0);
        gl.viewport(0, 0, desc.Width as i32, desc.Height as i32);
        gl.enable(gl::SCISSOR_TEST);

        let (hw, hh) = (cw / 2, ch / 2);
        // (논리 좌표 lx, ly는 좌상 원점 top-left; 색 r,g,b)
        let quads: [(i32, i32, i32, i32, f32, f32, f32); 4] = if quadrants {
            [
                (0, 0, hw, hh, 1.0, 0.0, 0.0),        // 좌상 = red
                (hw, 0, cw - hw, hh, 0.0, 1.0, 0.0),  // 우상 = green
                (0, hh, hw, ch - hh, 0.0, 0.0, 1.0),  // 좌하 = blue
                (hw, hh, cw - hw, ch - hh, 1.0, 1.0, 1.0), // 우하 = white
            ]
        } else {
            [(0, 0, cw, ch, 1.0, 0.0, 0.0); 4] // 전체 red (진단)
        };

        for (lx, ly, qw, qh, r, g, b) in quads {
            // D3D 텍스처는 top-left 행 순서. GL scissor는 bottom-left 원점이므로
            // top-left 논리 좌표를 y-flip해 지정한다. WR도 NativeSurface를 top-left로
            // 취급하므로 이 규약이 곧 통합 규약이다.
            let gl_y = desc.Height as i32 - (offset.y + ly + qh);
            gl.scissor(offset.x + lx, gl_y, qw, qh);
            gl.clear_color(r, g, b, 1.0);
            gl.clear(gl::COLOR_BUFFER_BIT);
        }
        gl.disable(gl::SCISSOR_TEST);
        gl.flush();
        gl.finish();

        hr_check(unsafe { (*vsurf).EndDraw() }, "EndDraw")?;
        rc.destroy_render_pbuffer(pbuf);
        // pbuffer 바인딩을 풀고 창 서피스를 다시 current로(이후 GL 호출 안전 상태 복원).
        let _ = rc.make_current();
        unsafe {
            (*(tex as *mut IUnknown)).Release();
        }
        Ok(())
    }

    /// G3 실패 시: 전체 red 단색으로 다시 그려 Commit 경로가 성립하는지만 확인(진단 출력).
    unsafe fn diagnose_full_fill(
        rc: &WindowRenderingContext,
        gl: &std::rc::Rc<dyn gl::Gl>,
        dcomp: *mut IDCompositionDevice,
        vsurf: *mut IDCompositionVirtualSurface,
        hwnd: HWND,
        cw: i32,
        ch: i32,
    ) -> Result<(), String> {
        unsafe { render_quadrants(rc, gl, vsurf, cw, ch, false)? };
        hr_check(unsafe { (*dcomp).Commit() }, "Commit(full-fill)")?;
        pump_messages(hwnd);
        std::thread::sleep(std::time::Duration::from_millis(500));
        pump_messages(hwnd);
        let dc = unsafe { GetDC(ptr::null_mut()) };
        let center = unsafe { sample_client_pixel(hwnd, dc, cw / 2, ch / 2) };
        unsafe {
            ReleaseDC(ptr::null_mut(), dc);
        }
        eprintln!(
            "  [진단] 전체 red 채움 후 중앙 픽셀: got=0x{center:06x} (want~0x0000ff). \
             일치하면 Commit 경로는 성립(=원 문제는 좌표), 불일치면 표시 경로 자체 실패."
        );
        Ok(())
    }

    /// COM 객체 best-effort Release(판정 후). WindowRenderingContext는 이후 정상 Drop.
    fn release_com(
        vsurf: *mut IDCompositionVirtualSurface,
        visual: *mut IDCompositionVisual,
        root: *mut IDCompositionVisual,
        target: *mut IDCompositionTarget,
        dcomp: *mut IDCompositionDevice,
        dxgi: *mut IDXGIDevice,
    ) {
        unsafe {
            if !vsurf.is_null() {
                (*(vsurf as *mut IUnknown)).Release();
            }
            if !visual.is_null() {
                (*(visual as *mut IUnknown)).Release();
            }
            if !root.is_null() {
                (*(root as *mut IUnknown)).Release();
            }
            if !target.is_null() {
                (*(target as *mut IUnknown)).Release();
            }
            if !dcomp.is_null() {
                (*(dcomp as *mut IUnknown)).Release();
            }
            if !dxgi.is_null() {
                (*(dxgi as *mut IUnknown)).Release();
            }
        }
    }
}
