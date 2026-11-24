//! The list pane (modern-sample.html): a header with the title, how many
//! keys it shows and the add menu, the Usage views as a switch under it,
//! the search, and a row per key under its kind's sticky heading (owner
//! keys, client keys, devices, agent tokens, internal identities). Each row
//! is a two-line [`ListRow`]: the kind's icon, the name, its state (a dot
//! and a word) and a line of facts (spend, scope, last use).
//! The header, search and keys are the shared list pane's
//! ([`crate::widgets::list_pane`]).

use leptos::prelude::*;
use lmgw_api_types::KeyRow;

use super::super::key_line::{
    bare_name, kind_heading, kind_icon, over_budget, scope_text, spend_text, used_words, KINDS,
};
use super::KeysCtx;
use crate::widgets::list_pane::{
    GroupHead, ListBox, ListEmpty, ListHead, ListNav, ListNotice, ListRefs, ListSearch,
};
use crate::widgets::{Dot, Ico, ListRow, MenuItem, RowMenu};

/// Does this key match what was typed in the search box? Its name, kind,
/// note, scope patterns, hosted label and who made it.
pub fn matches_query(r: &KeyRow, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let hay = [
        r.name.as_str(),
        r.kind.as_str(),
        r.note.as_str(),
        r.scope_patterns.as_str(),
        r.tool_scope_patterns.as_str(),
        r.hosts_label.as_deref().unwrap_or_default(),
        r.made_by.as_deref().unwrap_or_default(),
        r.app_id.as_deref().unwrap_or_default(),
    ];
    hay.iter().any(|h| h.to_lowercase().contains(&q))
}

/// A row's state as the list says it, most pressing first: switched off,
/// paused by its account, expired, over its budget, a device's connection,
/// else active. `expired` is whether its expiry has passed.
pub fn state_of(r: &KeyRow, expired: bool) -> (Dot, String) {
    if !r.enabled {
        return (Dot::Off, "Disabled".into());
    }
    if r.paused.is_some() {
        return (Dot::Off, "Paused".into());
    }
    if expired {
        return (Dot::Error, "Expired".into());
    }
    if over_budget(r) {
        return (Dot::Error, "Over budget".into());
    }
    if r.kind == "device" {
        return if r.online.is_empty() {
            (Dot::Stopped, "Offline".into())
        } else {
            (Dot::Running, "Online".into())
        };
    }
    (Dot::Stopped, "Active".into())
}

/// The row's second line: what it reaches, its spend against its budget
/// (left out while it has neither), when it was last used.
pub fn meta_of(r: &KeyRow, cur: &str) -> String {
    let mut parts = vec![scope_text(r)];
    if r.budget_micro > 0 || r.spent_micro > 0 {
        parts.push(spend_text(r, cur));
    }
    parts.push(used_words(r.last_used.as_deref()));
    parts.join(" · ")
}

