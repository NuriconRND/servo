/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use embedder_traits::{EventLoopWaker, RefreshDriver};
use paint_api::display_list::{
    PaintAnimation, PaintAnimationEasing, PaintAnimationProperty, PaintAnimationSegment,
};
use rustc_hash::FxHashMap;
use servo_base::cross_process_instant::CrossProcessInstant;
use servo_base::id::WebViewId;
use servo_config::prefs;
use webrender_api::units::LayoutTransform;
use webrender_api::{ColorF, PropertyBindingKey, PropertyValue};

use crate::paint_animation_mesh_probe;
use crate::refresh_driver::TimerRefreshDriver;
use crate::webview_renderer::WebViewRenderer;

/// The amount of the time the caret blinks before ceasing, in order to preserve power. User
/// activity (a new display list) will reset this.
///
/// TODO: This should be controlled by system settings.
pub(crate) const CARET_BLINK_TIMEOUT: Duration = Duration::from_secs(30);

/// 디스플레이 리스트가 만들어진 뒤 여기 도착하기까지 걸렸다고 인정하는 최대 시간.
///
/// ★신뢰할 수 없는 값으로 애니메이션을 과거로 끌고 가지 않기 위한 상한이다.★ 정상 경로에서
/// 이 시간은 밀리초 단위다(리플로 잔여 + 전송 + 씬 빌드). 그보다 훨씬 큰 값이 나온다면 그
/// 디스플레이 리스트가 어딘가에서 오래 묵었거나 시계가 어긋난 것이고, 그때 그 값을 그대로
/// 빼면 애니메이션이 이미 끝난 지점으로 순간이동한다. 상한에 걸리면 그만큼만 되돌리므로
/// 최악이라도 예전 동작(도착 시각 기준)보다 나쁘지 않다.
const MAX_DISPLAY_LIST_TRANSIT: Duration = Duration::from_millis(250);

/// A struct responsible for managing paint-side animations. Currently this only handles text caret
/// blinking, but the idea is that in the future this would handle other types of paint-side
/// animations as well.
///
/// Note: This does not control animations requiring layout (all CSS transitions and animations
/// currently) nor animations due to touch events such as fling.
pub(crate) struct WebContentAnimator {
    event_loop_waker: Box<dyn EventLoopWaker>,
    timer_refresh_driver: Rc<TimerRefreshDriver>,
    /// ★애니메이션 값 표본이 타는 클럭.★ `gfx_vsync_enabled` 로 vsync 드라이버가
    /// 설치돼 있으면 그것이고, 아니면 위 타이머다(그 경우 `observe_next_frame` 이
    /// `queue_timer(paint_timer_period(), ..)` 이라 동작이 종전과 같다).
    ///
    /// ***왜 나눠 들고 있나*** -- 캐럿 깜빡임은 진짜 타이머(500ms)라 프레임 틱을
    /// 탈 수 없다. 표본만 표출과 같은 클럭에 얹는다.
    ///
    /// ★이것을 안 맞추면 vsync 로 묶는 것이 오히려 해롭다.★ 표출만 vsync 에 묶고
    /// 표본이 자유 구동 타이머에 남으면 둘이 매 프레임 맞물렸다 어긋나면서, 값을
    /// 뜬 시각과 화면에 나가는 시각의 간격이 프레임마다 달라진다 -- 자유 구동일 때
    /// 주기적으로 한 번 나던 딸꾹질이 **내내** 나는 떨림으로 바뀐다(실기 확인,
    /// log_ani_debug/30).
    frame_refresh_driver: Rc<dyn RefreshDriver>,
    caret_visible: Cell<bool>,
    timer_scheduled: Cell<bool>,
    need_update: Arc<AtomicBool>,
    /// Set while a wake for the next paint-animation frame is already queued, so a long
    /// animation queues one timer at a time instead of one per turn.
    paint_animation_wake_scheduled: Arc<AtomicBool>,
}

impl WebContentAnimator {
    pub(crate) fn new(
        event_loop_waker: Box<dyn EventLoopWaker>,
        timer_refresh_driver: Rc<TimerRefreshDriver>,
        frame_refresh_driver: Rc<dyn RefreshDriver>,
    ) -> Self {
        Self {
            event_loop_waker,
            timer_refresh_driver,
            frame_refresh_driver,
            caret_visible: Cell::new(true),
            timer_scheduled: Default::default(),
            need_update: Default::default(),
            paint_animation_wake_scheduled: Default::default(),
        }
    }

    /// Ask for another turn of the paint loop soon, so an animation nobody else is
    /// driving still advances.
    ///
    /// The wall usually has video pushing frames anyway, but a page whose only moving
    /// thing is one CSS animation has nothing else to wake the painter -- and the whole
    /// point of these animations is that they keep running when script has stopped
    /// feeding us.
    ///
    /// ★박자는 표출과 같은 클럭에서 온다.★ 여기가 값 표본을 뜨는 시점이므로, 이것이
    /// 표출과 다른 클럭에 있으면 둘이 매 프레임 어긋난다 -- `frame_refresh_driver` 주석.
    /// 주기 인자를 받지 않는 이유도 그것이다: 주기는 드라이버가 안다.
    pub(crate) fn wake_for_paint_animation(&self) {
        if self.paint_animation_wake_scheduled.load(Ordering::Relaxed) {
            return;
        }
        let event_loop_waker = self.event_loop_waker.clone();
        let scheduled = self.paint_animation_wake_scheduled.clone();
        self.frame_refresh_driver
            .observe_next_frame(Box::new(move || {
                scheduled.store(false, Ordering::Relaxed);
                event_loop_waker.wake();
            }));
        self.paint_animation_wake_scheduled
            .store(true, Ordering::Relaxed);
    }

    pub(crate) fn schedule_timer_if_necessary(&self) {
        if self.timer_scheduled.get() {
            return;
        }

        let Some(caret_blink_time) = prefs::get().editing_caret_blink_time() else {
            return;
        };

        let event_loop_waker = self.event_loop_waker.clone();
        let need_update = self.need_update.clone();
        self.timer_refresh_driver.queue_timer(
            caret_blink_time,
            Box::new(move || {
                need_update.store(true, Ordering::Relaxed);
                event_loop_waker.wake();
            }),
        );
        self.timer_scheduled.set(true);
    }

    pub(crate) fn update(
        &self,
        webview_renderers: &FxHashMap<WebViewId, WebViewRenderer>,
    ) -> Option<Vec<PropertyValue<ColorF>>> {
        if !self.need_update.load(Ordering::Relaxed) {
            return None;
        }

        let mut colors = Vec::new();
        for renderer in webview_renderers.values() {
            renderer.for_each_connected_pipeline(&mut |pipeline_details| {
                if let Some(property_value) =
                    pipeline_details.animations.update(self.caret_visible.get())
                {
                    colors.push(property_value);
                }
            });
        }

        self.timer_scheduled.set(false);
        self.need_update.store(false, Ordering::Relaxed);

        if colors.is_empty() {
            // All animations have stopped. When a new blinking caret is activated we want
            // it to start in the visible state, so we set `caret_visible` to true here.
            self.caret_visible.set(true);
            return None;
        }

        self.caret_visible.set(!self.caret_visible.get());
        self.schedule_timer_if_necessary();
        Some(colors)
    }
}

