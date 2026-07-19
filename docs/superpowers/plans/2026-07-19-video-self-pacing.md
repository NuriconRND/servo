# escape 비디오 자체 페이싱 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** D3D11 링 경로 비디오의 프레임 도착이 컴포짓을 직접 구동하게 해(rAF/하트비트 의존 제거) 무애니 페이지 동결을 근본수정하고 AMD 잉여 컴포짓을 제거한다 (스펙 §14, 승인됨).

**Architecture:** script 측 프레임 렌더러 `render_d3d11_yuv_frame`(htmlmediaelement.rs — 링 경로에서 매 프레임 도착마다 실행되지만 첫 프레임 이후 보낼 ImageUpdate가 없어 §3-m 훅이 침묵)에서 신규 `PaintMessage::VideoFrameArrived(PainterId)`를 송신 → paint.rs 핸들러 → painter의 신규 `composite_for_video_arrival()`이 **§3-m 즉시 컴포짓과 동일한 가드**(rAF 활성 시 억제 / pending_frames==0 / renderer_behind 아님)를 통과할 때만 컴포짓-온리 프레임 생성. 컴포지터(canvas_flush) 무변경.

**정밀화(스펙 §14.1 대비, 마감 시 §14에 기록)**: 신호 지점은 "gst 링 publish"가 아니라 script 측 렌더러다 — 같은 도착 케이던스이며 paint_api 핸들·painter 라우팅이 기성이라 더 단순. 또한 이 지점은 escape 전용이 아니라 **A-dyn 링 경로 공통**(WR direct-sample 포함)이라, 링 경로 전체의 하트비트 의존이 함께 해소된다(off 모드의 소프트웨어 YUV 경로는 기존 per-frame UpdateImage로 이미 커버 — 무접촉).

**Tech Stack:** Rust (paint_api / servo-paint / script), PowerShell 검증.

## Global Constraints

- 리포 `D:\2_TechReview\20260606_multigpu_browser\servo`, 브랜치 `multigpu-tiled-wall`. **푸시 금지.** 커밋 한국어, Claude 서명 금지.
- 킬스위치 `SERVO_VIDEO_SELF_PACING` — 리터럴 "0"만 off, 그 외/미설정 = on (스펙 §14.2).
- painter의 신규 경로는 §3-m 가드 전 항목을 반드시 유지: `animation_callbacks_running()`(rAF 주도 시 억제 — 이중 페이싱/지터 방지의 기존 설계), `pending_frames.get()==0`, `!renderer_behind()`. 컴포짓 폭주 금지(재시도 금지 목록 "vsync 드라이버"와의 차별점).
- canvas_flush/dcomp_compositor 무접촉. off 모드(비DComp) 소프트웨어 경로 무접촉.
- 셸 포그라운드만(백그라운드 대기 금지 — Start-Sleep은 같은 호출 안에서), 타임아웃 600000ms, 실행 후 servoshell kill+env 정리.
- **빌드**: `Stop-Process servoshell` → `. .\etc\multigpu\servo_env.ps1` → `$ErrorActionPreference='Continue'` → `. .\.venv\Scripts\Activate.ps1` → `python mach build --release`.
- 유닛테스트: `cargo test -p servo-paint --lib --features paint_api/no-wgl <필터>`.
- 런처 `-Page`는 리포 루트 상대경로. winshot은 `scratchpad\shot.png` 고정 → 복사.

---

### Task 1: 도착 신호 배관 (PaintMessage → painter 가드 컴포짓)

**Files:**
- Modify: `components/shared/paint/lib.rs` (`PaintMessage` enum :92 부근에 variant, `update_images` 송신 함수(:479) 옆에 sender)
- Modify: `components/paint/paint.rs` (`PaintMessage::UpdateImages` 핸들러(:1829 부근) 옆에 새 arm)
- Modify: `components/paint/painter.rs` (env static(:60-68 기존 비디오 게이트들 옆) + 순수 함수 + `composite_for_video_arrival` + tests)
- Modify: `components/script/dom/html/htmlmediaelement.rs` (`render_d3d11_yuv_frame` 말미에 송신 1줄)

