/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Windows용 D3D11 비디오 렌더 경로 (WR YUV 직접 샘플).
//!
//! appsink가 sysmem 프레임을 받고, build_frame이 각 plane을 소비자(ANGLE)
//! 디바이스 위의 DYNAMIC plane 텍스처에 CPU memcpy만 한다. 변환·fence·RGBA
//! 공유 링은 없다 — WebRender가 이 plane 텍스처를 직접 YUV 샘플한다.
//! 슬롯 상태기계와 Map/Unmap(소비자측)은 `servo_media_player::d3d11_ring`
//! 레지스트리가 담당하며, 프로듀서는 D3D immediate context를 전혀 만지지
//! 않는다(유일한 D3D 호출은 링 (재)생성 시의 free-threaded `CreateTexture2D`).
//! env `SERVO_MEDIA_D3D11_VIDEO=1` 게이트 (기본 off).
//! 설계: docs/superpowers/plans/2026-07-12-wr-yuv-direct-sample.md

// Windows 전용 — 다른 타겟에서는 빈 크레이트로 컴파일된다 (workspace member라
// 비Windows `--workspace` 빌드에도 포함되므로 게이트 필수).
#[cfg(windows)]
mod ring_producer;

// RenderD3D11 본체 — ring_producer와 마찬가지로 Windows 전용. 비Windows에서는
// 이 모듈 자체가 컴파일되지 않아 크레이트가 계속 빈 크레이트로 유지된다.
#[cfg(windows)]
mod render_d3d11 {
    use std::env;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    use gstreamer::prelude::*;
    use servo_media_gstreamer_render::Render;
    use servo_media_player::PlayerError;
    use servo_media_player::d3d11_ring::D3d11PlaneRings;
    use servo_media_player::video::{
        Buffer, VideoFrame, VideoFrameD3D11YuvData, VideoFrameData, VideoFrameYuvColorRange,
        VideoFrameYuvColorSpace, VideoFrameYuvFormat,
    };

    use crate::ring_producer;

    const D3D11_VIDEO_ENV: &str = "SERVO_MEDIA_D3D11_VIDEO";

    // D3D11PROF: 파이프라인(플레이어) 식별자 발급기 — 로그에서 타일 구분용 (임시 계측).
    static PROFILE_ID_SEQ: AtomicU32 = AtomicU32::new(0);

    // D3D11PROF: 파이프라인별 하트비트 스로틀. 각 파이프라인의 build_frame은 전용
    // 스트리밍 스레드에서 불리므로 thread_local이 자연스럽게 파이프라인당 1개다.
    thread_local! {
        static LAST_PROF_LOG: std::cell::Cell<Option<std::time::Instant>> =
            const { std::cell::Cell::new(None) };
        // D3D11PROF: 직전 로그 이후 이 파이프라인이 완성한 프레임 수 — 로그 라인 끝
        // fr=N 필드로 실려 프로듀서 도착률(프레임/s) 복원용 (임시 계측).
        static FRAMES_SINCE_LOG: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        // D3D11PROF(§14): 직전 샘플 도착 시각/pts — 도착 갭(>200ms)과 루프 wrap(pts
        // 역행) 감지용. 스트리밍 스레드=파이프라인 1:1이라 thread_local로 충분.
        static LAST_ARRIVAL: std::cell::Cell<Option<std::time::Instant>> =
            const { std::cell::Cell::new(None) };
        static LAST_PTS_MS: std::cell::Cell<f64> = const { std::cell::Cell::new(-1.0) };
    }

    fn env_flag_enabled(name: &str) -> bool {
        env::var(name).is_ok_and(|value| {
            value == "1"
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes")
                || value.eq_ignore_ascii_case("on")
        })
    }