/// This structure tracks the animations active for a given pipeline. Currently only caret
/// blinking is tracked, but in the future this could perhaps track paint-side animations.
#[derive(Default)]
pub(crate) struct PipelineAnimations {
    caret: RefCell<Option<CaretAnimation>>,
    /// What layout last told us to keep playing, and the clock it is played against.
    /// Replaced wholesale by every display list -- see [`PaintAnimation`].
    paint: RefCell<Vec<ActivePaintAnimation>>,
    /// The last value of every paint animation that has stopped playing, kept so that a
    /// later transaction does not erase it. See [`HeldPaintValues`].
    held: RefCell<HeldPaintValues>,

    /// 맞물림 프로브 전용 재사용 버퍼. ★프로브가 꺼져 있으면 비어 있고 할당도 없다★ --
    /// 켜진 뒤에는 용량이 유지되므로 설치마다 할당하지 않는다.
    mesh_buffers: RefCell<MeshBuffers>,
}

/// ***전송 한 번이 안 실린 키를 전부 지운다.***
///
/// `Transaction::reset_dynamic_properties` 는 WebRender 의 `SceneProperties` 에서
/// transform/float/color 맵 셋을 통째로 비우고 **그 트랜잭션에 실린 것만** 다시 채운다
/// (`webrender/src/scene.rs`, `flush_pending_updates`). 그리고 맵에 없는 키는
/// `resolve_float`/`resolve_layout_transform` 이 바인딩에 구워진 기본값으로 되돌린다 --
/// 그 기본값은 **디스플레이 리스트를 만든 순간의 값**이다.
///
/// 그래서 먼저 끝난 애니메이션의 키는, 아직 도는 다른 애니메이션이 값을 밀 때마다 화면에서
/// 시작값으로 되돌아간다. 페이드인이면 투명 -- 즉 검은 화면이다. 되돌아간 값은 다음 디스플레이
/// 리스트가 올 때까지 그대로 있으므로, 끊긴 길이는 프레임 한 장이 아니라 **초 단위**가 된다.
///
/// ***고칠 자리는 "더 자주 보내는 쪽"이 아니라 "지우는 쪽"이다.*** 끝난 값을 매 주기 다시
/// 밀면 페인터마다 초당 한 주기치 프레임이 더 나가고(`ActivePaintAnimation::running` 주석의
/// 실측), 그건 비디오 표출을 다시 밀어내는 길이다. 대신 마지막 값을 여기 붙들어 두었다가
/// **이미 나가기로 정해진 전송에만 얹는다** -- 추가 전송도, 추가 프레임도 없다.
/// [`PipelineAnimations::install_paint_animations`] 가 맞물림을 잴 때 쓰는 버퍼 넷.
///
/// ★설치 경로에서 할당하지 않기 위해 들고 있는다.★ `clear` 는 용량을 남긴다.
#[derive(Default)]
struct MeshBuffers {
    before_floats: Vec<PropertyValue<f32>>,
    before_transforms: Vec<PropertyValue<LayoutTransform>>,
    after_floats: Vec<PropertyValue<f32>>,
    after_transforms: Vec<PropertyValue<LayoutTransform>>,
}

#[derive(Default)]
struct HeldPaintValues {
    floats: Vec<PropertyValue<f32>>,
    transforms: Vec<PropertyValue<LayoutTransform>>,
}

/// 한 파이프라인이 붙들 수 있는 값의 상한. 설계의 일부가 아니라 누수 방지턱이다: 키는
/// 디스플레이 리스트가 다시 선언해 줄 때 비로소 정리되므로(`prune_held`), 다시는 돌아오지
/// 않는 요소의 키가 쌓일 수 있다. 애니메이션되는 요소가 수십 개인 화면에서는 닿지 않는다.
const MAX_HELD_PAINT_VALUES: usize = 512;

impl HeldPaintValues {
    fn hold_float(&mut self, value: PropertyValue<f32>) {
        match self
            .floats
            .iter_mut()
            .find(|held| held.key.id == value.key.id)
        {
            Some(slot) => *slot = value,
            None => {
                if self.floats.len() >= MAX_HELD_PAINT_VALUES {
                    self.floats.remove(0);
                }
                self.floats.push(value);
            },
        }
    }

    fn hold_transform(&mut self, value: PropertyValue<LayoutTransform>) {
        match self
            .transforms
            .iter_mut()
            .find(|held| held.key.id == value.key.id)
        {
            Some(slot) => *slot = value,
            None => {
                if self.transforms.len() >= MAX_HELD_PAINT_VALUES {
                    self.transforms.remove(0);
                }
                self.transforms.push(value);
            },
        }
    }

    fn is_empty(&self) -> bool {
        self.floats.is_empty() && self.transforms.is_empty()
    }
}

/// A [`PaintAnimation`] anchored to this painter's clock.
struct ActivePaintAnimation {
    property: PaintAnimationProperty,
    /// The instant the animation's own timeline reads zero. Usually in the past.
    zero: Instant,
    /// Whether the segments run to the animation's end, as opposed to stopping at the
    /// sampling horizon. Not used to decide whether to keep playing -- see `running` --
    /// but it is the difference between "finished" and "outran its runway", which is worth
    /// keeping straight when reading this back.
    complete: bool,
}

/// Interpolate one segment. `Copy + Lerp` would be nicer than a trait per type, but two
/// concrete types do not earn a trait.
fn segment_progress<T>(segment: &PaintAnimationSegment<T>, elapsed: f64) -> f64 {
    let span = segment.end - segment.start;
    let raw = if span > 0.0 {
        (elapsed - segment.start) / span
    } else {
        1.0
    };
    segment.easing.ease(raw)
}

/// Pick the segment covering `elapsed`, clamping to the first or last one outside the
/// range. Segments are few (one for a transition, a handful for keyframes), so a scan
/// beats anything cleverer.
fn find_segment<T>(
    segments: &[PaintAnimationSegment<T>],
    elapsed: f64,
) -> Option<&PaintAnimationSegment<T>> {
    if segments.is_empty() {
        return None;
    }
    if elapsed <= segments[0].start {
        return segments.first();
    }
    segments
        .iter()
        .find(|segment| elapsed < segment.end)
        .or_else(|| segments.last())
}

