//! MCP servers — southbound tool servers aggregated behind the gateway's
//! northbound /mcp endpoint, and the inventory of every tool that adds up
//! to, as a list beside the selected one's detail on the shared split view
//! (docs/design/ui-rebuild/modern-sample.html). Live status rides the shared
//! SSE bus.
//!
//! The list holds the configured servers and the built-in toolsets
//! (`lmgw__*`, `docs__*`). The selection is the path (`/mcp-servers/3`,
//! `/mcp-servers/new`, `/mcp-servers/builtin/lmgw`): a server's detail is
//! its form and its tools, a built-in's its tools; with nothing selected
//! the detail is the whole tool inventory.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{McpServerSaved, McpServerView, McpServersResponse, ToolInventory};
use serde_json::json;

use crate::live::{use_live, LiveBus};
use crate::scope::Latest;
use crate::url_state::use_query_signal;
use crate::widgets::list_pane::{query_keeping, ListRefs, Nav};
use crate::widgets::nav_split::{provide_save_key, NavSplit, SplitConf, SplitState};
use crate::widgets::{use_toasts, Modal, ModalFooter, Toasts};

/// The detail pane: the inventory, a server, a built-in, or gone.
mod detail;
/// One server: the header, the hero, the groups, its tools, the save toast.
mod editor;
/// The form's signals, its baseline and its body.
mod form;
/// The list pane's rows.
mod list;
/// The tool inventory: the filter, a group per source, a row per tool.
mod tools;

use tools::InvFilter;

const CONF: SplitConf = SplitConf {
    class: "mcp-split",
    width_key: "mcp.list.width",
    list_width: (240, 32, 360),
    detail_min: 460,
    narrow_below: 800.0,
    handle_label: "Resize the list",
};

/// What the path selects.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Sel {
    /// A server that is not saved yet.
    New,
    Server(i64),
    /// A built-in toolset, by its label.
    Builtin(String),
}

impl Sel {
    fn parse(id: Option<&str>, label: Option<&str>) -> Option<Self> {
        match (id?, label) {
            ("new", None) => Some(Self::New),
            ("builtin", Some(l)) if !l.is_empty() => Some(Self::Builtin(l.to_string())),
            (id, None) => id.parse().ok().map(Self::Server),
            _ => None,
        }
    }

    fn path(&self) -> String {
        match self {
            Self::New => "/mcp-servers/new".into(),
            Self::Server(id) => format!("/mcp-servers/{id}"),
            Self::Builtin(l) => format!(
                "/mcp-servers/builtin/{}",
                String::from(js_sys::encode_uri_component(l))
            ),
        }
    }
}

/// The query carried from one selection to the next: the list's search
/// (`find`) and the tool filter (`q`, `show`).
const LIST_KEYS: &[&str] = &["find", "q", "show"];

/// The carried query out of a search string.
fn kept_query(search: &str) -> String {
    query_keeping(search, LIST_KEYS)
}

/// The servers list, refetched in place after every op.
#[derive(Clone, Copy)]
struct ServersData {
    rows: RwSignal<Option<Vec<McpServerView>>>,
    error: RwSignal<Option<String>>,
    latest: Latest,
}

impl ServersData {
    fn new() -> Self {
        let d = Self {
            rows: RwSignal::new(None),
            error: RwSignal::new(None),
            latest: Latest::new(),
        };
        d.load();
        d
    }

    fn load(self) {
        let Some(ticket) = self.latest.next() else {
            return;
        };
        spawn_local(async move {
            let res = crate::api::get::<McpServersResponse>("/api/mcp-servers").await;
            if !self.latest.is(ticket) {
                return;
            }
            match res {
                Ok(r) => {
                    self.rows.set(Some(r.mcp_servers));
                    self.error.set(None);
                }
                Err(e) => self.error.set(Some(e.to_string())),
            }
        });
    }

