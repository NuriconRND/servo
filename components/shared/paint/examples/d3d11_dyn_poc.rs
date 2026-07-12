/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! PoC: DYNAMIC D3D11 텍스처의 EGLImage 래핑 + WRITE_DISCARD rename 투명성 게이트.
//! 스펙 docs/superpowers/specs/2026-07-12-wr-yuv-direct-sample-design.md §6.
//! 실패 시 구현 중단 + 사용자 문의 (Global Constraints HALT 규칙).
//!
//! 실행 (PowerShell):
//!   . .\etc\multigpu\servo_env.ps1; $ErrorActionPreference='Continue'
//!   $env:RUST_LOG='warn'
//!   cargo run --release -p servo-paint-api --example d3d11_dyn_poc --features no-wgl
//!
//! Windows + `no-wgl`(ANGLE) 빌드 전용 — 미디어 D3D11 인터롭 메서드
//! (`media_d3d11_device_handle`/`map_d3d11_dynamic_texture`/
//! `wrap_d3d11_texture_as_gl_texture`/...)가 이 조합에서만 실제 구현을 갖고,
//! 그 외에는 트레잇 기본값(`None`/no-op)이라 이 PoC는 무의미하다.
//!
//! 게이트가 검증하는 것:
//!   GATE1: DYNAMIC+SHADER_RESOURCE 텍스처(R8, RG8)가 EGLImage로 래핑되어
//!          올바른 값을 샘플하는가.
//!   GATE2: 같은 wrap을 재사용한 채 Map(WRITE_DISCARD)로 다른 값을 쓰고
//!          Unmap한 뒤 재샘플하면 새 값이 보이는가(rename이 EGLImage SRV에
//!          투명한가).
//!
//! 무결성 규칙: 게이트를 통과시키기 위해 Usage를 DEFAULT로 바꾸거나 바인드
//! 플래그를 추가하거나 패턴 사이에 재래핑하지 않는다. 명세대로 실패하면
//! 실패로 보고한다(FAIL도 유효한 결과).

#[cfg(all(target_os = "windows", feature = "no-wgl"))]
fn main() {
    std::process::exit(poc::run());
}

#[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
fn main() {
    eprintln!(
        "d3d11_dyn_poc: Windows + `no-wgl`(ANGLE) 조합 전용 PoC입니다 — 미디어 D3D11 인터롭 \
         메서드가 cfg(all(target_os = \"windows\", feature = \"no-wgl\"))로 게이트되어 다른 \
         빌드 설정에서는 전부 None/no-op입니다. 이 빌드에서는 스킵합니다."
    );
    std::process::exit(1);
}

#[cfg(all(target_os = "windows", feature = "no-wgl"))]
mod poc {
    use std::ffi::c_void;
    use std::ptr;
    use std::rc::Rc;