**Interfaces:**
- Consumes: 기존 `CrossProcessPaintApi(self.0: IpcSender<PaintMessage>)` 패턴, painter의 `generate_frame(&self, &mut Transaction, RenderReasons)` / `pending_frames` / `renderer_behind()` / `animation_callbacks_running()` / `display_composite_in_flight`, htmlmediaelement의 `self.paint_api`+`self.webview_id`.
- Produces: `PaintMessage::VideoFrameArrived(PainterId)`; `CrossProcessPaintApi::notify_video_frame_arrived(&self, painter_id: PainterId)`; `Painter::composite_for_video_arrival(&mut self)`(pub(crate)); 순수 함수 `video_self_pacing_enabled(token: Option<&str>) -> bool` — Task 2 검증이 킬스위치 거동에 의존.

- [ ] **Step 1: 실패하는 테스트 작성**

`components/paint/painter.rs`에 tests 모듈이 없으면 파일 말미에 추가:

```rust
#[cfg(test)]
mod tests {
    use super::video_self_pacing_enabled;

    #[test]
    fn video_self_pacing_enabled_default_on_literal_zero_off() {
        // 기본(미설정) = on (스펙 §14.2)
        assert!(video_self_pacing_enabled(None));
        // 리터럴 "0"만 off
        assert!(!video_self_pacing_enabled(Some("0")));
        // 그 외 값은 전부 on (오타 안전 방향)
        assert!(video_self_pacing_enabled(Some("1")));
        assert!(video_self_pacing_enabled(Some("off")));
        assert!(video_self_pacing_enabled(Some("")));
    }
}
```

- [ ] **Step 2: 실패 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl self_pacing`
Expected: 컴파일 실패 — `cannot find function video_self_pacing_enabled`

- [ ] **Step 3: painter 구현**

`components/paint/painter.rs` — 기존 비디오 게이트 statics(:60-68, `VIDEO_UPDATE_COALESCE_DISABLED` 등) 옆에 (해당 파일의 static 선언 관례를 그대로 따라 — LazyLock/Lazy 형태 확인 후 동일하게):

```rust
/// escape/링 경로 비디오 자체 페이싱 킬스위치(스펙 §14.2). 기본 on —
/// `SERVO_VIDEO_SELF_PACING=0`(리터럴)일 때만 off. 순수 판정은
/// `video_self_pacing_enabled` (TDD 대상).
fn video_self_pacing_enabled(token: Option<&str>) -> bool {
    !matches!(token, Some("0"))
}

static VIDEO_SELF_PACING: LazyLock<bool> = LazyLock::new(|| {
    video_self_pacing_enabled(std::env::var("SERVO_VIDEO_SELF_PACING").ok().as_deref())
});
```

(파일이 `once_cell::sync::Lazy`를 쓰면 그 형태로 — 주변 static과 동일 idiom 필수.)

`update_images` 근처에 신규 메서드 — **가드 구성은 update_images 말미의 즉시 컴포짓 블록(:2027-2036)과 정확히 동일한 항목**(그 블록의 장문 주석이 각 가드의 존재 이유 — 특히 rAF 억제):

```rust
    /// D3D11 링 경로 비디오의 프레임 도착 신호(스펙 §14). 링 경로는 첫 프레임 이후
    /// ImageUpdate가 없어 update_images의 즉시 컴포짓 훅이 침묵한다 — 이 메서드가
    /// 같은 가드로 그 빈자리를 채운다(리소스 업데이트 없는 컴포짓-온리 프레임).
    /// 가드 의미론은 update_images 말미 블록과 동일: rAF가 케이던스를 주도 중이면
    /// 양보(이중 페이싱 지터 방지), in-flight/백로그 시 신호 소멸(폭주 불가).
    pub(crate) fn composite_for_video_arrival(&mut self) {
        if !*VIDEO_SELF_PACING {
            return;
        }
        if self.animation_callbacks_running() ||
            self.pending_frames.get() != 0 ||
            self.renderer_behind()
        {
            return;
        }
        let mut txn = Transaction::new();
        self.generate_frame(&mut txn, RenderReasons::SCENE);
        self.display_composite_in_flight.set(true);
        self.send_transaction(txn);
    }
