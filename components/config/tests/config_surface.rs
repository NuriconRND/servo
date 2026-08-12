/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 설정 표면이 흩어지지 않도록 강제하는 테스트.
//!
//! 규칙만 정해 두면 다음 병합에서 무너진다 — 이미 SERVO_WR_PICTURE_TILE 과
//! SERVO_WR_PICTURE_TILE_SIZE 로 한 번 겪었다. 그래서 테스트로 건다.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use servo_config::removed_env;

/// upstream servo 가 소유한 이름. 우리가 관리하지 않는다.
const UPSTREAM_OWNED: &[&str] = &[
    "SERVO_TRACING",
    "SERVO_DIAGNOSTICS",
    "SERVO_STYLE_THREAD_STACK_SIZE_KB",
];

/// 운용 설정으로 분류돼 pref 로 옮겨질 예정이지만, 아직 이관 태스크가 실행되지 않아
/// env::var 로 남아 있는 이름. 이 태스크(Task 1)의 범위는 조사용 17 개뿐이다 — 나머지는
/// `docs/superpowers/plans/2026-08-11-config-surface-consolidation.md`의 Task 2/4/5가
/// 옮긴다(합계 6+4+9=19, 계획서의 "운용 19개"와 일치함을 확인했다).
///
/// ★이 목록은 Task 1 브리프(task-1-brief.md)에는 없다★ — 브리프의 `UPSTREAM_OWNED`만
/// 갖고 전체 저장소를 스캔하면, 이 19개가 전부 "미등록"으로 걸려 Step 6("두 테스트
/// 통과")를 만족할 방법이 없다. 브리프와 실제 저장소 상태(운용 19개가 아직 pref로 옮겨지지
/// 않음)가 어긋나는 지점이라 상위 계획서 기준으로 이 목록을 추가했다 — 임의로 지어낸 것이
/// 아니라 계획서 Task 2/4/5의 Interfaces 절에 나열된 pref 필드에서 역산했다. Task
/// 2/4/5가 각각 실행되면 해당 그룹을 이 목록에서 지운다 — 그러면 `every_env_name_read_
/// in_sources_is_registered`가 그 시점부터 다시 해당 이름들의 표류를 잡아준다(이관 후에도
/// 옛 이름으로 읽는 코드가 남아 있으면 실패해야 하므로).
const PENDING_PREF_MIGRATION: &[&str] = &[
    // Task 2: gfx_* 6개는 전부 옮겨서 지웠다(env 읽기 자체를 pref 로 교체). SERVO_COMPOSITOR_DCOMP는
    // Task 3이 배선을 끝내면서 지웠다 — 3 상태 파싱이 paint_api::rendering_context::DcompMode::parse
    // 한 곳으로 모였고, surfman(third_party/surfman/.../surface.rs)은 더 이상 그 이름을 env로
    // 읽지 않는다(paint_api가 정규화한 불리언만 주입받는다). PoC(dcomp_native_poc.rs)도 pref
    // 파싱 없이 surfman의 불리언 API를 직접 부르므로 이 이름을 읽지 않는다.
    // Task 4: gfx_video_* 4개는 전부 옮겨서 지웠다(env 읽기 자체를 pref 로 교체) —
    // SERVO_VIDEO_ESCAPE 는 gfx_video_escape_mode(String, 파싱은
    // paint_api::rendering_context::parse_video_escape_token 한 곳), 나머지 셋은 단순
    // bool/i64 pref(gfx_video_escape_stable_swapchain/gfx_video_decouple_enabled/
    // gfx_video_escape_promote_hysteresis)로 옮겨 dcomp_compositor.rs/display_list/mod.rs
    // 가 pref! 로 직접 읽는다.
    // Task 5: media_* 9개는 전부 옮겨서 지웠다(env 읽기 자체를 pref 로 교체) —
    // SERVO_GSTREAMER_DISABLE_AUDIO 는 media_audio_enabled 로 의미가 뒤집혔다(긍정형,
    // 기본 true). SERVO_MEDIA_D3D11_VIDEO 는 media(render-d3d11)/paint(painter) 두
    // 크레이트가 동일한 판정식(1/true/yes/on, 대소문자 무시)으로 각자 읽던 것을
    // pref!(media_d3d11_enabled) 하나로 합쳤다. SERVO_MEDIA_SYNC_GROUP 은 브리프/설계
    // 문서가 media_sync_group_enabled 를 bool 로 표기했지만 실제 판정(player.rs
    // sync_group_target)은 파이프라인 목표 수(N>=2)를 받는 정수라 media_sync_group_target:
    // i64 로 이름과 타입을 함께 고쳤다(근거는 task-5-report.md). 나머지 6개(GAPLESS_LOOP/
    // DIRECT_FILE/AVDEC_MAX_THREADS/VIDEO_DECODER_POLICY/VIDEO_SINK_POLICY/
    // WEBRTC_JITTER_LATENCY_MS)는 구 env 미설정 시의 실제 동작을 그대로 기본값으로 옮겼다.
];