impl ActivePaintAnimation {
    /// Whether this animation still has anything left to say at `now`.
    ///
    /// ***Past the end of what was sampled there is nothing left to say, finished or
    /// not.*** An animation that outran its horizon is not over -- the rest of it lives in
    /// script -- but the value cannot move again until the next display list arrives, and
    /// WebRender keeps the last value it was given. Re-sending it every frame would burn
    /// a frame per painter per tick to redraw an unchanged picture; measured on the 4-GPU
    /// wall, 2026-09-08, that was 30 frames a second per painter of pure waste.
    ///
    /// It also makes `playing=` in the log mean "actually moving", which is the question
    /// being asked of it.
    fn running(&self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.zero).as_secs_f64();
        match &self.property {
            PaintAnimationProperty::Opacity(_, segments) => {
                segments.last().is_some_and(|last| elapsed < last.end)
            },
            PaintAnimationProperty::Transform(_, segments) => {
                segments.last().is_some_and(|last| elapsed < last.end)
            },
        }
    }

    /// The instant this animation's own timeline runs out, if it has segments at all.
    /// 창 `[from, to]` 안에서 이 애니메이션이 요구하는 샘플 시각들을 `out` 에 더한다.
    ///
    /// 창의 양 끝(이 애니메이션의 구간으로 clamp)과, 창 안에 들어오는 ***세그먼트 경계***.
    /// 근거는 `PipelineAnimations::paint_animation_sample_times` 의 doc 주석에 있다.
    fn sample_times(&self, from: Instant, to: Instant, step: Duration, out: &mut Vec<Instant>) {
        let push = |out: &mut Vec<Instant>, t: Instant| {
            if !out.contains(&t) {
                out.push(t);
            }
        };
        // 창 양 끝. 끝난 뒤는 움직이지 않으므로 `ends_at` 로 clamp 한다.
        let cap = self.ends_at();
        let clamp = |t: Instant| match cap {
            Some(end) if t > end => end,
            _ => t,
        };
        push(out, clamp(from));
        push(out, clamp(to));

        // 세그먼트 경계와, ★비선형 이징이면 그 내부의 균일 샘플★. 근거는
        // `PipelineAnimations::paint_animation_sample_times` 의 doc 주석 3 번에 있다.
        let spans: Vec<(f64, f64, bool)> = match &self.property {
            PaintAnimationProperty::Opacity(_, segments) => segments
                .iter()
                .map(|segment| {
                    (
                        segment.start,
                        segment.end,
                        !matches!(segment.easing, PaintAnimationEasing::Linear),
                    )
                })
                .collect(),
            PaintAnimationProperty::Transform(_, segments) => segments
                .iter()
                .map(|segment| {
                    (
                        segment.start,
                        segment.end,
                        !matches!(segment.easing, PaintAnimationEasing::Linear),
                    )
                })
                .collect(),
        };
        let at_offset = |offset: f64| self.zero.checked_add(Duration::from_secs_f64(offset));
        for (start, end, eased) in spans {
            if let Some(at) = at_offset(end) {
                if at > from && at < to {
                    push(out, at);
                }
            }
            if !eased || step.is_zero() {
                continue;
            }
            // 이 세그먼트가 창과 겹치는 부분만 훑는다. 간격은 한 표출 주기다.
            let Some(seg_from) = at_offset(start) else {
                continue;
            };
            let Some(seg_to) = at_offset(end) else {
                continue;
            };
            let mut at = seg_from.max(from);
            let upper = seg_to.min(to);
            while at < upper {
                push(out, at);
                at += step;
            }
        }
    }

    /// 이 애니메이션의 전체 길이(초). 마지막 세그먼트의 `end` 다.
    fn total_span(&self) -> Option<f64> {
        let end = match &self.property {
            PaintAnimationProperty::Opacity(_, segments) => segments.last()?.end,
            PaintAnimationProperty::Transform(_, segments) => segments.last()?.end,
        };
        (end.is_finite() && end > 0.0).then_some(end)
    }

    fn ends_at(&self) -> Option<Instant> {
        let end = match &self.property {
            PaintAnimationProperty::Opacity(_, segments) => segments.last()?.end,
            PaintAnimationProperty::Transform(_, segments) => segments.last()?.end,
        };
        if !end.is_finite() || end < 0.0 {
            return None;
        }
        self.zero.checked_add(Duration::from_secs_f64(end))
    }

    /// Sample the value this animation stops at.
    ///
    /// Sampled at exactly `ends_at` rather than at `now`: past the end `segment_progress`
    /// would hand the easing a ratio above one, and an easing curve is only defined on
    /// [0, 1] -- a cubic-bezier extrapolates, so "the value it finished on" would come out
    /// as some value it never had.
    fn sample_final(&self, held: &mut HeldPaintValues) {
        let Some(end) = self.ends_at() else {
            return;
        };
        let mut floats = Vec::new();
        let mut transforms = Vec::new();
        self.sample(end, &mut floats, &mut transforms);
        for value in floats {
            held.hold_float(value);
        }
        for value in transforms {
            held.hold_transform(value);
        }
    }

    fn sample(
        &self,
        now: Instant,
        floats: &mut Vec<PropertyValue<f32>>,
        transforms: &mut Vec<PropertyValue<LayoutTransform>>,
    ) {
        let elapsed = now.saturating_duration_since(self.zero).as_secs_f64();
        match &self.property {
            PaintAnimationProperty::Opacity(key, segments) => {
                let Some(segment) = find_segment(segments, elapsed) else {
                    return;
                };
                let progress = segment_progress(segment, elapsed) as f32;
                floats.push(PropertyValue {
                    key: *key,
                    value: segment.from + (segment.to - segment.from) * progress,
                });
            },
            PaintAnimationProperty::Transform(key, segments) => {
                let Some(segment) = find_segment(segments, elapsed) else {
                    return;
                };
                let progress = segment_progress(segment, elapsed) as f32;
                // Componentwise on the matrix. Layout subdivided anything whose CSS
                // interpolation is not linear in the matrix, so within one segment this
                // agrees with the styled value to well under a pixel.
                let from = segment.from.to_array();
                let to = segment.to.to_array();
                let mut lerped = [0.0f32; 16];
                for (index, slot) in lerped.iter_mut().enumerate() {
                    *slot = from[index] + (to[index] - from[index]) * progress;
                }
                transforms.push(PropertyValue {
                    key: *key,
                    value: LayoutTransform::from_array(lerped),
                });
            },
        }
    }
}

