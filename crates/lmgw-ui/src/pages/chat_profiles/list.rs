//! The list pane: the title with how many profiles there are and which one
//! new threads start with, the back button to the Chat at the start of the
//! header, Add (a new profile, the built-in Concise), the search (`?q=`,
//! matched against the name and the persona) and one row per profile: its
//! name, "Default" on the one new threads start with, and a line of who uses
//! it. ↑ ↓ Home End move, Enter opens (the shared list pane's keys).

use leptos::prelude::*;
use leptos_router::components::A;
use lmgw_api_types::chat_profiles::{Profile, BUILTIN_CONCISE};

use super::{Page, Sel};
use crate::pages::collection::matches;
use crate::widgets::list_pane::{
    ListBox, ListEmpty, ListHead, ListNav, ListNotice, ListRefs, ListSearch,
};
use crate::widgets::{filter_words, Ico, Icon, ListRow, MenuItem, RowMenu};

/// A row: the path that selects it, and what it shows.
#[derive(Clone, Debug, PartialEq)]
struct Item {
    key: String,
    name: String,
    meta: String,
    default: bool,
}

/// Who uses a profile, in a few words: "Built-in · used by 3 threads · 1
/// folder" (the built-in part for built-ins only), or "Not used".
pub(super) fn meta_of(p: &Profile) -> String {
    let (n, f) = (p.used_by.threads, p.used_by.folders.len());
    let mut parts = Vec::new();
    if p.builtin.is_some() {
        parts.push("Built-in".to_string());
    }
    if n == 0 && f == 0 {
        parts.push(
            if parts.is_empty() {
                "Not used"
            } else {
                "not used"
            }
            .to_string(),
        );
    } else {
        let mut used = Vec::new();
        if n > 0 {
            used.push(crate::fmt::count_of(n as usize, "threads"));
        }
        if f > 0 {
            used.push(crate::fmt::count_of(f, "folders"));
        }
        let used = format!("used by {}", used.join(" · "));
        parts.push(if parts.is_empty() {
            capitalised(&used)
        } else {
            used
        });
    }
    parts.join(" · ")
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

fn item_of(p: &Profile, default: Option<i64>) -> Item {
    Item {
        key: Sel::Row(p.id).path(),
        name: p.name.clone(),
        meta: meta_of(p),
        default: default == Some(p.id),
    }
}

#[component]
pub(super) fn ProfilesList(
    page: Page,
    sel: Memo<Option<Sel>>,
    key: Memo<Option<String>>,
    narrow: Memo<bool>,
    refs: ListRefs<String>,
    open_row: Callback<(String, bool)>,
) -> impl IntoView {
    let query = crate::url_state::use_query_signal("q");
    let items = Memo::new(move |_| {
        let words = filter_words(&query.get());
        page.list.with(|l| {
            let Some(l) = l else { return Vec::new() };
            l.profiles
                .iter()
                .filter(|p| matches(&format!("{} {}", p.name, p.persona), &words))
                .map(|p| item_of(p, l.default_profile_id))
                .collect::<Vec<_>>()
        })
    });
    let order =
        Memo::new(move |_| items.with(|v| v.iter().map(|i| i.key.clone()).collect::<Vec<_>>()));
    let nav = ListNav::new(refs, order, key, narrow, open_row, String::clone);
    let sub = Signal::derive(move || {
        page.list.with(|l| match l {
            None if page.error.with(Option::is_some) => "not loaded".to_string(),
            None => "Loading…".to_string(),
            Some(l) => {
                let default = l
                    .default_profile_id
                    .and_then(|id| l.profiles.iter().find(|p| p.id == id))
                    .map(|p| format!("default: {}", p.name))
                    .unwrap_or_else(|| "no default".to_string());
                format!(
                    "{} · {default}",
                    crate::fmt::count_of(l.profiles.len(), "profiles")
                )
            }
        })
    });
    let has_builtin = move || {
        page.list.with(|l| {
            l.as_ref().is_some_and(|l| {
                l.profiles
                    .iter()
                    .any(|p| p.builtin.as_deref() == Some(BUILTIN_CONCISE))
            })
        })
    };
    let add_items = Signal::derive(move || {
        let from = sel.get_untracked();
        let builtin = MenuItem::new("Add the built-in Concise", move || page.add_builtin(true));
        vec![
            MenuItem::new("New profile", move || page.open_new(from)),
            if has_builtin() {
                builtin.disabled(true).title("Concise is in the list")
            } else {
                builtin
            },
        ]
    });
    let back_href = page.back_href();
    let loaded = Signal::derive(move || page.list.with(Option::is_some));
    view! {
        <ListHead
            scrolled=nav.scrolled
            narrow=narrow
            title="Chat profiles"
            sub=sub
            lead=ViewFn::from(move || {
                view! {
                    <A href=back_href.clone() attr:class="icon-btn" attr:title="Back to chat" attr:aria-label="Back to chat">
                        <Icon ico=Ico::Back/>
                    </A>
                }
            })
            actions=ViewFn::from(move || view! { <RowMenu items=add_items icon=Ico::Plus title="Add"/> })
        />
        <ListSearch nav=nav query=query placeholder="Search profiles"/>
        <ListNotice
            error=page.error
            stale=loaded
            failed="Loading profiles failed: "
            retry=Callback::new(move |()| page.load())
        />
        <ListBox nav=nav label="Chat profiles">
            <ListEmpty
                loaded=loaded
                error=page.error
                empty=Signal::derive(move || items.with(Vec::is_empty))
                query=query
                none="No profiles yet: threads use none, the default behaviour. Add one with +"
                no_match="Nothing matches"
            />
            <For each=move || items.get() key=|i| format!("{i:?}") let:i>
                {
                    let k = StoredValue::new(i.key.clone());
                    let tip = format!("{}\n{}", i.name, i.meta);
                    // `state` is a prop that is there or not: the default row
                    // says so, the others have no state word.
                    if i.default {
                        view! {
                            <ListRow
                                key=i.key.clone()
                                name=i.name
                                meta=i.meta
                                title=tip
                                state="Default"
                                selected=Signal::derive(move || k.with_value(|k| nav.lit(k)))
                                on_click=Callback::new(move |()| nav.click(k.get_value()))
                            />
                        }
                            .into_any()
                    } else {
                        view! {
                            <ListRow
                                key=i.key.clone()
                                name=i.name
                                meta=i.meta
                                title=tip
                                selected=Signal::derive(move || k.with_value(|k| nav.lit(k)))
                                on_click=Callback::new(move |()| nav.click(k.get_value()))
                            />
                        }
                            .into_any()
                    }
                }
            </For>
        </ListBox>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::chat_profiles::{FolderRef, UsedBy};

    fn p(builtin: bool, threads: i64, folders: usize) -> Profile {
        Profile {
            builtin: builtin.then(|| BUILTIN_CONCISE.to_string()),
            used_by: UsedBy {
                threads,
                folders: (0..folders)
                    .map(|i| FolderRef {
                        id: i as i64,
                        name: format!("f{i}"),
                    })
                    .collect(),
            },
            ..Profile::default()
        }
    }

    #[test]
    fn a_row_says_who_uses_it_and_the_built_in_only_for_built_ins() {
        assert_eq!(
            meta_of(&p(true, 3, 1)),
            "Built-in · used by 3 threads · 1 folder"
        );
        assert_eq!(meta_of(&p(false, 0, 0)), "Not used");
        assert_eq!(meta_of(&p(false, 1, 0)), "Used by 1 thread");
        assert_eq!(meta_of(&p(true, 0, 0)), "Built-in · not used");
        assert_eq!(meta_of(&p(false, 0, 2)), "Used by 2 folders");
    }
}