fn repo_root() -> PathBuf {
    // components/config -> 워크트리 루트
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// `root` 아래 `*.rs` 파일을 재귀 수집한다. `target/`, `third_party/stylo/`, `.git/` 은
/// 건너뛴다. `third_party/surfman` 은 건너뛰지 않는다 — 서브모듈이 아니라 이 포크가 직접
/// 수정해 온 코드다.
fn walk_rust_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let skip_dir_names: &[&str] = &["target", ".git"];
    let skip_dir_suffix = Path::new("third_party/stylo");

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if skip_dir_names.contains(&name.as_ref()) {
                    continue;
                }
                if path.ends_with(skip_dir_suffix) {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    out
}

/// 소스 텍스트에서 `"SERVO_…"` 형태의 문자열 리터럴을 추출한다. 순수 함수 — Step 7의
/// `the_drift_check_actually_catches_an_unregistered_name`이 이 함수 자체를 검증한다.
///
/// 알려진 한계: 문자열 리터럴만 잡는다. `format!("SERVO_{}", suffix)`처럼 조립되는 이름은
/// 못 잡는다.
fn extract_env_names(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut rest = text;
    while let Some(at) = rest.find("\"SERVO_") {
        rest = &rest[at + 1..];
        if let Some(end) = rest.find('"') {
            let name = &rest[..end];
            if name.starts_with("SERVO_")
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                found.insert(name.to_string());
            }
        }
    }
    found
}

/// `debug_env::FOO` 형태의 상수 참조를 추출한다. 순수 함수 —
/// `the_dead_knob_check_actually_catches_an_unread_flag`가 이 함수 자체를 검증한다.
///
/// ★**왜 문자열이 아니라 식별자를 세는가**★ — Task 1 이 이름 리터럴을 호출부에서 전부
/// 걷어냈다. 그래서 "이 노브가 실제로 읽히는가" 를 `"SERVO_…"` 리터럴로는 알 수 없다.
/// 호출부에 남은 유일한 흔적이 `debug_env::DCOMP_READBACK` 같은 상수 참조다.
fn extract_debug_env_const_refs(text: &str) -> BTreeSet<String> {
    const MARKER: &str = "debug_env::";
    let mut found = BTreeSet::new();
    let mut rest = text;
    while let Some(at) = rest.find(MARKER) {
        rest = &rest[at + MARKER.len()..];
        let ident: String = rest
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        // 소문자로 시작하면 함수(`debug_env::enabled` 등)라 상수가 아니다.
        if !ident.is_empty() {
            found.insert(ident);
        }
    }
    found
}

/// 이름이 **선언**되는 등록처 파일. 스캔 대상에서 제외한다.
///
/// ★이 제외가 없으면 표류 테스트가 조용히 무효가 된다★ — `debug_env.rs` 는 등록된 17 개의
/// 이름 리터럴을 그 자체로 전부 담고 있어서, 스캔에 포함하면 "누군가 읽고 있다" 가 **항상
/// 참**이 된다. 2026-08-12 확인: `every_registered_flag_is_actually_read_somewhere` 가 그
/// 이유로 실패할 수 없는 상태였다(Task 9 가 삭제 누락 안전망으로 삼기로 한 바로 그
/// 테스트다). `removed_env.rs` 도 같은 성격이라 함께 제외한다 — 거기 있는 19 개는 정의상
/// 아무도 읽지 않는 이름이다.
///
/// 이 파일 자체(`config_surface.rs`)도 제외한다 — 스캐너 구현부에 `"\"SERVO_"` 패턴
/// 리터럴이, 합성 테스트에 `"SERVO_MADE_UP_KNOB"` 리터럴이 들어 있어 자기 자신을
/// 오탐한다(둘 다 실제 env 읽기가 아니다).
const REGISTRY_FILES: &[&str] = &[
    "components/config/debug_env.rs",
    "components/config/removed_env.rs",
    "components/config/tests/config_surface.rs",
];

