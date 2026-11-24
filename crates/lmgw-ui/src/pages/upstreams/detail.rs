//! The detail pane: nothing selected (what the list holds, and Add), an
//! upstream ([`super::editor`]), a new one, or a selection the gateway does
//! not have.

use leptos::prelude::*;

use super::{list_query, Page, Sel};
use crate::widgets::nav_split::{track_scroll, DetailHead};
use crate::widgets::{ActionRow, Ico, Icon, PrefGroup};

#[component]
pub(super) fn DetailPane(
    page: Page,
    sel: Memo<Option<Sel>>,
    narrow: Memo<bool>,
    back: Callback<()>,
) -> impl IntoView {
    // Whether the selected upstream is (still) there; not known yet counts
    // as there, so the page does not flash "gone".
    let known = Memo::new(move |_| match sel.get() {
        Some(Sel::Row(id)) => page
            .list
            .with(|l| l.as_ref().map(|l| l.iter().any(|u| u.id == id))),
        _ => Some(true),
    });
    let present = Memo::new(move |_| known.get() != Some(false));
    // The shown upstream went away (its Delete): back to the list, without
    // asking about edits that have no row left to go to.
    let location = leptos_router::hooks::use_location();
    Effect::new(move |prev: Option<(Option<Sel>, Option<bool>)>| {
        let now = (sel.get(), known.get());
        if let Some((was, Some(true))) = prev {
            if was == now.0 && now.1 == Some(false) {
                let q = list_query(&location.search.get_untracked());
                page.nav.go_unasked(&format!("/upstreams{q}"), true);
            }
        }
        now
    });
    move || {
        let s = sel.get();
        if !present.get() {
            return view! { <Gone narrow=narrow back=back/> }.into_any();
        }
        match s {
            None => view! { <StatusPage page=page narrow=narrow back=back/> }.into_any(),
            Some(Sel::New) => {
                view! { <super::editor::UpDetail page=page id=None narrow=narrow back=back/> }
                    .into_any()
            }
            Some(Sel::Row(id)) => {
                view! { <super::editor::UpDetail page=page id=Some(id) narrow=narrow back=back/> }
                    .into_any()
            }
        }
    }
}

/// Nothing selected: what the list holds, and the way to add one.
#[component]
fn StatusPage(page: Page, narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let scrolled = RwSignal::new(false);
    let rows = move || {
        page.list.with(|l| {
            let Some(l) = l else { return Vec::new() };
            let mut by: Vec<(String, usize)> = Vec::new();
            for u in l {
                let p = super::protocol_label(&u.protocol);
                match by.iter_mut().find(|(k, _)| k == p) {
                    Some((_, n)) => *n += 1,
                    None => by.push((p.to_string(), 1)),
                }
            }
            by
        })
    };
    let state = move || {
        page.list.with(|l| {
            let l = l.as_deref().unwrap_or_default();
            let off = l.iter().filter(|u| !u.enabled).count();
            let quota = l.iter().filter(|u| u.quota.is_some()).count();
            let passthrough = l.iter().filter(|u| u.enabled && u.expose_all).count();
            (l.len(), off, quota, passthrough)
        })
    };
    view! {
        <DetailHead narrow=narrow back=back title="Upstreams" scrolled=scrolled/>
        <div class="detail-scroll mdetail-scroll" tabindex="-1" on:scroll=move |ev| track_scroll(&ev, scrolled)>
            <div class="detail-flow upstatus">
                <div class="detail-hero">
                    <div class="glyph">
                        <Icon ico=Ico::Cloud/>
                    </div>
                    <div class="hero-text">
                        <h1>"Upstreams"</h1>
                        <div class="sub">
                            "Where requests go: cloud providers, and servers lmgw does not start itself. Pick one in the list, or add one."
                        </div>
                    </div>
                </div>
                <PrefGroup title="In the list">
                    {move || {
                        let (n, off, quota, passthrough) = state();
                        let line = |title: &'static str, v: usize| {
                            view! {
                                <div class="prow">
                                    <div class="prow-text">
                                        <span class="prow-title">{title}</span>
                                    </div>
                                    <span class="prow-value">{crate::fmt::grouped(v as u64)}</span>
                                </div>
                            }
                        };
                        view! {
                            {line("Upstreams", n)}
                            {line("Disabled", off)}
                            {line("Out of quota", quota)}
                            {line("Exposing their whole catalog", passthrough)}
                        }
                    }}
                </PrefGroup>
                <Show when=move || page.list.with(|l| l.as_ref().is_some_and(|l| !l.is_empty()))>
                    <PrefGroup title="By protocol">
                        {move || {
                            rows()
                                .into_iter()
                                .map(|(p, n)| {
                                    view! {
                                        <div class="prow">
                                            <div class="prow-text">
                                                <span class="prow-title">{p}</span>
                                            </div>
                                            <span class="prow-value">{n}</span>
                                        </div>
                                    }
                                })
                                .collect_view()
                        }}
                    </PrefGroup>
                </Show>
                <PrefGroup title="Add">
                    <ActionRow
                        title="Add upstream"
                        sub="A provider's API (OpenAI, Anthropic, Gemini, an OpenAI-compatible one) or a llama.cpp server"
                        chevron=true
                        on_click=Callback::new(move |()| {
                            page.nav.go(&Sel::New.path(), false);
                        })
                    />
                </PrefGroup>
            </div>
        </div>
    }
}

/// A selection that names an upstream the gateway does not have.
#[component]
fn Gone(narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let scrolled = RwSignal::new(false);
    view! {
        <DetailHead narrow=narrow back=back title="Not here" scrolled=scrolled/>
        <div class="detail-scroll mdetail-scroll">
            <div class="detail-flow">
                <div class="mdetail-empty">
                    <p>"This upstream is not in the gateway's config: it was deleted, or the link names one that never was."</p>
                    <button type="button" class="btn" on:click=move |_| back.run(())>
                        "Back to the list"
                    </button>
                </div>
            </div>
        </div>
    }
}
