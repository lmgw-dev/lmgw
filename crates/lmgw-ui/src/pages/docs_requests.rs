//! The doc-request queue (quickdoc §7, §11): a boxed list of rows, one per
//! request (its library@version, reason, who asked, how often and when),
//! each with Start ingest and a menu, under the Pending, Fulfilled,
//! Dismissed and All toggle with their counts.
//!
//! `docs__request` is how an agent that missed a corpus says so; this is where
//! the owner answers. Two things are deliberate here. **Start ingest** prefills
//! the wizard from the request instead of creating anything — ingestion always
//! starts with the owner, never with the agent. And **fulfilled** is not an
//! action: a request becomes fulfilled when an ingest for its `library@version`
//! completes, which is the only thing that actually knows it happened.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{Ack, DocRequestRow, DocRequestsResponse};
use serde_json::json;

use crate::fmt::hue_for;
use crate::widgets::{use_toasts, MenuItem, PrefGroup, RowMenu};

use super::docs::{refresh_pending, use_docs, use_docs_pending, WizardSeed};

/// The queue's views, in the order the seg shows them; `pending` is the
/// default (an empty `?status=`).
const STATUSES: [&str; 4] = ["pending", "fulfilled", "dismissed", "all"];

#[component]
pub fn RequestQueue() -> impl IntoView {
    let status = crate::url_state::use_query_signal("status");
    let filter = Memo::new(move |_| {
        let s = status.get();
        STATUSES
            .iter()
            .copied()
            .find(|t| *t == s)
            .unwrap_or("pending")
    });
    let reload = RwSignal::new(0u32);
    // Every request, filtered here: the seg says how many each view holds.
    let requests = LocalResource::new(move || {
        reload.get();
        crate::api::get::<DocRequestsResponse>("/api/docs/requests")
    });
    let rows = Memo::new(move |_| {
        requests
            .get()
            .and_then(|r| r.ok())
            .map(|r| r.requests)
            .unwrap_or_default()
    });
    let count =
        move |t: &str| rows.with(|rs| rs.iter().filter(|r| t == "all" || r.status == t).count());
    let shown = Memo::new(move |_| {
        let f = filter.get();
        rows.with(|rs| {
            rs.iter()
                .filter(|r| f == "all" || r.status == f)
                .cloned()
                .collect::<Vec<_>>()
        })
    });

    view! {
        <div class="fill-pane docs-scroll">
            <div class="docs-flow">
                <div class="docs-strip">
                    <div class="seg" role="group" aria-label="Which requests">
                        {STATUSES
                            .into_iter()
                            .map(|t| {
                                view! {
                                    <button
                                        class=move || if filter.get() == t { "seg-btn active" } else { "seg-btn" }
                                        aria-pressed=move || (filter.get() == t).to_string()
                                        on:click=move |_| status.set(if t == "pending" { String::new() } else { t.to_string() })
                                    >
                                        {status_label(t)}
                                        <span class=move || {
                                            if t == "pending" && count(t) != 0 { "count attn" } else { "count" }
                                        }>{move || count(t)}</span>
                                    </button>
                                }
                            })
                            .collect_view()}
                    </div>
                </div>
                <PrefGroup
                    title="Requests"
                    desc="Filed by agents through docs__request when docs__resolve misses; fulfilled when an ingest for it finishes. A repeat request for the same library@version bumps its counter instead of piling up. Start ingest opens the wizard prefilled from the request — nothing is created until you press Create there."
                >
                    {move || match requests.get() {
                        None => view! { <div class="prow kb-none">"Loading…"</div> }.into_any(),
                        Some(Err(e)) => {
                            view! {
                                <div class="prow problem-row">
                                    "Failed to load: " {e.to_string()} " "
                                    <button class="link-btn" on:click=move |_| reload.update(|n| *n += 1)>
                                        "Retry"
                                    </button>
                                </div>
                            }
                                .into_any()
                        }
                        Some(Ok(_)) => {
                            view! {
                                <div class="docs-rows">
                                    <For
                                        each=move || shown.get()
                                        key=|r| (r.id, r.status.clone(), r.count)
                                        let:row
                                    >
                                        <RequestRow
                                            r=row
                                            on_change=move || reload.update(|n| *n += 1)
                                        />
                                    </For>
                                </div>
                                <Show when=move || shown.with(Vec::is_empty)>
                                    <div class="prow kb-none">
                                        "Nothing here. Agents file requests with the "
                                        <code>"docs__request"</code> " tool when " <code>"docs__resolve"</code>
                                        " misses."
                                    </div>
                                </Show>
                            }
                                .into_any()
                        }
                    }}
                </PrefGroup>
            </div>
        </div>
    }
}

