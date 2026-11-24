//! Upstreams — cloud and self-hosted providers the gateway routes to, as a
//! list beside the selected one's detail on the shared split view
//! (docs/design/ui-rebuild/modern-sample.html). The selection is the path
//! (`/upstreams/<id>`, `/upstreams/new`), so a link opens the page with that
//! upstream selected, and leaving one with unsaved edits asks first.
//!
//! Secrets follow the house convention: an empty API-key field keeps the
//! stored key, header values round-trip as "<set>".

use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{RowSaved, UpstreamModelsResponse, UpstreamView, UpstreamsResponse};
use serde_json::json;

use super::models::ModelsData;
use crate::catalog::use_model_catalog;
use crate::fmt::grouped;
use crate::scope::Latest;
use crate::widgets::list_pane::{query_keeping, ListRefs, Nav};
use crate::widgets::nav_split::{provide_save_key, NavSplit, SplitConf, SplitState};
use crate::widgets::{use_toasts, Modal, ModalFooter, Toasts};

/// The detail pane: nothing selected, an upstream, a new one, or gone.
mod detail;
/// One upstream: the header, the hero, the groups, the save toast.
mod editor;
/// The form's signals, its baseline and its body.
mod form;
/// The hero: glyph, name, URL and the chips of its state.
mod hero;
/// The list pane's rows.
mod list;
mod llama_facts;
mod quota;

const CONF: SplitConf = SplitConf {
    class: "upstreams-split",
    width_key: "upstreams.list.width",
    list_width: (240, 32, 360),
    detail_min: 440,
    narrow_below: 800.0,
    handle_label: "Resize the list",
};

/// One upstream's live catalog on this page: how many models it lists, or
/// why it could not be read.
#[derive(Clone, PartialEq)]
enum Catalog {
    Loading,
    Ready(usize),
    Failed(String),
}

/// What the path selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sel {
    /// An upstream that is not saved yet.
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
            Self::New => "/upstreams/new".into(),
            Self::Row(id) => format!("/upstreams/{id}"),
        }
    }
}

/// The list's own query (`q`), carried from one selection to the next.
const LIST_KEYS: &[&str] = &["q"];

/// The list's own query out of a search string.
fn list_query(search: &str) -> String {
    query_keeping(search, LIST_KEYS)
}

/// What the panes share, captured at render: handlers run without an owner
/// to look context up in.
#[derive(Clone, Copy)]
struct Page {
    toasts: Toasts,
    /// The upstreams as last read; a failed refresh keeps them.
    list: RwSignal<Option<Vec<UpstreamView>>>,
    error: RwSignal<Option<String>>,
    latest: Latest,
    catalogs: RwSignal<HashMap<i64, Catalog>>,
    /// The newest catalog read per upstream: an older one landing late is
    /// dropped.
    counting: StoredValue<HashMap<i64, u64>>,
    models: ModelsData,
    deleting: RwSignal<Option<UpstreamView>>,
    refresh_catalog: Callback<()>,
    nav: Nav,
}

impl Page {
    fn load(self) {
        let Some(ticket) = self.latest.next() else {
            return;
        };
        spawn_local(async move {
            let res = crate::api::get::<UpstreamsResponse>("/api/upstreams").await;
            if !self.latest.is(ticket) {
                return;
            }
            match res {
                Ok(r) => {
                    for u in r.upstreams.iter().filter(|u| u.enabled) {
                        if self.catalogs.with_untracked(|m| !m.contains_key(&u.id)) {
                            self.count(u.id, None);
                        }
                    }
                    self.list.set(Some(r.upstreams));
                    self.error.set(None);
                }
                Err(e) => self.error.set(Some(e.to_string())),
            }
        });
        self.models.load();
    }

    /// Retry: the list again, and every enabled row's catalog asked again
    /// (a count that failed stays failed until it is).
    fn retry(self) {
        let enabled: Vec<i64> = self.list.with_untracked(|l| {
            l.iter()
                .flatten()
                .filter(|u| u.enabled)
                .map(|u| u.id)
                .collect()
        });
        self.catalogs.update(|m| {
            for id in enabled {
                m.remove(&id);
            }
        });
        self.load();
    }

    fn by_id(self, id: i64) -> Option<UpstreamView> {
        self.list.with(|l| {
            l.as_ref()
                .and_then(|l| l.iter().find(|u| u.id == id).cloned())
        })
    }

    /// Read one upstream's catalog size; with `tested`, toast the connection
    /// test's result with it.
    fn count(self, id: i64, tested: Option<String>) {
        let Some(n) = self.counting.try_update_value(|m| {
            let n = m.entry(id).or_default();
            *n += 1;
            *n
        }) else {
            return;
        };
        self.catalogs.update(|m| {
            m.insert(id, Catalog::Loading);
        });
        spawn_local(async move {
            let res =
                crate::api::get::<UpstreamModelsResponse>(format!("/api/upstream-models?id={id}"))
                    .await;
            let state = match res {
                Ok(r) => Catalog::Ready(r.entries.len().max(r.models.len())),
                Err(e) => Catalog::Failed(e.to_string()),
            };
            if let Some(name) = tested {
                match &state {
                    Catalog::Ready(n) => self.toasts.ok(format!(
                        "{name}: connection OK · lists {} models",
                        grouped(*n as u64)
                    )),
                    Catalog::Failed(e) => self.toasts.warn(format!(
                        "{name}: connection OK, but its catalog did not load — {e}"
                    )),
                    Catalog::Loading => {}
                }
            }
            if self.counting.try_with_value(|m| m.get(&id) == Some(&n)) != Some(true) {
                return;
            }
            self.catalogs.update(|m| {
                m.insert(id, state);
            });
        });
    }

