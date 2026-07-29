# DComp 투명 구멍 조사 기록 (2026-07-29, 진행 중)

DirectComposition 네이티브 컴포지터(`SERVO_COMPOSITOR_DCOMP=1`)를 켜고 월을 다중 창으로 띄우면
**두 번째 창에 사각형 영역이 투명하게 뚫려 뒤 데스크톱이 비쳐 보이는** 현상. 이 문서는 조사 경과와
그 과정에서 추가한 진단용 프로브 페이지의 사용법을 남긴다.

> **이 현상은 가드밴드(`overlapPx`) 작업과 무관한 선재 버그다.** `overlapPx: 0`(가드밴드 코드가
> 사실상 no-op이 되는 조건)에서도 그대로 재현된다. 가드밴드 관련 내용은
> `docs/superpowers/specs/2026-07-28-wall-guard-band-present-inset-design.md` 참조.

## 1. 증상

- 조건: `SERVO_COMPOSITOR_DCOMP=1` + `--wall-all-tiles`(창 2개 이상) + `multigpu_wall_video_file_probe.html`
- 두 번째 디스플레이에 대략 1400×570 크기의 사각형이 투명. 좌변이 정확히 타일 경계(desktop x=1920),
  상변이 y≈512.
- 첫 번째 창은 항상 정상.

메커니즘의 1차 원인: DComp가 발동하면 `WindowRenderingContext::present()`가 통째로 스킵되므로
(`components/shared/paint/rendering_context.rs`), **DComp 비주얼이 덮지 않는 영역은 그릴 주체가
아예 없어** 창 백버퍼의 미초기화 상태 = 투명으로 남는다. 즉 구멍 = **커버리지 구멍**.

`SERVO_DCOMP_DEBUG=1` 로그의 서피스 clip 좌표를 환산하면 관측 구멍과 정확히 일치한다:

```
tile 1이 받는 서피스
  id12  clip=(0,0)-(1952,512)        bind 0회
  id14  clip=(1434,512)-(1952,1080)
  id15  clip=(0,24)-(1952,1080)      bind은 row 0 타일뿐
→ fb x0..1434, y512..1080 을 덮는 비주얼 없음
→ desktop = 1888 + fb_x 이므로 x1920..3322, y512..1080  (실측 ~1920..3316, ~509..1080)

tile 0에는 창 전체를 덮는 id8 clip=(0,0)-(1952,1080)이 있다 — 두 painter 비대칭.
```

## 2. 반증된 가설 (같은 길을 다시 파지 말 것)

| 가설 | 반증 근거 |
|---|---|
| 비디오가 external compositor surface로 승격돼 WR이 구멍을 냈는데 컴포지터가 못 채움 | `SERVO_DCOMP_DEBUG=1` 로그에 `create_external_surface` / `attach_external_image` **0회**. 일반 `create_surface`(픽처 캐시)만 16개. 승격 자체가 없다 |
| §4.5 전역 provider(`VIDEO_EXTERNAL_PROVIDER` `OnceLock`) 다중창 미지원 | 위와 같이 external 경로를 아예 안 탄다. `no VideoExternalSurfaceProvider registered` 경고도 없음 |
| 스왑체인 present withhold | `withhold` 로그 0줄 |
| 가드밴드/`present_inset` | `overlapPx: 0`에서도 재현 |

> **주의:** `[dcomp-dbg] external add` 로그의 *부재*는 "external 경로 미사용"과 "provider 조기 반환"
> 양쪽과 양립한다(그 로그가 provider 조회 뒤에 있음). 로그 부재를 근거로 삼기 전에 코드 위치를 확인할 것.

## 3. 이분 탐색 결과 — 트리거

| 프로브 | 비디오 | 문서 넘침 | 비디오가 뷰포트 밖 | `<main position:relative>` | 구멍 |
|---|---|---|---|---|---|
| `video_minimal` | 소형 | ✗ | ✗ | ✗ | 없음 |
| `overflow` | ✗ | O | — | ✗ | 없음 |
| `video_overflow` | 소형 | O | ✗ | ✗ | 없음 |
| `video_geom` | 대형 | O | O | ✗ | 없음 |
| `subtract`(기준점) | 대형 | O | O | **O** | **남** |
| `subtract?no=main` | 대형 | O | O | 제거 | 없음 |
| `subtract?no=controls` | 대형 | O | O | O | 남 |
| `subtract?main=static` | 대형 | O | O | relative만 해제 | **없음** |
| `subtract?main=small` | 대형 | O | O | relative 유지, 3840×1080 | **남** |

