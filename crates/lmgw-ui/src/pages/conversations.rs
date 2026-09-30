//! Conversations — stored /v1/responses chains: observe and evict. The unit
//! of eviction is always the whole chain (deleting single responses would
//! strand a conversation a client is still extending).

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{ChainDetail, ChainRow, ResponsesIndex};
use serde_json::{json, Value};

use leptos_router::hooks::use_navigate;

use crate::fmt::hue_for;
use crate::widgets::{use_toasts, Modal, PageFrame, PageMode};

fn urlenc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

/// Conversations (`/traffic/conversations`). The chain being read is in the
/// URL (`?chain=`), so it survives a reload and Back returns to the list.
#[component]
pub fn TrafficConversations() -> impl IntoView {
    let toasts = use_toasts();
    let navigate = use_navigate();
    let reload = RwSignal::new(0u32);
    let bump = move || reload.update(|n| *n += 1);
    let index = LocalResource::new(move || {
        reload.get();
        crate::api::get::<ResponsesIndex>("/api/responses")
    });
    let chain = crate::url_state::use_query_signal("chain");
    let clear_confirm = RwSignal::new(false);
    let count = Signal::derive(move || index.get().and_then(|r| r.ok()).map(|i| i.total_chains));

    let open_chain = {
        let navigate = navigate.clone();
        Callback::new(move |id: String| {
            navigate(
                &format!("/traffic/conversations?chain={}", urlenc(&id)),
                Default::default(),
            )
        })
    };
    let back = Callback::new(move |()| navigate("/traffic/conversations", Default::default()));

    let gc = move |scope: &'static str| {
        spawn_local(async move {
            match crate::api::post::<Value, _>("/api/op/responses_gc", &json!({ "scope": scope }))
                .await
            {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("done")
                        .to_string());
                    bump();
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let listing = move || chain.with(String::is_empty);

    view! {
        <PageFrame
            title="Traffic"
            mode=PageMode::Fill
            head_extra=move || view! { <super::traffic::TrafficTabs conversations=count/> }
            actions=move || {
                view! {
                    <Show when=listing>
                        <button class="btn ghost" on:click=move |_| gc("rules")>
                            "Evict by rules now"
                        </button>
                        <button class="btn danger" on:click=move |_| clear_confirm.set(true)>
                            "Clear all"
                        </button>
                    </Show>
                }
            }
        >
            {move || {
                let id = chain.get();
                if !id.is_empty() {
                    return view! { <ChainView chain_id=id on_back=back/> }.into_any();
                }
                match index.get() {
                    None => view! { <div class="card dim">"Loading…"</div> }.into_any(),
                    Some(Err(e)) => {
                        view! {
                            <div class="chart-err">
                                "Could not load — " {e.to_string()} " "
                                <button class="link-btn" on:click=move |_| bump()>
                                    "Retry"
                                </button>
                            </div>
                        }
                            .into_any()
                    }
                    Some(Ok(idx)) => {
                        let chains = idx.chains;
                        let empty = chains.is_empty();
                        // The server lists the most recently active
                        // conversations only (200); the total above counts all
                        // of them, so the gap is said.
                        let listed = chains.len() as i64;
                        let held_back = (listed < idx.total_chains)
                            .then(|| format!(" · listing the {listed} most recently active"));
                        view! {
                            <div class="dim conv-summary">
                                {format!(
                                    "{}, {} — {}",
                                    crate::fmt::count_of(idx.total_chains.max(0) as usize, "conversations"),
                                    crate::fmt::count_of(idx.total_responses.max(0) as usize, "responses"),
                                    idx.rules,
                                )}
                                {held_back}
                                {(!idx.store_enabled)
                                    .then_some(" · storing is OFF (new responses are not kept)")}
                            </div>
                            <div class="fill-pane hug card pad0">
                                <table class="data many-cols conv-table">
                                    <thead>
                                        <tr>
                                            <th>"Conversation"</th>
                                            <th>"Model"</th>
                                            <th>"Status"</th>
                                            <th class="num-h">"Turns"</th>
                                            <th class="num-h col-p2">"Tokens"</th>
                                            <th class="num-h col-p2">"Size"</th>
                                            <th>"Last activity"</th>
                                            <th class="actions"></th>
                                        </tr>
                                    </thead>
                                    <tbody>
                                        <For
                                            each=move || chains.clone()
                                            key=|c| c.chain_id.clone()
                                            let:c
                                        >
                                            <ChainRowView c=c open=open_chain on_change=bump/>
                                        </For>
                                    </tbody>
                                </table>
                                <Show when=move || empty>
                                    <div class="empty">
                                        "No stored conversations — /v1/responses chains appear here."
                                    </div>
                                </Show>
                                {idx
                                    .capped
                                    .then(|| {
                                        view! {
                                            <div class="log-more dim">
                                                "Showing the 200 most recently active."
                                            </div>
                                        }
                                    })}
                            </div>
                        }
                            .into_any()
                    }
                }
            }}
            <Modal open=clear_confirm title="Clear all conversations">
                <p>"Delete every stored conversation, ignoring the eviction rules?"</p>
                <div class="row" style="justify-content:flex-end">
                    <button class="btn ghost" on:click=move |_| clear_confirm.set(false)>
                        "Keep them"
                    </button>
                    <button
                        class="btn danger"
                        on:click=move |_| {
                            clear_confirm.set(false);
                            gc("all");
                        }
                    >
                        "Clear all"
                    </button>
                </div>
            </Modal>
        </PageFrame>
    }
}

#[component]
fn ChainRowView(
    c: ChainRow,
    open: Callback<String>,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let toasts = use_toasts();
    let hue = hue_for(&c.model);
    // A capped model cell gives up the provider prefix first: "…/claude-sonnet-5"
    // tells one row from the next, "anthropic/cl…" does not.
    let (model_pfx, model_base) = match c.model.rfind('/') {
        Some(i) => c.model.split_at(i + 1),
        None => ("", c.model.as_str()),
    };
    let (model_pfx, model_base) = (model_pfx.to_string(), model_base.to_string());
    let open_id = c.chain_id.clone();
    let del_id = StoredValue::new(c.chain_id.clone());
    let confirm = RwSignal::new(false);
    let status_class = match c.status.as_str() {
        "completed" => "chip ok",
        "failed" => "chip err",
        _ => "chip off",
    };
    // A chain blocked on tool approval says so *instead of* its last
    // response's status: it is the one state that needs the owner, and one
    // chip keeps the column narrow. The status is in the chip's tooltip.
    let status_chip = if c.awaiting_approval {
        view! {
            <span
                class="chip live"
                title=format!("last response {} · blocked on tool approval", c.status)
            >
                <span class="dot"></span>
                "awaiting approval"
            </span>
        }
        .into_any()
    } else {
        view! {
            <span class=status_class>
                <span class="dot"></span>
                {c.status.clone()}
            </span>
        }
        .into_any()
    };
    // Tokens and size fold away on a narrow table; they stay readable on the
    // chain's own cell (par:PAR-6).
    let lead_title = format!(
        "{} · {} → {} tokens · {}",
        c.chain_id, c.input_tokens, c.output_tokens, c.size
    );
    view! {
        <tr>
            // The chain id is the column that gives: the rest of an id is in
            // its tooltip, and the other cells are what a row is told apart by.
            <td class="clip" title=lead_title>
                <button class="link-btn mono-sm" on:click=move |_| open.run(open_id.clone())>
                    {c.chain_id.clone()}
                </button>
            </td>
            <td class="conv-model mono-sm" title=c.model.clone()>
                <div class="id-cell">
                    <i class="swatch" style=format!("--hue:{hue}")></i>
                    <span class="pfx">{model_pfx}</span>
                    <span class="id-base">{model_base}</span>
                </div>
            </td>
            <td>{status_chip}</td>
            <td class="num">{c.responses}</td>
            <td class="num col-p2">{format!("{} → {}", c.input_tokens, c.output_tokens)}</td>
            <td class="num col-p2">{c.size.clone()}</td>
            <td class="dim mono-sm">{c.last_at.get(0..16).unwrap_or("").to_string()}</td>
            <td class="actions">
                <div class="row" style="justify-content:flex-end; flex-wrap:nowrap">
                    <button class="btn ghost" on:click=move |_| confirm.set(true)>
                        "Delete"
                    </button>
                    <Modal open=confirm title="Delete conversation">
                        <p>"Delete this conversation and all its responses?"</p>
                        <div class="row" style="justify-content:flex-end">
                            <button class="btn ghost" on:click=move |_| confirm.set(false)>
                                "Keep it"
                            </button>
                            <button
                                class="btn danger"
                                on:click=move |_| {
                                    confirm.set(false);
                                    let chain_id = del_id.get_value();
                                    spawn_local(async move {
                                        match crate::api::post::<
                                            Value,
                                            _,
                                        >(
                                                "/api/op/response_chain_delete",
                                                &json!({ "chain_id" : chain_id }),
                                            )
                                            .await
                                        {
                                            Ok(_) => {
                                                toasts.ok("conversation deleted");
                                                on_change();
                                            }
                                            Err(e) => toasts.err(e.to_string()),
                                        }
                                    });
                                }
                            >
                                "Delete"
                            </button>
                        </div>
                    </Modal>
                </div>
            </td>
        </tr>
    }
}

#[component]
fn ChainView(chain_id: String, on_back: Callback<()>) -> impl IntoView {
    let detail = LocalResource::new({
        let chain_id = chain_id.clone();
        move || {
            let chain_id = chain_id.clone();
            async move {
                crate::api::get::<ChainDetail>(format!(
                    "/api/responses/chain?id={}",
                    urlenc(&chain_id)
                ))
                .await
            }
        }
    });
    view! {
        <div class="row conv-summary">
            <button class="btn ghost" on:click=move |_| on_back.run(())>
                "← all conversations"
            </button>
            <span class="mono-sm dim">{chain_id.clone()}</span>
        </div>
        <div class="fill-pane">
            {move || match detail.get() {
                None => view! { <div class="card dim">"Loading…"</div> }.into_any(),
                Some(Err(e)) => {
                    view! { <div class="card dim">{e.to_string()}</div> }.into_any()
                }
                Some(Ok(d)) => {
                    let items = d.responses;
                    view! {
                        <div class="chain-flow">
                            <For each=move || items.clone() key=|r| r.id.clone() let:r>
                                <div class="card edit-section">
                                    <div class="row">
                                        <span class="mono-sm">{r.id.clone()}</span>
                                        <span class=if r.status == "completed" {
                                            "chip ok"
                                        } else {
                                            "chip err"
                                        }>
                                            <span class="dot"></span>
                                            {r.status.clone()}
                                        </span>
                                        <span class="dim mini-note">{r.outline.clone()}</span>
                                        <span class="spacer" style="flex:1"></span>
                                        <span class="dim mono-sm">
                                            {format!(
                                                "{} → {} tok · {}",
                                                r.input_tokens.unwrap_or(0),
                                                r.output_tokens.unwrap_or(0),
                                                r.created_at.get(0..16).unwrap_or(""),
                                            )}
                                        </span>
                                    </div>
                                    {(!r.pending.is_empty())
                                        .then(|| {
                                            view! {
                                                <div class="wiz-err" style="margin-top:8px">
                                                    "Awaiting approval: " {r.pending.join(", ")}
                                                </div>
                                            }
                                        })}
                                    {(!r.text.is_empty())
                                        .then(|| {
                                            view! {
                                                <div class="chain-text">{r.text.clone()}</div>
                                            }
                                        })}
                                    <details class="chain-json">
                                        <summary class="dim">"full response JSON"</summary>
                                        <pre class="preset">{r.body.clone()}</pre>
                                    </details>
                                </div>
                            </For>
                        </div>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}
