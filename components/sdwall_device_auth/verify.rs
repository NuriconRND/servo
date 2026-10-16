use std::fmt;

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

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceAuthResponse {
    pub state: String,
    pub fingerprint: String,
    pub project_key: String,
    pub nonce: String,
    pub signature: String,
}

/// 지문은 `{:?}` 로도 새지 않게 가린다(명세: 로그에 남기지 않는다).
impl fmt::Debug for DeviceAuthResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceAuthResponse")
            .field("state", &self.state)
            .field("fingerprint", &"<redacted>")
            .field("project_key", &self.project_key)
            .field("nonce", &self.nonce)
            .field("signature", &self.signature)
            .finish()
    }
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
    if key.verify(message.as_bytes(), &signature).is_err()
        || response.nonce != sent_nonce
        || response.state != "ok"
    {
        return AuthState::Invalid;
    }
    AuthState::Ok {
        project_key: response.project_key.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_public_key_pem;

    const TEST_PEM: &str = include_str!("keys/test/response-signing-test-public.pem");
    const VECTORS: &str = include_str!("keys/test/device-auth-vectors.json");

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Vector {
        name: String,
        sent_nonce: String,
        expected: String,
        response: DeviceAuthResponse,
    }

    #[derive(serde::Deserialize)]
    struct Vectors {
        vectors: Vec<Vector>,
    }

    #[test]
    fn all_six_vectors_match_expected() {
        let key = parse_public_key_pem(TEST_PEM).unwrap();
        let vectors: Vectors = serde_json::from_str(VECTORS).unwrap();
        assert_eq!(vectors.vectors.len(), 6);
        for v in vectors.vectors {
            let got = verify_response(&key, &v.sent_nonce, &v.response);
            assert_eq!(got.label(), v.expected, "vector `{}`", v.name);
            if v.expected == "ok" {
                assert_eq!(
                    got,
                    AuthState::Ok {
                        project_key: v.response.project_key.clone()
                    }
                );
            }
        }
    }

    #[test]
    fn debug_redacts_fingerprint() {
        let response = DeviceAuthResponse {
            state: "ok".into(),
            fingerprint: "HJY0BWGD".into(),
            project_key: "123412".into(),
            nonce: "a1B2c3D4e5F6g7H8".into(),
            signature: "c2ln".into(),
        };
        let debug = format!("{response:?}");
        assert!(!debug.contains("HJY0BWGD"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(
            debug.contains("123412") && debug.contains("a1B2c3D4e5F6g7H8"),
            "{debug}"
        );
        let pretty = format!("{response:#?}");
        assert!(!pretty.contains("HJY0BWGD"), "{pretty}");
    }

    #[test]
    fn garbage_signature_is_invalid() {
        let key = parse_public_key_pem(TEST_PEM).unwrap();
        let mut r: Vectors = serde_json::from_str(VECTORS).unwrap();
        let mut resp = r.vectors.remove(0).response;
        for sig in ["", "not base64!", "AAAA", &"A".repeat(200)] {
            resp.signature = sig.to_owned();
            assert_eq!(
                verify_response(&key, "a1B2c3D4e5F6g7H8", &resp),
                AuthState::Invalid,
                "{sig}"
            );
        }
    }

    #[test]
    fn release_key_rejects_test_signed_ok() {
        // 명세 3절 "서명 틀림": 실제 공개 키로는 시험 키 서명이 통과하지 않는다.
        let key = parse_public_key_pem(include_str!("keys/response-signing-public.pem")).unwrap();
        let mut r: Vectors = serde_json::from_str(VECTORS).unwrap();
        let v = r.vectors.remove(0);
        assert_eq!(
            verify_response(&key, &v.sent_nonce, &v.response),
            AuthState::Invalid
        );
    }
}