/// A view's name on its button.
fn status_label(t: &str) -> &'static str {
    match t {
        "fulfilled" => "Fulfilled",
        "dismissed" => "Dismissed",
        "all" => "All",
        _ => "Pending",
    }
}

#[component]
fn RequestRow(
    r: DocRequestRow,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let state = use_docs();
    let pending = use_docs_pending();
    let toasts = use_toasts();
    let id = r.id;
    let label = if r.version.is_empty() {
        r.library.clone()
    } else {
        format!("{}@{}", r.library, r.version)
    };
    let hue = hue_for(&label);
    let is_pending = r.status == "pending";
    let is_dismissed = r.status == "dismissed";

    let set_status = move |status: &'static str| {
        spawn_local(async move {
            match crate::api::post::<Ack, _>(
                format!("/api/docs/requests/{id}/status"),
                &json!({ "status": status }),
            )
            .await
            {
                Ok(_) => {
                    toasts.ok(format!("request marked {status}"));
                    refresh_pending(pending);
                    on_change();
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let seed = WizardSeed {
        library: r.library.clone(),
        version: r.version.clone(),
        reason: r.reason.clone(),
        requested_by: r.client_name.clone(),
    };
    let start = move |_| state.wizard.set(Some(seed.clone()));
    // Dismiss and Restore undo each other, so neither asks.
    let menu = Signal::derive(move || {
        let mut v = Vec::new();
        if is_pending {
            v.push(
                MenuItem::new("Dismiss", move || set_status("dismissed"))
                    .title("take it off the queue; Restore brings it back"),
            );
        }
        if is_dismissed {
            v.push(MenuItem::new("Restore", move || set_status("pending")));
        }
        v
    });
    let reason = r.reason.clone().unwrap_or_else(|| "No reason given".into());
    let has_menu = is_pending || is_dismissed;
    let asker = r
        .client_name
        .clone()
        .unwrap_or_else(|| "unnamed client".into());
    // Everything about the request, in one tooltip.
    let lead_title = format!(
        "{label}\nasked by {asker} · first {} · last {}",
        r.first_requested_at, r.last_requested_at
    );

    let meta = format!(
        "asked by {asker} · {} time{} · first {} · last {}",
        r.count,
        if r.count == 1 { "" } else { "s" },
        r.first_requested_at,
        r.last_requested_at
    );
    view! {
        <div class="prow docs-request" title=lead_title>
            <span class="rail-dot" style=format!("--hue:{hue}")></span>
            <div class="prow-text">
                <span class="prow-title">
                    <span class="mono-sm docs-request-lib">{label.clone()}</span>
                    {(!is_pending)
                        .then(|| {
                            view! {
                                " "
                                <span class=if is_dismissed { "chip off" } else { "chip ok" }>
                                    {r.status.clone()}
                                </span>
                            }
                        })}
                </span>
                <span class="prow-sub">{reason.clone()}</span>
                <span class="prow-sub docs-request-meta">{meta}</span>
            </div>
            <div class="prow-end">
                <button
                    class="btn"
                    title="Open the ingest wizard prefilled from this request"
                    on:click=start
                >
                    "Start ingest"
                </button>
                {has_menu.then(|| view! { <RowMenu items=menu/> })}
            </div>
        </div>
    }
}
