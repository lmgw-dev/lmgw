//! MCP servers — southbound tool servers aggregated behind the gateway's
//! northbound /mcp endpoint, and the inventory of every tool that adds up to.
//! Live status badges ride the shared SSE bus.
//!
//! The inventory is the page's one long list (108 tools on the dev copy, 57
//! of them from one server), so it is the fill pane: one band per source,
//! folded until asked for, with every count in its header.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{
    McpServerView, McpServersResponse, ToolEntryView, ToolInventory, ToolSourceView,
};
use serde_json::{json, Value};
use wasm_bindgen::JsCast;

use crate::fmt::{grouped, of};
use crate::live::use_live;
use crate::url_state::use_query_signal;
use crate::widgets::{
    filter_words, use_toasts, ConfirmButton, Facet, FacetSet, FilterBar, GroupRow, MenuItem, Modal,
    ModalFooter, ModalSize, ModelPicker, PageFrame, PageMode, RowMenu, Section, Select, Toasts,
};

/// Columns of the inventory table: tick, tool, description, state, why.
const INV_COLS: u32 = 5;

/// The servers list, refetched in place after every op.
#[derive(Clone, Copy)]
struct ServersData {
    rows: RwSignal<Option<Vec<McpServerView>>>,
    error: RwSignal<Option<String>>,
}

impl ServersData {
    fn new() -> Self {
        let d = Self {
            rows: RwSignal::new(None),
            error: RwSignal::new(None),
        };
        d.load();
        d
    }

    fn load(self) {
        spawn_local(async move {
            match crate::api::get::<McpServersResponse>("/api/mcp-servers").await {
                Ok(r) => {
                    self.rows.set(Some(r.mcp_servers));
                    self.error.set(None);
                }
                Err(e) => self.error.set(Some(e.to_string())),
            }
        });
    }
}

/// `/api/tools`, refetched after a switch and whenever a server's status
/// changes (a server that connects brings its tools with it).
#[derive(Clone, Copy)]
struct InvData {
    inv: RwSignal<Option<ToolInventory>>,
    error: RwSignal<Option<String>>,
}

impl InvData {
    fn new() -> Self {
        let d = Self {
            inv: RwSignal::new(None),
            error: RwSignal::new(None),
        };
        d.load();
        d
    }

    fn load(self) {
        spawn_local(async move {
            match crate::api::get::<ToolInventory>("/api/tools").await {
                Ok(r) => {
                    self.inv.set(Some(r));
                    self.error.set(None);
                }
                Err(e) => self.error.set(Some(e.to_string())),
            }
        });
    }
}

/// The inventory's filter, as the URL holds it: `q`, `show` (a facet) and
/// `server` (a server id, set by clicking its row).
#[derive(Clone, Copy)]
struct InvFilter {
    query: RwSignal<String>,
    facet: RwSignal<String>,
    server: RwSignal<String>,
}

impl InvFilter {
    fn narrowed(self) -> bool {
        !self.query.with(|q| q.trim().is_empty())
            || !self.facet.with(String::is_empty)
            || !self.server.with(String::is_empty)
    }

    fn server_id(self) -> Option<i64> {
        self.server.with(|s| s.parse().ok())
    }

    fn keeps(self, t: &ToolEntryView, words: &[String]) -> bool {
        let f = self.facet.get();
        tool_facet(t, &f)
            && self.server_id().is_none_or(|id| t.server_id == Some(id))
            && tool_matches(t, words)
    }
}

fn tool_facet(t: &ToolEntryView, facet: &str) -> bool {
    match facet {
        "offered" => t.available,
        "off" => !t.enabled && !t.stale,
        "unavailable" => t.enabled && !t.available && !t.stale,
        "stale" => t.stale,
        _ => true,
    }
}

fn tool_matches(t: &ToolEntryView, words: &[String]) -> bool {
    let hay = format!(
        "{} {} {} {}",
        t.name,
        t.description.as_deref().unwrap_or_default(),
        t.upstream_name.as_deref().unwrap_or_default(),
        t.source_name
    )
    .to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// Which band a tool sits in: its source, or the stale records at the end.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Band {
    Source(String, Option<i64>),
    Stale,
}

impl Band {
    fn holds(&self, t: &ToolEntryView) -> bool {
        match self {
            Band::Source(label, id) => !t.stale && &t.source_label == label && &t.server_id == id,
            Band::Stale => t.stale,
        }
    }

    fn persist_key(&self) -> String {
        match self {
            Band::Source(label, _) => format!("open.mcp.src.{label}"),
            Band::Stale => "open.mcp.src.stale".into(),
        }
    }
}