fn registry_paths() -> BTreeSet<PathBuf> {
    let root = repo_root();
    REGISTRY_FILES
        .iter()
        .map(|relative| {
            root.join(relative)
                .canonicalize()
                .unwrap_or_else(|error| panic!("등록처 파일 {relative} 를 찾지 못했다: {error}"))
        })
        .collect()
}

/// 등록처 파일을 뺀 나머지 소스를 훑는다. 두 표류 테스트가 공유한다.
fn scan_sources<T, F>(extract: F) -> BTreeSet<T>
where
    T: Ord,
    F: Fn(&str) -> BTreeSet<T>,
{
    let mut found = BTreeSet::new();
    let registries = registry_paths();
    for entry in walk_rust_files(&repo_root()) {
        if registries.contains(&entry) {
            continue;
        }
        let text = std::fs::read_to_string(&entry).unwrap_or_default();
        found.extend(extract(&text));
    }
    found
}

/// 소스에서 `env::var("SERVO_…")` 로 읽는 이름을 모은다.
///
/// third_party/stylo 는 제외한다 — 2026-07-16 에 upstream 체크아웃을 통째로
/// 복사한 것이라 그 안의 노브는 upstream 소유다. third_party/surfman 은 포함한다
/// — 서브모듈이 아니라 이 포크가 직접 수정해 온 코드다.
fn env_names_read_in_sources() -> BTreeSet<String> {
    scan_sources(extract_env_names)
}

#[test]
fn every_env_name_read_in_sources_is_registered() {
    let registered: BTreeSet<String> = servo_config::debug_env::ALL
        .iter()
        .map(|flag| flag.name.to_string())
        .collect();
    let allowed: BTreeSet<String> = UPSTREAM_OWNED
        .iter()
        .chain(PENDING_PREF_MIGRATION.iter())
        .map(|s| s.to_string())
        .collect();

    let unregistered: Vec<String> = env_names_read_in_sources()
        .into_iter()
        .filter(|name| !registered.contains(name) && !allowed.contains(name))
        .collect();

    assert!(
        unregistered.is_empty(),
        "테이블에 없는 환경변수를 읽고 있다: {unregistered:?}\n\
         조사용이면 debug_env::ALL 에 등록하고, 운용 설정이면 pref 로 옮겨라."
    );
}

/// `DebugFlag` 의 env 이름에서 대응하는 러스트 상수 식별자를 유도한다.
///
/// 관례는 `SERVO_` 접두사를 뗀 나머지다(`SERVO_DCOMP_READBACK` → `DCOMP_READBACK`). 17 개
/// 전부가 이 관례를 지킨다(2026-08-12 확인). 새 노브가 이를 어기면
/// `every_registered_flag_is_actually_read_somewhere` 가 그 노브를 죽었다고 보고한다 —
/// 조용히 넘어가지 않으니 그때 이름을 맞추면 된다.
fn const_ident_for(env_name: &str) -> &str {
    env_name.strip_prefix("SERVO_").unwrap_or(env_name)
}

fn dead_flags_among<'a>(
    flags: &[&'a servo_config::debug_env::DebugFlag],
    referenced: &BTreeSet<String>,
) -> Vec<&'a str> {
    flags
        .iter()
        .map(|flag| flag.name)
        .filter(|name| !referenced.contains(const_ident_for(name)))
        .collect()
}

