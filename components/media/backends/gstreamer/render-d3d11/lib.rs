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
//! pref `media_d3d11_enabled=true` 게이트 (기본 off, 구 env `SERVO_MEDIA_D3D11_VIDEO`).
//! 설계: docs/superpowers/plans/2026-07-12-wr-yuv-direct-sample.md

// Windows 전용 — 다른 타겟에서는 빈 크레이트로 컴파일된다 (workspace member라
// 비Windows `--workspace` 빌드에도 포함되므로 게이트 필수).
#[cfg(windows)]
mod ring_producer;

// RenderD3D11 본체 — ring_producer와 마찬가지로 Windows 전용. 비Windows에서는
// 이 모듈 자체가 컴파일되지 않아 크레이트가 계속 빈 크레이트로 유지된다.
#[cfg(windows)]
mod render_d3d11 {
    use std::sync::{Arc, Mutex};

    use gstreamer::prelude::*;
    use servo_config::pref;
    use servo_media_gstreamer_render::Render;
    use servo_media_player::PlayerError;
    use servo_media_player::d3d11_ring::{D3d11PlaneRings, RING_DEMAND_TTL};
    use servo_media_player::video::{
        Buffer, VideoFrame, VideoFrameD3D11YuvData, VideoFrameData, VideoFrameYuvColorRange,
        VideoFrameYuvColorSpace, VideoFrameYuvFormat,
    };

    use crate::ring_producer;

