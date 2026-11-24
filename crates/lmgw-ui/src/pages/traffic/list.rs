//! The Requests list pane: the header bar (the title, what the list shows and
//! whether the live tail feeds it, the GPU, Pause or Resume, the filters),
//! the Requests and Conversations switch, the search over the model, the
//! filters a link set as chips, and a row per request under its day's
//! sticky heading. "Load n older" closes the list.

use leptos::html;
use leptos::prelude::*;
use lmgw_api_types::{KeysResponse, RequestRow};

use super::{
    conversations_count, fallback_label, gpu, KnownValues, Log, TrafficSwitch, PAGE_SIZES,
};
use crate::fmt::{grouped, log_time};
use crate::widgets::list_pane::{GroupHead, ListBox, ListHead, ListNav};
use crate::widgets::{Dot, Ico, Icon, ListRow, Popover, Select, Switch};

/// One line of the list: a day's heading, or a request.
#[derive(Clone, PartialEq)]
enum Entry {
    Day(String, String, usize),
    Row(Box<RequestRow>),
}

fn entry_key(e: &Entry) -> String {
    match e {
        Entry::Day(day, _, _) => format!("d{day}"),
        Entry::Row(r) => r.log_id.to_string(),
    }
}

fn entries(rows: &[RequestRow]) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::with_capacity(rows.len() + 4);
    let mut day = String::new();
    let mut head_at = 0usize;
    for row in rows {
        let t = log_time(&row.ts);
        if t.day != day {
            day = t.day.clone();
            head_at = out.len();
            out.push(Entry::Day(t.day, t.day_label, 0));
        }
        if let Some(Entry::Day(_, _, n)) = out.get_mut(head_at) {
            *n += 1;
        }
        out.push(Entry::Row(Box::new(row.clone())));
    }
    out
}

/// Whether the request named no model: the server stores `?` then.
pub(super) fn names_no_model(r: &RequestRow) -> bool {
    r.ingress_proto != "mcp" && matches!(r.requested_alias.trim(), "" | "?")
}

/// The name a row goes by: the MCP tool for a tool call, else the alias,
/// "No model" for a request that named none.
pub(super) fn row_name(r: &RequestRow) -> String {
    if r.ingress_proto == "mcp" {
        r.mcp_tool
            .clone()
            .unwrap_or_else(|| r.requested_alias.clone())
    } else if names_no_model(r) {
        "No model".to_string()
    } else {
        r.requested_alias.clone()
    }
}

/// A row's second line: the time, who answered, how long it took, the
/// tokens, and what is unusual about it.
fn meta_of(r: &RequestRow) -> String {
    let t = log_time(&r.ts);
    let mut parts = vec![t.time];
    match (&r.upstream_name, &r.upstream_model) {
        (Some(u), Some(m)) if !m.is_empty() => parts.push(format!("{u} · {m}")),
        (Some(u), _) => parts.push(u.clone()),
        (None, Some(m)) => parts.push(m.clone()),
        (None, None) => {}
    }
    if let Some(ms) = r.total_ms {
        parts.push(format!("{} ms", grouped(ms.max(0) as u64)));
    }
    match (r.prompt_tokens, r.completion_tokens) {
        (Some(p), Some(c)) => parts.push(format!("{p} → {c} tok")),
        (Some(p), None) => parts.push(format!("{p} tok")),
        _ => {}
    }
    if let Some(reason) = &r.fallback_reason {
        parts.push(fallback_label(reason));
    }
    if r.degraded.is_some() {
        parts.push("degraded".into());
    }
    if let Some(k) = r.rung {
        parts.push(format!("rung {k}"));
    }
    parts.join(" · ")
}

