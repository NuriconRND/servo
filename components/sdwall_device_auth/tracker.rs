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
        t.on_injection_failed();
        assert_eq!(t.on_document_event(Complete), Some(ok()));
    }
}