    /// Enable, disable or delete; what the gateway serves changes with it.
    fn set(self, action: &'static str, id: i64) {
        spawn_local(async move {
            match crate::api::post::<RowSaved, _>(
                "/api/op/upstream_set",
                &json!({ "action": action, "id": id }),
            )
            .await
            {
                Ok(v) => {
                    self.toasts.ok(v.message);
                    // A delete names the fallbacks it leaves without a
                    // target: its aliases and passthrough ids go with it.
                    for note in v.fallback_notes {
                        self.toasts.warn(note);
                    }
                    self.refresh_catalog.try_run(());
                    if action == "enable" {
                        // Enabled again: its catalog is asked again.
                        self.count(id, None);
                    }
                    self.load();
                }
                Err(e) => self.toasts.err(e.to_string()),
            }
        });
    }

    /// Ask it for its model list: checks the URL, the key and the protocol.
    fn test(self, id: i64, name: String, testing: RwSignal<bool>) {
        if testing.get_untracked() {
            return;
        }
        testing.set(true);
        spawn_local(async move {
            let res = crate::api::post::<lmgw_api_types::UpstreamTested, _>(
                "/api/op/upstream_test",
                &json!({ "id": id }),
            )
            .await;
            let _ = testing.try_set(false);
            match res {
                // The count comes with the OK: a connection that works but
                // lists nothing is worth knowing about. An `openai` row on a
                // host that is not OpenAI's says so as well (openai egress
                // design, decision 13).
                Ok(tested) => {
                    if let Some(note) = tested.note {
                        self.toasts.warn(note);
                    }
                    self.count(id, Some(name));
                    // A llama.cpp row's facts were asked again.
                    self.load();
                }
                Err(e) => self.toasts.err(e.to_string()),
            }
        });
    }
}

#[component]
pub fn Upstreams() -> impl IntoView {
    let catalog = use_model_catalog();
    let page = Page {
        toasts: use_toasts(),
        list: RwSignal::new(None),
        error: RwSignal::new(None),
        latest: Latest::new(),
        catalogs: RwSignal::new(HashMap::new()),
        counting: StoredValue::new(HashMap::new()),
        models: ModelsData::new(),
        deleting: RwSignal::new(None),
        refresh_catalog: Callback::new(move |()| catalog.refresh()),
        nav: Nav::new(),
    };
    page.load();
    let params = leptos_router::hooks::use_params_map();
    let sel = Memo::new(move |_| params.with(|p| Sel::parse(p.get_str("id"))));
    let key = Memo::new(move |_| sel.get().map(Sel::path));
    let split = SplitState::new(CONF);
    let narrow = split.narrow;
    let refs = ListRefs::new();
    let save_key = provide_save_key();
    let open_row = Callback::new(move |(to, replace): (String, bool)| {
        page.nav.open(&to, LIST_KEYS, replace);
    });
    let back = Callback::new(move |()| page.nav.back("/upstreams", LIST_KEYS, refs));
    view! {
        <NavSplit
            split=split
            has_sel=Signal::derive(move || sel.with(Option::is_some))
            save_key=save_key
            search=refs.search
            search_off=super::collection::search_off(narrow, key)
            list=ViewFn::from(move || {
                view! { <list::UpList page=page sel=key narrow=narrow refs=refs open_row=open_row/> }
            })
            detail=ViewFn::from(move || {
                view! { <detail::DetailPane page=page sel=sel narrow=narrow back=back/> }
            })
        />
        <DeleteModal page=page/>
    }
}

/// Deleting an upstream takes its aliases with it: the question says how
/// many, from the configured models.
#[component]
fn DeleteModal(page: Page) -> impl IntoView {
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = page.deleting.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && page.deleting.with_untracked(Option::is_some) {
            page.deleting.set(None);
        }
    });
    view! {
        <Modal open=open title="Delete upstream">
            {move || {
                page.deleting
                    .get()
                    .map(|u| {
                        let id = u.id;
                        let aliases: Vec<String> = page.models.full.with(|f| {
                            f.as_ref()
                                .map(|f| {
                                    f.aliases
                                        .iter()
                                        .filter(|a| a.upstream_id == id)
                                        .map(|a| a.alias.clone())
                                        .collect()
                                })
                                .unwrap_or_default()
                        });
                        let what = match aliases.len() {
                            0 => "No alias uses it.".to_string(),
                            1 => format!("Its alias {} is removed with it.", aliases[0]),
                            n => format!("Its {n} aliases are removed with it: {}.", aliases.join(", ")),
                        };
                        view! {
                            <p>"Delete '" {u.name.clone()} "'? " {what}</p>
                            <ModalFooter>
                                <button class="btn ghost" on:click=move |_| open.set(false)>
                                    "Keep it"
                                </button>
                                <button
                                    class="btn danger"
                                    on:click=move |_| {
                                        open.set(false);
                                        page.set("delete", id);
                                    }
                                >
                                    "Delete"
                                </button>
                            </ModalFooter>
                        }
                    })
            }}
        </Modal>
    }
}

