# 비디오 external 스왑체인 안정화 — 스케일 애니메이션 처닝 제거 설계

- 날짜: 2026-07-20
- 상태: 설계 승인(사용자), 계획/구현 대기
- 관련: `docs/superpowers/specs/2026-07-17-video-wr-escape-design.md` §12.4(이월 사항 — 스케일 애니메이션 스왑체인 재생성 처닝)
- 대상 파일: `components/paint/dcomp_compositor.rs` (external 서피스 경로 한정)

## 1. 배경과 문제

`SERVO_VIDEO_ESCAPE=external` 모드에서 승격된 비디오는 비디오별 DComp flip 스왑체인에
직결된다(§2026-07-17 스펙). 스왑체인 백버퍼 크기는 **clip_rect 크기**로 잡히고
(`dcomp_compositor.rs:1408` `size = clip_rect.size()`), clip 크기가 바뀌면 재생성한다
(`:1491-1493` `ext.swapchain.is_none() || ext.swapchain_size != size`).

**문제**: CSS `scale` 애니메이션이 걸린 비디오 타일은 매 프레임 clip 크기가 연속으로
변하므로 스왑체인을 **매 프레임 재생성**한다. 재생성마다 `content_attached=false`/
`last_presented=None` 리셋(`:1519-1520`)이 발생해 잠재적 Present 보류/블랙 소지도 있다.

### 1.1 실측 (2026-07-20, 현재 external 빌드)

`complex_media_transforms.html`(4×3=12타일 + PiP) `SERVO_DCOMP_DEBUG=1` ~90초 계측:

| 신호 | 값 | 비고 |
|---|---|---|
| `create_external_surface` | 시작 시 11회만 | 승격 비디오 11개, 이후 생성/파괴 0 |
| `external swapchain (re)create` | **5653회 — 전부 `NativeSurfaceId(15)`** | 크기 연속 변화(392×286 ↔ 473×345) |
| 그 외 승격 타일(6~16) 재생성 | 각 1회(시작) | 정적 타일은 처닝 없음 |

`NativeSurfaceId(15)` = `fx-bob` 타일(이동+**스케일** 0.82↔1.0 애니메이션). WR이 전달하는
값: `scale=(0.993…, 애니메이션)`, `offset`, `clip=(스케일된 풋프린트)`, `src=1920×1080`.
`CompositorSurfaceTransform = ScaleOffset`(webrender composite.rs:1426)이라 `transform.scale`
/`transform.offset`으로 2D 스케일·오프셋을 직접 분해 가능하다.

**참고(재조사 방지)**: 같은 페이지의 회전 타일(`fx-spinz` Z-회전, `fx-flipy` 3D Y-플립)은
2D scale/translation이 아니므로 WR 승격에 실패하여(picture.rs:2701 폴백 조건) external
surface를 아예 받지 않고 콘텐츠 패스 폴백에 일관되게 머문다 — 승격/강등 토글(플래핑)이
없다. 캔버스 시절 §3-19가 기록한 "flipY 승격 플래핑"은 이 스왑체인 regen 지표를 flipY에
오귀속한 것으로 판단된다(실측상 범인은 `bob`의 스케일).

## 2. 목표 / 완료 기준

- **목표**: 스케일 애니메이션 비디오의 스왑체인 재생성을 정상 상태에서 0으로 만든다
  (재생성은 승격 시 1회, 실 레이아웃 리사이즈 시만).
- **완료 기준**
  1. transforms 페이지 재계측 시 `external swapchain (re)create`가 5653 → 승격분(~11)만.
  2. `bob` 타일이 육안상 정상(스케일·위치 정확, 크롭/스트레치/블랙 없음).
  3. 정적 타일(scale=1)·lockstep·순수 월 45타일/mixed/stress 무회귀.
  4. 크레이트 테스트 통과.

## 3. 범위 / 비범위

- **범위**: external 서피스의 스왑체인 크기 결정 + 비주얼 배치(스케일 반영 방식).
- **비범위**:
  - scale > 1.0(네이티브 초과 확대) 최적 화질 — 참조 크기가 첫 프레임 풋프린트라
    이후 확대 시 DComp 업스케일(블러). 데모는 scale≤1이라 무해. 필요 시 후속.
  - 부분 클립(clip < 스케일 풋프린트) 정밀 처리 — 현재 코드가 이미 clip==풋프린트로
    취급하는 한계를 그대로 계승(무회귀).
  - hybrid/surface 등 다른 DComp 스토리지 모드, 회전/3D 폴백 경로.

## 4. 근본원인 (코드 레벨)

external 서피스는 **스케일을 clip에 baked**하는 v1 계약을 쓴다(`:1354-1356` 주석:
"스왑체인 백버퍼는 이미 clip 크기로 비디오를 담고 있으므로 … scale은 무시"). 즉
`swapchain 크기 == 화면 표시 크기 == clip.size()`. 표시 크기가 스케일로 매 프레임 변하면
스왑체인 크기도 변해 재생성이 강제된다. **스왑체인 크기를 표시 크기에 묶은 것**이 근본원인.

## 5. 설계 (Option B — 스왑체인 크기 고정 + 비주얼 변환 스케일)

핵심: **스왑체인 크기를 언스케일 풋프린트로 고정**하고, 표시 스케일은 **DComp 비주얼
변환(SetTransform 행렬)**으로 적용한다. 스왑체인 크기가 스케일과 무관해져 재생성이 사라진다.

### 5.1 참조 크기(reference size) 산정·고정