```

- [ ] **Step 4: 메시지/핸들러/송신 구현**

`components/shared/paint/lib.rs` — `PaintMessage` enum의 `UpdateImages`(:155) 옆:

```rust
    /// D3D11 링 경로 비디오 프레임 도착 알림(스펙 §14) — 리소스 업데이트 없이
    /// painter의 도착 구동 컴포짓만 요청. UpdateImages와 동일한 painter 라우팅.
    VideoFrameArrived(PainterId),
```

같은 파일 `update_images` 송신 함수(:479) 옆:

```rust
    /// 링 경로 비디오의 프레임 도착 신호(스펙 §14). 페이로드 없음 — painter가
    /// 자체 가드로 병합/폐기한다.
    pub fn notify_video_frame_arrived(&self, painter_id: PainterId) {
        if let Err(e) = self.0.send(PaintMessage::VideoFrameArrived(painter_id)) {
            warn!("Could not send video frame arrival to Paint {}", e);
        }
    }
```

`components/paint/paint.rs` — `PaintMessage::UpdateImages` arm(:1829 부근) 옆에 새 arm. **painter 라우팅은 UpdateImages arm과 동일 방식**(그 arm이 wall fanout으로 target_painter_ids를 계산하면 같은 대상 집합에, 아니면 `maybe_painter_mut(painter_id)` 단일 대상에 — 실제 UpdateImages arm의 라우팅 코드를 읽고 그대로 미러링하되 이미지 데이터 관련 부분만 제외):

```rust
            PaintMessage::VideoFrameArrived(painter_id) => {
                // UpdateImages와 동일한 대상 painter(들)에 도착 컴포짓을 요청한다.
                // (wall fanout 구성이면 동일 target 집합 — UpdateImages arm의 라우팅 미러.)
                if let Some(mut painter) = self.maybe_painter_mut(painter_id) {
                    painter.composite_for_video_arrival();
                }
            },
```

(UpdateImages arm에 fanout 분기가 있으면 — :1810 부근 `target_painter_ids` — 그 로직을 이 arm에도 적용해 각 target painter에 `composite_for_video_arrival()` 호출. 이미지 벡터 복제는 불필요.)

`components/script/dom/html/htmlmediaelement.rs` — `render_d3d11_yuv_frame`의 정상 경로 말미(기존 `self.paint_api.update_images(...)` 호출 뒤 또는 updates가 비어 보내지 않는 경로 포함 — **매 프레임 반드시 1회** 실행되는 위치에):

```rust
        // 스펙 §14: 링 경로는 첫 프레임 이후 ImageUpdate가 없어 도착 신호를 별도로
        // 보낸다 — painter가 rAF 부재 시 이 신호로 컴포짓을 페이싱한다(가드/병합은
        // painter 몫, 여기선 무조건 1회 송신).
        self.paint_api
            .notify_video_frame_arrived(self.webview_id.into());
