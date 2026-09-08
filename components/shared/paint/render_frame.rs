/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 렌더 프레임 경계를 렌더러 스레드 안에서 공유하기 위한 표식.
//!
//! ★시간 창으로는 프레임 안의 일을 제한할 수 없다★ — "16ms 마다 예산이 찬다"는 규칙은
//! 프레임이 이미 190ms 로 부풀어 있으면 그 프레임이 도는 동안 창이 열두 번 다시 차서
//! 아무것도 막지 못한다(실측: 그렇게 초당 75건이 통과했다). 제한의 기준은 프레임이어야
//! 하고, 그래야 부푼 프레임이 **애초에 만들어지지 않는다**.
//!
//! 페인터가 `render()` 직전에 [`begin_render_frame`] 을 부르고, 같은 스레드에서 도는 것들
//! (WebRender 의 external image `lock` 등)이 [`current_render_frame`] 으로 자기가 어느
//! 프레임 안에 있는지 안다. 페인터마다 스레드가 다르므로 thread_local 로 충분하다.

use std::cell::Cell;

thread_local! {
    static RENDER_FRAME: Cell<u64> = const { Cell::new(0) };
}

/// 이 스레드에서 새 렌더 프레임이 시작됨을 표시한다.
pub fn begin_render_frame() {
    RENDER_FRAME.with(|frame| frame.set(frame.get().wrapping_add(1)));
}

/// 이 스레드가 지금 몇 번째 렌더 프레임 안에 있는가. 호출자는 "값이 바뀌었는가"만 보므로
/// 프레임 밖에서 읽어도 무방하다.
pub fn current_render_frame() -> u64 {
    RENDER_FRAME.with(Cell::get)
}
