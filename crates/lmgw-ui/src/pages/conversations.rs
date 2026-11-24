//! Conversations — stored /v1/responses chains: observe and evict. The unit
//! of eviction is always the whole chain (deleting single responses would
//! strand a conversation a client is still extending).
//!
//! A list beside the opened chain on the shared split view, the chain in the
//! path (`/traffic/conversations/<chain id>`; the old `?chain=` link lands
//! there). The list is what the server lists: the most recently active
//! chains, its cap said under the search when it holds some back.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::{use_location, use_navigate, use_params_map};
use lmgw_api_types::{ChainDetail, ChainRow, MessageAck, ResponseItem, ResponsesIndex};
use serde_json::json;

use super::traffic::TrafficSwitch;
use crate::fmt::{count_of, grouped};
use crate::url_state::use_query_signal;
use crate::widgets::list_pane::{
    query_without, seg, ListBox, ListHead, ListNav, ListRefs, ListSearch,
};
use crate::widgets::nav_split::{
    provide_save_key, track_scroll, DetailHead, NavSplit, SplitConf, SplitState,
};
use crate::widgets::{
    filter_words, matches_word, use_toasts, ActionRow, Dot, Ico, Icon, InfoRow, ListRow, MenuItem,
    Modal, PrefGroup, RowMenu, StackRow, StateChip,
};

const CONF: SplitConf = SplitConf {
    class: "conv-split",
    width_key: "conversations.list.width",
    list_width: (300, 38, 480),
    detail_min: 420,
    narrow_below: 800.0,
    handle_label: "Resize the list",
};

/// The dot of a stored response's status: at rest is neutral, work in
/// progress is the accent, a failure is red.
fn status_dot(status: &str) -> Dot {
    match status {
        "completed" => Dot::Stopped,
        "in_progress" => Dot::Busy,
        "failed" => Dot::Error,
        _ => Dot::Off,
    }
}

/// A stored response's status as a word: the wire value's words, capitalised.
fn status_word(status: &str) -> String {
    match status {
        "in_progress" => "In progress".to_string(),
        other => {
            let mut w = other.replace('_', " ");
            if let Some(f) = w.get_mut(0..1) {
                f.make_ascii_uppercase();
            }
            w
        }
    }
}

/// A chain's state as the list says it, and its hero chip with it: blocked
/// on an approval first, it is the one state that needs the owner.
fn state_of(c: &ChainRow) -> (Dot, String) {
    if c.awaiting_approval {
        return (Dot::Warn, "Awaiting approval".into());
    }
    (status_dot(&c.status), status_word(&c.status))
}

fn meta_of(c: &ChainRow) -> String {
    format!(
        "{} · {} · {} → {} tok · {} · {}",
        c.last_at.get(0..16).unwrap_or(""),
        count_of(c.responses.max(0) as usize, "turns"),
        grouped(c.input_tokens.max(0) as u64),
        grouped(c.output_tokens.max(0) as u64),
        c.size,
        c.chain_id
    )
}

fn matches(c: &ChainRow, q: &str) -> bool {
    let hay = format!("{} {} {}", c.chain_id, c.model, c.status);
    filter_words(q).iter().all(|w| matches_word(&hay, w))
}

fn chain_href(id: &str, search: &str) -> String {
    format!(
        "/traffic/conversations/{}{}",
        seg(id),
        query_without(search, &["chain"])
    )
}

