//! The folded reference panel (labs-store design §2.1, §3.2): the saved
//! question sets and the set runs, the head model's limits with their rules,
//! its facts and its notes, and the question types' rules as the wire
//! documents them.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::DecisionMeasure;

use super::batch_list::RunsPanel;
use super::page::Lab;
use super::sets_panel::SetsPanel;
use crate::fmt::grouped;

#[component]
pub(super) fn Reference(lab: Lab) -> impl IntoView {
    view! {
        <div class="lab-help dl-ref">
            <SetsPanel lab=lab/>
            <RunsPanel lab=lab/>
            {move || {
                let Some(m) = lab.model() else {
                    return view! { <div class="empty">"No model selected."</div> }.into_any();
                };
                let limits = m
                    .limits
                    .iter()
                    .map(|l| {
                        let what = match l.applies_to {
                            DecisionMeasure::StateAndQuestion => "the state and one question",
                            DecisionMeasure::OptionSpan => "the option span",
                        };
                        let n = l
                            .tokens
                            .map(|t| format!("{} tokens", grouped(t)))
                            .unwrap_or_else(|| "not known".into());
                        view! {
                            <div class="dl-limit">
                                <div class="caps-row">
                                    <span class="k">{what}</span>
                                    <span class="v mono-sm">{n}</span>
                                </div>
                                <p class="mini-note dim">{format!("{} — {}", l.of, l.rule)}</p>
                            </div>
                        }
                    })
                    .collect_view();
                let facts = m
                    .facts
                    .clone()
                    .map(|f| {
                        view! {
                            <div class="caps">
                                <div class="caps-row">
                                    <span class="k">"batch"</span>
                                    <span class="v mono-sm">
                                        {format!("{} ({})", f.batch, f.batch_source.label())}
                                    </span>
                                </div>
                                <div class="caps-row">
                                    <span class="k">"ctx_size"</span>
                                    <span class="v mono-sm">
                                        {f.ctx_size.map(|c| c.to_string()).unwrap_or_else(|| "default".into())}
                                    </span>
                                </div>
                                <div class="caps-row">
                                    <span class="k">"n_ctx_slot"</span>
                                    <span class="v mono-sm">
                                        {f
                                            .n_ctx_slot
                                            .map(|c| c.to_string())
                                            .unwrap_or_else(|| "not running".into())}
                                    </span>
                                </div>
                                <div class="caps-row">
                                    <span class="k">"threads"</span>
                                    <span class="v mono-sm">
                                        {f
                                            .threads_in_effect
                                            .map(|t| format!("{t} ({})", f.threads_source.label()))
                                            .unwrap_or_else(|| f.threads_source.label().to_string())}
                                    </span>
                                </div>
                            </div>
                        }
                    });
                let warnings = (!m.warnings.is_empty())
                    .then(|| {
                        view! {
                            <div class="notice warn">
                                <b>"What its container's start reported"</b>
                                {m.warnings.join(" · ")}
                            </div>
                        }
                    });
                let notes = (!m.notes.is_empty())
                    .then(|| {
                        let n = m.notes.len();
                        view! {
                            <details class="lab-details">
                                <summary>{format!("What it publishes — {n}")}</summary>
                                <ul class="side-notes">
                                    {m.notes.iter().map(|n| view! { <li>{n.clone()}</li> }).collect_view()}
                                </ul>
                            </details>
                        }
                    });
                view! {
                    <h3 class="lab-h3">"Limits"</h3>
                    {if m.limits.is_empty() {
                        view! {
                            <p class="mini-note dim">
                                "No limit is known for this model: its header did not read, or the catalog states no context."
                            </p>
                        }
                            .into_any()
                    } else {
                        limits.into_any()
                    }}
                    {facts}
                    {warnings}
                    {notes}
                }
                    .into_any()
            }}
            <h3 class="lab-h3">"Question types"</h3>
            <ul class="side-notes">
                <li>
                    <b>"choice"</b>
                    " picks one option. Names are unique; a description says what an option means and moves accuracy a lot. The wire allows 255 options; some types fewer, and llama.cpp's 400 names the limit."
                </li>
                <li>
                    <b>"score"</b>
                    " rates on 2 to 10 levels, lowest first. The answer is the expected level, which can fall between two."
                </li>
                <li>
                    <b>"noul"</b>
                    " answers yes or no as P(yes); optional criteria say what each means. The lab's certainty is max(p, 1−p)."
                </li>
                <li>
                    "The state is text or JSON (" <span class="mono-sm">"null"</span>
                    " for the types that take none); answers come back under each question's id, in the order asked."
                </li>
            </ul>
        </div>
    }
}
