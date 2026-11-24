//! The list pane (modern-sample.html): a header with the title, how many
//! agents it shows under it and the add menu, a search entry, and a row per
//! agent under its run kind's sticky heading. Each row is a two-line
//! [`ListRow`]: the kind's icon, the name, the state (a dot and a word) and
//! a line of facts.
//! The header, search and keys are the shared list pane's
//! ([`crate::widgets::list_pane`]).

use leptos::prelude::*;
use lmgw_api_types::AgentCard;

use super::{activity_line, card_warnings, matches_query, requires_warning, AgentsCtx};
use crate::widgets::list_pane::{
    GroupHead, ListBox, ListEmpty, ListHead, ListNav, ListNotice, ListRefs, ListSearch,
};
use crate::widgets::{Dot, Ico, ListRow, MenuItem, RowMenu};

/// The run kinds in the list's order: (kind, heading, icon).
pub(crate) const KINDS: &[(&str, &str, Ico)] = &[
    ("chat", "Chat agents", Ico::Chat),
    ("batch", "Batch agents", Ico::List),
    ("container", "Container agents", Ico::Box),
];

/// A kind's icon; a row whose manifest could not be read has none of the
/// three.
pub(crate) fn kind_icon(kind: &str) -> Ico {
    KINDS
        .iter()
        .find(|(k, _, _)| *k == kind)
        .map_or(Ico::Info, |(_, _, i)| *i)
}

/// The heading a kind's rows sit under.
pub(super) fn kind_heading(kind: &str) -> &'static str {
    KINDS
        .iter()
        .find(|(k, _, _)| *k == kind)
        .map_or("Not readable", |(_, h, _)| *h)
}

/// A row's state as the list says it, most pressing first: a manifest this
/// build cannot read, switched off, a run in flight, tools that do not
/// resolve, a warning that blocks a start, other warnings, else ready.
/// `running` is whether a run of it is live on the jobs feed.
pub fn state_of(c: &AgentCard, running: bool) -> (Dot, String) {
    if c.error.is_some() {
        return (Dot::Error, "Not readable".into());
    }
    if !c.enabled {
        return (Dot::Off, "Disabled".into());
    }
    if running {
        return (Dot::Busy, "Running".into());
    }
    if !c.requires_ok {
        return (Dot::Error, "Tools missing".into());
    }
    if c.warnings.iter().any(|w| w.blocks_start) {
        return (Dot::Error, "Blocked".into());
    }
    match c.warnings.len() {
        0 => (Dot::Stopped, "Ready".into()),
        1 => (Dot::Stopped, "1 warning".into()),
        n => (Dot::Stopped, format!("{n} warnings")),
    }
}

/// The row's second line: the model it asks for, the tool labels, whether
/// it serves an app, what it has done, and that it ships with lmgw.
pub fn meta_of(c: &AgentCard) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(e) = &c.error {
        return e.clone();
    }
    parts.push(if c.effective_model.is_empty() {
        "no model".to_string()
    } else {
        c.effective_model.clone()
    });
    if !c.labels.is_empty() {
        parts.push(c.labels.join(", "));
    }
    if c.app {
        parts.push("app".to_string());
    }
    parts.push(activity_line(c));
    if c.source == "builtin" {
        parts.push("shipped".to_string());
    }
    parts.join(" · ")
}

