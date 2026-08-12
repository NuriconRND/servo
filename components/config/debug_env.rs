/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 조사용 환경변수의 단일 등록처.
//!
//! 운용 설정은 pref 다(설계 문서 §3 의 판정 기준). 여기 있는 것은 실패 주입·
//! 프로파일링·이분탐색용이며, 조사가 끝나면 지운다.
//!
//! 호출부가 이름 문자열을 갖지 않는 것이 핵심이다 — 이름이 두 곳에 있으면
//! 한쪽만 바뀌어 조용히 갈라진다. 실제로 `SERVO_D3D11_PROFILE`은
//! `servo-media-gstreamer-render-d3d11`과 `servo-media-thread` 두 크레이트에서, `SERVO_MEDIA_
//! DISABLE_ENOUGHDATA_BACKOFF`는 `servo-media-gstreamer`와 `servo-script` 두 크레이트에서 각자
//! 읽고 있었다(2026-08-11 조사 결과 두 판정식은 동일했다 — 상세는 task-1-report.md).
//!
//! ## `Kind`에 대해
//!
//! 세 종류만 있다: `Presence`(설정 여부만 봄), `Int`(정수), `Str`(원본 문자열, 호출부가
//! 자체 파싱/판정). 다수의 노브가 `"1"`/`"true"` 류의 truthy 문자열만 켜짐으로 치고 그
//! 외(빈 문자열, `"0"`, 없음)는 꺼짐으로 치는데, 이는 `Presence`(존재만 보고 값은 무시)와
//! 다르다 — `Presence`로 등록하면 `FOO=0`이 "꺼짐"에서 "켜짐"으로 조용히 뒤집힌다. 그런
//! 노브는 `Str`로 등록하고 호출부가 원래 판정식을 그대로 유지한다(판정식이 노브마다
//! 조금씩 달라서 — `"1"/"true"`, `"1"/"true"/"on"`, `"1"/"true"/"yes"/"on"` 세 변종이 실존한다 —
//! 하나로 합치면 그 자체가 조용한 동작 변경이 된다).
//!
//! ## `cache_index`에 대해
//!
//! `ALL`은 캐시 슬롯의 정적 배열([`CACHES`])을 인덱스로 가리키는데, 이 인덱스를
//! [`DebugFlag::cache_index`]에 손으로 적어 넣는다. 상수와 인덱스가 어긋나면(순서를
//! 바꾸거나 복사-붙여넣기 실수) "A를 물었는데 B가 캐시되는" 조용한 버그가 된다. 이를
//! 막기 위해 모듈 하단에 `ALL[i].cache_index == i`를 확인하는 const 블록이 있다 — 어긋나면
//! **컴파일이 실패한다**(런타임에 발견되는 게 아니라). 대안으로 이름 문자열로 매 호출마다
//! `ALL`을 선형 탐색하는 방법도 있었지만, `dcomp_debug()`처럼 타일당 프레임당(초당
//! 수천 회) 불리는 게이트가 있어 O(1) 배열 접근을 유지했다.
//!
//! ## `enabled`/`int`/`string`의 kind 검사에 대해
//!
//! 브리프 초안은 `debug_assert_eq!`를 썼는데, 그러면 릴리스 빌드에서 잘못된 kind로
//! 접근해도 조용히 통과한다(예: `Str` 플래그를 실수로 `enabled()`로 읽으면 항상 `false`).
//! 여기서는 `assert_eq!`로 올렸다 — 이 함수들은 프로세스당 최초 1회만 env를 읽고
//! 이후에는 캐시된 값을 반환하므로(hot-path에서도 배열 인덱싱 + enum 비교 정도), 릴리스에서
//! 항상 검사해도 비용이 무시할 만하다. 배선 실수를 프로덕션에서도 즉시 패닉으로 드러내는
//! 편이 조용한 오독보다 낫다고 판단했다.

use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// 설정돼 있기만 하면 켜짐(값은 보지 않는다). `enabled()`로 읽는다.
    Presence,
    /// 정수 값(부호 있음). `int()`로 읽는다. 미설정 또는 파싱 실패 시 `None`(둘을 구분하지
    /// 않는다 — 호출부가 이미 둘을 같은 "무시하고 기본값" 취급이었다).
    Int,
    /// 원본 문자열. `string()`으로 읽는다. 호출부가 자체 truthy 판정이나 포맷 파싱
    /// (`"WxH"` 등)을 한다 — 판정식이 노브마다 달라 여기서 통일하지 않는다.
    Str,
}

#[derive(Debug)]
pub struct DebugFlag {
    pub name: &'static str,
    pub kind: Kind,
    /// 무엇을 조사하려고 만든 노브인가. 문서 표가 이 문장을 쓴다.
    pub doc: &'static str,
    /// `CACHES`의 슬롯 인덱스. `ALL`에서의 선언 위치와 반드시 같아야 한다 — 모듈 하단의
    /// const 어서션이 어긋나면 컴파일을 막는다.
    cache_index: usize,
}