```

- [ ] **Step 5: 유닛 + 스위트 + 빌드**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl self_pacing` → PASS
Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl dcomp` → 전부 PASS
릴리스 빌드(Global Constraints 순서) → 성공.

- [ ] **Step 6: 2×2 동결 수정 스모크 (결정 게이트)**

빈 rAF가 **없는** 임시 페이지로 확인 — `tests\html\video_grid_wall_clean.html`을 임시 복사해 rAF 블록만 삭제한 `scratchpad\clean_noraf.html`을 만들고 `tests\html\zz_tmp_noraf.html`로 복사(검증 후 삭제):

```powershell
$env:SERVO_DCOMP_DEBUG = "1"; $env:SERVO_VIDEO_ESCAPE_PROF = "1"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 2 -Rows 2 -DComp -VideoEscape canvas -Page tests\html\zz_tmp_noraf.html -Sync 4
# (같은 호출에서 Start-Sleep 40 → winshot 2매(5초 간격) → Stop-Process)
```
판독: **rAF 없이 재생 진행**(두 캡처 카운터 전진 = 동결 소멸 — 이전엔 동결이 실증된 조건), vesc-prof `frames/presents ≈ 30/s`(비디오 카데이던스). 이어 `$env:SERVO_VIDEO_SELF_PACING="0"`으로 재실행 → **동결 재현**(킬스위치 실효 + 수정 인과 증명). env 정리, zz_tmp 파일 삭제.

- [ ] **Step 7: 커밋**

```powershell
git add components/shared/paint/lib.rs components/paint/paint.rs components/paint/painter.rs components/script/dom/html/htmlmediaelement.rs
git commit -m "링 경로 비디오 자체 페이싱: 프레임 도착 신호(VideoFrameArrived)로 컴포짓 구동, rAF/하트비트 의존 해제(스펙 14, 킬스위치 SERVO_VIDEO_SELF_PACING=0)"
```

---

### Task 2: 페이지/가이드 되돌림 + 검증 배터리 + 패키지 + 스펙 기록

**Files:**
- Modify: `tests/html/video_grid_wall_clean.html` (빈 rAF 루프+load-bearing 주석 제거 → "자체 페이싱이 담당(스펙 §14)" 주석으로 교체)
- Modify: `etc/multigpu/package_run_wall.ps1` (rAF caution 블록을 자체 페이싱 안내로 교체 + AMD 판독 항목 1줄: 자체 페이싱 후 GPU% 하락 예측 ~56%)
- Modify: `docs/superpowers/specs/2026-07-18-shared-video-canvas-design.md` (`### 14.6 구현 결과` + §14.1 정밀화 기록: 신호 지점=script 렌더러, 적용 범위=링 경로 공통)
- 패키지: exe/런처/페이지 재복사 + zip 재생성

**Interfaces:**
- Consumes: Task 1의 신호 경로·킬스위치, 기존 검증 도구(winshot/vesc-prof/런처).
- Produces: 검증 기록. Rust 변경 0(Task 1 빌드 그대로).

- [ ] **Step 1: 클린 페이지 rAF 제거**

`video_grid_wall_clean.html`에서 빈 rAF 루프와 그 위 load-bearing 사유 주석 블록을 제거하고 교체:

```html
<!-- 스펙 §14(2026-07-19): 링 경로 비디오는 프레임 도착 신호가 컴포짓을 자체
     페이싱하므로 이 페이지에 rAF/하트비트가 필요 없다(§13.4 이탈1의 빈 rAF
     load-bearing 해제). 킬스위치 SERVO_VIDEO_SELF_PACING=0 시에만 동결됨(진단). -->
```

- [ ] **Step 2: 45타일 검증 (핵심 게이트)**

```powershell
$env:SERVO_DCOMP_DEBUG = "1"; $env:SERVO_VIDEO_ESCAPE_PROF = "1"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 9 -Rows 5 -DComp -VideoEscape canvas -Page tests\html\video_grid_wall_clean.html -Sync 45
```
60초 관측(포그라운드 Start-Sleep 분할): ①재생 정상(winshot 2매 카운터 전진+lockstep) ②vesc-prof **frames/presents ≈ 30/s 수렴**(§14.4 — 기존 빈 rAF 시절 36-47/s에서 하락) ③`canvas swapchain (re)create` 1회. 종료.

- [ ] **Step 3: 무회귀 배터리**

