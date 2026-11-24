//! The list pane of a page that is a collection of editable things on the
//! shared split view ([`crate::widgets::nav_split`]): Upstreams and MCP
//! servers. A header bar with the title, a line under it and the page's
//! actions, the search, and the rows under sticky group headings, each a
//! two-line [`ListRow`] whose key is the path that selects it.
//!
//! The header, the search, the keys and the rest are the shared list pane's
//! ([`crate::widgets::list_pane`]).

use leptos::prelude::*;

use crate::widgets::list_pane::{
    GroupHead, ListBox, ListEmpty, ListHead, ListNav, ListNotice, ListRefs, ListSearch,
};
use crate::widgets::{Dot, Ico, ListRow};

/// One line of the list.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Line {
    /// A group's sticky heading and how many rows it shows.
    Head(String, usize),
    Row(Item),
}

/// A row: the path that selects it, and what it shows.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Item {
    /// The path that selects it (`/upstreams/3`).
    pub key: String,
    pub name: String,
    pub meta: String,
    pub dot: Dot,
    pub state: String,
    /// Dimmed: switched off.
    pub off: bool,
    pub icon: Ico,
}

impl Line {
    fn key(&self) -> Option<&str> {
        match self {
            Self::Row(i) => Some(&i.key),
            Self::Head(..) => None,
        }
    }
}

/// The search's words, lowercased: a row is kept when its text holds all.
pub(crate) fn matches(hay: &str, words: &[String]) -> bool {
    let hay = hay.to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

#[component]
pub(crate) fn CollectionPane(
    title: &'static str,
    /// The line under the title: how many, how they are.
    #[prop(into)]
    sub: Signal<String>,
    /// The list's name for assistive technology.
    label: &'static str,
    /// The search's placeholder ("Search upstreams").
    placeholder: &'static str,
    query: RwSignal<String>,
    #[prop(into)] lines: Signal<Vec<Line>>,
    /// Loaded (rows or none), still loading, or why loading failed.
    #[prop(into)]
    loaded: Signal<bool>,
    #[prop(into)] error: Signal<Option<String>>,
    retry: Callback<()>,
    /// What an empty list says (no search).
    empty: &'static str,
    sel: Memo<Option<String>>,
    narrow: Memo<bool>,
    refs: ListRefs<String>,
    /// Open a row (a click, Enter, or the selection moving in a wide window).
    open_row: Callback<(String, bool)>,
    /// The header's actions (Add).
    actions: ViewFn,
) -> impl IntoView {
    let order = Memo::new(move |_| {
        lines.with(|ls| {
            ls.iter()
                .filter_map(|l| l.key().map(str::to_string))
                .collect::<Vec<_>>()
        })
    });
    let nav = ListNav::new(refs, order, sel, narrow, open_row, String::clone);
    let render = move |l: Line| -> AnyView {
        match l {
            Line::Head(label, n) => view! { <GroupHead label=label n=n/> }.into_any(),
            Line::Row(i) => {
                let key = StoredValue::new(i.key.clone());
                let tip = if i.meta.is_empty() {
                    i.name.clone()
                } else {
                    format!("{}\n{}", i.name, i.meta)
                };
                view! {
                    <ListRow
                        key=i.key.clone()
                        name=i.name
                        meta=i.meta
                        title=tip
                        dot=Signal::stored(i.dot)
                        state=i.state
                        icon=i.icon
                        selected=Signal::derive(move || key.with_value(|k| nav.lit(k)))
                        off=Signal::stored(i.off)
                        on_click=Callback::new(move |()| nav.click(key.get_value()))
                    />
                }
                .into_any()
            }
        }
    };

    view! {
        <ListHead scrolled=nav.scrolled narrow=narrow title=title sub=sub actions=actions/>
        <ListSearch nav=nav query=query placeholder=placeholder/>
        <ListNotice error=error stale=loaded failed="Loading failed: " retry=retry/>
        <ListBox nav=nav label=label>
            <ListEmpty
                loaded=loaded
                error=error
                empty=Signal::derive(move || order.with(Vec::is_empty))
                query=query
                none=empty
                no_match="Nothing matches"
            />
            <For each=move || lines.get() key=|l| format!("{l:?}") let:l>
                {render(l)}
            </For>
        </ListBox>
    }
}

/// The host (and port) of a URL, for a row's line: `https://openrouter.ai/api/v1`
/// → `openrouter.ai`.
pub(crate) fn host_of(url: &str) -> String {
    let rest = url
        .split_once("://")
        .map_or(url, |(_, r)| r)
        .trim_end_matches('/');
    rest.split('/').next().unwrap_or(rest).to_string()
}

/// Keep the browser's own Ctrl+F away while a narrow window shows a
/// detail: its list and search are hidden then.
pub(crate) fn search_off(narrow: Memo<bool>, sel: Memo<Option<String>>) -> Signal<bool> {
    Signal::derive(move || narrow.get() && sel.with(Option::is_some))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_names_its_host() {
        assert_eq!(host_of("https://openrouter.ai/api/v1"), "openrouter.ai");
        assert_eq!(host_of("http://127.0.0.1:9393/"), "127.0.0.1:9393");
        assert_eq!(host_of("api.example/v1"), "api.example");
        assert_eq!(host_of(""), "");
    }

    #[test]
    fn a_search_keeps_a_row_holding_every_word() {
        let w = vec!["open".to_string(), "router".to_string()];
        assert!(matches("OpenRouter · openai_compatible", &w));
        assert!(!matches("Anthropic", &w));
        assert!(matches("anything", &[]));
    }
}