**트리거 = `position: relative` 속성 자체.** 박스 크기는 무관하다(뷰포트와 같은 크기여도 재현).

### 분석

확정된 조건 집합: **비디오 + 문서가 뷰포트를 넘침 + `position:relative` 래퍼 + translation이 걸린 painter.**

마지막 항이 핵심이다. 월 팬아웃은 painter마다 root reference frame을 타일 원점만큼 이동시킨다
(`components/paint/painter.rs`의 `push_reference_frame(-viewport_origin)`).
**tile 0은 origin이 (0,0)이라 항등 변환**이고, tile 1만 −1888이 걸린다.

`position: relative`는 그 자체로 스태킹 문맥을 만들지 않지만 positioned box가 되어 Servo 디스플레이
리스트에서 별도 취급을 받는다. 그것이 painter별 참조프레임 이동과 겹치면서 WR picture-cache의
슬라이스/타일 할당에 빈 영역이 생기는 것으로 보인다. 이 가설은 관측 네 가지를 모두 설명한다:

| 관측 | 설명 |
|---|---|
| 두 번째 창에서만 | tile 0은 translation이 0 |
| 타일 크기 ≥ 창이면 소멸 | 슬라이스당 타일 1장이면 할당 누락 불가 |
| 비디오가 필요 | 비디오가 별도 슬라이스를 유발(승격은 안 되지만 분할은 됨) |
| `overlapPx` 무관 | 0에서도 origin=−1920으로 여전히 non-zero |

**아직 메커니즘 수준 국소화이지 WR 코드 라인까지 짚은 것은 아니다.** 다음 조사는 Servo
`components/layout/display_list/stacking_context.rs`의 relative 처리와 WR picture-cache 슬라이스
할당 경로.

## 4. 회피책

| 방법 | 비용 |
|---|---|
| **페이지: 큰 래퍼의 `position: relative` 제거** | 래퍼가 문서 원점에 있으면 절대배치 자식 위치가 동일 → **시각적 차이 없음, 비용 0** |
| 엔진: `SERVO_WR_PICTURE_TILE_SIZE=1920x1080` | 동작 확인됨. 단 부분 무효화 입도를 통째로 포기 |

월 페이지는 "전체 월 크기로 저작하고 일부만 표시"가 정상 패턴이라 **큰 relative 래퍼는 통상적인
저작 방식**이다. 엔진 수정 없이 두면 계속 밟게 된다.

## 5. 진단용 프로브 페이지

전부 `tests/html/` 아래. 비디오 소스는 `tests/Wildlife_FHD30fps_counter_10Mbitrate.mp4`.

| 파일 | 목적 |
|---|---|
| `multigpu_wall_ruler_probe.html` | 절대 가상좌표 눈금(8px/120px 격자, 120px마다 좌표 라벨). 크롭 정확성 판정용 — 타일 창 구석 라벨이 그 타일 `rect` 원점과 일치해야 통과 |
| `multigpu_wall_video_minimal_probe.html` | 비디오 O, 문서 넘침 X. 대조군 |
| `multigpu_wall_overflow_probe.html` | 비디오 X, 문서 넘침 O. 대조군 |
| `multigpu_wall_video_overflow_probe.html` | 비디오 소형 + 넘침. 대조군 |
| `multigpu_wall_video_geom_probe.html` | `file_probe`의 비디오 기하만 재현, 장식 없음. 대조군 |
| `multigpu_wall_video_bisect_probe.html` | (구판, 가산 방식) `?controls=1&bg=1&stage=1&strip=1&panel=1` — **기준점이 원본의 부분집합이 아니라 위음성이 났다. 사용 비권장** |
| **`multigpu_wall_video_subtract_probe.html`** | **주력.** 기준점이 `file_probe`와 동일하고 `?no=`로 제거만 한다 |

