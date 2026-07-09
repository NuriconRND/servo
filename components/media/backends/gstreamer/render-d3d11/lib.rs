/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Windows용 D3D11 비디오 렌더 경로.
//!
//! 파이프라인의 스트리밍 스레드에서 d3d11upload/d3d11convert로 GPU 업로드·RGBA 변환을
//! 수행하고, 공유 텍스처 링에 복사한 뒤 공유 핸들만 Servo로 전달한다. 렌더러 스레드의
//! 비디오 업로드(glTexSubImage2D)를 제거하는 것이 목적. env `SERVO_MEDIA_D3D11_VIDEO=1`
//! 게이트 (기본 off). 설계: docs/superpowers/specs/2026-07-09-d3d11-per-pipeline-upload-design.md

pub mod ffi;
pub mod interop;

pub use interop::SharedGstD3D11Device;