    fn by_id(self, id: i64) -> Option<McpServerView> {
        self.rows.with(|r| {
            r.as_ref()
                .and_then(|r| r.iter().find(|s| s.id == id).cloned())
        })
    }
}

/// `/api/tools`, refetched after a switch and whenever a server's status
/// changes (a server that connects brings its tools with it).
#[derive(Clone, Copy)]
struct InvData {
    inv: RwSignal<Option<ToolInventory>>,
    error: RwSignal<Option<String>>,
    latest: Latest,
}

impl InvData {
    fn new() -> Self {
        let d = Self {
            inv: RwSignal::new(None),
            error: RwSignal::new(None),
            latest: Latest::new(),
        };
        d.load();
        d
    }

    fn load(self) {
        let Some(ticket) = self.latest.next() else {
            return;
        };
        spawn_local(async move {
            let res = crate::api::get::<ToolInventory>("/api/tools").await;
            if !self.latest.is(ticket) {
                return;
            }
            match res {
                Ok(r) => {
                    self.inv.set(Some(r));
                    self.error.set(None);
                }
                Err(e) => self.error.set(Some(e.to_string())),
            }
        });
    }
}

/// What the panes share.
#[derive(Clone, Copy)]
struct Page {
    toasts: Toasts,
    live: LiveBus,
    servers: ServersData,
    inv: InvData,
    filter: InvFilter,
    deleting: RwSignal<Option<McpServerView>>,
    nav: Nav,
}

impl Page {
    fn reload(self) {
        self.servers.load();
        self.inv.load();
    }

    /// A server's live status: (status, tools it lists, detail), the bus's
    /// once it has one, else the list's.
    fn status(self, s: &McpServerView) -> (String, i64, Option<String>) {
        self.live
            .mcp
            .with(|m| {
                m.as_ref()
                    .and_then(|m| m.iter().find(|v| v.id == s.id))
                    .map(|v| (v.status.clone(), v.tool_count, v.detail.clone()))
            })
            .unwrap_or_else(|| (s.status.clone(), s.tool_count, s.status_detail.clone()))
    }

    /// How many of a server's tools the gateway offers: what it lists less
    /// what is switched off here, so it agrees with the inventory.
    fn offered(self, id: i64) -> Option<usize> {
        self.inv.inv.with(|i| {
            i.as_ref().map(|i| {
                i.tools
                    .iter()
                    .filter(|t| t.server_id == Some(id) && t.available)
                    .count()
            })
        })
    }

    /// Enable, disable, delete or test a server.
    fn op(self, action: &'static str, id: i64, busy: Option<RwSignal<bool>>, then: Option<String>) {
        if busy.is_some_and(|b| b.get_untracked()) {
            return;
        }
        if let Some(b) = busy {
            b.set(true);
        }
        spawn_local(async move {
            let res = crate::api::post::<McpServerSaved, _>(
                "/api/op/mcp_server_set",
                &json!({ "action": action, "id": id }),
            )
            .await;
            if let Some(b) = busy {
                let _ = b.try_set(false);
            }
            match res {
                Ok(v) => {
                    self.toasts.ok(then.unwrap_or(v.message));
                    if action != "test" {
                        self.reload();
                    }
                }
                Err(e) => self.toasts.err(e.to_string()),
            }
        });
    }
}