// ---------------------------------------------------------------------------------------------
// Phase 5 wall frame barrier 실패 주입 (components/paint/paint.rs, WallFrameDelayInjection).
// 셋 다 프로세스 시작 시 1회만 읽힌다(Paint::new -> WallFrameCoordinator::from_environment).
// ---------------------------------------------------------------------------------------------

pub const WALL_FRAME_DELAY_TARGET_INDEX: DebugFlag = DebugFlag {
    name: "SERVO_WALL_FRAME_DELAY_TARGET_INDEX",
    kind: Kind::Int,
    doc: "wall frame barrier 실패 주입: 프레임 준비 신호를 보류할 0-기반 paint target 인덱스. \
          미설정이면 주입 기능 전체가 꺼진다(느린 렌더러 시뮬레이션, Phase 5).",
    cache_index: 0,
};

pub const WALL_FRAME_DELAY_AFTER: DebugFlag = DebugFlag {
    name: "SERVO_WALL_FRAME_DELAY_AFTER",
    kind: Kind::Int,
    doc: "위 주입이 적용되기 시작하는 첫 logical_frame_id. 미설정/파싱 실패 시 기본 1.",
    cache_index: 1,
};

pub const WALL_FRAME_DELAY_COUNT: DebugFlag = DebugFlag {
    name: "SERVO_WALL_FRAME_DELAY_COUNT",
    kind: Kind::Int,
    doc: "지연을 적용할 logical frame 개수. 미설정/파싱 실패 시 기본 1, 0이면 주입 무효화.",
    cache_index: 2,
};

// ---------------------------------------------------------------------------------------------
// D3D11 렌더 경로 진단 (components/media/backends/gstreamer/render-d3d11/lib.rs 와
// components/media/media-thread/lib.rs 두 크레이트가 각자 읽는다 — 판정식은 동일함을 확인함).
// ---------------------------------------------------------------------------------------------

pub const D3D11_PROFILE: DebugFlag = DebugFlag {
    name: "SERVO_D3D11_PROFILE",
    kind: Kind::Str,
    doc: "D3D11 비디오 렌더 경로의 단계별 타이밍 계측을 켠다. truthy: \"1\"/\"true\"/\"on\" \
          (대소문자 무시). render-d3d11 크레이트와 media-thread 크레이트 두 곳에서 각자 이 \
          진단을 게이트한다.",
    cache_index: 3,
};

pub const D3D11_PROFILE_MS: DebugFlag = DebugFlag {
    name: "SERVO_D3D11_PROFILE_MS",
    kind: Kind::Str,
    doc: "위 계측의 로그 임계값(ms, f64). build_frame 총시간이 이 값 이상일 때만 로깅. \
          미설정/파싱 실패 시 기본 8.0. 정수 전용 파서로는 표현 못 하는 f64 값이라 Str로 \
          등록했다(예 \"8.5\").",
    cache_index: 4,
};

// ---------------------------------------------------------------------------------------------
// DirectComposition 컴포지터 진단 (components/paint/dcomp_compositor.rs). 다섯 다 순수 존재
// 체크(`.is_ok()`)라 Kind::Presence.
// ---------------------------------------------------------------------------------------------

pub const DCOMP_DEBUG: DebugFlag = DebugFlag {
    name: "SERVO_DCOMP_DEBUG",
    kind: Kind::Presence,
    doc: "DComp bind/add_surface 좌표 진단 로그(Task 5 스모크 디버깅). 타일당 프레임당 불리는 \
          핫패스라 OnceLock 캐시가 필수.",
    cache_index: 5,
};

pub const DCOMP_READBACK: DebugFlag = DebugFlag {
    name: "SERVO_DCOMP_READBACK",
    kind: Kind::Presence,
    doc: "DComp 서피스를 CPU 로 읽어 내려 실제로 그려졌는지 확인한다(Task 6 결함 진단 전용, \
          glReadPixels로 파이프라인을 멈추므로 느리다, 호출부가 <=120프레임으로 제한).",
    cache_index: 6,
};

pub const DCOMP_VALIDPROBE: DebugFlag = DebugFlag {
    name: "SERVO_DCOMP_VALIDPROBE",
    kind: Kind::Presence,
    doc: "Task 9 결함 진단: non-opaque 서피스의 per-Virtual-bind valid_rect/dirty_rect 커버리지 \
          로그(<=300프레임으로 제한).",
    cache_index: 7,
};

