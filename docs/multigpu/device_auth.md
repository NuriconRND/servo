# 장비 인증 주입 (winit_wall)

SDWall 페이지가 장비 인증 결과를 알 수 있도록, `winit_wall`이 DeviceService(127.0.0.1:7250)에
스스로 물어 결과를 페이지의 `window.sdwallDeviceAuth`로 넣어 준다. 명세:
`servo-handoff/servo-device-auth.md`. 대상 셸은 `winit_wall`뿐이다(servoshell은 바꾸지 않았다).

## 흐름

1. 폴러 스레드가 `GET http://127.0.0.1:7250/device-auth?nonce=<16~64자 영숫자, 매번 새로>`를
   묻는다. 간격 3초. 한도는 **질의 하나 전체에 2초**다(연결·요청 쓰기·응답 읽기가 같은 마감
   시각을 나눠 쓴다. 단계마다 2초가 새로 주어지지 않는다). 인증 정상이 되면 그 자리에서 넘기고 **더 묻지 않는다**.
2. 판정: 서명 맞음 + 응답 `nonce` == 보낸 값 + `state == "ok"` 이면 정상. `invalid`, 서명 틀림,
   nonce 다름, 503은 비정상(`invalid`). 연결 실패·시간 초과는 확인 안 됨(`unreachable`).
   서명은 `state|fingerprint|projectKey|nonce`(UTF-8) 위의 ECDSA P-256/SHA-256, P1363 64바이트 base64.
3. 폴러는 결과를 `WakerEvent::DeviceAuth`로 이벤트 루프에 보낸다. `InjectionTracker`가
   "이 문서에 이 값을 이미 넣었는가"를 들고 있다가, 값이 바뀌었거나 새 문서가 시작되었을 때만
   `evaluate_javascript`로 넣는다. 넣을 때마다 `sdwall-device-auth` 이벤트를 쏜다. 단, 실행되는
   문서에 **같은 값**(`state`와 `projectKey`가 같음)이 이미 있으면 스크립트는 다시 쓰지도 알리지도
   않고 `'same'`을 돌려준다(아래 "알려진 관찰").
4. 페이지 값: `window.sdwallDeviceAuth = { state, projectKey }`, `state` 는
   `"ok" | "invalid" | "unreachable"`, `projectKey`는 `ok`일 때만. `fingerprint`는 페이지로
   넘기지도, 로그에 남기지도 않는다.

새 문서(새로고침·다른 문서 이동)마다 다시 넣고 알린다. `#` 뒤만 바뀌는 이동은 다시 넣지 않는다.

## 범위

- 넘기는 곳은 **시작 URL의 오리진**(운영 `https://<서버 IP>:7245`, 시험 `http://127.0.0.1:7250`)
  아래의 `/ipwall/`, `/portal/` 뿐이다. 별도 설정은 없다. 시작 URL이 `file://` 같은 불투명
  오리진이면 아무 데도 넘기지 않는다.
- **범위를 집행하는 것은 주입 스크립트 안의 가드 하나뿐이다**: `window.top`(최상위 프레임만),
  `location.origin`, 경로. 엔진(Rust) 쪽에서 URL로 미리 거르지 않는다.
  - 이유: `evaluate_javascript`는 내보낸 순간과 실행되는 순간 사이에 문서가 바뀔 수 있고,
    `webview.url()`은 실제로 스크립트를 실행할 문서보다 늦을 수 있다. 엔진에서 거르면 정당한
    주입을 버리고, 그래도 실행 순간의 문서는 보장하지 못한다.
  - 가드는 **값을 실제로 받을 바로 그 문서**에서 돈다. 위조할 수 없는 속성(`window.top`,
    `location.origin`)을 먼저 보고 거기서 끊으므로(short-circuit), 다른 오리진·iframe에는 값을
    쓰지 않는다.
  - `PageScope::contains`(Rust)는 같은 규칙을 시험으로 고정하는 `#[cfg(test)]` 거울일 뿐이다.

## 서명 키

