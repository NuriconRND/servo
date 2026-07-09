/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! PoC: mp4 → decodebin → d3d11upload → appsink(memory:D3D11Memory, 디코더 원 포맷)
//! → 디바이스 일치 assert → GstD3D11Converter로 공유 링 슬롯에 직접 YUV→RGBA 렌더 →
//! 별개 D3D11 디바이스에서 판독해 비검정 확인.
//!
//! 실행: cargo run -p servo-media-gstreamer-render-d3d11 --release --example d3d11_upload_poc -- <mp4 경로>
//!
//! Windows 전용 — 이 크레이트의 D3D11 상호운용 의존성(winapi/wio/gstreamer-app)이
//! `cfg(windows)`에서만 활성화되므로, 다른 타겟에서는 본문 전체를 건너뛰고
//! 스텁 `main`만 컴파일한다 (workspace `--workspace` 빌드가 비Windows에서도 이
//! 예제를 시도하기 때문).

#[cfg(windows)]
fn main() {
    windows_impl::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "d3d11_upload_poc: Windows 전용 예제입니다 (render-d3d11의 D3D11 상호운용 \
         의존성이 cfg(windows) 게이트) — 이 플랫폼에서는 스킵합니다."
    );
}

#[cfg(windows)]
mod windows_impl {
    use gstreamer::glib::translate::ToGlibPtr;
    use gstreamer::prelude::*;
    use servo_media_gstreamer_render_d3d11::ffi::{GstD3D11Api, GstD3D11Converter, GstD3D11Memory};
    use servo_media_gstreamer_render_d3d11::{SharedGstD3D11Device, SharedTextureRing};
    use winapi::Interface;
    use winapi::shared::dxgiformat::DXGI_FORMAT_R8G8B8A8_UNORM;
    use winapi::shared::dxgitype::DXGI_SAMPLE_DESC;
    use winapi::um::d3d11 as d3d;
    use winapi::um::d3dcommon::D3D_DRIVER_TYPE_HARDWARE;
    use wio::com::ComPtr;

    const FRAMES_TO_CHECK: usize = 60;

    pub fn run() {
        let path = std::env::args()
            .nth(1)
            .expect("사용법: d3d11_upload_poc <mp4 경로>");
        gstreamer::init().expect("gstreamer init 실패");

        let api = GstD3D11Api::load().expect("gstd3d11-1.0-0.dll 로드 실패");
        let device = SharedGstD3D11Device::get_or_create().expect("GstD3D11Device 생성 실패");

        // format 미지정: 디코더 원 포맷(I420/NV12)을 D3D11 메모리로 그대로 받고,
        // RGBA 변환은 아래에서 GstD3D11Converter가 링 슬롯에 직접 수행한다.
        let desc = format!(
            "filesrc location=\"{}\" ! decodebin ! d3d11upload ! \
             video/x-raw(memory:D3D11Memory) ! \
             appsink name=sink sync=false max-buffers=4",
            path.replace('\\', "/")
        );
        let pipeline = gstreamer::parse::launch(&desc)
            .expect("파이프라인 생성 실패")
            .downcast::<gstreamer::Pipeline>()
            .expect("Pipeline 다운캐스트 실패");

        // 프로덕션과 동일: 우리 디바이스를 파이프라인에 주입 (d3d11 엘리먼트가 이걸 쓰는지
        // 아래 GetDevice 비교로 확증)
        let context = device.gst_context().expect("gst_d3d11_context_new 실패");
        pipeline.set_context(&context);

        let appsink = pipeline
            .by_name("sink")
            .expect("appsink 없음")
            .downcast::<gstreamer_app::AppSink>()
            .expect("AppSink 다운캐스트 실패");

        pipeline
            .set_state(gstreamer::State::Playing)
            .expect("PLAYING 전환 실패");

        let mut ring = SharedTextureRing::new(device.clone());
        let mut converter: Option<*mut GstD3D11Converter> = None; // PoC라 해제 생략(프로세스 종료 회수)
        let mut last = None;
        for i in 0..FRAMES_TO_CHECK {
            let sample = appsink
                .pull_sample()
                .unwrap_or_else(|_| panic!("샘플 {i} 획득 실패 (caps 협상 실패 가능성)"));
            let caps = sample.caps().expect("caps 없음");
            let info = gstreamer_video::VideoInfo::from_caps(caps).expect("VideoInfo 실패");
            let buffer = sample.buffer().expect("버퍼 없음");
            let width = info.width() as i32;
            let height = info.height() as i32;

            unsafe {
                let mem_ptr = buffer.peek_memory(0).as_mut_ptr();
                assert_ne!(
                    (api.is_d3d11_memory)(mem_ptr),
                    0,
                    "프레임 {i}: D3D11Memory가 아님 — caps 협상 확인 필요"
                );
                // 디바이스 일치 확증: 디코드 텍스처의 디바이스 == 우리가 주입한 디바이스
                let resource = (api.memory_get_resource_handle)(mem_ptr as *mut GstD3D11Memory);
                assert!(!resource.is_null(), "프레임 {i}: resource 핸들 null");
                let mut frame_device: *mut d3d::ID3D11Device = std::ptr::null_mut();
                (*resource).GetDevice(&mut frame_device);
                let frame_device = ComPtr::from_raw(frame_device);
                assert_eq!(
                    frame_device.as_raw(),
                    device.d3d11_device(),
                    "프레임 {i}: 파이프라인이 주입 디바이스를 쓰지 않음 (context 주입 실패)"
                );

                // 변환기 lazy 생성 (in = 디코더 원 포맷, out = RGBA 동일 크기)
                if converter.is_none() {
                    let out_info = gstreamer_video::VideoInfo::builder(
                        gstreamer_video::VideoFormat::Rgba,
                        info.width(),
                        info.height(),
                    )
                    .build()
                    .expect("out VideoInfo 실패");
                    let conv = (api.converter_new)(
                        device.raw(),
                        info.to_glib_none().0,
                        out_info.to_glib_none().0,
                        std::ptr::null_mut(),
                    );
                    assert!(!conv.is_null(), "gst_d3d11_converter_new 실패");
                    converter = Some(conv);
                }

                // 링 슬롯 확보 → 변환 렌더(YUV→RGBA, 슬롯에 직접) → 완료 fence
                let (out_buffer, slot_index) = ring.acquire(width, height).expect("슬롯 확보 실패");
                let ok = (api.converter_convert_buffer)(
                    converter.unwrap(),
                    buffer.as_mut_ptr(),
                    out_buffer.as_mut_ptr(),
                );
                assert_ne!(ok, 0, "프레임 {i}: convert_buffer 실패 (Step 3 실패 분기 참조)");
                let (handle, epoch) = ring.finish(slot_index).expect("완료 fence 실패");
                last = Some((handle, epoch, info.width(), info.height()));
            }
        }

        let (handle, epoch, width, height) = last.expect("프레임 0개");
        let readback = open_and_read_on_second_device(handle, width, height);
        let non_black = readback
            .chunks_exact(4)
            .filter(|px| px[0] > 8 || px[1] > 8 || px[2] > 8)
            .count();
        let pct = non_black as f64 * 100.0 / (width as f64 * height as f64);
        pipeline.set_state(gstreamer::State::Null).ok();

        println!(
            "POC OK: frames={FRAMES_TO_CHECK} size={width}x{height} epoch={epoch} nonblack={pct:.1}%"
        );
        assert!(pct > 30.0, "판독 결과가 거의 검정 ({pct:.1}%) — 복사/동기화 문제");
    }

