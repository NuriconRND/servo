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

    /// 시험 전용: 아래 스크립트 가드(`window.top`·`location.origin`·경로)를 Rust 로 옮긴 거울.
    ///
    /// ***엔진 쪽에서 이것으로 주입을 거르지 않는다.*** `webview.url()` 은 실제로 스크립트를
    /// 실행할 문서보다 늦을 수 있어, 거르면 정당한 주입을 버린다. 집행은 스크립트 가드
    /// 하나뿐이다 -- 실행되는 바로 그 문서에서 돌고, 위조할 수 없는 속성(`top`, `origin`)을
    /// 먼저 보고 거기서 끊는다. 이 함수는 그 규칙을 시험으로 고정하는 데만 쓴다.
    #[cfg(test)]
    pub fn contains(&self, url: &Url) -> bool {
        url.origin().ascii_serialization() == self.origin
            && SCOPED_PATH_PREFIXES
                .iter()
                .any(|prefix| url.path().starts_with(prefix))
    }

    /// 페이지 안에서 실행할 스크립트.
    ///
    /// ***가드를 스크립트 안에도 둔다.*** `evaluate_javascript` 는 내보낸 순간과 실행되는
    /// 순간 사이에 문서가 바뀔 수 있다(이동 직후). 실행되는 문서가 스스로 범위를 확인해야
    /// 다른 오리진에 값이 새지 않는다.
    ///
    /// ***같은 문서에 같은 값이면 다시 쓰지도 알리지도 않는다(`'same'`).*** WebView 는 URL 없이
    /// 만들어져 about:blank 가 먼저 뜨고, 그 문서로 추적기가 "넣을 준비"가 된다. 실제 문서의
    /// `LoadStatus::Started` 는 `spin_event_loop` 안에서야 셸에 오는데, 그 전에 콘스텔레이션은
    /// 이미 실제 문서를 활성화했을 수 있다. 그 사이에 결과가 오면 실제 문서에 한 번 들어가고
    /// (#1), 뒤이은 Started+HeadParsed 로 또 들어간다(#2) -- 한 문서에 알림이 두 번 난다.
    /// 셸은 어느 문서가 실행할지 확실히 모르므로, 실행하는 문서가 스스로 거른다.
    /// 비교는 가드 ***뒤***다 -- 범위 밖 문서는 무엇이 들어 있든 `'skip'` 이다.
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
var v={value},c=window.sdwallDeviceAuth;\
if(c!==null&&typeof c==='object'&&c.state===v.state&&c.projectKey===v.projectKey)return 'same';\
window.sdwallDeviceAuth=v;\
window.dispatchEvent(new Event('sdwall-device-auth'));\
return 'set';\
}})()"
        )
    }
}

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
        let s = PageScope::from_start_url(&u("http://127.0.0.1:7250/ipwall/device-auth-check"))
            .unwrap();
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
        let ok = s.injection_script(&AuthState::Ok {
            project_key: "123412".into(),
        });
        assert!(
            ok.contains(r#"var v={"projectKey":"123412","state":"ok"}"#),
            "{ok}"
        );
        assert!(ok.contains("window.sdwallDeviceAuth=v;"));
        assert!(ok.contains("window.dispatchEvent(new Event('sdwall-device-auth'))"));
        assert!(ok.contains(r#"location.origin!=="https://10.0.0.5:7245""#));
        assert!(ok.contains("window.top!==window"));
        assert!(!ok.contains("HJY0BWGD"));
        let invalid = s.injection_script(&AuthState::Invalid);
        assert!(
            invalid.contains(r#"var v={"state":"invalid"}"#),
            "{invalid}"
        );
        let unreachable = s.injection_script(&AuthState::Unreachable);
        assert!(unreachable.contains(r#"var v={"state":"unreachable"}"#));
    }

    /// 같은 문서에 같은 값이 두 번 오면(about:blank 다음 실제 문서 시작이 늦게 알려질 때)
    /// 두 번째는 쓰지도 알리지도 않고 `'same'` 을 돌려준다. 가드보다 ***뒤***에 있어야 한다 --
    /// 범위 밖 문서는 무엇이 들어 있든 `'skip'` 이다.
    #[test]
    fn same_value_in_same_document_is_not_reassigned_or_redispatched() {
        for state in [
            AuthState::Ok {
                project_key: "123412".into(),
            },
            AuthState::Invalid,
            AuthState::Unreachable,
        ] {
            let script = prod().injection_script(&state);
            let at = |needle: &str| {
                script
                    .find(needle)
                    .unwrap_or_else(|| panic!("`{needle}` missing in {script}"))
            };
            let skip = at("return 'skip';");
            let compare = at("c.state===v.state&&c.projectKey===v.projectKey");
            let same = at("return 'same';");
            let assign = at("window.sdwallDeviceAuth=v;");
            let dispatch = at("window.dispatchEvent(");
            let set = at("return 'set';");
            assert!(skip < compare && compare < same, "{script}");
            assert!(
                same < assign && assign < dispatch && dispatch < set,
                "{script}"
            );
            // 이미 있는 값은 쓰기 전에 읽는다.
            assert!(at("c=window.sdwallDeviceAuth") < compare, "{script}");
        }
    }

    #[test]
    fn project_key_is_json_escaped() {
        let script = prod().injection_script(&AuthState::Ok {
            project_key: "1\"</script>'\\".into(),
        });
        assert!(
            script.contains(r#""projectKey":"1\"</script>'\\""#),
            "{script}"
        );
    }
}
