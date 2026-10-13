# 장비 인증 결과 주입 (Device Auth Injection) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 배포 엔진(`winit_wall`)이 같은 장비의 DeviceService에 장비 인증을 묻고, ECDSA P-256 서명을 확인한 결과를 SDWall 페이지(`/ipwall/`, `/portal/`)에 `window.sdwallDeviceAuth` + `sdwall-device-auth` 이벤트로 넘긴다.

**Architecture:** 엔진 비의존 순수 로직(묻기·서명 확인·폴링·주입 스크립트·문서 추적)을 새 크레이트 `components/sdwall_device_auth`(`sdwall-device-auth`)에 두고 단위 시험으로 고정한다. `winit_wall`은 백그라운드 폴러 스레드 → `EventLoopProxy` → 메인 스레드로 결과를 받고, `WebViewDelegate::notify_load_status_changed`로 새 문서를 감지해 `WebView::evaluate_javascript`로 주입한다. 공개 키는 빌드 시 `include_str!`로 박히며 빌드 환경 변수 `SDWALL_DEVICE_AUTH_SIGNING_KEY=test`일 때만 시험 키가 들어간다.

**Tech Stack:** Rust 2024, `p256` 0.13(ecdsa/pem), `base64` 0.22, `rand` 0.9, `serde`/`serde_json`, `url`, std `TcpStream`(HTTP/1.0 수제 GET — 새 HTTP 크레이트 없음), PowerShell(배포 스크립트 가드).

**Spec:** `F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser\servo-handoff\servo-device-auth.md` (정본: Claude Docs "장비 유효성 설계" — "장비 인증" 탭). 시험 값: `servo-handoff\keys\test\` (`README.md`, `device-auth-vectors.json`).

## Global Constraints

- 주소: `GET http://127.0.0.1:7250/device-auth?nonce=<무작위 값>` — 포트 7250 고정.
- 무작위 값: 영문자·숫자 16~64자, 물을 때마다 새로 만든다.
- 서명 문자열: `state|fingerprint|projectKey|nonce`, UTF-8, 응답 값 그대로 이어 붙인다.
- 서명: ECDSA P-256 / SHA-256, IEEE P1363(r‖s 64바이트) base64. `Signature::from_slice`로 읽는다(`from_der` 아님).
- 공개 키는 **빌드할 때 넣는다**. 설정 파일·CLI·현장 값으로 바꿀 수 없어야 한다.
- 인증 정상 = 서명 맞음 ∧ 응답 `nonce` == 보낸 값 ∧ `state == "ok"`. 인증 비정상 = `invalid`·서명 틀림·nonce 다름·503. 인증 확인 안 됨 = 연결 실패·시간 초과.
- 인증 정상이 아닌 동안 몇 초(3초)마다 다시 묻는다. 정상이 되면 그 자리에서 넘기고 더 묻지 않는다.
- `fingerprint`는 페이지로 넘기지 않는다(로그에도 남기지 않는다).
- 페이지 값: `window.sdwallDeviceAuth = { state, projectKey }`, `state` ∈ `"ok"|"invalid"|"unreachable"`, `projectKey`는 `ok`일 때만.
- 값을 넣거나 바꿀 때마다 `window.dispatchEvent(new Event('sdwall-device-auth'))`.
- 새 문서(새로고침·이동)마다 다시 넣고 알린다. `#` 뒤만 바뀌는 이동은 해당 없다.
- 넘기는 곳은 SDWall 페이지(`/ipwall/`, `/portal/` 아래)뿐이고 다른 오리진에는 넘기지 않는다.
- 엔진 변경 완료 조건(CLAUDE.md): `cargo check`/`build` 통과, `rustfmt --edition 2024 --check <touched .rs>`, `git diff --check`.

## 정한 것 (2026-10-13 사용자 확인)

1. **SDWall 오리진 = 시작 URL의 오리진.** 운영 `https://<서버 IP>:7245/ipwall/#/output`, 시험 `http://127.0.0.1:7250/ipwall/device-auth-check`가 모두 시작 URL이므로 별도 설정 없이 맞는다. 시작 URL이 `file://` 등 불투명 오리진이면 아무 데도 넘기지 않는다.
2. **대상 셸은 `winit_wall`만.** servoshell은 바꾸지 않는다.
3. **확인은 사람 없이 자동으로 돈다.** `winit_wall` 화면은 상호작용이 없다. 새로고침·다른 문서 이동·`#` 이동을 엔진이 스스로 하는 자가 시험(`--device-auth-selftest`, **시험 키 빌드에서만 받는 인자**)과, 경우별로 fake-auth를 띄우고 로그를 판정하는 스크립트(`etc/multigpu/device_auth_check.ps1`)로 확인한다.
4. **현장에서는 개발자 도구를 끈다(명세 4절 답).** `run_wall_dist.ps1`은 `-Devtools`를 주지 않으면 이미 꺼져 있다. 이 계획은 그 사실을 문서와 배포 스크립트 안내에 남기고, 바이너리에서 강제로 막지는 않는다(개발 중 `-Devtools` 디버깅을 남기기 위해).
5. 실제 공개 키(`response-signing-public.pem`)를 servo 저장소에 커밋한다(공개 키라 문제없음). 시험 **개인** 키는 servo 저장소에 넣지 않는다.

확인된 사실 (2026-10-13 실측): fake-auth는 HTTP/1.1 요청에는 `Transfer-Encoding: chunked`로, HTTP/1.0 요청에는 `Connection: close` + 본문 그대로 답한다(상태 줄은 `HTTP/1.1 200 OK`). 확인 페이지는 받을 때마다 `console.log('[device-auth-check]', when, text, shape)`를 남기고(`when` = `열 때`|`알림`, `shape` = `없음`|`맞음`|`모양이 다름`), "다른 문서로 이동"은 `?n=<n+1>`이다.

## Review Focus

- **.NET이 만든 high-S 서명** — DeviceService(.NET)는 low-S 정규화를 하지 않으므로 응답의 약 절반이 high-S다. `p256` 검증이 이를 거부하면 "가끔 invalid"가 된다. Task 3의 `live_fake_auth_repeated`(30회 연속 `Ok`)가 고정한다.
- **Kestrel 응답 프레이밍** — HTTP/1.1이면 chunked로 올 수 있다. HTTP/1.0 요청으로 EOF까지 읽는다. 같은 라이브 시험이 실제 DeviceService로 고정한다.
- **연결은 받고 답하지 않는 DeviceService**(기동 중·멈춤) — 2초 안에 `Unreachable`이 되고 폴링이 계속돼야 한다. Task 2 `silent_server_is_unreachable`.
- **`HeadParsed` 전에 다시 이동 / 주입 실패** — `Started→Started`는 `WebView::set_load_status`가 삼킨다. 새 문서가 값을 못 받는 일이 없어야 한다. Task 5 추적기 시험.
- **범위 밖 페이지** — 다른 오리진의 같은 경로, `/ipwall`(슬래시 없음), `/ipwallx/`, iframe. Task 4 범위 시험과 주입 JS의 자체 가드(`window.top`, `location.origin`, 경로)로 고정한다. `projectKey`에 따옴표가 와도 JS가 깨지지 않아야 한다(`project_key_is_json_escaped`).

## File Structure

| 파일 | 책임 |
|---|---|
| `components/sdwall_device_auth/Cargo.toml` | 새 크레이트 정의 |
| `components/sdwall_device_auth/build.rs` | `SDWALL_DEVICE_AUTH_SIGNING_KEY` → `cfg(sdwall_test_signing_key)` |
| `components/sdwall_device_auth/lib.rs` | 모듈 선언과 공개 재노출 |
| `components/sdwall_device_auth/key.rs` | 빌드 시 박힌 공개 키, `SIGNING_KEY_KIND` |
| `components/sdwall_device_auth/verify.rs` | `AuthState`, `DeviceAuthResponse`, `verify_response` |
| `components/sdwall_device_auth/client.rs` | nonce, HTTP/1.0 GET, `query_device_auth` |
| `components/sdwall_device_auth/poller.rs` | `run_poll_loop`, `spawn_poller` |
| `components/sdwall_device_auth/page.rs` | `PageScope`(범위 판정), `injection_script` |
| `components/sdwall_device_auth/tracker.rs` | `InjectionTracker`(문서별 주입 시점) |
| `components/sdwall_device_auth/keys/response-signing-public.pem` | 실제 공개 키 (handoff에서 복사) |
| `components/sdwall_device_auth/keys/test/response-signing-test-public.pem` | 시험 공개 키 (복사) |
| `components/sdwall_device_auth/keys/test/device-auth-vectors.json` | 시험 벡터 6개 (복사) |
| `Cargo.toml` (워크스페이스) | `[workspace.dependencies]`에 크레이트 등록 |
| `components/servo/Cargo.toml` | `[dev-dependencies]`에 추가 (winit_wall은 example) |
| `components/servo/examples/winit_wall/main.rs` | 폴러 기동, `WakerEvent::DeviceAuth`, 델리게이트, 주입, 자가 시험 배선 |
| `components/servo/examples/winit_wall/device_auth_selftest.rs` | 자가 시험 순서(새로고침→이동→`#`)와 판정, 순수 로직 |
| `etc/multigpu/device_auth_key_kind.ps1` | exe에 박힌 키 종류(`release`/`test`)를 가르는 공용 함수 |
| `etc/multigpu/device_auth_check.ps1` | 경우별 fake-auth 기동 → 엔진 실행 → 로그 판정 (자동) |
| `etc/multigpu/make_wall_dist.ps1` | 시험 키로 빌드된 exe 배포 거부 |
| `docs/multigpu/device_auth.md` | 빌드·시험 방법 기록 |

---

### Task 1: 크레이트 골격, 빌드 시 공개 키, 서명 확인

**Files:**
- Create: `components/sdwall_device_auth/{Cargo.toml,build.rs,lib.rs,key.rs,verify.rs}`
- Create: `components/sdwall_device_auth/keys/...` (3개 파일 복사)
- Modify: `Cargo.toml` (`[workspace.dependencies]`, 알파벳 순서 자리)

**Interfaces:**
- Produces:
  - `pub enum AuthState { Ok { project_key: String }, Invalid, Unreachable }` (`Clone, Debug, PartialEq, Eq`), `impl AuthState { pub fn label(&self) -> &'static str }` → `"ok"|"invalid"|"unreachable"`
  - `pub struct DeviceAuthResponse { pub state, pub fingerprint, pub project_key, pub nonce, pub signature: String }` (serde camelCase)
  - `pub fn verify_response(key: &VerifyingKey, sent_nonce: &str, response: &DeviceAuthResponse) -> AuthState` (`Ok` 또는 `Invalid`만 돌려준다)
  - `pub fn parse_public_key_pem(pem: &str) -> Result<VerifyingKey, String>`
  - `pub fn embedded_verifying_key() -> VerifyingKey`
  - `pub const SIGNING_KEY_KIND: &str` (`"release"` | `"test"`)
  - `pub use p256::ecdsa::VerifyingKey;`

- [ ] **Step 1: 키·벡터 복사**

```powershell
$h = "..\servo-handoff\keys"; $k = "components\sdwall_device_auth\keys"
New-Item -ItemType Directory -Force "$k\test" | Out-Null
Copy-Item "$h\response-signing-public.pem" "$k\"
Copy-Item "$h\test\response-signing-test-public.pem","$h\test\device-auth-vectors.json" "$k\test\"
```
개인 키(`response-signing-test-private.pem`)는 복사하지 않는다.

