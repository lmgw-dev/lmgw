//! A llama.cpp upstream's server, a group of its detail: what `GET /props`
//! said (build, one slot's context, modalities), when, or why it is unknown
//! — for the server, and for a router per model too (llama egress design
//! §4.2).

use leptos::prelude::*;
use lmgw_api_types::UpstreamLlamaFacts;

use crate::fmt::RelTime;
use crate::pages::overview::{llama_props_badge, llama_props_line};
use crate::widgets::PrefGroup;

#[component]
pub(super) fn LlamaGroup(#[prop(into)] facts: Signal<Vec<UpstreamLlamaFacts>>) -> impl IntoView {
    view! {
        <PrefGroup
            title="llama-server"
            desc="What the server says about itself in GET /props (a router: per model); read again after an edit, a transport failure, a media refusal or Test."
        >
            {move || {
                let facts = facts.get();
                if facts.is_empty() {
                    view! {
                        <div class="prow notice-row">
                            "Not read yet: lmgw asks its GET /props in the background on the first chat request, or on Test."
                        </div>
                    }
                    .into_any()
                } else {
                    facts.into_iter().map(model_facts).collect_view().into_any()
                }
            }}
        </PrefGroup>
    }
}

/// The server, or one model of a router: its facts, or why there are none.
fn model_facts(f: UpstreamLlamaFacts) -> impl IntoView {
    let title = if f.model.is_empty() {
        if f.router { "Router" } else { "Server" }.to_string()
    } else {
        f.model.clone()
    };
    let build = f.props.as_ref().map(|p| {
        p.build_info
            .clone()
            .unwrap_or_else(|| "build unknown".into())
    });
    let said = match (&f.props, &f.unknown) {
        (Some(_), _) if f.router => view! {
            <span class="type-badge" title="A llama-server router: its facts are per model">"router"</span>
        }
        .into_any(),
        (Some(p), _) => view! {
            <span class="type-badge" title=llama_props_line(p)>{llama_props_badge(p)}</span>
        }
        .into_any(),
        (None, Some(_)) => {
            let word = if f.cached { "unknown" } else { "not read" };
            view! { <span class="chip warn">{word}</span> }.into_any()
        }
        (None, None) => view! { <span class="dim">"asking…"</span> }.into_any(),
    };
    let why = f.props.is_none().then(|| f.unknown.clone()).flatten();
    let read = f.read_at;
    let asking = (f.probing && (f.props.is_some() || f.unknown.is_some())).then(|| {
        view! { <span class="chip info" title="Being asked again"><span class="dot"></span>"asking"</span> }
    });
    let mono = !f.model.is_empty();
    view! {
        <div class="prow up-llama" title=format!("asked at {}", f.base_url)>
            <div class="prow-text">
                <span class="prow-title" class:mono=mono>{title}</span>
                <span class="prow-sub">
                    {build.map(|b| view! { <span class="mono-sm">{b}</span> " · " })}
                    {match read {
                        Some(t) => view! { "read " <RelTime ts=t as f64/> }.into_any(),
                        None => view! { "not answered yet" }.into_any(),
                    }}
                </span>
                {why.map(|w| view! { <span class="prow-err">{w}</span> })}
            </div>
            <div class="prow-end">{asking} {said}</div>
        </div>
    }
}