/// What a list line is.
#[derive(Clone, PartialEq)]
enum Entry {
    /// A kind's sticky heading and how many rows it shows.
    Head(&'static str, usize),
    /// A key by its id and kind: what it shows it reads from the list as it
    /// stands, so spend landing updates the row in place.
    Row(i64, String),
}

fn entry_key(e: &Entry) -> String {
    match e {
        Entry::Head(h, n) => format!("h:{h}:{n}"),
        Entry::Row(id, kind) => format!("r:{id}:{kind}"),
    }
}

/// The lines under the search, and how many keys they show.
fn entries_of(keys: &[KeyRow], query: &str) -> (Vec<Entry>, usize) {
    let mut out = Vec::new();
    let mut shown = 0;
    let mut kinds: Vec<&str> = KINDS.iter().map(|(k, _, _)| *k).collect();
    for r in keys {
        if !kinds.contains(&r.kind.as_str()) {
            kinds.push(r.kind.as_str());
        }
    }
    for kind in kinds {
        let mine: Vec<&KeyRow> = keys
            .iter()
            .filter(|r| r.kind == kind && matches_query(r, query))
            .collect();
        if mine.is_empty() {
            continue;
        }
        shown += mine.len();
        out.push(Entry::Head(kind_heading(kind), mine.len()));
        out.extend(mine.into_iter().map(|r| Entry::Row(r.id, r.kind.clone())));
    }
    (out, shown)
}

/// The ways to add, behind the header's +: a key (on the status page, where
/// its form is) and a device.
fn add_items(page: KeysCtx) -> Signal<Vec<MenuItem>> {
    Signal::derive(move || {
        vec![
            MenuItem::new("New key…", move || page.new_key())
                .title("A client key or an owner key: its form is on the page with nothing selected"),
            MenuItem::new("Pair a device…", move || page.devices.pair_open.set(true))
                .title("A client app (a desktop assistant, a phone) pairs with a link that carries a key of its own"),
        ]
    })
}

#[component]
pub(super) fn ListPane(
    page: KeysCtx,
    sel: Memo<Option<i64>>,
    narrow: Memo<bool>,
    refs: ListRefs<i64>,
    /// Open a row (a click, Enter, or the selection moving in a wide window).
    open_row: Callback<(i64, bool)>,
) -> impl IntoView {
    let query = crate::url_state::use_query_signal("q");
    let keys = page.keys;

    let shown = Memo::new(move |_| {
        let q = query.get();
        keys.data.with(|k| {
            entries_of(
                k.as_ref().map(|k| k.keys.as_slice()).unwrap_or_default(),
                &q,
            )
        })
    });
    let order = Memo::new(move |_| {
        shown.with(|(es, _)| {
            es.iter()
                .filter_map(|e| match e {
                    Entry::Row(id, _) => Some(*id),
                    Entry::Head(..) => None,
                })
                .collect::<Vec<_>>()
        })
    });
    let cur = Memo::new(move |_| {
        keys.data
            .with(|d| d.as_ref().map(|k| k.currency.clone()).unwrap_or_default())
    });
    let count_line = move || {
        let total = keys.data.with(|k| k.as_ref().map(|k| k.keys.len()));
        match total {
            None if keys.err.with(Option::is_some) => "not loaded".to_string(),
            None => "Loading…".to_string(),
            Some(total) => {
                let all = crate::fmt::count_of(total, "keys");
                if query.with(|q| q.trim().is_empty()) {
                    all
                } else {
                    format!("{} of {all}", shown.with(|(_, n)| *n))
                }
            }
        }
    };

    let nav = ListNav::new(refs, order, sel, narrow, open_row, |id: &i64| {
        format!("/usage/keys/{id}")
    });

    let render = move |e: Entry| -> AnyView {
        match e {
            Entry::Head(label, n) => view! { <GroupHead label=label n=n/> }.into_any(),
            Entry::Row(id, kind) => {
                let row = Memo::new(move |_| {
                    keys.data.with(|d| {
                        d.as_ref()
                            .and_then(|k| k.keys.iter().find(|r| r.id == id).cloned())
                    })
                });
                let name = Memo::new(move |_| {
                    row.with(|r| {
                        r.as_ref()
                            .map(|r| bare_name(&r.kind, &r.name).to_string())
                            .unwrap_or_default()
                    })
                });
                let st = Memo::new(move |_| {
                    row.with(|r| {
                        r.as_ref().map_or((Dot::Off, String::new()), |r| {
                            let expired = r
                                .expires_at
                                .as_deref()
                                .is_some_and(super::super::key_line::expiry_past);
                            state_of(r, expired)
                        })
                    })
                });
                let meta = Memo::new(move |_| {
                    row.with(|r| {
                        r.as_ref()
                            .map(|r| meta_of(r, &cur.get()))
                            .unwrap_or_default()
                    })
                });
                let tip = move || {
                    row.with(|r| {
                        r.as_ref()
                            .map(|r| {
                                let mut lines = vec![r.name.clone(), meta.get()];
                                if !r.note.is_empty() {
                                    lines.push(r.note.clone());
                                }
                                lines.join("\n")
                            })
                            .unwrap_or_default()
                    })
                };
                view! {
                    <ListRow
                        key=format!("/usage/keys/{id}")
                        icon=kind_icon(&kind)
                        name=move || name.get()
                        meta=move || meta.get()
                        title=tip
                        dot=Signal::derive(move || st.get().0)
                        state=move || st.get().1
                        selected=Signal::derive(move || nav.lit(&id))
                        off=Signal::derive(move || row.with(|r| r.as_ref().is_some_and(|r| !r.enabled)))
                        on_click=Callback::new(move |()| nav.click(id))
                    />
                }
                .into_any()
            }
        }
    };

    let add = ViewFn::from(move || {
        view! { <RowMenu items=add_items(page) icon=Ico::Plus title="Add a key or pair a device"/> }
    });
    let loaded = Signal::derive(move || keys.data.with(Option::is_some));
    view! {
        <ListHead
            scrolled=nav.scrolled
            narrow=narrow
            title="Keys & budgets"
            sub=Signal::derive(count_line)
            actions=add
        />
        <div class="keys-views">
            <super::super::UsageTabs keys=keys prices=page.prices compact=true/>
        </div>
        <ListSearch
            nav=nav
            query=query
            placeholder="Search keys"
            title="Name, kind, note, scope, hosted label or who made it"
        />
        <ListNotice
            error=keys.err
            stale=loaded
            failed="Loading keys failed: "
            retry=Callback::new(move |()| page.reload())
        />
        <ListBox nav=nav label="Keys">
            <ListEmpty
                loaded=loaded
                error=keys.err
                empty=Signal::derive(move || shown.with(|(es, _)| es.is_empty()))
                query=query
                none="No keys yet: add one with +. Internal consumers (admin chat, quickdoc ingest, …) appear here as identities once they spend."
                no_match="No keys match"
            />
            <For each=move || shown.with(|(es, _)| es.clone()) key=entry_key let:e>
                {render(e)}
            </For>
        </ListBox>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, kind: &str, name: &str) -> KeyRow {
        KeyRow {
            id,
            kind: kind.into(),
            name: name.into(),
            enabled: true,
            scope_mode: "all".into(),
            tool_scope_mode: "all".into(),
            ..Default::default()
        }
    }

    #[test]
    fn an_expired_key_is_red_in_its_list_and_its_hero() {
        let (dot, word) = state_of(&row(1, "key", "ci"), true);
        assert_eq!((dot, word.as_str()), (Dot::Error, "Expired"));
        assert_eq!(dot.chip_class(), "chip err");
        assert_eq!(state_of(&row(2, "key", "ci"), false).0.chip_class(), "chip");
    }

    #[test]
    fn the_state_says_the_most_pressing_thing() {
        let mut r = row(1, "device", "device:phone");
        assert_eq!(state_of(&r, false), (Dot::Stopped, "Offline".to_string()));
        r.online = vec!["feed".into()];
        assert_eq!(state_of(&r, false), (Dot::Running, "Online".to_string()));
        r.budget_micro = 1;
        r.spent_micro = 2;
        assert_eq!(state_of(&r, false).1, "Over budget");
        assert_eq!(state_of(&r, true).1, "Expired");
        r.paused = Some("account disabled".into());
        assert_eq!(state_of(&r, true).1, "Paused");
        r.enabled = false;
        assert_eq!(state_of(&r, true), (Dot::Off, "Disabled".to_string()));
        assert_eq!(state_of(&row(2, "key", "ci"), false).1, "Active");
    }

    #[test]
    fn rows_group_by_kind_in_the_lists_order_and_the_search_counts() {
        let mut ci = row(3, "key", "ci");
        ci.note = "the build runner".into();
        let keys = vec![
            row(1, "internal", "internal:admin_chat"),
            ci,
            row(2, "owner", "owner:dashboard"),
            row(4, "device", "device:phone"),
            row(5, "future", "future:x"),
        ];
        let (es, n) = entries_of(&keys, "");
        assert_eq!(n, 5);
        let heads: Vec<&str> = es
            .iter()
            .filter_map(|e| match e {
                Entry::Head(h, _) => Some(*h),
                Entry::Row(..) => None,
            })
            .collect();
        assert_eq!(
            heads,
            [
                "Owner keys",
                "Client keys",
                "Devices",
                "Internal identities",
                "Other keys"
            ]
        );
        assert_eq!(entries_of(&keys, "runner").1, 1);
        assert_eq!(entries_of(&keys, "DEVICE").1, 1);
    }

    /// A row is keyed by its key, never by what its figures say: spend that
    /// lands is read by the row, which stays.
    #[test]
    fn a_row_is_keyed_by_its_id_and_kind() {
        assert_eq!(
            entry_key(&Entry::Row(7, "key".into())),
            entry_key(&Entry::Row(7, "key".into()))
        );
        assert_ne!(
            entry_key(&Entry::Row(7, "key".into())),
            entry_key(&Entry::Row(7, "owner".into()))
        );
    }
}