/// What a list line is.
#[derive(Clone, PartialEq)]
enum Entry {
    /// A kind's sticky heading and how many rows it shows.
    Head(&'static str, usize),
    Row(Box<AgentCard>),
}

/// What the list keys a line by. A row is its agent and the heading it sits
/// under: what it shows of its card it reads from the list itself, so a
/// changed card updates the row in place rather than replacing it.
fn entry_key(e: &Entry) -> String {
    match e {
        Entry::Head(h, n) => format!("h:{h}:{n}"),
        Entry::Row(c) => format!("r:{}:{}", c.id, c.kind),
    }
}

/// The lines under the search, and how many agents they show.
fn entries_of(cards: &[AgentCard], query: &str) -> (Vec<Entry>, usize) {
    let mut out = Vec::new();
    let mut shown = 0;
    let mut kinds: Vec<&str> = KINDS.iter().map(|(k, _, _)| *k).collect();
    // Whatever else a row says it is (an unreadable one says nothing) goes
    // last, under its own heading.
    for c in cards {
        if !kinds.contains(&c.kind.as_str()) {
            kinds.push(c.kind.as_str());
        }
    }
    for kind in kinds {
        let mine: Vec<&AgentCard> = cards
            .iter()
            .filter(|c| c.kind == kind && matches_query(c, query))
            .collect();
        if mine.is_empty() {
            continue;
        }
        shown += mine.len();
        out.push(Entry::Head(kind_heading(kind), mine.len()));
        out.extend(mine.into_iter().cloned().map(|c| Entry::Row(Box::new(c))));
    }
    (out, shown)
}

/// The ways to add an agent, behind the header's +.
pub(super) fn add_items(page: AgentsCtx) -> Signal<Vec<MenuItem>> {
    Signal::derive(move || {
        vec![
            MenuItem::new("New agent…", move || page.new_open.set(true)).title(
                "Write the smallest valid manifest, a chat agent with a model picker, and open it",
            ),
            MenuItem::new("Import a manifest…", move || page.import_open.set(true))
                .title("An .agent.json someone exported, as a file or pasted"),
            MenuItem::new("Install from an image…", move || {
                page.install_open.set(true)
            })
            .title("An agent package: an OCI image carrying its manifest at /lmgw/agent.json"),
            MenuItem::new("Restore shipped agents", move || page.restore.run(()))
                .disabled(page.restoring.get())
                .title("Put the agents lmgw ships with back, the ones deleted included"),
        ]
    })
}

#[component]
pub(super) fn ListPane(
    page: AgentsCtx,
    sel: Memo<Option<String>>,
    narrow: Memo<bool>,
    refs: ListRefs<String>,
    /// Open a row (a click, Enter, or the selection moving in a wide window).
    open_row: Callback<(String, bool)>,
) -> impl IntoView {
    let live = crate::live::use_live();
    let query = crate::url_state::use_query_signal("q");
    let cards = page.cards;

    let shown = Memo::new(move |_| {
        let q = query.get();
        cards.with(|c| entries_of(c.as_deref().unwrap_or_default(), &q))
    });
    let order = Memo::new(move |_| {
        shown.with(|(es, _)| {
            es.iter()
                .filter_map(|e| match e {
                    Entry::Row(c) => Some(c.id.clone()),
                    Entry::Head(..) => None,
                })
                .collect::<Vec<_>>()
        })
    });
    // Which agents have a run in flight: the jobs frame, keyed `agent:<id>`.
    let running = Memo::new(move |_| {
        live.jobs.with(|j| {
            j.as_deref()
                .unwrap_or_default()
                .iter()
                .filter(|j| {
                    j.kind == "agent_run" && matches!(j.status.as_str(), "queued" | "running")
                })
                .filter_map(|j| j.key.as_deref()?.strip_prefix("agent:").map(str::to_string))
                .collect::<Vec<_>>()
        })
    });
    let count_line = move || {
        let total = cards.with(|c| c.as_ref().map(Vec::len));
        match total {
            None if page.error.with(Option::is_some) => "not loaded".to_string(),
            None => "Loading…".to_string(),
            Some(total) => {
                let all = crate::fmt::count_of(total, "agents");
                if query.with(|q| q.trim().is_empty()) {
                    all
                } else {
                    // What the search holds back is said: "1 of 3 agents".
                    format!("{} of {all}", shown.with(|(_, n)| *n))
                }
            }
        }
    };

    let nav = ListNav::new(refs, order, sel, narrow, open_row, |id: &String| {
        format!("/agents/{id}")
    });

    let render = move |e: Entry| -> AnyView {
        match e {
            Entry::Head(label, n) => view! { <GroupHead label=label n=n/> }.into_any(),
            Entry::Row(c) => {
                let id = StoredValue::new(c.id.clone());
                let icon = kind_icon(&c.kind);
                let first = StoredValue::new(*c);
                // The row reads its card from the list as it stands, so a
                // save or a run's end that changes it (the model, a
                // warning, the last run) shows without the row being
                // rebuilt; the card it was made from covers the moment it
                // leaves the list.
                let c = Memo::new(move |_| {
                    cards
                        .with(|cs| {
                            id.with_value(|id| {
                                cs.as_deref()
                                    .and_then(|cs| cs.iter().find(|c| c.id == *id).cloned())
                            })
                        })
                        .unwrap_or_else(|| first.get_value())
                });
                let st = Memo::new(move |_| {
                    let live = running.with(|r| id.with_value(|id| r.contains(id)));
                    c.with(|c| state_of(c, live))
                });
                let name = Memo::new(move |_| {
                    c.with(|c| {
                        if c.name.is_empty() {
                            c.id.clone()
                        } else {
                            c.name.clone()
                        }
                    })
                });
                let meta = Memo::new(move |_| c.with(meta_of));
                // The row's lines in full, and what keeps it from running.
                let tip = move || {
                    c.with(|c| {
                        let mut lines = vec![name.get(), meta.get()];
                        lines.extend(requires_warning(c));
                        lines.extend(card_warnings(c));
                        lines.join("\n")
                    })
                };
                view! {
                    <ListRow
                        key=format!("/agents/{}", id.get_value())
                        icon=icon
                        name=move || name.get()
                        meta=move || meta.get()
                        title=tip
                        dot=Signal::derive(move || st.get().0)
                        state=move || st.get().1
                        selected=Signal::derive(move || id.with_value(|id| nav.lit(id)))
                        off=Signal::derive(move || c.with(|c| !c.enabled))
                        on_click=Callback::new(move |()| nav.click(id.get_value()))
                    />
                }
                .into_any()
            }
        }
    };

    let add = ViewFn::from(move || {
        view! { <RowMenu items=add_items(page) icon=Ico::Plus title="Add an agent"/> }
    });
    view! {
        <ListHead
            scrolled=nav.scrolled
            narrow=narrow
            title="Agents"
            sub=Signal::derive(count_line)
            actions=add
        />
        <ListSearch
            nav=nav
            query=query
            placeholder="Search agents"
            title="Name, id, description, model, kind or tool label"
        />
        <ListNotice
            error=page.error
            stale=Signal::derive(move || cards.with(Option::is_some))
            failed="Loading agents failed: "
            retry=page.reload
        />
        <ListBox nav=nav label="Agents">
            <ListEmpty
                loaded=Signal::derive(move || cards.with(Option::is_some))
                error=page.error
                empty=Signal::derive(move || shown.with(|(es, _)| es.is_empty()))
                query=query
                none="No agents yet: add one with +"
                no_match="No agents match"
            />
            <For
                each=move || shown.with(|(es, _)| es.clone())
                key=entry_key
                let:e
            >
                {render(e)}
            </For>
        </ListBox>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::AgentWarning;

    fn card() -> AgentCard {
        AgentCard {
            id: "mail-labeler".into(),
            name: "Mail labeler".into(),
            kind: "batch".into(),
            effective_model: "gemma4-e4b".into(),
            labels: vec!["gws".into()],
            enabled: true,
            requires_ok: true,
            ..Default::default()
        }
    }

    /// A run in flight is the accent in the list; the hero says the same.
    #[test]
    fn a_run_in_flight_is_busy() {
        assert_eq!(state_of(&card(), true).0.chip_class(), "chip info");
    }

    /// The most pressing thing about a row is what its state says.
    #[test]
    fn the_state_says_the_most_pressing_thing() {
        let mut c = card();
        assert_eq!(state_of(&c, false), (Dot::Stopped, "Ready".to_string()));
        assert_eq!(state_of(&c, true), (Dot::Busy, "Running".to_string()));
        c.warnings = vec![AgentWarning {
            code: "image_missing".into(),
            ..Default::default()
        }];
        assert_eq!(state_of(&c, false).1, "1 warning");
        c.warnings[0].blocks_start = true;
        assert_eq!(state_of(&c, false), (Dot::Error, "Blocked".to_string()));
        c.requires_ok = false;
        assert_eq!(state_of(&c, false).1, "Tools missing");
        c.enabled = false;
        assert_eq!(state_of(&c, true), (Dot::Off, "Disabled".to_string()));
        c.error = Some("schema_version 9".into());
        assert_eq!(state_of(&c, false).1, "Not readable");
    }

    #[test]
    fn the_meta_line_names_the_model_tools_and_activity() {
        let mut c = card();
        assert_eq!(meta_of(&c), "gemma4-e4b · gws · never run");
        c.source = "builtin".into();
        c.app = true;
        c.effective_model.clear();
        assert_eq!(meta_of(&c), "no model · gws · app · never run · shipped");
    }

    /// A row's key is its agent and its kind's heading, never what its card
    /// says: a card that changes is read by the row, which stays.
    #[test]
    fn a_row_is_keyed_by_its_agent_not_by_its_card() {
        let before = card();
        let mut after = card();
        after.effective_model = "qwen3".into();
        after.warnings = vec![AgentWarning::default()];
        after.last_run = Some(Default::default());
        let key = |c: &AgentCard| entry_key(&Entry::Row(Box::new(c.clone())));
        assert_eq!(key(&before), key(&after));
        after.kind = "container".into();
        assert_ne!(key(&before), key(&after), "another heading is another line");
    }

    /// Rows group under their kind in the list's order, and the search
    /// counts what it shows.
    #[test]
    fn rows_group_by_kind_and_the_search_counts() {
        let mut chat = card();
        chat.id = "folder-chat".into();
        chat.name = "Folder chat".into();
        chat.kind = "chat".into();
        let mut odd = card();
        odd.id = "future".into();
        odd.kind = String::new();
        let cards = vec![card(), odd, chat];
        let (es, n) = entries_of(&cards, "");
        assert_eq!(n, 3);
        let heads: Vec<&str> = es
            .iter()
            .filter_map(|e| match e {
                Entry::Head(h, _) => Some(*h),
                Entry::Row(_) => None,
            })
            .collect();
        assert_eq!(heads, ["Chat agents", "Batch agents", "Not readable"]);
        let (_, n) = entries_of(&cards, "folder");
        assert_eq!(n, 1);
    }
}