/// Switch tools one `tool_set` at a time, counting as it goes, then refetch
/// once. There is no bulk op: the loop is the owner's own clicks, made for
/// them, and its progress is on screen while it runs.
fn switch_all(
    names: Vec<String>,
    enable: bool,
    band: String,
    progress: RwSignal<Option<(usize, usize, bool)>>,
    toasts: Toasts,
    inv: InvData,
) {
    let total = names.len();
    if total == 0 || progress.get_untracked().is_some() {
        return;
    }
    progress.set(Some((0, total, enable)));
    spawn_local(async move {
        let mut failed: Vec<String> = Vec::new();
        for (i, name) in names.into_iter().enumerate() {
            if let Err(e) = crate::api::post::<Value, _>(
                "/api/op/tool_set",
                &json!({ "name": name, "enabled": enable }),
            )
            .await
            {
                failed.push(format!("{name}: {e}"));
            }
            progress.set(Some((i + 1, total, enable)));
        }
        progress.set(None);
        inv.load();
        let verb = if enable {
            "switched on"
        } else {
            "switched off"
        };
        if failed.is_empty() {
            toasts.ok(format!("{total} tools {verb} in {band}"));
        } else {
            toasts.err(format!(
                "{} of {total} tools in {band} could not be {verb}: {}",
                failed.len(),
                failed.join("; ")
            ));
        }
    });
}