#[component]
pub(super) fn ListPane(log: Log, nav: ListNav<i64>, narrow: Memo<bool>) -> impl IntoView {
    let f = log.f;
    let shown = Memo::new(move |_| log.rows.with(|r| entries(r)));
    let filters_open = RwSignal::new(false);
    let filters_btn: NodeRef<html::Button> = NodeRef::new();

    // Filters a link set that no control shows: each a chip with its remover.
    let chips = Memo::new(move |_| {
        let mut v: Vec<(String, RwSignal<String>)> = Vec::new();
        let a = f.alias.get();
        if !a.is_empty() {
            v.push((format!("alias = {a}"), f.alias));
        }
        let u = f.upstream.get();
        if !u.is_empty() {
            v.push((format!("upstream = {u}"), f.upstream));
        }
        let k = f.error_kind.get();
        if !k.is_empty() {
            v.push((
                format!("Kind: {}", crate::labels::error_kind_label(&k)),
                f.error_kind,
            ));
        }
        v
    });
    let own_active = Signal::derive(move || f.own_active());

    let render = move |e: Entry| -> AnyView {
        match e {
            Entry::Day(day, label, _) => {
                // Keyed by its day: the count follows the live tail.
                let n = Signal::derive(move || {
                    shown.with(|s| {
                        s.iter()
                            .find_map(|e| match e {
                                Entry::Day(d, _, n) if *d == day => Some(*n),
                                _ => None,
                            })
                            .unwrap_or(0)
                    })
                });
                view! { <GroupHead label=label n=n/> }.into_any()
            }
            Entry::Row(r) => {
                let id = r.log_id;
                let name = row_name(&r);
                let meta = meta_of(&r);
                let t = log_time(&r.ts);
                let mut tip = vec![name.clone(), meta.clone(), t.utc.clone()];
                if let Some(m) = &r.error_msg {
                    tip.push(m.clone());
                }
                let key = format!("/traffic/{id}");
                let selected = Signal::derive(move || nav.lit(&id));
                let on_click = Callback::new(move |()| nav.click(id));
                let (tip, state) = (tip.join("\n"), r.status.to_string());
                let state_ico = r.streamed.then_some(Ico::Stream);
                let quiet = names_no_model(&r);
                // A failed request has the red dot; one that was answered
                // says its status alone. A streamed one has the stream icon.
                if r.status >= 400 {
                    view! {
                        <ListRow
                            key=key
                            name=name
                            meta=meta
                            title=tip
                            dot=Signal::derive(|| Dot::Error)
                            state=state
                            state_ico=state_ico
                            state_title="Streamed"
                            quiet_name=quiet
                            selected=selected
                            on_click=on_click
                        />
                    }
                    .into_any()
                } else {
                    view! {
                        <ListRow
                            key=key
                            name=name
                            meta=meta
                            title=tip
                            state=state
                            state_ico=state_ico
                            state_title="Streamed"
                            quiet_name=quiet
                            selected=selected
                            on_click=on_click
                        />
                    }
                    .into_any()
                }
            }
        }
    };

    let actions = ViewFn::from(move || {
        view! {
            <gpu::GpuButton vram=log.vram/>
            <button
                type="button"
                class="icon-btn"
                title=move || {
                    if log.halted.get() {
                        "Resume the live tail (the rows counted meanwhile are read)"
                    } else {
                        "Pause the live tail: new rows are counted, not prepended"
                    }
                }
                aria-label=move || if log.halted.get() { "Resume" } else { "Pause" }
                on:click=move |_| {
                    if log.halted.get_untracked() {
                        log.resume.run(())
                    } else {
                        log.paused.set(true)
                    }
                }
            >
                {move || {
                    if log.halted.get() {
                        view! { <Icon ico=Ico::Play/> }
                    } else {
                        view! { <Icon ico=Ico::Pause/> }
                    }
                }}
            </button>
            <button
                type="button"
                class="icon-btn tr-filter-btn"
                class:on=move || own_active.get()
                node_ref=filters_btn
                title="Filters: upstream, class, key, errors only, rows per page"
                aria-label="Filters"
                aria-expanded=move || filters_open.get().to_string()
                on:click=move |_| filters_open.update(|o| *o = !*o)
            >
                <Icon ico=Ico::Sliders/>
            </button>
            <Popover open=filters_open anchor=filters_btn class="tr-filters-pop" min_width=320>
                <Filters log=log/>
            </Popover>
        }
    });

    view! {
        <ListHead scrolled=nav.scrolled narrow=narrow title="Traffic" sub=log.status actions=actions/>
        <TrafficSwitch conversations=false count=conversations_count()/>
        <label class="msearch" class:has=move || !f.alias_text.with(String::is_empty)>
            <Icon ico=Ico::Search/>
            <input
                node_ref=nav.search
                data-slash
                data-untracked
                type="search"
                placeholder="Model contains…"
                spellcheck="false"
                autocomplete="off"
                aria-label="Model contains"
                title="A part of the model's alias, any case · / or Ctrl+F focuses, Esc clears"
                prop:value=move || f.alias_text.get()
                on:input=move |ev| f.alias_text.set(event_target_value(&ev))
                on:keydown=move |ev| nav.on_search_key(f.alias_text, ev)
            />
            <span class="known-box tr-known">
                <KnownValues dim="alias" noun="aliases" text=f.alias_text apply=f.alias_q/>
            </span>
        </label>
        <Show when=move || !chips.with(Vec::is_empty) || own_active.get()>
            <div class="mlist-chips">
                <For each=move || chips.get() key=|(l, _)| l.clone() let:c>
                    {
                        let (label, sig) = c;
                        view! {
                            <span class="chip filter-chip">
                                {label.clone()}
                                <button
                                    type="button"
                                    class="chip-x"
                                    title="Remove this filter"
                                    aria-label=format!("Remove {label}")
                                    on:click=move |_| sig.set(String::new())
                                >
                                    <Icon ico=Ico::Close/>
                                </button>
                            </span>
                        }
                    }
                </For>
                <Show when=move || own_active.get()>
                    <button type="button" class="link-btn" on:click=move |_| filters_open.set(true)>
                        "More filters set"
                    </button>
                </Show>
                <button type="button" class="link-btn" on:click=move |_| f.clear()>
                    "Clear all"
                </button>
            </div>
        </Show>
        {move || {
            log.error
                .get()
                .map(|e| view! { <div class="notice err mlist-notice">"Could not load — " {e}</div> })
        }}
        <ListBox nav=nav label="Requests">
            <div class="tr-rows" class:stale=move || log.loading.get() && log.rows.with(|r| !r.is_empty())>
                <For each=move || shown.get() key=entry_key let:e>
                    {render(e)}
                </For>
            </div>
            <Show when=move || !log.loading.get() && log.rows.with(Vec::is_empty)>
                <div class="mlist-empty">"No matching requests."</div>
            </Show>
            <Show when=move || log.rows.with(|r| !r.is_empty())>
                <div class="mlist-foot">
                    <Show
                        when=move || log.more.get()
                        fallback=|| {
                            view! {
                                <span>"The start of the log for these filters — nothing older is kept."</span>
                            }
                        }
                    >
                        <button
                            type="button"
                            class="btn sm"
                            disabled=move || log.loading.get()
                            on:click=move |_| log.load_older.run(())
                        >
                            {move || format!("Load {} older", grouped(log.page.get() as u64))}
                        </button>
                        <span>"appends below; the live tail pauses while older rows are shown"</span>
                    </Show>
                </div>
            </Show>
        </ListBox>
    }
}

