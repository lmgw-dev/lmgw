//! `/chat/profiles`: the personality-profile editor (personality-profiles
//! design §4.1) as a list beside the selected profile's detail on the shared
//! split view (docs/design/ui-rebuild/modern-sample.html), like Upstreams.
//! The selection is the path (`/chat/profiles/<id>`, `/chat/profiles/new`),
//! so a link opens the page with that profile selected, and leaving one with
//! unsaved edits asks first through the shared guard.
//!
//! `?thread=<id>`, set by the thread drawer's "Edit profiles…", is carried
//! across every selection: it makes Preview, Test and Speak assemble as that
//! thread would and tests on its model, and the list header's back button
//! returns to that thread. The editor itself is the kit's
//! (`lmgw_ui_kit::profiles::ProfileEditor`), which the desktop client uses
//! too.
//!
//! The Chat's own profile pieces (the list its pickers read, the drawer's
//! field and the header chip) are `lmgw-chat-ui`'s; the drawer's "Edit
//! profiles…" links here through the dashboard's `ChatLocation`.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::chat_profiles::{Profile, ProfileList};
use lmgw_ui_kit::http::Http;
use lmgw_ui_kit::profiles::api;
use serde_json::Value;

use crate::scope::Latest;
use crate::widgets::list_pane::{ListRefs, Nav};
use crate::widgets::nav_split::{provide_save_key, NavSplit, SplitConf, SplitState};
use crate::widgets::{use_toasts, Toasts};

/// The detail pane: nothing selected, a profile, a new one, or gone.
mod detail;
/// The list pane's header and rows.
mod list;

const CONF: SplitConf = SplitConf {
    class: "profiles-split",
    width_key: "chat.profiles.list.width",
    list_width: (240, 32, 360),
    detail_min: 440,
    narrow_below: 800.0,
    handle_label: "Resize the list",
};

/// The page's own base path.
const BASE: &str = "/chat/profiles";

/// The list's own query: the search and the thread this was opened from,
/// carried from one selection to the next.
const LIST_KEYS: &[&str] = &["q", "thread"];

/// What the path selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sel {
    /// A profile that is not saved yet.
    New,
    Row(i64),
}

impl Sel {
    fn parse(id: Option<&str>) -> Option<Self> {
        match id? {
            "new" => Some(Self::New),
            id => id.parse().ok().map(Self::Row),
        }
    }

    fn path(self) -> String {
        match self {
            Self::New => format!("{BASE}/new"),
            Self::Row(id) => format!("{BASE}/{id}"),
        }
    }
}

/// Where Discard on a new profile's draft goes back to: the path "New
/// profile" was opened from while the list still holds that row, else the
/// page's own (nothing selected).
fn back_from_new(before: Option<i64>, list: &ProfileList) -> String {
    match before {
        Some(id) if list.profiles.iter().any(|p| p.id == id) => Sel::Row(id).path(),
        _ => BASE.to_string(),
    }
}

/// What the panes share, captured at render: handlers run without an owner
/// to look context up in.
#[derive(Clone, Copy)]
struct Page {
    toasts: Toasts,
    http: Http,
    /// The profiles as last read; a failed refresh keeps them.
    list: RwSignal<Option<ProfileList>>,
    error: RwSignal<Option<String>>,
    latest: Latest,
    nav: Nav,
    /// The thread the page was opened from (`?thread=`), read once.
    thread: Option<i64>,
    /// The thread's model, for Test and Count; known once `ready`.
    test_model: RwSignal<Option<String>>,
    /// The thread's model was read (or there is no thread): an editor
    /// mounts after it, since it takes the model at mount.
    ready: RwSignal<bool>,
    /// The profile "New profile" was opened from, for its draft's Discard.
    before_new: StoredValue<Option<i64>>,
}

impl Page {
    fn load(self) {
        let Some(ticket) = self.latest.next() else {
            return;
        };
        spawn_local(async move {
            let res = api::list(self.http).await;
            if !self.latest.is(ticket) {
                return;
            }
            match res {
                Ok(l) => {
                    self.list.set(Some(l));
                    self.error.set(None);
                }
                Err(e) => self.error.set(Some(e.to_string())),
            }
        });
    }

    /// A stored row (a create, a save, a reset, the built-in added) into the
    /// list in the gateway's order, then the list read again.
    fn stored(self, p: Profile) {
        self.list.update(|l| {
            if let Some(l) = l {
                lmgw_ui_kit::profiles::upsert(l, p);
            }
        });
        self.load();
    }

    fn by_id(self, id: i64) -> Option<Profile> {
        self.list.with(|l| {
            l.as_ref()
                .and_then(|l| l.profiles.iter().find(|p| p.id == id).cloned())
        })
    }

