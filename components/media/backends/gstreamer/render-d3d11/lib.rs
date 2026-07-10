/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Windows용 D3D11 비디오 렌더 경로.
//!
//! 파이프라인의 스트리밍 스레드에서 d3d11upload/d3d11convert로 GPU 업로드·RGBA 변환을
//! 수행하고, 공유 텍스처 링에 복사한 뒤 공유 핸들만 Servo로 전달한다. 렌더러 스레드의
//! 비디오 업로드(glTexSubImage2D)를 제거하는 것이 목적. env `SERVO_MEDIA_D3D11_VIDEO=1`
//! 게이트 (기본 off). 설계: docs/superpowers/specs/2026-07-09-d3d11-per-pipeline-upload-design.md

// Windows 전용 — 다른 타겟에서는 빈 크레이트로 컴파일된다 (workspace member라
// 비Windows `--workspace` 빌드에도 포함되므로 게이트 필수).
#[cfg(windows)]
pub mod ffi;
#[cfg(windows)]
pub mod interop;

#[cfg(windows)]
pub use interop::{SharedGstD3D11Device, SharedTextureRing};

// RenderD3D11 본체 — interop/ffi와 마찬가지로 Windows 전용. 비Windows에서는 이 모듈
// 자체가 컴파일되지 않아 크레이트가 계속 빈 크레이트로 유지된다.
#[cfg(windows)]
mod render_d3d11 {
    use std::env;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use gstreamer::glib::translate::ToGlibPtr;
    use gstreamer::prelude::*;
    use servo_media_gstreamer_render::Render;
    use servo_media_player::PlayerError;
    use servo_media_player::video::{Buffer, VideoFrame, VideoFrameD3D11Data, VideoFrameData};

    // 부모 모듈(lib.rs)의 `pub use interop::{SharedGstD3D11Device, SharedTextureRing};`를
    // 가져온다.
    use super::*;
    use crate::ffi::GstD3D11Converter;

    const D3D11_VIDEO_ENV: &str = "SERVO_MEDIA_D3D11_VIDEO";

    // D3D11PROF: 파이프라인(플레이어) 식별자 발급기 — 로그에서 타일 구분용 (임시 계측).
    static PROFILE_ID_SEQ: AtomicU32 = AtomicU32::new(0);