- 언스케일 풋프린트: `ref = round(clip_rect.size() / transform.scale)`, `max(ref, 1×1)`.
- **승격 후 첫 스왑체인 생성 시 1회 산정하여 고정**(`ExternalStorage`에 저장).
- CSS `scale` transform은 레이아웃 박스를 바꾸지 않으므로 언스케일 풋프린트는 애니메이션
  동안 안정(스케일 지터 ±1px는 무시).

### 5.2 재생성 판정 변경

- 기존: `swapchain.is_none() || swapchain_size != clip.size()`
- 변경: `swapchain.is_none()` **또는** `|new_ref − 고정_ref| > TOLERANCE`(각 축 4px)
  — 스케일 애니는 재생성을 트리거하지 않고, 실 레이아웃 리사이즈(기존 파괴 경로를 안 탄
  경우)만 방어적으로 재생성. `resize_active`(드래그) 중 억제는 기존 로직 유지.

### 5.3 비주얼 배치(스케일 반영)

`place_external_visual`에서 현재 `SetOffsetX/Y_1 + SetClip_1(로컬 (0,0)-(clip.w,clip.h))`을
다음으로 교체:

- `display = clip_rect.size()`(스케일된 표시 풋프린트).
- 스케일 행렬 `sx = display.w / ref.w`, `sy = display.h / ref.h`.
- **`SetTransform_1(D2D_MATRIX_3X2_F{ m11=sx, m22=sy, m21=m12=0, dx=dy=0 })`** — 원점 기준
  스케일. 위치는 기존대로 `SetOffsetX/Y_1(clip.min)`(변환과 합성 = 스케일 후 clip.min 평행이동).
- `SetClip_1(로컬 (0,0)-(ref.w, ref.h))` — 백버퍼 전체(변환 적용 후 표시 풋프린트로 매핑).
- **정적 타일(scale=1)**: `ref==display` → 행렬=단위 스케일 → 현재 SetOffset 동작과 동일(무회귀).

> 구현 확인 사항: DComp에서 SetClip이 SetTransform에 대해 pre/post-transform 어느 좌표계에
> 적용되는지(MS docs 근거 `:464` 주석). 로컬 클립 `(0,0)-(ref)`가 변환 후 표시 풋프린트로
> 정확히 매핑되는지 육안(크롭/오버플로 없음)으로 검증. 어긋나면 클립을 변환에 맞춰 조정.

### 5.4 convert 패스

**무변경.** 지금처럼 전체 백버퍼(=참조 크기)에 YUV→RGBA 1-draw. scale≤1이면 DComp가
다운스케일 → 화질 저하 무시 가능. `src=1920×1080` → `ref`(예: 476×347) 다운스케일은 기존과
동일 성격.

### 5.5 데이터 구조 변경

`ExternalStorage`(`:909`)에 참조 크기 필드를 둔다(신규 `ref_size: DeviceIntSize` 또는 기존
`swapchain_size` 의미를 "참조 크기"로 재정의 — 계획에서 확정). 첫 생성 시 설정, 재생성
판정·행렬 계산의 기준.

## 6. 게이트

- **기본 on**(기존 external 낭비를 고치는 것, 정적 타일 바이트동일).
- 킬스위치 env **`SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN`** — `0`이면 구 동작(clip 크기 스왑체인,
  매 프레임 재생성)으로 복귀. 프로세스당 1회 lazy 읽기(다른 게이트 관례와 동일). AMD A/B용.

## 7. 변경 지점 요약

| 지점 | 변경 |
|---|---|
| `dcomp_compositor.rs:909` `ExternalStorage` | 참조 크기 필드 추가/재정의 |
| `:1358` `place_external_visual` | SetOffset+SetClip → SetTransform(스케일)+SetOffset+SetClip |
| `:1408` `add_external_surface` | 표시 크기(clip)와 참조 크기 분리 전달 |
| `:1463` `present_external` | 재생성 판정을 참조 크기 기준으로, 킬스위치 분기 |
| 게이트 헬퍼(파일 상단 `dcomp_debug`류 옆) | `stable_swapchain()` lazy env 읽기 |

## 8. 검증 계획

1. **객관(로그)**: transforms 페이지 `SERVO_DCOMP_DEBUG=1` ~90초 → `external swapchain
   (re)create` 카운트가 승격분(~11)만. `NativeSurfaceId(15)` 처닝 소멸.
2. **육안**: bob 타일 스케일 애니메이션이 크롭/스트레치/블랙 없이 부드럽게. 정적 타일 무변화.
3. **무회귀**: 순수 월 `video_grid_6x6_play.html` 45타일(정적, scale=1) lockstep·표시 무변화;
   mixed_media_demo / complex_media_stress 정상.
4. **A/B**: 킬스위치 `=0`로 구 동작(재생성 다수) 재현되는지 확인(레버 유효성).
5. **크레이트 테스트**: `dcomp_video_convert` 등 기존 테스트 통과.

## 9. 리스크 / 완화

- **SetTransform 좌표계/클립 상호작용**(§5.3): 신규 경로. 육안 검증 + 필요 시 클립 좌표 조정.
  정적 타일이 identity라 최소한 순수 월은 안전.
- **첫 프레임 참조 산정 시 scale이 비정상값**(예: 0 근처): `max(ref,1)` 클램프 + scale이
  아주 작을 때 방어. 데모는 scale∈[0.82,1].
- **참조 고정 후 실 레이아웃 리사이즈**: TOLERANCE 가드로 재생성. 기존 리사이즈/파괴 경로가
  대부분 선처리.
- 롤백: 킬스위치 `=0` 또는 커밋 revert.