pub const DCOMP_NO_PARTIAL_PRESENT: DebugFlag = DebugFlag {
    name: "SERVO_DCOMP_NO_PARTIAL_PRESENT",
    kind: Kind::Presence,
    doc: "진단: 부분 Present만 끄는 스위치(스펙 §3) — 강등 폴백 경로 검증용.",
    cache_index: 8,
};

// ---------------------------------------------------------------------------------------------
// components/paint/painter.rs 킬 스위치/진단. 넷 다 `"1"`/`"true"` truthy 판정
// (대소문자 무시, `"on"`/`"yes"`는 포함하지 않음 — D3D11_PROFILE 계열과 다른 변종).
// ---------------------------------------------------------------------------------------------

pub const DISABLE_VIDEO_IMMEDIATE_COMPOSITE: DebugFlag = DebugFlag {
    name: "SERVO_DISABLE_VIDEO_IMMEDIATE_COMPOSITE",
    kind: Kind::Str,
    doc: "FPS-jitter 조사 A/B 게이트: update_images의 비디오-도착당 즉시 재합성을 끄고 스크립트 \
          렌더링-기회 페이싱으로 대체. 기본 = 즉시 재합성 활성.",
    cache_index: 9,
};

pub const LOG_PRESENT_CADENCE: DebugFlag = DebugFlag {
    name: "SERVO_LOG_PRESENT_CADENCE",
    kind: Kind::Str,
    doc: "진단: 초당 1회, 실제 엔진 present 빈도(frame-ready rate)와 프레임 간 최악 간격을 \
          로깅한다 — 페이지의 requestAnimationFrame 카운트나 외부 캡처 도구와 무관한 \
          그라운드트루스.",
    cache_index: 10,
};

pub const DCOMP_DISABLE_RESIZE_REBUILD: DebugFlag = DebugFlag {
    name: "SERVO_DCOMP_DISABLE_RESIZE_REBUILD",
    kind: Kind::Str,
    doc: "킬 스위치(기본 = 활성). 런타임 리사이즈(드래그/최대화) 정착 후 picture-cache \
          재구축을 끈다 — Task 12/12b 마스터 스위치, A/B 검증 및 회귀 시 안전 밸브. Windows \
          전용.",
    cache_index: 11,
};

pub const DCOMP_DISABLE_RESIZE_VIRTUAL: DebugFlag = DebugFlag {
    name: "SERVO_DCOMP_DISABLE_RESIZE_VIRTUAL",
    kind: Kind::Str,
    doc: "task-12b 전용 킬 스위치(기본 = 활성). \"드래그 중 가상 서피스 모드\"만 끄고 Task 12의 \
          정착 재구축은 유지한다. Windows 전용.",
    cache_index: 12,
};

// ---------------------------------------------------------------------------------------------
// components/paint/dcomp_compositor.rs 의 나머지 하나(순수 존재 체크).
// ---------------------------------------------------------------------------------------------

pub const VIDEO_ESCAPE_PROF: DebugFlag = DebugFlag {
    name: "SERVO_VIDEO_ESCAPE_PROF",
    kind: Kind::Presence,
    doc: "external 비디오 present 파이프라인 프로파일러 게이트. 켜지면 렌더러 스레드가 초당 \
          1회 [vesc-prof] 집계 라인(info)을 낸다 — acquire/convert/present 중 어느 단계가 \
          프레임 예산을 먹는지 판독용.",
    cache_index: 13,
};

// ---------------------------------------------------------------------------------------------
// 미디어 EnoughData 백오프 킬 스위치 (components/media/backends/gstreamer/player.rs 와
// components/script/dom/html/htmlmediaelement.rs 두 크레이트가 각자 읽는다 — 판정식은
// 동일함을 확인함).
// ---------------------------------------------------------------------------------------------

pub const MEDIA_DISABLE_ENOUGHDATA_BACKOFF: DebugFlag = DebugFlag {
    name: "SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF",
    kind: Kind::Str,
    doc: "킬 스위치: PlayerError::EnoughData 백오프(요청 취소/재탐색)를 끈다. truthy: \
          \"1\"/\"true\"/\"yes\"/\"on\"(대소문자 무시). gstreamer player 백엔드와 \
          HTMLMediaElement 두 곳에서 각자 같은 판정으로 게이트한다.",
    cache_index: 14,
};

// ---------------------------------------------------------------------------------------------
// components/paint/painter.rs 의 WebRender picture-cache 타일 크기 오버라이드.
// ---------------------------------------------------------------------------------------------

pub const WR_PICTURE_TILE_SIZE: DebugFlag = DebugFlag {
    name: "SERVO_WR_PICTURE_TILE_SIZE",
    kind: Kind::Str,
    doc: "실험 노브: WR picture cache 타일 크기 오버라이드, \"WxH\" 형식(예 \"1920x1080\"). \
          미설정이면 WR 기본(1024x512)을 그대로 쓴다. 타일 수/무효화 입도/타일당 \
          DComp bind-unbind 오버헤드 A/B용.",
    cache_index: 15,
};