#[test]
fn every_registered_flag_is_actually_read_somewhere() {
    // 반대 방향. 이것이 없으면 죽은 항목이 테이블에 쌓인다.
    //
    // 판정 신호가 이름 문자열이 아니라 `debug_env::` 상수 참조인 이유는
    // extract_debug_env_const_refs 의 주석 참고 — Task 1 이 호출부에서 이름 리터럴을
    // 전부 없앴기 때문에 문자열로는 이 방향을 아예 잴 수 없다.
    let referenced = scan_sources(extract_debug_env_const_refs);
    let dead = dead_flags_among(servo_config::debug_env::ALL, &referenced);
    assert!(
        dead.is_empty(),
        "테이블에 있는데 아무도 읽지 않는다: {dead:?}\n\
         호출부에서 지웠으면 debug_env::ALL 과 상수 선언도 함께 지워라."
    );
}

#[test]
fn the_dead_knob_check_actually_catches_an_unread_flag() {
    // 통과만 하는 검사는 거짓 안심이다 — 이 검사는 실제로 2026-08-12 까지 그 상태였다.
    // 스캐너가 debug_env.rs 자체를 훑는 바람에 등록된 이름이 항상 "읽힘" 으로 잡혀
    // 실패할 수가 없었다(Task 9 가 삭제 누락 안전망으로 삼기로 한 테스트다).
    let text = r#"
        let a = debug_env::enabled(&debug_env::DCOMP_READBACK);
        let b = debug_env::int(&debug_env::D3D11_PROFILE_MS);
    "#;
    let referenced = extract_debug_env_const_refs(text);
    assert!(referenced.contains("DCOMP_READBACK"));
    assert!(referenced.contains("D3D11_PROFILE_MS"));
    // 소문자로 시작하는 함수는 상수로 세지 않는다.
    assert!(!referenced.contains("enabled"));

    // 참조되지 않는 항목이 실제로 걸리는가.
    let dead = dead_flags_among(
        &[
            &servo_config::debug_env::DCOMP_READBACK,
            &servo_config::debug_env::LOG_PRESENT_CADENCE,
        ],
        &referenced,
    );
    assert_eq!(dead, vec!["SERVO_LOG_PRESENT_CADENCE"]);
}

#[test]
fn pending_migration_entries_must_still_be_read_somewhere() {
    // PENDING_PREF_MIGRATION 은 pref 로 옮기는 중인 이름의 임시 면제다. 옮기고 나면 env
    // 읽기가 사라지므로 이 테스트가 실패하고, 그때 목록에서 지우게 된다. 이 강제 장치가
    // 없으면 이관 태스크가 옛 env::var 읽기를 지우지 않고 남겨도
    // every_env_name_read_in_sources_is_registered 는 못 잡는다 — 그 이름이 이 목록으로
    // 계속 "허용"되기 때문이다. Task 1 이 막으려던 표류가 바로 그 형태라 사람이 손으로
    // 줄을 지우도록 강제한다.
    let read = env_names_read_in_sources();
    let stale: Vec<&str> = PENDING_PREF_MIGRATION
        .iter()
        .copied()
        .filter(|name| !read.contains(*name))
        .collect();
    assert!(
        stale.is_empty(),
        "pref 로 옮겨 더 이상 읽지 않는 이름이 면제 목록에 남아 있다: {stale:?}\n\
         PENDING_PREF_MIGRATION 에서 지워라."
    );
}

#[test]
fn the_drift_check_actually_catches_an_unregistered_name() {
    // 스캐너가 실제로 이름을 뽑아내는지 — 합성 소스로 확인한다.
    let text = r#" let x = std::env::var("SERVO_MADE_UP_KNOB").is_ok(); "#;
    assert!(extract_env_names(text).contains("SERVO_MADE_UP_KNOB"));
    // 리터럴이 아닌 조립 형태는 못 잡는다는 것도 함께 고정한다(알려진 한계).
    let assembled = r#" let n = format!("SERVO_{}", suffix); std::env::var(n) "#;
    assert!(extract_env_names(assembled).is_empty());
}