- [ ] **Step 2: Cargo.toml / build.rs / lib.rs 작성**

`components/sdwall_device_auth/Cargo.toml`:
```toml
[package]
name = "sdwall-device-auth"
version.workspace = true
authors.workspace = true
license.workspace = true
edition.workspace = true
rust-version.workspace = true
publish = false
description = "SDWall device auth: ask DeviceService, verify its signature, hand the result to the page."

[lib]
name = "sdwall_device_auth"
path = "lib.rs"

[dependencies]
base64 = { workspace = true }
log = { workspace = true }
p256 = { workspace = true, features = ["ecdsa", "pem"] }
rand = { workspace = true }
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }
url = { workspace = true }
```

워크스페이스 `Cargo.toml` `[workspace.dependencies]`(알파벳 자리, `paint_api` 줄처럼 path 의존 블록 쪽):
```toml
sdwall_device_auth = { package = "sdwall-device-auth", path = "components/sdwall_device_auth" }
```

`build.rs`:
```rust
//! The response-signing public key is chosen at build time and nowhere else.

fn main() {
    println!("cargo::rerun-if-env-changed=SDWALL_DEVICE_AUTH_SIGNING_KEY");
    println!("cargo::rustc-check-cfg=cfg(sdwall_test_signing_key)");
    match std::env::var("SDWALL_DEVICE_AUTH_SIGNING_KEY").as_deref() {
        Err(_) | Ok("") | Ok("release") => {},
        Ok("test") => println!("cargo::rustc-cfg=sdwall_test_signing_key"),
        Ok(other) => panic!(
            "SDWALL_DEVICE_AUTH_SIGNING_KEY must be `release` or `test`, got `{other}`"
        ),
    }
}
```

`lib.rs`:
```rust
//! SDWall 장비 인증: DeviceService 에 묻고, 서명을 확인하고, 결과를 페이지로 넘긴다.
//!
//! 명세: servo-handoff/servo-device-auth.md. 엔진에 의존하지 않는 부분만 여기 둔다 --
//! 셸은 폴러를 띄우고, 문서 이벤트를 `InjectionTracker` 에 넣고, 나온 스크립트를 실행한다.

mod key;
mod verify;

pub use key::{SIGNING_KEY_KIND, embedded_verifying_key, parse_public_key_pem};
pub use p256::ecdsa::VerifyingKey;
pub use verify::{AuthState, DeviceAuthResponse, verify_response};
```

- [ ] **Step 3: 실패하는 시험 작성 (`verify.rs` 하단, `key.rs` 하단)**

`verify.rs` 시험 모듈:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_public_key_pem;

    const TEST_PEM: &str = include_str!("keys/test/response-signing-test-public.pem");
    const VECTORS: &str = include_str!("keys/test/device-auth-vectors.json");

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Vector { name: String, sent_nonce: String, expected: String, response: DeviceAuthResponse }

    #[derive(serde::Deserialize)]
    struct Vectors { vectors: Vec<Vector> }

    #[test]
    fn all_six_vectors_match_expected() {
        let key = parse_public_key_pem(TEST_PEM).unwrap();
        let vectors: Vectors = serde_json::from_str(VECTORS).unwrap();
        assert_eq!(vectors.vectors.len(), 6);
        for v in vectors.vectors {
            let got = verify_response(&key, &v.sent_nonce, &v.response);
            assert_eq!(got.label(), v.expected, "vector `{}`", v.name);
            if v.expected == "ok" {
                assert_eq!(got, AuthState::Ok { project_key: v.response.project_key.clone() });
            }
        }
    }

    #[test]
    fn garbage_signature_is_invalid() {
        let key = parse_public_key_pem(TEST_PEM).unwrap();
        let mut r: Vectors = serde_json::from_str(VECTORS).unwrap();
        let mut resp = r.vectors.remove(0).response;
        for sig in ["", "not base64!", "AAAA", &"A".repeat(200)] {
            resp.signature = sig.to_owned();
            assert_eq!(verify_response(&key, "a1B2c3D4e5F6g7H8", &resp), AuthState::Invalid, "{sig}");
        }
    }

    #[test]
    fn release_key_rejects_test_signed_ok() {
        // 명세 3절 "서명 틀림": 실제 공개 키로는 시험 키 서명이 통과하지 않는다.
        let key = parse_public_key_pem(include_str!("keys/response-signing-public.pem")).unwrap();
        let mut r: Vectors = serde_json::from_str(VECTORS).unwrap();
        let v = r.vectors.remove(0);
        assert_eq!(verify_response(&key, &v.sent_nonce, &v.response), AuthState::Invalid);
    }
}
```

`key.rs` 시험 모듈:
```rust
#[cfg(test)]
mod tests {
    #[test]
    fn embedded_key_parses() {
        let _ = super::embedded_verifying_key();
        assert!(matches!(super::SIGNING_KEY_KIND, "release" | "test"));
    }
}
```

- [ ] **Step 4: 실패 확인**

Run: `cargo test -p sdwall-device-auth --lib`
Expected: 컴파일 실패 (`verify_response` 등 없음)

- [ ] **Step 5: 구현**

`key.rs`:
```rust
use p256::ecdsa::VerifyingKey;
use p256::pkcs8::DecodePublicKey;

// ***빌드할 때 넣는다.*** 설정 파일이나 현장 값으로 바꿀 길을 만들지 않는다 -- 그 길이
// 있으면 누구나 자기 키로 서명한 "인증 정상"을 넣을 수 있다.
#[cfg(not(sdwall_test_signing_key))]
const EMBEDDED_PEM: &str = include_str!("keys/response-signing-public.pem");
#[cfg(sdwall_test_signing_key)]
const EMBEDDED_PEM: &str = include_str!("keys/test/response-signing-test-public.pem");

/// 이 빌드에 박힌 키가 무엇인지. 시험 키 빌드를 배포에서 걸러내는 데 쓴다.
pub const SIGNING_KEY_KIND: &str = if cfg!(sdwall_test_signing_key) { "test" } else { "release" };

pub fn parse_public_key_pem(pem: &str) -> Result<VerifyingKey, String> {
    VerifyingKey::from_public_key_pem(pem).map_err(|error| error.to_string())
}

pub fn embedded_verifying_key() -> VerifyingKey {
    parse_public_key_pem(EMBEDDED_PEM).expect("embedded device-auth public key must parse")
}
```

`verify.rs`:
```rust
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use serde::Deserialize;

/// 명세 1절 "결과 셋".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthState {
    /// 서명이 맞고, nonce 가 같고, `state == "ok"`.
    Ok { project_key: String },
    /// `invalid`, 서명 틀림, nonce 다름, 503 (그 밖의 응답 이상 포함).
    Invalid,
    /// DeviceService 가 응답하지 않음 (연결 실패, 시간 초과).
    Unreachable,
}

impl AuthState {
    pub fn label(&self) -> &'static str {
        match self {
            AuthState::Ok { .. } => "ok",
            AuthState::Invalid => "invalid",
            AuthState::Unreachable => "unreachable",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceAuthResponse {
    pub state: String,
    pub fingerprint: String,
    pub project_key: String,
    pub nonce: String,
    pub signature: String,
}

pub fn verify_response(
    key: &VerifyingKey,
    sent_nonce: &str,
    response: &DeviceAuthResponse,
) -> AuthState {
    // 응답에 온 값을 그대로 잇는다 -- 정규화하면 서명과 어긋난다.
    let message = format!(
        "{}|{}|{}|{}",
        response.state, response.fingerprint, response.project_key, response.nonce
    );
    let Ok(bytes) = STANDARD.decode(&response.signature) else {
        return AuthState::Invalid;
    };
    // P1363(r‖s 64바이트)이다. DER 이 아니다.
    let Ok(signature) = Signature::from_slice(&bytes) else {
        return AuthState::Invalid;
    };
    if key.verify(message.as_bytes(), &signature).is_err() ||
        response.nonce != sent_nonce ||
        response.state != "ok"
    {
        return AuthState::Invalid;
    }
    AuthState::Ok {
        project_key: response.project_key.clone(),
    }
}
```

`p256::pkcs8` 경로가 안 잡히면 `p256 = { workspace = true, features = ["ecdsa", "pem", "pkcs8"] }`로 기능을 더한다.

- [ ] **Step 6: 통과 확인**

Run: `cargo test -p sdwall-device-auth --lib`
Expected: 4 passed. 이어서 `$env:SDWALL_DEVICE_AUTH_SIGNING_KEY='test'; cargo test -p sdwall-device-auth --lib embedded; Remove-Item Env:SDWALL_DEVICE_AUTH_SIGNING_KEY` 도 PASS (시험 키 빌드 경로가 컴파일되는지 확인). `$env:SDWALL_DEVICE_AUTH_SIGNING_KEY='bogus'`이면 build.rs 패닉으로 빌드 실패하는지 한 번 확인하고 지운다.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock components/sdwall_device_auth
git commit -m "feat(device-auth): 응답 서명을 빌드에 박힌 공개 키로 확인한다

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: nonce와 DeviceService 묻기 (HTTP/1.0 클라이언트)

**Files:**
- Create: `components/sdwall_device_auth/client.rs`
- Modify: `components/sdwall_device_auth/lib.rs`

**Interfaces:**
- Consumes: `AuthState`, `DeviceAuthResponse`, `verify_response`, `VerifyingKey` (Task 1)
- Produces:
  - `pub const DEVICE_SERVICE_ADDR: &str = "127.0.0.1:7250";`
  - `pub fn new_nonce() -> String` (영숫자 32자)
  - `pub fn query_device_auth(addr: SocketAddr, key: &VerifyingKey, timeout: Duration) -> AuthState`
  - (모듈 내부) `enum HttpOutcome { Status { code: u16, body: Vec<u8> }, Unreachable, Malformed }`, `fn http_get(addr, path_and_query, timeout) -> HttpOutcome`

판정 규칙: 상태 줄을 받기 전에 연결 실패/리셋/EOF/시간 초과 → `Unreachable`. 상태 줄을 받았으면: 200 + 파싱되는 JSON → `verify_response`; 200 + 깨진 JSON → `Invalid`; 그 밖의 코드(503 포함) → `Invalid`; 상태 줄이 HTTP 형식이 아니거나 64 KiB 초과 → `Invalid`(`Malformed`).

- [ ] **Step 1: 실패하는 시험 작성 (`client.rs` 하단)**

시험용 가짜 서버는 고정 32바이트 개인 키로 RFC6979 서명한다(난수 불필요). 서명 키는 시험 안에서만 만든다.

```rust
#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use p256::ecdsa::signature::Signer;
    use p256::ecdsa::{Signature, SigningKey};

    use super::*;

    fn signing_key() -> SigningKey {
        SigningKey::from_slice(&[7u8; 32]).unwrap()
    }

    /// 요청 한 개를 받고 `respond(nonce)` 가 만든 바이트를 그대로 쓰고 닫는 서버.
    fn serve_once(respond: impl FnOnce(String) -> Vec<u8> + Send + 'static) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let nonce = request
                .split("nonce=").nth(1).unwrap_or("")
                .split(|c: char| c == ' ' || c == '&').next().unwrap_or("").to_owned();
            let _ = stream.write_all(&respond(nonce));
        });
        addr
    }

    fn signed_body(state: &str, project_key: &str, nonce: &str) -> String {
        let message = format!("{state}|HJY0BWGD|{project_key}|{nonce}");
        let signature: Signature = signing_key().sign(message.as_bytes());
        let signature = STANDARD.encode(signature.to_bytes());
        format!(r#"{{"state":"{state}","fingerprint":"HJY0BWGD","projectKey":"{project_key}","nonce":"{nonce}","signature":"{signature}"}}"#)
    }

    fn http(status: &str, body: &str) -> Vec<u8> {
        format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\r\n{body}").into_bytes()
    }

    const T: Duration = Duration::from_millis(500);

    #[test]
    fn nonce_is_fresh_alphanumeric_within_bounds() {
        let a = new_nonce();
        let b = new_nonce();
        assert!((16..=64).contains(&a.len()));
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, b);
    }

    #[test]
    fn signed_ok_is_ok() {
        let addr = serve_once(|n| http("200 OK", &signed_body("ok", "123412", &n)));
        let key = *signing_key().verifying_key();
        assert_eq!(query_device_auth(addr, &key, T), AuthState::Ok { project_key: "123412".into() });
    }

    #[test]
    fn signed_invalid_is_invalid() {
        let addr = serve_once(|n| http("200 OK", &signed_body("invalid", "", &n)));
        assert_eq!(query_device_auth(addr, signing_key().verifying_key(), T), AuthState::Invalid);
    }

    #[test]
    fn echoing_a_stale_nonce_is_invalid() {
        let addr = serve_once(|_| http("200 OK", &signed_body("ok", "123412", "Z9y8X7w6V5u4T3s2")));
        assert_eq!(query_device_auth(addr, signing_key().verifying_key(), T), AuthState::Invalid);
    }

    #[test]
    fn status_503_is_invalid() {
        let addr = serve_once(|_| http("503 Service Unavailable", ""));
        assert_eq!(query_device_auth(addr, signing_key().verifying_key(), T), AuthState::Invalid);
    }

    #[test]
    fn broken_json_is_invalid() {
        let addr = serve_once(|_| http("200 OK", "{not json"));
        assert_eq!(query_device_auth(addr, signing_key().verifying_key(), T), AuthState::Invalid);
    }

    #[test]
    fn non_http_reply_is_invalid() {
        let addr = serve_once(|_| b"SSH-2.0-OpenSSH\r\n\r\n".to_vec());
        assert_eq!(query_device_auth(addr, signing_key().verifying_key(), T), AuthState::Invalid);
    }

    #[test]
    fn closed_port_is_unreachable() {
        let addr = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap(); // 리스너는 곧바로 닫힌다
        assert_eq!(query_device_auth(addr, signing_key().verifying_key(), T), AuthState::Unreachable);
    }

    #[test]
    fn silent_server_is_unreachable() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _keep = thread::spawn(move || {
            let _conn = listener.accept();
            thread::sleep(Duration::from_secs(3));
        });
        let started = std::time::Instant::now();
        assert_eq!(query_device_auth(addr, signing_key().verifying_key(), T), AuthState::Unreachable);
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[test]
    fn closed_before_status_line_is_unreachable() {
        let addr = serve_once(|_| Vec::new());
        assert_eq!(query_device_auth(addr, signing_key().verifying_key(), T), AuthState::Unreachable);
    }
}
```

`lib.rs`에 `mod client;` 와 `pub use client::{DEVICE_SERVICE_ADDR, new_nonce, query_device_auth};` 추가.

- [ ] **Step 2: 실패 확인**

Run: `cargo test -p sdwall-device-auth --lib client`
Expected: 컴파일 실패 (`query_device_auth` 없음)

- [ ] **Step 3: 구현 (`client.rs` 본문)**

```rust
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use rand::Rng;
use rand::distr::Alphanumeric;

