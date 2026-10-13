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
            ok.contains(r#"window.sdwallDeviceAuth={"projectKey":"123412","state":"ok"}"#),
            "{ok}"
        );
        assert!(ok.contains("window.dispatchEvent(new Event('sdwall-device-auth'))"));
        assert!(ok.contains(r#"location.origin!=="https://10.0.0.5:7245""#));
        assert!(ok.contains("window.top!==window"));
        assert!(!ok.contains("HJY0BWGD"));
        let invalid = s.injection_script(&AuthState::Invalid);
        assert!(
            invalid.contains(r#"window.sdwallDeviceAuth={"state":"invalid"}"#),
            "{invalid}"
        );
        let unreachable = s.injection_script(&AuthState::Unreachable);
        assert!(unreachable.contains(r#"window.sdwallDeviceAuth={"state":"unreachable"}"#));
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