#[test]
fn gfx_defaults_match_the_behaviour_before_the_migration() {
    // Preferences 는 prefs 모듈에 있다(pref_util 이 아니다 - 거기에는 PrefValue 만 있다).
    let defaults = servo_config::prefs::Preferences::const_default();
    // 이전 동작을 그대로 유지해야 한다 - 기본값이 바뀌면 조용히 화면이 달라진다.
    assert_eq!(
        defaults.gfx_refresh_hz, 120,
        "refresh_driver.rs 의 unwrap_or(120)"
    );
    assert!(
        !defaults.gfx_vsync_enabled,
        "DwmFlush 가 코어를 상시 소모해 기본 off 다"
    );
    // 브리프 원안은 `assert_eq!(defaults.gfx_dcomp_mode, "off")`였지만, 이 파일의 다른
    // String pref 는 예외 없이 `const_default()`에서 빈 문자열이고 실제 기본 동작은
    // "미설정"으로 해석된다(옛 SERVO_COMPOSITOR_DCOMP env 도 미설정=off였다). 그 관례를
    // 깨는 대신(그러려면 const fn 을 포기해야 했다 - 코디네이터 지시로 되돌림), 빈
    // 문자열을 off 와 동일시한다. "빈 문자열 -> off" 해석은 Task 3 이 배선을 끝냈다
    // (`paint_api::rendering_context::DcompMode::parse("")` == `DcompMode::Off`).
    assert!(
        defaults.gfx_dcomp_mode.is_empty(),
        "빈 문자열 = off (이 파일의 다른 String pref 와 같은 관례)"
    );
    // 아래 세 개는 브리프에 값이 안 나와 있어 paint.rs 의 실제 판정을 읽어 확정했다
    // (WallFramePacingConfig::from_environment, 옮기기 전 코드): 환경변수 미설정이면
    // mode 는 Latest 가 되고 enabled() 는 `mode == Latest`이므로 페이싱은 기본 켜짐이다.
    assert!(
        defaults.gfx_wall_frame_pacing_enabled,
        "WALL_FRAME_PACING_ENV 미설정 시 WallFramePacingMode::Latest -> enabled()==true"
    );
    assert_eq!(defaults.gfx_wall_frame_max_pending, 1);
    assert_eq!(defaults.gfx_wall_frame_min_interval_ms, 16);
}

#[test]
fn video_defaults_preserve_the_kill_switches() {
    // Task 4: gfx_video_* 4개. 아래 둘은 기본 on 인 킬스위치다 — env 미설정 = off 로
    // 추측하면 기능이 꺼진다(task-4-brief.md §"기본값을 추측하지 마라"):
    //   SERVO_VIDEO_DECOUPLE: map(|v| v != "0").unwrap_or(true) -> 기본 on
    //   SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN: as_deref() != Ok("0") -> 기본 on
    // SERVO_VIDEO_ESCAPE_PROMOTE_HYSTERESIS 는 unwrap_or(10) 이 기본값이다.
    let defaults = servo_config::prefs::Preferences::const_default();
    assert!(
        defaults.gfx_video_decouple_enabled,
        "SERVO_VIDEO_DECOUPLE 은 != 0 판정이라 기본 on"
    );
    assert!(
        defaults.gfx_video_escape_stable_swapchain,
        "as_deref() != Ok(\"0\") 이라 기본 on"
    );
    assert_eq!(defaults.gfx_video_escape_promote_hysteresis, 10);
    // gfx_video_escape_mode 는 이 파일의 다른 String pref 와 같은 관례 — 빈 문자열 = off
    // (external 만 유효 토큰).
    assert!(defaults.gfx_video_escape_mode.is_empty());
}

#[test]
fn wr_picture_tile_size_defaults_to_no_override() {
    // 빈 문자열 = 오버라이드 없음. 이 노브는 Task 1 이 조사용 env 로 등록했다가 나중에
    // pref 로 옮긴 것이라(런처 -TileSize 로 출하된 운용 노브였다) 기본값이 옛 env 미설정과
    // 같은 "손대지 않음" 이어야 한다 - WR 기본 분기(콘텐츠 1024x512, 스크롤바 특수 크기)가
    // 그대로 살아야 한다.
    let defaults = servo_config::prefs::Preferences::const_default();
    assert!(defaults.gfx_wr_picture_tile_size.is_empty());
}

#[test]
fn disable_audio_is_inverted_into_a_positive_pref() {
    let defaults = servo_config::prefs::Preferences::const_default();
    // SERVO_GSTREAMER_DISABLE_AUDIO 는 부정형이었다. servo 관례가 *_enabled
    // 긍정형이고 이중부정은 실수의 단골이라 뒤집는다. 기본은 오디오 켜짐.
    assert!(defaults.media_audio_enabled);
}