- 공개 키는 **빌드 때 박힌다**. 설정 파일·CLI·현장 값으로 바꿀 수 없다.
- 기본은 실제 키(`response-signing-public.pem`, 공개 키라 저장소에 커밋).
- `SDWALL_DEVICE_AUTH_SIGNING_KEY=test` 로 빌드했을 때만 시험 키. 다른 값이면 빌드가 실패한다.
  시험 **개인** 키는 이 저장소에 없다(`servo-handoff/keys/test`).
- 시험 키 빌드는 fake-auth가 서명한 "인증 정상"을 받아들이므로 **현장에 배포하면 안 된다**.
  `etc/multigpu/make_wall_dist.ps1`이 exe 안의 PEM 본문으로 키 종류를 가르고
  (`etc/multigpu/device_auth_key_kind.ps1`의 `Get-DeviceAuthKeyKind` -> `release|test|unknown`),
  `release`가 아니면 예외로 멈춘다. 실험실용 묶음이 필요하면 `-AllowTestSigningKey`(경고가 난다).
- `unknown`은 exe 안에서 두 PEM 본문(시험 키, 실제 키)이 **둘 다 보이거나 둘 다 안 보일 때**다.
  정상 빌드에서는 나오지 않는다 -- 손댄 exe, 다른 곳에서 온 exe, 키 확인이 들어가기 전의 오래된
  exe 같은 경우다. 무엇으로 서명을 확인하는지 알 수 없으므로 `-AllowTestSigningKey`로도 묶지
  않는다(그 스위치는 `test`만 허용한다). 깨끗이 다시 빌드해야 한다.
  `device_auth_key_kind.ps1`은 묶기 전에만 쓰므로 배포 폴더에는 넣지 않는다.

## 개발자 도구

현장 배포는 개발자 도구를 **끈다**(명세 4절 답, 2026-10-13). `run_wall_dist.ps1`은 `-Devtools`를
주지 않으면 이미 꺼져 있다. `-Devtools <addr>`는 개발 전용이며 경고를 낸다: 켜져 있으면 콘솔에서
`window.sdwallDeviceAuth`를 직접 넣어 `/output`을 표출시킬 수 있다. 바이너리에서 강제로 막지는
않는다(개발 중 디버깅을 남기기 위해).

## 빌드·실행 환경 메모 (실행 중 알게 된 것)

