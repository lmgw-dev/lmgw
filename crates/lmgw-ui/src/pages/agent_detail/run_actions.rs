//! The detail header's run buttons, by run kind: Open in Chat for a `chat`
//! agent (one op: the thread is seeded server-side from the manifest and the
//! page navigates to it), Start run for a `container` one, List only and
//! Dry run for a `batch` one; Stop while a run is in flight. Each says in its
//! tooltip what keeps it off (the run group says it in words too).

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;
use lmgw_api_types::AgentChatOpened;
use serde_json::json;

use super::*;
use crate::widgets::{Ico, Icon};

#[component]
pub(super) fn RunActions(kind: String, ctx: RunCtx) -> impl IntoView {
    let run = ctx.run;
    let toasts = ctx.toasts;
    let has_classify = ctx.shape.with_untracked(|s| s.has_classify);
    // `StoredValue`, not a local clone: the handlers live inside reactive
    // children, which are re-rendered, so a handler that moved a cloned
    // navigator out of its environment would be `FnOnce`.
    let navigate = StoredValue::new(use_navigate());
    let busy = RwSignal::new(false);
    let live = Signal::derive(move || run.summary.with(|s| s.as_ref().is_some_and(is_live)));
    let off = Signal::derive(move || ctx.blocked() || run.busy.get() || live.get());
    let classify_off =
        Signal::derive(move || ctx.classify_blocked() || run.busy.get() || live.get());
    let why = move || ctx.blocked_by.get().unwrap_or_default();
    let classify_why = move || ctx.classify_blocked_by.get().unwrap_or_default();

    let open_chat = move |_| {
        if busy.get_untracked() {
            return;
        }
        // The form as it stands, not what is stored: selecting a model and
        // opening a thread with it is one gesture, and the thread is seeded
        // from what the person is looking at. Save config is a separate act
        // that sets the defaults this falls back to.
        let values = match ctx.draft.patch(&ctx.fields.get_untracked()) {
            Ok(p) => p,
            Err(e) => {
                ctx.problem.set(Some(e));
                return;
            }
        };
        ctx.problem.set(None);
        busy.set(true);
        let id = ctx.agent_id.get_value();
        spawn_local(async move {
            let res = crate::api::post::<AgentChatOpened, _>(
                "/api/op/agent_open_chat",
                &json!({ "id": id, "values": values }),
            )
            .await;
            busy.set(false);
            match res {
                Ok(v) => {
                    for w in v.warnings {
                        // Amber, not red: the thread was created. A warning
                        // here is "it opened, and here is what is off about
                        // it" — an unserved alias, a missing MCP server.
                        toasts.warn(w);
                    }
                    if let Some(go) = navigate.try_get_value() {
                        go(&v.url, Default::default());
                    }
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let stop = move || {
        view! {
            <button
                type="button"
                class="btn flat"
                title="Ask the run to stop; what it has written so far stays"
                on:click=move |_| ctx.cancel()
            >
                <Icon ico=Ico::Stop/>
                "Stop"
            </button>
        }
    };

    match kind.as_str() {
        "chat" => view! {
            <button
                type="button"
                class="btn suggested"
                disabled=move || busy.get() || ctx.blocked()
                title=move || {
                    if ctx.blocked() {
                        why()
                    } else {
                        "Open a Chat thread seeded from this agent, with the config as it stands".to_string()
                    }
                }
                on:click=open_chat
            >
                {move || if busy.get() { "Opening…" } else { "Open in Chat" }}
            </button>
        }
        .into_any(),
        "container" => view! {
            <Show
                when=move || live.get()
                fallback=move || {
                    view! {
                        <button
                            type="button"
                            class="btn suggested"
                            title=move || {
                                if ctx.blocked() {
                                    why()
                                } else {
                                    "Start the agent's image with the config as it stands; it writes nothing until Apply".to_string()
                                }
                            }
                            disabled=move || off.get()
                            on:click=move |_| ctx.start("run")
                        >
                            <Icon ico=Ico::Play/>
                            "Start run"
                        </button>
                    }
                }
            >
                {stop}
            </Show>
        }
        .into_any(),
        _ => view! {
            <Show
                when=move || live.get()
                fallback=move || {
                    view! {
                        <button
                            type="button"
                            class="btn flat"
                            title=move || {
                                if ctx.blocked() {
                                    why()
                                } else {
                                    "source and fetch only — no model call".to_string()
                                }
                            }
                            disabled=move || off.get()
                            on:click=move |_| ctx.start("list")
                        >
                            "List only"
                        </button>
                        {has_classify
                            .then(|| {
                                view! {
                                    <button
                                        type="button"
                                        class="btn suggested"
                                        title=move || {
                                            if ctx.classify_blocked() {
                                                classify_why()
                                            } else {
                                                "list, fetch and classify; nothing is written until Apply".to_string()
                                            }
                                        }
                                        disabled=move || classify_off.get()
                                        on:click=move |_| ctx.start("classify")
                                    >
                                        <Icon ico=Ico::Play/>
                                        "Dry run"
                                    </button>
                                }
                            })}
                    }
                }
            >
                {stop}
            </Show>
        }
        .into_any(),
    }
}