#[test]
fn media_defaults_preserve_the_old_env_unset_behaviour() {
    // Task 5: media_* 9개. media_audio_enabled 를 제외한 나머지는 구 env 가 미설정일
    // 때의 실제 판정을 그대로 기본값으로 옮긴 것이다(추측 금지 — 각 호출부를 읽어 확정).
    let defaults = servo_config::prefs::Preferences::const_default();
    assert!(
        !defaults.media_d3d11_enabled,
        "env_flag_enabled(SERVO_MEDIA_D3D11_VIDEO) 는 env 미설정 시 false"
    );
    // media_sync_group_target 은 브리프 표기(media_sync_group_enabled: bool)와 달리 목표
    // 파이프라인 수 i64 다 — 구 sync_group_target() 이 usize 파싱 + filter(>=2) 라 미설정은
    // None(=off)이었고, 0 이 그 상태를 대표한다(0 은 filter 조건을 만족 못 해 사실상 off).
    assert_eq!(defaults.media_sync_group_target, 0);
    assert!(!defaults.media_gapless_loop_enabled);
    assert!(!defaults.media_direct_file_enabled);
    // -1 = "미설정, 자동" 의 보초값 — 0 이상은 실제 스레드 캡이라 0 을 기본값으로 쓰면
    // 안 된다(구 env 부재 = 디코더를 건드리지 않음, 0 = 1스레드로 강제하는 것과 다르다).
    assert_eq!(defaults.media_avdec_max_threads, -1);
    assert!(
        defaults.media_video_decoder_policy.is_empty(),
        "빈 문자열 = Software"
    );
    assert!(
        defaults.media_video_sink_policy.is_empty(),
        "빈 문자열 = Smooth"
    );
    assert_eq!(defaults.media_webrtc_jitter_latency_ms, 0);
}

// ---------------------------------------------------------------------------------------------
// Task 6: pref 로 옮긴 env 가 설정돼 있으면 기동을 막는다.
// ---------------------------------------------------------------------------------------------

/// pref 로 옮긴 20 개(설계 문서 §4 표 + 나중에 합류한 WR_PICTURE_TILE_SIZE). 여기에
/// **명시적으로** 적는다 — 차단 목록 자신에서 유도하면 "목록에 있는 것이 목록에 있다" 는
/// 동어반복이 되어 누락을 못 잡는다.
const MIGRATED_ENV: &[&str] = &[
    // Task 2/3: gfx_* 6
    "SERVO_COMPOSITOR_DCOMP",
    "SERVO_WIN_VSYNC",
    "SERVO_REFRESH_TIMER_HZ",
    "SERVO_WALL_FRAME_PACING",
    "SERVO_WALL_FRAME_MAX_PENDING",
    "SERVO_WALL_FRAME_MIN_INTERVAL_MS",
    // Task 4: gfx_video_* 4
    "SERVO_VIDEO_ESCAPE",
    "SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN",
    "SERVO_VIDEO_ESCAPE_PROMOTE_HYSTERESIS",
    "SERVO_VIDEO_DECOUPLE",
    // Task 5: media_* 9
    "SERVO_MEDIA_D3D11_VIDEO",
    "SERVO_MEDIA_SYNC_GROUP",
    "SERVO_MEDIA_GAPLESS_LOOP",
    "SERVO_MEDIA_DIRECT_FILE",
    "SERVO_GSTREAMER_AVDEC_MAX_THREADS",
    "SERVO_GSTREAMER_DISABLE_AUDIO",
    "SERVO_GSTREAMER_VIDEO_DECODER_POLICY",
    "SERVO_VIDEO_SINK_POLICY",
    "SERVO_WEBRTC_JITTER_LATENCY_MS",
    // 추가 이관: Task 1 이 조사용으로 분류했으나 실제로는 운용 노브였다(런처 -TileSize 로
    // 출하되고 DComp 투명 구멍의 회피책으로 쓰인다).
    "SERVO_WR_PICTURE_TILE_SIZE",
];

#[test]
fn a_removed_env_name_names_its_replacement() {
    let entry = removed_env::lookup("SERVO_COMPOSITOR_DCOMP").expect("등록돼 있어야 한다");
    assert!(
        entry.message.contains("gfx_dcomp_mode"),
        "무엇으로 바꿀지 알려줘야 한다: {}",
        entry.message
    );
}