#[component]
pub fn McpServers() -> impl IntoView {
    let live = use_live();
    let page = Page {
        toasts: use_toasts(),
        live,
        servers: ServersData::new(),
        inv: InvData::new(),
        filter: InvFilter {
            query: use_query_signal("q"),
            facet: use_query_signal("show"),
        },
        deleting: RwSignal::new(None),
        nav: Nav::new(),
    };
    // A server that connects, drops or errors changes what it offers: the
    // inventory follows the bus, not only the owner's own clicks. Only a real
    // change counts. The bus repeats the current statuses on every (re)connect,
    // and each `/api/tools` fetch connects every enabled server that is not up
    // — a podman run for an isolated one — so a refetch per reconnect woke
    // idle servers for nothing.
    let statuses = Memo::new(move |_| {
        live.mcp.with(|m| {
            m.as_ref().map(|v| {
                let mut s: Vec<(i64, String, i64)> = v
                    .iter()
                    .map(|x| (x.id, x.status.clone(), x.tool_count))
                    .collect();
                s.sort();
                s
            })
        })
    });
    // The Memo only moves when the statuses do; a page opened before the bus
    // had any takes the first ones as news too.
    Effect::new(move |prev: Option<()>| {
        statuses.with(|_| ());
        if prev.is_some() {
            page.inv.load();
        }
    });
    // A Chat job that starts or ends names its thread in the `chat` frame:
    // the list is read again, so a server's open-task count follows (MCP
    // Tasks design §6). The frames are the Chat page's to take, not ours.
    Effect::new(move |prev: Option<()>| {
        live.chat.arrived();
        if prev.is_some() {
            page.servers.load();
        }
    });

    let params = leptos_router::hooks::use_params_map();
    let sel = Memo::new(move |_| params.with(|p| Sel::parse(p.get_str("id"), p.get_str("label"))));
    let key = Memo::new(move |_| sel.with(|s| s.as_ref().map(Sel::path)));
    let split = SplitState::new(CONF);
    let narrow = split.narrow;
    let refs = ListRefs::new();
    let save_key = provide_save_key();
    let open_row = Callback::new(move |(to, replace): (String, bool)| {
        page.nav.open(&to, LIST_KEYS, replace);
    });
    let back = Callback::new(move |()| page.nav.back("/mcp-servers", LIST_KEYS, refs));
    view! {
        <NavSplit
            split=split
            has_sel=Signal::derive(move || sel.with(Option::is_some))
            save_key=save_key
            search=refs.search
            search_off=super::collection::search_off(narrow, key)
            list=ViewFn::from(move || {
                view! { <list::McpList page=page sel=key narrow=narrow refs=refs open_row=open_row/> }
            })
            detail=ViewFn::from(move || {
                view! { <detail::DetailPane page=page sel=sel narrow=narrow back=back/> }
            })
        />
        <DeleteModal page=page/>
    }
}

/// Deleting a server takes its tools off /mcp: asked once, in words.
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
        <Modal open=open title="Delete MCP server">
            {move || {
                page.deleting
                    .get()
                    .map(|s| {
                        let id = s.id;
                        let name = StoredValue::new(s.name.clone());
                        view! {
                            <p>"Delete '" {s.name.clone()} "'? Its tools leave /mcp with it."</p>
                            <ModalFooter>
                                <button class="btn ghost" on:click=move |_| open.set(false)>
                                    "Keep it"
                                </button>
                                <button
                                    class="btn danger"
                                    on:click=move |_| {
                                        open.set(false);
                                        page.op("delete", id, None, Some(format!("{} deleted", name.get_value())));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_names_its_selection() {
        assert_eq!(Sel::parse(Some("new"), None), Some(Sel::New));
        assert_eq!(Sel::parse(Some("3"), None), Some(Sel::Server(3)));
        assert_eq!(
            Sel::parse(Some("builtin"), Some("lmgw")),
            Some(Sel::Builtin("lmgw".into()))
        );
        assert_eq!(Sel::parse(Some("builtin"), None), None);
        assert_eq!(Sel::parse(Some("3"), Some("x")), None);
        assert_eq!(Sel::parse(None, None), None);
        assert_eq!(Sel::Server(3).path(), "/mcp-servers/3");
        assert_eq!(Sel::New.path(), "/mcp-servers/new");
    }

    #[test]
    fn the_list_search_and_the_tool_filter_travel() {
        assert_eq!(
            kept_query("?find=git&q=read&show=off&x=1"),
            "?find=git&q=read&show=off"
        );
        assert_eq!(kept_query(""), "");
    }
}