/// 세그먼트의 시간 원점.
///
/// ★원점은 이 리스트가 **만들어진** 시각이지 **받은** 시각이 아니다.★
///
/// 세그먼트는 레이아웃이 샘플링을 시작한 순간을 0 으로 갖고, `offset_from_display_list` 도
/// 그 순간에서 잰 값이다. 받은 시각에 0 을 박으면 그 사이에 든 시간(리플로 잔여 + 전송 +
/// 씬 빌드)만큼 예측 전체가 과거로 밀린다. 그 시간이 일정하다면 일정한 지연일 뿐이지만
/// 일정하지 않다 -- 같은 벽에서 한가할 때 0.4ms, 콘텐츠 전환 중 2.0ms 다
/// (log_ani_debug/08 `SCRIPTBUSY`). 만들어진 시각에 박으면 연달아 오는 예측들이 서로
/// 정확히 맞물린다.
fn animation_zero(
    built_at: CrossProcessInstant,
    now: CrossProcessInstant,
    received_at: Instant,
    offset_from_display_list: f64,
) -> Instant {
    let raw = (now - built_at).as_seconds_f64();
    let limit = MAX_DISPLAY_LIST_TRANSIT.as_secs_f64();
    let transit = raw.clamp(0.0, limit);
    let estimate = received_at
        .checked_sub(Duration::from_secs_f64(transit))
        .unwrap_or(received_at);

    // ★추정 대신 실제 틱 시각을 쓴다.★ 2026-11-13.
    //
    // `built_at` 은 페이싱 틱 시각이고(`PaintDisplayListInfo::built_at` 의 doc), 위의
    // `estimate` 는 그것을 두 시계의 차로 ***추정***한 것이다. 그 차는 "scene build 의
    // sub-millisecond transit" 이 아니라 ★틱부터 도착까지의 전체 지연이고 벽에서 5~30ms★
    // 다. 그래서 환산 오차가 DL 마다 다르게 들어가고, DL 이 WebGL 때문에 초당 36 회 오면
    // (`DLARRIVE avg 28ms`) 애니메이션의 기준점이 ***초당 36 회 흔들린다.*** 생산도 렌더도
    // 60Hz 인데(`PRODUCE window edges=60.0/s calls=60.0/s`) 움직임이 튀는 기구가 이것이다.
    //
    // 틱 시각은 이미 로컬 시계로 지나가므로(`wall_tick::note_tick_edge`), 추정값에 가장
    // 가까운 기록을 쓰면 오차가 사라지고 기준점이 16.67ms 격자에 앉는다.
    //
    // ★한 프레임보다 멀면 쓰지 않는다★ -- 그만큼 떨어진 기록은 다른 틱이고, 링에 그 틱이
    // 없다는 뜻이다(기록이 아직 없는 첫 DL, 또는 1 초보다 묵은 DL). 그 경우 앞 판과 똑같이
    // 추정값을 쓴다.
    const SNAP_LIMIT: Duration = Duration::from_micros(8_333);
    let snapped = if crate::wall_tick::animation_zero_snaps_to_tick() {
        crate::wall_tick::nearest_tick(estimate).filter(|(_, dist)| *dist <= SNAP_LIMIT)
    } else {
        None
    };
    note_animation_zero(raw, limit, snapped.map(|(_, dist)| dist));
    let created_at = snapped.map_or(estimate, |(tick, _)| tick);

    let offset = Duration::from_secs_f64(offset_from_display_list.abs());
    if offset_from_display_list <= 0.0 {
        created_at.checked_sub(offset).unwrap_or(created_at)
    } else {
        created_at + offset
    }
}

/// `animation_zero` 한 번을 센다. 10 초마다 `frame_source_probe` 가 `ANIMZERO` 로 찍는다.
///
/// ## 무엇을 가르나 (2026-11-13)
///
/// * `transit` -- 틱부터 DL 도착까지의 전체 지연. 주석이 약속한 5~30ms 인지 확인한다.
/// * `clamped` -- `MAX_DISPLAY_LIST_TRANSIT`(250ms)에 걸린 횟수. 걸리면 기준점이 그만큼
///   과거로 잘려 애니메이션이 뒤로 끌린다. `display_list.rs:1236` 이 `TRANSITCLAMP` 를
///   찍겠다고 적어 뒀지만 ***구현되어 있지 않았다.***
/// * `snap` -- 추정값과 실제 틱 시각의 거리. ★이것이 저더의 크기다★ -- 0 이면 추정이
///   정확했다는 뜻이고, 수 ms 면 기준점이 그만큼 흔들리고 있었다는 뜻이다.
/// * `miss` -- 스냅할 틱을 못 찾은 횟수(한 프레임보다 멀다). 크면 링이 짧거나 DL 이 묵었다.
///
/// ## 비용
///
/// DL 마다 한 번(초당 36 회)이고 원자 일곱이다. 타이밍 경로가 아니다.
fn note_animation_zero(raw_transit: f64, limit: f64, snap: Option<Duration>) {
    let us = |v: f64| (v * 1_000_000.0).clamp(0.0, u64::MAX as f64) as u64;
    ANIM_ZERO_CALLS.fetch_add(1, Ordering::Relaxed);
    let transit_us = us(raw_transit.max(0.0));
    ANIM_ZERO_TRANSIT_SUM.fetch_add(transit_us, Ordering::Relaxed);
    ANIM_ZERO_TRANSIT_MAX.fetch_max(transit_us, Ordering::Relaxed);
    if raw_transit > limit {
        ANIM_ZERO_CLAMPED.fetch_add(1, Ordering::Relaxed);
    }
    match snap {
        Some(dist) => {
            let dist_us = dist.as_micros() as u64;
            ANIM_ZERO_SNAP_SUM.fetch_add(dist_us, Ordering::Relaxed);
            ANIM_ZERO_SNAP_MAX.fetch_max(dist_us, Ordering::Relaxed);
        },
        None => {
            ANIM_ZERO_MISS.fetch_add(1, Ordering::Relaxed);
        },
    }
}

static ANIM_ZERO_CALLS: AtomicU64 = AtomicU64::new(0);
static ANIM_ZERO_TRANSIT_SUM: AtomicU64 = AtomicU64::new(0);
static ANIM_ZERO_TRANSIT_MAX: AtomicU64 = AtomicU64::new(0);
static ANIM_ZERO_CLAMPED: AtomicU64 = AtomicU64::new(0);
static ANIM_ZERO_SNAP_SUM: AtomicU64 = AtomicU64::new(0);
static ANIM_ZERO_SNAP_MAX: AtomicU64 = AtomicU64::new(0);
static ANIM_ZERO_MISS: AtomicU64 = AtomicU64::new(0);

/// `ANIMZERO` 한 줄을 만든다. 호출마다 구간 값을 비운다.
pub(crate) fn animation_zero_report(secs: f64) -> Option<String> {
    let calls = ANIM_ZERO_CALLS.swap(0, Ordering::Relaxed);
    if calls == 0 {
        return None;
    }
    let snapped = calls - ANIM_ZERO_MISS.load(Ordering::Relaxed).min(calls);
    let miss = ANIM_ZERO_MISS.swap(0, Ordering::Relaxed);
    let snap_sum = ANIM_ZERO_SNAP_SUM.swap(0, Ordering::Relaxed);
    Some(format!(
        "ANIMZERO t={secs:.0}s calls={calls} ({:.1}/s)          transit(avg={}us max={}us) clamped={}          snap(n={snapped} avg={}us max={}us) miss={miss}",
        calls as f64 / secs.max(1e-9),
        ANIM_ZERO_TRANSIT_SUM.swap(0, Ordering::Relaxed) / calls,
        ANIM_ZERO_TRANSIT_MAX.swap(0, Ordering::Relaxed),
        ANIM_ZERO_CLAMPED.swap(0, Ordering::Relaxed),
        if snapped > 0 { snap_sum / snapped } else { 0 },
        ANIM_ZERO_SNAP_MAX.swap(0, Ordering::Relaxed),
    ))
}

impl PipelineAnimations {
    pub(crate) fn update(&self, caret_visible: bool) -> Option<PropertyValue<ColorF>> {
        let mut maybe_caret = self.caret.borrow_mut();
        let caret = maybe_caret.as_mut()?;

        if let Some(update) = caret.update(caret_visible) {
            return Some(update);
        }
        *maybe_caret = None;
        None
    }