use crate::{AuthState, DeviceAuthResponse, VerifyingKey, verify_response};

/// 명세 1절. 포트는 고정이다 -- 설정으로 열어 두면 다른 프로세스가 대신 답할 수 있다.
pub const DEVICE_SERVICE_ADDR: &str = "127.0.0.1:7250";

const NONCE_LEN: usize = 32;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

pub fn new_nonce() -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(NONCE_LEN)
        .map(char::from)
        .collect()
}

pub fn query_device_auth(addr: SocketAddr, key: &VerifyingKey, timeout: Duration) -> AuthState {
    let nonce = new_nonce();
    match http_get(addr, &format!("/device-auth?nonce={nonce}"), timeout) {
        HttpOutcome::Unreachable => AuthState::Unreachable,
        HttpOutcome::Malformed => AuthState::Invalid,
        HttpOutcome::Status { code: 200, body } => {
            match serde_json::from_slice::<DeviceAuthResponse>(&body) {
                Ok(response) => verify_response(key, &nonce, &response),
                Err(_) => AuthState::Invalid,
            }
        },
        // 503 = 서명 키 없이 빌드된 DeviceService. 그 밖의 코드도 "답은 했는데 정상이 아니다".
        HttpOutcome::Status { .. } => AuthState::Invalid,
    }
}

enum HttpOutcome {
    Status { code: u16, body: Vec<u8> },
    Unreachable,
    Malformed,
}

/// HTTP/1.0 GET, EOF 까지 읽는다.
///
/// ***1.0 인 이유:*** 1.1 이면 Kestrel 이 chunked 로 답할 수 있고, 그러면 청크 해석이
/// 필요하다. 1.0 요청에는 서버가 chunked 를 쓰면 안 되므로 연결이 닫힐 때까지 읽으면 된다.
fn http_get(addr: SocketAddr, path_and_query: &str, timeout: Duration) -> HttpOutcome {
    let deadline = Instant::now() + timeout;
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, timeout) else {
        return HttpOutcome::Unreachable;
    };
    let request = format!(
        "GET {path_and_query} HTTP/1.0\r\nHost: {addr}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    if stream.set_write_timeout(Some(timeout)).is_err() ||
        stream.write_all(request.as_bytes()).is_err()
    {
        return HttpOutcome::Unreachable;
    }

    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || stream.set_read_timeout(Some(remaining)).is_err() {
            return HttpOutcome::Unreachable;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                data.extend_from_slice(&chunk[..n]);
                if data.len() > MAX_RESPONSE_BYTES {
                    return HttpOutcome::Malformed;
                }
            },
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            // 시간 초과·리셋: 아무것도 못 받았으면 "응답하지 않음"이다.
            Err(_) => return HttpOutcome::Unreachable,
        }
    }
    parse_response(&data)
}

fn parse_response(data: &[u8]) -> HttpOutcome {
    if data.is_empty() {
        return HttpOutcome::Unreachable;
    }
    let Some(header_end) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
        return HttpOutcome::Malformed;
    };
    let head = String::from_utf8_lossy(&data[..header_end]);
    let status_line = head.lines().next().unwrap_or("");
    let mut parts = status_line.split_whitespace();
    let (Some(version), Some(code)) = (parts.next(), parts.next()) else {
        return HttpOutcome::Malformed;
    };
    let Ok(code) = code.parse::<u16>() else {
        return HttpOutcome::Malformed;
    };
    if !version.starts_with("HTTP/1.") {
        return HttpOutcome::Malformed;
    }
    HttpOutcome::Status {
        code,
        body: data[header_end + 4..].to_vec(),
    }
}
```

주의: 상태 줄을 받은 **뒤에** 시간이 초과돼도(본문이 끝나지 않음) `Unreachable`이다. 명세의 "시간 초과"에 해당하기 때문이다.

- [ ] **Step 4: 통과 확인**

Run: `cargo test -p sdwall-device-auth --lib`
Expected: Task 1의 4개 + 10개 모두 PASS, 경고 없음.

- [ ] **Step 5: Commit**

```bash
git add components/sdwall_device_auth
git commit -m "feat(device-auth): DeviceService 에 무작위 값으로 묻고 결과를 셋으로 가른다

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: 폴링 루프와 실제 DeviceService 확인

**Files:**
- Create: `components/sdwall_device_auth/poller.rs`
- Modify: `components/sdwall_device_auth/lib.rs`

**Interfaces:**
- Consumes: `query_device_auth`, `embedded_verifying_key`, `DEVICE_SERVICE_ADDR` (Task 1·2)
- Produces:
  - `pub const RETRY_INTERVAL: Duration` (3초), `pub const QUERY_TIMEOUT: Duration` (2초)
  - `pub fn run_poll_loop(query: impl FnMut() -> AuthState, sleep: impl FnMut(Duration), on_change: impl FnMut(AuthState), interval: Duration)` — 바뀔 때만 `on_change`, `Ok`면 끝.
  - `pub fn spawn_poller(on_change: impl FnMut(AuthState) + Send + 'static) -> std::thread::JoinHandle<()>`

- [ ] **Step 1: 실패하는 시험 작성 (`poller.rs` 하단)**

```rust
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    fn ok() -> AuthState {
        AuthState::Ok { project_key: "123412".into() }
    }

    fn run(script: Vec<AuthState>) -> (Vec<AuthState>, usize) {
        let mut queue: VecDeque<_> = script.into();
        let mut changes = Vec::new();
        let mut sleeps = 0;
        run_poll_loop(
            || queue.pop_front().expect("polled after Ok"),
            |d| { assert_eq!(d, RETRY_INTERVAL); sleeps += 1 },
            |s| changes.push(s),
            RETRY_INTERVAL,
        );
        (changes, sleeps)
    }

    #[test]
    fn reports_only_changes_and_stops_at_ok() {
        use AuthState::*;
        let (changes, sleeps) = run(vec![Unreachable, Unreachable, Invalid, Invalid, Unreachable, ok()]);
        assert_eq!(changes, vec![Unreachable, Invalid, Unreachable, ok()]);
        assert_eq!(sleeps, 5);
    }

    #[test]
    fn ok_first_try_reports_once_without_sleeping() {
        assert_eq!(run(vec![ok()]), (vec![ok()], 0));
    }

    /// 실제 DeviceService 로 확인한다 (Review Focus: high-S 서명, Kestrel 프레이밍).
    /// 먼저 띄운다: DeviceService.exe fake-auth --state ok --project-key 123412
    /// 실행: cargo test -p sdwall-device-auth --lib -- --ignored live_fake_auth_repeated
    #[test]
    #[ignore]
    fn live_fake_auth_repeated() {
        let key = crate::parse_public_key_pem(include_str!(
            "keys/test/response-signing-test-public.pem"
        ))
        .unwrap();
        let addr = crate::DEVICE_SERVICE_ADDR.parse().unwrap();
        for i in 0..30 {
            assert_eq!(
                crate::query_device_auth(addr, &key, QUERY_TIMEOUT),
                ok(),
                "query #{i}"
            );
        }
    }
}
```

`lib.rs`에 `mod poller;` 와 `pub use poller::{QUERY_TIMEOUT, RETRY_INTERVAL, run_poll_loop, spawn_poller};`.

- [ ] **Step 2: 실패 확인**

Run: `cargo test -p sdwall-device-auth --lib poller`
Expected: 컴파일 실패

- [ ] **Step 3: 구현**

