//! The doc-request queue (quickdoc §7, §11).
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
use crate::widgets::{use_toasts, Explain, MenuItem, RowMenu};

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
        <div class="docs-strip">
            <div class="seg">
                {STATUSES
                    .into_iter()
                    .map(|t| {
                        view! {
                            <button
                                class=move || if filter.get() == t { "seg-btn active" } else { "seg-btn" }
                                on:click=move |_| status.set(if t == "pending" { String::new() } else { t.to_string() })
                            >
                                {t}
                                <span class=move || {
                                    if t == "pending" && count(t) != 0 { "count attn" } else { "count" }
                                }>{move || count(t)}</span>
                            </button>
                        }
                    })
                    .collect_view()}
            </div>
            <Explain summary="Filed by agents through docs__request; fulfilled when an ingest for it finishes." persist="docs.explain.requests">
                "A repeat request for the same library@version bumps its counter instead of "
                "piling up. Start ingest opens the wizard prefilled from the request — nothing "
                "is created until you press Create there."
            </Explain>
        </div>

        <div class="fill-pane hug card pad0">
            <table class="data requests-table">
                <thead>
                    <tr>
                        <th>"Library"</th>
                        <th>"Reason"</th>
                        <th class="col-p2">"Requested by"</th>
                        <th class="num-h">"Times"</th>
                        <th class="col-p3">"First seen"</th>
                        <th class="col-p2">"Last seen"</th>
                        <th class="actions"></th>
                    </tr>
                </thead>
                <tbody>
                    {move || match requests.get() {
                        None => {
                            view! {
                                <tr>
                                    <td class="dim" colspan="7">"Loading…"</td>
                                </tr>
                            }
                                .into_any()
                        }
                        Some(Err(e)) => {
                            view! {
                                <tr>
                                    <td class="dim" colspan="7">
                                        "Failed to load: " {e.to_string()} " "
                                        <button class="link-btn" on:click=move |_| reload.update(|n| *n += 1)>
                                            "Retry"
                                        </button>
                                    </td>
                                </tr>
                            }
                                .into_any()
                        }
                        Some(Ok(_)) => {
                            view! {
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
                            }
                                .into_any()
                        }
                    }}
                </tbody>
            </table>
            <Show when=move || requests.get().is_some_and(|r| r.is_ok()) && shown.with(Vec::is_empty)>
                <div class="empty">
                    "Nothing here. Agents file requests with the "
                    <code>"docs__request"</code> " tool when " <code>"docs__resolve"</code>
                    " misses."
                </div>
            </Show>
        </div>
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
    let reason = r.reason.clone().unwrap_or_else(|| "—".into());
    let has_menu = is_pending || is_dismissed;
    let asker = r
        .client_name
        .clone()
        .unwrap_or_else(|| "unnamed client".into());
    // Who asked and when fold away on a narrow table; they stay readable
    // here (par:PAR-6).
    let lead_title = format!(
        "{label}\nasked by {asker} · first {} · last {}",
        r.first_requested_at, r.last_requested_at
    );

    view! {
        <tr>
            <td title=lead_title>
                <span class="model-chip" style=format!("--hue:{hue}")>
                    <i></i>
                    {label.clone()}
                </span>
                {(!is_pending)
                    .then(|| {
                        view! {
                            " "
                            <span class=if is_dismissed { "chip off" } else { "chip ok" }>
                                <span class="dot"></span>
                                {r.status.clone()}
                            </span>
                        }
                    })}
            </td>
            <td class="dim clip" title=reason.clone()>{reason.clone()}</td>
            <td class="dim mono-sm col-p2">{asker}</td>
            <td class="num">{r.count}</td>
            <td class="dim mono-sm col-p3">{r.first_requested_at.clone()}</td>
            <td class="dim mono-sm col-p2">{r.last_requested_at.clone()}</td>
            <td class="actions">
                <span class="row-acts">
                    <button
                        class="btn sm"
                        title="Open the ingest wizard prefilled from this request"
                        on:click=start
                    >
                        "Start ingest"
                    </button>
                    {has_menu.then(|| view! { <RowMenu items=menu/> })}
                </span>
            </td>
        </tr>
    }
}
