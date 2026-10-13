use p256::ecdsa::VerifyingKey;
use p256::pkcs8::DecodePublicKey;

// ***빌드할 때 넣는다.*** 설정 파일이나 현장 값으로 바꿀 길을 만들지 않는다 -- 그 길이
// 있으면 누구나 자기 키로 서명한 "인증 정상"을 넣을 수 있다.
#[cfg(not(sdwall_test_signing_key))]
const EMBEDDED_PEM: &str = include_str!("keys/response-signing-public.pem");
#[cfg(sdwall_test_signing_key)]
const EMBEDDED_PEM: &str = include_str!("keys/test/response-signing-test-public.pem");

/// 이 빌드에 박힌 키가 무엇인지. 시험 키 빌드를 배포에서 걸러내는 데 쓴다.
pub const SIGNING_KEY_KIND: &str = if cfg!(sdwall_test_signing_key) {
    "test"
} else {
    "release"
};

pub fn parse_public_key_pem(pem: &str) -> Result<VerifyingKey, String> {
    VerifyingKey::from_public_key_pem(pem).map_err(|error| error.to_string())
}

pub fn embedded_verifying_key() -> VerifyingKey {
    parse_public_key_pem(EMBEDDED_PEM).expect("embedded device-auth public key must parse")
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_key_parses() {
        let _ = super::embedded_verifying_key();
        assert!(matches!(super::SIGNING_KEY_KIND, "release" | "test"));
    }
}
