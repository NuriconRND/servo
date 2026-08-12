/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 설정 표면이 흩어지지 않도록 강제하는 테스트.
//!
//! 규칙만 정해 두면 다음 병합에서 무너진다 — 이미 SERVO_WR_PICTURE_TILE 과
//! SERVO_WR_PICTURE_TILE_SIZE 로 한 번 겪었다. 그래서 테스트로 건다.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

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
    // Task 5: media.* 9개
    "SERVO_MEDIA_D3D11_VIDEO",
    "SERVO_MEDIA_SYNC_GROUP",
    "SERVO_MEDIA_GAPLESS_LOOP",
    "SERVO_MEDIA_DIRECT_FILE",
    "SERVO_GSTREAMER_AVDEC_MAX_THREADS",
    "SERVO_GSTREAMER_DISABLE_AUDIO",
    "SERVO_GSTREAMER_VIDEO_DECODER_POLICY",
    "SERVO_VIDEO_SINK_POLICY",
    "SERVO_WEBRTC_JITTER_LATENCY_MS",
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

/// 소스에서 `env::var("SERVO_…")` 로 읽는 이름을 모은다.
///
/// third_party/stylo 는 제외한다 — 2026-07-16 에 upstream 체크아웃을 통째로
/// 복사한 것이라 그 안의 노브는 upstream 소유다. third_party/surfman 은 포함한다
/// — 서브모듈이 아니라 이 포크가 직접 수정해 온 코드다.
///
/// 이 파일 자체(`config_surface.rs`)는 스캔 대상에서 제외한다 — 스캐너 구현부에
/// `"\"SERVO_"` 패턴 리터럴이, 합성 테스트에 `"SERVO_MADE_UP_KNOB"` 리터럴이 들어 있어
/// 자기 자신을 오탐한다(둘 다 실제 env 읽기가 아니다). 브리프 원안의 스캐너를 그대로
/// 전체 저장소에 돌리면 이 자기-오탐이 실제로 발생함을 확인했다.
fn env_names_read_in_sources() -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let root = repo_root();
    let self_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/config_surface.rs")
        .canonicalize()
        .expect("this test file");
    for entry in walk_rust_files(&root) {
        if entry == self_path {
            continue;
        }
        let text = std::fs::read_to_string(&entry).unwrap_or_default();
        found.extend(extract_env_names(&text));
    }
    found
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

#[test]
fn every_registered_flag_is_actually_read_somewhere() {
    // 반대 방향. 이것이 없으면 죽은 항목이 테이블에 쌓인다.
    let read = env_names_read_in_sources();
    let dead: Vec<&str> = servo_config::debug_env::ALL
        .iter()
        .map(|flag| flag.name)
        .filter(|name| !read.contains(*name))
        .collect();
    assert!(
        dead.is_empty(),
        "테이블에 있는데 아무도 읽지 않는다: {dead:?}"
    );
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
