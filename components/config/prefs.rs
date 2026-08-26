/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Preferences are the global configuration options that can be changed at runtime.

use std::env::consts::ARCH;
use std::sync::{RwLock, RwLockReadGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use servo_config_macro::ServoPreferences;

pub use crate::pref_util::PrefValue;

static PREFERENCES: RwLock<Preferences> = RwLock::new(Preferences::const_default());

/// A trait to be implemented by components that wish to be notified about runtime changes to the
/// global preferences for the current process.
pub trait PreferencesObserver: Send + Sync {
    /// This method is called when the global preferences have been updated. The argument to the
    /// method is an array of tuples where the first component is the name of the preference and
    /// the second component is the new value of the preference.
    fn prefs_changed(&self, _changes: &[(&'static str, PrefValue)]) {}
}

static OBSERVERS: RwLock<Vec<Box<dyn PreferencesObserver>>> = RwLock::new(Vec::new());

#[inline]
/// Get the current set of global preferences for Servo.
pub fn get() -> RwLockReadGuard<'static, Preferences> {
    PREFERENCES.read().unwrap()
}

/// Subscribe to notifications about changes to the global preferences for the current process.
pub fn add_observer(observer: Box<dyn PreferencesObserver>) {
    OBSERVERS.write().unwrap().push(observer);
}

/// Update the values of the global preferences for the current process. This also notifies the
/// observers previously added using [`add_observer`].
pub fn set(preferences: Preferences) {
    // Get list of changes, returning early if the preferences haven't changed.
    let changed = preferences.diff(&PREFERENCES.read().unwrap());
    if changed.is_empty() {
        return;
    }

    // Map between Stylo preference names and Servo preference names as the This should be
    // kept in sync with components/script/dom/bindings/codegen/run.py which generates the
    // DOM CSS style accessors.
    stylo_static_prefs::set_pref!("layout.unimplemented", preferences.layout_unimplemented);
    stylo_static_prefs::set_pref!("layout.threads", preferences.layout_threads as i32);
    stylo_static_prefs::set_pref!("layout.columns.enabled", preferences.layout_columns_enabled);
    stylo_static_prefs::set_pref!("layout.grid.enabled", preferences.layout_grid_enabled);
    stylo_static_prefs::set_pref!(
        "layout.css.attr.enabled",
        preferences.layout_css_attr_enabled
    );
    stylo_static_prefs::set_pref!(
        "layout.writing-mode.enabled",
        preferences.layout_writing_mode_enabled
    );
    stylo_static_prefs::set_pref!(
        "layout.container-queries.enabled",
        preferences.layout_container_queries_enabled
    );
    stylo_static_prefs::set_pref!(
        "layout.variable_fonts.enabled",
        preferences.layout_variable_fonts_enabled
    );

    *PREFERENCES.write().unwrap() = preferences;

    for observer in &*OBSERVERS.read().unwrap() {
        observer.prefs_changed(&changed);
    }
}

/// A convenience macro for accessing a preference value using its static path.
/// Passing an invalid path is a compile-time error.
#[macro_export]
macro_rules! pref {
    ($name: ident) => {
        $crate::prefs::get().$name.clone()
    };
}

/// The set of global preferences supported by Servo.
///
/// Each preference has a default value that determines its initial state. These defaults
/// fall into roughly three categories:
/// - **Stable**: enabled by default.
/// - **Experimental**: disabled by default, but intended to be enabled for experimental use.
/// - **Unstable**: disabled by default.
///
/// For a full overview of which preferences are experimental, see the
/// [experimental features documentation](https://book.servo.org/design-documentation/experimental-features.html).
#[derive(Clone, Deserialize, Serialize, ServoPreferences)]
pub struct Preferences {
    pub fonts_default: String,
    pub fonts_serif: String,
    pub fonts_sans_serif: String,
    pub fonts_monospace: String,
    pub fonts_default_size: i64,
    pub fonts_default_monospace_size: i64,
    /// The amount of time that a half cycle of a text caret blink takes in milliseconds.
    /// If this value is less than or equal to zero, then caret blink is disabled.
    pub editing_caret_blink_time: i64,
    pub css_animations_testing_enabled: bool,
    /// Start the devtools server at startup
    pub devtools_server_enabled: bool,
    /// The address:port the devtools server listens to, default to 127.0.0.1:7000.
    pub devtools_server_listen_address: String,
    // feature: WebGPU | #24706 | Web/API/WebGPU_API
    pub dom_webgpu_enabled: bool,
    /// List of comma-separated backends to be used by wgpu.
    pub dom_webgpu_wgpu_backend: String,
    /// Multi-GPU tiled-wall fan-out: mirror WebGPU device/command work onto a
    /// dedicated wgpu device per additional physical GPU so each wall tile can
    /// eventually render/compute on its own GPU (DX12/Windows only). Off by default.
    pub dom_webgpu_multigpu_fanout: bool,
    /// Multi-GPU GPU-direct present: copy each frame's WebGPU canvas into a shared D3D12
    /// texture and let the tile compositor sample it directly (no CPU readback). Requires
    /// `dom_webgpu_multigpu_fanout`. Experimental; off by default.
    pub dom_webgpu_gpu_direct: bool,
    // feature: AbortController | #34866 | Web/API/AbortController
    pub dom_abort_controller_enabled: bool,
    // feature: Adopted Stylesheet | #38132 | Web/API/Document/adoptedStyleSheets
    pub dom_adoptedstylesheet_enabled: bool,
    pub dom_allow_preloading_module_descendants: bool,
    // feature: Clipboard API | #36084 | Web/API/Clipboard_API
    pub dom_async_clipboard_enabled: bool,
    pub dom_bluetooth_enabled: bool,
    pub dom_bluetooth_testing_enabled: bool,
    pub dom_allow_scripts_to_close_windows: bool,
    // feature: Media Capture and Streams API | #26861 | Web/API/Media_Capture_and_Streams_API
    pub dom_canvas_capture_enabled: bool,
    pub dom_canvas_text_enabled: bool,
    /// Selects canvas backend
    ///
    /// Available values:
    /// - ` `/`auto`
    /// - vello
    /// - vello_cpu
    pub dom_canvas_backend: String,
    pub dom_clipboardevent_enabled: bool,
    pub dom_composition_event_enabled: bool,
    // feature: CookieStore | #37674 | Web/API/CookieStore
    pub dom_cookiestore_enabled: bool,
    // feature: Credential Management API | #38788 | Web/API/Credential_Management_API
    pub dom_credential_management_enabled: bool,
    // feature: WebCrypto API | #40687 | Web/API/Web_Crypto_API
    pub dom_crypto_subtle_enabled: bool,
    pub dom_document_dblclick_timeout: i64,
    pub dom_document_dblclick_dist: i64,
    /// Enforce the framing policy a site declares — `X-Frame-Options` and the CSP
    /// `frame-ancestors` directive — when a **child** navigable (an `iframe`) is
    /// navigated. On by default; this is the standard behaviour.
    ///
    /// Turning it off is a video-wall escape hatch: a wall page that embeds outside
    /// sites in `iframe`s is blocked by nearly every commercial site, and the wall is
    /// a display-only kiosk on a private network, so the clickjacking defence these
    /// headers provide buys nothing. It does not affect top-level navigation, which
    /// these checks never blocked in the first place.
    pub dom_enforce_framing_policy: bool,
    /// `<iframe toplevel>` 을 인정한다. 켜면 그 속성이 붙은 iframe 의 내용이 부모의
    /// 박스 안에서 그대로 렌더되면서(모든 CSS 적용) browsing context 만 top-level 이
    /// 되어, `X-Frame-Options` / CSP `frame-ancestors` / `top !== self` 프레임버스팅이
    /// 성립하지 않는다. 꺼져 있으면 그 속성은 완전히 무시되고 평범한 iframe 이다.
    ///
    /// ★설계상 스푸핑을 허용한다★ — 우리 UI 안에 남의 사이트를 진짜처럼 배치할 수
    /// 있다. 사설망 표출 전용 키오스크라는 전제에서만 켠다. v1 은 입력 라우팅이 없어
    /// 클릭재킹은 성립하지 않지만, 입력을 얹으면 그 항목은 되살아난다.
    pub dom_iframe_toplevel_embed_enabled: bool,
    // feature: Document.execCommand | #25005 | Web/API/Document/execCommand
    pub dom_exec_command_enabled: bool,
    // feature: CSS Font Loading API | #29376 | Web/API/CSS_Font_Loading_API
    pub dom_fontface_enabled: bool,
    pub dom_fullscreen_test: bool,
    // feature: Gamepad API | #10977 | Web/API/Gamepad_API
    pub dom_gamepad_enabled: bool,
    // feature: Geolocation API | #38903 | Web/API/Geolocation_API
    pub dom_geolocation_enabled: bool,
    // feature: Screen Wake Lock API | #43615 | Web/API/Screen_Wake_Lock_API
    pub dom_wakelock_enabled: bool,
    // feature: IndexedDB | #6963 | Web/API/IndexedDB_API
    pub dom_indexeddb_enabled: bool,
    // feature: IntersectionObserver | #35767 | Web/API/Intersection_Observer_API
    pub dom_intersection_observer_enabled: bool,
    pub dom_microdata_testing_enabled: bool,
    pub dom_uievent_which_enabled: bool,
    // feature: MutationObserver | #6633 | Web/API/MutationObserver
    pub dom_mutation_observer_enabled: bool,
    // feature: Navigator.registerProtocolHandler() | #40615 | Web/API/Navigator/registerProtocolHandler
    pub dom_navigator_protocol_handlers_enabled: bool,
    // feature: Notification API | #34841 | Web/API/Notifications_API
    pub dom_notification_enabled: bool,
    // feature: OffscreenCanvas | #34111 | Web/API/OffscreenCanvas
    pub dom_offscreen_canvas_enabled: bool,
    pub dom_parallel_css_parsing_enabled: bool,
    // feature: Permissions API | #31235 | Web/API/Permissions_API
    pub dom_permissions_enabled: bool,
    pub dom_permissions_testing_allowed_in_nonsecure_contexts: bool,
    // feature: ResizeObserver | #39790 | Web/API/ResizeObserver
    pub dom_resize_observer_enabled: bool,
    /// Let the STANDARD `<img>` element transparently decode image formats
    /// beyond the browser-standard allowlist (TIFF/EXR/HDR/TGA/DDS/QOI/PNM/JPEG
    /// XL/…) via `pixels::load_extended_from_memory`. Truly undecodable data
    /// still falls back to the standard broken-image/`error` path. Off by
    /// default; standard formats are unaffected.
    pub dom_image_extended_formats_enabled: bool,
    /// Let the STANDARD `<video>` element report non-standard containers
    /// (Matroska/AVI/WMV/MPEG-TS/FLV/…) as playable for `<source type>`
    /// selection and `canPlayType()`. Off by default.
    pub dom_video_extended_containers_enabled: bool,
    /// Let the STANDARD `<video>` element play direct-URI network streams
    /// (`rtsp://`/`rtsps://`) by routing to a GStreamer `NetworkUri` player
    /// instead of the AppSrc fetch path. Off by default.
    pub dom_video_network_uri_enabled: bool,
    // feature: Sanitizer API | #43948 | Web/API/HTML_Sanitizer_API
    pub dom_sanitizer_enabled: bool,
    /// Enable the screen-capture API (`MediaDevices.getDisplayMedia()`).
    // feature: Screen Capture | Web/API/Screen_Capture_API
    pub dom_screen_capture_enabled: bool,
    pub dom_script_asynch: bool,
    // feature: Storage API | #43976 | Web/API/Storage_API
    pub dom_storage_manager_api_enabled: bool,
    // feature: ServiceWorker | #36538 | Web/API/Service_Worker_API
    pub dom_serviceworker_enabled: bool,
    pub dom_serviceworker_timeout_seconds: i64,
    // feature: SharedWorker | #7458 | Web/API/SharedWorker
    pub dom_sharedworker_enabled: bool,
    pub dom_servo_helpers_enabled: bool,
    pub dom_servoparser_async_html_tokenizer_enabled: bool,
    pub dom_testbinding_enabled: bool,
    pub dom_testbinding_prefcontrolled_enabled: bool,
    pub dom_testbinding_prefcontrolled2_enabled: bool,
    pub dom_testbinding_preference_value_falsy: bool,
    pub dom_testbinding_preference_value_quote_string_test: String,
    pub dom_testbinding_preference_value_space_string_test: String,
    pub dom_testbinding_preference_value_string_empty: String,
    pub dom_testbinding_preference_value_string_test: String,
    pub dom_testbinding_preference_value_truthy: bool,
    pub dom_testing_element_activation_enabled: bool,
    pub dom_testing_html_input_element_select_files_enabled: bool,
    pub dom_testperf_enabled: bool,
    // https://testutils.spec.whatwg.org#availability
    pub dom_testutils_enabled: bool,
    /// <https://w3c.github.io/touch-events/#conditionally-exposing-legacy-touch-event-apis>
    pub dom_touch_events_legacy_apis_enabled: bool,
    /// <https://html.spec.whatwg.org/multipage/#transient-activation-duration>
    pub dom_transient_activation_duration_ms: i64,
    /// Enable WebGL2 APIs.
    // feature: WebGL2 | #41394 | Web/API/WebGL2RenderingContext
    pub dom_webgl2_enabled: bool,
    // feature: WebRTC | #41396 | Web/API/WebRTC_API
    pub dom_webrtc_enabled: bool,
    // feature: WebRTC Transceiver | #41396 | Web/API/RTCRtpTransceiver
    pub dom_webrtc_transceiver_enabled: bool,
    // feature: WebVTT | #22312 | Web/API/WebVTT_API
    pub dom_webvtt_enabled: bool,
    pub dom_webxr_enabled: bool,
    pub dom_webxr_test: bool,
    pub dom_webxr_first_person_observer_view: bool,
    pub dom_webxr_glwindow_enabled: bool,
    pub dom_webxr_glwindow_left_right: bool,
    pub dom_webxr_glwindow_red_cyan: bool,
    pub dom_webxr_glwindow_spherical: bool,
    pub dom_webxr_glwindow_cubemap: bool,
    pub dom_webxr_hands_enabled: bool,
    // feature: WebXR Layers | #27468 | Web/API/XRCompositionLayer
    pub dom_webxr_layers_enabled: bool,
    pub dom_webxr_openxr_enabled: bool,
    pub dom_webxr_sessionavailable: bool,
    pub dom_webxr_unsafe_assume_user_intent: bool,
    pub dom_worklet_enabled: bool,
    pub dom_worklet_blockingsleep_enabled: bool,
    pub dom_worklet_testing_enabled: bool,
    pub dom_worklet_timeout_ms: i64,
    /// <https://drafts.csswg.org/cssom-view/#the-visualviewport-interface>
    // feature: VisualViewport | #41341 | Web/API/VisualViewport
    pub dom_visual_viewport_enabled: bool,
    /// True to compile all WebRender shaders when Servo initializes. This is mostly
    /// useful when modifying the shaders, to ensure they all compile after each change is
    /// made.
    pub gfx_precache_shaders: bool,
    /// Whether or not antialiasing is enabled for text rendering.
    pub gfx_text_antialiasing_enabled: bool,
    /// Whether or not subpixel antialiasing is enabled for text rendering.
    pub gfx_subpixel_text_antialiasing_enabled: bool,
    pub gfx_texture_swizzling_enabled: bool,
    /// DirectComposition 네이티브 컴포지터 모드.
    /// `off` = 기존 Draw 경로, `on` = 하이브리드(전면 갱신 서피스를 스왑체인으로 승격),
    /// `surface` = 가상 서피스 전용. 3 상태이므로 bool 두 개로 쪼개지 않는다
    /// (on 과 surface 가 배타적이라는 것을 타입이 막아야 한다).
    /// **빈 문자열(`""`)이 기본값이고 `"off"`와 같은 뜻이다** — 이 파일의 다른 모든
    /// String pref 와 같은 관례(`const_default()`는 항상 빈 문자열, 실제 기본 동작은
    /// 소비하는 쪽이 "미설정"으로 해석)를 따른다. 옛 `SERVO_COMPOSITOR_DCOMP` env 가
    /// 마찬가지로 미설정=off 였으니 빈 문자열이 그 상태와 정확히 대응한다.
    /// 파싱은 `paint_api::rendering_context::DcompMode::parse` 한 곳뿐이다(Task 3 완료) —
    /// 두 셸(servoshell/winit_wall)의 기동 경로가 이 필드를 파싱해 `set_dcomp_mode()`로
    /// `RenderingContext` 생성 전에 주입하고, surfman은 그 결과 불리언만 받는다. 옛
    /// `SERVO_COMPOSITOR_DCOMP` env는 더 이상 읽히지 않는다.
    pub gfx_dcomp_mode: String,
    /// Windows 에서 DWM 합성 클럭(vsync)에 프레임 생산을 맞출지 여부. 기본 꺼짐 —
    /// `DwmFlush` 가 스핀-웨이트로 동작해 코어 1개를 상시 소모한다(`vsync_refresh_driver.rs`).
    pub gfx_vsync_enabled: bool,
    /// 프리-vsync 페이싱용 자유 실행 페인트 타이머 주기(Hz). 특정 디스플레이 주사율(예 60)에
    /// 맞춰 프레임 생산이 vsync 를 앞질러 저더가 생기는 것을 줄인다. `[1, 1000]` 범위를
    /// 벗어나면 경고 후 기본값(120)을 쓴다.
    pub gfx_refresh_hz: i64,
    /// wall frame barrier 의 프레임 페이싱(최신-프레임-우선 코얼레싱)을 켤지 여부. 기본
    /// 켜짐 — 꺼지면 도착한 프레임을 전부 순서대로 페인트하는 이전(legacy) 동작으로 되돌아간다.
    pub gfx_wall_frame_pacing_enabled: bool,
    /// wall frame 코얼레싱이 쌓아 둘 수 있는 최대 보류 프레임 수. 0 이하면 경고 후
    /// 기본값(1)을 쓴다.
    pub gfx_wall_frame_max_pending: i64,
    /// wall frame 코얼레싱 간 최소 간격(ms). 0 이하면 경고 후 기본값(16)을 쓴다.
    pub gfx_wall_frame_min_interval_ms: i64,
    /// WebRender picture cache 의 타일 크기 오버라이드(구 env `SERVO_WR_PICTURE_TILE_SIZE`).
    ///
    /// 인정하는 값 셋:
    /// - `""`(기본) — 오버라이드하지 않는다. WR 기본 분기를 그대로 쓴다(콘텐츠 1024x512,
    ///   스크롤바는 WR 이 자체 특수 크기를 고른다).
    /// - `WxH`(예 `1920x1080`) — 모든 painter 가 같은 크기를 쓴다.
    /// - `display` — **painter 마다 자기 창의 실제 크기**로 정한다. 타일 해상도가 섞인
    ///   월에서도 각 타일이 자기 해상도에 맞고, 레이아웃을 바꿔도 값을 다시 적을 필요가 없다.
    ///
    /// 타일 크기가 창 이상이면 슬라이스당 타일이 1 장이 된다 — 타일 수·무효화 입도·타일당
    /// DComp bind/unbind 오버헤드가 함께 달라지므로 A/B 축이자 운용 노브다. DComp 투명 구멍의
    /// 현행 회피책이 이 값을 디스플레이 해상도에 맞추는 것이라 조사용 env 가 아니라 pref 다.
    ///
    /// ★값을 크게 잡을 때 주의★: **WebRender 는 이 값을 클램프하지 않는다**(2026-08-12 확인 —
    /// `render_backend.rs` 가 `frame_config.tile_size_override` 에 그대로 넣고 `picture.rs` 가
    /// 그대로 `desired_tile_size` 로 쓴다). 실질 상한은 GPU 텍스처 크기이고, 넘으면 타일
    /// 텍스처 할당이 실패한다.
    pub gfx_wr_picture_tile_size: String,
    /// 비디오 프레임을 WebRender 콘텐츠 패스에서 분리해 DirectComposition 외부 서피스로
    /// "탈출"시킬지의 게이트 모드. `external` = 탈출 on(스왑체인/가상 서피스 프로모션
    /// 후보), 그 외(빈 문자열 포함) = off. **`off`/`on`/`true` 류 3-상태 truthy 표기가
    /// 아니다** — 유효 토큰은 `external` 단 하나뿐이고, 그 정본은
    /// `paint_api::rendering_context::VideoEscapeMode`/`parse_video_escape_token`이다(옛
    /// `native` 진단 모드는 미표출 결함 확정으로 제거됨 — 그 흔적이 남아 있으면 오해하기
    /// 쉽다). 파싱은 그 한 곳뿐이고, 두 셸(servoshell/winit_wall)의 기동 경로가
    /// `set_video_escape_mode()`로 주입한다 — `gfx_dcomp_mode`/`DcompMode::parse`와 같은
    /// 패턴(Task 3). DComp 네이티브 컴포지터 게이트가 꺼져 있으면 이 값과 무관하게 항상
    /// Off 로 취급된다(`video_escape_mode()`가 매 호출 재확인).
    pub gfx_video_escape_mode: String,
    /// 외부 탈출 스왑체인 안정화 킬스위치. 기본 켜짐 — 끄면(false) 스왑체인을 매 프레임
    /// clip 크기로 재생성하는 옛 동작(AMD A/B, 롤백용)으로 되돌아간다. 옛 env
    /// `SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN`은 `as_deref() != Ok("0")` 판정이라 미설정이
    /// 곧 on 이었다 — 이 bool pref 의 기본값 `true`가 그 상태를 그대로 보존한다.
    pub gfx_video_escape_stable_swapchain: bool,
    /// 비디오를 외부 컴포지터 서피스로 프로모션하기 전 요구하는 연속 2D
    /// scale/translation 프레임 수(히스테리시스). 0 이면 즉시 프로모션(레거시 동작).
    /// 옛 env `SERVO_VIDEO_ESCAPE_PROMOTE_HYSTERESIS`의 `unwrap_or(10)`을 그대로
    /// 기본값 10 으로 보존한다.
    pub gfx_video_escape_promote_hysteresis: i64,
    /// 비디오별 external flip 스왑체인의 백버퍼 수. 기본 2 = 현행.
    ///
    /// ★2 면 in-flight 프레임이 1 장뿐이라 `Present` 가 컴포지터의 백버퍼 반납을 기다린다★ —
    /// 45 영상 실측에서 렌더러 스레드가 1 초 중 85%(854ms)를 Present 에서 보내는데 CPU 는
    /// 0.3 코어도 쓰지 않는다(= 일하는 게 아니라 자고 있다). 1 회당 비용이 영상 수에 따라
    /// 0.42ms(20 개) → 0.70ms(45 개)로 커지는 것도 고정 드라이버 비용이 아니라 대기라는
    /// 쪽에 맞는다. 이 값을 올려 그 대기를 없앨 수 있는지 재는 노브다.
    ///
    /// **콘텐츠 스왑체인에는 적용되지 않는다** — 그쪽 부분 Present 의 catch-up 복사가
    /// `GetBuffer(1)` = 직전 프레임 버퍼라는 정확한 2 버퍼 핑퐁을 전제한다(스펙 §5.1-1).
    /// 이 pref 는 external 비디오 스왑체인 전용이다.
    ///
    /// 범위 밖(2 미만/4 초과)이면 경고 후 클램프한다.
    pub gfx_video_escape_buffer_count: i64,
    /// 외부 비디오 갱신을 콘텐츠 프레임 생성(`generate_frame`)에서 분리할지의 킬스위치.
    /// 기본 켜짐 — 옛 env `SERVO_VIDEO_DECOUPLE`은 `map(|v| v != "0").unwrap_or(true)`
    /// 판정이라 미설정이 곧 on 이었다. 끄면(false) 비디오 프레임마다 generate_frame 을
    /// 부르는 이전 동작으로 되돌아간다.
    pub gfx_video_decouple_enabled: bool,
    /// The amount of image keys we request per batch for the image cache.
    pub image_key_batch_size: i64,
    /// Whether or not the DOM inspector should show shadow roots of user-agent shadow trees
    pub inspector_show_servo_internal_shadow_roots: bool,
    /// A locale tag (eg. es-ES) to use for language negotiation instead of the system locale.
    /// An empty string represents no override.
    /// TODO: Option<> support in PrefValue
    pub intl_locale_override: String,
    pub js_asmjs_enabled: bool,
    pub js_baseline_interpreter_enabled: bool,
    /// Whether to disable the jit within SpiderMonkey
    pub js_disable_jit: bool,
    pub js_baseline_jit_enabled: bool,
    pub js_baseline_jit_unsafe_eager_compilation_enabled: bool,
    pub js_ion_enabled: bool,
    pub js_ion_unsafe_eager_compilation_enabled: bool,
    pub js_mem_gc_compacting_enabled: bool,
    pub js_mem_gc_empty_chunk_count_min: i64,
    pub js_mem_gc_high_frequency_heap_growth_max: i64,
    pub js_mem_gc_high_frequency_heap_growth_min: i64,
    pub js_mem_gc_high_frequency_high_limit_mb: i64,
    pub js_mem_gc_high_frequency_low_limit_mb: i64,
    pub js_mem_gc_high_frequency_time_limit_ms: i64,
    pub js_mem_gc_incremental_enabled: bool,
    pub js_mem_gc_incremental_slice_ms: i64,
    pub js_mem_gc_low_frequency_heap_growth: i64,
    pub js_mem_gc_per_zone_enabled: bool,
    pub js_mem_gc_zeal_frequency: i64,
    pub js_mem_gc_zeal_level: i64,
    pub js_mem_max: i64,
    pub js_native_regex_enabled: bool,
    pub js_offthread_compilation_enabled: bool,
    pub js_timers_minimum_duration: i64,
    pub js_wasm_baseline_enabled: bool,
    pub js_wasm_enabled: bool,
    pub js_wasm_ion_enabled: bool,
    // feature: Largest Contentful Paint | #42000 | Web/API/LargestContentfulPaint
    pub largest_contentful_paint_enabled: bool,
    pub layout_animations_test_enabled: bool,
    // feature: CSS Multicol | #22397 | Web/CSS/Guides/Multicol_layout
    pub layout_columns_enabled: bool,
    // feature: CSS Grid | #34479 | Web/CSS/Guides/Grid_layout
    pub layout_grid_enabled: bool,
    pub layout_container_queries_enabled: bool,
    pub layout_css_attr_enabled: bool,
    pub layout_style_sharing_cache_enabled: bool,
    pub layout_threads: i64,
    pub layout_unimplemented: bool,
    // feature: Variable fonts | #38800 | Web/CSS/Guides/Fonts/Variable_fonts
    pub layout_variable_fonts_enabled: bool,
    // feature: CSS writing modes | #2560 | Web/CSS/Guides/Writing_modes
    pub layout_writing_mode_enabled: bool,
    /// Enable hardware acceleration for video playback.
    pub media_glvideo_enabled: bool,
    /// For a local `file://` `<video>`/`<audio>`, let the media engine read the file directly
    /// (GStreamer `filesrc`) instead of fetching and pushing its bytes through Servo's network
    /// stack. More robust seek/loop and no double disk read; bypasses the resource loader (no
    /// CSP/`buffered`/progress integration), so it applies to local files only. On by default;
    /// turn off to force the servosrc byte-push path (e.g. web-compat debugging).
    pub media_local_direct_file: bool,
    /// Use generated media sources for getUserMedia() instead of host capture devices.
    pub media_capture_mocking_enabled: bool,
    /// Zero-based monitor index captured by `getDisplayMedia()` (-1 = primary monitor).
    /// Ignored when `media_screen_capture_window_title` is non-empty.
    pub media_screen_capture_monitor_index: i64,
    /// Whether `getDisplayMedia()` shows the mouse cursor in the captured stream.
    pub media_screen_capture_show_cursor: bool,
    /// If non-empty, `getDisplayMedia()` captures the first top-level window whose title
    /// contains this string (window capture) instead of a whole monitor.
    pub media_screen_capture_window_title: String,
    /// Enable a non-standard event handler for verifying behavior of media elements during tests.
    pub media_testing_enabled: bool,
    /// GStreamer D3D11 per-pipeline GPU 업로드/YUV 직접 샘플 경로(구 env
    /// `SERVO_MEDIA_D3D11_VIDEO`). 기본 꺼짐 — 꺼지면 기존 CPU(I420 borrowed) 경로가
    /// 쓰인다. `media(render-d3d11)`와 `paint(painter)` 두 크레이트가 각자 읽던 노브인데,
    /// 두 판정식이 문자 그대로 동일함을 확인했다(둘 다 `1`/`true`/`yes`/`on`, 대소문자
    /// 무시) — Task 5 로 pref 하나로 합쳐 두 크레이트가 같은 값을 본다.
    pub media_d3d11_enabled: bool,
    /// 다중 `<video>` 파이프라인 동시 시작 동기화(월 데모)에서 **함께 출발하기를 기다릴
    /// 파이프라인 목표 수**. 온오프 스위치가 아니다 — 구 env `SERVO_MEDIA_SYNC_GROUP=N`이
    /// 그랬듯 타일 수를 그대로 받는다(`player.rs::sync_group_target`: 구코드는 usize 파싱
    /// 후 `filter(|count| *count >= 2)`). `2` 미만(기본 `0` 포함) = 비활성이고, 이는 구 env
    /// 미설정과 같은 동작이다.
    ///
    /// 브리프/설계문서는 이 노브를 `media_sync_group_enabled`(bool)로 적었으나 실제 판정이
    /// 정수라 이름과 타입을 함께 고쳤다 — `_enabled` 접미사는 이 파일의 다른 모든 pref 에서
    /// bool 을 뜻하므로 `--pref media_sync_group_enabled=45` 는 읽는 사람을 속인다.
    pub media_sync_group_target: i64,
    /// `<video loop>` 무결절(gapless) SEGMENT 재탐색 루프(구 env
    /// `SERVO_MEDIA_GAPLESS_LOOP`). 기본 꺼짐 — 꺼지면 스펙대로 EOS -> flushing seek(0)
    /// 경로를 쓴다.
    pub media_gapless_loop_enabled: bool,
    /// 백엔드 전역 직접 로컬 파일 재생 오버라이드(구 env `SERVO_MEDIA_DIRECT_FILE`).
    /// `media_local_direct_file`(기본 켜짐, 스크립트 단 `is_direct_local` 힌트)과는 별개의
    /// 신호이고 `resolve_direct_file_url`에서 OR 로 합쳐진다 — 혼동하지 말 것. 기본 꺼짐.
    pub media_direct_file_enabled: bool,
    /// 소프트웨어 `avdec_*` 비디오 디코더의 워커 스레드 상한(구 env
    /// `SERVO_GSTREAMER_AVDEC_MAX_THREADS`). `-1` = 미설정(자동 스레드 수, 구 env 부재와
    /// 동일 — 디코더를 건드리지 않는다). `0` 이상이면 그 값으로 캡한다(구
    /// `parse::<i32>() >= 0` 검증 그대로).
    pub media_avdec_max_threads: i64,
    /// `rtspsrc` 지터버퍼 크기(ms). `-1` = 미설정 — GStreamer 기본값 **2000ms** 를 그대로
    /// 둔다(현행 동작).
    ///
    /// ★첫 프레임까지의 지연을 지배하는 값이다★ — 실측(`gst-launch`, 사내 카메라):
    /// 기본값 2000 에서 첫 디코드 프레임까지 2.02s, `latency=0` 에서 0.76s, `latency=200`
    /// 에서 0.73s 였다. 파이프라인을 손으로 조립해 얻는 이득은 0.24s 에 불과했으므로,
    /// 지연 문제의 지렛대는 구조가 아니라 이 값이다.
    ///
    /// 공짜가 아니다: 줄일수록 지터 흡수를 포기하는 것이라 네트워크가 흔들리면 끊김으로
    /// 나타나고, 파라미터 세트(SPS/PPS)보다 슬라이스가 먼저 파서에 닿을 여지도 커진다
    /// (`media_rtsp_wait_for_keyframe` 참고).
    pub media_rtsp_latency_ms: i64,
    /// `rtph264depay` 의 `wait-for-keyframe`. 기본 `false` = 미설정(GStreamer 기본값과
    /// 동일, 현행 동작).
    ///
    /// 켜면 깨진 지점 이후 다음 키프레임까지 출력을 미룬다. RTSP 를 스트림 중간(mid-GOP)에
    /// 붙었을 때 `h264parse` 가 파라미터 세트 없이 슬라이스를 받아
    /// `Could not decode stream. No caps set` 로 죽는 간헐 실패를 겨냥한 노브다.
    pub media_rtsp_wait_for_keyframe: bool,
    /// GStreamer 오디오 트랙 활성화. **의미가 뒤집힌 pref다** — 구 env는
    /// `SERVO_GSTREAMER_DISABLE_AUDIO`(끄는 스위치, truthy 판정)였고 이 pref는 켜는
    /// 스위치다. servo 관례가 `*_enabled` 긍정형이고 이중부정은 실수의 단골 자리라
    /// Task 5 에서 뒤집었다. 기본 `true`(오디오 켜짐) = 구 env 미설정과 동일한 동작.
    pub media_audio_enabled: bool,
    /// 소프트웨어 비디오 디코더 선택 정책(구 env
    /// `SERVO_GSTREAMER_VIDEO_DECODER_POLICY`). 인정 토큰(대소문자 무시):
    /// `auto`/`default` = Auto(자동 선택 유지), `software`/`avdec` = Software(avdec_*
    /// 소프트웨어 디코더로 랭크 강제). 그 외 값은 경고 후 Software 로 폴백. **빈 문자열 =
    /// Software** — 구 env 미설정 시의 `Err(_) => Software` 그대로이고, 이 파일의 다른
    /// String pref 와 같은 "빈 문자열 = 관례상 기본값" 규약을 따른다.
    pub media_video_decoder_policy: String,
    /// 비디오 sink(appsink) 버퍼링/지연 정책(구 env `SERVO_VIDEO_SINK_POLICY`). 인정
    /// 토큰(대소문자 무시): `low-latency`/`low_latency`/`latency` = LowLatency,
    /// `smooth`/`complete` = Smooth. 그 외 값은 경고 후 Smooth 로 폴백. **빈 문자열 =
    /// Smooth** — 구 env 미설정 시의 `Err(_) => Smooth` 그대로.
    pub media_video_sink_policy: String,
    /// 비디오 appsink 의 `qos` 를 정책과 **독립적으로** 덮어쓴다.
    ///
    /// `media_video_sink_policy` 는 qos/drop/max-lateness/max-buffers 를 한 묶음으로
    /// 바꾸므로 qos 만 재려면 변수가 넷 섞인다. 이 pref 는 그 분리를 위한 것이다.
    ///
    /// 인정 토큰: 빈 문자열 = 정책값 그대로(기본), `on`/`true`/`1` = 강제 on,
    /// `off`/`false`/`0` = 강제 off.
    ///
    /// ★왜 필요한가★ — 지금 기본(Smooth)은 `qos=false` 다. GStreamer 요소 기본값이
    /// 무엇이든 이 포크가 명시적으로 끈다. qos 가 꺼져 있으면 QoS 이벤트가 상류로 가지
    /// 않아 **avdec 이 부하 상황에서 프레임을 건너뛰지 못한다** — 한계를 넘는 순간
    /// 완만한 열화가 아니라 절벽으로 무너진다(45 타일 CPU 100% 관측, 2026-08-25).
    pub media_video_sink_qos: String,
    /// 비디오 싱크의 **페이싱 방식**. `clock`(기본) 또는 `thread`.
    ///
    /// `clock` = 현행. appsink 가 `sync=true` 로 파이프라인 클럭을 기다린다.
    ///
    /// `thread` = appsink `sync=false` + 스트리밍 스레드가 PTS 앵커에 맞춰 직접 잔다.
    ///
    /// ★왜 필요한가★ — `GstSystemClock::obtain()` 은 프로세스당 싱글턴이라 45 개
    /// 파이프라인의 싱크가 프레임마다 **같은 클럭 객체**에서 대기한다. 실측(2026-08-25,
    /// 80 논리코어, 45 x FHD30):
    ///
    /// ```text
    /// 45 프로세스, 클럭 각자      0.399 코어/영상   머신 30%
    /// 1 프로세스, 클럭 공유       0.795 코어/영상   머신 81%
    /// 1 프로세스, 클럭 없이 sleep 0.284 코어/영상   머신 33%
    /// ```
    ///
    /// 디코딩 자체는 배치와 무관하게 0.39 코어인데, **공유 클럭 대기가 디코딩만큼을 더
    /// 쓴다.** Servo 코드 밖에서 재현되므로 GStreamer 배치 문제다.
    ///
    /// ★비범위★ — `thread` 는 파이프라인마다 독립 앵커라 **영상 간 동기가 보장되지
    /// 않는다**(공유 base time 을 쓰는 `media_sync_group_target` 과 양립 불가). 다중
    /// 영상 동기는 별도 과제 "Video Sync Group" 으로 분리했다.
    pub media_video_sink_pacing: String,
    /// 미디어 파이프라인을 무엇으로 구성할지. `playbin3`(기본) 또는 `uridecodebin3`.
    ///
    /// `playbin3` = 현행. `gstreamer_play::Play` 가 playbin3 를 소유한다.
    ///
    /// `uridecodebin3` = playsink 없이 파이프라인을 직접 만들고 appsink 를 손으로 링크한다.
    ///
    /// ★왜★ — playbin3 는 playsink 를 포함하고, playsink 는 디코더와 appsink 사이에
    /// `vqueue` 를 끼운다. 그 큐는 **원본 3.1MB 프레임을 프레임마다 스레드 경계 너머로
    /// 나른다.** 실측(45 x FHD30, Servo 없이 gst-launch, 코어/영상):
    ///
    /// ```text
    /// 큐 없음                      0.284
    /// 디먹서 직후 큐 + 오디오까지  0.294   <- 앞쪽 큐는 압축 데이터라 사실상 공짜
    /// multiqueue(앞) + queue(뒤)   0.729   <- 뒤쪽 큐가 비용 전부
    /// ```
    ///
    /// `decodebin3` 의 코덱 자동선택은 그대로 쓴다(H264/H265/VP9 등). 없애는 것은
    /// playsink 뿐이다.
    ///
    /// ★제약★ — 렌더러가 appsink 자체가 아닌 것을 붙이는 경우(unix 의 `glsinkbin`) 이
    /// 모드를 쓸 수 없고, 경고 후 `playbin3` 로 폴백한다. 트랙 선택 API 도 아직 없다.
    pub media_pipeline_mode: String,
    /// WebRTC `webrtcbin`의 지터버퍼 latency(ms, 구 env
    /// `SERVO_WEBRTC_JITTER_LATENCY_MS`). 기본 `0` = 무버퍼(최저 지연) — 구
    /// `unwrap_or(0)` 그대로.
    pub media_webrtc_jitter_latency_ms: i64,
    /// The default timeout set for establishing a network connection in seconds. This amount
    /// if for the entire process of connecting to an address. For instance, if a particular host is
    /// associated with multiple IP addresses, this timeout will be divided equally among
    /// each IP address.
    pub network_connection_timeout: u64,
    /// Block a non-secure subresource fetched from a secure context (mixed content).
    /// On by default; this is the standard behaviour.
    ///
    /// The companion of `dom_enforce_framing_policy` for the video wall. A wall page
    /// served from `file://` is a secure context by spec (`url/origin.rs`), so every
    /// `http://` target it embeds is blocked — and serving the page from `localhost`
    /// does not help, because loopback is potentially trustworthy too.
    pub network_enforce_mixed_content: bool,
    pub network_enforce_tls_enabled: bool,
    pub network_enforce_tls_localhost: bool,
    pub network_enforce_tls_onion: bool,
    pub network_http_cache_disabled: bool,
    /// A url for a http proxy. We treat an empty string as no proxy.
    pub network_http_proxy_uri: String,
    /// A url for a https proxy. We treat an empty string as no proxy.
    pub network_https_proxy_uri: String,
    /// The domains for which we will not have a proxy. No effect if `network_http_proxy_uri` is not set.
    /// The exact behavior is given by
    /// <https://docs.rs/hyper-util/latest/hyper_util/client/proxy/matcher/struct.Builder.html#method.no>
    pub network_http_no_proxy: String,
    /// The weight of the http memory cache
    /// Notice that this is not equal to the number of different urls in the cache.
    pub network_http_cache_size: u64,
    pub network_local_directory_listing_enabled: bool,
    /// Force the use of `rust-webpki` verification for CA roots. If this is false (the
    /// default), then `rustls-platform-verifier` will be used, except on Android where
    /// `rust-webpki` is always used.
    pub network_use_webpki_roots: bool,
    /// The length of the session history, in navigations, for each `WebView. Back-forward
    /// cache entries that are more than `session_history_max_length` steps in the future or
    /// `session_history_max_length` steps in the past will be discarded. Navigating forward
    /// or backward to that entry will cause the entire page to be reloaded.
    pub session_history_max_length: i64,
    /// The background color of shell's viewport. This will be used by OpenGL's `glClearColor`.
    pub shell_background_color_rgba: [f64; 4],
    pub webgl_testing_context_creation_error: bool,
    /// Maximum number of workers for the main thread pool
    pub thread_pool_workers_max: u64,
    /// Number of workers per thread pool, if we fail to detect how much
    /// parallelism is available at runtime.
    pub thread_pool_fallback_workers: u64,
    /// Maximum number of workers for the asynchronous networking runtime thread pool
    pub thread_pool_async_runtime_workers_max: u64,
    /// Maximum number of workers for WebRender
    pub thread_pool_webrender_workers_max: u64,
    /// The user-agent to use for Servo. This can also be set via [`UserAgentPlatform`] in
    /// order to set the value to the default value for the given platform.
    pub user_agent: String,
    /// Whether or not the viewport meta tag is enabled.
    pub viewport_meta_enabled: bool,
    pub log_filter: String,
    /// Whether the accessibility code is enabled.
    pub accessibility_enabled: bool,
    /// Whether to run accessibility tree integrity checks, and any other expensive checks.
    /// This should only be true in tests.
    pub expensive_accessibility_test_assertions_enabled: bool,
}

impl Preferences {
    // `pub`로 올렸다(원래는 비공개) — Task 2 브리프의 필수 테스트(config_surface.rs, 별도
    // 크레이트로 컴파일되는 통합 테스트)와 이 크레이트 안의 `config_dump` 모듈 양쪽에서
    // 불러야 한다. `const fn`은 그대로 유지한다 — 모든 String pref 가 빈 문자열
    // 기본값이라 const 컨텍스트 제약을 벗어나는 필드가 없다(gfx_dcomp_mode 도 포함).
    pub const fn const_default() -> Self {
        Self {
            css_animations_testing_enabled: false,
            editing_caret_blink_time: 600,
            devtools_server_enabled: false,
            devtools_server_listen_address: String::new(),
            dom_abort_controller_enabled: true,
            dom_adoptedstylesheet_enabled: false,
            dom_allow_preloading_module_descendants: false,
            dom_allow_scripts_to_close_windows: false,
            dom_async_clipboard_enabled: false,
            dom_bluetooth_enabled: false,
            dom_bluetooth_testing_enabled: false,
            dom_canvas_capture_enabled: false,
            dom_canvas_text_enabled: true,
            dom_canvas_backend: String::new(),
            dom_clipboardevent_enabled: true,
            dom_composition_event_enabled: false,
            dom_cookiestore_enabled: false,
            dom_credential_management_enabled: false,
            dom_crypto_subtle_enabled: true,
            dom_document_dblclick_dist: 1,
            dom_document_dblclick_timeout: 300,
            dom_enforce_framing_policy: true,
            dom_iframe_toplevel_embed_enabled: false,
            dom_exec_command_enabled: false,
            dom_fontface_enabled: false,
            dom_fullscreen_test: false,
            dom_gamepad_enabled: true,
            dom_geolocation_enabled: false,
            dom_wakelock_enabled: false,
            dom_indexeddb_enabled: false,
            dom_intersection_observer_enabled: false,
            dom_microdata_testing_enabled: false,
            dom_uievent_which_enabled: true,
            dom_mutation_observer_enabled: true,
            dom_navigator_protocol_handlers_enabled: false,
            dom_notification_enabled: false,
            dom_parallel_css_parsing_enabled: true,
            dom_offscreen_canvas_enabled: false,
            dom_permissions_enabled: false,
            dom_permissions_testing_allowed_in_nonsecure_contexts: false,
            dom_resize_observer_enabled: true,
            dom_image_extended_formats_enabled: false,
            dom_video_extended_containers_enabled: false,
            dom_video_network_uri_enabled: false,
            dom_sanitizer_enabled: false,
            dom_screen_capture_enabled: false,
            dom_script_asynch: true,
            dom_storage_manager_api_enabled: false,
            dom_serviceworker_enabled: false,
            dom_serviceworker_timeout_seconds: 60,
            dom_sharedworker_enabled: false,
            dom_servo_helpers_enabled: false,
            dom_servoparser_async_html_tokenizer_enabled: false,
            dom_testbinding_enabled: false,
            dom_testbinding_prefcontrolled2_enabled: false,
            dom_testbinding_prefcontrolled_enabled: false,
            dom_testbinding_preference_value_falsy: false,
            dom_testbinding_preference_value_quote_string_test: String::new(),
            dom_testbinding_preference_value_space_string_test: String::new(),
            dom_testbinding_preference_value_string_empty: String::new(),
            dom_testbinding_preference_value_string_test: String::new(),
            dom_testbinding_preference_value_truthy: false,
            dom_testing_element_activation_enabled: false,
            dom_testing_html_input_element_select_files_enabled: false,
            dom_testperf_enabled: false,
            dom_testutils_enabled: false,
            // Following Firefox and Chrome, we are enabling the touch events legacy APIs for android.
            // Additionally, enabling it in ohos for compatibility as well.
            dom_touch_events_legacy_apis_enabled: cfg!(target_os = "android")
                | cfg!(target_env = "ohos"),
            dom_transient_activation_duration_ms: 5000,
            dom_webgl2_enabled: false,
            dom_webgpu_enabled: false,
            dom_webgpu_wgpu_backend: String::new(),
            dom_webgpu_multigpu_fanout: false,
            dom_webgpu_gpu_direct: false,
            dom_webrtc_enabled: false,
            dom_webrtc_transceiver_enabled: false,
            dom_webvtt_enabled: false,
            dom_webxr_enabled: true,
            dom_webxr_first_person_observer_view: false,
            dom_webxr_glwindow_cubemap: false,
            dom_webxr_glwindow_enabled: true,
            dom_webxr_glwindow_left_right: false,
            dom_webxr_glwindow_red_cyan: false,
            dom_webxr_glwindow_spherical: false,
            dom_webxr_hands_enabled: true,
            dom_webxr_layers_enabled: false,
            dom_webxr_openxr_enabled: true,
            dom_webxr_sessionavailable: false,
            dom_webxr_test: false,
            dom_webxr_unsafe_assume_user_intent: false,
            dom_worklet_blockingsleep_enabled: false,
            dom_worklet_enabled: false,
            dom_worklet_testing_enabled: false,
            dom_worklet_timeout_ms: 10,
            dom_visual_viewport_enabled: false,
            accessibility_enabled: false,
            expensive_accessibility_test_assertions_enabled: false,
            fonts_default: String::new(),
            fonts_default_monospace_size: 13,
            fonts_default_size: 16,
            fonts_monospace: String::new(),
            fonts_sans_serif: String::new(),
            fonts_serif: String::new(),
            gfx_precache_shaders: false,
            gfx_text_antialiasing_enabled: true,
            gfx_subpixel_text_antialiasing_enabled: true,
            gfx_texture_swizzling_enabled: true,
            // 근거는 doc 주석 참고 (task-2 브리프 §4 로 확정; 배선은 Task 3 이 완료).
            gfx_dcomp_mode: String::new(), // 빈 문자열 = off. 위 필드 doc 주석 참고
            gfx_vsync_enabled: false,
            gfx_refresh_hz: 120,
            gfx_wall_frame_pacing_enabled: true,
            gfx_wall_frame_max_pending: 1,
            gfx_wall_frame_min_interval_ms: 16,
            // 빈 문자열 = 오버라이드 없음(WR 기본). 위 필드 doc 주석 참고.
            gfx_wr_picture_tile_size: String::new(),
            // 근거는 위 필드 doc 주석 참고 (task-4 브리프의 반례 표: 킬스위치 둘은 기본
            // on, promote_hysteresis 는 옛 unwrap_or(10) 그대로).
            gfx_video_escape_mode: String::new(), // 빈 문자열 = off (external 만 유효)
            gfx_video_escape_stable_swapchain: true,
            gfx_video_escape_promote_hysteresis: 10,
            gfx_video_escape_buffer_count: 2,
            gfx_video_decouple_enabled: true,
            image_key_batch_size: 10,
            inspector_show_servo_internal_shadow_roots: false,
            intl_locale_override: String::new(),
            js_asmjs_enabled: true,
            js_baseline_interpreter_enabled: true,
            js_baseline_jit_enabled: true,
            js_baseline_jit_unsafe_eager_compilation_enabled: false,
            js_disable_jit: false,
            js_ion_enabled: true,
            js_ion_unsafe_eager_compilation_enabled: false,
            js_mem_gc_compacting_enabled: true,
            js_mem_gc_empty_chunk_count_min: 1,
            js_mem_gc_high_frequency_heap_growth_max: 300,
            js_mem_gc_high_frequency_heap_growth_min: 150,
            js_mem_gc_high_frequency_high_limit_mb: 500,
            js_mem_gc_high_frequency_low_limit_mb: 100,
            js_mem_gc_high_frequency_time_limit_ms: 1000,
            js_mem_gc_incremental_enabled: true,
            js_mem_gc_incremental_slice_ms: 10,
            js_mem_gc_low_frequency_heap_growth: 150,
            js_mem_gc_per_zone_enabled: false,
            js_mem_gc_zeal_frequency: 100,
            js_mem_gc_zeal_level: 0,
            js_mem_max: -1,
            js_native_regex_enabled: true,
            js_offthread_compilation_enabled: true,
            js_timers_minimum_duration: 1000,
            js_wasm_baseline_enabled: true,
            js_wasm_enabled: true,
            js_wasm_ion_enabled: true,
            largest_contentful_paint_enabled: false,
            layout_animations_test_enabled: false,
            layout_columns_enabled: false,
            layout_container_queries_enabled: false,
            layout_css_attr_enabled: false,
            layout_grid_enabled: false,
            layout_style_sharing_cache_enabled: true,
            // TODO(mrobinson): This should likely be based on the number of processors.
            layout_threads: 3,
            layout_unimplemented: false,
            layout_variable_fonts_enabled: false,
            layout_writing_mode_enabled: false,
            media_capture_mocking_enabled: false,
            media_screen_capture_monitor_index: -1,
            media_screen_capture_show_cursor: true,
            media_screen_capture_window_title: String::new(),
            media_glvideo_enabled: false,
            media_local_direct_file: true,
            media_testing_enabled: false,
            // 근거는 위 필드 doc 주석 참고 (task-5 브리프의 반례: DISABLE_AUDIO 반전은 기본
            // true, SYNC_GROUP 은 브리프 표기와 달리 목표 수 정수, 나머지는 구 env 미설정
            // 동작 보존).
            media_d3d11_enabled: false,
            media_sync_group_target: 0,
            media_gapless_loop_enabled: false,
            media_direct_file_enabled: false,
            media_avdec_max_threads: -1,
            media_rtsp_latency_ms: -1,
            media_rtsp_wait_for_keyframe: false,
            media_audio_enabled: true,
            media_video_decoder_policy: String::new(),
            media_video_sink_policy: String::new(),
            media_video_sink_qos: String::new(),
            media_video_sink_pacing: String::new(),
            media_pipeline_mode: String::new(),
            media_webrtc_jitter_latency_ms: 0,
            network_connection_timeout: 15,
            network_enforce_mixed_content: true,
            network_enforce_tls_enabled: false,
            network_enforce_tls_localhost: false,
            network_enforce_tls_onion: false,
            network_http_cache_disabled: false,
            network_http_proxy_uri: String::new(),
            network_https_proxy_uri: String::new(),
            network_http_no_proxy: String::new(),
            network_http_cache_size: 5000,
            network_local_directory_listing_enabled: true,
            network_use_webpki_roots: false,
            session_history_max_length: 20,
            shell_background_color_rgba: [1.0, 1.0, 1.0, 1.0],
            log_filter: String::new(),
            thread_pool_workers_max: 4,
            thread_pool_async_runtime_workers_max: 6,
            thread_pool_fallback_workers: 3,
            thread_pool_webrender_workers_max: 4,
            webgl_testing_context_creation_error: false,
            user_agent: String::new(),
            viewport_meta_enabled: false,
        }
    }

    /// The amount of time that a half cycle of a text caret blink takes. If blinking is disabled
    /// this returns `None`.
    pub fn editing_caret_blink_time(&self) -> Option<Duration> {
        if self.editing_caret_blink_time > 0 {
            Some(Duration::from_millis(self.editing_caret_blink_time as u64))
        } else {
            None
        }
    }

    /// `self`(현재 값)과 `other`(가령 기본값) 사이에서 서로 다른 pref 만 (이름, 현재값,
    /// 비교값) 3튜플로 돌려준다. 매크로가 만드는 `diff()`는 (이름, 현재값) 2튜플뿐이라 —
    /// 기동 덤프(`config_dump::log_effective_config`)는 "기본값이 무엇이었는지"도 같이
    /// 찍어야 하므로, `diff()`가 고른 이름들에 대해 `other.get_value()`로 비교값을 채운다.
    pub fn diff_from(&self, other: &Self) -> Vec<(&'static str, PrefValue, PrefValue)> {
        self.diff(other)
            .into_iter()
            .map(|(name, current_value)| {
                let other_value = other.get_value(name);
                (name, current_value, other_value)
            })
            .collect()
    }
}

impl Default for Preferences {
    fn default() -> Self {
        let mut preferences = Self::const_default();
        preferences.user_agent = UserAgentPlatform::default().to_user_agent_string();
        if let Ok(proxy_uri) = std::env::var("http_proxy").or_else(|_| std::env::var("HTTP_PROXY"))
        {
            preferences.network_http_proxy_uri = proxy_uri;
        }
        if let Ok(proxy_uri) =
            std::env::var("https_proxy").or_else(|_| std::env::var("HTTPS_PROXY"))
        {
            preferences.network_https_proxy_uri = proxy_uri;
        }
        if let Ok(no_proxy) = std::env::var("no_proxy").or_else(|_| std::env::var("NO_PROXY")) {
            preferences.network_http_no_proxy = no_proxy
        }

        preferences
    }
}

pub enum UserAgentPlatform {
    Desktop,
    Android,
    OpenHarmony,
    Ios,
}

impl UserAgentPlatform {
    /// Return the default `UserAgentPlatform` for this platform. This is
    /// not an implementation of `Default` so that it can be `const`.
    pub const fn default() -> Self {
        if cfg!(target_os = "android") {
            Self::Android
        } else if cfg!(target_env = "ohos") {
            Self::OpenHarmony
        } else if cfg!(target_os = "ios") {
            Self::Ios
        } else {
            Self::Desktop
        }
    }
}

impl UserAgentPlatform {
    /// Convert this [`UserAgentPlatform`] into its corresponding `String` value, ie the
    /// default user-agent to use for this platform.
    pub fn to_user_agent_string(&self) -> String {
        const SERVO_VERSION: &str = env!("CARGO_PKG_VERSION");
        match self {
            UserAgentPlatform::Desktop
                if cfg!(all(target_os = "windows", target_arch = "x86_64")) =>
            {
                format!(
                    "Mozilla/5.0 (Windows NT 10.0; Win64; {ARCH}rv:140.0) Servo/{SERVO_VERSION} Firefox/140.0"
                )
            },
            UserAgentPlatform::Desktop if cfg!(target_os = "macos") => {
                format!(
                    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:140.0) Servo/{SERVO_VERSION} Firefox/140.0"
                )
            },
            UserAgentPlatform::Desktop => {
                format!(
                    "Mozilla/5.0 (X11; Linux {ARCH}; rv:140.0) Servo/{SERVO_VERSION} Firefox/140.0"
                )
            },
            UserAgentPlatform::Android => {
                format!(
                    "Mozilla/5.0 (Android 10; Mobile; rv:140.0) Servo/{SERVO_VERSION} Firefox/140.0"
                )
            },
            UserAgentPlatform::OpenHarmony => format!(
                "Mozilla/5.0 (OpenHarmony; Mobile; rv:140.0) Servo/{SERVO_VERSION} Firefox/140.0"
            ),
            UserAgentPlatform::Ios => format!(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_6 like Mac OS X; rv:140.0) Servo/{SERVO_VERSION} Firefox/140.0"
            ),
        }
    }
}