/// The protocol value of llama.cpp's own server (llama-server, ik_llama.cpp).
const LLAMA_CPP: &str = "llama_cpp";

/// The protocol value of the OpenAI platform (api.openai.com).
const OPENAI: &str = "openai";

/// The protocol value of every other OpenAI-shaped provider.
const OPENAI_COMPATIBLE: &str = "openai_compatible";

/// The protocols, with their words.
const PROTOCOLS: [(&str, &str); 5] = [
    (OPENAI, "OpenAI"),
    (OPENAI_COMPATIBLE, "OpenAI-compatible"),
    ("anthropic", "Anthropic"),
    ("gemini", "Gemini"),
    (LLAMA_CPP, "llama.cpp"),
];

/// A protocol in words: what the picker, a list row and the hero say.
fn protocol_label(p: &str) -> &str {
    PROTOCOLS
        .iter()
        .find(|(v, _)| *v == p)
        .map_or(p, |(_, l)| l)
}

/// The kind once the protocol moved from `prev` to `next`: `llama_cpp` is
/// always `llama_server` and `openai` always `generic`, leaving `llama_cpp`
/// resets the kind to `generic`, `llama_server` survives only under
/// `anthropic` and `audio_cpp` only under `openai_compatible` (llama egress
/// design §5, openai egress design §1.5, what `settle` accepts). Any other
/// move keeps the owner's pick.
fn kind_after_protocol(prev: &str, next: &str, kind: &str) -> String {
    if next == LLAMA_CPP {
        "llama_server".into()
    } else if next == OPENAI
        || prev == LLAMA_CPP
        || (next != "anthropic" && kind == "llama_server")
        || (next != OPENAI_COMPATIBLE && kind == "audio_cpp")
    {
        "generic".into()
    } else {
        kind.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_names_its_selection_and_back() {
        for sel in [Sel::New, Sel::Row(7)] {
            let path = sel.path();
            assert_eq!(
                Sel::parse(path.strip_prefix("/upstreams/")),
                Some(sel),
                "{path}"
            );
        }
        assert_eq!(Sel::parse(None), None);
        assert_eq!(Sel::parse(Some("x")), None);
        assert_eq!(list_query("?q=open&x=1"), "?q=open");
        assert_eq!(list_query(""), "");
    }

    #[test]
    fn the_kind_follows_the_protocol_into_and_out_of_llama_cpp() {
        assert_eq!(
            kind_after_protocol("openai_compatible", "llama_cpp", "generic"),
            "llama_server"
        );
        assert_eq!(
            kind_after_protocol("anthropic", "llama_cpp", "audio_cpp"),
            "llama_server"
        );
        // Leaving llama_cpp: never openai_compatible + llama_server, which the
        // server refuses.
        assert_eq!(
            kind_after_protocol("llama_cpp", "openai_compatible", "llama_server"),
            "generic"
        );
        assert_eq!(
            kind_after_protocol("llama_cpp", "anthropic", "llama_server"),
            "generic"
        );
        // Elsewhere the owner's pick stands.
        assert_eq!(
            kind_after_protocol("openai_compatible", "anthropic", "llama_server"),
            "llama_server"
        );
        // llama_server only survives under anthropic.
        assert_eq!(
            kind_after_protocol("anthropic", "gemini", "llama_server"),
            "generic"
        );
        assert_eq!(
            kind_after_protocol("anthropic", "openai_compatible", "llama_server"),
            "generic"
        );
        assert_eq!(
            kind_after_protocol("anthropic", "anthropic", "llama_server"),
            "llama_server"
        );
    }

    /// The OpenAI platform is always generic; an audio.cpp server lives under
    /// openai_compatible only (openai egress design, decision 7).
    #[test]
    fn the_kind_follows_the_protocol_into_and_out_of_the_openai_platform() {
        for kind in ["generic", "audio_cpp", "llama_server"] {
            for prev in ["openai_compatible", "anthropic", "gemini", "llama_cpp"] {
                assert_eq!(kind_after_protocol(prev, "openai", kind), "generic");
            }
        }
        // audio_cpp survives only under openai_compatible.
        assert_eq!(
            kind_after_protocol("openai_compatible", "openai_compatible", "audio_cpp"),
            "audio_cpp"
        );
        assert_eq!(
            kind_after_protocol("openai", "openai_compatible", "generic"),
            "generic"
        );
        for next in ["anthropic", "gemini"] {
            assert_eq!(
                kind_after_protocol("openai_compatible", next, "audio_cpp"),
                "generic"
            );
        }
    }
}