    fn open_and_read_on_second_device(handle: u64, width: u32, height: u32) -> Vec<u8> {
        // Task 2 테스트의 판독 헬퍼와 동일 구현 (여기 복제 — 예제는 테스트 코드를 import 못 함)
        unsafe {
            let mut device = std::ptr::null_mut();
            let mut context = std::ptr::null_mut();
            let hr = d3d::D3D11CreateDevice(
                std::ptr::null_mut(),
                D3D_DRIVER_TYPE_HARDWARE,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
                0,
                d3d::D3D11_SDK_VERSION,
                &mut device,
                std::ptr::null_mut(),
                &mut context,
            );
            assert_eq!(hr, 0, "두 번째 디바이스 생성 실패 hr={hr:#x}");
            let device = ComPtr::from_raw(device);
            let context = ComPtr::from_raw(context);

            let mut opened: *mut winapi::ctypes::c_void = std::ptr::null_mut();
            let hr = device.OpenSharedResource(
                handle as winapi::shared::ntdef::HANDLE,
                &d3d::ID3D11Texture2D::uuidof(),
                &mut opened,
            );
            assert_eq!(hr, 0, "OpenSharedResource 실패 hr={hr:#x}");
            let opened = ComPtr::from_raw(opened as *mut d3d::ID3D11Texture2D);

            let desc = d3d::D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: d3d::D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: d3d::D3D11_CPU_ACCESS_READ,
                MiscFlags: 0,
            };
            let mut staging = std::ptr::null_mut();
            let hr = device.CreateTexture2D(&desc, std::ptr::null(), &mut staging);
            assert_eq!(hr, 0, "staging 생성 실패 hr={hr:#x}");
            let staging = ComPtr::from_raw(staging);

            context.CopyResource(staging.as_raw() as *mut _, opened.as_raw() as *mut _);
            let mut mapped = std::mem::zeroed::<d3d::D3D11_MAPPED_SUBRESOURCE>();
            let hr = context.Map(staging.as_raw() as *mut _, 0, d3d::D3D11_MAP_READ, 0, &mut mapped);
            assert_eq!(hr, 0, "Map 실패 hr={hr:#x}");
            let mut out = vec![0u8; (width * height * 4) as usize];
            for row in 0..height as usize {
                let src_row = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
                let dst = &mut out[row * width as usize * 4..(row + 1) * width as usize * 4];
                std::ptr::copy_nonoverlapping(src_row, dst.as_mut_ptr(), width as usize * 4);
            }
            context.Unmap(staging.as_raw() as *mut _, 0);
            out
        }
    }
}