각 60초 내외, 판독 기준 포함:
1. **perf 페이지(rAF 있는 월)**: `-Cols 9 -Rows 5 -DComp -VideoEscape canvas -Sync 45` — rAF 활성 페이지에서 컴포짓률이 기존(~60/s)에서 이상 상승하지 않는지(vesc-prof — `animation_callbacks_running` 억제 가드 실효 증거), 재생 정상.
2. **mixed_media_demo**: `-Cols 3 -Rows 2 ... -Page tests\html\mixed_media_demo.html -Sync 6` — 전 요소 정상(시계/티커/자막), presents<frames 유지.
3. **WR direct-sample 경로(escape off + DComp)**: `-Cols 3 -Rows 3 -DComp` (play 페이지 기본) — 링 경로 공통 신호가 WR 모드에서도 무해(재생 정상, 하트비트 dot 페이지라 rAF... play 페이지는 rAF 하트비트가 있으므로 억제 가드 경로 — 정상 재생만 확인).
4. **off(비DComp)**: `-Cols 2 -Rows 2` (DComp 스위치 없이) — 소프트웨어 경로 무접촉 확인(재생 정상).
5. **킬스위치**: Task 1 Step 6에서 이미 동결 재현으로 검증 — 결과만 인용.

- [ ] **Step 4: 가이드/패키지**

`package_run_wall.ps1`: 기존 "empty rAF 유지" caution 블록을 교체(영어):

```powershell
#   NOTE (self-pacing, 2026-07-19): ring-path video now drives composites from frame
#   arrival (SERVO_VIDEO_SELF_PACING=0 reverts to page-rAF pacing for diagnosis).
#   Wall pages no longer need a rAF/heartbeat. Expected on AMD: canvas GPU% drops
#   (~90% -> ~56% at 6x6) as composites converge to the 30fps video cadence.
```

패키지 재생성:
```powershell
Copy-Item target\release\servoshell.exe D:\ServoWallPackage\ -Force
Copy-Item etc\multigpu\package_run_wall.ps1 D:\ServoWallPackage\run_wall.ps1 -Force
Copy-Item tests\html\video_grid_wall_clean.html D:\ServoWallPackage\tests\html\ -Force
Compress-Archive -Path D:\ServoWallPackage\* -DestinationPath D:\ServoWallPackage.zip -Force
```
패키지 스모크: `-Cols 2 -Rows 2 -DComp -VideoEscape canvas -Page tests\html\video_grid_wall_clean.html -Sync 4` 기동 마커+재생.

- [ ] **Step 5: 스펙 §14.6 결과 + 커밋**

`### 14.6 구현 결과 (2026-07-19)`: 커밋 SHA, §14.1 정밀화(신호 지점=script 렌더러 `render_d3d11_yuv_frame`, 적용 범위=A-dyn 링 경로 공통 — escape 한정 아님·off 소프트웨어 경로 무접촉), 검증 수치(동결 소멸/presents≈30/무회귀 4종/킬스위치 동결 재현), 이탈(없으면 "이탈 없음").

```powershell
git add tests/html/video_grid_wall_clean.html etc/multigpu/package_run_wall.ps1 docs/superpowers/specs/2026-07-18-shared-video-canvas-design.md
git commit -m "자체 페이싱 마감: 클린 페이지 rAF 제거(동결 수정 실증), 무회귀 배터리, AMD 가이드 갱신, 패키지 재생성(스펙 14.6)"
```

---

## Self-Review 결과

1. **스펙 커버리지**: §14.1 신호+가드→Task 1(가드 4종 명시), §14.2 게이트/킬스위치→Task 1(순수 함수 TDD), §14.3 페이지 되돌림→Task 2 Step 1(+검증 수단으로 Step 2), §14.4 검증 4항→Task 1 Step 6+Task 2 Step 2-3, §14.5 완료 기준→Task 2 Step 4-5. 갭 없음. §14.1의 "gst publish 지점" 문면과 실제 신호 지점(script 렌더러)의 차이는 계획 헤더+Task 2 Step 5에서 스펙 정밀화로 기록하도록 명시.
2. **플레이스홀더**: 없음 — 라우팅 미러("UpdateImages arm과 동일")는 대상 코드 위치(:1810-1830)와 방법을 명시한 지시이며 구현 코드도 기본형 제공.
3. **타입 일관성**: `PaintMessage::VideoFrameArrived(PainterId)` / `notify_video_frame_arrived(&self, painter_id: PainterId)` / `composite_for_video_arrival(&mut self)` / `video_self_pacing_enabled(Option<&str>) -> bool` — 정의·사용 전 태스크 일치.
