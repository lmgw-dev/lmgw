//! The gateway's **northbound tool inventory**: every tool lmgw can serve, the
//! source it comes from, and whether it is offered right now — plus the owner's
//! per-tool kill switch.
//!
//! ## Why one place
//!
//! Before this, "what does this gateway offer" had no answer anywhere. The MCP
//! page listed *servers*; the built-in toolsets (`lmgw__*`, `docs__*`) were
//! served northbound but appeared in no UI at all, so `docs__query` was reaching
//! every agent with nothing that even named it. Three planes each computed their
//! own view of the surface ([`ingress`](super::ingress) for `/mcp` and
//! `/mcp/admin`, [`exec`](super::exec) for `/v1/responses` and Chat), which is
//! exactly the shape a switch drifts out of.
//!
//! So: one enumeration ([`list`]) that every UI reads, and one predicate
//! ([`Snapshot::tool_disabled`](crate::config::Snapshot::tool_disabled)) that
//! every serving path consults.
//!
//! ## Composition
//!
//! A tool is offered **iff its source is enabled and it is not owner-disabled**.
//! The two conditions are reported separately because they are fixed in
//! different places: a source condition is a server toggle or the `self_admin`
//! Setting, an owner-disable is this table. Hiding is never the whole story —
//! every plane also refuses a disabled tool **by name**, so a client that
//! remembers the name from an earlier `tools/list` gets the reason instead of a
//! silent success.

use serde::Serialize;

use crate::config::{SelfAdmin, Snapshot};
use crate::state::SharedState;

use super::{docs, exec, kb, selfadmin};

/// Display name of the self-admin source, matching the `upstream_name` its
/// calls already log under.
const SELF_ADMIN_SOURCE_NAME: &str = "lmgw (self-admin)";
/// Same, for the quickdoc toolset.
const DOCS_SOURCE_NAME: &str = "lmgw (docs)";
/// Same, for the knowledge-base toolset.
const KB_SOURCE_NAME: &str = "lmgw (knowledge)";

/// `builtin` — one of lmgw's own toolsets; `server` — a registered southbound
/// MCP server.
const KIND_BUILTIN: &str = "builtin";
const KIND_SERVER: &str = "server";

/// One tool the gateway can serve, with its provenance and current state.
#[derive(Debug, Clone, Serialize)]
pub struct ToolEntry {
    /// Fully-qualified exposed name — what a caller actually names, and the key
    /// the disable switch is stored under.
    pub name: String,
    /// The label a thread or a `{"type":"mcp"}` block attaches this tool's
    /// source by (`lmgw`, `docs`, or the server's prefix/name).
    pub source_label: String,
    /// Human name of the source, for a group header.
    pub source_name: String,
    /// [`KIND_BUILTIN`] or [`KIND_SERVER`].
    pub source_kind: String,
    pub server_id: Option<i64>,
    /// Northbound route it is served on: `/mcp` or `/mcp/admin`.
    pub plane: String,
    pub description: Option<String>,
    /// The server's own name for it, when a prefix hides it.
    pub upstream_name: Option<String>,
    /// The name it would have if no other source offered a tool of that
    /// name: set when a collision gave it its server's prefix
    /// ([`super::names`]).
    pub moved_from: Option<String>,
    /// Why it moved, with [`Self::moved_from`]: who else claims that name,
    /// and whether that claim is only what a server listed when it was last
    /// connected.
    pub moved_reason: Option<String>,
    /// The owner's switch. `false` ⇒ a `tool_disabled` row exists.
    pub enabled: bool,
    /// Offered right now — `enabled` **and** the source condition holds.
    pub available: bool,
    /// Why it is not available. `None` exactly when `available`.
    pub reason: Option<String>,
    /// A disable record whose tool the gateway no longer offers: kept visible
    /// here rather than retained as invisible state.
    pub stale: bool,
    pub disabled_at: Option<String>,
}

/// One source of tools, so a source that currently offers *none* (disabled
/// server, self-admin off, a server that has not connected) is still visible
/// with the reason — the per-tool rows cannot carry that, because there are no
/// per-tool rows to carry it.
#[derive(Debug, Clone, Serialize)]
pub struct ToolSource {
    pub label: String,
    pub name: String,
    pub kind: String,
    pub server_id: Option<i64>,
    pub plane: String,
    /// Whether the source itself is in a state where its tools are offered.
    pub available: bool,
    pub reason: Option<String>,
    /// Tools this source contributes to the inventory right now.
    pub tool_count: i64,
    /// How many of those the owner has switched off.
    pub disabled_count: i64,
}

