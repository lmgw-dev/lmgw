//! The detail's hero: the upstream's glyph, its name and URL, and chips for
//! what it is now — its state (ready, asking, out of quota, unreachable,
//! disabled), its protocol and kind, what its catalog holds (a link to it
//! in Models → Upstream catalogs when the whole catalog is exposed),
//! whether a key is stored, and the note of an `openai` row on a host that
//! is not OpenAI's.

use leptos::prelude::*;
use lmgw_api_types::UpstreamView;

use super::list::state_of;
use super::{protocol_label, Catalog, Page, LLAMA_CPP};
use crate::fmt::grouped;
use crate::widgets::{Ico, Icon, StateChip};

#[component]
pub(super) fn Hero(
    page: Page,
    #[prop(into)] name: Signal<String>,
    #[prop(into)] sub: Signal<String>,
    #[prop(into)] row: Signal<Option<UpstreamView>>,
) -> impl IntoView {
    let icon = move || {
        if row.with(|r| r.as_ref().is_some_and(|u| u.protocol == LLAMA_CPP)) {
            Ico::Chip
        } else {
            Ico::Cloud
        }
    };
    view! {
        <div class="detail-hero">
            <div class="glyph">{move || view! { <Icon ico=icon()/> }}</div>
            <div class="hero-text">
                <h1>{move || name.get()}</h1>
                <div
                    class=move || if sub.with(|s| s.contains("://")) { "sub mono-sub" } else { "sub" }
                    title=move || sub.get()
                >{move || sub.get()}</div>
                <div class="chips">{move || row.get().map(|u| chips(page, u))}</div>
            </div>
        </div>
    }
}

fn chips(page: Page, u: UpstreamView) -> impl IntoView {
    let id = u.id;
    let cat = Memo::new(move |_| page.catalogs.with(|m| m.get(&id).cloned()));
    let state = {
        let u = u.clone();
        move || {
            let c = cat.get();
            let (dot, word) = state_of(&u, c.as_ref());
            let tip = match &c {
                Some(Catalog::Failed(e)) if u.enabled && u.quota.is_none() => e.clone(),
                _ if !u.enabled => "A disabled upstream is not asked".into(),
                _ => String::new(),
            };
            view! { <StateChip dot=dot word=word title=tip/> }
        }
    };
    let name = u.name.clone();
    let expose_all = u.expose_all;
    let enabled = u.enabled;
    let catalog = move || {
        if !enabled {
            return None;
        }
        let Some(Catalog::Ready(n)) = cat.get() else {
            return None;
        };
        if !expose_all {
            return Some(
                view! {
                    <span class="chip" title="Not exposed as a whole catalog: its models are reachable through aliases">
                        {format!("{} models · aliases only", grouped(n as u64))}
                    </span>
                }
                .into_any(),
            );
        }
        let hidden = page.models.full.with(|f| {
            f.as_ref()
                .and_then(|f| f.passthrough.iter().find(|p| p.id == id))
                .map_or(0, |p| p.hidden.len())
        });
        let href = format!(
            "/models/catalog?upstream={}",
            String::from(js_sys::encode_uri_component(&name))
        );
        Some(
            view! {
                <a class="chip chip-link" href=href title="Open it in Models → Upstream catalogs">
                    {format!("{} models · {} hidden", grouped(n as u64), grouped(hidden as u64))}
                </a>
            }
            .into_any(),
        )
    };
    let kind = match u.kind.as_str() {
        "llama_server" if u.protocol != LLAMA_CPP => Some("llama-server"),
        "audio_cpp" => Some("audio.cpp"),
        _ => None,
    };
    let protocol = match kind {
        Some(k) => format!("{} · {k}", protocol_label(&u.protocol)),
        None => protocol_label(&u.protocol).to_string(),
    };
    let key = if u.has_api_key {
        "API key stored"
    } else {
        "No API key"
    };
    view! {
        {state}
        <span class="chip">{protocol}</span>
        {catalog}
        <span class="chip">{key}</span>
        {u.host_note.clone().map(|note| view! { <span class="chip warn" title=note>"Not OpenAI's host"</span> })}
    }
}
