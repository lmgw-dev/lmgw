//! The model picker (UX plan §2 #12): every place a model is chosen, over the
//! shared catalog, instead of a 485-option list.
//!
//! The pop leads with a filter (all words must match, over id, source and
//! vendor), source chips with counts, and the line that says how much is
//! showing. Models are grouped by where they are served from — Recent, the
//! local runtimes, then one group per upstream — and a group of 50 or more
//! starts folded ("kilo-gw · 395"), opening while a filter narrows it ("12 of
//! 395"). Models for other tasks than the picker's are held back behind a
//! chip that says how many there are; nothing is ever silently missing.

use std::collections::BTreeSet;

use leptos::html;
use leptos::prelude::*;

use super::filter_words;
use super::popover::{self, Popover};
use crate::catalog::{use_model_catalog, CatalogEntry, FRESH_SECS};
use crate::fmt::{grouped, hue_for, price};

/// A group this size or larger starts folded when nothing is typed — unless
/// it is the only group, which has nothing to make room for.
const FOLD_AT: usize = 50;

/// Where a caller's own `entries` list stands. While it loads, or after it
/// failed, an id missing from it is not known to be unserved, and the pop
/// says why the list is short.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ListStatus {
    #[default]
    Ready,
    Loading,
    Failed(String),
}

/// One line of the pop's list. Every item renders exactly one element, so
/// the keyboard's position is also a child index of the list.
#[derive(Clone, PartialEq)]
enum Item {
    Head {
        group: String,
        label: String,
        count: String,
        open: bool,
        note: Option<&'static str>,
    },
    Row {
        /// `r:` in Recent, `e:` in its group, `x:` not allowed here.
        tag: char,
        entry: CatalogEntry,
        disabled: bool,
    },
    /// The typed text itself, for an id the catalog does not list.
    Custom(String),
    /// The current value, which nothing serves any more.
    Pinned(String),
    /// The caller's "" option ("manifest default").
    Empty(String),
}

impl Item {
    /// Stable identity, for the keyboard's position.
    fn nav_id(&self) -> String {
        match self {
            Item::Head { group, .. } => format!("h:{group}"),
            Item::Row { tag, entry, .. } => format!("{tag}:{}", entry.id),
            Item::Custom(_) => "c:".to_string(),
            Item::Pinned(id) => format!("p:{id}"),
            Item::Empty(_) => "empty".to_string(),
        }
    }

    /// Identity plus everything it displays: a keyed list never updates an
    /// entry in place, so a head whose count changed must get a new key.
    fn render_key(&self) -> String {
        match self {
            Item::Head {
                group, count, open, ..
            } => format!("h:{group}:{count}:{open}"),
            Item::Custom(q) => format!("c:{q}"),
            other => other.nav_id(),
        }
    }

    fn navigable(&self) -> bool {
        !matches!(self, Item::Row { disabled: true, .. })
    }

    fn pickable(&self) -> bool {
        matches!(
            self,
            Item::Row {
                disabled: false,
                ..
            } | Item::Custom(_)
                | Item::Empty(_)
        )
    }
}