    /// Where the header's back button goes: the thread, or the Chat.
    fn back_href(self) -> String {
        match self.thread {
            Some(id) => format!("/chat?t={id}"),
            None => "/chat".to_string(),
        }
    }

    /// The page's own base with the list's query kept (the status page).
    fn base_href(self) -> String {
        format!("{BASE}{}", self.nav.kept(LIST_KEYS))
    }

    /// Open a blank draft, remembering the profile it was opened from.
    fn open_new(self, from: Option<Sel>) {
        self.before_new.set_value(match from {
            Some(Sel::Row(id)) => Some(id),
            _ => None,
        });
        self.nav.open(&Sel::New.path(), LIST_KEYS, false);
    }

    /// Add the built-in Concise back, and open it.
    fn add_builtin(self, then_open: bool) {
        spawn_local(async move {
            match api::create_builtin(self.http, lmgw_api_types::chat_profiles::BUILTIN_CONCISE)
                .await
            {
                Ok(p) => {
                    let id = p.id;
                    let name = p.name.clone();
                    self.stored(p);
                    self.toasts.ok(format!("Added '{name}'"));
                    if then_open {
                        self.nav.open(&Sel::Row(id).path(), LIST_KEYS, false);
                    }
                }
                Err(e) => self.toasts.err(e.to_string()),
            }
        });
    }
}

#[component]
pub fn ChatProfiles() -> impl IntoView {
    let thread = leptos_router::hooks::use_query_map()
        .with_untracked(|q| q.get("thread").and_then(|v| v.parse::<i64>().ok()));
    let page = Page {
        toasts: use_toasts(),
        http: crate::api::use_http(),
        list: RwSignal::new(None),
        error: RwSignal::new(None),
        latest: Latest::new(),
        nav: Nav::new(),
        thread,
        test_model: RwSignal::new(None),
        ready: RwSignal::new(thread.is_none()),
        before_new: StoredValue::new(None),
    };
    page.load();
    // The thread's model, for Test: read once; a thread that is gone just
    // leaves the model to the editor's own picker.
    if let Some(id) = thread {
        let http = page.http;
        spawn_local(async move {
            if let Ok(v) = http.get::<Value>(format!("/chat/api/threads/{id}")).await {
                let model = v["thread"]["model_alias"]
                    .as_str()
                    .filter(|m| !m.is_empty())
                    .map(str::to_string);
                page.test_model.try_set(model);
            }
            page.ready.try_set(true);
        });
    }
    let params = leptos_router::hooks::use_params_map();
    let sel = Memo::new(move |_| params.with(|p| Sel::parse(p.get_str("sel"))));
    let key = Memo::new(move |_| sel.get().map(Sel::path));
    let split = SplitState::new(CONF);
    let narrow = split.narrow;
    let refs = ListRefs::new();
    let save_key = provide_save_key();
    let open_row = Callback::new(move |(to, replace): (String, bool)| {
        page.nav.open(&to, LIST_KEYS, replace);
    });
    let back = Callback::new(move |()| page.nav.back(BASE, LIST_KEYS, refs));
    view! {
        <NavSplit
            split=split
            has_sel=Signal::derive(move || sel.with(Option::is_some))
            save_key=save_key
            search=refs.search
            search_off=super::collection::search_off(narrow, key)
            list=ViewFn::from(move || {
                view! { <list::ProfilesList page=page sel=sel key=key narrow=narrow refs=refs open_row=open_row/> }
            })
            detail=ViewFn::from(move || {
                view! { <detail::DetailPane page=page sel=sel narrow=narrow back=back/> }
            })
        />
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: i64) -> Profile {
        Profile {
            id,
            ..Profile::default()
        }
    }

    #[test]
    fn a_path_names_its_selection() {
        for sel in [Sel::New, Sel::Row(7)] {
            let path = sel.path();
            assert_eq!(
                Sel::parse(path.strip_prefix("/chat/profiles/")),
                Some(sel),
                "{path}"
            );
        }
        assert_eq!(Sel::parse(None), None);
        assert_eq!(Sel::parse(Some("x")), None);
    }

    #[test]
    fn a_new_profile_s_discard_goes_back_while_its_origin_is_there() {
        let l = ProfileList {
            profiles: vec![p(1), p(3)],
            default_profile_id: None,
        };
        assert_eq!(back_from_new(Some(3), &l), "/chat/profiles/3");
        assert_eq!(back_from_new(None, &l), "/chat/profiles");
        // Gone meanwhile: the status page, not an empty pane on a dead id.
        assert_eq!(back_from_new(Some(9), &l), "/chat/profiles");
    }
}