#[test]
fn every_migrated_env_is_listed_as_removed() {
    // pref 로 옮긴 것이 전부 차단 목록에 있어야 한다. 하나라도 빠지면
    // 그 스크립트는 조용히 옛 설정을 잃는다.
    for name in MIGRATED_ENV {
        assert!(
            removed_env::lookup(name).is_some(),
            "{name} 이 차단 목록에 없다"
        );
    }
    // 반대 방향 — 차단 목록에 이관한 것 말고 다른 게 끼어들지 않았는가. 목록은 한시적이라
    // (설계 문서 §9) 무엇이 왜 들어 있는지가 흐려지면 걷어낼 시점을 판단할 수 없다.
    let expected: BTreeSet<&str> = MIGRATED_ENV.iter().copied().collect();
    let extra: Vec<&str> = removed_env::REMOVED
        .iter()
        .map(|entry| entry.name)
        .filter(|name| !expected.contains(name))
        .collect();
    assert!(
        extra.is_empty(),
        "차단 목록에 이관 목록 밖의 이름이 있다: {extra:?}"
    );
}

#[test]
fn every_removed_entry_names_a_pref_that_actually_exists() {
    // ★안내가 통하지 않는 이름을 가르치면 안 된다★ — 모르는 pref 이름은 조용히 무시되지
    // 않고 `Unknown preference` 로 즉시 패닉한다(prefs.rs 의 set_value). 안내대로 따라 한
    // 운영자가 기동조차 못 하게 된다. Task 3 에서 경고 문구가 점 표기 pref 이름을 가르쳐
    // 같은 함정을 만든 적이 있다.
    for entry in removed_env::REMOVED {
        assert!(
            servo_config::prefs::Preferences::exists(entry.pref),
            "{} 이 안내하는 pref {} 가 존재하지 않는다",
            entry.name,
            entry.pref
        );
        // 기계가 읽는 필드(pref)와 사람이 읽는 문구(message)가 갈라지면, 검사는 통과하는데
        // 화면에는 틀린 이름이 뜬다.
        assert!(
            entry.message.contains(entry.pref),
            "{} 의 안내 문구가 pref {} 를 언급하지 않는다: {}",
            entry.name,
            entry.pref,
            entry.message
        );
    }
}

#[test]
fn the_semantics_that_flipped_are_spelled_out_in_the_guidance() {
    // 이 둘은 단순 개명이 아니다. 옛 이름을 그대로 옮겨 적으면 반대로 동작한다.
    let audio = removed_env::lookup("SERVO_GSTREAMER_DISABLE_AUDIO").expect("등록돼 있어야 한다");
    assert!(
        audio.message.contains("media_audio_enabled=false"),
        "부정형이 긍정형으로 뒤집혔다는 것을 값까지 적어야 한다: {}",
        audio.message
    );
    // SERVO_MEDIA_SYNC_GROUP 은 온오프가 아니라 목표 파이프라인 수였다 - =true 로 바꿔
    // 적으면 pref 타입 불일치로 기동이 죽는다.
    let sync = removed_env::lookup("SERVO_MEDIA_SYNC_GROUP").expect("등록돼 있어야 한다");
    assert!(
        sync.message.contains("media_sync_group_target=N"),
        "값이 개수라는 것을 알려야 한다: {}",
        sync.message
    );
}

#[test]
fn check_blocks_only_the_names_that_are_actually_set() {
    // `check()` 는 프로세스 전역 env 를 읽는다 - 테스트가 병렬로 도는 데다 Rust 2024 에서
    // set_var 는 unsafe 다. 그래서 판정 본체를 순수 함수로 분리해 두고 그것을 검증한다.
    let none = removed_env::blocked_by(|_| false);
    assert!(none.is_empty(), "설정된 것이 없으면 아무것도 막지 않는다");

    let one = removed_env::blocked_by(|name| name == "SERVO_COMPOSITOR_DCOMP");
    assert_eq!(one.len(), 1);
    assert!(one[0].contains("SERVO_COMPOSITOR_DCOMP"));
    assert!(one[0].contains("gfx_dcomp_mode"));

    // 여러 개가 설정돼 있으면 전량을 보고한다 - 하나씩 고치며 재기동하게 만들면 안 된다.
    let all = removed_env::blocked_by(|_| true);
    assert_eq!(all.len(), MIGRATED_ENV.len());
}