    /// ★낡음(2026-07-12 기록, 2026-08-25 `6ff890d501b` 이후 무효)★ — 실제 자리는 `render.rs:309-316`.
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
        group_id: Option<u64>,
        /// 현재 링을 만든 caps(포맷/크기/색상). 변경되면 링 교체.
        in_caps: Option<gstreamer::Caps>,
        /// 링이 아직 한 번도 소비되지 않음(전 슬롯 Unmapped, claim이 항상 None).
        /// true인 동안은 드롭 대신 첫 프레임을 스테이징한다.
        ring_never_consumed: bool,
        /// 소비자 디바이스 미발행 경고 1회 래치(파이프라인당).
        warned_no_device: bool,
        /// gst 버퍼 map 실패 경고 1회 래치(파이프라인당) — 로그 폭주 방지.
        warned_map_fail: bool,
    }

    pub struct RenderD3D11 {
        // 플레이어당 링 상태. build_frame은 스트리밍 스레드 1개에서만 불리지만
        // Render는 &self라 내부 가변성 필요.
        state: Mutex<PlayerState>,
    }

    impl RenderD3D11 {
        /// pref 게이트 + 사전 점검. 하나라도 실패하면 None → 기존 CPU(Raw) 경로 폴백.
        pub fn new() -> Option<RenderD3D11> {
            if !pref!(media_d3d11_enabled) {
                return None;
            }
            let options = servo_config::opts::get();
            if options.multiprocess || options.force_ipc {
                log::warn!("D3D11 video: 단일 프로세스 전용 — Raw 경로 폴백");
                return None;
            }
            log::info!("D3D11 video: plane 링 프로듀서 경로 활성");
            Some(RenderD3D11 {
                state: Mutex::new(PlayerState {
                    group_id: None,
                    in_caps: None,
                    ring_never_consumed: true,
                    warned_no_device: false,
                    warned_map_fail: false,
                }),
            })
        }

        /// 현재 caps 에 맞는 **그룹**을 보장한다(필요 시 (재)생성). 그룹은
        /// 디바이스를 모른다 — 실제 링은 소비자가 수요를 등록한 디바이스마다
        /// [`ensure_device_ring`](Self::ensure_device_ring) 이 만든다.
        fn ensure_group(&self, state: &mut PlayerState, caps: &gstreamer::CapsRef) -> Option<u64> {
            if state.group_id.is_some() && state.in_caps.as_deref() == Some(caps) {
                return state.group_id;
            }
            // caps 가 바뀌었으면 옛 그룹과 그에 딸린 **모든 디바이스 링**을 회수한다
            // (실제 Unmap/Release 는 각 소비자가 자기 디바이스 것만 가져가 수행).
            if let Some(old) = state.group_id.take() {
                D3d11PlaneRings::remove_group(old);
                state.in_caps = None;
            }
            let group_id = D3d11PlaneRings::create_group();
            state.group_id = Some(group_id);
            state.in_caps = Some(caps.to_owned());
            state.ring_never_consumed = true;
            Some(group_id)
        }

        /// `device` 에 이 그룹의 링이 없으면 그 디바이스 위에 plane 텍스처를 만들어
        /// 붙인다. 이미 있으면 그대로 돌려준다.
        ///
        /// ★여기가 "타일에 보이는 것만 그 GPU 로" 의 실행 지점이다★ — 호출은
        /// [`D3d11PlaneRings::demanded_devices`] 가 돌려준 디바이스에 대해서만 일어난다.
        fn ensure_device_ring(
            &self,
            group_id: u64,
            device: usize,
            format: VideoFrameYuvFormat,
            gst_format: gstreamer_video::VideoFormat,
            width: i32,
            height: i32,
        ) -> Option<u64> {
            if let Some(ring_id) = D3d11PlaneRings::ring_for(group_id, device) {
                return Some(ring_id);
            }
            let slots = ring_producer::create_plane_textures(device, format, width, height)?;
            let ring_id = D3d11PlaneRings::create_ring(device, format.plane_count(), slots);
            D3d11PlaneRings::attach_ring(group_id, device, ring_id);
            // 협상된 실제 gst VideoFormat(gst_format)과 매핑된 표시 포맷(format)을
            // 함께 남긴다 — 10-bit 협상(I42010le/P01010le vs 8-bit 강등) 확인용.
            // device 를 함께 남겨 어느 GPU 에 올라갔는지 로그로 확인할 수 있게 한다.
            log::info!(
                "D3D11 video: plane 링 생성 group_id={group_id} ring_id={ring_id}                  {width}x{height} gst={gst_format:?} -> {format:?} device=0x{device:x}"
            );
            Some(ring_id)
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
            let group_id = self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .group_id
                .take();
            if let Some(group_id) = group_id {
                D3d11PlaneRings::remove_group(group_id);
            }
        }
    }

    impl Render for RenderD3D11 {
        fn is_gl(&self) -> bool {
            false
        }

        fn build_frame(&self, sample: gstreamer::Sample) -> Option<VideoFrame> {
            let buffer = sample.buffer()?;
            if buffer.n_memory() == 0 {
                return None;
            }

            let caps = sample.caps()?;
            let info = gstreamer_video::VideoInfo::from_caps(caps).ok()?;
            let width = info.width() as i32;
            let height = info.height() as i32;

            // 표시 계약은 I420(Y,U,V)/NV12. YV12는 gst plane 순서가 Y,V,U이므로
            // 복사 시 U/V를 스왑해 계약에 맞춘다(swap_uv). 10-bit은 I420_10LE(평면형
            // R16×3, LSB) / P010_10LE(반평면형 R16+RG16, MSB) 두 계열을 직접 수용한다.
            let (format, swap_uv) = match info.format() {
                gstreamer_video::VideoFormat::I420 => (VideoFrameYuvFormat::I420, false),
                gstreamer_video::VideoFormat::Yv12 => (VideoFrameYuvFormat::I420, true),
                gstreamer_video::VideoFormat::Nv12 => (VideoFrameYuvFormat::NV12, false),
                gstreamer_video::VideoFormat::I42010le => (VideoFrameYuvFormat::I420_10, false),
                gstreamer_video::VideoFormat::P01010le => (VideoFrameYuvFormat::P010, false),
                other => {
                    log::warn!("D3D11 video: 미지원 포맷 {other:?} — 드롭");
                    return None;
                },
            };
            let (color_space, color_range) = colorimetry_from_info(&info);

            let mut state = self.state.lock().unwrap();
            let state = &mut *state;

            let group_id = self.ensure_group(state, caps)?;

            // 수요가 끊긴 디바이스의 링을 먼저 회수한다(타일이 이 영상을 더 이상
            // 보여주지 않게 된 경우). 그다음 **수요가 살아 있는 디바이스만** 업로드
            // 대상이 된다 — 이것이 "타일에 보이는 것만 그 GPU 로" 의 실체다.
            D3d11PlaneRings::expire_stale_demand(group_id, RING_DEMAND_TTL);
            let target_devices = D3d11PlaneRings::demanded_devices(group_id, RING_DEMAND_TTL);
            if target_devices.is_empty() {
                // 아직 아무 타일도 이 영상을 합성하지 않았다(렌더러 준비 전이거나
                // 화면 밖). 업로드는 건너뛰되 프레임은 정상 반환해야 한다 — 여기서
                // None 을 돌려주면 appsink 가 FlowError::Error 로 바꿔 파이프라인이
                // 통째로 정지한다(bf70293c4 계열).
                if !state.warned_no_device {
                    log::info!("D3D11 video: 아직 이 영상을 합성하는 타일이 없다 — 업로드 보류");
                    state.warned_no_device = true;
                }
                return VideoFrame::new(
                    width,
                    height,
                    Arc::new(D3D11YuvFrameBuffer {
                        data: VideoFrameD3D11YuvData {
                            group_id,
                            ring_epoch: 1,
                            format,
                            color_space,
                            color_range,
                        },
                    }),
                );
            }

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
            let frame = match gstreamer_video::VideoFrameRef::from_buffer_ref_readable(
                buffer, &info,
            ) {
                Ok(frame) => frame,
                Err(_) => {
                    if state.ring_never_consumed {
                        return None;
                    }
                    if !state.warned_map_fail {
                        log::warn!(
                            "D3D11 video: gst 버퍼 map 실패 — 기존 Presenting 슬롯 재표시로 폴백"
                        );
                        state.warned_map_fail = true;
                    }
                    // 아래와 동일한 메타데이터 프레임을 즉시 반환 — 새 프레임
                    // 복사 없이 재표시로 스트림 유지(배압 arm과 동형).
                    return VideoFrame::new(
                        width,
                        height,
                        Arc::new(D3D11YuvFrameBuffer {
                            data: VideoFrameD3D11YuvData {
                                group_id,
                                ring_epoch: 1,
                                format,
                                color_space,
                                color_range,
                            },
                        }),
                    );
                },
            };

            // ★디바이스별 업로드★ — 수요가 있는 디바이스마다 그 GPU 위의 링에
            // 복사한다. 6x6 그리드를 4 타일이 나눠 보면 각 영상은 보통 한 타일에만
            // 보이므로 **총 업로드 횟수는 단일 GPU 때와 같다**(타일 경계에 걸친
            // 영상만 두 번 올라간다).
            for device in target_devices {
                let Some(ring_id) =
                    self.ensure_device_ring(group_id, device, format, info.format(), width, height)
                else {
                    // 텍스처 생성 실패(디바이스 상실 등) — 이 디바이스만 건너뛴다.
                    // 다른 타일은 계속 정상 재생돼야 한다.
                    continue;
                };

                match D3d11PlaneRings::claim_free_slot(ring_id) {
                    Some(claimed) => {
                        let slot = claimed.slot;
                        ring_producer::copy_planes(&frame, format, swap_uv, &claimed);
                        D3d11PlaneRings::publish_slot(ring_id, slot);
                        // 첫 성공 claim = 어느 링이든 소비되기 시작했다는 신호.
                        state.ring_never_consumed = false;
                    },
                    None if state.ring_never_consumed => {
                        // 초기 구간(전 슬롯 Unmapped): 첫 프레임을 CPU에 스테이징해 두면
                        // 렌더러의 InitialMapAll이 표시한다. 첫 성공 claim 전까지 매
                        // 프레임 덮어쓴다. 스테이징은 링별이므로 디바이스마다 따로 둔다.
                        // 기본은 스테이징하지 않는다: 그 바이트는 렌더 스레드의 최초
                        // 소비 안에서 매핑된 D3D11 메모리로 복사되는데, 실측으로 최초
                        // 소비의 절반이 그 복사다(12.6~23.9ms 대 6.5~10.9ms). 프레임
                        // 예산이 16.6ms 이므로 그 한 번이 프레임을 넘긴다. 여기서 매
                        // 프레임 3MB 를 복사해 두는 일도 함께 사라진다.
                        if servo_config::pref!(media_ring_stage_first_frame) {
                            let vecs = ring_producer::planes_to_vecs(&frame, format, swap_uv);
                            // 밀려난 이전 버퍼는 **여기서** 떨어진다 — 자물쇠 밖, 이
                            // 스레드에서. 안에서 떨구면 레지스트리가 그만큼 잠긴다.
                            drop(D3d11PlaneRings::stage_first_frame(ring_id, vecs));
                        }
                    },
                    None => {
                        // 배압: 이 디바이스의 모든 슬롯이 아직 소비 대기다. 이번 gst
                        // 프레임 바이트는 이 디바이스에 대해서만 버리고(memcpy 전 —
                        // 비용 없음) 다른 디바이스는 계속 진행한다. None 을 반환하지
                        // 않는 이유는 종전과 같다: appsink 가 None 을 치명적
                        // FlowError::Error 로 바꿔 파이프라인 전체를 정지시킨다.
                        // 렌더러는 직전 Presenting 슬롯을 재표시한다(정상적 드롭).
                    },
                }
            }

            VideoFrame::new(
                width,
                height,
                Arc::new(D3D11YuvFrameBuffer {
                    data: VideoFrameD3D11YuvData {
                        group_id,
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
            // 자동 삽입해 목록 내 포맷으로 맞춘다. 8-bit(I420/YV12/NV12)에 더해
            // 10-bit(I420_10LE 평면형, P010_10LE 반평면형)도 직접 수용한다 — 각각
            // R16/RG16 DYNAMIC plane 텍스처로 memcpy되고 WR이 16-bit 컨테이너로
            // 샘플한다(강등 videoconvert 회피).
            let caps = video_sink_caps();
            appsink.set_property("caps", &caps);
            pipeline.set_property("video-sink", appsink);
            Ok(())
        }

        /// 이 렌더러는 appsink 자체를 넘기므로 손으로 링크하는 파이프라인에서도 쓸 수 있다.
        fn detached_video_sink_caps(&self) -> Option<gstreamer::Caps> {
            Some(video_sink_caps())
        }
    }

    /// appsink 가 받는 포맷. 두 경로가 같은 caps 를 써야 한다 — 한쪽만 바뀌면 링크가
    /// 조용히 실패하거나 원치 않는 변환이 끼어든다.
    fn video_sink_caps() -> gstreamer::Caps {
        gstreamer::Caps::builder("video/x-raw")
            .field(
                "format",
                gstreamer::List::new(["I420", "YV12", "NV12", "I420_10LE", "P010_10LE"]),
            )
            .field("pixel-aspect-ratio", gstreamer::Fraction::from((1, 1)))
            .build()
    }
}

#[cfg(windows)]
pub use render_d3d11::RenderD3D11;