### `multigpu_wall_video_subtract_probe.html` 쿼리스트링

파라미터 없이 열면 `multigpu_wall_video_file_probe.html`과 동일한 구조 = **구멍이 나야 정상**.

| 파라미터 | 제거되는 것 |
|---|---|
| `?no=main` | `<main position:relative>` 래퍼(자식을 body로 승격) |
| `?no=controls` | `<video controls>` |
| `?no=bg` | html+body 4중 그라디언트 배경 |
| `?no=stage` | `.video-stage`의 border / box-shadow / background |
| `?no=strip` | `.motion-strip`(하단 무한 애니메이션) |
| `?no=labels` | 경계 라벨 4개 |
| `?main=static` | 래퍼 유지, `position: relative`만 해제 |
| `?main=small` | relative 유지, 크기만 3840×1080 |

여러 개 동시 지정: `?no=main,controls`

### 실행 방법과 함정

```powershell
. .\scripts\servo_env.ps1
cd W:\servo_multigpu-tiled-wall
$env:SERVO_COMPOSITOR_DCOMP=1
Remove-Item Env:\SERVO_WR_PICTURE_TILE_SIZE -ErrorAction SilentlyContinue
$L = 'etc\multigpu\config\wall_layout.example_2x1_display.json'
$B = 'file:///W:/servo_multigpu-tiled-wall/tests/html/multigpu_wall_video_subtract_probe.html'

.\target\release\servoshell.exe --wall-all-tiles --wall-layout $L "$B"            # 기준점: 구멍 남
.\target\release\servoshell.exe --wall-all-tiles --wall-layout $L "$B?no=main"    # 구멍 없음
```

1. **쿼리스트링은 절대 `file:///` URL에서만 동작한다.** 상대 경로 + `?query`를 주면 servoshell이
   전체를 파일명으로 취급해 로드에 실패하고 네트워크 URL로 재해석한다(로그에 `dns error`).
2. **매번 좌상단 패널의 `REMOVED:` / `MAIN:` 줄로 파라미터가 실제로 먹었는지 확인할 것.**
   안 먹은 채 판정하면 전부 무의미하다(실제로 그렇게 위음성이 났었다).
3. `SERVO_WR_PICTURE_TILE_SIZE`가 설정돼 있으면 구멍이 무조건 안 난다. 반드시 지우고 시작.

### 로그 캡처

```powershell
$p = Start-Process -PassThru -FilePath .\target\release\servoshell.exe `
  -ArgumentList '--wall-all-tiles','--wall-layout',$L,$B -RedirectStandardError out.log
Start-Sleep -Seconds 10
$p.CloseMainWindow() | Out-Null; $p.WaitForExit(20000) | Out-Null
```

- PowerShell `2>` 리다이렉트는 servoshell stderr를 놓쳐 **0바이트 로그**가 나오는 경우가 있다.
  파일에 `wall:` 로 시작하는 줄이 없으면 캡처 실패로 판단할 것 — 그 줄들은 `eprintln!`이라
  `RUST_LOG`와 무관하게 항상 출력된다.
- 강제 종료가 아니라 `CloseMainWindow()`로 정상 종료해야 버퍼가 flush된다.
- warn-once 메시지는 시작 직후에 소진되므로 **로그 시작부**를 봐야 한다.
- 로그량 조절: `RUST_LOG='warn'`(경고만, 몇 줄) / `RUST_LOG='warn,paint::dcomp_compositor=info'`
  + `SERVO_DCOMP_DEBUG=1`(서피스 생성·배치 좌표).

## 6. 별건 — 미디어 파일 누락

비디오 프로브 4개 중 **3개는 참조하는 mp4가 리포에 없어 조용히 아무것도 재생하지 않는다**
(`4k_Sample2.mp4`, `4k_3DMark.mp4`, `Longboarding MUKA RAW (1080p 60fps).mp4`).
페이지는 정상으로 보이는데 비디오만 비어 있어서, 비교 대상으로 쓰면 잘못된 결론이 나온다
(실제로 "이 페이지에서만 재현된다"는 오판의 원인이었다). 파일을 채우거나 페이지에 눈에 띄게 표시할 것.