    /// SERVO_D3D11_PROFILE=1 이면 단계별 타이밍을 측정/로깅한다 (프로세스 1회 판정).
    fn profile_enabled() -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| {
            env::var("SERVO_D3D11_PROFILE").is_ok_and(|v| {
                v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on")
            })
        })
    }

    /// build_frame 총시간이 이 임계(ms) 이상일 때만 로깅 (로그 폭주 방지).
    /// SERVO_D3D11_PROFILE_MS 로 조정, 기본 8ms.
    fn profile_threshold_ms() -> f64 {
        static T: OnceLock<f64> = OnceLock::new();
        *T.get_or_init(|| {
            env::var("SERVO_D3D11_PROFILE_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8.0)
        })
    }

    /// colorimetry 매핑 (components/media/backends/gstreamer/render.rs:240-246과 동일).
    fn colorimetry_from_info(
        info: &gstreamer_video::VideoInfo,
    ) -> (VideoFrameYuvColorSpace, VideoFrameYuvColorRange) {
        let color_space = match info.colorimetry().matrix() {
            gstreamer_video::VideoColorMatrix::Bt709 => VideoFrameYuvColorSpace::Rec709,
            gstreamer_video::VideoColorMatrix::Bt2020 => VideoFrameYuvColorSpace::Rec2020,
            _ => VideoFrameYuvColorSpace::Rec601,
        };
        let color_range = match info.colorimetry().range() {
            gstreamer_video::VideoColorRange::Range0_255 => VideoFrameYuvColorRange::Full,
            _ => VideoFrameYuvColorRange::Limited,
        };
        (color_space, color_range)
    }

    /// D3D11Yuv 프레임 페이로드 — 링 참조 + 표시 메타데이터만 싣는다(픽셀 없음).
    struct D3D11YuvFrameBuffer {
        data: VideoFrameD3D11YuvData,
    }

    impl Buffer for D3D11YuvFrameBuffer {
        fn frame_data(&self) -> Option<VideoFrameData> {
            Some(VideoFrameData::D3D11Yuv(self.data))
        }
    }

    struct PlayerState {
        /// 현재 활성 plane 링(없으면 아직 미생성/디바이스 대기).
        ring_id: Option<u64>,
        /// 현재 링을 만든 caps(포맷/크기/색상). 변경되면 링 교체.
        in_caps: Option<gstreamer::Caps>,
        /// 링이 아직 한 번도 소비되지 않음(전 슬롯 Unmapped, claim이 항상 None).
        /// true인 동안은 드롭 대신 첫 프레임을 스테이징한다.
        ring_never_consumed: bool,
        /// 현재 링 생성 이후의 배압 드롭(Free 슬롯 없음) 누계.
        drop_count: u64,
        /// 소비자 디바이스 미발행 경고 1회 래치(파이프라인당).
        warned_no_device: bool,
        /// gst 버퍼 map 실패 경고 1회 래치(파이프라인당) — 로그 폭주 방지.
        warned_map_fail: bool,
    }

    pub struct RenderD3D11 {
        // 플레이어당 링 상태. build_frame은 스트리밍 스레드 1개에서만 불리지만
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
            let profile_id = PROFILE_ID_SEQ.fetch_add(1, Ordering::Relaxed);
            log::info!("D3D11 video: plane 링 프로듀서 경로 활성 (profile_id={profile_id})");
            Some(RenderD3D11 {
                state: Mutex::new(PlayerState {
                    ring_id: None,
                    in_caps: None,
                    ring_never_consumed: true,
                    drop_count: 0,
                    warned_no_device: false,
                    warned_map_fail: false,
                }),
                profile_id,
            })
        }

        /// 현재 caps에 맞는 링을 보장한다(필요 시 (재)생성). 소비자(ANGLE)
        /// 디바이스가 아직 발행되지 않았으면 실패를 캐시하지 않고 None을 반환해
        /// 다음 프레임에서 재시도한다.
        fn ensure_ring(
            &self,
            state: &mut PlayerState,
            caps: &gstreamer::CapsRef,
            format: VideoFrameYuvFormat,
            width: i32,
            height: i32,
        ) -> Option<u64> {
            // 기존 링이 현재 caps에 유효.
            if state.ring_id.is_some() && state.in_caps.as_deref() == Some(caps) {
                return state.ring_id;
            }
            // (재)생성 필요 — 소비자 디바이스가 먼저 발행돼 있어야 한다. 없으면
            // 실패를 캐시하지 않고(in_caps 갱신 안 함) 드롭 후 다음 프레임 재시도.
            let device = match D3d11PlaneRings::consumer_device() {
                Some(device) => device,
                None => {
                    if !state.warned_no_device {
                        log::warn!(
                            "D3D11 video: 소비자(ANGLE) 디바이스 미발행 — 렌더러 준비 전까지 드롭 (id={})",
                            self.profile_id
                        );
                        state.warned_no_device = true;
                    }
                    return None;
                },
            };
            // 구 링 회수(실제 Unmap/Release는 렌더러가 take_removed_rings로 수행).
            if let Some(old) = state.ring_id.take() {
                D3d11PlaneRings::remove_ring(old);
                state.in_caps = None;
            }
            let slots = ring_producer::create_plane_textures(device, format, width, height)?;
            let ring_id = D3d11PlaneRings::create_ring(format.plane_count(), slots);
            state.ring_id = Some(ring_id);
            state.in_caps = Some(caps.to_owned());
            state.ring_never_consumed = true;
            state.drop_count = 0;
            log::info!(
                "D3D11 video: plane 링 생성 ring_id={ring_id} {width}x{height} {format:?} (id={})",
                self.profile_id
            );
            Some(ring_id)
        }

        /// D3D11PROF: claim/copy/publish 타이밍 + 드롭 카운터 하트비트(1초 1줄 +
        /// 임계 초과 프레임). profile on일 때만 호출.
        #[allow(clippy::too_many_arguments)]
        fn prof_log(
            &self,
            state: &PlayerState,
            ring_id: u64,
            bf_start: std::time::Instant,
            t_claim: std::time::Duration,
            t_copy: std::time::Duration,
            t_publish: std::time::Duration,
        ) {
            let total_ms = bf_start.elapsed().as_secs_f64() * 1000.0;
            let over = total_ms >= profile_threshold_ms();
            let frames = FRAMES_SINCE_LOG.with(|c| {
                let n = c.get() + 1;
                c.set(n);
                n
            });
            let heartbeat = LAST_PROF_LOG.with(|c| match c.get() {
                Some(t) if t.elapsed() < std::time::Duration::from_secs(1) => false,
                _ => true,
            });
            if over || heartbeat {
                LAST_PROF_LOG.with(|c| c.set(Some(std::time::Instant::now())));
                FRAMES_SINCE_LOG.with(|c| c.set(0));
                let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
                log::warn!(
                    "D3D11PROF id={} over={} total={:.2} claim={:.3} copy={:.2} \
                     publish={:.3} drops={} regdrops={} fr={}",
                    self.profile_id,
                    if over { 1 } else { 0 },
                    total_ms,
                    ms(t_claim),
                    ms(t_copy),
                    ms(t_publish),
                    state.drop_count,
                    D3d11PlaneRings::dropped_frames(ring_id),
                    frames,
                );
            }
        }
    }

    impl Drop for RenderD3D11 {
        fn drop(&mut self) {
            // 플레이어 해체(엘리먼트 제거 / 페이지 내비게이션): 활성 링을
            // 레지스트리에 반납해 렌더러의 take_removed_rings 프롤로그가
            // Unmap + 텍스처 Release를 수행하게 한다. remove_ring은 그 외에는
            // caps 변경 시에만 불리므로, 이 Drop이 없으면 링(DYNAMIC 텍스처
            // 8~12개 + 레지스트리 엔트리 + 살아있는 매핑)이 영구히 누수된다.
            // Drop 안에서 unwrap하지 않고 포이즌을 복구한다 — 소멸자에서
            // (특히 언와인딩 중) 패닉하면 프로세스가 abort되며, 그래도 링은
            // 반드시 반납해야 하기 때문이다.
            let ring_id = self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .ring_id
                .take();
            if let Some(ring_id) = ring_id {
                D3d11PlaneRings::remove_ring(ring_id);
            }
        }
    }

    impl Render for RenderD3D11 {
        fn is_gl(&self) -> bool {
            false
        }

        fn build_frame(&self, sample: gstreamer::Sample) -> Option<VideoFrame> {
            let prof = profile_enabled(); // D3D11PROF
            let bf_start = std::time::Instant::now(); // D3D11PROF
            let buffer = sample.buffer()?;
            if buffer.n_memory() == 0 {
                return None;
            }
            // D3D11PROF(§14): 도착 갭(>200ms) + 루프 wrap(pts 역행) 이벤트 로깅 —
            // 개별 타일 간헐 멈춤과 gapless 되감기의 상관 판정용 (임시 계측).
            if prof {
                let pts_ms = buffer
                    .pts()
                    .map(|t| t.nseconds() as f64 / 1_000_000.0)
                    .unwrap_or(-1.0);
                let prev_pts = LAST_PTS_MS.with(|c| c.replace(pts_ms));
                if pts_ms >= 0.0 && prev_pts >= 0.0 && pts_ms < prev_pts - 1000.0 {
                    log::warn!(
                        "D3D11PROF wrap id={} pts_ms={pts_ms:.0} prev_pts_ms={prev_pts:.0}",
                        self.profile_id
                    );
                }
                let prev_arrival = LAST_ARRIVAL.with(|c| c.replace(Some(bf_start)));
                if let Some(prev) = prev_arrival {
                    let gap_ms = (bf_start - prev).as_secs_f64() * 1000.0;
                    if gap_ms > 200.0 {
                        log::warn!(
                            "D3D11PROF arrgap id={} gap_ms={gap_ms:.0} pts_ms={pts_ms:.0}",
                            self.profile_id
                        );
                    }
                }
            }

            let caps = sample.caps()?;
            let info = gstreamer_video::VideoInfo::from_caps(caps).ok()?;
            let width = info.width() as i32;
            let height = info.height() as i32;

            // 표시 계약은 I420(Y,U,V)/NV12. YV12는 gst plane 순서가 Y,V,U이므로
            // 복사 시 U/V를 스왑해 계약에 맞춘다(swap_uv).
            let (format, swap_uv) = match info.format() {
                gstreamer_video::VideoFormat::I420 => (VideoFrameYuvFormat::I420, false),
                gstreamer_video::VideoFormat::Yv12 => (VideoFrameYuvFormat::I420, true),
                gstreamer_video::VideoFormat::Nv12 => (VideoFrameYuvFormat::NV12, false),
                other => {
                    log::warn!("D3D11 video: 미지원 포맷 {other:?} — 드롭");
                    return None;
                },
            };
            let (color_space, color_range) = colorimetry_from_info(&info);

            let mut state = self.state.lock().unwrap();
            let state = &mut *state;

            let ring_id = self.ensure_ring(state, caps, format, width, height)?;

            // 여기서만 gst 버퍼를 readable로 map한다 — 바이트는 아래에서 즉시
            // 슬롯으로 복사되고 build_frame 반환과 함께 샘플이 풀로 돌아간다.
            // map 실패(일시적 gst 버퍼 오류)는 배압 arm(위 None arm)과 동일한
            // 위험을 가진다: 여기서 None을 반환하면 appsink 콜백(player.rs
            // render_sample)이 치명적 FlowError::Error로 바꾸고 그 -5가 상류
            // qtdemux_loop를 중단시켜 파이프라인 전체가 정지한다(bf70293c4와
            // 같은 계열의 잔여 경로, Task8 리뷰에서 지목). 링이 이미 한 번
            // 이상 소비돼 유효한 Presenting 슬롯이 있다면(ring_never_consumed
            // == false) 새 프레임 복사를 생략하고 기존 슬롯을 그대로 재표시해
            // 스트림을 유지한다. 아직 한 번도 소비되지 않았다면(startup) 재표시할
            // 슬롯이 없으므로 기존 동작대로 None을 반환한다(startup-window
            // 잔여 경로 — 의도적으로 미변경).
            let frame =
                match gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info) {
                    Ok(frame) => frame,
                    Err(_) => {
                        if state.ring_never_consumed {
                            return None;
                        }
                        state.drop_count += 1;
                        if !state.warned_map_fail {
                            log::warn!(
                                "D3D11 video: gst 버퍼 map 실패 — 기존 Presenting 슬롯 재표시로 폴백 (id={})",
                                self.profile_id
                            );
                            state.warned_map_fail = true;
                        }
                        if prof {
                            log::warn!(
                                "D3D11PROF mapfail id={} ring={ring_id} drops={} regdrops={}",
                                self.profile_id,
                                state.drop_count,
                                D3d11PlaneRings::dropped_frames(ring_id),
                            );
                        }
                        // 아래와 동일한 메타데이터 프레임을 즉시 반환 — 새 프레임
                        // 복사 없이 재표시로 스트림 유지(배압 arm과 동형).
                        return VideoFrame::new(
                            width,
                            height,
                            Arc::new(D3D11YuvFrameBuffer {
                                data: VideoFrameD3D11YuvData {
                                    ring_id,
                                    ring_epoch: 1,
                                    format,
                                    color_space,
                                    color_range,
                                },
                            }),
                        );
                    },
                };

            let claim_start = std::time::Instant::now(); // D3D11PROF
            match D3d11PlaneRings::claim_free_slot(ring_id) {
                Some(claimed) => {
                    let t_claim = claim_start.elapsed(); // D3D11PROF
                    let slot = claimed.slot;
                    let copy_start = std::time::Instant::now(); // D3D11PROF
                    ring_producer::copy_planes(&frame, format, swap_uv, &claimed);
                    let t_copy = copy_start.elapsed(); // D3D11PROF
                    let pub_start = std::time::Instant::now(); // D3D11PROF
                    D3d11PlaneRings::publish_slot(ring_id, slot);
                    let t_publish = pub_start.elapsed(); // D3D11PROF
                    // 첫 성공 claim = 링이 소비되기 시작했다는 신호.
                    state.ring_never_consumed = false;
                    if prof {
                        self.prof_log(state, ring_id, bf_start, t_claim, t_copy, t_publish);
                    }
                },
                None if state.ring_never_consumed => {
                    // 초기 구간(전 슬롯 Unmapped): 첫 프레임을 CPU에 스테이징해 두면
                    // 렌더러의 InitialMapAll이 표시한다. 첫 성공 claim 전까지 매
                    // 프레임 덮어쓴다.
                    let vecs = ring_producer::planes_to_vecs(&frame, format, swap_uv);
                    D3d11PlaneRings::stage_first_frame(ring_id, vecs);
                },
                None => {
                    // 배압: 모든 슬롯이 아직 소비 대기(Published) 상태다. 이번 gst
                    // 프레임 바이트는 버리되(memcpy 전 — 비용 없음) None을 반환하지
                    // 않는다. appsink 콜백은 None(=get_frame_from_sample 실패)을 치명적
                    // FlowError::Error로 바꾸고, 그 -5가 상류 qtdemux_loop를 중단시켜
                    // 파이프라인 전체가 정지한다(45타일에서 렌더러가 못 따라오면 첫
                    // 배압 드롭 한 번에 전 타일 정지 — 통합검증에서 관측·규명). 대신
                    // 아래 링 기술자를 그대로 반환해 렌더러가 직전 Presenting 슬롯을
                    // 재표시하게 한다(정상적 프레임 드롭). 재표시 합성이 슬롯 1개를
                    // 소비하므로 배압도 자연히 완화된다. 이 arm은 ring_never_consumed
                    // == false일 때만 도달하므로 Presenting 슬롯이 항상 존재한다.
                    state.drop_count += 1;
                    if prof {
                        log::warn!(
                            "D3D11PROF drop id={} ring={ring_id} drops={} regdrops={}",
                            self.profile_id,
                            state.drop_count,
                            D3d11PlaneRings::dropped_frames(ring_id),
                        );
                    }
                    // 아래 VideoFrame::new로 흘려보낸다 — 재표시로 스트림 유지.
                },
            }

            VideoFrame::new(
                width,
                height,
                Arc::new(D3D11YuvFrameBuffer {
                    data: VideoFrameD3D11YuvData {
                        ring_id,
                        ring_epoch: 1,
                        format,
                        color_space,
                        color_range,
                    },
                }),
            )
        }

        fn build_video_sink(
            &self,
            appsink: &gstreamer::Element,
            pipeline: &gstreamer::Element,
        ) -> Result<(), PlayerError> {
            // appsink가 sysmem 프레임을 직접 받고 build_frame이 DYNAMIC plane
            // 텍스처로 memcpy한다. 포맷 목록 밖 디코더 출력은 playbin이 videoconvert를
            // 자동 삽입해 목록 내 포맷으로 맞춘다.
            let caps = gstreamer::Caps::builder("video/x-raw")
                .field("format", gstreamer::List::new(["I420", "YV12", "NV12"]))
                .field("pixel-aspect-ratio", gstreamer::Fraction::from((1, 1)))
                .build();
            appsink.set_property("caps", &caps);
            pipeline.set_property("video-sink", appsink);
            Ok(())
        }
    }
}

#[cfg(windows)]
pub use render_d3d11::RenderD3D11;
