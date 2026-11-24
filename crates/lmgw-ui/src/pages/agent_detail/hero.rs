//! The Run tab's hero: the run kind's icon, the agent's name, its id and
//! version, the description folded to two lines, and chips for what it is
//! now — its state (as the list says it, a live run included), its kind,
//! the model it will ask for, the tools it attaches, its app's state.

use leptos::prelude::*;
use lmgw_api_types::AgentDetail;

use super::*;
use crate::pages::agents::kind_icon;
use crate::widgets::{ClampText, Dot, Icon, StateChip};

/// The agent's state as the list says it, from the detail: `live` is
/// whether a run of it is on screen and in flight.
pub fn detail_state(d: &AgentDetail, live: bool) -> (Dot, String) {
    if d.error.is_some() {
        return (Dot::Error, "Not readable".into());
    }
    if !d.enabled {
        return (Dot::Off, "Disabled".into());
    }
    if live || d.live_job.is_some() {
        return (Dot::Busy, "Running".into());
    }
    if !d.requires_ok {
        return (Dot::Error, "Tools missing".into());
    }
    if d.warnings.iter().any(|w| w.blocks_start) {
        return (Dot::Error, "Blocked".into());
    }
    match d.warnings.len() {
        0 => (Dot::Stopped, "Ready".into()),
        1 => (Dot::Stopped, "1 warning".into()),
        n => (Dot::Stopped, format!("{n} warnings")),
    }
}

/// The kind as the hero's chip says it.
pub fn kind_word(kind: &str) -> &'static str {
    match kind {
        "chat" => "Chat agent",
        "batch" => "Batch agent",
        "container" => "Container agent",
        _ => "Unknown kind",
    }
}

#[component]
pub(super) fn Hero(d: Memo<AgentDetail>, run: RunState) -> impl IntoView {
    let live = Memo::new(move |_| run.summary.with(|s| s.as_ref().is_some_and(is_live)));
    let state = move || {
        let (dot, word) = d.with(|d| detail_state(d, live.get()));
        view! { <StateChip dot=dot word=word/> }
    };
    // What the hero shows of the agent, apart from the state chip above:
    // rebuilt when that changes, not on every re-read (a run's end).
    let facts = Memo::new(move |_| {
        d.with(|d| {
            let name = if d.name.is_empty() {
                d.id.clone()
            } else {
                d.name.clone()
            };
            let mut sub = d.id.clone();
            if !d.version.is_empty() {
                sub.push_str(&format!(" · v{}", d.version));
            }
            let app = d.service.as_ref().map(|s| {
                if s.running {
                    ("chip ok", "App running")
                } else if s.starting {
                    ("chip info", "App starting")
                } else {
                    ("chip", "App stopped")
                }
            });
            (
                name,
                sub,
                d.description.trim().to_string(),
                d.kind.clone(),
                d.effective_model.clone(),
                d.requires
                    .iter()
                    .map(|r| r.label.clone())
                    .collect::<Vec<_>>(),
                app,
                d.source == "builtin",
                // The kind's chip says, on hover, what a run is held to: the
                // image and its bounds, or where the package came from.
                runtime_summary(d.runtime.as_ref(), d.provenance.as_ref()),
            )
        })
    });
    move || {
        let (name, sub, desc, kind, model, labels, app, builtin, kind_tip) = facts.get();
        let model_chip = if model.is_empty() {
            "No model".to_string()
        } else {
            format!("Model {model}")
        };
        view! {
            <div class="detail-hero agent-hero">
                <div class="glyph">
                    <Icon ico=kind_icon(&kind)/>
                </div>
                <div class="hero-text">
                    <h1>{name}</h1>
                    <div class="sub mono-sub" title=sub.clone()>{sub.clone()}</div>
                    // Read once and then only looked up: folded to two lines,
                    // with its "more".
                    {(!desc.is_empty()).then(|| view! { <ClampText text=desc lines=2 class="hero-desc"/> })}
                    <div class="chips">
                        {state}
                        <span class="chip" title=(!kind_tip.is_empty()).then_some(kind_tip)>
                            {kind_word(&kind)}
                        </span>
                        <span class="chip" title="the alias this agent asks for, config already applied">
                            {model_chip}
                        </span>
                        {(!labels.is_empty())
                            .then(|| {
                                let all = labels.join(", ");
                                view! {
                                    <span class="chip" title=format!("the tool labels it attaches: {all}")>
                                        {format!("Tools {all}")}
                                    </span>
                                }
                            })}
                        {app.map(|(cls, word)| view! { <span class=cls>{word}</span> })}
                        {builtin.then(|| view! { <span class="chip">"Shipped"</span> })}
                    </div>
                </div>
            </div>
        }
    }
}
