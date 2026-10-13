use std::net::SocketAddr;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::{AuthState, DEVICE_SERVICE_ADDR, embedded_verifying_key, query_device_auth};

/// Interval between re-queries while authentication is not OK.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(3);
/// Connect+response time limit for a single query; exceeding it means "unreachable".
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// Reports only changed results and stops once authentication is OK.
///
/// DeviceService decides once at startup, so there is no reason to ask again after OK.
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    fn ok() -> AuthState {
        AuthState::Ok {
            project_key: "123412".into(),
        }
    }

    fn run(script: Vec<AuthState>) -> (Vec<AuthState>, usize) {
        let mut queue: VecDeque<_> = script.into();
        let mut changes = Vec::new();
        let mut sleeps = 0;
        run_poll_loop(
            || queue.pop_front().expect("polled after Ok"),
            |d| {
                assert_eq!(d, RETRY_INTERVAL);
                sleeps += 1
            },
            |s| changes.push(s),
            RETRY_INTERVAL,
        );
        (changes, sleeps)
    }

    #[test]
    fn reports_only_changes_and_stops_at_ok() {
        use AuthState::*;
        let (changes, sleeps) = run(vec![
            Unreachable,
            Unreachable,
            Invalid,
            Invalid,
            Unreachable,
            ok(),
        ]);
        assert_eq!(changes, vec![Unreachable, Invalid, Unreachable, ok()]);
        assert_eq!(sleeps, 5);
    }

    #[test]
    fn ok_first_try_reports_once_without_sleeping() {
        assert_eq!(run(vec![ok()]), (vec![ok()], 0));
    }

    /// Live check against the real DeviceService (high-S signatures, Kestrel framing).
    /// Start first: DeviceService.exe fake-auth --state ok --project-key 123412
    /// Run: cargo test -p sdwall-device-auth --lib -- --ignored live_fake_auth_repeated
    #[test]
    #[ignore]
    fn live_fake_auth_repeated() {
        let key =
            crate::parse_public_key_pem(include_str!("keys/test/response-signing-test-public.pem"))
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