#[component]
pub fn McpServers() -> impl IntoView {
    let live = use_live();
    let servers = ServersData::new();
    let inv = InvData::new();
    let filter = InvFilter {
        query: use_query_signal("q"),
        facet: use_query_signal("show"),
        server: use_query_signal("server"),
    };
    let editing = RwSignal::new(None::<McpServerView>);
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = editing.get().is_some();
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && editing.get_untracked().is_some() {
            editing.set(None);
        }
    });
    // A server that connects, drops or errors changes what it offers: the
    // inventory follows the bus, not only the owner's own clicks. Only a real
    // change counts. The bus repeats the current statuses on every (re)connect,
    // and each `/api/tools` fetch connects every enabled server that is not up
    // — a podman run for an isolated one — so a refetch per reconnect woke
    // idle servers for nothing.
    let statuses = Memo::new(move |_| {
        live.mcp.with(|m| {
            m.as_ref().map(|v| {
                let mut s: Vec<(i64, String, Option<i64>)> = v
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
            inv.load();
        }
    });
    let reload = move || {
        servers.load();
        inv.load();
    };

    view! {
        <PageFrame
            title="MCP servers"
            sub="tools aggregated behind /mcp"
            mode=PageMode::Fill
            actions=move || {
                view! {
                    <button
                        class="btn primary"
                        on:click=move |_| {
                            editing
                                .set(
                                    Some(McpServerView {
                                        transport: "stdio".into(),
                                        timeout_ms: 60_000,
                                        autostart: true,
                                        allow_sampling: true,
                                        enabled: true,
                                        ..Default::default()
                                    }),
                                )
                        }
                    >
                        "Add server"
                    </button>
                }
            }
        >
            <ServersSection servers=servers inv=inv filter=filter editing=editing on_change=reload/>
            <Inventory inv=inv filter=filter servers=servers/>

            <Modal open=open title="MCP server" size=ModalSize::Wide guard=true>
                {move || {
                    editing
                        .get()
                        .map(|s| {
                            view! { <McpForm s=s open=open on_saved=reload/> }.into_any()
                        })
                }}
            </Modal>
        </PageFrame>
    }
}

#[component]
fn ServersSection(
    servers: ServersData,
    inv: InvData,
    filter: InvFilter,
    editing: RwSignal<Option<McpServerView>>,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let live = use_live();
    let count = Signal::derive(move || {
        servers.rows.with(|r| {
            r.as_ref()
                .map(|r| grouped(r.len() as u64))
                .unwrap_or_default()
        })
    });
    // Folded, the head still says how they are doing.
    let summary = Signal::derive(move || {
        let statuses: Vec<String> = servers.rows.with(|rows| {
            rows.iter()
                .flatten()
                .map(|s| {
                    live.mcp
                        .with(|m| {
                            m.as_ref()
                                .and_then(|m| m.iter().find(|v| v.id == s.id))
                                .map(|v| v.status.clone())
                        })
                        .unwrap_or_else(|| s.status.clone())
                })
                .collect()
        });
        let mut parts: Vec<(String, usize)> = Vec::new();
        for st in statuses {
            match parts.iter_mut().find(|(s, _)| *s == st) {
                Some((_, n)) => *n += 1,
                None => parts.push((st, 1)),
            }
        }
        parts
            .into_iter()
            .map(|(s, n)| format!("{n} {s}"))
            .collect::<Vec<_>>()
            .join(" · ")
    });
    view! {
        <Section title="Servers" count=count summary=summary persist="mcp.servers">
            {move || {
                servers
                    .error
                    .get()
                    .map(|e| {
                        view! {
                            <div class="notice err row">
                                "Loading the servers failed: "
                                {e}
                                <button class="btn ghost sm" on:click=move |_| servers.load()>
                                    "Retry"
                                </button>
                            </div>
                        }
                    })
            }}
            {move || match servers.rows.get() {
                None => {
                    servers
                        .error
                        .with(Option::is_none)
                        .then(|| view! { <div class="card dim">"Loading…"</div> })
                        .into_any()
                }
                Some(rows) if rows.is_empty() => {
                    view! {
                        <div class="card empty">
                            "No MCP servers configured — Add server wires one in, and its tools appear on /mcp."
                        </div>
                    }
                        .into_any()
                }
                Some(rows) => {
                    view! {
                        <div class="card pad0">
                            <table class="data mcp-servers">
                                <thead>
                                    <tr>
                                        <th>"Server"</th>
                                        <th>"Status"</th>
                                        <th class="col-p3">"Transport"</th>
                                        <th>"Endpoint"</th>
                                        <th class="col-p2">"Prefix"</th>
                                        <th></th>
                                    </tr>
                                </thead>
                                <tbody>
                                    <For each=move || rows.clone() key=|s| format!("{s:?}") let:s>
                                        <McpRow
                                            s=s
                                            inv=inv
                                            filter=filter
                                            editing=editing
                                            on_change=on_change
                                        />
                                    </For>
                                </tbody>
                            </table>
                        </div>
                    }
                        .into_any()
                }
            }}
        </Section>
    }
}

#[component]
fn StatusChip(id: i64, initial: String, initial_count: i64, inv: InvData) -> impl IntoView {
    let live = use_live();
    let state = Memo::new(move |_| {
        live.mcp
            .get()
            .and_then(|views| {
                views
                    .into_iter()
                    .find(|v| v.id == id)
                    .map(|v| (v.status, v.tool_count.unwrap_or(0), v.detail))
            })
            .unwrap_or_else(|| (initial.clone(), initial_count, None))
    });
    // What the server lists is not what the gateway offers: tools switched
    // off here are subtracted, so this agrees with the inventory below.
    let offered = Memo::new(move |_| {
        inv.inv.with(|i| {
            i.as_ref().map(|i| {
                i.tools
                    .iter()
                    .filter(|t| t.server_id == Some(id) && t.available)
                    .count()
            })
        })
    });
    view! {
        <span
            class=move || match state.get().0.as_str() {
                "ready" => "chip ok",
                "connecting" => "chip live",
                "error" => "chip err",
                _ => "chip off",
            }
            title=move || state.get().2.unwrap_or_default()
        >
            <span class="dot"></span>
            {move || {
                let (status, count, _) = state.get();
                match (status.as_str(), offered.get()) {
                    ("ready", Some(n)) => format!("ready · {n}/{count} offered"),
                    ("ready", None) => format!("ready · {count} tools"),
                    _ => status,
                }
            }}
        </span>
    }
}

#[component]
fn McpRow(
    s: McpServerView,
    inv: InvData,
    filter: InvFilter,
    editing: RwSignal<Option<McpServerView>>,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let toasts = use_toasts();
    let id = s.id;
    let enabled = s.enabled;
    let testing = RwSignal::new(false);
    let test = move |_| {
        if testing.get_untracked() {
            return;
        }
        testing.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>(
                "/api/op/mcp_server_set",
                &json!({ "action": "test", "id": id }),
            )
            .await;
            testing.set(false);
            match res {
                Ok(v) => toasts.ok(v
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("connected")
                    .to_string()),
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let endpoint = if s.transport == "stdio" {
        match &s.container_image {
            Some(img) if !img.is_empty() => format!("podman: {img}"),
            _ => {
                let args = s.args.split_whitespace().collect::<Vec<_>>().join(" ");
                format!("{} {args}", s.command.clone().unwrap_or_default())
                    .trim()
                    .to_string()
            }
        }
    } else {
        s.url.clone().unwrap_or_default()
    };
    let edit_s = StoredValue::new(s.clone());
    let picked = move || filter.server_id() == Some(id);
    let items = Signal::derive(move || {
        let toggle = if enabled { "disable" } else { "enable" };
        vec![
            MenuItem::new("Edit", move || editing.set(Some(edit_s.get_value()))),
            MenuItem::new(if enabled { "Disable" } else { "Enable" }, move || {
                spawn_local(async move {
                    match crate::api::post::<Value, _>(
                        "/api/op/mcp_server_set",
                        &json!({ "action": toggle, "id": id }),
                    )
                    .await
                    {
                        Ok(_) => on_change(),
                        Err(e) => toasts.err(e.to_string()),
                    }
                });
            })
            .title(if enabled {
                "Disconnect it and stop offering its tools; the configuration stays"
            } else {
                "Offer its tools again (it connects on first use, or now if autostart is on)"
            }),
        ]
    });
    // A click on the row (not on one of its controls) narrows the inventory
    // below to this server's tools; a second click lets go.
    let pick = move |ev: leptos::ev::MouseEvent| {
        let on_control = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|el| el.closest("button, a, input, [popover]").ok().flatten())
            .is_some();
        if on_control {
            return;
        }
        filter.server.set(if picked() {
            String::new()
        } else {
            id.to_string()
        });
    };
    view! {
        <tr
            class="pickable"
            class:picked=picked
            class:muted=!enabled
            title="Click to list only this server's tools below"
            on:click=pick
        >
            <td>
                <button
                    class="link-btn"
                    title="Edit this server"
                    on:click=move |_| editing.set(Some(edit_s.get_value()))
                >
                    {s.name.clone()}
                </button>
            </td>
            <td>
                <StatusChip id=id initial=s.status.clone() initial_count=s.tool_count inv=inv/>
            </td>
            <td class="col-p3">
                <span class="type-badge">{s.transport.clone()}</span>
            </td>
            <td class="clip dim mono-sm" title=endpoint.clone()>
                {endpoint.clone()}
            </td>
            <td class="col-p2 dim mono-sm">
                {(!s.tool_prefix.is_empty()).then(|| format!("{}__", s.tool_prefix))}
            </td>
            <td class="actions">
                <button
                    class="btn sm"
                    disabled=move || testing.get()
                    title="Connect now and list its tools"
                    on:click=test
                >
                    {move || if testing.get() { "Testing…" } else { "Test" }}
                </button>
                <RowMenu items=items/>
            </td>
        </tr>
    }
}

/// Everything the gateway serves northbound, per tool, with the owner's switch.
///
/// The servers table above is the *config* plane — which upstreams exist. This
/// is the surface those add up to, and it is the only place the built-in
/// toolsets (`lmgw__*`, `docs__*`) appear at all: they were being served to
/// every connected agent with nothing in the dashboard that even named them.
#[component]
fn Inventory(inv: InvData, filter: InvFilter, servers: ServersData) -> impl IntoView {
    let words = Memo::new(move |_| filter_words(&filter.query.get()));
    let narrowed = Signal::derive(move || filter.narrowed());
    let tools = Memo::new(move |_| {
        inv.inv
            .with(|i| i.as_ref().map(|i| i.tools.clone()).unwrap_or_default())
    });
    let total = Signal::derive(move || tools.with(Vec::len));
    let shown = Signal::derive(move || {
        let w = words.get();
        tools.with(|ts| ts.iter().filter(|t| filter.keeps(t, &w)).count())
    });
    let facets = Signal::derive(move || {
        tools.with(|ts| {
            [
                ("offered", "Offered"),
                ("off", "Off"),
                ("unavailable", "Unavailable"),
                ("stale", "Stale"),
            ]
            .into_iter()
            .map(|(id, label)| Facet {
                id: id.into(),
                label: label.into(),
                count: ts.iter().filter(|t| tool_facet(t, id)).count(),
            })
            .collect::<Vec<_>>()
        })
    });
    let chips = Signal::derive(move || {
        let Some(id) = filter.server_id() else {
            return Vec::new();
        };
        let name = servers
            .rows
            .with(|r| {
                r.iter()
                    .flatten()
                    .find(|s| s.id == id)
                    .map(|s| s.name.clone())
            })
            .unwrap_or_else(|| format!("#{id}"));
        vec![(
            format!("server: {name}"),
            Callback::new(move |()| filter.server.set(String::new())),
        )]
    });
    let head = Signal::derive(move || {
        tools.with(|ts| {
            let n = |f: &str| ts.iter().filter(|t| tool_facet(t, f)).count();
            format!(
                "{} offered · {} off",
                grouped(n("offered") as u64),
                grouped(n("off") as u64)
            )
        })
    });
    let bands = Memo::new(move |_| {
        inv.inv.with(|i| {
            let Some(i) = i else {
                return Vec::new();
            };
            let mut v: Vec<(Band, Option<ToolSourceView>)> = i
                .sources
                .iter()
                .map(|s| (Band::Source(s.label.clone(), s.server_id), Some(s.clone())))
                .collect();
            if i.tools.iter().any(|t| t.stale) {
                v.push((Band::Stale, None));
            }
            v
        })
    });

    view! {
        <div class="inv-head">
            <span class="sec-title">"Tool inventory"</span>
            <span class="count">{move || grouped(total.get() as u64)}</span>
            <span class="dim">{move || head.get()}</span>
            <span class="dim inv-note">
                "a tool switched off is neither listed nor callable · applies now"
            </span>
        </div>
        <FilterBar
            query=filter.query
            placeholder="Filter tools by name or description"
            shown=shown
            total=total
            noun="tools"
            facets=FacetSet {
                items: facets,
                active: filter.facet,
            }
            chips=chips
            on_clear=Callback::new(move |()| filter.server.set(String::new()))
        />
        <div class="fill-pane card pad0 inv-pane">
            <table class="data inv-table">
                <thead>
                    <tr>
                        <th class="pick" title="Offered to clients">"On"</th>
                        <th>"Tool"</th>
                        <th>"Description"</th>
                        <th>"State"</th>
                        <th>"Why"</th>
                    </tr>
                </thead>
                <tbody>
                    {move || {
                        inv.error
                            .get()
                            .map(|e| {
                                view! {
                                    <tr>
                                        <td colspan=INV_COLS class="wrap">
                                            <span class="notice err row">
                                                "Loading the tools failed: "
                                                {e}
                                                <button class="btn ghost sm" on:click=move |_| inv.load()>
                                                    "Retry"
                                                </button>
                                            </span>
                                        </td>
                                    </tr>
                                }
                            })
                    }}
                    <Show when=move || inv.inv.with(Option::is_none) && inv.error.with(Option::is_none)>
                        <tr>
                            <td colspan=INV_COLS class="dim">"Loading…"</td>
                        </tr>
                    </Show>
                    <Show when=move || narrowed.get() && shown.get() == 0 && inv.inv.with(Option::is_some)>
                        <tr>
                            <td colspan=INV_COLS class="dim">"No tool matches the filter."</td>
                        </tr>
                    </Show>
                    <For each=move || bands.get() key=|(b, s)| format!("{b:?}{s:?}") let:band>
                        <ToolBand band=band.0 source=band.1 inv=inv filter=filter words=words/>
                    </For>
                </tbody>
            </table>
        </div>
    }
}

/// One source's header row plus its tools. A source with no tools keeps its
/// header — that row is where "the server is disabled" gets said.
#[component]
fn ToolBand(
    band: Band,
    source: Option<ToolSourceView>,
    inv: InvData,
    filter: InvFilter,
    words: Memo<Vec<String>>,
) -> impl IntoView {
    let toasts = use_toasts();
    let open = crate::prefs::persisted_bool(&band.persist_key(), false);
    let band = StoredValue::new(band);
    let all = Memo::new(move |_| {
        inv.inv.with(|i| {
            i.as_ref()
                .map(|i| {
                    band.with_value(|b| {
                        i.tools
                            .iter()
                            .filter(|t| b.holds(t))
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                })
                .unwrap_or_default()
        })
    });
    let shown = Memo::new(move |_| {
        let w = words.get();
        all.with(|ts| {
            ts.iter()
                .filter(|t| filter.keeps(t, &w))
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    let narrowed = Signal::derive(move || filter.narrowed());
    let server_id = source.as_ref().and_then(|s| s.server_id);
    // Filtered to nothing, a band steps aside — unless it is the server the
    // owner just picked, whose empty band is where it says why.
    let visible = move || {
        !narrowed.get()
            || shown.with(|s| !s.is_empty())
            || (server_id.is_some() && filter.server_id() == server_id)
    };
    let source_reason = StoredValue::new(source.as_ref().and_then(|s| s.reason.clone()));
    let (label, what) = match &source {
        Some(s) => (s.name.clone(), format!("{} · {}", s.kind, s.plane)),
        None => (
            "Stale".to_string(),
            "switched off here, but no longer offered by anything".to_string(),
        ),
    };
    let band_name = StoredValue::new(label.clone());
    let count = Signal::derive(move || shown.with(|s| of(s.len(), all.with(Vec::len))));
    let meta = {
        let reason = source_reason.get_value();
        move || {
            let what = what.clone();
            let reason = reason.clone();
            move || {
                let (on, off, unavailable) = all.with(|ts| {
                    (
                        ts.iter().filter(|t| t.available).count(),
                        ts.iter().filter(|t| !t.enabled).count(),
                        ts.iter().filter(|t| t.enabled && !t.available).count(),
                    )
                });
                let mut parts = vec![format!("{on} on"), format!("{off} off")];
                if unavailable > 0 {
                    parts.push(format!("{unavailable} unavailable"));
                }
                parts.push(what.clone());
                if let Some(r) = &reason {
                    parts.push(r.clone());
                }
                let line = parts.join(" · ");
                let tip = line.clone();
                view! { <span title=tip>{line}</span> }
            }
        }
    };
    let progress = RwSignal::new(None::<(usize, usize, bool)>);
    // Enable/Disable all act on what the band shows: all of it, or what a
    // filter left of it — and say which.
    let targets = move |enable: bool| {
        shown.with(|ts| {
            ts.iter()
                .filter(|t| t.enabled != enable)
                .map(|t| t.name.clone())
                .collect::<Vec<_>>()
        })
    };
    let actions = move || {
        move || {
            if band.with_value(|b| *b == Band::Stale) {
                return ().into_any();
            }
            if let Some((done, total, enable)) = progress.get() {
                let verb = if enable { "Enabling" } else { "Disabling" };
                return view! { <span class="bulk-progress">{format!("{verb} {done}/{total}…")}</span> }
                    .into_any();
            }
            let whole = shown.with(Vec::len) == all.with(Vec::len);
            let button = move |enable: bool| {
                let n = targets(enable).len();
                let (verb, state) = if enable {
                    ("Enable", "off")
                } else {
                    ("Disable", "on")
                };
                let label = if whole {
                    format!("{verb} all")
                } else {
                    format!("{verb} {n} shown")
                };
                let title = if n == 0 {
                    format!("Nothing here is {state}")
                } else {
                    format!(
                        "Switch {} the {n} tool{} that {} {state} — applies now",
                        if enable { "on" } else { "off" },
                        if n == 1 { "" } else { "s" },
                        if n == 1 { "is" } else { "are" },
                    )
                };
                view! {
                    <button
                        class="btn ghost sm"
                        disabled=n == 0
                        title=title
                        on:click=move |_| {
                            switch_all(
                                targets(enable),
                                enable,
                                band_name.get_value(),
                                progress,
                                toasts,
                                inv,
                            )
                        }
                    >
                        {label}
                    </button>
                }
            };
            view! { {button(true)} {button(false)} }.into_any()
        }
    };

    view! {
        <Show when=visible>
            <GroupRow
                colspan=INV_COLS
                label=label.clone()
                count=count
                open=open
                meta=meta.clone()
                actions=actions
            />
            <Show when=move || open.get() || narrowed.get()>
                <For
                    each=move || shown.get()
                    key=|t| {
                        (
                            t.name.clone(),
                            t.enabled,
                            t.available,
                            t.stale,
                            t.reason.clone(),
                            t.description.clone(),
                        )
                    }
                    let:t
                >
                    <ToolRow t=t source_reason=source_reason.get_value() inv=inv/>
                </For>
                <Show when=move || all.with(Vec::is_empty)>
                    <tr>
                        <td colspan=INV_COLS class="dim">"— no tools right now"</td>
                    </tr>
                </Show>
            </Show>
        </Show>
    }
}

/// One tool: the owner's switch (it applies on the click), its description,
/// its state, and a reason only when it is not the obvious one — a tool the
/// owner switched off says "off", and its band already says why its source
/// is down.
#[component]
fn ToolRow(t: ToolEntryView, source_reason: Option<String>, inv: InvData) -> impl IntoView {
    let toasts = use_toasts();
    let name = StoredValue::new(t.name.clone());
    let (enabled, available, stale) = (t.enabled, t.available, t.stale);
    let busy = RwSignal::new(false);
    let toggle = move |ev: leptos::ev::Event| {
        let want = event_target_checked(&ev);
        if busy.get_untracked() {
            return;
        }
        let input = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok());
        busy.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>(
                "/api/op/tool_set",
                &json!({ "name": name.get_value(), "enabled": want }),
            )
            .await;
            busy.set(false);
            if let Err(e) = res {
                // The row is not rebuilt when nothing changed, so the tick
                // goes back by hand.
                if let Some(el) = input {
                    el.set_checked(enabled);
                }
                toasts.err(e.to_string());
            }
            inv.load();
        });
    };
    let (chip, label) = if stale {
        ("chip warn", "stale")
    } else if available {
        ("chip ok", "offered")
    } else if !enabled {
        ("chip off", "off")
    } else {
        ("chip off", "unavailable")
    };
    let why = t
        .reason
        .clone()
        .filter(|_| enabled || stale)
        .filter(|r| Some(r) != source_reason.as_ref())
        .unwrap_or_default();
    // The source's prefix is dimmed: within a band it is the same on every row.
    let prefix = format!("{}__", t.source_label);
    let (pfx, base) = match t.name.strip_prefix(&prefix) {
        Some(rest) => (prefix.clone(), rest.to_string()),
        None => (String::new(), t.name.clone()),
    };
    let desc = t.description.clone().unwrap_or_default();
    let tick_title = if enabled {
        "Offered — untick to switch it off here (applies now)"
    } else {
        "Switched off here — tick to offer it again (applies now)"
    };
    view! {
        <tr class:muted=!enabled>
            <td class="pick">
                <input
                    type="checkbox"
                    prop:checked=enabled
                    disabled=move || busy.get()
                    title=tick_title
                    on:change=toggle
                />
            </td>
            <td class="mono-sm" title=t.name.clone()>
                <span class="pfx">{pfx}</span>
                {base}
            </td>
            <td class="clip dim" title=desc.clone()>{desc.clone()}</td>
            <td>
                <span class=chip title=t.reason.clone().unwrap_or_default()>
                    <span class="dot"></span>
                    {label}
                </span>
            </td>
            <td class="dim" title=why.clone()>{why.clone()}</td>
        </tr>
    }
}

#[component]
fn McpForm(
    s: McpServerView,
    open: RwSignal<bool>,
    on_saved: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let toasts = use_toasts();
    let create = s.id == 0;
    let id = s.id;
    let name = RwSignal::new(s.name.clone());
    let transport = RwSignal::new(s.transport.clone());
    let command = RwSignal::new(s.command.clone().unwrap_or_default());
    let args = RwSignal::new(s.args.clone());
    let env = RwSignal::new(s.env.clone());
    let cwd = RwSignal::new(s.cwd.clone().unwrap_or_default());
    let image = RwSignal::new(s.container_image.clone().unwrap_or_default());
    let run_args = RwSignal::new(s.extra_run_args.clone());
    let url = RwSignal::new(s.url.clone().unwrap_or_default());
    let headers = RwSignal::new(s.headers.clone());
    let prefix = RwSignal::new(s.tool_prefix.clone());
    let timeout = RwSignal::new(s.timeout_ms.to_string());
    let idle = RwSignal::new(s.idle_seconds.to_string());
    let sampling_alias = RwSignal::new(s.sampling_alias.clone().unwrap_or_default());
    let autostart = RwSignal::new(s.autostart);
    let allow_sampling = RwSignal::new(s.allow_sampling);
    let enabled = RwSignal::new(s.enabled);
    let saving = RwSignal::new(false);
    let deleting = RwSignal::new(false);
    let saved_name = StoredValue::new(s.name.clone());
    let delete = move || {
        if deleting.get_untracked() {
            return;
        }
        deleting.set(true);
        // Read now: the dialog can be gone by the time the answer lands.
        let name = saved_name.get_value();
        spawn_local(async move {
            let res = crate::api::post::<Value, _>(
                "/api/op/mcp_server_set",
                &json!({ "action": "delete", "id": id }),
            )
            .await;
            deleting.set(false);
            match res {
                Ok(_) => {
                    toasts.ok(format!("{name} deleted"));
                    open.set(false);
                    on_saved();
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let transport_opts = Signal::derive(|| {
        [
            ("stdio", "stdio (subprocess)"),
            ("http", "streamable http"),
            ("sse", "sse"),
        ]
        .into_iter()
        .map(|(v, l)| (v.to_string(), l.to_string()))
        .collect::<Vec<_>>()
    });

    let save = move |_| {
        if saving.get_untracked() {
            return;
        }
        let (timeout_v, idle_v): (u64, i64) = match (
            timeout.get_untracked().trim().parse(),
            idle.get_untracked().trim().parse(),
        ) {
            (Ok(t), Ok(i)) => (t, i),
            _ => {
                toasts.err("timeout and idle seconds must be numbers");
                return;
            }
        };
        let body = json!({
            "action": if create { "create" } else { "update" },
            "id": if create { Value::Null } else { json!(id) },
            "name": name.get_untracked().trim(),
            "transport": transport.get_untracked(),
            "command": command.get_untracked().trim(),
            "args": args.get_untracked(),
            "env": env.get_untracked(),
            "cwd": cwd.get_untracked().trim(),
            "container_image": image.get_untracked().trim(),
            "extra_run_args": run_args.get_untracked(),
            "url": url.get_untracked().trim(),
            "headers": headers.get_untracked(),
            "tool_prefix": prefix.get_untracked().trim(),
            "timeout_ms": timeout_v,
            "idle_seconds": idle_v,
            "sampling_alias": sampling_alias.get_untracked().trim(),
            "autostart": autostart.get_untracked(),
            "allow_sampling": allow_sampling.get_untracked(),
            "enabled": enabled.get_untracked(),
        });
        saving.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>("/api/op/mcp_server_set", &body).await;
            saving.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("saved")
                        .to_string());
                    open.set(false);
                    on_saved();
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    // A list box is as tall as its lines and one more to type into: three
    // empty 70px boxes pushed the ticks below the fold of a half-empty dialog.
    let rows =
        |text: RwSignal<String>| move || (text.with(|t| t.lines().count()) + 1).max(2).to_string();
    let stdio = move || transport.get() == "stdio";

    // Wide, in sections that flow into columns (review ux:U-13): where the
    // server is, what it is started or reached with, how it behaves.
    view! {
        <div class="card-flow editor-flow">
            <section class="card edit-section">
                <h3>"Server"</h3>
                <div class="field-grid">
                    <div class="field">
                        <label>"Name"</label>
                        <input class="input" prop:value=move || name.get()
                            on:input=move |ev| name.set(event_target_value(&ev))/>
                    </div>
                    <div class="field">
                        <label>"Transport"</label>
                        <Select value=transport options=transport_opts/>
                    </div>
                    <Show when=stdio>
                        <div class="field wide">
                            <label>"Container image" <span class="field-unit">"empty runs the bare command"</span></label>
                            <input class="input mono" prop:value=move || image.get()
                                on:input=move |ev| image.set(event_target_value(&ev))/>
                        </div>
                        <div class="field">
                            <label>"Command"</label>
                            <input class="input mono" prop:value=move || command.get()
                                on:input=move |ev| command.set(event_target_value(&ev))/>
                        </div>
                        <div class="field">
                            <label>"Working directory"</label>
                            <input class="input mono" prop:value=move || cwd.get()
                                on:input=move |ev| cwd.set(event_target_value(&ev))/>
                        </div>
                    </Show>
                    <Show when=move || !stdio()>
                        <div class="field wide">
                            <label>"URL"</label>
                            <input class="input mono" prop:value=move || url.get()
                                on:input=move |ev| url.set(event_target_value(&ev))/>
                        </div>
                    </Show>
                </div>
            </section>
            <section class="card edit-section">
                <h3>{move || if stdio() { "Process" } else { "Request" }}</h3>
                <Show when=stdio>
                    <div class="field">
                        <label>"Arguments" <span class="field-unit">"one per line"</span></label>
                        <textarea class="input mono ta set-ta" rows=rows(args) prop:value=move || args.get()
                            on:input=move |ev| args.set(event_target_value(&ev))></textarea>
                    </div>
                    <div class="field">
                        <label>"Environment" <span class="field-unit">"KEY=VALUE per line"</span></label>
                        <textarea class="input mono ta set-ta" rows=rows(env) prop:value=move || env.get()
                            on:input=move |ev| env.set(event_target_value(&ev))></textarea>
                    </div>
                    <div class="field">
                        <label>"Extra podman run args" <span class="field-unit">"one per line"</span></label>
                        <textarea class="input mono ta set-ta" rows=rows(run_args) prop:value=move || run_args.get()
                            on:input=move |ev| run_args.set(event_target_value(&ev))></textarea>
                    </div>
                </Show>
                <Show when=move || !stdio()>
                    <div class="field">
                        <label>"Headers" <span class="field-unit">"Name: Value per line; stored values show as <set>"</span></label>
                        <textarea class="input mono ta set-ta" rows=rows(headers) prop:value=move || headers.get()
                            on:input=move |ev| headers.set(event_target_value(&ev))></textarea>
                    </div>
                </Show>
            </section>
            <section class="card edit-section">
                <h3>"Behaviour"</h3>
                <div class="field-grid fg-s">
                    <div class="field">
                        <label>"Tool prefix"</label>
                        <input class="input mono" prop:value=move || prefix.get()
                            on:input=move |ev| prefix.set(event_target_value(&ev))/>
                    </div>
                    <div class="field">
                        <label>"Timeout" <span class="field-unit">"ms"</span></label>
                        <input class="input mono" prop:value=move || timeout.get()
                            on:input=move |ev| timeout.set(event_target_value(&ev))/>
                    </div>
                    <div class="field">
                        <label>"Idle" <span class="field-unit">"s · 0 = never reap"</span></label>
                        <input class="input mono" prop:value=move || idle.get()
                            on:input=move |ev| idle.set(event_target_value(&ev))/>
                    </div>
                </div>
                // The same choice Settings → Agents & tools makes for every
                // server, so the same picker: a typo here was a sampling
                // route that silently went nowhere.
                <div class="field">
                    <label>"Sampling alias" <span class="field-unit">"a small dedicated model"</span></label>
                    <ModelPicker
                        value=sampling_alias
                        tasks=&["chat"]
                        empty_label="The global one (Settings → Agents & tools)"
                        recent_key="mcp-sampling"
                    />
                </div>
                <div class="row" style="margin-top:10px">
                    <label class="row dim" style="gap:5px">
                        <input type="checkbox" prop:checked=move || autostart.get()
                            on:change=move |ev| autostart.set(event_target_checked(&ev))/>
                        "autostart"
                    </label>
                    <label class="row dim" style="gap:5px">
                        <input type="checkbox" prop:checked=move || allow_sampling.get()
                            on:change=move |ev| allow_sampling.set(event_target_checked(&ev))/>
                        "allow sampling"
                    </label>
                    <label class="row dim" style="gap:5px">
                        <input type="checkbox" prop:checked=move || enabled.get()
                            on:change=move |ev| enabled.set(event_target_checked(&ev))/>
                        "enabled"
                    </label>
                </div>
            </section>
        </div>
        <ModalFooter>
            // Away from Save, on the far left, and two clicks: its tools
            // leave /mcp with it.
            {(!create)
                .then(|| {
                    view! {
                        <span class="foot-danger">
                            <ConfirmButton
                                label="Delete server"
                                confirm=format!("Delete {}?", saved_name.get_value())
                                on_confirm=Callback::new(move |()| delete())
                                class="btn ghost"
                                disabled=deleting
                                title="Remove it from the gateway; its tools leave /mcp"
                            />
                        </span>
                    }
                })}
            <button class="btn ghost" on:click=move |_| open.set(false)>
                "Cancel"
            </button>
            <button class="btn primary" disabled=move || saving.get() on:click=save>
                {move || if saving.get() { "Saving…" } else { "Save server" }}
            </button>
        </ModalFooter>
    }
}
