/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 기동 시 "기본값과 다른" pref 와 "설정된" 조사용 env 만 찍는 진단 덤프.
//!
//! 전량을 매번 찍으면 아무도 읽지 않는다. 조용한 것이 기본이어야 무언가 떴을 때
//! 의미가 생긴다. 이 덤프가 있으면 로그만 보고 그때 어떤 설정으로 돌렸는지 알 수
//! 있다 - 지금은 실행 명령을 따로 보관해야만 안다.
//!
//! 호출부는 두 셸(`ports/servoshell`, `components/servo/examples/winit_wall`)의
//! 기동 경로다 — pref 가 확정된(`prefs::set()`이 끝난) 뒤, 파이프라인/창 생성 전에
//! 부른다.

use crate::prefs::{self, Preferences};

/// 기본값과 다른 pref, 그리고 설정된 조사용 env 만 찍는다.
pub fn log_effective_config() {
    let current = prefs::get();
    let defaults = Preferences::const_default();
    for (name, value, default) in current.diff_from(&defaults) {
        eprintln!("servo: config: {name}={value} (default {default})");
    }
    // `current`(RwLockReadGuard)를 여기서 명시적으로 드롭한다 — 아래는 pref 락과 무관한
    // 별개의 진단이라 굳이 락을 쥔 채로 진행할 이유가 없다.
    drop(current);

    let set: Vec<&str> = crate::debug_env::ALL
        .iter()
        .filter(|flag| std::env::var(flag.name).is_ok())
        .map(|flag| flag.name)
        .collect();
    if !set.is_empty() {
        eprintln!("servo: config: debug env: {}", set.join(", "));
    }
}
