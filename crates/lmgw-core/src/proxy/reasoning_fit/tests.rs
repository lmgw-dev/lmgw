//! What a fitted off reports and says (module doc of [`super`]).

use super::refusal::Retry;
use super::{Basis, Fitted, Off};
use crate::config::{Protocol, Route, Upstream, UpstreamKind};

fn fitted(off: Off, basis: Basis) -> Fitted {
    Fitted {
        off: Some((off, basis)),
        reasoned: false,
    }
}

fn lowest(l: &str) -> Off {
    Off::Lowest(l.to_string())
}

/// A local row's route, for the log line `observe` writes.
fn local_route() -> Route {
    Route {
        upstream: Upstream {
            id: 1,
            name: "local".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::LlamaServer,
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 0,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
        upstream_model: "talk".into(),
        param_defaults: Default::default(),
    }
}

#[test]
fn the_last_resort_reports_and_says_the_model_reasons_by_default() {
    let mut f = fitted(Off::Control, Basis::Default);
    assert_eq!(f.ignored(), None);
    f.retried(lowest("minimal"), Retry::Guided);
    assert_eq!(f.sent(), Some((lowest("minimal"), false)));
    f.retried(Off::Omitted, Retry::Fallback);
    assert_eq!(f.sent(), Some((Off::Omitted, true)));
    // Nothing of the off is left to retry.
    assert_eq!(f.retryable(), None);
    assert_eq!(f.ignored(), Some("enabled"));
    assert_eq!(
        f.note("gpt").as_deref(),
        Some(
            "gpt refused every way lmgw has to switch reasoning off; it reasons as it does by \
             default"
        )
    );
    // Remembered, it says the same on the next request.
    let learned = fitted(Off::Omitted, Basis::Learned { fallback: true });
    assert_eq!(learned.note("gpt"), f.note("gpt"));
}

#[test]
fn a_lowest_level_says_so_and_a_model_without_control_says_nothing() {
    let low = fitted(lowest("minimal"), Basis::Default);
    assert_eq!(
        low.note("nano").as_deref(),
        Some("nano cannot switch reasoning off; it reasons at its lowest level (minimal)")
    );
    assert_eq!(low.ignored(), Some("enabled"));
    // No control and no reasoning seen: it does not reason, as far as
    // anything tells — reported (the off was not sent), not said.
    let none = fitted(Off::Omitted, Basis::Learned { fallback: false });
    assert_eq!(none.ignored(), Some("enabled"));
    assert_eq!(none.note("nano41"), None);
    // The off as the protocol spells it: nothing either way.
    let off = fitted(Off::Control, Basis::Facts("x"));
    assert_eq!((off.ignored(), off.note("m")), (None, None));
    assert_eq!(Fitted::default().note("m"), None);
}

#[test]
fn reasoning_seen_after_an_off_is_reported_and_said() {
    let route = local_route();
    // A local template that does not read `enable_thinking`.
    let mut local = fitted(Off::Control, Basis::Local);
    assert_eq!(local.retryable(), None, "a local route never retries");
    local.observe(&route, false);
    assert_eq!((local.ignored(), local.note("talk")), (None, None));
    local.observe(&route, true);
    assert_eq!(local.ignored(), Some("enabled"));
    assert_eq!(
        local.note("talk").as_deref(),
        Some("talk did not switch reasoning off; it reasoned anyway")
    );
    // A model at its lowest level was expected to reason: its own sentence.
    let mut low = fitted(lowest("low"), Basis::Default);
    low.observe(&route, true);
    assert!(low.note("m").unwrap().contains("lowest level (low)"));
    // No off asked: nothing observed.
    let mut none = Fitted::default();
    none.observe(&route, true);
    assert_eq!((none.ignored(), none.note("m")), (None, None));
}
