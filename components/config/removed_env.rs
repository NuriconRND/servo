/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! pref 로 옮겨 더 이상 읽지 않는 환경변수. 설정돼 있으면 **기동을 막는다.**
//!
//! 경고 후 통과가 아니다 — 켰다고 믿는데 실제로는 안 켜진 상태가 아예 안 켠 것보다
//! 나쁘기 때문이다(wall_view 인증 설정에서 같은 판단을 했다). 이 프로젝트는 이미 그
//! 형태로 한 번 당했다: Task 3 에서 `etc/multigpu/*.ps1` 이 `$env:SERVO_COMPOSITOR_DCOMP`
//! 로 DComp 를 켜고 있었는데, 셸이 pref 를 무조건 주입하게 되면서 그 env 가 죽었다.
//! 엔진은 아무 경고도 찍지 않았고 화면은 그냥 돌았다 — 무엇이 꺼졌는지 알 방법이 없었다.
//!
//! ## 왜 그냥 삭제하지 않는가
//!
//! | 방식 | 결과 |
//! |---|---|
//! | 그냥 삭제 | 스크립트가 **조용히** 옛 설정을 잃는다 |
//! | 계속 인정 + 경고 | 정본이 둘이 된다 — 없애려던 표류가 그대로 남는다 |
//! | **설정돼 있으면 기동 차단** | 무엇을 무엇으로 바꾸라고 알려주고 멈춘다 |
//!
//! ## 이 목록은 한시적이다
//!
//! 옮긴 20 개에 대해서만 두고, 정리가 안정되면 걷어낸다(설계 문서 §9). 그래서
//! `every_migrated_env_is_listed_as_removed` 가 **양방향**으로 건다 — 20 개가 다 있어야
//! 하고, 20 개 밖의 이름이 끼어들어도 실패한다. 무엇이 왜 들어 있는지가 흐려지면 걷어낼
//! 시점을 판단할 수 없다.
//!
//! 원래 19 개였고 `SERVO_WR_PICTURE_TILE_SIZE` 가 나중에 합류했다 — Task 1 이 조사용으로
//! 분류했는데 실제로는 런처 `-TileSize` 로 출하되고 DComp 투명 구멍의 회피책으로 쓰이는
//! **운용 노브**였다. 분류 기준(설계 문서 §3)대로라면 처음부터 pref 였어야 한다.
//!
//! ## `pref` 필드가 따로 있는 이유
//!
//! `message` 만 두면 안내 문구의 pref 이름이 틀려도 아무도 모른다. **모르는 pref 이름은
//! 조용히 무시되지 않고 `Unknown preference` 로 즉시 패닉한다**(`prefs.rs` 의 `set_value`)
//! — 안내대로 따라 한 운영자가 기동조차 못 하게 된다. Task 3 에서 경고 문구가 통하지 않는
//! 점 표기 pref 이름을 가르쳐 같은 함정을 만든 적이 있다. 기계가 읽는 `pref` 를 따로 두고
//! 테스트가 (1) 그 pref 가 실존하는지 (2) `message` 가 그 이름을 담고 있는지를 확인한다.

/// pref 로 옮겨 더 이상 읽지 않는 환경변수 하나.
pub struct RemovedEnv {
    /// 옛 환경변수 이름.
    pub name: &'static str,
    /// 대체 pref 의 이름. ★밑줄 표기다★ — 점 표기(`gfx.dcomp.mode`)는 통하지 않는다
    /// (파생 매크로가 `stringify!(field)` 로 매칭한다).
    pub pref: &'static str,
    /// 운영자에게 보여 줄 안내. `--pref <이름>=<값>` 형태의 **실행 가능한 예시**를 담는다
    /// — 이름만 알려 주면 값 문법에서 다시 막힌다.
    pub message: &'static str,
}

