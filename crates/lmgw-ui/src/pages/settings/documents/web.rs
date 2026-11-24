//! Settings › Documents' "Web addresses" (document-conversion design ruling
//! 9, §8): whether tools that used docling may name a web address for lmgw
//! to fetch (`settings_set`'s `documents_fetch_sources`), and, in the expert
//! view, the sites it may fetch from (`documents_source_hosts`). Both apply
//! at once, like "Parallel tool runs". Read from `doc_tools`' `web`.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::documents::WebSources;
use lmgw_api_types::units::fmt_bytes;
use serde_json::json;

use super::use_docs;
use crate::widgets::{use_toasts, Switch};

/// What the switch's row says under its title: what it allows, and who.
pub(super) fn switch_said(w: &WebSources) -> String {
    if w.fetch {
        "On: any caller with an API key can name a web address instead of sending the file, and \
         lmgw downloads it, but only from the allowed sites, and at most the request size limit \
         per request. Callers without a key can't, even while keys are not required."
            .to_string()
    } else {
        "Tools that used docling can name a web address for lmgw to download instead of sending \
         the file. Once this is on, any caller with an API key can; callers without a key can't, \
         even while keys are not required. While it is off they send the file itself, and lmgw \
         tells them so."
            .to_string()
    }
}

/// The warning while the request size limit is 0: web addresses need one.
pub(super) fn no_limit_said(w: &WebSources) -> Option<&'static str> {
    (w.pool.per_fetch_bytes == 0).then_some(
        "The request size limit is 0 (no limit), and web addresses need a set limit, so every \
         web address is refused until one is set under Network & access › Max request body.",
    )
}

/// The fetch pool in a line, for the expert view.
pub(super) fn pool_said(w: &WebSources) -> String {
    let p = &w.pool;
    let mut s = format!(
        "Downloads at once: {}, the same as Parallel tool runs, in a pool of their own. Running \
         now: {}, waiting: {}.",
        p.slots, p.running, p.waiting
    );
    if p.per_fetch_bytes > 0 {
        s.push_str(&format!(
            " At most {} × {} = {} of downloads are held at once.",
            p.slots,
            fmt_bytes(p.per_fetch_bytes),
            fmt_bytes(u64::from(p.slots).saturating_mul(p.per_fetch_bytes))
        ));
    }
    s
}

/// The allowed sites in a line, for the row outside the expert view.
pub(super) fn sites_said(w: &WebSources) -> String {
    if w.hosts.is_empty() {
        "No site is allowed yet, so nothing is downloaded even when this is on. The sites are \
         listed in the expert view."
            .to_string()
    } else {
        format!(
            "Allowed sites: {}. They are edited in the expert view.",
            w.hosts.join(", ")
        )
    }
}

const SITES_HINT: &str = "Only these sites are downloaded from: one per line, a name or an \
     address, with :port when it is not the usual web port (80 for http, 443 for https). \
     Addresses of this computer or the local network are refused even for a listed name, unless \
     that address is listed here too (for example 192.0.2.20:8080). A redirect is followed only \
     to a listed site.";

#[component]
pub(super) fn WebRows(web: WebSources) -> impl IntoView {
    let d = use_docs();
    let toasts = use_toasts();
    let on = RwSignal::new(web.fetch);
    let saved = RwSignal::new(web.fetch);
    let busy = RwSignal::new(false);
    let typed = RwSignal::new(web.hosts.join("\n"));
    let save = move |body: serde_json::Value, done: &'static str| {
        busy.set(true);
        spawn_local(async move {
            match crate::api::post::<serde_json::Value, _>("/api/op/settings_set", &body).await {
                Ok(_) => {
                    toasts.ok(done.to_string());
                    d.load(false);
                }
                Err(e) => {
                    toasts.err(e.to_string());
                    on.try_set(saved.get_untracked());
                }
            }
            busy.try_set(false);
        });
    };
    // The switch saves when flipped (and flips back if the save fails).
    Effect::new(move |_| {
        let v = on.get();
        if v != saved.get_untracked() {
            saved.set(v);
            save(
                json!({ "documents_fetch_sources": v }),
                if v {
                    "Callers can send web addresses from the allowed sites"
                } else {
                    "Web addresses are refused again"
                },
            );
        }
    });
    let set_sites = move |_| {
        let lines: Vec<String> = typed
            .get_untracked()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        save(
            json!({ "documents_source_hosts": lines }),
            "Allowed sites saved",
        );
    };
    let expert = d.expert;
    let sub = switch_said(&web);
    let no_limit = no_limit_said(&web);
    let pool = pool_said(&web);
    let sites = sites_said(&web);
    let empty = web.hosts.is_empty();
    let rows = move || (typed.get().lines().count() + 1).max(3).to_string();
    view! {
        <div class="prow doc-row" data-web="switch">
            <div class="prow-text">
                <span class="prow-title">"Let callers send web addresses"</span>
                <span class="prow-sub">{sub}</span>
                {no_limit.map(|t| view! { <span class="prow-sub doc-warn">{t}</span> })}
                <span class="prow-sub" class:doc-warn=empty hidden=move || expert.get()>{sites}</span>
                <span class="prow-sub" hidden=move || !expert.get()>{pool}</span>
            </div>
            <div class="prow-end">
                <Switch value=on label="Let callers send web addresses" disabled=busy/>
            </div>
        </div>
        <div class="prow doc-row stack" data-web="sites" hidden=move || !expert.get()>
            <div class="prow-text">
                <span class="prow-title">"Allowed sites"</span>
                <span class="prow-sub">{SITES_HINT}</span>
            </div>
            <textarea
                class="prow-area set-area mono"
                rows=rows
                spellcheck="false"
                aria-label="Allowed sites"
                placeholder="docs.example"
                prop:value=move || typed.get()
                on:input=move |ev| typed.set(event_target_value(&ev))
            ></textarea>
            <div class="prow-end">
                <button type="button" class="btn sm" disabled=move || busy.get() on:click=set_sites>
                    "Save sites"
                </button>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rows_say_what_switching_on_allows_and_where_the_sites_are() {
        let mut w = WebSources::default();
        assert!(switch_said(&w).contains("While it is off they send the file itself"));
        assert!(switch_said(&w).contains("any caller with an API key can; callers without a key"));
        assert!(sites_said(&w).starts_with("No site is allowed yet"));
        assert!(no_limit_said(&w).unwrap().contains("need a set limit"));
        w.fetch = true;
        w.hosts = vec!["docs.example".into(), "203.0.113.7:8080".into()];
        assert!(switch_said(&w).contains("only from the allowed sites"));
        assert!(switch_said(&w).contains("Callers without a key can't"));
        w.pool.slots = 8;
        w.pool.running = 1;
        w.pool.per_fetch_bytes = 64 << 20;
        assert_eq!(no_limit_said(&w), None);
        assert_eq!(
            pool_said(&w),
            "Downloads at once: 8, the same as Parallel tool runs, in a pool of their own. \
             Running now: 1, waiting: 0. At most 8 × 64.0 MiB = 512 MiB of downloads are held \
             at once."
        );
        assert_eq!(
            sites_said(&w),
            "Allowed sites: docs.example, 203.0.113.7:8080. They are edited in the expert view."
        );
    }
}
