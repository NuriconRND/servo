//! SDWall 장비 인증: DeviceService 에 묻고, 서명을 확인하고, 결과를 페이지로 넘긴다.
//!
//! 명세: servo-handoff/servo-device-auth.md. 엔진에 의존하지 않는 부분만 여기 둔다 --
//! 셸은 폴러를 띄우고, 문서 이벤트를 `InjectionTracker` 에 넣고, 나온 스크립트를 실행한다.

mod client;
mod key;
mod page;
mod poller;
mod tracker;
mod verify;

pub use client::{DEVICE_SERVICE_ADDR, new_nonce, query_device_auth};
pub use key::{SIGNING_KEY_KIND, embedded_verifying_key, parse_public_key_pem};
pub use p256::ecdsa::VerifyingKey;
pub use page::{PageScope, SCOPED_PATH_PREFIXES};
pub use poller::{QUERY_TIMEOUT, RETRY_INTERVAL, run_poll_loop, spawn_poller};
pub use tracker::{DocumentEvent, InjectionTracker};
pub use verify::{AuthState, DeviceAuthResponse, verify_response};