```rust
use std::net::SocketAddr;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::{AuthState, DEVICE_SERVICE_ADDR, embedded_verifying_key, query_device_auth};

/// 인증 정상이 아닌 동안 다시 묻는 간격 (명세: "몇 초마다").
pub const RETRY_INTERVAL: Duration = Duration::from_secs(3);
/// 한 번 묻기의 연결+응답 시간 한도. 넘으면 "인증 확인 안 됨".
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// 바뀐 결과만 알리고, 인증 정상이면 멈춘다.
///
/// DeviceService 는 기동할 때 한 번 판정하므로 정상 뒤에는 다시 물을 이유가 없다.
pub fn run_poll_loop(
    mut query: impl FnMut() -> AuthState,
    mut sleep: impl FnMut(Duration),
    mut on_change: impl FnMut(AuthState),
    interval: Duration,
) {
    let mut last: Option<AuthState> = None;
    loop {
        let state = query();
        let done = matches!(state, AuthState::Ok { .. });
        if last.as_ref() != Some(&state) {
            last = Some(state.clone());
            on_change(state);
        }
        if done {
            return;
        }
        sleep(interval);
    }
}

pub fn spawn_poller(on_change: impl FnMut(AuthState) + Send + 'static) -> JoinHandle<()> {
    let addr: SocketAddr = DEVICE_SERVICE_ADDR.parse().expect("constant address");
    thread::Builder::new()
        .name("sdwall-device-auth".into())
        .spawn(move || {
            let key = embedded_verifying_key();
            run_poll_loop(
                || query_device_auth(addr, &key, QUERY_TIMEOUT),
                thread::sleep,
                on_change,
                RETRY_INTERVAL,
            )
        })
        .expect("spawn device-auth poller")
}
```

- [ ] **Step 4: 통과 확인**

Run: `cargo test -p sdwall-device-auth --lib`
Expected: 단위 시험 PASS, `live_fake_auth_repeated` ignored.

- [ ] **Step 5: 실제 DeviceService로 라이브 확인**

다른 PowerShell 창에서:
```powershell
..\servo-handoff\DeviceService\DeviceService.exe fake-auth --state ok --project-key 123412 --key ..\servo-handoff\keys\test\response-signing-test-private.pem
```
Run: `cargo test -p sdwall-device-auth --lib -- --ignored live_fake_auth_repeated`
Expected: PASS (30회 모두 `Ok`). 실패하면 멈추고 원인부터 본다: 간헐 `Invalid` → high-S 거부 의심(`Signature::normalize_s()`로 정규화 후 검증하도록 `verify_response` 수정, 벡터 시험 재확인); 항상 `Invalid` → `http_get`이 받은 바이트를 로그로 찍어 프레이밍 확인. 확인 뒤 fake-auth 창을 닫는다.

- [ ] **Step 6: Commit**

```bash
git add components/sdwall_device_auth
git commit -m "feat(device-auth): 인증 정상이 될 때까지 다시 묻는 폴러를 둔다

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: 넘길 페이지 범위와 주입 스크립트

**Files:**
- Create: `components/sdwall_device_auth/page.rs`
- Modify: `components/sdwall_device_auth/lib.rs`

**Interfaces:**
- Consumes: `AuthState`
- Produces:
  - `pub const SCOPED_PATH_PREFIXES: [&str; 2] = ["/ipwall/", "/portal/"];`
  - `pub struct PageScope` — `pub fn from_start_url(url: &Url) -> Option<PageScope>`, `pub fn contains(&self, url: &Url) -> bool`, `pub fn injection_script(&self, state: &AuthState) -> String`
  - 스크립트 완료 값: 넣었으면 `"set"`, 범위 밖이면 `"skip"` (문자열)

- [ ] **Step 1: 실패하는 시험 작성 (`page.rs` 하단)**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn prod() -> PageScope {
        PageScope::from_start_url(&u("https://10.0.0.5:7245/ipwall/#/output")).unwrap()
    }

    #[test]
    fn scope_follows_start_origin_and_paths() {
        let s = prod();
        assert!(s.contains(&u("https://10.0.0.5:7245/ipwall/#/output")));
        assert!(s.contains(&u("https://10.0.0.5:7245/ipwall/")));
        assert!(s.contains(&u("https://10.0.0.5:7245/portal/a/b?x=1")));
        assert!(!s.contains(&u("https://10.0.0.5:7245/")));
        assert!(!s.contains(&u("https://10.0.0.5:7245/ipwall")));
        assert!(!s.contains(&u("https://10.0.0.5:7245/ipwallx/")));
        assert!(!s.contains(&u("https://10.0.0.6:7245/ipwall/")));
        assert!(!s.contains(&u("http://10.0.0.5:7245/ipwall/")));
        assert!(!s.contains(&u("https://10.0.0.5:7246/ipwall/")));
    }

    #[test]
    fn test_page_origin_is_in_scope_when_it_is_the_start_url() {
        let s = PageScope::from_start_url(&u("http://127.0.0.1:7250/ipwall/device-auth-check")).unwrap();
        assert!(s.contains(&u("http://127.0.0.1:7250/ipwall/device-auth-check")));
    }

    #[test]
    fn opaque_start_url_has_no_scope() {
        assert!(PageScope::from_start_url(&u("file:///C:/x/ipwall/index.html")).is_none());
        assert!(PageScope::from_start_url(&u("data:text/html,hi")).is_none());
    }

    #[test]
    fn script_carries_value_guard_and_event() {
        let s = prod();
        let ok = s.injection_script(&AuthState::Ok { project_key: "123412".into() });
        assert!(ok.contains(r#"window.sdwallDeviceAuth={"projectKey":"123412","state":"ok"}"#), "{ok}");
        assert!(ok.contains("window.dispatchEvent(new Event('sdwall-device-auth'))"));
        assert!(ok.contains(r#"location.origin!=="https://10.0.0.5:7245""#));
        assert!(ok.contains("window.top!==window"));
        assert!(!ok.contains("HJY0BWGD"));
        let invalid = s.injection_script(&AuthState::Invalid);
        assert!(invalid.contains(r#"window.sdwallDeviceAuth={"state":"invalid"}"#), "{invalid}");
        let unreachable = s.injection_script(&AuthState::Unreachable);
        assert!(unreachable.contains(r#"window.sdwallDeviceAuth={"state":"unreachable"}"#));
    }

    #[test]
    fn project_key_is_json_escaped() {
        let script = prod().injection_script(&AuthState::Ok { project_key: "1\"</script>'\\".into() });
        assert!(script.contains(r#""projectKey":"1\"</script>'\\""#), "{script}");
    }
}
```

`serde_json::json!` 객체는 키를 정렬해 직렬화하므로(`preserve_order` 기능이 꺼진 경우) 위 기대 문자열은 `projectKey`가 `state`보다 앞이다. 워크스페이스에서 `preserve_order`가 켜져 있으면 `{"state":"ok","projectKey":"123412"}` 순서가 되므로, Step 4에서 실패하면 기대 문자열의 키 순서만 맞춘다(의미는 같다).

`lib.rs`에 `mod page;` 와 `pub use page::{PageScope, SCOPED_PATH_PREFIXES};`.

- [ ] **Step 2: 실패 확인**

Run: `cargo test -p sdwall-device-auth --lib page`
Expected: 컴파일 실패

- [ ] **Step 3: 구현**

```rust
use serde_json::json;
use url::Url;

use crate::AuthState;

/// 명세 2절: 넘기는 곳은 SDWall 페이지(`/ipwall/`, `/portal/` 아래)다.
pub const SCOPED_PATH_PREFIXES: [&str; 2] = ["/ipwall/", "/portal/"];

/// 값을 넘겨도 되는 페이지.
///
/// SDWall 오리진은 ***엔진을 띄운 시작 URL의 오리진***이다. 운영(`https://<서버>:7245`)과
/// 시험(`http://127.0.0.1:7250`)이 둘 다 시작 URL이라 별도 설정이 필요 없다.
pub struct PageScope {
    origin: String,
}

impl PageScope {
    /// 불투명 오리진(`file:`, `data:` ...)이면 `None` -- 아무 데도 넘기지 않는다.
    pub fn from_start_url(url: &Url) -> Option<Self> {
        let origin = url.origin();
        origin.is_tuple().then(|| Self {
            origin: origin.ascii_serialization(),
        })
    }

    pub fn contains(&self, url: &Url) -> bool {
        url.origin().ascii_serialization() == self.origin &&
            SCOPED_PATH_PREFIXES
                .iter()
                .any(|prefix| url.path().starts_with(prefix))
    }

    /// 페이지 안에서 실행할 스크립트.
    ///
    /// ***가드를 스크립트 안에도 둔다.*** `evaluate_javascript` 는 내보낸 순간과 실행되는
    /// 순간 사이에 문서가 바뀔 수 있다(이동 직후). 실행되는 문서가 스스로 범위를 확인해야
    /// 다른 오리진에 값이 새지 않는다.
    pub fn injection_script(&self, state: &AuthState) -> String {
        let value = match state {
            AuthState::Ok { project_key } => json!({ "state": "ok", "projectKey": project_key }),
            AuthState::Invalid => json!({ "state": "invalid" }),
            AuthState::Unreachable => json!({ "state": "unreachable" }),
        };
        let origin = serde_json::to_string(&self.origin).expect("string serializes");
        let prefixes = serde_json::to_string(&SCOPED_PATH_PREFIXES).expect("array serializes");
        format!(
            "(function(){{\
var p=location.pathname;\
if(window.top!==window||location.origin!=={origin}||!{prefixes}.some(function(x){{return p.indexOf(x)===0;}}))return 'skip';\
window.sdwallDeviceAuth={value};\
window.dispatchEvent(new Event('sdwall-device-auth'));\
return 'set';\
}})()"
        )
    }
}
```

- [ ] **Step 4: 통과 확인**

Run: `cargo test -p sdwall-device-auth --lib`
Expected: 모두 PASS.

- [ ] **Step 5: Commit**

```bash
git add components/sdwall_device_auth
git commit -m "feat(device-auth): SDWall 페이지에만 값을 넣고 알리는 스크립트를 만든다

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: 문서별 주입 시점 추적기

**Files:**
- Create: `components/sdwall_device_auth/tracker.rs`
- Modify: `components/sdwall_device_auth/lib.rs`

**Interfaces:**
- Consumes: `AuthState`
- Produces:
  - `pub enum DocumentEvent { Started, HeadParsed, Complete }` (`Clone, Copy, Debug, PartialEq, Eq`)
  - `#[derive(Default)] pub struct InjectionTracker`
  - `pub fn on_document_event(&mut self, event: DocumentEvent) -> Option<AuthState>` — 지금 넣을 값
  - `pub fn on_auth_state(&mut self, state: AuthState) -> Option<AuthState>` — 지금 넣을 값
  - `pub fn document(&self) -> u64` — 현재 문서 세대(`Started`마다 1 증가). 주입할 때 함께 들고 간다
  - `pub fn on_injection_failed(&mut self, document: u64)` — 그 문서가 아직 현재 문서면 다음 `HeadParsed`/`Complete`에서 다시 넣게 한다. 앞 문서의 늦은 실패는 버린다 (리뷰 후 결정, 2026-10-13)

규칙: `Started`는 새 문서(이전 주입 무효). 문서가 `HeadParsed` 또는 `Complete`에 이르렀고 결과가 있으면 그 문서에 한 번 넣는다. 결과가 바뀌면 준비된 현재 문서에 곧바로 다시 넣는다. 결과가 아직 없으면 넣지 않는다(페이지는 기다린다). `#`만 바뀌는 이동은 `LoadStatus`를 내지 않으므로 아무것도 하지 않는다.

