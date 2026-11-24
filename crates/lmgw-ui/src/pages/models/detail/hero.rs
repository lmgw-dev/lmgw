//! The detail's hero: the class's icon, the name, the file or upstream, and
//! chips for what the row is now — its container's state (running, with
//! what it holds on the GPU and since when; starting; waiting for its
//! download), where it runs (the CPU), who started it (background work),
//! the warnings its start found, a ladder's rung, what it can do (vision,
//! tools, reasoning), its licence, its fallback and its recent deferrals.
//! Live from the event stream, as the list's state is.

use leptos::prelude::*;

use super::super::list::state_of;
use super::super::{licence, runtime_of, Class, Row};
use crate::fmt::{age, fmt_bytes};
use crate::widgets::{Dot, Icon, StateChip};

#[component]
pub(in crate::pages::models) fn Hero(
    class: Class,
    #[prop(into)] name: Signal<String>,
    #[prop(into)] sub: Signal<String>,
    #[prop(into)] row: Signal<Option<Row>>,
    /// The subtitle is a sentence, not a path or an id: the text face.
    #[prop(into, default = Signal::derive(|| false))]
    sub_plain: Signal<bool>,
) -> impl IntoView {
    view! {
        <div class="detail-hero">
            <div class="glyph">
                <Icon ico=class.icon()/>
            </div>
            <div class="hero-text">
                <h1>{move || name.get()}</h1>
                <div class="sub" class:mono-sub=move || !sub_plain.get() title=move || sub.get()>{move || sub.get()}</div>
                <div class="chips">
                    {move || row.get().map(|r| view! { <Chips r=r/> })}
                </div>
            </div>
        </div>
    }
}

#[component]
fn Chips(r: Row) -> impl IntoView {
    let live = crate::live::use_live();
    let catalog = crate::catalog::use_model_catalog();
    let r = StoredValue::new(r);
    let (class, mid) = r.with_value(|r| (r.class, r.model_id.clone()));
    let mid = StoredValue::new(mid);
    let rt = Memo::new(move |_| {
        live.runtime.with(|rows| {
            rows.as_deref()
                .and_then(|rows| mid.with_value(|m| runtime_of(rows, class, m).cloned()))
        })
    });
    let dl = crate::gpu_download::live_for(
        class.runtime().unwrap_or_default().to_string(),
        mid.get_value(),
    );
    let vram = Memo::new(move |_| {
        let container = class.runtime()?;
        live.vram.with(|v| {
            v.as_ref().and_then(|v| {
                mid.with_value(|m| {
                    v.resident
                        .iter()
                        .find(|x| x.container == container && &x.model == m)
                        .map(|x| x.estimated_bytes)
                })
            })
        })
    });

    let state = move || {
        if dl.with(Option::is_some) && rt.with(Option::is_none) {
            return view! {
                <crate::gpu_download::DlChip dl=Signal::derive(move || dl.get().unwrap_or_default())/>
            }
            .into_any();
        }
        let (dot, word) = r.with_value(|row| rt.with(|x| state_of(row, x.as_ref(), false)));
        let mut text = word;
        if dot == Dot::Running {
            if let Some(b) = vram.get() {
                text.push_str(&format!(" · {} VRAM", fmt_bytes(b)));
            }
            if let Some(x) = rt.get() {
                text.push_str(&format!(" · up {}", age(x.started_at_age_seconds)));
            }
        }
        view! { <StateChip dot=dot word=text/> }.into_any()
    };

    // What the runtime frame says beyond up or down: only ever the
    // exception, nothing for the normal case.
    let extras = move || {
        let Some(x) = rt.get() else {
            return Vec::new();
        };
        let mut v = Vec::new();
        if x.placement.as_deref() == Some("cpu") {
            v.push(view! {
                <span class="chip" title="Its container runs on the CPU: nothing on the GPU, and the GPU hold does not stop it">
                    "CPU"
                </span>
            }
            .into_any());
        }
        if x.owner.as_deref() == Some("background") {
            v.push(view! {
                <span class="chip" title="Started by background traffic (a background candidate alias, or a request sent with x-lmgw-background: on); no ordinary request has claimed it since">
                    "background"
                </span>
            }
            .into_any());
        }
        if x.draining_for_owner {
            v.push(view! {
                <span class="chip warn" title="An ordinary request is waiting for GPU room, so no model takes new background work until it is served">
                    "owner waiting for room"
                </span>
            }
            .into_any());
        }
        if !x.warnings.is_empty() {
            let n = x.warnings.len();
            v.push(
                view! {
                    <span class="chip warn" title=x.warnings.join("\n")>
                        {format!("{n} warning{}", if n == 1 { "" } else { "s" })}
                    </span>
                }
                .into_any(),
            );
        }
        // Ladder design §6: the rung it runs, the GGUF a hover away.
        if let Some(rung) = &x.rung {
            v.push(
                view! {
                    <span class="chip" title=rung.gguf.clone()>
                        {format!("rung {}/{}", rung.rung, rung.of)}
                    </span>
                }
                .into_any(),
            );
        }
        if let Some(c) = &x.climbing {
            v.push(
                view! {
                    <span class="chip info" title=c.reason.clone()>
                        {format!("climbing to {}/{}", c.to, c.of)}
                    </span>
                }
                .into_any(),
            );
        }
        v
    };

    // What the gateway publishes it as can do.
    let facts = move || {
        let name = r.with_value(|r| r.name.clone());
        catalog.entries.with(|es| {
            es.iter()
                .find(|e| e.id == name)
                .map(|e| {
                    let mut v = Vec::new();
                    if e.vision == Some(true) {
                        v.push("Vision");
                    }
                    if e.tools {
                        v.push("Tools");
                    }
                    if e.reasoning {
                        v.push("Reasoning");
                    }
                    v
                })
                .unwrap_or_default()
        })
    };

    let (note, licence, fallback, deferrals, name) = r.with_value(|r| {
        (
            r.note.clone(),
            r.licence.clone(),
            r.fallback.clone(),
            r.deferrals_24h,
            r.name.clone(),
        )
    });
    let fallback_chip = match fallback {
        Some(f) => view! {
            <span class="chip" title=format!("Fallback {f}: answers when {name} can't")>
                {format!("Fallback → {f}")}
            </span>
        }
        .into_any(),
        None => view! {
            <span class="chip" title="No fallback: when it cannot answer, the request gets the error it gives">
                "No fallback"
            </span>
        }
        .into_any(),
    };
    view! {
        {state}
        {extras}
        {move || facts().into_iter().map(|f| view! { <span class="chip">{f}</span> }).collect_view()}
        {(!note.is_empty()).then(|| view! { <span class="chip">{note}</span> })}
        {licence.map(|chip| view! { <licence::LicenceChip chip=*chip/> })}
        {fallback_chip}
        <crate::pages::candidate_alias_editor::DeferredBadge deferrals_24h=deferrals/>
    }
}
