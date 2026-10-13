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