/// The inventory: sources first (group headers), then every tool.
#[derive(Debug, Clone, Serialize)]
pub struct Inventory {
    pub sources: Vec<ToolSource>,
    pub tools: Vec<ToolEntry>,
}

/// Why the `self_admin` mode gate hides a tool, if it does. `writes` selects
/// between the two thresholds the gate has.
fn self_admin_reason(mode: SelfAdmin, writes: bool) -> Option<String> {
    if !mode.allows_read() {
        return Some(
            "the self-admin toolset is switched off (Self-admin tools, under Settings → Network & \
             access)"
                .to_string(),
        );
    }
    if writes && !mode.allows_write() {
        return Some(
            "self-admin is set to 'read only'; this tool changes configuration \
             (Self-admin tools, under Settings → Network & access)"
                .to_string(),
        );
    }
    None
}

/// The owner-disable half of the composition, as a reason string.
fn disabled_reason() -> String {
    "switched off here by the owner".to_string()
}

/// Pull `name` / `description` out of a `tools/list` entry.
fn entry_name(v: &serde_json::Value) -> String {
    v.get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn entry_desc(v: &serde_json::Value) -> Option<String> {
    v.get("description")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// Everything this gateway can serve northbound, right now.
///
/// Listing the southbound half goes through
/// [`McpManager::list_tools`](crate::mcp::McpManager::list_tools), so it
/// **connects an enabled server that is not up yet** — exactly as a northbound
/// `tools/list` and the Chat thread picker already do. That is deliberate: an
/// `autostart = false` server's tools are still offered (a client's first list
/// connects it), and an inventory that omitted them would be a different, more
/// flattering surface than the one callers actually get.
pub async fn list(state: &SharedState) -> Inventory {
    let snap = state.snapshot();
    let mode = snap.settings.self_admin;
    let mut sources: Vec<ToolSource> = Vec::new();
    let mut tools: Vec<ToolEntry> = Vec::new();

    // ---- the self-admin toolset (/mcp/admin, plus Admin Chat) ----
    let admin_tools: Vec<ToolEntry> = selfadmin::full_catalog()
        .iter()
        .map(|(entry, writes)| {
            let name = entry_name(entry);
            build(
                &snap,
                name,
                exec::SELF_ADMIN_LABEL,
                SELF_ADMIN_SOURCE_NAME,
                KIND_BUILTIN,
                None,
                "/mcp/admin",
                entry_desc(entry),
                None,
                self_admin_reason(mode, *writes),
            )
        })
        .collect();
    sources.push(source_of(
        exec::SELF_ADMIN_LABEL,
        SELF_ADMIN_SOURCE_NAME,
        KIND_BUILTIN,
        None,
        "/mcp/admin",
        self_admin_reason(mode, false),
        &admin_tools,
    ));
    tools.extend(admin_tools);

    // ---- the quickdoc toolset (/mcp, next to the servers' tools) ----
    let docs_tools: Vec<ToolEntry> = docs::list()
        .iter()
        .map(|entry| {
            build(
                &snap,
                entry_name(entry),
                exec::DOCS_LABEL,
                DOCS_SOURCE_NAME,
                KIND_BUILTIN,
                None,
                "/mcp",
                entry_desc(entry),
                None,
                None,
            )
        })
        .collect();
    sources.push(source_of(
        exec::DOCS_LABEL,
        DOCS_SOURCE_NAME,
        KIND_BUILTIN,
        None,
        "/mcp",
        None,
        &docs_tools,
    ));
    tools.extend(docs_tools);

    // ---- the knowledge-base toolset (/mcp, and the `kb` label) ----
    let kb_tools: Vec<ToolEntry> = kb::list()
        .iter()
        .map(|entry| {
            build(
                &snap,
                entry_name(entry),
                exec::KB_LABEL,
                KB_SOURCE_NAME,
                KIND_BUILTIN,
                None,
                "/mcp",
                entry_desc(entry),
                None,
                None,
            )
        })
        .collect();
    sources.push(source_of(
        exec::KB_LABEL,
        KB_SOURCE_NAME,
        KIND_BUILTIN,
        None,
        "/mcp",
        None,
        &kb_tools,
    ));
    tools.extend(kb_tools);

    // ---- the registered southbound servers ----
    let agg = state.mcp.list_tools(&snap).await;
    let views = state.mcp.status_views(&snap).await;
    let mut servers: Vec<&crate::config::McpServer> = snap.mcp_servers.values().collect();
    servers.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    for server in servers {
        let label = exec::server_label(server);
        // A disabled server's tools stay listed when a stale connection still
        // has them, so state the server condition rather than letting a tool
        // look available because the process has not been torn down yet.
        let status = views.iter().find(|v| v.id == server.id);
        let reason = if !server.enabled {
            Some(format!("MCP server '{}' is disabled", server.name))
        } else {
            match status.map(|v| v.status) {
                Some("ready") => None,
                Some(other) => Some(match status.and_then(|v| v.detail.clone()) {
                    Some(d) if !d.is_empty() => {
                        format!("MCP server '{}' is {other}: {d}", server.name)
                    }
                    _ => format!("MCP server '{}' is {other}", server.name),
                }),
                None => Some(format!("MCP server '{}' is not connected", server.name)),
            }
        };
        let server_tools: Vec<ToolEntry> = agg
            .tools
            .iter()
            .filter_map(|t| {
                let (sid, upstream) = agg.reverse.get(t.name.as_ref())?;
                (*sid == server.id).then(|| {
                    let mut entry = build(
                        &snap,
                        t.name.to_string(),
                        &label,
                        &server.name,
                        KIND_SERVER,
                        Some(server.id),
                        "/mcp",
                        t.description.as_ref().map(|d| d.to_string()),
                        Some(upstream.clone()),
                        reason.clone(),
                    );
                    moved_from(&snap, &agg, &mut entry);
                    entry
                })
            })
            .collect();
        sources.push(source_of(
            &label,
            &server.name,
            KIND_SERVER,
            Some(server.id),
            "/mcp",
            reason,
            &server_tools,
        ));
        tools.extend(server_tools);
    }

    // ---- disable records with no tool behind them any more ----
    //
    // A switch whose tool is gone (server deleted, upstream renamed it, an
    // lmgw upgrade dropped a built-in) must not sit in the database as state
    // nobody can see: it reappears the moment the name comes back. It is listed
    // as stale, and re-enabling it is what clears the row.
    let mut stale: Vec<ToolEntry> = snap
        .disabled_tools
        .iter()
        .filter(|(name, _)| !tools.iter().any(|t| &&t.name == name))
        .map(|(name, rec)| ToolEntry {
            name: name.clone(),
            source_label: rec.source.clone(),
            source_name: rec.source.clone(),
            source_kind: KIND_SERVER.to_string(),
            server_id: None,
            plane: String::new(),
            description: None,
            upstream_name: None,
            moved_from: None,
            moved_reason: None,
            enabled: false,
            available: false,
            reason: Some(format!(
                "{} — but no tool by this name is offered any more{}",
                disabled_reason(),
                if rec.source.is_empty() {
                    String::new()
                } else {
                    format!(" (it was '{}')", rec.source)
                }
            )),
            stale: true,
            disabled_at: Some(rec.disabled_at.clone()),
        })
        .collect();
    stale.sort_by(|a, b| a.name.cmp(&b.name));
    tools.extend(stale);

    Inventory { sources, tools }
}

/// One tool row, composing the owner switch with its source's condition.
#[allow(clippy::too_many_arguments)]
fn build(
    snap: &Snapshot,
    name: String,
    source_label: &str,
    source_name: &str,
    source_kind: &str,
    server_id: Option<i64>,
    plane: &str,
    description: Option<String>,
    upstream_name: Option<String>,
    source_reason: Option<String>,
) -> ToolEntry {
    let record = snap.disabled_tools.get(&name);
    let enabled = record.is_none();
    // Both halves are reported when both fail: re-enabling a tool whose server
    // is also off would otherwise look like it did nothing.
    let reason = match (&record, source_reason) {
        (Some(_), Some(s)) => Some(format!("{} (and {s})", disabled_reason())),
        (Some(_), None) => Some(disabled_reason()),
        (None, s) => s,
    };
    ToolEntry {
        name,
        source_label: source_label.to_string(),
        source_name: source_name.to_string(),
        source_kind: source_kind.to_string(),
        server_id,
        plane: plane.to_string(),
        description,
        upstream_name,
        moved_from: None,
        moved_reason: None,
        enabled,
        available: reason.is_none(),
        reason,
        stale: false,
        disabled_at: record.map(|r| r.disabled_at.clone()),
    }
}

/// A tool a collision gave its server's prefix ([`super::names`]) says so
/// and who else claims its name, and a switch set under another name of the
/// same tool holds for it — the name it had before the collision, or the one
/// a collision gave it, once the collision ends: it is off, saying which
/// switch turns it on (the stale record of that name).
fn moved_from(snap: &Snapshot, agg: &super::Aggregate, entry: &mut ToolEntry) {
    if let Some(before) = agg.qualified.get(&entry.name) {
        entry.moved_from = Some(before.clone());
        entry.moved_reason = Some(moved_reason(
            before,
            agg.moved_by.get(&entry.name).map_or(&[][..], Vec::as_slice),
        ));
    }
    if !entry.enabled {
        return;
    }
    let Some(by) = agg.disabled_by(snap, &entry.name) else {
        return;
    };
    let why = if agg.qualified.get(&entry.name).is_some_and(|b| b == by) {
        format!(
            "switched off as '{by}', the name it had before another source offered a tool of \
             that name and it took its server's prefix — switch '{by}' on to offer it"
        )
    } else {
        format!(
            "switched off as '{by}', a name it has while another source offers a tool of its \
             own name — switch '{by}' on to offer it"
        )
    };
    entry.enabled = false;
    entry.available = false;
    entry.reason = Some(match entry.reason.take() {
        Some(s) => format!("{why} (and {s})"),
        None => why,
    });
}

/// Why a tool moved from `before`: who else claims it.
fn moved_reason(before: &str, by: &[super::Claimant]) -> String {
    use super::Claimant;
    let claims: Vec<String> = by
        .iter()
        .map(|c| match c {
            Claimant::Builtin(ns) => format!("'{ns}' is one of lmgw's own namespaces"),
            Claimant::Server {
                name,
                connected: true,
                ..
            } => format!("server '{name}' offers a tool of that name"),
            Claimant::Server {
                name,
                connected: false,
                ..
            } => format!(
                "server '{name}' offered a tool of that name when it was last connected (it is \
                 not connected now; its claim is that last listing)"
            ),
        })
        .collect();
    if claims.is_empty() {
        format!("'{before}' is claimed by another tool, so this one took its server's name")
    } else {
        format!(
            "'{before}': {} — so this one took its server's name",
            claims.join("; ")
        )
    }
}

fn source_of(
    label: &str,
    name: &str,
    kind: &str,
    server_id: Option<i64>,
    plane: &str,
    reason: Option<String>,
    tools: &[ToolEntry],
) -> ToolSource {
    ToolSource {
        label: label.to_string(),
        name: name.to_string(),
        kind: kind.to_string(),
        server_id,
        plane: plane.to_string(),
        available: reason.is_none(),
        reason,
        tool_count: tools.len() as i64,
        disabled_count: tools.iter().filter(|t| !t.enabled).count() as i64,
    }
}

#[cfg(test)]
mod tests {
    use super::super::Claimant;
    use super::moved_reason;

    /// A moved tool's reason names who else claims its name, and says when
    /// that claim is only a server's last listing.
    #[test]
    fn a_moved_reason_names_the_claimant_and_a_stale_listing() {
        let server = |name: &str, connected| Claimant::Server {
            id: 1,
            name: name.into(),
            connected,
        };
        let live = moved_reason("read", &[server("beta", true)]);
        assert!(
            live.contains("server 'beta' offers") && !live.contains("not connected"),
            "{live}"
        );
        let stale = moved_reason("read", &[server("beta", false)]);
        assert!(
            stale.contains("'beta'") && stale.contains("not connected now"),
            "{stale}"
        );
        let ns = moved_reason("kb__x", &[Claimant::Builtin("kb__")]);
        assert!(ns.contains("lmgw's own namespaces"), "{ns}");
    }
}