    use dpi::PhysicalSize;
    use gleam::gl::{self, Gl};
    use paint_api::rendering_context::{RenderingContext, WindowRenderingContext};
    use raw_window_handle::{DisplayHandle, RawWindowHandle, Win32WindowHandle, WindowHandle};
    use winapi::Interface;
    use winapi::shared::dxgi::{IDXGIAdapter, IDXGIDevice, DXGI_ADAPTER_DESC};
    use winapi::shared::dxgiformat::{DXGI_FORMAT, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM};
    use winapi::shared::dxgitype::DXGI_SAMPLE_DESC;
    use winapi::shared::minwindef::{HINSTANCE, LPARAM, LRESULT, UINT, WPARAM};
    use winapi::shared::windef::HWND;
    use winapi::um::d3d11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_WRITE, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_DYNAMIC, ID3D11Device, ID3D11Texture2D,
    };
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::winuser::{
        CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, WNDCLASSW,
        WS_OVERLAPPEDWINDOW,
    };
    use wio::com::ComPtr;

    /// D3D11 텍스처와 FBO 판독 크기 (4x4 — 모든 텍셀을 같은 값으로 채우므로 필터링이
    /// 결과를 흐릴 수 없다).
    const TEX_SIZE: u32 = 4;

    const VS_SRC: &str = "\
attribute vec2 a_position;
attribute vec2 a_texcoord;
varying vec2 v_texcoord;
void main() {
    v_texcoord = a_texcoord;
    gl_Position = vec4(a_position, 0.0, 1.0);
}
";

    const FS_SRC: &str = "\
precision mediump float;
varying vec2 v_texcoord;
uniform sampler2D u_texture;
void main() {
    vec4 c = texture2D(u_texture, v_texcoord);
    gl_FragColor = vec4(c.r, c.g, 0.0, 1.0);
}
";

    /// 오프스크린으로만 쓰이는 숨김 win32 창. `WindowRenderingContext`가 요구하는
    /// (DisplayHandle, WindowHandle) 쌍을 얻기 위한 최소 껍데기 — 절대 Show/메시지
    /// 펌프를 하지 않는다.
    struct HiddenWindow {
        hwnd: HWND,
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: UINT,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    impl HiddenWindow {
        fn new() -> Self {
            unsafe {
                let hinstance: HINSTANCE = GetModuleHandleW(ptr::null());
                let class_name: Vec<u16> = "ServoD3D11DynPoc\0".encode_utf16().collect();
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
                    0,
                    class_name.as_ptr(),
                    class_name.as_ptr(),
                    WS_OVERLAPPEDWINDOW,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    64,
                    64,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    hinstance,
                    ptr::null_mut(),
                );
                assert!(!hwnd.is_null(), "CreateWindowExW 실패 (숨김 오프스크린 창)");
                HiddenWindow { hwnd }
            }
        }

        fn raw_window_handle(&self) -> RawWindowHandle {
            let hwnd = std::num::NonZeroIsize::new(self.hwnd as isize).expect("HWND가 0");
            RawWindowHandle::Win32(Win32WindowHandle::new(hwnd))
        }
    }

    impl Drop for HiddenWindow {
        fn drop(&mut self) {
            unsafe {
                DestroyWindow(self.hwnd);
            }
        }
    }

    /// D3D11 디바이스에 종속된 어댑터(GPU) 설명을 조회해 출력한다 — 이 PoC가 실제로
    /// 어떤 물리 GPU에서 실행됐는지(하드웨어 vs WARP) 실측으로 남기기 위함.
    fn print_adapter_description(device: *mut ID3D11Device) {
        unsafe {
            let mut dxgi_device: *mut IDXGIDevice = ptr::null_mut();
            let hr = (*device).QueryInterface(
                &IDXGIDevice::uuidof(),
                &mut dxgi_device as *mut *mut IDXGIDevice as *mut *mut c_void,
            );
            if hr < 0 || dxgi_device.is_null() {
                println!("어댑터 설명 조회 실패 (QueryInterface hr={hr:#x})");
                return;
            }
            let dxgi_device = ComPtr::from_raw(dxgi_device);

            let mut adapter: *mut IDXGIAdapter = ptr::null_mut();
            let hr = dxgi_device.GetAdapter(&mut adapter);
            if hr < 0 || adapter.is_null() {
                println!("어댑터 조회 실패 (GetAdapter hr={hr:#x})");
                return;
            }
            let adapter = ComPtr::from_raw(adapter);

            let mut desc: DXGI_ADAPTER_DESC = std::mem::zeroed();
            if (*adapter).GetDesc(&mut desc) < 0 {
                println!("어댑터 GetDesc 실패");
                return;
            }
            let len = desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..len]);
            println!(
                "어댑터: \"{name}\" VendorId={:#06x} DeviceId={:#06x} LUID={}:{} \
                 (connection.create_adapter() -> create_hardware_adapter(), Intel 회피 선호; \
                 WARP는 create_software_adapter 별도 경로 — 이 PoC는 미사용)",
                desc.VendorId, desc.DeviceId, desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart
            );
        }
    }

    /// DYNAMIC + BIND_SHADER_RESOURCE + CPU_WRITE 4x4 텍스처 생성.
    fn create_dynamic_texture(
        device: *mut ID3D11Device,
        format: DXGI_FORMAT,
        size: u32,
    ) -> Result<ComPtr<ID3D11Texture2D>, i32> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size,
            Height: size,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_SHADER_RESOURCE,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE,
            MiscFlags: 0,
        };
        let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
        let hr = unsafe { (*device).CreateTexture2D(&desc, ptr::null(), &mut tex) };
        if hr < 0 || tex.is_null() {
            return Err(hr);
        }
        Ok(unsafe { ComPtr::from_raw(tex) })
    }

    /// `map_d3d11_dynamic_texture` → 패턴을 모든 텍셀에 기록 → `unmap_d3d11_texture`.
    fn write_pattern(
        context: &WindowRenderingContext,
        texture_ptr: usize,
        channels: usize,
        pattern: &[u8],
    ) -> Result<(), String> {
        let Some((dst_ptr, dst_pitch)) = context.map_d3d11_dynamic_texture(texture_ptr) else {
            return Err("map_d3d11_dynamic_texture 실패".to_string());
        };
        let row_bytes = TEX_SIZE as usize * channels;
        let mut buf = vec![0u8; row_bytes * TEX_SIZE as usize];
        for chunk in buf.chunks_mut(channels) {
            chunk.copy_from_slice(pattern);
        }
        context.copy_rows_to_mapped(dst_ptr, dst_pitch, &buf, row_bytes, TEX_SIZE as usize);
        context.unmap_d3d11_texture(texture_ptr);
        Ok(())
    }

    fn compile_shader(gl: &Rc<dyn Gl>, shader_type: gl::GLenum, source: &str) -> Result<gl::GLuint, String> {
        let shader = gl.create_shader(shader_type);
        gl.shader_source(shader, &[source.as_bytes()]);
        gl.compile_shader(shader);
        let mut status = [0i32];
        unsafe {
            gl.get_shader_iv(shader, gl::COMPILE_STATUS, &mut status);
        }
        if status[0] == 0 {
            let log = gl.get_shader_info_log(shader);
            gl.delete_shader(shader);
            return Err(log);
        }
        Ok(shader)
    }

    struct GlResources {
        program: gl::GLuint,
        vao: gl::GLuint,
        vbo: gl::GLuint,
        fbo: gl::GLuint,
        fbo_texture: gl::GLuint,
        u_texture_loc: gl::GLint,
    }

    fn setup_gl_resources(gl: &Rc<dyn Gl>, size: u32) -> Result<GlResources, String> {
        let vs = compile_shader(gl, gl::VERTEX_SHADER, VS_SRC).map_err(|e| format!("정점 셰이더 컴파일 실패: {e}"))?;
        let fs = compile_shader(gl, gl::FRAGMENT_SHADER, FS_SRC).map_err(|e| format!("프래그먼트 셰이더 컴파일 실패: {e}"))?;

        let program = gl.create_program();
        gl.attach_shader(program, vs);
        gl.attach_shader(program, fs);
        gl.link_program(program);
        let mut link_status = [0i32];
        unsafe {
            gl.get_program_iv(program, gl::LINK_STATUS, &mut link_status);
        }
        if link_status[0] == 0 {
            let log = gl.get_program_info_log(program);
            return Err(format!("프로그램 링크 실패: {log}"));
        }
        gl.delete_shader(vs);
        gl.delete_shader(fs);

        let a_position = gl.get_attrib_location(program, "a_position");
        let a_texcoord = gl.get_attrib_location(program, "a_texcoord");
        let u_texture_loc = gl.get_uniform_location(program, "u_texture");
        assert!(a_position >= 0 && a_texcoord >= 0, "attribute location 조회 실패");

        let vao = gl.gen_vertex_arrays(1)[0];
        gl.bind_vertex_array(vao);

        let vbo = gl.gen_buffers(1)[0];
        gl.bind_buffer(gl::ARRAY_BUFFER, vbo);
        // 풀스크린 쿼드 (2 삼각형): [x, y, u, v] x 6.
        #[rustfmt::skip]
        let vertices: [f32; 24] = [
            -1.0, -1.0,  0.0, 0.0,
             1.0, -1.0,  1.0, 0.0,
            -1.0,  1.0,  0.0, 1.0,
            -1.0,  1.0,  0.0, 1.0,
             1.0, -1.0,  1.0, 0.0,
             1.0,  1.0,  1.0, 1.0,
        ];
        gl::buffer_data(&**gl, gl::ARRAY_BUFFER, &vertices, gl::STATIC_DRAW);

        let stride = (4 * std::mem::size_of::<f32>()) as gl::GLsizei;
        let uv_offset = (2 * std::mem::size_of::<f32>()) as gl::GLuint;
        gl.enable_vertex_attrib_array(a_position as gl::GLuint);
        gl.vertex_attrib_pointer(a_position as gl::GLuint, 2, gl::FLOAT, false, stride, 0);
        gl.enable_vertex_attrib_array(a_texcoord as gl::GLuint);
        gl.vertex_attrib_pointer(a_texcoord as gl::GLuint, 2, gl::FLOAT, false, stride, uv_offset);

        gl.bind_vertex_array(0);
        gl.bind_buffer(gl::ARRAY_BUFFER, 0);

        // 판독용 RGBA8 FBO. NEAREST — 필터링이 assert를 흐릴 수 없게.
        let fbo_texture = gl.gen_textures(1)[0];
        gl.bind_texture(gl::TEXTURE_2D, fbo_texture);
        gl.tex_image_2d(
            gl::TEXTURE_2D,
            0,
            gl::RGBA as gl::GLint,
            size as gl::GLsizei,
            size as gl::GLsizei,
            0,
            gl::RGBA,
            gl::UNSIGNED_BYTE,
            None,
        );
        gl.tex_parameter_i(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as gl::GLint);
        gl.tex_parameter_i(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::NEAREST as gl::GLint);
        gl.bind_texture(gl::TEXTURE_2D, 0);

        let fbo = gl.gen_framebuffers(1)[0];
        gl.bind_framebuffer(gl::FRAMEBUFFER, fbo);
        gl.framebuffer_texture_2d(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, fbo_texture, 0);
        let status = gl.check_frame_buffer_status(gl::FRAMEBUFFER);
        gl.bind_framebuffer(gl::FRAMEBUFFER, 0);
        if status != gl::FRAMEBUFFER_COMPLETE {
            return Err(format!("FBO incomplete: {status:#x}"));
        }

        Ok(GlResources {
            program,
            vao,
            vbo,
            fbo,
            fbo_texture,
            u_texture_loc,
        })
    }

    fn cleanup_gl_resources(gl: &Rc<dyn Gl>, resources: GlResources) {
        gl.delete_framebuffers(&[resources.fbo]);
        gl.delete_textures(&[resources.fbo_texture]);
        gl.delete_buffers(&[resources.vbo]);
        gl.delete_vertex_arrays(&[resources.vao]);
        gl.delete_program(resources.program);
    }

    /// 래핑된 D3D11 텍스처를 FBO에 그려 넣고 읽어온다. (픽셀, 그리는 동안의 최종 GL 에러)
    fn draw_and_readback(gl: &Rc<dyn Gl>, resources: &GlResources, source_gl_texture: u32, size: u32) -> (Vec<u8>, gl::GLenum) {
        gl.bind_framebuffer(gl::FRAMEBUFFER, resources.fbo);
        gl.viewport(0, 0, size as gl::GLsizei, size as gl::GLsizei);
        gl.clear_color(0.0, 0.0, 0.0, 0.0);
        gl.clear(gl::COLOR_BUFFER_BIT);
        gl.use_program(resources.program);
        gl.active_texture(gl::TEXTURE0);
        gl.bind_texture(gl::TEXTURE_2D, source_gl_texture);
        gl.uniform_1i(resources.u_texture_loc, 0);
        gl.bind_vertex_array(resources.vao);
        gl.draw_arrays(gl::TRIANGLES, 0, 6);
        let pixels = gl.read_pixels(0, 0, size as gl::GLsizei, size as gl::GLsizei, gl::RGBA, gl::UNSIGNED_BYTE);
        let error = gl.get_error();
        gl.bind_vertex_array(0);
        gl.bind_texture(gl::TEXTURE_2D, 0);
        gl.bind_framebuffer(gl::FRAMEBUFFER, 0);
        (pixels, error)
    }

    /// 판독된 RGBA8 픽셀들이 모두 `expected`(R, 필요시 RG)와 정확히 일치하는지 확인한다.
    /// (bool, 진단 문자열) 반환 — 진단 문자열은 GATE 판정 라인에 그대로 붙인다.
    fn check_pixels(pixels: &[u8], size: u32, channels: usize, expected: &[u8]) -> (bool, String) {
        let mut all_match = true;
        let mut first_mismatch: Option<(u32, u32, u8, u8)> = None;
        for y in 0..size {
            for x in 0..size {
                let idx = ((y * size + x) * 4) as usize;
                let r = pixels[idx];
                let g = pixels[idx + 1];
                let ok = r == expected[0] && (channels < 2 || g == expected[1]);
                if !ok {
                    all_match = false;
                    if first_mismatch.is_none() {
                        first_mismatch = Some((x, y, r, g));
                    }
                }
            }
        }
        let diag = if channels == 1 {
            match first_mismatch {
                None => format!("observed_R={:#04x} expected_R={:#04x}", pixels[0], expected[0]),
                Some((x, y, r, _)) => format!(
                    "observed_R={r:#04x} expected_R={:#04x} mismatch_at=({x},{y})",
                    expected[0]
                ),
            }
        } else {
            match first_mismatch {
                None => format!(
                    "observed_R={:#04x} observed_G={:#04x} expected_R={:#04x} expected_G={:#04x}",
                    pixels[0], pixels[1], expected[0], expected[1]
                ),
                Some((x, y, r, g)) => format!(
                    "observed_R={r:#04x} observed_G={g:#04x} expected_R={:#04x} expected_G={:#04x} \
                     mismatch_at=({x},{y})",
                    expected[0], expected[1]
                ),
            }
        };
        (all_match, diag)
    }

    /// 포맷 한 라운드: create → map → 패턴A → unmap → wrap(1회) → draw+readback → assert A →
    /// map(DISCARD) → 패턴B → unmap → draw+readback → assert B → destroy wrap → release.
    /// 항상 정확히 두 개의 `GATE1(label)=...`/`GATE2(label)=...` 줄을 출력하고 (gate1, gate2)를
    /// 반환한다.
    #[allow(clippy::too_many_arguments)]
    fn run_format_round(
        context: &WindowRenderingContext,
        gl: &Rc<dyn Gl>,
        resources: &GlResources,
        device: *mut ID3D11Device,
        format: DXGI_FORMAT,
        channels: usize,
        label: &str,
        pattern_a: &[u8],
        pattern_b: &[u8],
    ) -> (bool, bool) {
        println!("--- {label} 라운드 ---");

        let texture = match create_dynamic_texture(device, format, TEX_SIZE) {
            Ok(texture) => texture,
            Err(hr) => {
                println!("GATE1({label})=FAIL reason=CreateTexture2D_failed hr={hr:#x}");
                println!("GATE2({label})=FAIL reason=skipped(GATE1_prereq_failed)");
                return (false, false);
            },
        };
        let texture_ptr = texture.as_raw() as usize;

        if let Err(error) = write_pattern(context, texture_ptr, channels, pattern_a) {
            println!("GATE1({label})=FAIL reason={error}(pattern_A)");
            println!("GATE2({label})=FAIL reason=skipped(GATE1_prereq_failed)");
            let raw = texture.into_raw();
            context.release_d3d11_texture(raw as usize);
            return (false, false);
        }

        let Some(wrap) = context.wrap_d3d11_texture_as_gl_texture(texture_ptr) else {
            println!(
                "GATE1({label})=FAIL reason=wrap_d3d11_texture_as_gl_texture_failed \
                 (RUST_LOG=warn stderr의 EGL 에러 참조)"
            );
            println!("GATE2({label})=FAIL reason=skipped(GATE1_prereq_failed)");
            let raw = texture.into_raw();
            context.release_d3d11_texture(raw as usize);
            return (false, false);
        };
        println!(
            "  wrap 성공: egl_image={:#x} gl_texture={}",
            wrap.egl_image, wrap.gl_texture
        );

        let (pixels_a, gl_err_a) = draw_and_readback(gl, resources, wrap.gl_texture, TEX_SIZE);
        let (gate1_pixels, diag1) = check_pixels(&pixels_a, TEX_SIZE, channels, pattern_a);
        let gate1 = gate1_pixels && (gl_err_a == gl::NO_ERROR);
        let gate1_verdict = if gate1 { "PASS" } else { "FAIL" };
        if gl_err_a != gl::NO_ERROR {
            println!("GATE1({label})={gate1_verdict} gl_error={gl_err_a:#x} {diag1}");
        } else {
            println!("GATE1({label})={gate1_verdict} {diag1}");
        }

        // GATE2: 같은 wrap을 유지한 채 재-Map(DISCARD) → 새 패턴 → 재샘플.
        let gate2 = match write_pattern(context, texture_ptr, channels, pattern_b) {
            Err(error) => {
                println!("GATE2({label})=FAIL reason={error}(pattern_B)");
                false
            },
            Ok(()) => {
                let (pixels_b, gl_err_b) = draw_and_readback(gl, resources, wrap.gl_texture, TEX_SIZE);
                let (gate2_pixels, diag2) = check_pixels(&pixels_b, TEX_SIZE, channels, pattern_b);
                let gate2 = gate2_pixels && (gl_err_b == gl::NO_ERROR);
                let gate2_verdict = if gate2 { "PASS" } else { "FAIL" };
                if gl_err_b != gl::NO_ERROR {
                    println!("GATE2({label})={gate2_verdict} gl_error={gl_err_b:#x} {diag2}");
                } else {
                    println!("GATE2({label})={gate2_verdict} {diag2}");
                }
                gate2
            },
        };

        context.destroy_d3d11_gl_wrap(wrap);
        let raw = texture.into_raw();
        context.release_d3d11_texture(raw as usize);

        (gate1, gate2)
    }

    pub fn run() -> i32 {
        env_logger::init();

        let window = HiddenWindow::new();
        let display_handle = DisplayHandle::windows();
        let window_handle = unsafe { WindowHandle::borrow_raw(window.raw_window_handle()) };

        let context = match WindowRenderingContext::new(display_handle, window_handle, PhysicalSize::new(64, 64)) {
            Ok(context) => context,
            Err(error) => {
                eprintln!(
                    "WindowRenderingContext::new 실패: {error:?} — 하드웨어 ANGLE 디바이스를 얻지 \
                     못함 (RUST_LOG=warn로 재실행해 EGL/DXGI 진단 확인)"
                );
                return 1;
            },
        };

        // 과거 이 PoC는 정상 Drop 시 STATUS_ACCESS_VIOLATION(0xC0000005)을 냈고
        // process::exit로 Drop을 우회했었다. 두 개의 서로 다른 벤더링 surfman 버그가
        // 있었다: (1) angle/context.rs destroy_context의 이중 egl.DestroyContext(수정: 중복
        // 호출 제거), (2) 진짜 원인 — angle/device.rs Device::drop이 egl.Terminate로 ANGLE
        // 내부 ID3D11Device를 파괴한 "뒤" surfman이 보유한 ComPtr<ID3D11Device>의 Release가
        // 실행되어 이미 해제된 COM 객체를 역참조(use-after-free). 수정: d3d11_device를
        // ManuallyDrop으로 감싸 egl.Terminate "이전"에 Release. present-path-fast와는 무관
        // (FAST off에서도 동일 AV 재현 확인). 이제 run()이 정상 반환해 Drop 경로를 그대로
        // 타며, 이 PoC의 깨끗한(0) 종료가 위 두 수정의 회귀 검증이다.
        if let Err(error) = context.make_current() {
            eprintln!("make_current 실패: {error:?}");
            std::process::exit(1);
        }

        let Some(device_ptr) = context.media_d3d11_device_handle() else {
            eprintln!(
                "media_d3d11_device_handle()이 None을 반환 — no-wgl(ANGLE) 빌드가 아니거나 D3D11 \
                 디바이스 획득 실패"
            );
            std::process::exit(1);
        };
        let device = device_ptr as *mut ID3D11Device;
        println!("D3D11 디바이스 핸들 획득: {device_ptr:#x}");
        print_adapter_description(device);

        let gl = context.gleam_gl_api();
        let resources = match setup_gl_resources(&gl, TEX_SIZE) {
            Ok(resources) => resources,
            Err(error) => {
                eprintln!("GL 리소스 초기화 실패: {error}");
                std::process::exit(1);
            },
        };

        let (gate1_r8, gate2_r8) = run_format_round(
            &context,
            &gl,
            &resources,
            device,
            DXGI_FORMAT_R8_UNORM,
            1,
            "R8",
            &[0xA5],
            &[0x5A],
        );
        let (gate1_rg8, gate2_rg8) = run_format_round(
            &context,
            &gl,
            &resources,
            device,
            DXGI_FORMAT_R8G8_UNORM,
            2,
            "RG8",
            &[0xA5, 0x3C],
            &[0x5A, 0xC3],
        );

        cleanup_gl_resources(&gl, resources);

        // GATE1/GATE2 판정 라인(관측값 포함)은 run_format_round가 이미 각 라운드에서
        // 출력했다 — 여기서는 종료 코드만 집계한다.
        let exit_code = if gate1_r8 && gate1_rg8 && gate2_r8 && gate2_rg8 {
            0
        } else {
            1
        };
        // 정상 반환 — WindowRenderingContext가 스코프 종료 시 Drop 경로
        // (destroy_context → Device::drop의 egl.Terminate → d3d11_device Release)를
        // 그대로 타며, 이 PoC의 깨끗한(0) 종료 자체가 해체 AV 수정의 회귀 검증이다
        // (위 주석의 근본 원인 참조).
        exit_code
    }
}