- [ ] **Step 1: 실패하는 시험 작성 (`tracker.rs` 하단)**

```rust
#[cfg(test)]
mod tests {
    use super::DocumentEvent::*;
    use super::*;

    fn ok() -> AuthState {
        AuthState::Ok { project_key: "123412".into() }
    }

    #[test]
    fn nothing_before_first_result() {
        let mut t = InjectionTracker::default();
        assert_eq!(t.on_document_event(Started), None);
        assert_eq!(t.on_document_event(HeadParsed), None);
        assert_eq!(t.on_document_event(Complete), None);
        // 결과가 오면 이미 준비된 문서에 넣는다.
        assert_eq!(t.on_auth_state(AuthState::Unreachable), Some(AuthState::Unreachable));
    }

    #[test]
    fn result_before_document_waits_for_head_parsed_once() {
        let mut t = InjectionTracker::default();
        assert_eq!(t.on_auth_state(ok()), None);
        assert_eq!(t.on_document_event(Started), None);
        assert_eq!(t.on_document_event(HeadParsed), Some(ok()));
        assert_eq!(t.on_document_event(Complete), None);
    }

    #[test]
    fn every_new_document_gets_it_again() {
        let mut t = InjectionTracker::default();
        t.on_auth_state(ok());
        for _ in 0..3 {
            assert_eq!(t.on_document_event(Started), None);
            assert_eq!(t.on_document_event(HeadParsed), Some(ok()));
            assert_eq!(t.on_document_event(Complete), None);
        }
    }

    #[test]
    fn document_without_body_gets_it_at_complete() {
        let mut t = InjectionTracker::default();
        t.on_auth_state(AuthState::Invalid);
        t.on_document_event(Started);
        assert_eq!(t.on_document_event(Complete), Some(AuthState::Invalid));
    }

    #[test]
    fn swallowed_second_started_still_injects_new_document() {
        // 첫 문서가 HeadParsed 전에 다시 이동하면 두 번째 Started 는 WebView 가 삼킨다.
        let mut t = InjectionTracker::default();
        t.on_auth_state(ok());
        t.on_document_event(Started);
        // (두 번째 Started 없음)
        assert_eq!(t.on_document_event(HeadParsed), Some(ok()));
    }

    #[test]
    fn change_reinjects_into_ready_document_and_same_value_does_not() {
        let mut t = InjectionTracker::default();
        t.on_document_event(Started);
        t.on_document_event(HeadParsed);
        assert_eq!(t.on_auth_state(AuthState::Unreachable), Some(AuthState::Unreachable));
        assert_eq!(t.on_auth_state(AuthState::Unreachable), None);
        assert_eq!(t.on_auth_state(AuthState::Invalid), Some(AuthState::Invalid));
        assert_eq!(t.on_auth_state(ok()), Some(ok()));
    }

    #[test]
    fn change_during_loading_waits_for_head_parsed() {
        let mut t = InjectionTracker::default();
        t.on_auth_state(AuthState::Unreachable);
        t.on_document_event(Started);
        assert_eq!(t.on_auth_state(ok()), None);
        assert_eq!(t.on_document_event(HeadParsed), Some(ok()));
    }

    #[test]
    fn failed_injection_retries_at_next_milestone() {
        let mut t = InjectionTracker::default();
        t.on_auth_state(ok());
        t.on_document_event(Started);
        assert_eq!(t.on_document_event(HeadParsed), Some(ok()));
        t.on_injection_failed();
        assert_eq!(t.on_document_event(Complete), Some(ok()));
    }
}
```

`lib.rs`에 `mod tracker;` 와 `pub use tracker::{DocumentEvent, InjectionTracker};`.

- [ ] **Step 2: 실패 확인**

Run: `cargo test -p sdwall-device-auth --lib tracker`
Expected: 컴파일 실패

- [ ] **Step 3: 구현**

```rust
use crate::AuthState;

/// 최상위 문서의 적재 단계. 셸이 `servo::LoadStatus` 를 이것으로 옮긴다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentEvent {
    Started,
    HeadParsed,
    Complete,
}

/// 언제 페이지에 값을 넣을지 정한다.
///
/// 명세 2절 "다시 넘기기": 새 문서마다 넣고, 값이 바뀌면 다시 넣는다. `#` 뒤만 바뀌는
/// 이동은 `LoadStatus` 를 내지 않으므로 여기 오지 않는다.
#[derive(Default)]
pub struct InjectionTracker {
    state: Option<AuthState>,
    document_ready: bool,
    injected: bool,
}

impl InjectionTracker {
    pub fn on_document_event(&mut self, event: DocumentEvent) -> Option<AuthState> {
        match event {
            DocumentEvent::Started => {
                self.document_ready = false;
                self.injected = false;
                None
            },
            // `Started→Started` 는 WebView 가 삼키지만, 그 경우 앞 문서는 HeadParsed 에
            // 이르지 못했으므로 `injected` 가 여전히 false 다 -- 새 문서가 값을 받는다.
            DocumentEvent::HeadParsed | DocumentEvent::Complete => {
                self.document_ready = true;
                self.take_pending()
            },
        }
    }

    pub fn on_auth_state(&mut self, state: AuthState) -> Option<AuthState> {
        if self.state.as_ref() == Some(&state) {
            return None;
        }
        self.state = Some(state);
        self.injected = false;
        if self.document_ready {
            self.take_pending()
        } else {
            None
        }
    }

    pub fn on_injection_failed(&mut self) {
        self.injected = false;
    }