/// 등록된 조사용 환경변수 전부. 순서는 각 상수의 `cache_index`와 일치해야 한다(아래 const
/// 어서션이 강제한다).
pub const ALL: &[&DebugFlag] = &[
    &WALL_FRAME_DELAY_TARGET_INDEX,
    &WALL_FRAME_DELAY_AFTER,
    &WALL_FRAME_DELAY_COUNT,
    &D3D11_PROFILE,
    &D3D11_PROFILE_MS,
    &DCOMP_DEBUG,
    &DCOMP_READBACK,
    &DCOMP_VALIDPROBE,
    &DCOMP_NO_PARTIAL_PRESENT,
    &DISABLE_VIDEO_IMMEDIATE_COMPOSITE,
    &LOG_PRESENT_CADENCE,
    &DCOMP_DISABLE_RESIZE_REBUILD,
    &DCOMP_DISABLE_RESIZE_VIRTUAL,
    &VIDEO_ESCAPE_PROF,
    &MEDIA_DISABLE_ENOUGHDATA_BACKOFF,
    &WR_PICTURE_TILE_SIZE,
];

/// `ALL`의 각 원소가 자신이 선언한 `cache_index`와 실제 배열 위치가 같은지 컴파일 타임에
/// 확인한다. 어긋나면 여기서 컴파일이 실패한다 — "A를 물었는데 B가 캐시되는" 버그를
/// 런타임까지 끌고 가지 않기 위함(모호함 해소: 브리프는 방법을 지정하지 않았다).
const _: () = {
    let mut i = 0;
    while i < ALL.len() {
        assert!(
            ALL[i].cache_index == i,
            "servo_config::debug_env: DebugFlag.cache_index must match its position in ALL"
        );
        i += 1;
    }
};

struct CacheSlot {
    presence: OnceLock<bool>,
    int: OnceLock<Option<i64>>,
    string: OnceLock<Option<String>>,
}

impl CacheSlot {
    const fn new() -> Self {
        CacheSlot {
            presence: OnceLock::new(),
            int: OnceLock::new(),
            string: OnceLock::new(),
        }
    }
}

const SLOT_COUNT: usize = ALL.len();

static CACHES: [CacheSlot; SLOT_COUNT] = [const { CacheSlot::new() }; SLOT_COUNT];

fn cache(flag: &'static DebugFlag) -> &'static CacheSlot {
    &CACHES[flag.cache_index]
}

/// `flag.kind == Kind::Presence`. 설정 여부만 본다(값은 무시). 프로세스당 최초 1회만
/// `std::env::var`를 호출하고 이후에는 캐시된 값을 반환한다.
pub fn enabled(flag: &'static DebugFlag) -> bool {
    assert_eq!(
        flag.kind,
        Kind::Presence,
        "servo_config::debug_env::enabled() called on non-Presence flag {}",
        flag.name
    );
    *cache(flag)
        .presence
        .get_or_init(|| std::env::var(flag.name).is_ok())
}

/// `flag.kind == Kind::Int`. 미설정 또는 정수 파싱 실패 시 `None`(구분하지 않는다 — 호출부는
/// 이미 둘 다 "무시하고 기본값" 취급이었다). 파싱 실패는 1회 `eprintln!`으로 경고한다:
/// 조사용 노브라 기동을 막지는 않되, 조용히 기본값으로 돌아가면 "켰다고 믿는데 안 켜진
/// 상태"가 되므로 최소한의 신호는 남긴다.
pub fn int(flag: &'static DebugFlag) -> Option<i64> {
    assert_eq!(
        flag.kind,
        Kind::Int,
        "servo_config::debug_env::int() called on non-Int flag {}",
        flag.name
    );
    *cache(flag)
        .int
        .get_or_init(|| match std::env::var(flag.name) {
            Ok(raw) => match raw.trim().parse::<i64>() {
                Ok(value) => Some(value),
                Err(_) => {
                    eprintln!("servo: {}={raw} is not an integer; ignoring", flag.name);
                    None
                },
            },
            Err(_) => None,
        })
}

/// `flag.kind == Kind::Str`. 원본 문자열을 그대로 돌려준다 — truthy 판정이나 `"WxH"` 같은
/// 포맷 파싱은 호출부 책임(노브마다 판정식이 달라 여기서 통일하지 않는다).
pub fn string(flag: &'static DebugFlag) -> Option<String> {
    assert_eq!(
        flag.kind,
        Kind::Str,
        "servo_config::debug_env::string() called on non-Str flag {}",
        flag.name
    );
    cache(flag)
        .string
        .get_or_init(|| std::env::var(flag.name).ok())
        .clone()
}