    /// Sample every animation this pipeline is playing, and say whether any is still
    /// going. Values are appended, never replaced -- the caller collects across pipelines
    /// and sends one transaction.
    pub(crate) fn update_paint_animations(
        &self,
        now: Instant,
        floats: &mut Vec<PropertyValue<f32>>,
        transforms: &mut Vec<PropertyValue<LayoutTransform>>,
    ) -> bool {
        let mut animations = self.paint.borrow_mut();
        let mut held = self.held.borrow_mut();
        // A finished animation stops being sampled, but its last value is held rather than
        // forgotten -- see [`HeldPaintValues`] for why forgetting it blanks the element.
        animations.retain(|animation| {
            if animation.running(now) {
                return true;
            }
            animation.sample_final(&mut held);
            false
        });
        for animation in animations.iter() {
            animation.sample(now, floats, transforms);
        }
        !animations.is_empty()
    }

    /// 창 `[from, to]` 안에서 ★샘플해야 할 시각들★. 상태를 건드리지 않는다.
    ///
    /// 샘플 집합은 셋을 합친 것이다.
    ///
    /// 1. ★창의 양 끝★(이 애니메이션의 구간으로 clamp).
    /// 2. ★창 안에 들어오는 세그먼트 경계★ -- 전환점은 항상 경계이므로, 바운스가 창 안에서
    ///    왕복해도 그 꼭대기가 샘플에 들어온다. 두 끝점의 차이만 보던 1 차 구현이 바로
    ///    그것을 놓쳤다(왕복하면 차이가 0 에 가깝다).
    /// 3. ★비선형 이징 세그먼트의 내부 균일 샘플★ -- 아래가 그 이유다.
    ///
    /// ## ★3 번이 왜 필요한가★ (2026-11-11)
    ///
    /// 처음에는 1·2 만으로 "닫힌 형태" 라고 적었다. 근거는 "layout 이 비선형 이징을 행렬에서
    /// 선형인 세그먼트로 미리 쪼개 보낸다" 였는데, ***그것이 틀렸다.***
    /// `PaintAnimationEasing` 은 `Linear` 외에 `CubicBezier` 와 `Steps` 를 그대로 싣고
    /// (`display_list.rs:850-856`), `segment_progress` 가 `segment.easing.ease(raw)` 로
    /// ***세그먼트 안에서 비선형***으로 보간한다. 그러면 극값이 구간 끝에만 있다는 보장이
    /// 없다.
    ///
    /// 특히 IPWall 의 `bounce` 는 첫 구간이 `ease-out` 이라 ***시작이 가장 빠르다***. 경계
    /// 둘만 샘플하면 그 빠른 구간이 뭉개지고, 그것이 운영자가 본 "전환 애니메이션 시작시
    /// 순간 검은 영역 넓게"(log_webgl_perf_01/35)다. 재생 중 띠가 `scene` 에서 사라진 것도
    /// 같은 이유로 설명된다 -- 중간 구간은 이징이 완만해서 경계만으로 충분했다.
    ///
    /// 간격은 ★한 표출 주기★ 다. 창이 100ms 안쪽이므로 세그먼트당 최대 몇 개이고, 그보다
    /// 촘촘히 해도 사각형이 더 커지지 않는다(한 주기 안의 이동은 scissor 한 줄 수준이다).
    ///
    /// 끝난 애니메이션의 시각은 `ends_at()` 로 clamp 한다 -- 그 뒤로는 움직이지 않으므로
    /// 창이 전환 끝을 넘어가도 구멍이 생기지 않는다.
    /// 지금 돌고 있는 paint 애니메이션의 ★진행률★ -- 여럿이면 가장 늦게 시작한 것.
    ///
    /// `0.0`~`1.0` 이 애니메이션 중이고 `1.0` 을 넘으면 끝난 뒤다. 돌고 있는 것이 없으면
    /// `None` 이다.
    ///
    /// ## 왜 이 값인가 (2026-11-12, log_webgl_perf_01/46)
    ///
    /// `LOCKTAIL`(캔버스 서피스 획득이 8ms 이상)이 ***전환에 묶여 몰린다***는 것은 확정됐다
    /// (유휴 대비 x3.0~3.2, 순환 이동 영점 p = 0.006~0.009, 세 런 모두). 그런데 ***어느
    /// 구간인지는 1 초 타임스탬프로 가를 수 없다*** -- 최적 적합 창이 [2.05, 2.80)s 로
    /// 2500ms 경계를 걸치고, "애니메이션 마지막 0.75 초" 와 "끝난 직후 0.75 초" 의 우도
    /// 차이가 0.06 이다.
    ///
    /// 운영자 관찰("바운스 후반부, 표출 영역 약간 밖에서 우->좌로 돌아올 때")이 걸려 있는
    /// 구분이 바로 그것이다. 시계를 더 잘게 쪼개는 대신 ★사건이 자기 위상을 들고 다니게★
    /// 하면 시계 맞추기가 필요 없다.
    ///
    /// 가장 늦게 시작한 것을 고르는 이유: 전환은 들어오는 레이어와 나가는 레이어에 각각
    /// 애니메이션을 걸고(실측 `ANIMSCRIPTSTART` 가 `[전환 시작]` 의 두 배), 지금 화면을
    /// 지배하는 것은 나중 것이다.
    pub(crate) fn paint_animation_phase(&self, now: Instant) -> Option<f32> {
        let mut best: Option<(Instant, f32)> = None;
        for animation in self.paint.borrow().iter() {
            let Some(span) = animation.total_span() else {
                continue;
            };
            if span <= 0.0 {
                continue;
            }
            let elapsed = now.saturating_duration_since(animation.zero).as_secs_f64();
            let phase = (elapsed / span) as f32;
            match best {
                Some((zero, _)) if zero >= animation.zero => {},
                _ => best = Some((animation.zero, phase)),
            }
        }
        best.map(|(_, phase)| phase)
    }

    pub(crate) fn paint_animation_sample_times(
        &self,
        from: Instant,
        to: Instant,
        step: Duration,
        out: &mut Vec<Instant>,
    ) {
        for animation in self.paint.borrow().iter() {
            animation.sample_times(from, to, step, out);
        }
    }

    /// 주어진 시각의 값을 ★상태를 건드리지 않고★ 샘플한다.
    ///
    /// `update_paint_animations` 와 다르다 -- 그쪽은 끝난 애니메이션을 `retain` 으로
    /// 걷어내고 `sample_final` 로 마지막 값을 붙든다. ***미래 시각으로 그것을 부르면
    /// 아직 돌고 있는 애니메이션을 끝난 것으로 치고 버린다.*** 그래서 예측에는 이 함수를
    /// 쓴다. `&self` 와 `borrow()` 만 쓰고 아무것도 바꾸지 않는다.
    ///
    /// 그 시각에 이미 끝난 애니메이션은 건너뛴다 -- 끝난 것은 더 움직이지 않으므로 예측
    /// 이동량에 기여하지 않는 것이 맞다.
    pub(crate) fn sample_paint_animations_at(
        &self,
        now: Instant,
        floats: &mut Vec<PropertyValue<f32>>,
        transforms: &mut Vec<PropertyValue<LayoutTransform>>,
    ) {
        for animation in self.paint.borrow().iter() {
            if animation.running(now) {
                animation.sample(now, floats, transforms);
            }
        }
    }

