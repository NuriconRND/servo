/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 네이티브 크래시가 났을 때 **어디서 났는지**를 로그에 남긴다.
//!
//! 이벤트 뷰어는 오류 모듈과 오프셋 한 쌍만 준다. 그것으로 함수 하나는 짚을 수 있지만
//! (`gstreamer-1.0-0.dll +0x1c79a` -> `gst_bus_post`), **누가 그것을 불렀는지**는 알 수
//! 없다. 캡처 표출을 끌 때 죽는 크래시가 정확히 그 벽에 부딪혔다 -- 죽는 함수는 알아냈는데
//! 그 함수를 부른 것이 우리 코드의 어느 경로인지 몰라 세 번을 추측으로 고쳤다.
//!
//! 그래서 처리되지 않은 예외를 가로채 스택을 찍는다. 테스트 장비에는 심볼이 없으므로
//! **모듈 이름 + 그 모듈 안의 오프셋**으로 찍는다. 개발기에서 그 오프셋을 심볼라이즈하면
//! (우리 프레임은 `winit_wall.pdb`, 남의 프레임은 export 표) 호출 경로가 그대로 나온다.
//!
//! 덤프 파일을 쓰지 않는 이유: 덤프는 9.5GB 가 되고(실측) 공유 폴더로 옮기는 것부터
//! 일이다. 스택 한 줄씩이면 로그에 그대로 들어가고, 필요한 것은 그 스택이 전부다.
//!
//! # 오프셋을 이름으로 바꾸기
//!
//! 찍히는 값은 **모듈 안의 RVA** 다. PE 의 이미지 베이스(`0x140000000`)를 더해 넘긴다:
//!
//! ```text
//! # 우리 프레임 -- 그 빌드의 PDB 가 옆에 있어야 한다
//! "0x{RVA + 0x140000000:x}" | llvm-symbolizer --obj=winit_wall.exe --demangle
//!
//! # 남의 DLL -- PDB 가 없으면 export 표에서 그 RVA 바로 앞 함수를 찾는다
//! llvm-objdump --private-headers gstreamer-1.0-0.dll   # RVA 내림차순으로 훑는다
//! ```
//!
//! 이 경로는 자기시험(`main` 에서 널 쓰기)으로 한 번 확인했다 -- 찍힌 오프셋이
//! `winit_wall::main` 으로 정확히 돌아왔다.

#[cfg(windows)]
mod windows_impl {
    use std::ffi::c_void;
    use std::io::Write;

    use windows_sys::Win32::Foundation::{HMODULE, MAX_PATH};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        EXCEPTION_POINTERS, RtlCaptureStackBackTrace, SetUnhandledExceptionFilter,
    };
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameA, GetModuleHandleExA,
    };

    /// 스택에서 가져올 프레임 수. 이 크래시들은 깊지 않다(미디어 파이프라인 해체 경로가
    /// 가장 깊어 봐야 수십 프레임) -- 넉넉히 잡되 로그 한 덩어리로 읽히는 크기.
    const MAX_FRAMES: usize = 62;

    /// 주소가 속한 모듈의 파일 이름과 그 안에서의 오프셋. 모듈을 못 찾으면 `None`.
    fn module_and_offset(address: *mut c_void) -> Option<(String, usize)> {
        let mut module: HMODULE = std::ptr::null_mut();
        // SAFETY: 주소는 스택 추적이 돌려준 코드 주소이고, `UNCHANGED_REFCOUNT` 라
        // 모듈 참조수를 건드리지 않는다(크래시 처리 중에 로드/언로드를 유발하지 않는다).
        let found = unsafe {
            GetModuleHandleExA(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                address as *const u8,
                &mut module,
            )
        };
        if found == 0 || module.is_null() {
            return None;
        }
        let mut buffer = [0u8; MAX_PATH as usize];
        // SAFETY: `module` 은 방금 얻은 유효한 핸들이고 버퍼 길이를 같이 넘긴다.
        let length = unsafe { GetModuleFileNameA(module, buffer.as_mut_ptr(), buffer.len() as u32) }
            as usize;
        if length == 0 {
            return None;
        }
        let path = String::from_utf8_lossy(&buffer[..length]).into_owned();
        let name = path.rsplit(['\\', '/']).next().unwrap_or(&path).to_owned();
        let offset = (address as usize).wrapping_sub(module as usize);
        Some((name, offset))
    }

    /// 처리되지 않은 예외 하나를 stderr 에 적는다.
    ///
    /// ★`log` 를 쓰지 않는다★ -- 크래시 시점에는 로거의 상태를 믿을 수 없고(락을 쥔 채
    /// 죽었을 수 있다), 잠기면 이 핸들러 자체가 멎어 아무것도 남지 않는다. stderr 에 직접
    /// 쓰면 그 위험이 없고, 실행 로그가 어차피 stderr 리다이렉트라 같은 파일에 들어간다.
    unsafe extern "system" fn on_unhandled_exception(info: *const EXCEPTION_POINTERS) -> i32 {
        let mut out = std::io::stderr().lock();
        let _ = writeln!(
            out,
            "CRASH =================================================="
        );
        if !info.is_null() {
            // SAFETY: OS 가 넘겨준 포인터이며 이 핸들러가 도는 동안 유효하다.
            let record = unsafe { (*info).ExceptionRecord };
            if !record.is_null() {
                // SAFETY: 위와 같다.
                let (code, address) =
                    unsafe { ((*record).ExceptionCode, (*record).ExceptionAddress) };
                let _ = write!(out, "CRASH code=0x{:08x} at=", code as u32);
                match module_and_offset(address) {
                    Some((name, offset)) => {
                        let _ = writeln!(out, "{name}+0x{offset:x}");
                    },
                    None => {
                        let _ = writeln!(out, "0x{:x} (module unknown)", address as usize);
                    },
                }
            }
        }

        let mut frames = [std::ptr::null_mut::<c_void>(); MAX_FRAMES];
        // SAFETY: 길이를 정확히 넘기고, 돌려준 개수만큼만 읽는다.
        let captured = unsafe {
            RtlCaptureStackBackTrace(
                0,
                MAX_FRAMES as u32,
                frames.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        } as usize;
        for (index, frame) in frames.iter().take(captured).enumerate() {
            match module_and_offset(*frame) {
                Some((name, offset)) => {
                    let _ = writeln!(out, "CRASH  #{index:02} {name}+0x{offset:x}");
                },
                None => {
                    let _ = writeln!(out, "CRASH  #{index:02} 0x{:x}", *frame as usize);
                },
            }
        }
        let _ = writeln!(
            out,
            "CRASH =================================================="
        );
        let _ = out.flush();
        // 계속 넘긴다 -- 기존처럼 WER 이 받아 이벤트 뷰어 항목도 그대로 남는다.
        windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_CONTINUE_SEARCH
    }

    pub fn install() {
        // SAFETY: 프로세스 전역 필터를 한 번 건다. 이전 필터는 쓰지 않는다(기본값은 WER).
        unsafe { SetUnhandledExceptionFilter(Some(on_unhandled_exception)) };
    }
}

/// 크래시 보고를 건다. Windows 밖에서는 아무것도 하지 않는다.
pub fn install() {
    #[cfg(windows)]
    windows_impl::install();
}
