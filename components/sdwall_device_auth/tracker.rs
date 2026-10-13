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
    /// 문서 세대. `Started` 마다 증가한다. 늦게 도착한 실패가 앞 문서의 것인지 가리는 데 쓴다.
    document: u64,
}

impl InjectionTracker {
    /// 현재 문서 세대. 주입을 보내기 직전에 읽어 두었다가 실패할 때 `on_injection_failed` 에 넘긴다.
    pub fn document(&self) -> u64 {
        self.document
    }

    pub fn on_document_event(&mut self, event: DocumentEvent) -> Option<AuthState> {
        match event {
            DocumentEvent::Started => {
                self.document = self.document.wrapping_add(1);
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

    /// 주입 실패를 알리면 다음 `HeadParsed`/`Complete` 에서 다시 넣게 한다.
    ///
    /// `document` 는 실패한 주입을 보낼 때의 세대다. 그 뒤 새 문서가 시작됐다면(세대가 다르면)
    /// 앞 문서의 늦은 실패이므로 무시한다. 새 문서는 이미 자기 값을 받았을 수 있다.
    ///
    /// `Complete` 이후의 실패는 재시도하지 않는다. 그 시점에 문서는 이미 있으므로 실패는
    /// 둘 중 하나다. 문서가 교체됐다면 그 `Started` 가 다시 넣고, 아니라면 스크립트 오류라서
    /// 재시도해도 같은 결과가 나온다.
    pub fn on_injection_failed(&mut self, document: u64) {
        if document != self.document {
            return;
        }
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

#[cfg(test)]
mod tests {
    use super::DocumentEvent::*;
    use super::*;

    fn ok() -> AuthState {
        AuthState::Ok {
            project_key: "123412".into(),
        }
    }

    #[test]
    fn nothing_before_first_result() {
        let mut t = InjectionTracker::default();
        assert_eq!(t.on_document_event(Started), None);
        assert_eq!(t.on_document_event(HeadParsed), None);
        assert_eq!(t.on_document_event(Complete), None);
        // 결과가 오면 이미 준비된 문서에 넣는다.
        assert_eq!(
            t.on_auth_state(AuthState::Unreachable),
            Some(AuthState::Unreachable)
        );
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
        assert_eq!(
            t.on_auth_state(AuthState::Unreachable),
            Some(AuthState::Unreachable)
        );
        assert_eq!(t.on_auth_state(AuthState::Unreachable), None);
        assert_eq!(
            t.on_auth_state(AuthState::Invalid),
            Some(AuthState::Invalid)
        );
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
        t.on_injection_failed(t.document());
        assert_eq!(t.on_document_event(Complete), Some(ok()));
    }

    #[test]
    fn stale_failure_from_previous_document_is_ignored() {
        let mut t = InjectionTracker::default();
        t.on_auth_state(ok());
        t.on_document_event(Started);
        let a = t.document();
        // 문서 A 의 주입이 날아간 뒤 문서 B 가 시작되고 값을 받는다.
        t.on_document_event(Started);
        assert_eq!(t.on_document_event(HeadParsed), Some(ok()));
        // 문서 A 의 늦은 실패는 B 의 주입 상태를 건드리지 않는다.
        t.on_injection_failed(a);
        assert_eq!(t.on_document_event(Complete), None);
    }

    #[test]
    fn started_advances_document() {
        let mut t = InjectionTracker::default();
        let before = t.document();
        t.on_document_event(Started);
        assert_ne!(t.document(), before);
    }

    #[test]
    fn failure_after_complete_waits_for_next_document() {
        let mut t = InjectionTracker::default();
        t.on_auth_state(ok());
        t.on_document_event(Started);
        assert_eq!(t.on_document_event(HeadParsed), Some(ok()));
        assert_eq!(t.on_document_event(Complete), None);
        t.on_injection_failed(t.document());
        // Complete 이후 실패는 같은 문서에서 재시도하지 않는다.
        assert_eq!(t.on_auth_state(ok()), None);
        // 다음 문서가 시작되면 다시 넣는다.
        t.on_document_event(Started);
        assert_eq!(t.on_document_event(HeadParsed), Some(ok()));
    }
}