    fn take_pending(&mut self) -> Option<AuthState> {
        if self.injected {
            return None;
        }
        let state = self.state.clone()?;
        self.injected = true;
        Some(state)
    }
}
```

- [ ] **Step 4: 통과 확인 + 포맷**

Run: `cargo test -p sdwall-device-auth --lib` → 모두 PASS
Run: `rustfmt --edition 2024 --check components/sdwall_device_auth/*.rs` → 출력 없음 (있으면 `rustfmt --edition 2024`로 고치고 재확인)

- [ ] **Step 5: Commit**

```bash
git add components/sdwall_device_auth
git commit -m "feat(device-auth): 새 문서와 값 변화마다 한 번씩 넣도록 추적한다

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 6: winit_wall 연동

**Files:**
- Modify: `components/servo/Cargo.toml` (`[dev-dependencies]`에 `sdwall_device_auth = { workspace = true }`, 알파벳 자리)
- Modify: `components/servo/examples/winit_wall/main.rs`
  - `use` 블록 (22-41행 근처)
  - `struct AppState` (256행) 필드 추가, `Rc::new(AppState { ... })` (1790행 근처) 초기화
  - `impl ::servo::WebViewDelegate for AppState` (1502행 근처)
  - `enum WakerEvent` (2067행), `fn user_event` (1911행)
  - `spawn_pacing_thread(...)` 호출부(1905행 근처) 옆에서 폴러 기동
  - `#[cfg(test)] mod tests` (2163행 근처)

**Interfaces:**
- Consumes: `spawn_poller`, `AuthState`, `PageScope`, `InjectionTracker`, `DocumentEvent`, `SIGNING_KEY_KIND` (Task 1–5); `servo::{LoadStatus, JSValue, WebView}`; `WebView::evaluate_javascript(&self, script, callback: impl FnOnce(Result<JSValue, JavaScriptEvaluationError>) + 'static)` (components/servo/webview.rs:831) — `InjectionTracker::document() -> u64`, `on_injection_failed(document: u64)` (Task 5 ruling: 늦게 온 실패는 버린다)
- Produces: `fn document_event(status: LoadStatus) -> DocumentEvent` (main.rs, 시험 대상)

- [ ] **Step 1: 실패하는 시험 작성 (main.rs `mod tests` 안)**

```rust
#[test]
fn load_status_maps_to_document_event() {
    use sdwall_device_auth::DocumentEvent;
    assert_eq!(document_event(::servo::LoadStatus::Started), DocumentEvent::Started);
    assert_eq!(document_event(::servo::LoadStatus::HeadParsed), DocumentEvent::HeadParsed);
    assert_eq!(document_event(::servo::LoadStatus::Complete), DocumentEvent::Complete);
}
```

- [ ] **Step 2: 실패 확인**

Run: `cargo test -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release load_status_maps`
Expected: 컴파일 실패 (`document_event`, `sdwall_device_auth` 없음). 새 워크트리라 첫 빌드는 오래 걸린다(`target/` 없음).

- [ ] **Step 3: 구현**

`components/servo/Cargo.toml` `[dev-dependencies]`:
```toml
sdwall_device_auth = { workspace = true }
```

`main.rs` — 자유 함수(델리게이트 impl 근처):
```rust
/// `LoadStatus` 는 최상위 문서에만 나온다 -- 장비 인증을 넘길 문서도 최상위뿐이다.
fn document_event(status: ::servo::LoadStatus) -> sdwall_device_auth::DocumentEvent {
    use sdwall_device_auth::DocumentEvent;
    match status {
        ::servo::LoadStatus::Started => DocumentEvent::Started,
        ::servo::LoadStatus::HeadParsed => DocumentEvent::HeadParsed,
        ::servo::LoadStatus::Complete => DocumentEvent::Complete,
    }
}
```

`struct AppState` 필드:
```rust
    /// 장비 인증(servo-device-auth.md): 언제 넣을지. 평가 콜백도 만지므로 `Rc` 로 나눈다.
    device_auth: Rc<RefCell<sdwall_device_auth::InjectionTracker>>,
    /// 넘겨도 되는 페이지. 시작 URL 이 불투명 오리진이면 `None` -- 아무 데도 넘기지 않는다.
    device_auth_scope: Option<sdwall_device_auth::PageScope>,
```
초기화(`Rc::new(AppState { ... })`, 시작 URL 변수는 `config.url` — 실제 이름은 그 자리에서 확인):
```rust
            device_auth: Rc::default(),
            device_auth_scope: sdwall_device_auth::PageScope::from_start_url(&config.url),
```

`impl AppState`에 메서드:
```rust
    fn on_device_auth_state(&self, auth: sdwall_device_auth::AuthState) {
        // 지문은 로그에도 남기지 않는다 -- `AuthState` 에는 애초에 없다.
        log::warn!("device-auth: {}", auth.label());
        let pending = self.device_auth.borrow_mut().on_auth_state(auth);
        self.inject_device_auth(pending);
    }

    fn inject_device_auth(&self, pending: Option<sdwall_device_auth::AuthState>) {
        let (Some(auth), Some(scope)) = (pending, self.device_auth_scope.as_ref()) else {
            return;
        };
        // 이 주입이 어느 문서의 것인지 -- 늦게 온 실패가 새 문서의 주입을 무르지 않게 한다.
        let document = self.device_auth.borrow().document();
        let Some(webview) = self.webview.borrow().clone() else {
            self.device_auth.borrow_mut().on_injection_failed(document);
            return;
        };
        let tracker = self.device_auth.clone();
        let label = auth.label();
        webview.evaluate_javascript(scope.injection_script(&auth), move |result| match result {
            Ok(::servo::JSValue::String(outcome)) => {
                log::info!("device-auth: inject {label} -> {outcome}")
            },
            Ok(other) => log::warn!("device-auth: inject {label} -> unexpected {other:?}"),
            Err(error) => {
                log::warn!("device-auth: inject {label} failed: {error:?}");
                tracker.borrow_mut().on_injection_failed(document);
            },
        });
    }
```

델리게이트 impl에 추가:
```rust
    fn notify_load_status_changed(&self, _: WebView, status: ::servo::LoadStatus) {
        let pending = self
            .device_auth
            .borrow_mut()
            .on_document_event(document_event(status));
        self.inject_device_auth(pending);
    }
```

`enum WakerEvent`에 변형 추가:
```rust
    /// 장비 인증 폴러가 바뀐 결과를 알린다.
    DeviceAuth(sdwall_device_auth::AuthState),
```

`user_event` — `if let Self::Running(state) = self {` 바로 안, 기존 `let site = match event` **앞**:
```rust
            // 장비 인증은 표출 박자와 상관없다 -- 세지도, 렌더를 끌지도 않는다.
            if let WakerEvent::DeviceAuth(auth) = &event {
                state.on_device_auth_state(auth.clone());
                return;
            }
```
그리고 그 뒤 `event`를 매치하는 두 곳(`let site = match event`, 아래쪽 `match event`)에 각각 도달 불가 팔을 더한다:
```rust
                WakerEvent::DeviceAuth(_) => unreachable!("handled above"),
```

폴러 기동 — `spawn_pacing_thread(waker.proxy.clone(), ...)` 호출 옆(WebView가 만들어진 뒤, `*self = Self::Running(app_state)` 전):
```rust
        if sdwall_device_auth::SIGNING_KEY_KIND != "release" {
            log::warn!(
                "device-auth: built with the {} signing key -- not for deployment",
                sdwall_device_auth::SIGNING_KEY_KIND
            );
        }
        let device_auth_proxy = waker.proxy.clone();
        sdwall_device_auth::spawn_poller(move |auth| {
            let _ = device_auth_proxy.send_event(WakerEvent::DeviceAuth(auth));
        });
```
(`waker` 변수명이 그 자리에서 다르면 맞춘다. `EventLoopProxy<WakerEvent>`는 `Send`다.)

- [ ] **Step 4: 통과 확인**

Run: `cargo test -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release`
Expected: 기존 시험 + `load_status_maps_to_document_event` PASS.
Run: `cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release` → 경고 없이 성공.
Run: `rustfmt --edition 2024 --check components/servo/examples/winit_wall/main.rs` → 출력 없음.

- [ ] **Step 5: Commit**

```bash
git add Cargo.lock components/servo/Cargo.toml components/servo/examples/winit_wall/main.rs
git commit -m "feat(winit_wall): 장비 인증 결과를 SDWall 페이지로 넘긴다

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 7: 자가 시험 모드 `--device-auth-selftest` (시험 키 빌드 전용)

`winit_wall`은 사람이 누르지 않는다. 엔진이 스스로 확인 페이지를 **새로고침 → 다른 문서로 이동(`?n=<n+1>`) → `#`만 바꿈** 순서로 돌리고, 새 문서마다 값이 다시 들어갔는지, `#` 이동에는 다시 넣지 않았는지 판정한 뒤 종료한다. 인자는 시험 키 빌드에서만 받는다. 실제 키 빌드에서는 `unknown argument`로 거부되므로 현장에 이 경로가 없다.

**Files:**
- Create: `components/servo/examples/winit_wall/device_auth_selftest.rs`
- Modify: `components/servo/examples/winit_wall/main.rs` (`mod` 선언, `Config`/`parse_args`, `AppState`, `inject_device_auth`, `notify_load_status_changed`, `show_console_message`, `about_to_wait`)

**Interfaces:**
- Consumes: Task 6의 `AppState::inject_device_auth`, 델리게이트 메서드, `sdwall_device_auth::SIGNING_KEY_KIND`
- Produces:
  - `pub struct DeviceAuthSelfTest`, `pub fn new(now: Instant) -> Self`
  - `pub fn on_document_started(&mut self)`
  - `pub fn on_injected(&mut self) -> Option<&'static str>` — 주입 결과가 `"set"`일 때 부른다. 페이지에서 실행할 다음 동작 스크립트를 돌려준다.
  - `pub fn on_console(&mut self, message: &str)`
  - `pub fn take_verdict(&mut self, now: Instant) -> Option<Verdict>` — 판정이 나면 **한 번만** `Some`
  - `pub enum Verdict { Pass, Fail(String) }`
  - 로그 줄 (Task 8 스크립트가 읽는다, `warn`/`error`): `device-auth selftest: PASS documents=3`, `device-auth selftest: FAIL <이유>`

- [ ] **Step 1: 실패하는 시험 작성 (`device_auth_selftest.rs` 하단)**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn run_to_fragment(t: &mut DeviceAuthSelfTest) {
        t.on_document_started();
        assert_eq!(t.on_injected(), Some(RELOAD));
        // 같은 문서에 값이 다시 들어가도(값 변화) 다음 단계로 넘어가지 않는다.
        assert_eq!(t.on_injected(), None);
        t.on_document_started();
        assert_eq!(t.on_injected(), Some(NAVIGATE));
        t.on_document_started();
        assert_eq!(t.on_injected(), Some(FRAGMENT));
    }

    #[test]
    fn full_sequence_passes() {
        let now = Instant::now();
        let mut t = DeviceAuthSelfTest::new(now);
        run_to_fragment(&mut t);
        assert_eq!(t.take_verdict(now), None);
        t.on_console("[device-auth-check] 알림 {} 맞음");
        assert_eq!(t.take_verdict(now), None);
        t.on_console(FRAGMENT_SETTLED_MARKER);
        assert_eq!(t.take_verdict(now), Some(Verdict::Pass));
        assert_eq!(t.take_verdict(now), None, "reported once");
    }

    #[test]
    fn injection_after_fragment_fails() {
        let now = Instant::now();
        let mut t = DeviceAuthSelfTest::new(now);
        run_to_fragment(&mut t);
        assert_eq!(t.on_injected(), None);
        t.on_console(FRAGMENT_SETTLED_MARKER);
        assert!(matches!(t.take_verdict(now), Some(Verdict::Fail(_))));
    }

    #[test]
    fn marker_before_fragment_phase_is_ignored() {
        let now = Instant::now();
        let mut t = DeviceAuthSelfTest::new(now);
        t.on_document_started();
        t.on_injected();
        t.on_console(FRAGMENT_SETTLED_MARKER);
        assert_eq!(t.take_verdict(now), None);
    }

    #[test]
    fn timeout_fails_with_phase() {
        let now = Instant::now();
        let mut t = DeviceAuthSelfTest::new(now);
        t.on_document_started();
        t.on_injected();
        match t.take_verdict(now + SELFTEST_TIMEOUT) {
            Some(Verdict::Fail(reason)) => assert!(reason.contains("WaitReload"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn navigate_script_bumps_n() {
        assert!(NAVIGATE.contains("location.assign(location.pathname+'?n='+(n+1))"));
        assert!(FRAGMENT.contains(FRAGMENT_SETTLED_MARKER));
    }
}
```

- [ ] **Step 2: 실패 확인**

`main.rs` 상단 `mod tile;` 근처에 `mod device_auth_selftest;` 를 더하고:
Run: `cargo test -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release device_auth_selftest`
Expected: 컴파일 실패 (`DeviceAuthSelfTest` 없음)

- [ ] **Step 3: 구현 (`device_auth_selftest.rs` 본문)**

```rust
//! `--device-auth-selftest`: 사람 없이 장비 인증 "다시 넘기기"를 확인한다.
//!
//! ***시험 키 빌드에서만 받는 인자다.*** 실제 키 빌드는 이 인자를 모른다고 거부한다.
//! 순서: 첫 문서에 값이 들어가면 새로고침 → 새 문서에 들어가면 `?n=<n+1>` 로 이동 →
//! 새 문서에 들어가면 `#` 만 바꾸고 2초 기다림. 마지막 구간에 값이 또 들어가면 실패다
//! (명세 2절: `#` 뒤만 바뀌는 이동은 다시 넘기지 않는다).

use std::time::{Duration, Instant};

pub const SELFTEST_TIMEOUT: Duration = Duration::from_secs(45);
pub const FRAGMENT_SETTLED_MARKER: &str = "[device-auth-selftest] fragment-settled";

pub const RELOAD: &str = "setTimeout(function(){location.reload();},1000);'scheduled'";
pub const NAVIGATE: &str = "setTimeout(function(){var n=Number(new URLSearchParams(location.search).get('n')||'1');location.assign(location.pathname+'?n='+(n+1));},1000);'scheduled'";
pub const FRAGMENT: &str = "setTimeout(function(){location.hash='device-auth-selftest';setTimeout(function(){console.log('[device-auth-selftest] fragment-settled');},2000);},1000);'scheduled'";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    WaitFirst,
    WaitReload,
    WaitNavigate,
    WaitFragmentSettle,
    Settled,
    Reported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail(String),
}

pub struct DeviceAuthSelfTest {
    phase: Phase,
    new_document: bool,
    documents_injected: u32,
    extra_after_fragment: u32,
    deadline: Instant,
}

impl DeviceAuthSelfTest {
    pub fn new(now: Instant) -> Self {
        Self {
            phase: Phase::WaitFirst,
            new_document: false,
            documents_injected: 0,
            extra_after_fragment: 0,
            deadline: now + SELFTEST_TIMEOUT,
        }
    }

    pub fn on_document_started(&mut self) {
        self.new_document = true;
    }

    pub fn on_injected(&mut self) -> Option<&'static str> {
        let (next, script) = match self.phase {
            Phase::WaitFragmentSettle => {
                self.extra_after_fragment += 1;
                return None;
            },
            Phase::Settled | Phase::Reported => return None,
            // 새 문서가 아니면 같은 문서에 값이 다시 들어간 것(값 변화)이다 -- 단계가 아니다.
            _ if !self.new_document => return None,
            Phase::WaitFirst => (Phase::WaitReload, RELOAD),
            Phase::WaitReload => (Phase::WaitNavigate, NAVIGATE),
            Phase::WaitNavigate => (Phase::WaitFragmentSettle, FRAGMENT),
        };
        self.new_document = false;
        self.documents_injected += 1;
        self.phase = next;
        Some(script)
    }

    pub fn on_console(&mut self, message: &str) {
        if self.phase == Phase::WaitFragmentSettle && message.contains(FRAGMENT_SETTLED_MARKER) {
            self.phase = Phase::Settled;
        }
    }

    pub fn take_verdict(&mut self, now: Instant) -> Option<Verdict> {
        let verdict = match self.phase {
            Phase::Reported => return None,
            Phase::Settled if self.extra_after_fragment == 0 && self.documents_injected == 3 => {
                Verdict::Pass
            },
            Phase::Settled => Verdict::Fail(format!(
                "documents={} injected_after_fragment={}",
                self.documents_injected, self.extra_after_fragment
            )),
            phase if now >= self.deadline => Verdict::Fail(format!(
                "timeout in {phase:?} after {} documents",
                self.documents_injected
            )),
            _ => return None,
        };
        self.phase = Phase::Reported;
        Some(verdict)
    }
}
```

- [ ] **Step 4: main.rs 배선**

`Config`에 `device_auth_selftest: bool` 필드, `parse_args`의 지역 변수 `let mut device_auth_selftest = false;`, 매치 팔(`"--ignore-certificate-errors"` 옆):
```rust
            // 시험 키 빌드에서만 받는다 -- 실제 키 빌드에서는 아래 `unknown argument` 로 간다.
            "--device-auth-selftest" if sdwall_device_auth::SIGNING_KEY_KIND == "test" => {
                device_auth_selftest = true
            },
```
그리고 `Config { ... }`를 만드는 곳에 `device_auth_selftest,`.

`AppState` 필드와 초기화:
```rust
    /// `--device-auth-selftest` (시험 키 빌드 전용).
    device_auth_selftest: Option<Rc<RefCell<device_auth_selftest::DeviceAuthSelfTest>>>,
```
```rust
            device_auth_selftest: config.device_auth_selftest.then(|| {
                Rc::new(RefCell::new(device_auth_selftest::DeviceAuthSelfTest::new(
                    std::time::Instant::now(),
                )))
            }),
```

`notify_load_status_changed` 맨 앞:
```rust
        if matches!(status, ::servo::LoadStatus::Started) {
            if let Some(selftest) = &self.device_auth_selftest {
                selftest.borrow_mut().on_document_started();
            }
        }
```

`inject_device_auth`의 콜백 — `"set"`이면 자가 시험에 알리고, 나온 동작 스크립트를 실행한다. 콜백 앞에서 복제:
```rust
        let selftest = self.device_auth_selftest.clone();
        let selftest_webview = webview.clone();
```
`Ok(::servo::JSValue::String(outcome))` 팔을 다음으로 바꾼다:
```rust
            Ok(::servo::JSValue::String(outcome)) => {
                log::info!("device-auth: inject {label} -> {outcome}");
                if outcome == "set" {
                    let next = selftest.as_ref().and_then(|s| s.borrow_mut().on_injected());
                    if let Some(script) = next {
                        log::warn!("device-auth selftest: next step");
                        selftest_webview.evaluate_javascript(script, |_| {});
                    }
                }
            },
```

`show_console_message` 맨 앞:
```rust
        if let Some(selftest) = &self.device_auth_selftest {
            selftest.borrow_mut().on_console(&message);
        }
```

`impl AppState`에:
```rust
    /// 자가 시험이 끝났으면 판정을 남기고 `true` -- 호출자가 이벤트 루프를 끝낸다.
    fn device_auth_selftest_finished(&self) -> bool {
        let Some(selftest) = &self.device_auth_selftest else {
            return false;
        };
        let verdict = selftest.borrow_mut().take_verdict(std::time::Instant::now());
        match verdict {
            Some(device_auth_selftest::Verdict::Pass) => {
                log::warn!("device-auth selftest: PASS documents=3");
                true
            },
            Some(device_auth_selftest::Verdict::Fail(reason)) => {
                log::error!("device-auth selftest: FAIL {reason}");
                true
            },
            None => false,
        }
    }
```

`about_to_wait`(`event_loop`를 받는 곳; 기존 `event_loop.exit()` 호출 근처, 1974행 부근)의 `Running` 분기 안 맨 앞:
```rust
            if state.device_auth_selftest_finished() {
                event_loop.exit();
                return;
            }
```
(`about_to_wait`의 실제 구조에 맞춰 `state` 바인딩 이름을 따른다. 페이싱 스레드가 초당 60회 깨우므로 이 검사는 늦어도 한 박자 안에 돈다.)

- [ ] **Step 5: 통과 확인**

Run: `cargo test -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release`
Expected: 새 시험 5개 포함 모두 PASS.
Run: `$env:SDWALL_DEVICE_AUTH_SIGNING_KEY='test'; cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release` → 경고 없이 성공 (시험 키 경로 컴파일 확인). 확인 뒤 `Remove-Item Env:SDWALL_DEVICE_AUTH_SIGNING_KEY`.
Run: `rustfmt --edition 2024 --check components/servo/examples/winit_wall/main.rs components/servo/examples/winit_wall/device_auth_selftest.rs`

- [ ] **Step 6: Commit**

```bash
git add components/servo/examples/winit_wall
git commit -m "test(winit_wall): 시험 키 빌드에서 새로고침·이동·# 를 스스로 돌려 다시 넘기기를 판정한다

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 8: 자동 확인 스크립트와 실제 실행

명세 3절 표 네 줄과 "다시 넘기기"를 스크립트 한 번으로 돌린다. 시험 키 빌드와 실제 키 빌드에서 각각 한 번씩 실행한다.

**Files:**
- Create: `etc/multigpu/device_auth_key_kind.ps1`
- Create: `etc/multigpu/device_auth_check.ps1` (**UTF-8 BOM으로 저장**: Windows PowerShell 5.1은 BOM이 없으면 한글 리터럴을 ANSI로 읽는다)

**Interfaces:**
- Consumes: Task 7의 로그 줄 `device-auth selftest: PASS|FAIL`, 확인 페이지 로그 `[page] [device-auth-check] <열 때|알림> <값 JSON> <모양>`, `winit_wall` 인자 `--wall-layout`, `--wall-tile-index`, `--capture`, `--capture-sec`, `--device-auth-selftest`
- Produces: `function Get-DeviceAuthKeyKind([string] $Exe, [string] $Repo)` → `'release'|'test'|'unknown'` (Task 9 배포 가드도 쓴다). 스크립트 종료 코드: 모두 PASS면 0, 아니면 1.

- [ ] **Step 1: `device_auth_key_kind.ps1`**

```powershell
# exe 에 박힌 장비 인증 공개 키의 종류를 가른다.
#
# 키는 빌드 때 `include_str!` 로 박히므로(components/sdwall_device_auth/key.rs) PEM 본문이
# 바이너리에 그대로 있다. 첫 본문 줄(64자)은 키마다 다르다.
function Get-DeviceAuthKeyKind([string] $Exe, [string] $Repo) {
    $keys = Join-Path $Repo "components\sdwall_device_auth\keys"
    $testLine = (Get-Content (Join-Path $keys "test\response-signing-test-public.pem"))[1]
    $releaseLine = (Get-Content (Join-Path $keys "response-signing-public.pem"))[1]
    $hasTest = Select-String -Path $Exe -Pattern $testLine -SimpleMatch -Quiet
    $hasRelease = Select-String -Path $Exe -Pattern $releaseLine -SimpleMatch -Quiet
    if ($hasTest -and -not $hasRelease) { return 'test' }
    if ($hasRelease -and -not $hasTest) { return 'release' }
    return 'unknown'
}
```

- [ ] **Step 2: `device_auth_check.ps1`**

```powershell
<#
.SYNOPSIS
  장비 인증 전달을 사람 없이 확인한다 (servo-handoff/servo-device-auth.md 3절).
.DESCRIPTION
  경우마다 DeviceService fake-auth 를 띄우고, winit_wall 로 확인 페이지를 열고, 엔진 로그의
  `[device-auth-check]` 줄과 자가 시험 판정으로 PASS/FAIL 을 낸다.
    시험 키 빌드: ok / invalid / unreachable (각각 새로고침·이동·# 자가 시험) + unreachable->ok
    실제 키 빌드: 서명 틀림 + 자가 시험 인자 거부
  먼저 `. ..\scripts\servo_env.ps1` 로 DLL PATH 를 잡는다.
#>
param(
    [string] $Exe = "",
    [string] $DeviceService = "",
    [string] $Layout = "",
    [string] $OutDir = "",
    [int] $TimeoutSec = 90
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$root = Split-Path $repo -Parent
if (!$Exe) { $Exe = Join-Path $repo "target\release\examples\winit_wall.exe" }
if (!$DeviceService) { $DeviceService = Join-Path $root "servo-handoff\DeviceService\DeviceService.exe" }
if (!$Layout) { $Layout = Join-Path $root "config\wall_layout.local_3x1.json" }
if (!$OutDir) { $OutDir = Join-Path $root "device_auth_check" }
$testKey = Join-Path $root "servo-handoff\keys\test\response-signing-test-private.pem"
. (Join-Path $PSScriptRoot "device_auth_key_kind.ps1")
New-Item -ItemType Directory -Force $OutDir | Out-Null

$kind = Get-DeviceAuthKeyKind -Exe $Exe -Repo $repo
Write-Host "engine=$Exe key=$kind"
if ($kind -eq 'unknown') { throw "cannot tell which device-auth key $Exe was built with" }

$fakes = New-Object System.Collections.ArrayList
$results = New-Object System.Collections.ArrayList

function Wait-Port([int] $Port) {
    for ($i = 0; $i -lt 100; $i++) {
        try { $c = New-Object Net.Sockets.TcpClient('127.0.0.1', $Port); $c.Close(); return }
        catch { Start-Sleep -Milliseconds 200 }
    }
    throw "fake-auth did not open port $Port"
}

function Start-FakeAuth([string] $Name, [string[]] $FakeArgs, [int] $Port) {
    $a = @('fake-auth') + $FakeArgs + @('--port', "$Port", '--key', $testKey)
    $p = Start-Process -FilePath $DeviceService -ArgumentList $a -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput (Join-Path $OutDir "$Name.fake$Port.out.log") `
        -RedirectStandardError (Join-Path $OutDir "$Name.fake$Port.err.log")
    [void]$fakes.Add($p)
    Wait-Port $Port
}

function Stop-FakeAuth {
    foreach ($p in $fakes) { if (!$p.HasExited) { Stop-Process -Id $p.Id -Force } }
    $fakes.Clear()
    Start-Sleep -Milliseconds 500
}

# 엔진을 띄우고 스스로 끝날 때까지 기다린다(자가 시험 판정 또는 --capture-sec).
# 강제 종료하면 stderr 가 비므로 시간 초과는 그 자체로 실패다.
function Invoke-Engine([string] $Name, [string] $Url, [string[]] $Extra) {
    $log = Join-Path $OutDir "$Name.err.log"
    $out = Join-Path $OutDir "$Name.out.log"
    $a = @('--wall-layout', $Layout, '--wall-tile-index', '0') + $Extra + @($Url)
    $env:RUST_LOG = 'warn'
    $p = Start-Process -FilePath $Exe -ArgumentList $a -PassThru `
        -RedirectStandardError $log -RedirectStandardOutput $out
    $timedOut = -not $p.WaitForExit($TimeoutSec * 1000)
    if ($timedOut) { Stop-Process -Id $p.Id -Force }
    $lines = @(Get-Content $log -Encoding UTF8) + @(Get-Content $out -Encoding UTF8)
    return @{ Log = $log; TimedOut = $timedOut; Lines = $lines }
}

function Get-Notices($Run) { @($Run.Lines | Where-Object { $_ -match '\[device-auth-check\] 알림' }) }

function Add-Result([string] $Name, [bool] $Pass, [string] $Detail) {
    [void]$results.Add([pscustomobject]@{ Case = $Name; Result = $(if ($Pass) { 'PASS' } else { 'FAIL' }); Detail = $Detail })
}

# 공통 위생: 패닉 없음, 지문 없음, 모양이 다른 값 없음.
function Test-Hygiene($Run) {
    $bad = @($Run.Lines | Where-Object { $_ -match 'panicked|fingerprint|모양이 다름' })
    return $bad.Count -eq 0
}

# 자가 시험 경우: 새 문서 셋 모두 기대 값, # 이동 뒤 추가 주입 없음.
function Test-SelfTestCase([string] $Name, [string[]] $FakeArgs, [int] $FakePort, [int] $PagePort, [string] $State, [string] $ProjectKey) {
    try {
        Start-FakeAuth $Name $FakeArgs $FakePort
        $run = Invoke-Engine $Name "http://127.0.0.1:$PagePort/ipwall/device-auth-check" @('--device-auth-selftest')
    } finally { Stop-FakeAuth }
    $notices = Get-Notices $run
    $want = @($notices | Where-Object {
        $_ -match ('"state":"' + $State + '"') -and $_ -match '맞음' -and
        ($ProjectKey -eq '' -or $_ -match ('"projectKey":"' + $ProjectKey + '"'))
    })
    $pass = (-not $run.TimedOut) -and
        (@($run.Lines | Where-Object { $_ -match 'device-auth selftest: PASS' }).Count -eq 1) -and
        ($want.Count -ge 3) -and ($want.Count -eq $notices.Count) -and (Test-Hygiene $run)
    Add-Result $Name $pass "notices=$($notices.Count) expected=$($want.Count) timedOut=$($run.TimedOut) log=$($run.Log)"
}

if ($kind -eq 'test') {
    Test-SelfTestCase 'ok' @('--state', 'ok', '--project-key', '123412') 7250 7250 'ok' '123412'
    Test-SelfTestCase 'invalid' @('--state', 'invalid') 7250 7250 'invalid' ''
    # 7250 을 비우고 확인 페이지는 7251 에서 연다. 같은지 판정은 보지 않는다(모양만).
    Test-SelfTestCase 'unreachable' @('--state', 'invalid') 7251 7251 'unreachable' ''

    # unreachable -> ok: 엔진이 뜬 뒤 7250 에 fake-auth 를 띄운다. 같은 문서에 값이 바뀌어 다시 들어와야 한다.
    $name = 'unreachable_to_ok'
    $job = $null
    try {
        Start-FakeAuth $name @('--state', 'invalid') 7251
        $png = Join-Path $OutDir "$name.png"
        $job = Start-Job -ScriptBlock {
            param($ds, $key, $dir)
            Start-Sleep -Seconds 8
            & $ds fake-auth --state ok --project-key 123412 --port 7250 --key $key *> (Join-Path $dir 'unreachable_to_ok.fake7250.log')
        } -ArgumentList $DeviceService, $testKey, $OutDir
        $run = Invoke-Engine $name "http://127.0.0.1:7251/ipwall/device-auth-check" @('--capture', $png, '--capture-sec', '20')
    } finally {
        if ($job) { Stop-Job $job -ErrorAction SilentlyContinue; Remove-Job $job -Force -ErrorAction SilentlyContinue }
        Get-Process DeviceService -ErrorAction SilentlyContinue | Stop-Process -Force
        Stop-FakeAuth
    }
    $notices = [string[]](Get-Notices $run)
    $u = [array]::FindIndex($notices, [Predicate[string]]{ param($l) $l -match '"state":"unreachable"' })
    $o = [array]::FindIndex($notices, [Predicate[string]]{ param($l) $l -match '"state":"ok"' -and $l -match '"projectKey":"123412"' })
    Add-Result $name (($u -ge 0) -and ($o -gt $u) -and (Test-Hygiene $run)) "unreachable@$u ok@$o log=$($run.Log)"
}
else {
    # 서명 틀림: 실제 키 빌드는 시험 키가 서명한 "ok"를 invalid 로 넘긴다.
    $name = 'signature_mismatch'
    try {
        Start-FakeAuth $name @('--state', 'ok', '--project-key', '123412') 7250
        $run = Invoke-Engine $name "http://127.0.0.1:7250/ipwall/device-auth-check" @('--capture', (Join-Path $OutDir "$name.png"), '--capture-sec', '12')
    } finally { Stop-FakeAuth }
    $notices = Get-Notices $run
    $invalid = @($notices | Where-Object { $_ -match '"state":"invalid"' })
    $ok = @($notices | Where-Object { $_ -match '"state":"ok"' })
    Add-Result $name (($invalid.Count -ge 1) -and ($ok.Count -eq 0) -and (Test-Hygiene $run)) "invalid=$($invalid.Count) ok=$($ok.Count) log=$($run.Log)"

    # 실제 키 빌드는 자가 시험 인자를 받지 않는다.
    $name = 'selftest_rejected'
    $run = Invoke-Engine $name "http://127.0.0.1:7250/ipwall/device-auth-check" @('--device-auth-selftest')
    $rejected = @($run.Lines | Where-Object { $_ -match 'unknown argument: --device-auth-selftest' }).Count -ge 1
    Add-Result $name ($rejected -and -not $run.TimedOut) "log=$($run.Log)"
}

$results | Format-Table -AutoSize | Out-String | Write-Host
if (@($results | Where-Object { $_.Result -ne 'PASS' }).Count -gt 0) { exit 1 }
exit 0
```

실행 전 7250/7251에 다른 DeviceService가 떠 있으면 먼저 끈다. 스크립트가 PowerShell 문법 오류 없이 도는지 먼저 시험 키 빌드로 확인한다.

- [ ] **Step 3: 시험 키 빌드로 실행**

```powershell
. ..\scripts\servo_env.ps1
$env:SDWALL_DEVICE_AUTH_SIGNING_KEY = 'test'
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
.\etc\multigpu\device_auth_check.ps1
```
Expected: `ok`, `invalid`, `unreachable`, `unreachable_to_ok` 모두 PASS, 종료 코드 0. FAIL이면 해당 `.err.log`를 읽고 원인을 고친 뒤(systematic-debugging) 다시 돌린다. 판정 기준을 느슨하게 바꾸어 통과시키지 않는다.

- [ ] **Step 4: 실제 키 빌드로 실행**

```powershell
Remove-Item Env:SDWALL_DEVICE_AUTH_SIGNING_KEY
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
.\etc\multigpu\device_auth_check.ps1
```
Expected: `signature_mismatch`, `selftest_rejected` PASS, 종료 코드 0. 이 빌드를 그대로 둔다(작업 트리의 exe가 실제 키 빌드인 상태로 끝낸다).

- [ ] **Step 5: Commit**

```bash
git add etc/multigpu/device_auth_key_kind.ps1 etc/multigpu/device_auth_check.ps1
git commit -m "test(device-auth): fake-auth 경우별 전달을 사람 없이 확인하는 스크립트

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 9: 배포 가드, 개발자 도구 안내, 문서

**Files:**
- Modify: `etc/multigpu/make_wall_dist.ps1` (`param(` 블록 38행 근처, exe 존재 확인 53행 뒤)
- Modify: `etc/multigpu/run_wall_dist.ps1` (`-Devtools` 처리 793행 근처)
- Create: `docs/multigpu/device_auth.md`

**Interfaces:**
- Consumes: `Get-DeviceAuthKeyKind` (Task 8)

- [ ] **Step 1: 배포 가드**

`make_wall_dist.ps1` `param(`에:
```powershell
    # 시험 서명 키로 빌드된 엔진을 일부러 묶을 때만 (현장 배포 금지).
    [switch]$AllowTestSigningKey,
```
53행(`if (!(Test-Path $exe)) ...`) 뒤:
```powershell
# ★시험 서명 키 빌드를 배포하지 않는다.★ 키는 빌드 때 박히므로(SDWALL_DEVICE_AUTH_SIGNING_KEY)
# exe 안의 PEM 본문으로 가른다. 시험 키 빌드는 fake-auth 가 서명한 "인증 정상"을 받아들인다.
. (Join-Path $PSScriptRoot "device_auth_key_kind.ps1")
$deviceAuthKey = Get-DeviceAuthKeyKind -Exe $exe -Repo $repo
if ($deviceAuthKey -ne 'release') {
    if (-not $AllowTestSigningKey) {
        throw "winit_wall.exe device-auth signing key is '$deviceAuthKey', not 'release'. Rebuild without SDWALL_DEVICE_AUTH_SIGNING_KEY (or pass -AllowTestSigningKey for a lab package)."
    }
    Write-Warning "Packaging a '$deviceAuthKey'-signing-key engine (-AllowTestSigningKey). Not for site deployment."
}
```
(`$repo`가 그 스크립트에서 다른 이름이면 맞춘다. `device_auth_key_kind.ps1`은 묶기 전에만 쓰므로 배포 폴더에 넣지 않는다.)

- [ ] **Step 2: 개발자 도구 안내**

`run_wall_dist.ps1`의 `if ($Devtools -ne "") {` 블록 안 맨 앞:
```powershell
    # 현장 배포에서는 개발자 도구를 끈다(장비 인증 결정, docs/multigpu/device_auth.md).
    # 켜져 있으면 콘솔에서 window.sdwallDeviceAuth 를 직접 넣어 /output 을 표출시킬 수 있다.
    Write-Warning "Devtools ON ($Devtools): development only -- never on a site deployment (device auth can be forged from the console)."
```

- [ ] **Step 3: 가드 확인 (자동)**

실제 키 빌드(Task 8 Step 4 상태)로 `make_wall_dist.ps1`을 평소 인자로 돌려 가드를 지나는지 확인한다. 이어서 시험 키로 다시 빌드해 같은 명령이 `device-auth signing key is 'test'` 예외로 멈추는지 확인하고, **실제 키로 다시 빌드해 둔다**.

- [ ] **Step 4: 문서 `docs/multigpu/device_auth.md`**

담을 것(한국어, 근거와 함께):
- 흐름: 폴러 스레드(127.0.0.1:7250, 3초 간격, 2초 한도, 정상이면 멈춤) → `WakerEvent::DeviceAuth` → `InjectionTracker` → `evaluate_javascript`. 지문은 넘기지도 로그에 남기지도 않는다.
- 범위: 시작 URL 오리진 + `/ipwall/`, `/portal/`. 주입 스크립트 자체 가드(`window.top`, `location.origin`, 경로).
- 키: 기본 실제 키. `SDWALL_DEVICE_AUTH_SIGNING_KEY=test` 빌드에서만 시험 키. 다른 값이면 빌드 실패. `make_wall_dist.ps1` 가드.
- 개발자 도구: 현장 배포는 끈다(명세 4절 답, 2026-10-13). `run_wall_dist.ps1 -Devtools`는 개발 전용이며 경고를 낸다.
- 확인: `cargo test -p sdwall-device-auth --lib`, 라이브 `-- --ignored live_fake_auth_repeated`, `etc/multigpu/device_auth_check.ps1`(시험 키 빌드·실제 키 빌드 각 1회)과 Task 8 실행 결과 표.
- 자가 시험 `--device-auth-selftest`: 시험 키 빌드 전용. 순서와 판정 기준.

- [ ] **Step 5: 최종 점검**

```powershell
cargo test -p sdwall-device-auth --lib
cargo test -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
rustfmt --edition 2024 --check components/sdwall_device_auth/*.rs components/servo/examples/winit_wall/main.rs components/servo/examples/winit_wall/device_auth_selftest.rs
git diff --check
```
Expected: 모두 통과, 출력 없음.

- [ ] **Step 6: Commit**

```bash
git add etc/multigpu/make_wall_dist.ps1 etc/multigpu/run_wall_dist.ps1 docs/multigpu/device_auth.md
git commit -m "docs(device-auth): 시험 키 빌드 배포를 막고 현장 개발자 도구 끄기를 남긴다

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```
