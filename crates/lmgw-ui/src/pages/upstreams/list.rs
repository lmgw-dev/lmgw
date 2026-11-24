//! The list pane: the title with how many there are, Add, the search
//! (`?q=`, matched against the name, the protocol, the URL and the
//! passthrough prefix) and one row per upstream — its name, its state (a
//! dot and a word: ready, out of quota, unreachable, disabled) and a line of
//! facts (protocol, host, what its catalog holds).

use leptos::prelude::*;
use lmgw_api_types::UpstreamView;

use super::{protocol_label, Catalog, Page, Sel, LLAMA_CPP};
use crate::fmt::grouped;
use crate::pages::collection::{host_of, matches, CollectionPane, Item, Line};
use crate::widgets::list_pane::ListRefs;
use crate::widgets::{filter_words, Dot, Ico, Icon};

/// A row's state: the dot and its word.
pub(super) fn state_of(u: &UpstreamView, cat: Option<&Catalog>) -> (Dot, &'static str) {
    if !u.enabled {
        return (Dot::Off, "Disabled");
    }
    if u.quota.is_some() {
        return (Dot::Error, "Out of quota");
    }
    match cat {
        Some(Catalog::Failed(e)) if e.starts_with("unreachable") => (Dot::Error, "Unreachable"),
        Some(Catalog::Failed(_)) => (Dot::Error, "Catalog failed"),
        Some(Catalog::Loading) | None => (Dot::Busy, "Asking"),
        Some(Catalog::Ready(_)) => (Dot::Running, "Ready"),
    }
}

/// What its catalog holds, in a few words.
pub(super) fn catalog_words(u: &UpstreamView, cat: Option<&Catalog>) -> Option<String> {
    if !u.enabled {
        return None;
    }
    match cat {
        Some(Catalog::Ready(n)) if u.expose_all => Some(format!("{} models", grouped(*n as u64))),
        Some(Catalog::Ready(n)) => Some(format!("{} models · aliases only", grouped(*n as u64))),
        _ => None,
    }
}

fn line_of(u: &UpstreamView, cat: Option<&Catalog>) -> Item {
    let (dot, state) = state_of(u, cat);
    let mut meta = vec![protocol_label(&u.protocol).to_string()];
    let host = host_of(&u.base_url);
    if !host.is_empty() {
        meta.push(host);
    }
    meta.extend(catalog_words(u, cat));
    Item {
        key: Sel::Row(u.id).path(),
        name: u.name.clone(),
        meta: meta.join(" · "),
        dot,
        state: state.to_string(),
        off: !u.enabled,
        icon: if u.protocol == LLAMA_CPP {
            Ico::Chip
        } else {
            Ico::Cloud
        },
    }
}

#[component]
pub(super) fn UpList(
    page: Page,
    sel: Memo<Option<String>>,
    narrow: Memo<bool>,
    refs: ListRefs<String>,
    open_row: Callback<(String, bool)>,
) -> impl IntoView {
    let query = crate::url_state::use_query_signal("q");
    let lines = Memo::new(move |_| {
        let words = filter_words(&query.get());
        page.catalogs.with(|cats| {
            page.list.with(|l| {
                let rows: Vec<Line> = l
                    .iter()
                    .flatten()
                    .filter(|u| {
                        matches(
                            &format!(
                                "{} {} {} {} {}",
                                u.name,
                                u.protocol,
                                protocol_label(&u.protocol),
                                u.base_url,
                                u.expose_prefix
                            ),
                            &words,
                        )
                    })
                    .map(|u| Line::Row(line_of(u, cats.get(&u.id))))
                    .collect();
                rows
            })
        })
    });
    let sub = Signal::derive(move || {
        page.list.with(|l| match l {
            None if page.error.with(Option::is_some) => "not loaded".to_string(),
            None => "Loading…".to_string(),
            Some(l) => {
                let off = l.iter().filter(|u| !u.enabled).count();
                let quota = l.iter().filter(|u| u.quota.is_some()).count();
                let mut parts = vec![crate::fmt::count_of(l.len(), "upstreams")];
                if off > 0 {
                    parts.push(format!("{off} disabled"));
                }
                if quota > 0 {
                    parts.push(format!("{quota} out of quota"));
                }
                parts.join(" · ")
            }
        })
    });
    let add = move || {
        open_row.run((Sel::New.path(), false));
    };
    view! {
        <CollectionPane
            title="Upstreams"
            sub=sub
            label="Upstreams"
            placeholder="Search upstreams"
            query=query
            lines=lines
            loaded=Signal::derive(move || page.list.with(Option::is_some))
            error=page.error
            retry=Callback::new(move |()| page.retry())
            empty="No upstreams yet: add your first provider with +"
            sel=sel
            narrow=narrow
            refs=refs
            open_row=open_row
            actions=ViewFn::from(move || {
                view! {
                    <button
                        type="button"
                        class="icon-btn"
                        title="Add upstream"
                        aria-label="Add upstream"
                        on:click=move |_| add()
                    >
                        <Icon ico=Ico::Plus/>
                    </button>
                }
            })
        />
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_catalog_is_ready_and_green_in_list_and_hero() {
        let u = UpstreamView {
            enabled: true,
            ..Default::default()
        };
        let (dot, word) = state_of(&u, Some(&Catalog::Ready(0)));
        assert_eq!((dot, word), (Dot::Running, "Ready"));
        assert_eq!(dot.chip_class(), "chip ok");
    }
}