/// The filters the popover holds, as rows.
#[component]
fn Filters(log: Log) -> impl IntoView {
    let f = log.f;
    // Key names for the picker. A key filter matches on the id — a name match
    // breaks the moment a key is renamed — so the id is what travels in the
    // URL and this is only the label for it.
    let keys = LocalResource::new(|| crate::api::get::<KeysResponse>("/api/usage/keys"));
    let key_options = Signal::derive(move || {
        let mut v = vec![(String::new(), "All keys".to_string())];
        if let Some(Ok(k)) = keys.get() {
            v.extend(k.keys.iter().map(|k| (k.id.to_string(), k.name.clone())));
        }
        // A URL can carry an id the list does not know (a deleted key still has
        // history). Show the raw id rather than silently falling back to "all".
        let cur = f.key_id.get();
        if !cur.is_empty() && !v.iter().any(|(val, _)| *val == cur) {
            v.push((cur.clone(), format!("key #{cur}")));
        }
        v
    });
    let class_options = Signal::derive(|| {
        vec![
            (String::new(), "All classes".to_string()),
            ("chat".into(), "Chat".into()),
            ("aux".into(), "Aux (embed, rerank, decision)".into()),
            ("audio".into(), "Audio".into()),
            ("image".into(), "Image".into()),
            ("tool".into(), "Tool".into()),
        ]
    });
    let size_options = Signal::derive(|| {
        PAGE_SIZES
            .iter()
            .map(|s| {
                let n: u64 = s.parse().unwrap_or(0);
                (s.to_string(), format!("{} rows", grouped(n)))
            })
            .collect::<Vec<_>>()
    });
    // The switch's own signal, kept in step with the URL's `errors_only`.
    let errors = RwSignal::new(f.errors_only.get_untracked());
    Effect::new(move |_| {
        let on = f.errors_only.get();
        if errors.get_untracked() != on {
            errors.set(on);
        }
    });
    Effect::new(move |prev: Option<()>| {
        let on = errors.get();
        if prev.is_some() && f.errors_only.get_untracked() != on {
            f.errors_only_raw
                .set(if on { "true".into() } else { String::new() });
        }
    });
    view! {
        <div class="boxed tr-filters">
            <div class="prow entry">
                <div class="prow-text"><span class="prow-title">"Upstream contains"</span></div>
                <div class="prow-end">
                    <span class="known-box">
                        <input
                            class="prow-entry"
                            type="search"
                            placeholder="any upstream"
                            autocomplete="off"
                            spellcheck="false"
                            data-untracked
                            prop:value=move || f.upstream_text.get()
                            on:input=move |ev| f.upstream_text.set(event_target_value(&ev))
                        />
                        <KnownValues dim="upstream" noun="upstreams" text=f.upstream_text apply=f.upstream_q/>
                    </span>
                </div>
            </div>
            <div class="prow combo">
                <div class="prow-text"><span class="prow-title">"Class"</span></div>
                <div class="prow-end"><Select value=f.class options=class_options/></div>
            </div>
            <div class="prow combo">
                <div class="prow-text"><span class="prow-title">"Key"</span></div>
                <div class="prow-end"><Select value=f.key_id options=key_options/></div>
            </div>
            <div class="prow action" on:click=move |_| errors.update(|v| *v = !*v)>
                <div class="prow-text"><span class="prow-title">"Errors only"</span></div>
                <Switch value=errors label="Errors only"/>
            </div>
            <div class="prow combo" title="Rows per page — and how many the live tail keeps">
                <div class="prow-text">
                    <span class="prow-title">"Rows per page"</span>
                    <span class="prow-sub">"and how many the live tail keeps"</span>
                </div>
                <div class="prow-end"><Select value=log.size_pref options=size_options/></div>
            </div>
            <div class="prow tr-filters-foot">
                <button type="button" class="btn sm" on:click=move |_| f.clear()>
                    "Clear filters"
                </button>
            </div>
        </div>
    }
}
