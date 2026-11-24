//! The list pane: the title with how the servers are doing (live), Add,
//! the search (`?find=`, matched against the name, the transport and the
//! endpoint), and two groups — the configured servers, each with its live
//! state and a line of facts (transport, endpoint, tools offered, open
//! tasks), and the built-in toolsets.

use leptos::prelude::*;
use lmgw_api_types::McpServerView;

use super::{Page, Sel};
use crate::fmt::count_of;
use crate::pages::collection::{matches, CollectionPane, Item, Line};
use crate::widgets::list_pane::ListRefs;
use crate::widgets::{filter_words, Dot, Ico, Icon};

/// What a server is reached at, in one line.
pub(super) fn endpoint(s: &McpServerView) -> String {
    match s.transport.as_str() {
        // A paired device's hosted tools (client-apps design §5.2): it dials
        // lmgw, so there is no address to show — its device is.
        "device" => format!(
            "device '{}' · its own link",
            s.device.clone().unwrap_or_default()
        ),
        "stdio" => match &s.container_image {
            Some(img) if !img.is_empty() => format!("podman: {img}"),
            _ => {
                let args = s.args.split_whitespace().collect::<Vec<_>>().join(" ");
                format!("{} {args}", s.command.clone().unwrap_or_default())
                    .trim()
                    .to_string()
            }
        },
        _ => s.url.clone().unwrap_or_default(),
    }
}

/// A status as the list and the hero say it: the dot and its word.
pub(super) fn state_of(enabled: bool, status: &str) -> (Dot, String) {
    if !enabled {
        return (Dot::Off, "Disabled".into());
    }
    match status {
        "ready" => (Dot::Running, "Ready".into()),
        "connecting" => (Dot::Busy, "Connecting".into()),
        "error" => (Dot::Error, "Error".into()),
        "stopped" => (Dot::Stopped, "Stopped".into()),
        other => (Dot::Stopped, other.to_string()),
    }
}

#[component]
pub(super) fn McpList(
    page: Page,
    sel: Memo<Option<String>>,
    narrow: Memo<bool>,
    refs: ListRefs<String>,
    open_row: Callback<(String, bool)>,
) -> impl IntoView {
    let query = crate::url_state::use_query_signal("find");
    let lines = Memo::new(move |_| {
        let words = filter_words(&query.get());
        let mut out = Vec::new();
        let servers: Vec<Item> = page.servers.rows.with(|rows| {
            rows.iter()
                .flatten()
                .filter(|s| {
                    matches(
                        &format!(
                            "{} {} {} {}",
                            s.name,
                            s.transport,
                            endpoint(s),
                            s.tool_prefix
                        ),
                        &words,
                    )
                })
                .map(|s| {
                    let (status, count, _) = page.status(s);
                    let (dot, word) = state_of(s.enabled, &status);
                    let mut meta = vec![s.transport.clone()];
                    let e = endpoint(s);
                    if !e.is_empty() {
                        meta.push(e);
                    }
                    if s.enabled && status == "ready" {
                        meta.push(match page.offered(s.id) {
                            Some(n) => format!("{n}/{count} tools offered"),
                            None => count_of(count.max(0) as usize, "tools"),
                        });
                    }
                    if s.open_tasks > 0 {
                        meta.push(count_of(s.open_tasks as usize, "open tasks"));
                    }
                    Item {
                        key: Sel::Server(s.id).path(),
                        name: s.name.clone(),
                        meta: meta.join(" · "),
                        dot,
                        state: word,
                        off: !s.enabled,
                        icon: if s.transport == "device" {
                            Ico::Chip
                        } else {
                            Ico::Wrench
                        },
                    }
                })
                .collect()
        });
        if !servers.is_empty() {
            out.push(Line::Head("Servers".into(), servers.len()));
            out.extend(servers.into_iter().map(Line::Row));
        }
        let builtins: Vec<Item> = page.inv.inv.with(|i| {
            i.iter()
                .flat_map(|i| i.sources.iter())
                .filter(|s| s.kind == "builtin")
                .filter(|s| matches(&format!("{} {} {}", s.name, s.label, s.plane), &words))
                .map(|s| {
                    let on = (s.tool_count - s.disabled_count).max(0);
                    Item {
                        key: Sel::Builtin(s.label.clone()).path(),
                        name: s.name.clone(),
                        meta: format!(
                            "{}__ · {} · {on}/{} tools on",
                            s.label, s.plane, s.tool_count
                        ),
                        dot: if s.available { Dot::Running } else { Dot::Off },
                        state: if s.available { "Offered" } else { "Off" }.into(),
                        off: !s.available,
                        icon: if s.label == "docs" {
                            Ico::Book
                        } else {
                            Ico::Sliders
                        },
                    }
                })
                .collect()
        });
        if !builtins.is_empty() {
            out.push(Line::Head("Built in".into(), builtins.len()));
            out.extend(builtins.into_iter().map(Line::Row));
        }
        out
    });
    // Folded into the title's line: how they are doing.
    let sub = Signal::derive(move || {
        page.servers.rows.with(|rows| {
            let Some(rows) = rows else {
                return if page.servers.error.with(Option::is_some) {
                    "not loaded".to_string()
                } else {
                    "Loading…".to_string()
                };
            };
            let mut parts: Vec<(String, usize)> = Vec::new();
            for s in rows {
                let st = if s.enabled {
                    page.status(s).0
                } else {
                    "disabled".into()
                };
                match parts.iter_mut().find(|(k, _)| *k == st) {
                    Some((_, n)) => *n += 1,
                    None => parts.push((st, 1)),
                }
            }
            let mut out = vec![count_of(rows.len(), "servers")];
            out.extend(parts.into_iter().map(|(s, n)| format!("{n} {s}")));
            out.join(" · ")
        })
    });
    view! {
        <CollectionPane
            title="MCP servers"
            sub=sub
            label="MCP servers"
            placeholder="Search servers"
            query=query
            lines=lines
            loaded=Signal::derive(move || page.servers.rows.with(Option::is_some))
            error=page.servers.error
            retry=Callback::new(move |()| page.reload())
            empty="No MCP servers yet: + wires one in, and its tools appear on /mcp"
            sel=sel
            narrow=narrow
            refs=refs
            open_row=open_row
            actions=ViewFn::from(move || {
                view! {
                    <button
                        type="button"
                        class="icon-btn"
                        title="Add server"
                        aria-label="Add server"
                        on:click=move |_| open_row.run((Sel::New.path(), false))
                    >
                        <Icon ico=Ico::Plus/>
                    </button>
                }
            })
        />
    }
}
