//! View state that lives in the address bar (UX plan §4 "URLs"): a path
//! segment picks the view, the query string holds filters and sorting.
//!
//! This is the Traffic filter pattern (controls are the source of truth while
//! typing, the URL follows them, a URL that changes underneath is read back)
//! made reusable, so a filtered view is something the owner can bookmark,
//! reload or be linked to from another page.

// The area phases adopt these page by page; nothing reads them yet.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::HashMap;

use leptos::prelude::*;
use leptos_router::hooks::{query_signal_with_options, use_location, use_params_map};
use leptos_router::NavigateOptions;

thread_local! {
    /// The query string each path was last shown with, for this session.
    static LAST_QUERY: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

/// Keep, for every path, the query string it was last shown with — so a tab
/// that leads back to it can restore it ([`with_last_query`]). Once per app,
/// inside the router.
pub fn remember_queries() {
    let loc = use_location();
    Effect::new(move |_| {
        let path = loc.pathname.get();
        let search = loc.search.get();
        LAST_QUERY.with(|m| {
            m.borrow_mut().insert(path, search);
        });
    });
}

/// `path` with the query string it was last shown with: a filtered view left
/// for another tab comes back filtered (review code:U1). A path never shown,
/// or shown bare, is returned as it is; so is one that brings a query of its
/// own.
pub fn with_last_query(path: &str) -> String {
    if path.contains('?') {
        return path.to_string();
    }
    LAST_QUERY.with(|m| match m.borrow().get(path) {
        Some(q) if !q.is_empty() => format!("{path}?{q}"),
        _ => path.to_string(),
    })
}

/// One query parameter as a two-way string signal. `""` means the parameter
/// is absent from the URL.
///
/// Writes `replace` history entries with no scroll jump, keeping the path,
/// the hash and every other parameter — typing in a filter box must not leave
/// one history entry per keystroke. The router batches writes made in the
/// same frame, so two signals cleared by one "Clear filters" both land.
pub fn use_query_signal(param: &'static str) -> RwSignal<String> {
    let (url, set_url) = query_signal_with_options::<String>(
        param,
        NavigateOptions {
            replace: true,
            scroll: false,
            ..Default::default()
        },
    );
    let sig = RwSignal::new(url.get_untracked().unwrap_or_default());
    // URL → signal: a link, back/forward, or another control rewrote it.
    Effect::new(move |_| {
        let v = url.get().unwrap_or_default();
        if v != sig.get_untracked() {
            sig.set(v);
        }
    });
    // Signal → URL. Guarded on "actually different", so the two effects
    // settle instead of chasing each other.
    Effect::new(move |_| {
        let v = sig.get();
        if v == url.get_untracked().unwrap_or_default() {
            return;
        }
        set_url.set((!v.is_empty()).then_some(v));
    });
    sig
}

/// The view a `:tab?` path parameter names, as one of `allowed`. A missing or
/// unknown segment is `default`, so an old or mistyped link still lands on a
/// page rather than an empty one.
pub fn use_view(
    param: &'static str,
    allowed: &'static [&'static str],
    default: &'static str,
) -> Memo<&'static str> {
    let params = use_params_map();
    Memo::new(move |_| params.with(|p| pick_view(p.get_str(param), allowed, default)))
}

fn pick_view(
    seg: Option<&str>,
    allowed: &'static [&'static str],
    default: &'static str,
) -> &'static str {
    seg.and_then(|s| allowed.iter().copied().find(|a| *a == s))
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABS: &[&str] = &["requests", "conversations"];

    #[test]
    fn a_known_segment_picks_its_view() {
        assert_eq!(
            pick_view(Some("conversations"), TABS, "requests"),
            "conversations"
        );
    }

    #[test]
    fn a_missing_or_unknown_segment_falls_back() {
        assert_eq!(pick_view(None, TABS, "requests"), "requests");
        assert_eq!(pick_view(Some("nope"), TABS, "requests"), "requests");
        assert_eq!(pick_view(Some(""), TABS, "requests"), "requests");
    }
}