pub const REMOVED: &[RemovedEnv] = &[
    // ---- Task 2/3: gfx_* 6 ----
    // 2026-08-31: 조사용 A/B 게이트로 출발했으나 실측이 기본값을 뒤집어 운용 노브가 됐다.
    // ***의미도 반대다*** — 옛 env 는 "끄기", 새 pref 는 "켜기"이고 기본이 꺼짐이다.
    RemovedEnv {
        name: "SERVO_DISABLE_VIDEO_IMMEDIATE_COMPOSITE",
        pref: "gfx_video_immediate_composite_enabled",
        message: "use --pref gfx_video_immediate_composite_enabled=false to stop video \
                  arrivals driving composites; the sense is INVERTED (the env disabled it, \
                  the pref enables it) and it defaults to true, but that path is now \
                  coalesced to the gfx_refresh_hz cadence instead of firing per arrival",
    },
    RemovedEnv {
        name: "SERVO_COMPOSITOR_DCOMP",
        pref: "gfx_dcomp_mode",
        message: "use --pref gfx_dcomp_mode=on (hybrid) or --pref gfx_dcomp_mode=surface; \
                  the value is a three-state mode, not a boolean",
    },
    RemovedEnv {
        name: "SERVO_WIN_VSYNC",
        pref: "gfx_vsync_enabled",
        message: "use --pref gfx_vsync_enabled=true",
    },
    RemovedEnv {
        name: "SERVO_REFRESH_TIMER_HZ",
        pref: "gfx_refresh_hz",
        message: "use --pref gfx_refresh_hz=N (default 120, clamped to [1,1000])",
    },
    RemovedEnv {
        name: "SERVO_WALL_FRAME_PACING",
        pref: "gfx_wall_frame_pacing_enabled",
        message: "use --pref gfx_wall_frame_pacing_enabled=false to turn pacing off; \
                  note it defaults to true, unlike most _enabled prefs",
    },
    RemovedEnv {
        name: "SERVO_WALL_FRAME_MAX_PENDING",
        pref: "gfx_wall_frame_max_pending",
        message: "use --pref gfx_wall_frame_max_pending=N (default 1)",
    },
    RemovedEnv {
        name: "SERVO_WALL_FRAME_MIN_INTERVAL_MS",
        pref: "gfx_wall_frame_min_interval_ms",
        message: "use --pref gfx_wall_frame_min_interval_ms=N (default 16)",
    },
    // ---- Task 4: gfx_video_* 4 ----
    RemovedEnv {
        name: "SERVO_VIDEO_ESCAPE",
        pref: "gfx_video_escape_mode",
        message: "use --pref gfx_video_escape_mode=external; \
                  the value is a mode token, not a boolean (empty = off)",
    },
    RemovedEnv {
        name: "SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN",
        pref: "gfx_video_escape_stable_swapchain",
        message: "use --pref gfx_video_escape_stable_swapchain=false; \
                  this is a kill switch that defaults to true, so the old =0 means =false",
    },
    RemovedEnv {
        name: "SERVO_VIDEO_ESCAPE_PROMOTE_HYSTERESIS",
        pref: "gfx_video_escape_promote_hysteresis",
        message: "use --pref gfx_video_escape_promote_hysteresis=N (default 10)",
    },
    RemovedEnv {
        name: "SERVO_VIDEO_DECOUPLE",
        pref: "gfx_video_decouple_enabled",
        message: "use --pref gfx_video_decouple_enabled=false; \
                  this is a kill switch that defaults to true, so the old =0 means =false",
    },
    // ---- Task 5: media_* 9 ----
    RemovedEnv {
        name: "SERVO_MEDIA_D3D11_VIDEO",
        pref: "media_d3d11_enabled",
        message: "use --pref media_d3d11_enabled=true",
    },
    RemovedEnv {
        name: "SERVO_MEDIA_SYNC_GROUP",
        pref: "media_sync_group_target",
        message: "use --pref media_sync_group_target=N where N is the number of pipelines to \
                  start together (the old value, not a boolean); N below 2 disables it",
    },
    RemovedEnv {
        name: "SERVO_MEDIA_GAPLESS_LOOP",
        pref: "media_gapless_loop_enabled",
        message: "use --pref media_gapless_loop_enabled=true",
    },
    RemovedEnv {
        name: "SERVO_MEDIA_DIRECT_FILE",
        pref: "media_direct_file_enabled",
        message: "use --pref media_direct_file_enabled=true",
    },
    RemovedEnv {
        name: "SERVO_GSTREAMER_AVDEC_MAX_THREADS",
        pref: "media_avdec_max_threads",
        message: "use --pref media_avdec_max_threads=N (-1 = automatic, the old unset state)",
    },
    RemovedEnv {
        name: "SERVO_GSTREAMER_DISABLE_AUDIO",
        pref: "media_audio_enabled",
        message: "the replacement is inverted: use --pref media_audio_enabled=false to get \
                  what SERVO_GSTREAMER_DISABLE_AUDIO=1 used to do",
    },
    RemovedEnv {
        name: "SERVO_GSTREAMER_VIDEO_DECODER_POLICY",
        pref: "media_video_decoder_policy",
        message: "use --pref media_video_decoder_policy=auto (or =software; empty = software)",
    },
    RemovedEnv {
        name: "SERVO_VIDEO_SINK_POLICY",
        pref: "media_video_sink_policy",
        message: "use --pref media_video_sink_policy=low-latency (or =smooth; empty = smooth)",
    },
    RemovedEnv {
        name: "SERVO_WEBRTC_JITTER_LATENCY_MS",
        pref: "media_webrtc_jitter_latency_ms",
        message: "use --pref media_webrtc_jitter_latency_ms=N (default 0)",
    },
    // ---- 추가 이관: 조사용으로 등록돼 있었으나 실제로는 운용 노브였다 ----
    RemovedEnv {
        name: "SERVO_WR_PICTURE_TILE_SIZE",
        pref: "gfx_wr_picture_tile_size",
        message: "use --pref gfx_wr_picture_tile_size=WxH (e.g. 1920x1080), or \
                  --pref gfx_wr_picture_tile_size=display to match each tile window's own size",
    },
];