    /// Append the values of animations that have stopped, for keys nothing is currently
    /// animating, and say how many were appended.
    ///
    /// ★낡음(2026-09-11 기록, 2026-09-18 `ca080974d42`/`79d8875effe` 이후 무효)★ — 호출처는 `pump_paint_animation`(painter.rs:1510)이고 평소 그것은 `Painter::render` 끝에서 돈다.
    /// ***이 함수는 전송을 만들지 않는다.*** 이미 나가기로 정해진 트랜잭션에만 얹히도록
    /// 호출되며(`Painter::perform_updates`), 그래서 프레임 생산 판정 -- `has_values`,
    /// `push_due`, `animated_property_frame`, `still_animating` -- 어느 것에도 들어가지
    /// 않는다. 전송이 없으면 reset 도 없고, reset 이 없으면 붙들 이유도 없다.
    ///
    /// 지금 도는 애니메이션이 먼저다: 같은 키를 둘 다 갖고 있으면 살아 있는 쪽이 옳다.
    pub(crate) fn append_held_paint_values(
        &self,
        floats: &mut Vec<PropertyValue<f32>>,
        transforms: &mut Vec<PropertyValue<LayoutTransform>>,
    ) -> usize {
        let held = self.held.borrow();
        if held.is_empty() {
            return 0;
        }
        let mut appended = 0;
        for value in held.floats.iter() {
            if !floats.iter().any(|live| live.key.id == value.key.id) {
                floats.push(*value);
                appended += 1;
            }
        }
        for value in held.transforms.iter() {
            if !transforms.iter().any(|live| live.key.id == value.key.id) {
                transforms.push(*value);
                appended += 1;
            }
        }
        appended
    }

    /// Drop held values for keys the new display list drives again.
    ///
    /// Only those: a key the new list does not mention may still be referenced by the
    /// scene WebRender is rendering right now -- the new one is not built yet -- and
    /// dropping it there is the blank this whole mechanism exists to prevent. Keys that
    /// really are gone age out against [`MAX_HELD_PAINT_VALUES`].
    fn prune_held(&self, incoming: &[PaintAnimation]) {
        let mut held = self.held.borrow_mut();
        if held.is_empty() {
            return;
        }
        for animation in incoming {
            match &animation.property {
                PaintAnimationProperty::Opacity(key, _) => {
                    held.floats.retain(|value| value.key.id != key.id)
                },
                PaintAnimationProperty::Transform(key, _) => {
                    held.transforms.retain(|value| value.key.id != key.id)
                },
            }
        }
    }

    /// Whether this pipeline has anything the paint thread is playing on its own.
    ///
    /// Used to decide how often script still needs to be asked to tick the animation: see
    /// [`AnimationRefreshDriverObserver::frame_started`].
    pub(crate) fn has_paint_animations(&self) -> bool {
        !self.paint.borrow().is_empty()
    }

    /// Replace what this pipeline is playing with what the new display list says.
    ///
    /// ***Wholesale replacement is the correctness argument.*** Layout samples forward
    /// from the moment it built the list, so as long as script keeps up, every frame's
    /// prediction is overwritten by the truth before it can drift. Only when script stops
    /// producing display lists does the prediction actually get used for long, and that is
    /// exactly when it is worth having.
    pub(crate) fn install_paint_animations(&self, paint_animations: Vec<PaintAnimation>) {
        let received_at = Instant::now();
        let now = CrossProcessInstant::now();
        // ★교체 **전**의 예측이 이 순간 내는 값을 먼저 뜬다.★ 프로브가 꺼져 있으면
        // 이 블록 전체가 돌지 않는다.
        let probing = paint_animation_mesh_probe::is_enabled();
        if probing {
            let mut buffers = self.mesh_buffers.borrow_mut();
            buffers.before_floats.clear();
            buffers.before_transforms.clear();
            let MeshBuffers {
                before_floats,
                before_transforms,
                ..
            } = &mut *buffers;
            for animation in self.paint.borrow().iter() {
                animation.sample(received_at, before_floats, before_transforms);
            }
        }
        self.prune_held(&paint_animations);
        *self.paint.borrow_mut() = paint_animations
            .into_iter()
            .map(|animation| {
                let zero = animation_zero(
                    animation.built_at,
                    now,
                    received_at,
                    animation.offset_from_display_list,
                );
                ActivePaintAnimation {
                    property: animation.property,
                    zero,
                    complete: animation.complete,
                }
            })
            .collect();
        if probing {
            self.measure_mesh(received_at);
        }
    }

    /// 교체 전후의 예측이 같은 순간에 내는 값을 비교해서 프로브에 쌓는다.
    ///
    /// ★교체 **후**에 불린다★ -- `self.paint` 는 이미 새 예측이고, 교체 전 값은
    /// `mesh_buffers.before_*` 에 떠 두었다. 키가 양쪽에 다 있는 것만 비교할 수 있고,
    /// 새로 나타난 키는 비교 대상이 없으므로 따로 센다.
    fn measure_mesh(&self, received_at: Instant) {
        let mut buffers = self.mesh_buffers.borrow_mut();
        buffers.after_floats.clear();
        buffers.after_transforms.clear();
        let MeshBuffers {
            before_floats,
            before_transforms,
            after_floats,
            after_transforms,
        } = &mut *buffers;
        for animation in self.paint.borrow().iter() {
            animation.sample(received_at, after_floats, after_transforms);
        }

        let mut carried = 0u64;
        let mut fresh = 0u64;
        for after in after_floats.iter() {
            match before_floats.iter().find(|b| b.key.id == after.key.id) {
                Some(before) => {
                    carried += 1;
                    paint_animation_mesh_probe::note_mesh_error((after.value - before.value).abs());
                },
                None => fresh += 1,
            }
        }
        for after in after_transforms.iter() {
            match before_transforms.iter().find(|b| b.key.id == after.key.id) {
                Some(before) => {
                    carried += 1;
                    // ★행렬은 성분별 최대 차로 잰다.★ 한 성분이라도 어긋나면 화면이 어긋난다.
                    let a = after.value.to_array();
                    let b = before.value.to_array();
                    let worst = (0..16)
                        .map(|index| (a[index] - b[index]).abs())
                        .fold(0.0f32, f32::max);
                    paint_animation_mesh_probe::note_mesh_error(worst);
                },
                None => fresh += 1,
            }
        }
        paint_animation_mesh_probe::note_install(carried, fresh);
    }