- 릴리스 빌드는 **W: subst 드라이브**(`W:\` = 프로젝트 루트)에서 해야 한다. F: 경로에서는
  mozangle이 `Os error 206`(명령줄이 너무 김)으로 실패한다.
- `pkg-config`가 PATH에 있어야 한다(GStreamer `bin`을 PATH 앞에 둔다). 없으면 glib-sys가 실패한다.
- 점검 스크립트는 exe를 ANGLE DLL과 GStreamer 플러그인과 함께 따로 모아서(staging) 실행한다.
  exe만 복사하면 DLL을 못 찾는다.

## 확인 (모두 자동, 사람 개입 없음)

```powershell
cargo test -p sdwall-device-auth --lib
cargo test -p sdwall-device-auth --lib -- --ignored live_fake_auth_repeated   # fake-auth 필요
etc\multigpu\device_auth_check.ps1   # 시험 키 빌드에서 1회, 실제 키 빌드에서 1회
```

`device_auth_check.ps1`은 경우별로 fake-auth를 띄우고 확인 페이지
(`http://127.0.0.1:7250/ipwall/device-auth-check`)의 콘솔 로그를 정확한 개수로 판정한다.
2026-10-13 최종 실행 결과(같은 주입 무시(`'same'`)·종료 코드 수정 이후):

시험 키 빌드(`key=test`), 연속 2회 모두 통과, 종료 코드 0:

| 경우 | 결과 | 내용 |
| --- | --- | --- |
| ok | PASS | notices=3 expected=3 (`{"state":"ok","projectKey":"123412"}`), 자가 시험 PASS, exit=0 |
| invalid | PASS | notices=3 expected=3 (`{"state":"invalid"}`), 자가 시험 PASS, exit=0 |
| unreachable | PASS | notices=3 expected=3 (`{"state":"unreachable"}`), 자가 시험 PASS, exit=0 |
| unreachable_to_ok | PASS | unreachable@0 -> ok@1 (8초 뒤 fake-auth 기동) |

실제 키 빌드(`key=release`), 연속 2회 모두 통과, 종료 코드 0:

| 경우 | 결과 | 내용 |
| --- | --- | --- |
| signature_mismatch | PASS | invalid=1 ok=0 (시험 서명이 실제 키로는 맞지 않음) |
| selftest_rejected | PASS | exit=1, `unknown argument: --device-auth-selftest` |

### 자가 시험 `--device-auth-selftest`

**시험 키 빌드 전용**이다. 실제 키 빌드는 모르는 인자로 거부한다. 순서와 판정:

1. 첫 문서에 값이 들어가면 새로고침한다.
2. 새 문서에 값이 들어가면 `?n=<n+1>` 로 이동한다.
3. 새 문서에 값이 들어가면 `#`만 바꾸고 2초 기다린다(`fragment-settled`).

판정: 문서 3개에 값이 들어갔고, 마지막 구간(`#` 이동 뒤)에 값이 **또** 들어가지 않으면 `PASS`.
더 들어가면(명세 2절 위반) 또는 45초 안에 끝나지 않으면 `FAIL`. 결과는 `device-auth selftest:
PASS documents=3`(또는 `FAIL <이유>`) 로그와 **종료 코드**(PASS 0, FAIL 1)로 나온다.
`#` 구간에 스크립트가 `'same'`을 돌려줘도(알림은 없지만 다시 넘기려 한 것) `FAIL`로 센다.

다음 단계 스크립트는 평가 콜백 안에서 바로 실행하지 않고 `about_to_wait`로 미룬다. 콜백 안에서
`evaluate_javascript`를 부르면 Servo가 평가기 borrow를 쥔 채여서 재진입 패닉이 났기 때문이다
(수정 완료).

### 알려진 관찰: 한 문서에 두 번 들어가던 경우 (수정됨)

`signature_mismatch`에서 `invalid` 알림이 한 문서에 두 번 나온 적이 있었다. 원인:

1. WebView는 URL 없이 만들어지므로 about:blank가 먼저 뜬다. 그 문서의 HeadParsed/Complete로
   `InjectionTracker`가 문서 0에 대해 "넣을 준비"가 된다.
2. 실제 문서가 시작되면 그 `LoadStatus::Started`는 `spin_event_loop` 안에서야 셸에 온다. 그런데
   콘스텔레이션은 그 전에 실제 문서를 이미 활성화(ActivateDocument)했을 수 있다.
3. 그 사이에 `WakerEvent::DeviceAuth`가 처리되면 추적기는 아직 문서 0이라 생각하고 넣는데, 실제로
   실행하는 것은 새 문서다(#1). 뒤이어 Started+HeadParsed가 오면 새 문서라고 또 넣는다(#2).

명세상 틀린 동작은 아니지만(새 문서에 넣고 알림) 정확한 개수 판정이 흔들린다. 셸은 스크립트를
어느 문서가 실행할지 확실히 알 수 없으므로, **실행하는 문서가 스스로 거른다**: 주입 스크립트는
범위 가드 뒤에서 `window.sdwallDeviceAuth`에 이미 같은 `state`·`projectKey`(ok가 아니면 둘 다
없음)가 있으면 쓰지도 이벤트를 쏘지도 않고 `'same'`을 돌려준다. 값이 다르면(예: unreachable ->
ok) 그대로 쓰고 알린다. 셸은 `inject <state> -> set|same|skip`을 info 로그로 남긴다. 이 경합은
드물어서 최종 실행(실제 키 6회, `RUST_LOG=warn,winit_wall=info`)에서는 모두 `-> set` 한 번씩만
나왔다.
