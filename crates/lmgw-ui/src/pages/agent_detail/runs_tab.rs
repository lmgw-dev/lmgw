use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;
use lmgw_api_types::AgentDetail;

use super::*;
use crate::pages::agents::short_ts;
use crate::pages::chat::ChatThread;
use crate::pages::usage::money;
use crate::widgets::ConfirmButton;

// ---------------------------------------------------------------------------
// Runs / Threads
// ---------------------------------------------------------------------------

/// `GET /chat/api/threads?archived=all`: the envelope the chat-archive-pin-
/// attachments backend wraps every thread list in (chat.rs's own
/// `ThreadsResponse`, but that type is private to this page — the Runs tab
/// only ever needs the list, never the archived count).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
struct ThreadsEnvelope {
    threads: Vec<ChatThread>,
}

#[component]
pub(super) fn RunsTab(
    d: AgentDetail,
    run: RunState,
    runs: RunList,
    reload_runs: Callback<()>,
) -> impl IntoView {
    let navigate = StoredValue::new(use_navigate());
    let is_chat = d.kind == "chat";
    let id = StoredValue::new(d.id.clone());
    // The gateway's display currency, so a run's cost reads like every other
    // cost on the dashboard rather than as a bare number.
    let currency = StoredValue::new(d.currency.clone());
    let threads = RwSignal::new(None::<Result<Vec<ChatThread>, String>>);
    let load_threads = move || {
        let want = id.get_value();
        spawn_local(async move {
            // Filtered from the Chat page's own list rather than a second
            // endpoint: a thread is a thread, and `agent_id` is the only thing
            // that marks this one as the agent's (§2.5). `archived=all` so an
            // idle thread the auto-archive sweep filed away still shows here
            // (it just carries the "archived" mark below) instead of quietly
            // vanishing from its agent's Runs tab.
            let res = crate::api::get::<ThreadsEnvelope>("/chat/api/threads?archived=all")
                .await
                .map(|env| {
                    env.threads
                        .into_iter()
                        .filter(|t| t.agent_id.as_deref() == Some(want.as_str()))
                        .collect::<Vec<_>>()
                })
                .map_err(|e| e.to_string());
            threads.set(Some(res));
        });
    };
    if is_chat {
        load_threads();
    }

    // Reopening a run is one read: the rows come from its stored result, and
    // the review shape comes back with them, so a run finished last week still
    // draws its table (§6.2).
    let reopen = move |job_id: i64| {
        run.load(job_id);
        // Pinned, so the next jobs frame does not pull the Run tab back to
        // whatever happens to be live.
        run.pinned.set(Some(job_id));
        navigate.get_value()(&tab_href(&id.get_value(), "run"), Default::default());
    };

    view! {
        <Show when=move || is_chat>
            <div class="fill-pane hug card pad0">
                <table class="data">
                    <thead>
                        <tr>
                            <th>"Thread"</th>
                            <th>"Model"</th>
                            <th>"Last activity"</th>
                        </tr>
                    </thead>
                    <tbody>
                        {move || match threads.get() {
                            None => view! { <tr><td class="dim" colspan="3">"Loading…"</td></tr> }.into_any(),
                            Some(Err(e)) => {
                                view! {
                                    <tr>
                                        <td class="dim" colspan="3">
                                            "Failed to load the threads: " {e} " "
                                            <button class="link-btn" on:click=move |_| load_threads()>"Retry"</button>
                                        </td>
                                    </tr>
                                }
                                    .into_any()
                            }
                            Some(Ok(list)) => {
                                view! {
                                    <For each=move || list.clone() key=|t| (t.id, t.updated_at.clone()) let:t>
                                        {
                                            let title = if t.title.trim().is_empty() {
                                                format!("thread {}", t.id)
                                            } else {
                                                t.title.clone()
                                            };
                                            let archived = t.archived_at.clone();
                                            view! {
                                                <tr>
                                                    <td class="clip" title=title.clone()>
                                                        <a href=format!("/chat?t={}", t.id)>{title.clone()}</a>
                                                        {archived
                                                            .map(|a| {
                                                                view! {
                                                                    <span
                                                                        class="type-badge"
                                                                        title=format!("archived {a}")
                                                                    >
                                                                        "archived"
                                                                    </span>
                                                                }
                                                            })}
                                                    </td>
                                                    <td class="dim mono-sm">{t.model_alias.clone()}</td>
                                                    <td class="dim mono-sm" title=t.updated_at.clone()>
                                                        {short_ts(&t.updated_at)}
                                                    </td>
                                                </tr>
                                            }
                                        }
                                    </For>
                                }
                                    .into_any()
                            }
                        }}
                    </tbody>
                </table>
                <Show when=move || threads.with(|t| matches!(t, Some(Ok(l)) if l.is_empty()))>
                    <div class="empty">"No threads yet — open one from the Run tab."</div>
                </Show>
            </div>
        </Show>

        <Show when=move || !is_chat>
            <div class="fill-pane hug card pad0">
                <table class="data many-cols runs-table">
                    <thead>
                        <tr>
                            <th>"Run"</th>
                            <th>"Phase"</th>
                            <th>"Status"</th>
                            <th class="num-h">"Rows"</th>
                            <th class="num-h col-p2">"Attention"</th>
                            <th class="num-h col-p2">"Tokens"</th>
                            <th class="num-h col-p2">"Cost"</th>
                            <th class="num-h">"Duration"</th>
                            <th class="col-p3">"Started"</th>
                            <th class="col-p2">"Finished"</th>
                            <th>"Error"</th>
                            <th class="actions"></th>
                        </tr>
                    </thead>
                    <tbody>
                        {move || match runs.get() {
                            None => view! { <tr><td class="dim" colspan="12">"Loading…"</td></tr> }.into_any(),
                            Some(Err(e)) => {
                                view! {
                                    <tr>
                                        <td class="dim" colspan="12">
                                            "Failed to load the runs: " {e} " "
                                            <button class="link-btn" on:click=move |_| reload_runs.run(())>"Retry"</button>
                                        </td>
                                    </tr>
                                }
                                    .into_any()
                            }
                            Some(Ok(list)) => {
                                view! {
                                    <For each=move || list.clone() key=|r| (r.job_id, r.status.clone(), r.done) let:r>
                                        <tr>
                                            <td class="mono-sm">{format!("#{}", r.job_id)}</td>
                                            <td class="dim">{r.phase.clone()}</td>
                                            <td>
                                                <span class=status_chip(&r.status)>
                                                    <span class="dot"></span>
                                                    {r.status.clone()}
                                                </span>
                                            </td>
                                            <td
                                                class="num"
                                                title=r.detail["attention"]
                                                    .as_u64()
                                                    .map(|n| format!("{n} needed attention"))
                                                    .unwrap_or_default()
                                            >
                                                {progress_text(&r)}
                                            </td>
                                            <td class="num col-p2">
                                                {r.detail["attention"].as_u64().map(|n| n.to_string()).unwrap_or_default()}
                                            </td>
                                            <td class="num col-p2">
                                                {r.tokens.map(crate::fmt::grouped).unwrap_or_else(|| "—".into())}
                                            </td>
                                            <td
                                                class="num col-p2"
                                                title=match (r.tokens, r.cost_micro) {
                                                    (Some(_), None) => "no price is known for this model",
                                                    _ => "",
                                                }
                                            >
                                                // NULL is "nobody priced this", which is
                                                // not zero and must not read as free.
                                                {match r.cost_micro {
                                                    Some(m) => money(m, &currency.get_value()),
                                                    None => "—".to_string(),
                                                }}
                                            </td>
                                            // Tokens, cost and the finish drop first on
                                            // a narrow table; the duration keeps them in
                                            // reach (and Rows the attention count).
                                            <td
                                                class="num"
                                                title=format!(
                                                    "finished {} · {} tokens · {}",
                                                    r.finished_at.as_deref().map(short_ts).unwrap_or_else(|| "—".into()),
                                                    r.tokens.map(crate::fmt::grouped).unwrap_or_else(|| "no".into()),
                                                    match r.cost_micro {
                                                        Some(m) => money(m, &currency.get_value()),
                                                        None => "unpriced".to_string(),
                                                    },
                                                )
                                            >
                                                {duration_text(r.duration_ms)}
                                            </td>
                                            <td
                                                class="dim mono-sm col-p3"
                                                title=r.started_at.clone().unwrap_or_else(|| r.created_at.clone())
                                            >
                                                {short_ts(r.started_at.as_deref().unwrap_or(&r.created_at))}
                                            </td>
                                            <td class="dim mono-sm col-p2" title=r.finished_at.clone()>
                                                {r.finished_at.as_deref().map(short_ts).unwrap_or_default()}
                                            </td>
                                            <td class="clip status-err mono-sm" title=r.error.clone().unwrap_or_default()>
                                                {r.error.clone().unwrap_or_default()}
                                            </td>
                                            <td class="actions">
                                                {
                                                    let job_id = r.job_id;
                                                    // Opening another run replaces the review on
                                                    // screen, edits and all: ask when there are any.
                                                    let drops = move || {
                                                        run.job_id() != Some(job_id) && run.edits() > 0
                                                    };
                                                    move || {
                                                        if drops() {
                                                            view! {
                                                                <ConfirmButton
                                                                    label="Open"
                                                                    confirm=move || {
                                                                        format!(
                                                                            "Drop {} on #{}?",
                                                                            crate::fmt::count_of(run.edits(), "edits"),
                                                                            run.job_id().unwrap_or_default(),
                                                                        )
                                                                    }
                                                                    title="show this run's rows on the Run tab; the review open there has unapplied edits"
                                                                    on_confirm=Callback::new(move |()| reopen(job_id))
                                                                />
                                                            }
                                                                .into_any()
                                                        } else {
                                                            view! {
                                                                <button
                                                                    class="btn ghost sm"
                                                                    title="show this run's rows on the Run tab"
                                                                    on:click=move |_| reopen(job_id)
                                                                >
                                                                    "Open"
                                                                </button>
                                                            }
                                                                .into_any()
                                                        }
                                                    }
                                                }
                                            </td>
                                        </tr>
                                    </For>
                                }
                                    .into_any()
                            }
                        }}
                    </tbody>
                </table>
                <Show when=move || runs.with(|r| matches!(r, Some(Ok(l)) if l.is_empty()))>
                    <div class="empty">"No runs yet — start one on the Run tab."</div>
                </Show>
            </div>
        </Show>
    }
}
