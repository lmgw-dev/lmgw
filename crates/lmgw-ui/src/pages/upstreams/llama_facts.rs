//! A llama.cpp row's server under its row: what `GET /props` said (build, one
//! slot's context, modalities), when, or why it is unknown — for the server,
//! and for a router per model too (llama egress design §4.2).

use leptos::prelude::*;
use lmgw_api_types::UpstreamLlamaFacts;

use crate::fmt::RelTime;
use crate::pages::overview::{llama_props_badge, llama_props_line};

/// The detail row under a `llama_cpp` upstream (`span` columns wide).
#[component]
pub(super) fn LlamaFactsRow(
    facts: Vec<UpstreamLlamaFacts>,
    enabled: bool,
    span: u32,
) -> impl IntoView {
    let body = if facts.is_empty() {
        view! {
            <span class="dim">
                "not read yet — lmgw asks its GET /props in the background on the first chat request, or on Test"
            </span>
        }
        .into_any()
    } else {
        facts.into_iter().map(model_facts).collect_view().into_any()
    };
    view! {
        <tr class="detail-row up-llama" class:muted=!enabled>
            <td colspan=span>
                <div class="up-llama-facts">
                    <span
                        class="up-llama-label"
                        title="What the server says about itself in GET /props (a router: per model); read again after an edit, a transport failure, a media refusal or Test"
                    >
                        "llama-server"
                    </span>
                    {body}
                </div>
            </td>
        </tr>
    }
}

/// The server, or one model of a router: its facts, or why there are none.
fn model_facts(f: UpstreamLlamaFacts) -> impl IntoView {
    let said = match (&f.props, &f.unknown) {
        (Some(p), _) if f.router => view! {
            <span class="type-badge" title="A llama-server router: its facts are per model">"router"</span>
            <span class="dim mono-sm">
                {p.build_info.clone().unwrap_or_else(|| "build unknown".into())}
            </span>
        }
        .into_any(),
        (Some(p), _) => view! {
            <span class="type-badge" title=llama_props_line(p)>{llama_props_badge(p)}</span>
            <span class="dim mono-sm">
                {p.build_info.clone().unwrap_or_else(|| "build unknown".into())}
            </span>
        }
        .into_any(),
        (None, Some(why)) => {
            let word = if f.cached { "unknown" } else { "not read" };
            view! { <span class="problem" title=why.clone()>{format!("{word} — {why}")}</span> }
                .into_any()
        }
        (None, None) => view! { <span class="dim">"asking…"</span> }.into_any(),
    };
    let read = f.read_at.map(|t| {
        view! {
            <span class="dim">"read " <RelTime ts=t as f64/></span>
        }
    });
    let asking = (f.probing && (f.props.is_some() || f.unknown.is_some())).then(|| {
        view! { <span class="chip live" title="Being asked again"><span class="dot"></span>"asking"</span> }
    });
    view! {
        <span class="up-llama-model" title=format!("asked at {}", f.base_url)>
            {(!f.model.is_empty()).then(|| view! { <span class="mono-sm">{f.model}</span> })}
            {said}
            {read}
            {asking}
        </span>
    }
}