#[component]
pub fn ModelPicker(
    value: RwSignal<String>,
    /// The `capabilities.task` values this choice is for (`&["chat"]`);
    /// empty means every task. Models of other tasks stay reachable behind
    /// the "+N other tasks" chip, and a model whose task is unknown is
    /// always offered.
    #[prop(optional)]
    tasks: &'static [&'static str],
    /// Adds a "" choice with this label, e.g. "manifest default".
    #[prop(optional, into)]
    empty_label: Option<String>,
    /// Models the predicate refuses are listed disabled in a folded "Not
    /// allowed here" group, with the reason.
    #[prop(optional)]
    disallow: Option<(Callback<CatalogEntry, bool>, &'static str)>,
    /// A list of the caller's own instead of the gateway catalog (a lab's
    /// endpoint, an upstream's live catalog).
    #[prop(optional, into)]
    entries: Option<Signal<Vec<CatalogEntry>>>,
    /// How far `entries` has got (loading, failed); ready when unset.
    #[prop(optional, into)]
    status: Option<Signal<ListStatus>>,
    /// Offer the typed text as an id of its own.
    #[prop(optional)]
    allow_custom: bool,
    /// Remember picks under `lmgw.ui.recent.<key>` and list them first.
    #[prop(optional)]
    recent_key: Option<&'static str>,
    #[prop(into, default = false.into())] disabled: Signal<bool>,
) -> impl IntoView {
    let catalog = use_model_catalog();
    let all: Signal<Vec<CatalogEntry>> = entries.unwrap_or_else(|| catalog.entries.into());
    let own_list = entries.is_some();
    let own_status = move || status.map(|s| s.get()).unwrap_or_default();
    // The list is not final yet: a value missing from it may still be in it.
    let settling = move || {
        if own_list {
            own_status() != ListStatus::Ready
        } else {
            catalog.loading.get()
        }
    };

    let open = RwSignal::new(false);
    let btn: NodeRef<html::Button> = NodeRef::new();
    let input: NodeRef<html::Input> = NodeRef::new();
    let list: NodeRef<html::Div> = NodeRef::new();
    let query = RwSignal::new(String::new());
    let src = RwSignal::new(String::new());
    let other = RwSignal::new(false);
    let task_f = RwSignal::new(String::new());
    let toggled = RwSignal::new(BTreeSet::<String>::new());
    let active = RwSignal::new(None::<String>);
    let recents = RwSignal::new(Vec::<String>::new());
    let was_open = StoredValue::new(false);
    let empty_label = StoredValue::new(empty_label);

    let in_scope = move |e: &CatalogEntry| {
        tasks.is_empty() || e.task.as_deref().is_none_or(|t| tasks.contains(&t))
    };
    let scope_len = Memo::new(move |_| all.with(|a| a.iter().filter(|e| in_scope(e)).count()));
    let others = Memo::new(move |_| all.with(Vec::len) - scope_len.get());

    // What survives the task scope, the task and source chips and the words.
    let matched = Memo::new(move |_| {
        let words = filter_words(&query.get());
        let (src, task_f, other) = (src.get(), task_f.get(), other.get());
        all.with(|a| {
            a.iter()
                .filter(|e| other || in_scope(e))
                .filter(|e| task_f.is_empty() || e.task.as_deref().unwrap_or("?") == task_f)
                .filter(|e| src.is_empty() || e.source == src)
                .filter(|e| e.matches(&words))
                .cloned()
                .collect::<Vec<_>>()
        })
    });

    let items = Memo::new(move |_| {
        let q = query.get();
        let filtering = !q.trim().is_empty();
        let cur = value.get();
        let toggled = toggled.get();
        let is_open = |g: &str, default: bool| default != toggled.contains(g);
        let mut out = Vec::new();

        if allow_custom && filtering && all.with(|a| !a.iter().any(|e| e.id == q.trim())) {
            out.push(Item::Custom(q.trim().to_string()));
        }
        if !cur.is_empty() && !own_list_has(all, &cur) && !settling() {
            out.push(Item::Pinned(cur.clone()));
        }
        if let Some(l) = empty_label.get_value() {
            if !filtering || l.to_lowercase().contains(&q.trim().to_lowercase()) {
                out.push(Item::Empty(l));
            }
        }

        let refused = |e: &CatalogEntry| {
            disallow
                .as_ref()
                .is_some_and(|(pred, _)| pred.run(e.clone()))
        };
        let (allowed, not_allowed): (Vec<CatalogEntry>, Vec<CatalogEntry>) =
            matched.get().into_iter().partition(|e| !refused(e));

        if !filtering {
            let rec: Vec<CatalogEntry> = recents
                .get()
                .iter()
                .filter_map(|id| allowed.iter().find(|e| &e.id == id).cloned())
                .collect();
            if !rec.is_empty() {
                let open = is_open("!recent", true);
                out.push(Item::Head {
                    group: "!recent".into(),
                    label: "Recent".into(),
                    count: grouped(rec.len() as u64),
                    open,
                    note: None,
                });
                if open {
                    out.extend(rec.into_iter().map(|entry| Item::Row {
                        tag: 'r',
                        entry,
                        disabled: false,
                    }));
                }
            }
        }

        // Group totals are over the task scope, not the words: "12 of 395".
        let scope_total = |label: &str| {
            let other = other.get_untracked();
            all.with(|a| {
                a.iter()
                    .filter(|e| e.group_label == label && (other || in_scope(e)))
                    .count()
            })
        };
        let lone = allowed
            .first()
            .is_some_and(|f| allowed.iter().all(|e| e.group_label == f.group_label));
        let mut i = 0;
        while i < allowed.len() {
            let label = allowed[i].group_label.clone();
            let j = allowed[i..]
                .iter()
                .position(|e| e.group_label != label)
                .map_or(allowed.len(), |k| i + k);
            let members = &allowed[i..j];
            let total = scope_total(&label);
            let holds_value = members.iter().any(|e| e.id == cur);
            let open = is_open(&label, filtering || lone || total < FOLD_AT || holds_value);
            out.push(Item::Head {
                group: label.clone(),
                label: label.clone(),
                count: crate::fmt::of(members.len(), total),
                open,
                note: None,
            });
            if open {
                out.extend(members.iter().cloned().map(|entry| Item::Row {
                    tag: 'e',
                    entry,
                    disabled: false,
                }));
            }
            i = j;
        }

        if !not_allowed.is_empty() {
            let open = is_open("!refused", false);
            out.push(Item::Head {
                group: "!refused".into(),
                label: "Not allowed here".into(),
                count: grouped(not_allowed.len() as u64),
                open,
                note: disallow.as_ref().map(|(_, why)| *why),
            });
            if open {
                out.extend(not_allowed.into_iter().map(|entry| Item::Row {
                    tag: 'x',
                    entry,
                    disabled: true,
                }));
            }
        }
        out
    });

    // Opening starts over: no words, no chips, groups at their defaults,
    // the keyboard on the current value.
    Effect::new(move |_| {
        if !open.get() {
            return;
        }
        query.set(String::new());
        src.set(String::new());
        task_f.set(String::new());
        other.set(false);
        toggled.set(BTreeSet::new());
        recents.set(recent_key.map(crate::prefs::recent).unwrap_or_default());
        if !own_list {
            catalog.refresh_if_older(FRESH_SECS);
        }
        let cur = value.get_untracked();
        let start = items.with_untracked(|v| {
            v.iter()
                .find(|i| matches!(i, Item::Row { entry, .. } if entry.id == cur))
                .or_else(|| v.iter().find(|i| i.pickable()))
                .map(Item::nav_id)
        });
        active.set(start);
    });

    let pick = move |id: String| {
        if let (Some(k), false) = (recent_key, id.is_empty()) {
            crate::prefs::recent_push(k, &id);
        }
        value.set(id);
        open.set(false);
        if let Some(b) = btn.get_untracked() {
            let _ = b.focus();
            popover::fire_change(&b);
        }
    };
    let toggle_group = move |g: String| {
        toggled.update(|t| {
            if !t.remove(&g) {
                t.insert(g);
            }
        });
    };
    let activate = move |it: Item| match it {
        Item::Head { group, .. } => toggle_group(group),
        Item::Row {
            entry,
            disabled: false,
            ..
        } => pick(entry.id),
        Item::Custom(q) => pick(q),
        Item::Empty(_) => pick(String::new()),
        Item::Pinned(_) | Item::Row { .. } => {}
    };
    let refocus = move || {
        if let Some(i) = input.get_untracked() {
            let _ = i.focus();
        }
    };
    let move_to = move |idx: usize| {
        let Some(id) = items.with_untracked(|v| v.get(idx).map(Item::nav_id)) else {
            return;
        };
        active.set(Some(id));
        if let Some(l) = list.get_untracked() {
            popover::reveal_child(&l, idx);
        }
    };
    // The next navigable item from `from` in direction `dir`, `steps` times.
    let step = move |from: Option<usize>, down: bool, steps: usize| -> Option<usize> {
        items.with_untracked(|v| {
            let n = v.len();
            if n == 0 {
                return None;
            }
            let mut at = from;
            let mut moved = 0;
            let mut probe = from;
            loop {
                let next = match (probe, down) {
                    (None, true) => 0,
                    (None, false) => n - 1,
                    (Some(k), true) if k + 1 < n => k + 1,
                    (Some(k), false) if k > 0 => k - 1,
                    _ => break,
                };
                probe = Some(next);
                if v[next].navigable() {
                    at = Some(next);
                    moved += 1;
                    if moved == steps {
                        break;
                    }
                }
            }
            at
        })
    };
    let on_key = move |ev: web_sys::KeyboardEvent| {
        let cur = active.with_untracked(|a| {
            a.as_ref()
                .and_then(|id| items.with_untracked(|v| v.iter().position(|i| &i.nav_id() == id)))
        });
        let page = list
            .get_untracked()
            .map(|l| popover::page_rows(&l))
            .unwrap_or(8);
        let typed = !query.with_untracked(|q| q.is_empty());
        let target = match ev.key().as_str() {
            "ArrowDown" => step(cur, true, 1),
            "ArrowUp" => step(cur, false, 1),
            "PageDown" => step(cur, true, page),
            "PageUp" => step(cur, false, page),
            "Home" if !typed => step(None, true, 1),
            "End" if !typed => step(None, false, 1),
            "Enter" => {
                if let Some(it) = cur.and_then(|i| items.with_untracked(|v| v.get(i).cloned())) {
                    activate(it);
                }
                ev.prevent_default();
                return;
            }
            "Escape" => {
                ev.stop_propagation();
                ev.prevent_default();
                open.set(false);
                if let Some(b) = btn.get_untracked() {
                    let _ = b.focus();
                }
                return;
            }
            "Tab" => {
                open.set(false);
                if let Some(b) = btn.get_untracked() {
                    let _ = b.focus();
                }
                return;
            }
            _ => return,
        };
        ev.prevent_default();
        if let Some(i) = target {
            move_to(i);
        }
    };

    // The button: what is chosen, and where it is served from.
    let chosen = move || {
        let v = value.get();
        let found = all.with(|a| a.iter().find(|e| e.id == v).cloned());
        (v, found)
    };

    let source_chips = Memo::new(move |_| {
        let other = other.get();
        let mut chips: Vec<(String, String, usize)> = Vec::new();
        all.with(|a| {
            for e in a.iter().filter(|e| other || in_scope(e)) {
                match chips.iter_mut().find(|c| c.0 == e.source) {
                    Some(c) => c.2 += 1,
                    None => chips.push((e.source.clone(), e.group_label.clone(), 1)),
                }
            }
        });
        // One source would only repeat the "All" chip's count.
        if chips.len() < 2 {
            chips.clear();
        }
        chips
    });
    let task_chips = Memo::new(move |_| {
        let mut chips: Vec<(String, usize)> = Vec::new();
        all.with(|a| {
            for e in a {
                let t = e.task.clone().unwrap_or_else(|| "?".into());
                match chips.iter_mut().find(|c| c.0 == t) {
                    Some(c) => c.1 += 1,
                    None => chips.push((t, 1)),
                }
            }
        });
        chips.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        chips
    });

    view! {
        <div class="select model-picker">
            <button
                type="button"
                class="input select-btn mp-btn"
                node_ref=btn
                aria-haspopup="listbox"
                aria-expanded=move || open.get().to_string()
                disabled=move || disabled.get()
                on:pointerdown=move |_| was_open.set_value(open.get_untracked())
                on:click=move |_| {
                    let reopen = !was_open.get_value();
                    was_open.set_value(false);
                    if !disabled.get_untracked() {
                        open.set(reopen && !open.get_untracked());
                    }
                }
                on:keydown=move |ev| {
                    if !open.get_untracked() && matches!(ev.key().as_str(), "ArrowDown" | "ArrowUp") {
                        ev.prevent_default();
                        open.set(true);
                    }
                }
            >
                {move || {
                    let (v, found) = chosen();
                    match found {
                        Some(e) => {
                            let (pfx, name) = e.split_name();
                            view! {
                                <span class="mp-val" title=e.id.clone()>
                                    <i class="mp-dot" style=format!("--hue:{}", hue_for(&e.group_label))></i>
                                    <span class="mp-pfx">{pfx.to_string()}</span>
                                    {name.to_string()}
                                </span>
                                <span class="mp-src" title=e.group_label.clone()>{e.group_label.clone()}</span>
                            }
                                .into_any()
                        }
                        None if v.is_empty() => {
                            let l = empty_label.get_value();
                            let dim = l.is_none();
                            view! {
                                <span class="mp-val" class:dim=dim>
                                    {l.unwrap_or_else(|| "pick a model".to_string())}
                                </span>
                            }
                                .into_any()
                        }
                        // Still loading: the value is all there is to show.
                        None if settling() => {
                            view! { <span class="mp-val">{v}</span> }.into_any()
                        }
                        None => {
                            view! {
                                <span class="mp-val" title="No upstream or local runtime serves this id now">
                                    {v}
                                </span>
                                <span class="mp-src warn">"not served"</span>
                            }
                                .into_any()
                        }
                    }
                }}
                <span class="select-arrow">"▾"</span>
            </button>
            <Popover open=open anchor=btn class="mp-pop">
                <div class="pop-inner mp" on:keydown=on_key>
                    <div class="mp-top">
                        <input
                            class="input mp-q"
                            type="search"
                            node_ref=input
                            placeholder="Filter models — every word must match"
                            autocomplete="off"
                            spellcheck="false"
                            data-autofocus
                            data-untracked
                            prop:value=move || query.get()
                            on:input=move |ev| {
                                query.set(event_target_value(&ev));
                                // A match first: Enter after typing part of a
                                // name means that model, not the fragment.
                                let first = items.with(|v| {
                                    v.iter()
                                        .find(|i| matches!(i, Item::Row { disabled: false, .. }))
                                        .or_else(|| v.iter().find(|i| i.pickable()))
                                        .map(Item::nav_id)
                                });
                                active.set(first);
                                if let Some(l) = list.get_untracked() {
                                    l.set_scroll_top(0);
                                }
                            }
                        />
                        <div class="mp-facets">
                            <button
                                type="button"
                                class="facet"
                                class:on=move || src.with(String::is_empty)
                                on:click=move |_| {
                                    src.set(String::new());
                                    refocus();
                                }
                            >
                                "All"
                                <span class="count">
                                    {move || {
                                        let o = other.get();
                                        grouped(all.with(|a| a.iter().filter(|e| o || in_scope(e)).count()) as u64)
                                    }}
                                </span>
                            </button>
                            <For each=move || source_chips.get() key=|c| c.clone() let:c>
                                {
                                    let id = c.0.clone();
                                    let on = {
                                        let id = id.clone();
                                        move || src.with(|s| *s == id)
                                    };
                                    view! {
                                        <button
                                            type="button"
                                            class="facet"
                                            class:on=on
                                            on:click=move |_| {
                                                src.set(id.clone());
                                                refocus();
                                            }
                                        >
                                            {c.1.clone()}
                                            <span class="count">{grouped(c.2 as u64)}</span>
                                        </button>
                                    }
                                }
                            </For>
                            <Show when=move || { others.get() > 0 }>
                                <button
                                    type="button"
                                    class="facet"
                                    class:on=move || other.get()
                                    aria-pressed=move || other.get().to_string()
                                    title="Models for other tasks than this choice is for"
                                    on:click=move |_| {
                                        other.update(|o| *o = !*o);
                                        task_f.set(String::new());
                                        refocus();
                                    }
                                >
                                    {move || format!("+{} other tasks", grouped(others.get() as u64))}
                                </button>
                            </Show>
                        </div>
                        <Show when=move || other.get()>
                            <div class="mp-facets">
                                <button
                                    type="button"
                                    class="facet"
                                    class:on=move || task_f.with(String::is_empty)
                                    on:click=move |_| {
                                        task_f.set(String::new());
                                        refocus();
                                    }
                                >
                                    "every task"
                                </button>
                                <For each=move || task_chips.get() key=|c| c.clone() let:c>
                                    {
                                        let id = c.0.clone();
                                        let on = {
                                            let id = id.clone();
                                            move || task_f.with(|s| *s == id)
                                        };
                                        view! {
                                            <button
                                                type="button"
                                                class="facet"
                                                class:on=on
                                                on:click=move |_| {
                                                    task_f.set(id.clone());
                                                    refocus();
                                                }
                                            >
                                                {if c.0 == "?" { "unknown".to_string() } else { c.0.clone() }}
                                                <span class="count">{grouped(c.1 as u64)}</span>
                                            </button>
                                        }
                                    }
                                </For>
                            </div>
                        </Show>
                        <div class="mp-count">
                            {move || {
                                format!(
                                    "{} shown of {}",
                                    grouped(matched.with(Vec::len) as u64),
                                    grouped(all.with(Vec::len) as u64),
                                )
                            }}
                            {move || {
                                if own_list {
                                    return match own_status() {
                                        ListStatus::Ready => None,
                                        ListStatus::Loading => Some(view! { <span>" · loading…"</span> }.into_any()),
                                        ListStatus::Failed(e) => {
                                            Some(view! { <span class="mp-err">" · could not load: " {e}</span> }.into_any())
                                        }
                                    };
                                }
                                if let Some(e) = catalog.error.get() {
                                    Some(
                                        view! {
                                            <span class="mp-err">
                                                " · could not refresh: " {e} " · "
                                                <button type="button" class="link-btn" on:click=move |_| catalog.refresh()>
                                                    "Retry"
                                                </button>
                                            </span>
                                        }
                                            .into_any(),
                                    )
                                } else if catalog.loading.get() {
                                    Some(view! { <span>" · refreshing…"</span> }.into_any())
                                } else {
                                    None
                                }
                            }}
                        </div>
                    </div>
                    <div class="pop-list mp-list" role="listbox" node_ref=list>
                        <For each=move || items.get() key=Item::render_key let:it>
                            {render_item(it, value, active, activate)}
                        </For>
                        <Show when=move || items.with(Vec::is_empty)>
                            <div class="pop-empty">
                                {move || {
                                    if all.with(Vec::is_empty) {
                                        match (settling(), own_status()) {
                                            (_, ListStatus::Failed(_)) => "The list did not load",
                                            (true, _) => "Loading…",
                                            _ => "No models to choose from",
                                        }
                                            .to_string()
                                    } else {
                                        format!("Nothing matches \u{201c}{}\u{201d}", query.get().trim())
                                    }
                                }}
                            </div>
                        </Show>
                    </div>
                </div>
            </Popover>
        </div>
    }
}