pub fn lookup(name: &str) -> Option<&'static RemovedEnv> {
    REMOVED.iter().find(|entry| entry.name == name)
}

/// 판정 본체. `is_set` 이 참을 돌려주는 이름마다 안내 한 줄을 만든다.
///
/// env 읽기를 주입받는 이유는 테스트 때문이다 — `check()` 는 프로세스 전역 상태를 읽는데
/// 테스트는 병렬로 돌고 Rust 2024 에서 `set_var` 는 `unsafe` 다. 순수 함수로 갈라 두면
/// 조립 결과를 env 를 건드리지 않고 확인할 수 있다.
pub fn blocked_by(is_set: impl Fn(&str) -> bool) -> Vec<String> {
    REMOVED
        .iter()
        .filter(|entry| is_set(entry.name))
        .map(|entry| format!("{} is no longer read; {}", entry.name, entry.message))
        .collect()
}

/// 설정돼 있는 제거된 env 를 전부 모아 `Err` 로 돌려준다.
///
/// 하나만 보고하고 멈추지 않는다 — 스크립트는 대개 여러 개를 함께 설정하므로, 하나씩
/// 고치며 재기동하게 만들면 안 된다.
pub fn check() -> Result<(), Vec<String>> {
    let blocked = blocked_by(|name| std::env::var_os(name).is_some());
    if blocked.is_empty() {
        Ok(())
    } else {
        Err(blocked)
    }
}

/// 두 셸의 기동 경로가 부르는 편의 함수. 막히면 안내를 전량 찍고 종료한다.
///
/// `eprintln!` 인 이유는 이 시점에 로거가 아직 설치되지 않았을 수 있어서다 — Task 3 에서
/// `warn!` 로 찍은 경고가 로거보다 일러 **한 번도 뜨지 않은** 적이 있다.
pub fn check_or_exit() {
    if let Err(messages) = check() {
        eprintln!(
            "servo: error: {} environment variable(s) moved to prefs are still set.",
            messages.len()
        );
        for message in &messages {
            eprintln!("servo: error:   {message}");
        }
        // 마지막 줄은 **자기완결적**이어야 한다 — 배포본(ServoWallPackage)에는 docs/ 가
        // 들어가지 않으므로 저장소 경로를 가리키면 현장 운영자가 없는 파일을 찾게 된다.
        // 위 줄들이 이미 `--pref <이름>=<값>` 실행 예시를 담고 있으니, 여기서는 그 예시를
        // 그대로 따라 할 때 걸리는 마지막 함정(점 표기)만 짚는다.
        eprintln!("servo: error: use --pref instead; names use underscores, not dots.");
        std::process::exit(1);
    }
}