    pub(crate) fn handle_new_display_list(
        &self,
        caret_property_binding: Option<(PropertyBindingKey<ColorF>, ColorF)>,
        web_content_animator: &WebContentAnimator,
    ) {
        let Some(caret_blink_time) = prefs::get().editing_caret_blink_time() else {
            return;
        };

        *self.caret.borrow_mut() = match caret_property_binding {
            Some((caret_property_key, original_caret_color)) => {
                web_content_animator.schedule_timer_if_necessary();
                Some(CaretAnimation {
                    caret_property_key,
                    original_caret_color,
                    remaining_blink_count: (CARET_BLINK_TIMEOUT.as_millis()
                        / caret_blink_time.as_millis())
                        as usize,
                })
            },
            None => None,
        }
    }
}

/// Tracks the state of an ongoing caret blinking animation.
struct CaretAnimation {
    pub caret_property_key: PropertyBindingKey<ColorF>,
    pub original_caret_color: ColorF,
    pub remaining_blink_count: usize,
}

impl CaretAnimation {
    pub(crate) fn update(&mut self, caret_visible: bool) -> Option<PropertyValue<ColorF>> {
        if self.remaining_blink_count == 0 {
            return None;
        }

        self.remaining_blink_count = self.remaining_blink_count.saturating_sub(1);
        let value = if caret_visible || self.remaining_blink_count == 0 {
            self.original_caret_color
        } else {
            ColorF::TRANSPARENT
        };

        Some(PropertyValue {
            key: self.caret_property_key,
            value,
        })
    }
}

#[cfg(test)]
mod tests {
    use paint_api::display_list::PaintAnimationEasing;

    use super::*;

    fn opacity_key(id: u64) -> PropertyBindingKey<f32> {
        PropertyBindingKey::new(id)
    }

    /// A fade-in that ran `duration` seconds ago and is already over.
    fn finished_fade(id: u64, duration: f64) -> PaintAnimation {
        PaintAnimation {
            property: PaintAnimationProperty::Opacity(
                opacity_key(id),
                vec![PaintAnimationSegment {
                    start: 0.0,
                    end: duration,
                    from: 0.0,
                    to: 1.0,
                    easing: PaintAnimationEasing::Linear,
                }],
            ),
            // Its zero point is far enough in the past that it has already ended.
            offset_from_display_list: -(duration * 2.0),
            // Sampled now, so the install anchors it to now: these tests are about the
            // offset, not about how long a display list took to arrive.
            built_at: CrossProcessInstant::now(),
            complete: true,
        }
    }

    /// 같은 모양인데 이징이 `ease-out` 인 것. `bounce` 의 첫 구간이 이 꼴이다.
    fn running_eased(id: u64, duration: f64) -> PaintAnimation {
        let mut animation = running_fade(id, duration);
        animation.property = PaintAnimationProperty::Opacity(
            opacity_key(id),
            vec![PaintAnimationSegment {
                start: 0.0,
                end: duration,
                from: 0.0,
                to: 1.0,
                // `ease-out` 의 제어점. 시작이 가장 빠르다.
                easing: PaintAnimationEasing::CubicBezier(0.0, 0.0, 0.58, 1.0),
            }],
        );
        animation
    }

    fn times(animations: &PipelineAnimations, window: Duration, step: Duration) -> Vec<Instant> {
        let now = Instant::now();
        let mut out = Vec::new();
        animations.paint_animation_sample_times(now, now + window, step, &mut out);
        out
    }

    /// ★선형 세그먼트는 내부 샘플이 필요 없다.★ 세그먼트 안에서 변환이 `t` 에 선형이면
    /// 사각형의 극값이 구간 끝에만 있으므로, 창의 양 끝만으로 정확하다.
    #[test]
    fn linear_segments_need_no_interior_samples() {
        let animations = PipelineAnimations::default();
        animations.install_paint_animations(vec![running_fade(1, 2.0)]);
        let got = times(
            &animations,
            Duration::from_millis(100),
            Duration::from_millis(16),
        );
        assert_eq!(got.len(), 2, "창 양 끝뿐이어야 한다: {got:?}");
    }

    /// ★비선형 이징은 내부를 훑어야 한다.★ 이것이 2026-11-11 에 고친 결함이다 -- 설계는
    /// "layout 이 선형 세그먼트로 쪼개 보낸다" 를 전제했는데 `CubicBezier` 가 그대로
    /// 실려 온다. `bounce` 의 첫 구간이 `ease-out` 이라 시작이 가장 빠르고, 경계 둘만
    /// 샘플하면 그 구간이 뭉개져 타일 경계에 띠가 남았다(log_webgl_perf_01/35).
    #[test]
    fn eased_segments_get_interior_samples() {
        let animations = PipelineAnimations::default();
        animations.install_paint_animations(vec![running_eased(1, 2.0)]);
        let step = Duration::from_millis(16);
        let got = times(&animations, Duration::from_millis(100), step);
        assert!(
            got.len() > 2,
            "창 안을 한 주기 간격으로 훑어야 한다: {got:?}"
        );
        // 100ms 창을 16ms 로 훑으면 대략 일곱 개 안쪽이다 -- 너무 많으면 셰이딩이 는다.
        assert!(got.len() <= 9, "샘플이 과하다: {}", got.len());
    }

    /// ★간격이 0 이면 내부 샘플을 만들지 않는다.★ `lead = 0` 으로 기능을 끈 구성에서
    /// 비용이 늘지 않아야 한다.
    #[test]
    fn zero_step_disables_interior_samples() {
        let animations = PipelineAnimations::default();
        animations.install_paint_animations(vec![running_eased(1, 2.0)]);
        let got = times(&animations, Duration::from_millis(100), Duration::ZERO);
        assert_eq!(got.len(), 2, "창 양 끝뿐이어야 한다: {got:?}");
    }

    /// A fade-in that is only starting now, so it is still playing.
    fn running_fade(id: u64, duration: f64) -> PaintAnimation {
        PaintAnimation {
            offset_from_display_list: 0.0,
            ..finished_fade(id, duration)
        }
    }

    fn sample(animations: &PipelineAnimations) -> (Vec<PropertyValue<f32>>, bool) {
        let mut floats = Vec::new();
        let mut transforms = Vec::new();
        let playing =
            animations.update_paint_animations(Instant::now(), &mut floats, &mut transforms);
        (floats, playing)
    }

    /// The defect this whole mechanism exists for: after the animation ends, the next
    /// transaction must still carry its final value, or WebRender reverts the binding to
    /// the value baked in at display-list time -- opacity 0 for a fade-in.
    #[test]
    fn finished_animation_keeps_its_final_value() {
        let animations = PipelineAnimations::default();
        animations.install_paint_animations(vec![finished_fade(1, 0.5)]);

        let (floats, playing) = sample(&animations);
        assert!(!playing, "an animation past its end is not playing");
        assert!(floats.is_empty(), "and it is no longer sampled");

        let mut floats = Vec::new();
        let mut transforms = Vec::new();
        let appended = animations.append_held_paint_values(&mut floats, &mut transforms);
        assert_eq!(appended, 1);
        assert_eq!(floats.len(), 1);
        assert_eq!(floats[0].key.id, opacity_key(1).id);
        assert_eq!(
            floats[0].value, 1.0,
            "the value it finished on, not the one it started from"
        );
    }