// ---------------------------------------------------------------------------------------------
// Task 8: 문서 표류 방지.
// ---------------------------------------------------------------------------------------------

fn configuration_doc() -> String {
    let path = repo_root().join("docs/multigpu/configuration.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} 를 읽지 못했다: {error}", path.display()))
}

/// 공백을 하나로 접는다. 마크다운은 줄바꿈으로 자유롭게 감쌀 수 있으므로, 원문과
/// 문서를 같은 규칙으로 정규화한 뒤에 비교해야 한다.
fn squeeze(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `flags` 중 `doc` 에 이름이 없는 것. 검사 본체와 "검사가 무엇을 잡는지" 가 같은 코드를
/// 써야 의미가 있다.
fn names_missing_from<'a>(
    doc: &str,
    flags: &[&'a servo_config::debug_env::DebugFlag],
) -> Vec<&'a str> {
    flags
        .iter()
        .map(|flag| flag.name)
        .filter(|name| !doc.contains(*name))
        .collect()
}

#[test]
fn every_knob_appears_in_the_configuration_doc() {
    let doc = configuration_doc();
    let missing = names_missing_from(&doc, servo_config::debug_env::ALL);
    assert!(missing.is_empty(), "문서에 없는 조사용 노브: {missing:?}");
}

#[test]
fn every_migrated_pref_appears_in_the_configuration_doc() {
    // 옮긴 19 개의 **새 pref 이름**과 **옛 env 이름**이 둘 다 문서에 있어야 한다. 옛 이름이
    // 없으면 그것을 기억하는 사람이 문서에서 검색해도 못 찾는다 - 마이그레이션 표는 옛
    // 이름으로 찾아 들어오는 사람을 위한 것이다.
    let doc = configuration_doc();
    let missing: Vec<String> = removed_env::REMOVED
        .iter()
        .flat_map(|entry| [entry.pref, entry.name])
        .filter(|needle| !doc.contains(*needle))
        .map(str::to_string)
        .collect();
    assert!(missing.is_empty(), "문서에 없는 이름: {missing:?}");
}

#[test]
fn the_doc_repeats_the_knob_descriptions_verbatim() {
    // 이름만 검사하면 설명이 낡아도 통과한다 - 이름은 잘 안 바뀌고 설명은 자주 바뀐다.
    // debug_env 의 doc 필드를 정본으로 삼고 문서가 그것을 그대로 담고 있는지 본다.
    let doc = squeeze(&configuration_doc());
    let drifted: Vec<&str> = servo_config::debug_env::ALL
        .iter()
        .filter(|flag| !doc.contains(&squeeze(flag.doc)))
        .map(|flag| flag.name)
        .collect();
    assert!(
        drifted.is_empty(),
        "설명이 debug_env 의 doc 과 다르다: {drifted:?}\n\
         configuration.md 의 해당 항목을 debug_env.rs 의 doc 문자열로 맞춰라."
    );
}

#[test]
fn the_doc_check_actually_catches_a_missing_entry() {
    // 검사가 느슨해져 통과만 하는 것을 막는다 - wall_view 에서 자체완결성 검사가
    // 두 번 그렇게 무너졌고, 이 저장소에서도 2026-08-12 에 표류 테스트 하나가 무효인
    // 채로 발견됐다(the_dead_knob_check_actually_catches_an_unread_flag 참고).
    let synthetic = "SERVO_DCOMP_READBACK 만 적힌 문서";
    let missing = names_missing_from(synthetic, servo_config::debug_env::ALL);
    assert!(!missing.is_empty());
    assert!(
        !missing.contains(&"SERVO_DCOMP_READBACK"),
        "적혀 있는 것은 빠진 것으로 세면 안 된다"
    );
    // 정규화가 줄바꿈을 흡수하는지도 함께 고정한다 - 이것이 없으면 문서를 감쌀 때마다
    // 설명 검사가 거짓으로 실패한다.
    assert_eq!(squeeze("a\n  b\tc"), "a b c");
}
