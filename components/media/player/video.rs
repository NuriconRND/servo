/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::Arc;

use malloc_size_of_derive::MallocSizeOf;

#[derive(Clone, Copy, Debug, Eq, MallocSizeOf, PartialEq)]
pub enum VideoFrameYuvFormat {
    I420,
    NV12,
}

impl VideoFrameYuvFormat {
    pub fn plane_count(self) -> usize {
        match self {
            VideoFrameYuvFormat::I420 => 3,
            VideoFrameYuvFormat::NV12 => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, MallocSizeOf, PartialEq)]
pub enum VideoFrameYuvColorSpace {
    Rec601,
    Rec709,
    Rec2020,
}

#[derive(Clone, Copy, Debug, Eq, MallocSizeOf, PartialEq)]
pub enum VideoFrameYuvColorRange {
    Limited,
    Full,
}

#[derive(Clone, Copy, Debug, Eq, MallocSizeOf, PartialEq)]
pub struct VideoFramePlane {
    pub width: i32,
    pub height: i32,
    pub stride: i32,
}

#[derive(Clone, Debug, Eq, MallocSizeOf, PartialEq)]
pub struct VideoFrameYuvData {
    pub format: VideoFrameYuvFormat,
    pub planes: [Option<VideoFramePlane>; 3],
    pub color_space: VideoFrameYuvColorSpace,
    pub color_range: VideoFrameYuvColorRange,
}

impl VideoFrameYuvData {
    pub fn plane(&self, index: usize) -> Option<VideoFramePlane> {
        self.planes.get(index).copied().flatten()
    }

    pub fn plane_count(&self) -> usize {
        self.format.plane_count()
    }
}

/// A-dyn 경로: plane DYNAMIC 링(레지스트리 `d3d11_ring`) 참조 + 표시
/// 메타데이터. 슬롯 인덱스는 싣지 않는다 — 렌더러가 레지스트리에서 최신
/// Filled를 소비한다(latest-wins). `ring_epoch`는 vestigial(항상 1) — 링
/// 무효화(크기 변경 등)는 에폭 증가가 아니라 `ring_id` 신규 발급으로 처리한다
/// (스펙 §10.3-②).
#[derive(Clone, Copy, Debug, Eq, MallocSizeOf, PartialEq)]
pub struct VideoFrameD3D11YuvData {
    pub ring_id: u64,
    pub ring_epoch: u32,
    pub format: VideoFrameYuvFormat,
    pub color_space: VideoFrameYuvColorSpace,
    pub color_range: VideoFrameYuvColorRange,
}

#[derive(Clone, MallocSizeOf)]
pub enum VideoFrameData {
    Raw(#[conditional_malloc_size_of] Arc<Vec<u8>>),
    Yuv(VideoFrameYuvData),
    Texture(u32),
    OESTexture(u32),
    D3D11Yuv(VideoFrameD3D11YuvData),
}

pub trait Buffer: Send + Sync {
    /// Return the renderable frame payload.
    ///
    /// Raw BGRA frames may return an owned byte buffer. YUV frames should return
    /// only metadata here and expose decoded plane bytes through `plane_data()`
    /// so callers do not accidentally copy the whole frame.
    fn frame_data(&self) -> Option<VideoFrameData>;

    fn plane_data(&self, _plane_index: usize) -> Option<&[u8]> {
        None
    }
}

#[derive(Clone, MallocSizeOf)]
pub struct VideoFrame {
    width: i32,
    height: i32,
    data: VideoFrameData,
    #[ignore_malloc_size_of = "Difficult"]
    _buffer: Arc<dyn Buffer>,
}

impl VideoFrame {
    pub fn new(width: i32, height: i32, buffer: Arc<dyn Buffer>) -> Option<Self> {
        let data = buffer.frame_data()?;
        Some(VideoFrame {
            width,
            height,
            data,
            _buffer: buffer,
        })
    }

    pub fn get_width(&self) -> i32 {
        self.width
    }

    pub fn get_height(&self) -> i32 {
        self.height
    }

    pub fn get_data(&self) -> Arc<Vec<u8>> {
        match self.data {
            VideoFrameData::Raw(ref data) => data.clone(),
            _ => unreachable!("invalid raw data request for texture frame"),
        }
    }

    pub fn get_yuv_data(&self) -> Option<&VideoFrameYuvData> {
        match &self.data {
            VideoFrameData::Yuv(data) => Some(data),
            _ => None,
        }
    }

    pub fn get_plane_data(&self, plane_index: usize) -> Option<&[u8]> {
        match self.data {
            VideoFrameData::Yuv(_) => self._buffer.plane_data(plane_index),
            _ => None,
        }
    }

    pub fn get_texture_id(&self) -> u32 {
        match self.data {
            VideoFrameData::Texture(data) | VideoFrameData::OESTexture(data) => data,
            _ => unreachable!("invalid texture id request for raw data frame"),
        }
    }

    pub fn is_gl_texture(&self) -> bool {
        matches!(
            self.data,
            VideoFrameData::Texture(_) | VideoFrameData::OESTexture(_)
        )
    }

    pub fn is_yuv(&self) -> bool {
        matches!(self.data, VideoFrameData::Yuv(_))
    }

    pub fn is_d3d11_yuv(&self) -> bool {
        matches!(self.data, VideoFrameData::D3D11Yuv(_))
    }

    pub fn get_d3d11_yuv_data(&self) -> Option<VideoFrameD3D11YuvData> {
        match self.data {
            VideoFrameData::D3D11Yuv(data) => Some(data),
            _ => None,
        }
    }

    pub fn is_external_oes(&self) -> bool {
        matches!(self.data, VideoFrameData::OESTexture(_))
    }
}

pub trait VideoFrameRenderer: Send + 'static {
    fn render(&mut self, frame: VideoFrame);
}
