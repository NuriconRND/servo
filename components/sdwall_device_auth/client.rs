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
    if stream.set_write_timeout(Some(timeout)).is_err()
        || stream.write_all(request.as_bytes()).is_err()
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
                .split("nonce=")
                .nth(1)
                .unwrap_or("")
                .split(|c: char| c == ' ' || c == '&')
                .next()
                .unwrap_or("")
                .to_owned();
            let _ = stream.write_all(&respond(nonce));
        });
        addr
    }

    fn signed_body(state: &str, project_key: &str, nonce: &str) -> String {
        let message = format!("{state}|HJY0BWGD|{project_key}|{nonce}");
        let signature: Signature = signing_key().sign(message.as_bytes());
        let signature = STANDARD.encode(signature.to_bytes());
        format!(
            r#"{{"state":"{state}","fingerprint":"HJY0BWGD","projectKey":"{project_key}","nonce":"{nonce}","signature":"{signature}"}}"#
        )
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
        assert_eq!(
            query_device_auth(addr, &key, T),
            AuthState::Ok {
                project_key: "123412".into()
            }
        );
    }

    #[test]
    fn signed_invalid_is_invalid() {
        let addr = serve_once(|n| http("200 OK", &signed_body("invalid", "", &n)));
        assert_eq!(
            query_device_auth(addr, signing_key().verifying_key(), T),
            AuthState::Invalid
        );
    }

    #[test]
    fn echoing_a_stale_nonce_is_invalid() {
        let addr = serve_once(|_| http("200 OK", &signed_body("ok", "123412", "Z9y8X7w6V5u4T3s2")));
        assert_eq!(
            query_device_auth(addr, signing_key().verifying_key(), T),
            AuthState::Invalid
        );
    }

    #[test]
    fn status_503_is_invalid() {
        let addr = serve_once(|_| http("503 Service Unavailable", ""));
        assert_eq!(
            query_device_auth(addr, signing_key().verifying_key(), T),
            AuthState::Invalid
        );
    }

    #[test]
    fn broken_json_is_invalid() {
        let addr = serve_once(|_| http("200 OK", "{not json"));
        assert_eq!(
            query_device_auth(addr, signing_key().verifying_key(), T),
            AuthState::Invalid
        );
    }

    #[test]
    fn non_http_reply_is_invalid() {
        let addr = serve_once(|_| b"SSH-2.0-OpenSSH\r\n\r\n".to_vec());
        assert_eq!(
            query_device_auth(addr, signing_key().verifying_key(), T),
            AuthState::Invalid
        );
    }

    #[test]
    fn closed_port_is_unreachable() {
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap(); // 리스너는 곧바로 닫힌다
        assert_eq!(
            query_device_auth(addr, signing_key().verifying_key(), T),
            AuthState::Unreachable
        );
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
        assert_eq!(
            query_device_auth(addr, signing_key().verifying_key(), T),
            AuthState::Unreachable
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn closed_before_status_line_is_unreachable() {
        let addr = serve_once(|_| Vec::new());
        assert_eq!(
            query_device_auth(addr, signing_key().verifying_key(), T),
            AuthState::Unreachable
        );
    }
}