    /// Holding a value must never let it overwrite one that is still moving.
    #[test]
    fn a_running_animation_wins_over_a_held_value() {
        let animations = PipelineAnimations::default();
        animations.install_paint_animations(vec![finished_fade(1, 0.5), finished_fade(2, 0.5)]);
        sample(&animations);

        // Key 1 is being animated right now; key 2 is only held. Appending key 2 and not
        // key 1 is what proves the skip was a decision and not an empty held set.
        let mut floats = vec![PropertyValue {
            key: opacity_key(1),
            value: 0.25,
        }];
        let mut transforms = Vec::new();
        assert_eq!(
            animations.append_held_paint_values(&mut floats, &mut transforms),
            1
        );
        assert_eq!(floats.len(), 2);
        assert_eq!(floats[0].value, 0.25, "the live value is left alone");
        assert_eq!(floats[1].key.id, opacity_key(2).id);
    }

    /// A new display list that drives the key again takes the value back; one that says
    /// nothing about a key leaves it held, because the scene being rendered right now may
    /// still be the old one.
    #[test]
    fn a_new_display_list_reclaims_only_the_keys_it_drives() {
        let animations = PipelineAnimations::default();
        animations.install_paint_animations(vec![finished_fade(1, 0.5), finished_fade(2, 0.5)]);
        sample(&animations);

        animations.install_paint_animations(vec![running_fade(1, 10.0)]);

        let mut floats = Vec::new();
        let mut transforms = Vec::new();
        animations.append_held_paint_values(&mut floats, &mut transforms);
        assert_eq!(
            floats.len(),
            1,
            "key 1 is driven again; key 2 is still held"
        );
        assert_eq!(floats[0].key.id, opacity_key(2).id);
    }

    /// The held set is a value cache, not a queue: replaying the same key must not grow it.
    #[test]
    fn holding_the_same_key_twice_replaces_it() {
        let animations = PipelineAnimations::default();
        for _ in 0..3 {
            animations.install_paint_animations(vec![finished_fade(1, 0.5)]);
            sample(&animations);
        }

        let mut floats = Vec::new();
        let mut transforms = Vec::new();
        assert_eq!(
            animations.append_held_paint_values(&mut floats, &mut transforms),
            1
        );
    }

    /// ★원점은 "리스트가 만들어진 시각" 이어야 한다.★ 받은 시각에 박으면 그 사이에 든
    /// 시간(리플로 잔여 + 전송 + 씬 빌드)만큼 예측 전체가 과거로 밀린다.
    #[test]
    fn zero_is_anchored_at_build_time_not_receipt() {
        let built = CrossProcessInstant::epoch() + time::Duration::seconds_f64(100.0);
        let now = built + time::Duration::milliseconds(3);
        let received_at = Instant::now();
        let zero = animation_zero(built, now, received_at, 0.0);
        let behind = received_at.saturating_duration_since(zero);
        assert!(
            (behind.as_secs_f64() - 0.003).abs() < 1e-4,
            "받은 시각보다 3ms 앞이어야 한다: {behind:?}"
        );
    }

    /// ★이 테스트가 Task 6 의 이유다.★ 연달아 오는 리스트의 `built_at` 이 고른 격자 위에
    /// 있으면, 전송 시간이 매번 달라도 ***원점이 정확히 같은 간격으로*** 늘어서야 한다.
    /// 그래야 예측들이 서로 맞물린다. 원점이 흔들리면 그 흔들림이 그대로 화면의 떨림이다.
    #[test]
    fn even_build_times_give_even_zeros_despite_jittery_transit() {
        let base = CrossProcessInstant::epoch() + time::Duration::seconds_f64(100.0);
        let period = time::Duration::seconds_f64(1.0 / 60.0);
        let start = Instant::now();
        // 전송 지연을 0.2ms ~ 2.0ms 로 흔든다. 실측 범위다(log_ani_debug/08 SCRIPTBUSY).
        let transits_ms = [0.2, 2.0, 0.5, 1.7, 0.3];
        let zeros: Vec<Instant> = transits_ms
            .iter()
            .enumerate()
            .map(|(index, transit_ms)| {
                let built = base + period * (index as i32);
                let now = built + time::Duration::seconds_f64(transit_ms / 1000.0);
                let received_at = start
                    + std::time::Duration::from_secs_f64(
                        (index as f64) / 60.0 + transit_ms / 1000.0,
                    );
                animation_zero(built, now, received_at, 0.0)
            })
            .collect();
        for pair in zeros.windows(2) {
            let gap = pair[1].saturating_duration_since(pair[0]).as_secs_f64();
            assert!(
                (gap - 1.0 / 60.0).abs() < 1e-4,
                "원점 간격이 고르지 않다: {gap}"
            );
        }
    }

    /// 오래된 트리의 애니메이션은 과거에서 시작한다 -- `offset_from_display_list` 가 음수다.
    #[test]
    fn a_negative_offset_moves_zero_into_the_past() {
        let built = CrossProcessInstant::epoch() + time::Duration::seconds_f64(100.0);
        let received_at = Instant::now();
        let zero = animation_zero(built, built, received_at, -0.5);
        let behind = received_at.saturating_duration_since(zero);
        assert!(
            (behind.as_secs_f64() - 0.5).abs() < 1e-4,
            "0.5초 과거여야 한다: {behind:?}"
        );
    }

    /// ★전송 시간이 터무니없으면 자른다.★ 콘텐츠 전환이나 스톨 뒤에 몇 초짜리 transit 이
    /// 나오면 원점이 그만큼 과거로 가 애니메이션이 순간이동한다.
    #[test]
    fn an_absurd_transit_is_clamped() {
        let built = CrossProcessInstant::epoch() + time::Duration::seconds_f64(100.0);
        let now = built + time::Duration::seconds(10);
        let received_at = Instant::now();
        let zero = animation_zero(built, now, received_at, 0.0);
        let behind = received_at.saturating_duration_since(zero);
        assert!(
            behind <= MAX_DISPLAY_LIST_TRANSIT,
            "{behind:?} 가 상한 {MAX_DISPLAY_LIST_TRANSIT:?} 를 넘었다"
        );
    }

    /// ★`built_at` 이 미래일 수 있다.★ Task 6 이후 그 값은 셸이 잰 프레임 시각이고, paint 의
    /// `now` 와는 다른 시점의 읽기라 두 시계가 역전해 보일 수 있다. 하한 클램프를 잃으면
    /// `Duration::from_secs_f64` 가 음수로 ***패닉한다.***
    #[test]
    fn a_built_at_in_the_future_does_not_panic() {
        let now = CrossProcessInstant::epoch() + time::Duration::seconds_f64(100.0);
        let built = now + time::Duration::milliseconds(2);
        let received_at = Instant::now();
        let zero = animation_zero(built, now, received_at, 0.0);
        assert_eq!(
            zero, received_at,
            "transit 이 0 으로 잘려 수신 시각과 같아야 한다"
        );
    }
}