/// Is `id` in the list? (Tracked, like the memo it is called from.)
fn own_list_has(all: Signal<Vec<CatalogEntry>>, id: &str) -> bool {
    all.with(|a| a.iter().any(|e| e.id == id))
}

/// "128K", "1M", or the compact number for a window that is not a power of
/// two ("40K").
pub(crate) fn ctx_label(n: u64) -> String {
    const K: u64 = 1024;
    if n >= K * K && n.is_multiple_of(K * K) {
        format!("{}M", n / (K * K))
    } else if n >= K && n.is_multiple_of(K) {
        format!("{}K", n / K)
    } else {
        crate::fmt::compact(n as f64)
    }
}

fn render_item(
    it: Item,
    value: RwSignal<String>,
    active: RwSignal<Option<String>>,
    activate: impl Fn(Item) + Copy + Send + Sync + 'static,
) -> AnyView {
    let nav = it.nav_id();
    let is_active = {
        let nav = nav.clone();
        move || active.with(|a| a.as_deref() == Some(nav.as_str()))
    };
    let hover = {
        let nav = nav.clone();
        let ok = it.navigable();
        move |_| {
            if ok && active.with_untracked(|a| a.as_deref() != Some(nav.as_str())) {
                active.set(Some(nav.clone()));
            }
        }
    };
    let click = {
        let it = it.clone();
        move |_| activate(it.clone())
    };
    match it {
        Item::Head {
            label,
            count,
            open,
            note,
            ..
        } => view! {
            <div
                class="mp-head"
                class:active=is_active
                role="button"
                aria-expanded=open.to_string()
                title=note
                on:pointermove=hover
                on:click=click
            >
                <span class="caret-icon">"▸"</span>
                <span>{label}</span>
                <span class="count">{count}</span>
                {note.map(|n| view! { <span class="mp-note">{n}</span> })}
            </div>
        }
        .into_any(),
        Item::Row {
            entry, disabled, ..
        } => {
            let sel = {
                let id = entry.id.clone();
                move || value.with(|v| *v == id)
            };
            let (pfx, name) = entry.split_name();
            let (pfx, name) = (pfx.to_string(), name.to_string());
            let price_txt = match (entry.local, entry.price_in, entry.price_out) {
                (true, _, _) => None,
                (false, Some(i), Some(o)) if i == 0.0 && o == 0.0 => Some("free".to_string()),
                (false, Some(i), Some(o)) => Some(format!("${}/${}", price(i), price(o))),
                // A router: what it costs depends on where it sends the call.
                _ if entry.price_varies => Some("varies".to_string()),
                _ => None,
            };
            let task = entry.task.clone().filter(|t| t != "chat");
            view! {
                <div
                    class="mp-row"
                    class:sel=sel.clone()
                    class:active=is_active
                    class:off=disabled
                    role="option"
                    aria-selected=move || sel().to_string()
                    aria-disabled=disabled.then_some("true")
                    title=entry.id.clone()
                    on:pointermove=hover
                    on:click=click
                >
                    <i class="mp-dot" style=format!("--hue:{}", hue_for(&entry.group_label))></i>
                    <span class="mp-id">
                        <span class="mp-pfx">{pfx}</span>
                        {name}
                    </span>
                    <span class="mp-meta">
                        {task.map(|t| view! { <span class="mp-badge">{t}</span> })}
                        {(entry.vision == Some(true))
                            .then(|| view! { <span class="mp-badge" title="takes images">"vision"</span> })}
                        {entry.tools.then(|| view! { <span class="mp-badge" title="calls tools">"tools"</span> })}
                        {entry.reasoning.then(|| view! { <span class="mp-badge" title="can reason">"think"</span> })}
                        {entry.ctx.map(|c| view! { <span class="mp-ctx" title=format!("{} tokens of context", grouped(c))>{ctx_label(c)}</span> })}
                        {price_txt.map(|p| view! { <span class="mp-price" title="per million tokens in / out">{p}</span> })}
                    </span>
                </div>
            }
            .into_any()
        }
        Item::Custom(q) => view! {
            <div class="mp-row mp-custom" class:active=is_active role="option" on:pointermove=hover on:click=click>
                "Use \u{201c}" <span class="mp-id">{q}</span> "\u{201d} as typed"
            </div>
        }
        .into_any(),
        Item::Pinned(id) => view! {
            <div class="mp-row sel" class:active=is_active role="option" aria-selected="true" on:pointermove=hover>
                <span class="mp-id">{id}</span>
                <span class="mp-src warn">"not served"</span>
            </div>
        }
        .into_any(),
        Item::Empty(label) => {
            let sel = move || value.with(String::is_empty);
            view! {
                <div
                    class="mp-row"
                    class:sel=sel
                    class:active=is_active
                    role="option"
                    aria-selected=move || sel().to_string()
                    on:pointermove=hover
                    on:click=click
                >
                    <span class="mp-id dim">{label}</span>
                </div>
            }
            .into_any()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ctx_label;

    #[test]
    fn context_windows_read_the_way_they_are_quoted() {
        assert_eq!(ctx_label(131_072), "128K");
        assert_eq!(ctx_label(1_048_576), "1M");
        assert_eq!(ctx_label(40_000), "40K");
        assert_eq!(ctx_label(32_768), "32K");
    }
}