    // D3D11PROF: 파이프라인별 하트비트 스로틀. 각 파이프라인의 build_frame은 전용
    // 스트리밍 스레드에서 불리므로 thread_local이 자연스럽게 파이프라인당 1개다.
    thread_local! {
        static LAST_PROF_LOG: std::cell::Cell<Option<std::time::Instant>> =
            const { std::cell::Cell::new(None) };
        // D3D11PROF: 직전 로그 이후 이 파이프라인이 완성한 프레임 수 — 로그 라인 끝
        // fr=N 필드로 실려 프로듀서 도착률(프레임/s) 복원용 (§12 Q1 판별, 임시 계측).
        static FRAMES_SINCE_LOG: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    fn env_flag_enabled(name: &str) -> bool {
        env::var(name).is_ok_and(|value| {
            value == "1"
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes")
                || value.eq_ignore_ascii_case("on")
        })
    }

    struct D3D11FrameBuffer {
        data: VideoFrameD3D11Data,
    }

    impl Buffer for D3D11FrameBuffer {
        fn frame_data(&self) -> Option<VideoFrameData> {
            Some(VideoFrameData::D3D11(self.data))
        }
    }

    /// GstD3D11Converter 소유 핸들. GstObject 파생이라 g_object_unref로 해제.
    struct ConverterHandle(*mut GstD3D11Converter);

    impl Drop for ConverterHandle {
        fn drop(&mut self) {
            unsafe { gstreamer::glib::gobject_ffi::g_object_unref(self.0 as *mut _) }
        }
    }

    // 안전성: 변환기는 스트리밍 스레드 1개에서만 사용하며 PlayerState Mutex로 보호된다.
    unsafe impl Send for ConverterHandle {}

    struct PlayerState {
        ring: Option<SharedTextureRing>,
        converter: Option<ConverterHandle>,
        /// 변환기 무효화 판정용 — 포맷/크기/colorimetry 변경 감지.
        in_caps: Option<gstreamer::Caps>,
    }

    pub struct RenderD3D11 {
        device: Arc<SharedGstD3D11Device>,
        // 플레이어당 링+변환기. build_frame은 스트리밍 스레드 1개에서만 불리지만
        // Render는 &self라 내부 가변성 필요.
        state: Mutex<PlayerState>,
        // D3D11PROF: 이 플레이어의 파이프라인 식별자 (임시 계측).
        profile_id: u32,
    }

    impl RenderD3D11 {
        /// env 게이트 + 사전 점검. 하나라도 실패하면 None → 기존 CPU(Raw) 경로 폴백.
        pub fn new() -> Option<RenderD3D11> {
            if !env_flag_enabled(D3D11_VIDEO_ENV) {
                return None;
            }
            let options = servo_config::opts::get();
            if options.multiprocess || options.force_ipc {
                log::warn!("D3D11 video: 단일 프로세스 전용 — Raw 경로 폴백");
                return None;
            }
            // 변환은 라이브러리 GstD3D11Converter를 직접 쓰므로 엘리먼트는 d3d11upload만 필요.
            if gstreamer::ElementFactory::find("d3d11upload").is_none() {
                log::warn!(
                    "D3D11 video: d3d11upload 플러그인 없음 (gstd3d11.dll 번들 확인) — Raw 경로 폴백"
                );
                return None;
            }
            // 파이프라인(플레이어)별 전용 디바이스 — 프로세스 전역 공유 디바이스는
            // 45+ lockstep 루프 경계에서 단일 락 콘보이로 포화됨이 실측 확정
            // (interop.rs 모듈 doc comment 참조). 플레이어 Drop 시 디바이스도 해제된다.
            let device = SharedGstD3D11Device::create()?;
            let profile_id = PROFILE_ID_SEQ.fetch_add(1, Ordering::Relaxed);
            log::info!("D3D11 video: 파이프라인별 GPU 업로드 경로 활성 (profile_id={profile_id})");
            Some(RenderD3D11 {
                device,
                state: Mutex::new(PlayerState {
                    ring: None,
                    converter: None,
                    in_caps: None,
                }),
                profile_id,
            })
        }
    }

    impl Render for RenderD3D11 {
        fn is_gl(&self) -> bool {
            false
        }

        fn build_frame(&self, sample: gstreamer::Sample) -> Option<VideoFrame> {
            let prof = crate::interop::profile_enabled(); // D3D11PROF
            let bf_start = std::time::Instant::now(); // D3D11PROF
            let buffer = sample.buffer()?;
            if buffer.n_memory() == 0 {
                return None;
            }
            let caps = sample.caps()?;
            let info = gstreamer_video::VideoInfo::from_caps(caps).ok()?;
            let width = info.width() as i32;
            let height = info.height() as i32;
            let api = self.device.api();

            if unsafe { (api.is_d3d11_memory)(buffer.peek_memory(0).as_mut_ptr()) } == 0 {
                log::warn!("D3D11 video: 비 D3D11 메모리 샘플 — 프레임 드롭");
                return None;
            }

            let mut state = self.state.lock().unwrap();
            let state = &mut *state;

            // caps 변경(포맷/크기/색상 정보) 시 변환기 재생성
            if state.in_caps.as_deref() != Some(caps) {
                state.converter = None;
                state.in_caps = Some(caps.to_owned());
            }
            let ring = state
                .ring
                .get_or_insert_with(|| SharedTextureRing::new(self.device.clone()));
            ring.profile_id = self.profile_id; // D3D11PROF
            let acq_start = std::time::Instant::now(); // D3D11PROF
            let (out_buffer, slot_index) = ring.acquire(width, height)?;
            let t_acquire = acq_start.elapsed(); // D3D11PROF (recreate 포함)

            if state.converter.is_none() {
                let out_info = gstreamer_video::VideoInfo::builder(
                    gstreamer_video::VideoFormat::Rgba,
                    info.width(),
                    info.height(),
                )
                .build()
                .ok()?;
                // VideoInfo가 ToGlibPtr 미구현으로 컴파일 실패하면
                // `&info as *const _ as *const gstreamer_video::ffi::GstVideoInfo`로 조정.
                let raw = unsafe {
                    (api.converter_new)(
                        self.device.raw(),
                        info.to_glib_none().0,
                        out_info.to_glib_none().0,
                        std::ptr::null_mut(),
                    )
                };
                if raw.is_null() {
                    log::warn!("D3D11 video: converter 생성 실패 — 프레임 드롭");
                    return None;
                }
                state.converter = Some(ConverterHandle(raw));
            }
            let converter = state.converter.as_ref()?;

            // YUV→RGBA 변환을 공유 링 슬롯에 직접 렌더 (추가 복사 없음).
            //
            // gst_d3d11_converter_convert_buffer(일반 변형)는 내부적으로 디바이스 락을
            // 잡는다 — 짝 API `_unlocked`의 존재가 근거(gstd3d11converter.h:194). 그러므로
            // 여기서 device.lock()으로 감싸지 않는다(이중 락으로 인한 데드락 방지).
            let conv_start = std::time::Instant::now(); // D3D11PROF
            let ok = unsafe {
                (api.converter_convert_buffer)(
                    converter.0,
                    buffer.as_mut_ptr(),
                    out_buffer.as_mut_ptr(),
                )
            };
            let t_convert = conv_start.elapsed(); // D3D11PROF (convert 내부 디바이스 락 대기 포함)
            if ok == 0 {
                log::warn!("D3D11 video: convert_buffer 실패 — 프레임 드롭");
                return None;
            }
            let fin_start = std::time::Instant::now(); // D3D11PROF
            let (shared_handle, ring_epoch) = ring.finish(slot_index)?;
            let t_finish = fin_start.elapsed(); // D3D11PROF

            // D3D11PROF: 임계 초과 프레임(=스톨 후보)은 항상 로깅 + 정착 베이스라인용
            // 파이프라인당 ~1초 하트비트 1줄(정착 분포 확보). over=1 이면 임계 초과.
            // 판정: convert/ef_lockwait/poll_lockwait 지배=H1 락 콘보이, poll_lockwait+
            // polls 폭증=H2 스핀 폭풍, fence_loop 크고 lockwait 작음=H3 GPU 큐 포화.
            if prof {
                let total_ms = bf_start.elapsed().as_secs_f64() * 1000.0;
                let over = total_ms >= crate::interop::profile_threshold_ms();
                // 프로듀서 도착률 복원용 프레임 카운트 (fr= 필드, §12 Q1).
                let frames = FRAMES_SINCE_LOG.with(|c| {
                    let n = c.get() + 1;
                    c.set(n);
                    n
                });
                // 하트비트: 이 스트리밍 스레드(=파이프라인)에서 마지막 로그 후 1초 경과 시 1줄.
                let heartbeat = LAST_PROF_LOG.with(|c| match c.get() {
                    Some(t) if t.elapsed() < std::time::Duration::from_secs(1) => false,
                    _ => true,
                });
                if over || heartbeat {
                    LAST_PROF_LOG.with(|c| c.set(Some(std::time::Instant::now())));
                    FRAMES_SINCE_LOG.with(|c| c.set(0));
                    let st = ring.last_stats;
                    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
                    log::warn!(
                        "D3D11PROF id={} over={} total={:.1} acquire={:.1} convert={:.1} \
                         finish={:.1} ef_lockwait={:.2} poll_lockwait={:.2} fence_loop={:.1} \
                         polls={} fr={}",
                        self.profile_id,
                        if over { 1 } else { 0 },
                        total_ms,
                        t_acquire.as_secs_f64() * 1000.0,
                        t_convert.as_secs_f64() * 1000.0,
                        t_finish.as_secs_f64() * 1000.0,
                        ms(st.endflush_lock_wait),
                        ms(st.poll_lock_wait),
                        ms(st.fence_loop),
                        st.poll_count,
                        frames,
                    );
                }
            }
            // gst 버퍼(sample)는 여기서 스코프를 벗어나며 즉시 풀로 반환된다 — 프레임
            // 수명이 렌더러와 분리되는 것이 링 설계의 핵심 이점.
            VideoFrame::new(
                width,
                height,
                Arc::new(D3D11FrameBuffer {
                    data: VideoFrameD3D11Data {
                        shared_handle,
                        ring_epoch,
                    },
                }),
            )
        }

        fn build_video_sink(
            &self,
            appsink: &gstreamer::Element,
            pipeline: &gstreamer::Element,
        ) -> Result<(), PlayerError> {
            let bin = gstreamer::Bin::builder()
                .name("servo-d3d11-video-sink")
                .build();
            let upload = gstreamer::ElementFactory::make("d3d11upload")
                .build()
                .map_err(|error| PlayerError::Backend(format!("d3d11upload 생성 실패: {error:?}")))?;

            // format 미지정: 디코더 원 포맷(I420/NV12 등)을 D3D11 메모리로 그대로 받는다.
            // RGBA 변환은 build_frame의 GstD3D11Converter가 링 슬롯에 직접 수행.
            let caps = gstreamer::Caps::builder("video/x-raw")
                .features(["memory:D3D11Memory"])
                .field("pixel-aspect-ratio", gstreamer::Fraction::from((1, 1)))
                .build();
            appsink.set_property("caps", &caps);

            bin.add_many([&upload, appsink])
                .map_err(|error| PlayerError::Backend(format!("bin add 실패: {error:?}")))?;
            upload
                .link(appsink)
                .map_err(|error| PlayerError::Backend(format!("bin link 실패: {error:?}")))?;

            let upload_sink = upload
                .static_pad("sink")
                .ok_or_else(|| PlayerError::Backend("d3d11upload sink pad 없음".to_owned()))?;
            let ghost_pad = gstreamer::GhostPad::builder_with_target(&upload_sink)
                .map_err(|error| PlayerError::Backend(format!("ghost pad 실패: {error:?}")))?
                .name("sink")
                .build();
            bin.add_pad(&ghost_pad)
                .map_err(|error| PlayerError::Backend(format!("ghost pad add 실패: {error:?}")))?;

            // 우리 디바이스를 파이프라인 전체에 주입 — PoC(Task 3)에서 검증된 방식.
            if let Some(context) = self.device.gst_context() {
                pipeline.set_context(&context);
            }
            pipeline.set_property("video-sink", &bin);
            Ok(())
        }
    }
}

#[cfg(windows)]
pub use render_d3d11::RenderD3D11;