/// Conversations (`/traffic/conversations/:chain?`).
#[component]
pub fn TrafficConversations() -> impl IntoView {
    let toasts = use_toasts();
    let navigate = use_navigate();
    let params = use_params_map();
    let location = use_location();
    let reload = RwSignal::new(0u32);
    let bump = move || reload.update(|n| *n += 1);
    let index = LocalResource::new(move || {
        reload.get();
        crate::api::get::<ResponsesIndex>("/api/responses")
    });
    let sel = Memo::new(move |_| {
        params
            .with(|p| p.get("chain"))
            .filter(|s| !s.trim().is_empty())
    });
    let has_sel = Memo::new(move |_| sel.with(Option::is_some));
    let query = use_query_signal("q");
    let go = StoredValue::new(move |to: String, replace: bool| {
        navigate(
            &to,
            leptos_router::NavigateOptions {
                replace,
                scroll: false,
                ..Default::default()
            },
        )
    });
    // The old link: `?chain=<id>` opens the chain at its path.
    let old = use_query_signal("chain");
    Effect::new(move |_| {
        let id = old.get();
        if !id.is_empty() {
            let to = chain_href(&id, &location.search.get_untracked());
            go.with_value(|g| g(to, true));
        }
    });

    let chains = Memo::new(move |_| {
        index
            .get()
            .and_then(|r| r.ok())
            .map(|i| i.chains)
            .unwrap_or_default()
    });
    let shown = Memo::new(move |_| {
        let q = query.get();
        chains.with(|c| {
            c.iter()
                .filter(|c| matches(c, &q))
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    let count = Signal::derive(move || index.get().and_then(|r| r.ok()).map(|i| i.total_chains));

    let split = SplitState::new(CONF);
    let narrow = split.narrow;
    let save_key = provide_save_key();
    let open_row = Callback::new(move |(id, replace): (String, bool)| {
        if sel.get_untracked().as_deref() != Some(id.as_str()) {
            let to = chain_href(&id, &location.search.get_untracked());
            go.with_value(|g| g(to, replace));
        }
    });
    let order =
        Signal::derive(move || shown.with(|s| s.iter().map(|c| c.chain_id.clone()).collect()));
    let nav = ListNav::new(ListRefs::new(), order, sel, narrow, open_row, |id| {
        format!("/traffic/conversations/{id}")
    });
    let back = Callback::new(move |()| {
        let q = query_without(&location.search.get_untracked(), &["chain"]);
        go.with_value(|g| g(format!("/traffic/conversations{q}"), false));
        request_animation_frame(move || nav.focus_list());
    });

    let gc = move |scope: &'static str| {
        spawn_local(async move {
            match crate::api::post::<MessageAck, _>(
                "/api/op/responses_gc",
                &json!({ "scope": scope }),
            )
            .await
            {
                Ok(done) => {
                    toasts.ok(done.message);
                    bump();
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let clear_confirm = RwSignal::new(false);
    let menu = Signal::derive(move || {
        vec![
            MenuItem::new("Evict by rules now", move || gc("rules"))
                .title("Delete the conversations the eviction rules would delete next"),
            MenuItem::new("Clear all…", move || clear_confirm.set(true))
                .title("Delete every stored conversation, ignoring the eviction rules"),
        ]
    });
    let sub = Signal::derive(move || match index.get() {
        None => "Loading…".to_string(),
        Some(Err(_)) => "not loaded".to_string(),
        Some(Ok(i)) => {
            let all = format!(
                "{}, {}",
                count_of(i.total_chains.max(0) as usize, "conversations"),
                count_of(i.total_responses.max(0) as usize, "responses")
            );
            if query.with(|q| q.trim().is_empty()) {
                all
            } else {
                format!("{} shown · {all}", shown.with(Vec::len))
            }
        }
    });

    let list = ViewFn::from(move || {
        let actions = ViewFn::from(move || {
            view! { <RowMenu items=menu icon=Ico::More title="Conversations"/> }
        });
        view! {
            <ListHead scrolled=nav.scrolled narrow=narrow title="Traffic" sub=sub actions=actions/>
            <TrafficSwitch conversations=true count=count/>
            <ListSearch
                nav=nav
                query=query
                placeholder="Search conversations"
                title="Chain id, model or status"
            />
            {move || {
                index
                    .get()
                    .and_then(|r| r.ok())
                    .map(|idx| {
                        // The server lists the most recently active
                        // conversations only; the total above counts all of
                        // them, so the gap is said.
                        let listed = idx.chains.len() as i64;
                        let mut lines = vec![idx.rules.clone()];
                        if listed < idx.total_chains || idx.capped {
                            lines.push(format!("Listing the {listed} most recently active."));
                        }
                        if !idx.store_enabled {
                            lines.push("Storing is off: new responses are not kept.".into());
                        }
                        view! { <div class="mlist-note">{lines.join(" · ")}</div> }
                    })
            }}
            {move || {
                index
                    .get()
                    .and_then(|r| r.err())
                    .map(|e| {
                        view! {
                            <div class="notice err mlist-notice">
                                "Could not load — " {e.to_string()} " "
                                <button class="btn sm" on:click=move |_| bump()>
                                    "Retry"
                                </button>
                            </div>
                        }
                    })
            }}
            <ListBox nav=nav label="Conversations">
                <Show when=move || index.get().is_none()>
                    <div class="mlist-empty">"Loading…"</div>
                </Show>
                <Show when=move || {
                    index.get().is_some_and(|r| r.is_ok()) && shown.with(Vec::is_empty)
                }>
                    <div class="mlist-empty">
                        {move || {
                            if chains.with(Vec::is_empty) {
                                "No stored conversations — /v1/responses chains appear here."
                                    .to_string()
                            } else {
                                format!(
                                    "No conversations match \u{201c}{}\u{201d}",
                                    query.get().trim(),
                                )
                            }
                        }}
                    </div>
                </Show>
                <For
                    each=move || shown.get()
                    key=|c| {
                        (c.chain_id.clone(), c.last_at.clone(), c.status.clone(), c.awaiting_approval)
                    }
                    let:c
                >
                    {
                        let id = c.chain_id.clone();
                        let (lit, click) = (id.clone(), id.clone());
                        let (dot, word) = state_of(&c);
                        let meta = meta_of(&c);
                        let title = format!("{}\n{meta}", c.model);
                        view! {
                            <ListRow
                                key=format!("/traffic/conversations/{id}")
                                name=if c.model.is_empty() { id.clone() } else { c.model.clone() }
                                meta=meta
                                title=title
                                dot=Signal::derive(move || dot)
                                state=word
                                selected=Signal::derive(move || nav.lit(&lit))
                                on_click=Callback::new(move |()| nav.click(click.clone()))
                            />
                        }
                    }
                </For>
            </ListBox>
        }
    });

    let detail = ViewFn::from(move || {
        view! {
            <Show
                when=move || has_sel.get()
                fallback=move || {
                    view! {
                        <ConvStatus
                            narrow=narrow
                            back=back
                            index=Signal::derive(move || index.get().and_then(|r| r.ok()))
                        />
                    }
                }
            >
                <ChainView
                    id=Signal::derive(move || sel.get().unwrap_or_default())
                    row=Signal::derive(move || {
                        let id = sel.get().unwrap_or_default();
                        chains.with(|c| c.iter().find(|c| c.chain_id == id).cloned())
                    })
                    narrow=narrow
                    back=back
                    on_deleted=Callback::new(move |()| {
                        bump();
                        back.run(());
                    })
                />
            </Show>
        }
    });

    view! {
        <NavSplit
            split=split
            has_sel=has_sel
            save_key=save_key
            search=nav.search
            search_off=Signal::derive(move || narrow.get() && has_sel.get())
            list=list
            detail=detail
        />
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
    }
}

/// Nothing opened: what conversations are, and what is kept of them.
#[component]
fn ConvStatus(
    narrow: Memo<bool>,
    back: Callback<()>,
    index: Signal<Option<ResponsesIndex>>,
) -> impl IntoView {
    let scrolled = RwSignal::new(false);
    let navigate = use_navigate();
    let to_rules = Callback::new(move |()| {
        navigate(
            &crate::pages::settings_href("responses_retention_hours"),
            Default::default(),
        )
    });
    view! {
        <DetailHead
            narrow=narrow
            back=back
            back_label="Back to the conversations"
            title=Signal::derive(|| "Conversations".to_string())
            scrolled=scrolled
        />
        <div
            class="detail-scroll mdetail-scroll"
            tabindex="-1"
            on:scroll=move |ev| track_scroll(&ev, scrolled)
        >
            <div class="detail-flow conv-status">
                <div class="detail-hero">
                    <div class="glyph">
                        <Icon ico=Ico::Chat/>
                    </div>
                    <div class="hero-text">
                        <h1>"Conversations"</h1>
                        <div class="sub">
                            "Conversations a client keeps on the gateway through /v1/responses (store: true). "
                            "Pick one to read its turns; a whole conversation is what is deleted, never one turn."
                        </div>
                    </div>
                </div>
                <PrefGroup title="Kept">
                    <ActionRow
                        title="Eviction rules"
                        sub=move || {
                            index
                                .get()
                                .map(|i| crate::labels::capitalise(&i.rules))
                                .unwrap_or_default()
                        }
                        chevron=true
                        on_click=to_rules
                    />
                    {move || {
                        index
                            .get()
                            .map(|i| {
                                view! {
                                    <InfoRow
                                        title="Conversations"
                                        value=grouped(i.total_chains.max(0) as u64)
                                    />
                                    <InfoRow
                                        title="Responses"
                                        value=grouped(i.total_responses.max(0) as u64)
                                    />
                                }
                            })
                    }}
                </PrefGroup>
            </div>
        </div>
    }
}

#[component]
fn ChainView(
    id: Signal<String>,
    row: Signal<Option<ChainRow>>,
    narrow: Memo<bool>,
    back: Callback<()>,
    on_deleted: Callback<()>,
) -> impl IntoView {
    let toasts = use_toasts();
    // The answer carries the id it was asked for: one for a chain no longer
    // opened is not shown.
    let detail = LocalResource::new(move || {
        let chain_id = id.get();
        async move {
            let res = crate::api::get::<ChainDetail>(format!(
                "/api/responses/chain?id={}",
                seg(&chain_id)
            ))
            .await
            .map_err(|e| e.to_string());
            (chain_id, res)
        }
    });
    let current = Memo::new(move |_| {
        let want = id.get();
        detail.get().filter(|(i, _)| *i == want).map(|(_, r)| r)
    });
    let confirm = RwSignal::new(false);
    let delete = move || {
        let chain_id = id.get_untracked();
        spawn_local(async move {
            match crate::api::post::<MessageAck, _>(
                "/api/op/response_chain_delete",
                &json!({ "chain_id": chain_id }),
            )
            .await
            {
                Ok(_) => {
                    toasts.ok("conversation deleted");
                    on_deleted.run(());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let more = Signal::derive(move || {
        vec![
            MenuItem::new("Delete conversation…", move || confirm.set(true))
                .title("Delete this conversation and all its responses"),
        ]
    });
    let scrolled = RwSignal::new(false);
    let title = Signal::derive(move || {
        row.get()
            .map(|r| r.model)
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| id.get())
    });
    view! {
        <DetailHead
            narrow=narrow
            back=back
            back_label="Back to the conversations"
            title=title
            scrolled=scrolled
            more=more
        />
        <div
            class="detail-scroll mdetail-scroll"
            tabindex="-1"
            on:scroll=move |ev| track_scroll(&ev, scrolled)
        >
            <div class="detail-flow conv-detail">
                <div class="detail-hero">
                    <div class="glyph">
                        <Icon ico=Ico::Chat/>
                    </div>
                    <div class="hero-text">
                        <h1>{move || title.get()}</h1>
                        <div class="sub mono-sub">{move || id.get()}</div>
                        {move || {
                            row.get()
                                .map(|c| {
                                    let (dot, word) = state_of(&c);
                                    view! {
                                        <div class="chips">
                                            <StateChip dot=dot word=word/>
                                            <span class="chip">
                                                {count_of(c.responses.max(0) as usize, "turns")}
                                            </span>
                                        </div>
                                    }
                                })
                        }}
                    </div>
                </div>
                {move || {
                    row.get()
                        .map(|c| {
                            view! {
                                <PrefGroup title="Conversation">
                                    <InfoRow title="Model" value=c.model.clone() mono=true/>
                                    <InfoRow title="Turns" value=c.responses.to_string()/>
                                    <InfoRow
                                        title="Tokens"
                                        value=format!(
                                            "{} in → {} out",
                                            grouped(c.input_tokens.max(0) as u64),
                                            grouped(c.output_tokens.max(0) as u64),
                                        )
                                    />
                                    <InfoRow title="Size" value=c.size.clone()/>
                                    <InfoRow
                                        title="Started"
                                        value=c.first_at.get(0..16).unwrap_or("").to_string()
                                    />
                                    <InfoRow
                                        title="Last activity"
                                        value=c.last_at.get(0..16).unwrap_or("").to_string()
                                    />
                                    <InfoRow title="Last response" value=c.head_id.clone() mono=true/>
                                </PrefGroup>
                            }
                        })
                }}
                {move || match current.get() {
                    None => view! { <div class="mdetail-empty">"Loading…"</div> }.into_any(),
                    Some(Err(e)) => view! { <div class="notice err">{e}</div> }.into_any(),
                    Some(Ok(d)) => {
                        d.responses
                            .into_iter()
                            .enumerate()
                            .map(|(i, r)| view! { <TurnGroup n=i + 1 r=r/> })
                            .collect_view()
                            .into_any()
                    }
                }}
            </div>
        </div>
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
                        delete();
                    }
                >
                    "Delete"
                </button>
            </div>
        </Modal>
    }
}

/// One response of the chain as a boxed group: its status on the heading's
/// line, its outline under it, its tokens, its text, what it waits for, and
/// its JSON folded.
#[component]
fn TurnGroup(n: usize, r: ResponseItem) -> impl IntoView {
    let status = r.status.clone();
    let tokens = format!(
        "{} → {} tok · {}",
        r.input_tokens.unwrap_or(0),
        r.output_tokens.unwrap_or(0),
        r.created_at.get(0..16).unwrap_or(""),
    );
    let suffix = ViewFn::from(move || {
        let status = status.clone();
        view! { <StateChip dot=status_dot(&status) word=status_word(&status)/> }
    });
    view! {
        <PrefGroup title=format!("Turn {n}") desc=r.outline.clone() suffix=suffix>
            <InfoRow title="Response" value=r.id.clone() mono=true/>
            <InfoRow title="Tokens" value=tokens/>
            {(!r.pending.is_empty())
                .then(|| {
                    view! {
                        <div class="prow notice-row warn">
                            {format!("Awaiting approval: {}", r.pending.join(", "))}
                        </div>
                    }
                })}
            {(!r.text.is_empty())
                .then(|| {
                    view! {
                        <StackRow class="conv-text">
                            <div class="chain-text">{r.text.clone()}</div>
                        </StackRow>
                    }
                })}
            <StackRow class="conv-json">
                <details class="chain-json">
                    <summary>"Full response JSON"</summary>
                    <pre class="code-block">{r.body.clone()}</pre>
                </details>
            </StackRow>
        </PrefGroup>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(status: &str, awaiting: bool) -> ChainRow {
        ChainRow {
            status: status.into(),
            awaiting_approval: awaiting,
            ..Default::default()
        }
    }

    #[test]
    fn list_and_hero_read_one_state() {
        let cases = [
            (chain("completed", false), Dot::Stopped, "chip"),
            (chain("in_progress", false), Dot::Busy, "chip info"),
            (chain("failed", false), Dot::Error, "chip err"),
            (chain("cancelled", false), Dot::Off, "chip off"),
            (chain("in_progress", true), Dot::Warn, "chip warn"),
        ];
        for (c, dot, chip) in cases {
            let (d, _) = state_of(&c);
            assert_eq!(d, dot);
            assert_eq!(d.chip_class(), chip);
        }
        assert_eq!(state_of(&chain("in_progress", true)).1, "Awaiting approval");
        assert_eq!(state_of(&chain("in_progress", false)).1, "In progress");
        assert_eq!(state_of(&chain("completed", false)).1, "Completed");
    }
}
