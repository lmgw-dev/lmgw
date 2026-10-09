//! The exposed names of the aggregate (§7; the owner, 2026-10-09): two
//! sources that offer a tool under one name are not refused and neither is
//! shadowed — the colliding tools take their server's prefix.
//!
//! **A tool's names, in order.** Its own name `local` (the upstream's, or the
//! owner's rename — rename-then-prefix: a rename replaces the tool's own
//! name and the server's prefix still applies on top), then:
//! - a server with a tool prefix `p`: `p__local`, then `<name>__local`;
//! - a server without one (bare): `local`, then `<name>__local`.
//!
//! `<name>` is the server's name with every character a tool name cannot
//! carry (outside `[A-Za-z0-9_-]`) as `_`: what a bare server's label is
//! (its name), spelt as a prefix. The second name is for two servers that
//! share a prefix, and for a bare server. A device-hosted row has only the
//! first: its tools stay in its label's namespace, which no other server's
//! prefix or name runs into (`devices::label_refusal`,
//! `ops::reject_device_label`), so nothing outside the device claims a name
//! in it; a name two of its own tools share is skipped and surfaced, never
//! moved out of `<label>__`.
//!
//! **One name per tool of a server.** A server that lists one name twice
//! (or whose renames give two tools one name) offers the first; the others
//! are skipped and surfaced before any name is settled.
//!
//! **Who moves.** Each name is held by how strongly its spelling says whose
//! it is: a prefix (2) over a server's name (1) over a bare name (0). Where
//! two tools have one name, the weaker hold moves to its next name; two of
//! one strength both move — so two bare servers' `read` are `alpha__read`
//! and `beta__read`, never one `read` and one other, and the bare name no
//! longer routes anywhere. A bare server's literal `gh__search` moves for
//! the `search` of the server prefixed `gh`, whose name stays. A name in one
//! of lmgw's own namespaces (`lmgw__`, `docs__`, `kb__`) is held by lmgw: a
//! server's tool there moves out of it. Repeated until nothing collides or
//! nothing can move.
//!
//! **Stable.** Only the colliding tools change; every other keeps its name.
//! Which tools collide is read from every enabled server's tools — the
//! connected ones' as listed now, and each other's as it last listed them
//! (`known`, stored across restarts) — so a name does not change when a
//! server is reaped, sleeps or goes offline, nor with the order servers
//! connect in. A new tool that collides moves the old one with it.
//!
//! **What is left.** A name that still collides (two servers whose names
//! spell alike) goes to the strongest hold, then the first by server name;
//! the other is skipped and surfaced, as a name in a reserved namespace that
//! could not move out and one past [`MAX_TOOL_NAME_LEN`] are.

use std::collections::{HashMap, HashSet};

use rmcp::model::Tool;

use super::{
    resources, AggServerInput, Aggregate, Claimant, SkippedTool, MAX_TOOL_NAME_LEN,
    RESERVED_NAMESPACES,
};

/// How strongly a spelling says whose a name is (module doc).
type Hold = u8;
const BARE: Hold = 0;
const BY_NAME: Hold = 1;
const BY_PREFIX: Hold = 2;

/// A server's name spelt as a prefix (module doc).
pub fn name_qualifier(server_name: &str) -> String {
    server_name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The names a tool may be exposed under, in order (module doc): a device
/// row's only its label's.
fn candidates(prefix: &str, server_name: &str, local: &str, device: bool) -> Vec<(String, Hold)> {
    let prefix = prefix.trim();
    let q = name_qualifier(server_name);
    let mut out = if prefix.is_empty() {
        vec![(local.to_string(), BARE)]
    } else {
        vec![(format!("{prefix}__{local}"), BY_PREFIX)]
    };
    if !device && !q.is_empty() && q != prefix {
        out.push((format!("{q}__{local}"), BY_NAME));
    }
    out
}

fn reserved(name: &str) -> Option<&'static str> {
    RESERVED_NAMESPACES
        .iter()
        .find(|(_, ns)| name.starts_with(ns))
        .map(|(_, ns)| *ns)
}

/// One tool's claim.
struct Entry<'a> {
    /// Index into the sorted inputs: live first, then known.
    server: usize,
    tool: &'a Tool,
    live: bool,
    names: Vec<(String, Hold)>,
    at: usize,
}

impl Entry<'_> {
    fn name(&self) -> &str {
        &self.names[self.at].0
    }
    fn hold(&self) -> Hold {
        self.names[self.at].1
    }
    fn can_move(&self) -> bool {
        self.at + 1 < self.names.len()
    }
}

/// Move the weaker of every two claims of one name on (module doc).
fn settle(entries: &mut [Entry<'_>]) {
    loop {
        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            by_name.entry(e.name()).or_default().push(i);
        }
        let mut moving: Vec<usize> = Vec::new();
        for (name, ids) in &by_name {
            if reserved(name).is_some() {
                moving.extend(ids);
                continue;
            }
            if ids.len() < 2 {
                continue;
            }
            let top = ids.iter().map(|&i| entries[i].hold()).max().unwrap_or(BARE);
            let weaker: Vec<usize> = ids
                .iter()
                .copied()
                .filter(|&i| entries[i].hold() < top)
                .collect();
            moving.extend(if weaker.is_empty() {
                ids.clone()
            } else {
                weaker
            });
        }
        let mut moved = false;
        for i in moving {
            if entries[i].can_move() {
                entries[i].at += 1;
                moved = true;
            }
        }
        if !moved {
            return;
        }
    }
}

/// Who else claims the name entry `i` had before it moved: one of lmgw's own
/// namespaces, or each other server whose tool held that name at some step
/// (once each, in the inputs' order).
fn claimants(
    entries: &[Entry<'_>],
    inputs: &[(&AggServerInput<'_>, bool)],
    i: usize,
) -> Vec<Claimant> {
    let first = entries[i].names[0].0.as_str();
    if let Some(ns) = reserved(first) {
        return vec![Claimant::Builtin(ns)];
    }
    let mut out: Vec<Claimant> = Vec::new();
    for (j, e) in entries.iter().enumerate() {
        if j == i || !e.names[..=e.at].iter().any(|(n, _)| n == first) {
            continue;
        }
        let (s, connected) = inputs[e.server];
        if out
            .iter()
            .any(|c| matches!(c, Claimant::Server { id, .. } if *id == s.server_id))
        {
            continue;
        }
        out.push(Claimant::Server {
            id: s.server_id,
            name: s.server_name.to_string(),
            connected,
        });
    }
    out
}

/// [`super::build_aggregate`] with the tools of the servers not connected
/// now, as they last listed them (`known`): they claim their names, and
/// route nowhere.
pub fn build(servers: &mut [AggServerInput<'_>], known: &mut [AggServerInput<'_>]) -> Aggregate {
    let by_name = |a: &AggServerInput<'_>, b: &AggServerInput<'_>| {
        a.server_name
            .cmp(b.server_name)
            .then(a.server_id.cmp(&b.server_id))
    };
    servers.sort_by(by_name);
    known.sort_by(by_name);
    let inputs: Vec<(&AggServerInput<'_>, bool)> = servers
        .iter()
        .map(|s| (s, true))
        .chain(known.iter().map(|s| (s, false)))
        .collect();

    let mut entries: Vec<Entry<'_>> = Vec::new();
    let mut agg = Aggregate::default();
    for (server, (s, live)) in inputs.iter().enumerate() {
        let (mut seen, mut listed): (HashSet<&str>, HashSet<&str>) = Default::default();
        for tool in s.tools.iter() {
            let upstream = tool.name.as_ref();
            let ov = s.overrides.get(upstream);
            // Hidden overrides are excluded from the aggregate entirely (§7),
            // and claim no name.
            if ov.is_some_and(|o| o.hidden) {
                continue;
            }
            let local = ov
                .and_then(|o| o.rename.as_deref())
                .filter(|r| !r.trim().is_empty())
                .unwrap_or(upstream);
            // One name per tool of a server (module doc): the first keeps it.
            let twice = !listed.insert(upstream);
            if !seen.insert(local) {
                if *live {
                    let reason = if twice {
                        format!(
                            "the server lists '{upstream}' more than once; the first is offered"
                        )
                    } else {
                        format!(
                            "its rename gives it the name '{local}', which another tool of the \
                             server has; that one is offered"
                        )
                    };
                    tracing::warn!(
                        server = %s.server_name,
                        tool = %upstream,
                        "MCP tool dropped from aggregate: {reason}"
                    );
                    agg.skipped.push(SkippedTool {
                        server_id: s.server_id,
                        server_name: s.server_name.to_string(),
                        exposed_name: candidates(s.tool_prefix, s.server_name, local, s.device)
                            .swap_remove(0)
                            .0,
                        reason,
                    });
                }
                continue;
            }
            entries.push(Entry {
                server,
                tool,
                live: *live,
                names: candidates(s.tool_prefix, s.server_name, local, s.device),
                at: 0,
            });
        }
    }
    settle(&mut entries);

    // Who keeps a name that still collides: the strongest hold, then the
    // first by server name (the inputs' order).
    let mut keeper: HashMap<&str, usize> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        if reserved(e.name()).is_some() {
            continue;
        }
        let k = keeper.entry(e.name()).or_insert(i);
        if entries[*k].hold() < e.hold() {
            *k = i;
        }
    }

    for (i, e) in entries.iter().enumerate() {
        if !e.live {
            continue;
        }
        let s = inputs[e.server].0;
        let upstream = e.tool.name.as_ref();
        let exposed = e.name().to_string();
        let skip = |reason: String, agg: &mut Aggregate| {
            tracing::warn!(
                server = %s.server_name,
                tool = %upstream,
                exposed = %exposed,
                "MCP tool dropped from aggregate: {reason}"
            );
            agg.skipped.push(SkippedTool {
                server_id: s.server_id,
                server_name: s.server_name.to_string(),
                exposed_name: exposed.clone(),
                reason,
            });
        };
        // Reserved namespaces: a southbound server never gets to occupy an
        // `lmgw__*` (§20), `docs__*` (quickdoc §7) or `kb__*` (chat-complete
        // §9.4) name; `ops` refuses those prefixes at config time, so this
        // is a tool that could not move out of one.
        if let Some(ns) = reserved(&exposed) {
            skip(
                format!(
                    "exposed name '{exposed}' is in the reserved '{ns}' namespace (one of \
                     lmgw's own built-in toolsets), even under its server's name; rename it"
                ),
                &mut agg,
            );
            continue;
        }
        // 64-char ceiling: skip + surface, never truncate (§7, house rule).
        if exposed.len() > MAX_TOOL_NAME_LEN {
            skip(
                format!(
                    "exposed name is {} chars (> {MAX_TOOL_NAME_LEN}); shorten the tool_prefix \
                     or rename it",
                    exposed.len()
                ),
                &mut agg,
            );
            continue;
        }
        if let Some(&k) = keeper.get(exposed.as_str()).filter(|&&k| k != i) {
            let other = inputs[entries[k].server].0.server_name;
            skip(
                format!(
                    "exposed name '{exposed}' collides with server '{other}' even under its \
                     server's name (kept by '{other}'); rename it"
                ),
                &mut agg,
            );
            continue;
        }
        // Forward the upstream Tool verbatim with only its name rewritten to
        // the exposed name; inputSchema and all other fields pass through (§7).
        let mut tool = e.tool.clone();
        tool.name = exposed.clone().into();
        // Its UI resource in the server's namespace, as `/mcp` serves the
        // resource (L14): the server's tool prefix, whatever name the tool
        // took — a collision moves a tool's name, never its resources.
        resources::apps::namespace_tool(s.tool_prefix, &mut tool);
        if e.at > 0 {
            agg.qualified.insert(exposed.clone(), e.names[0].0.clone());
            agg.moved_by
                .insert(exposed.clone(), claimants(&entries, &inputs, i));
        }
        if e.names.len() > 1 {
            agg.spellings.insert(
                exposed.clone(),
                e.names.iter().map(|(n, _)| n.clone()).collect(),
            );
        }
        agg.tools.push(tool);
        agg.reverse
            .insert(exposed, (s.server_id, upstream.to_string()));
    }
    agg.tools.sort_by(|a, b| a.name.cmp(&b.name));
    agg
}

#[cfg(test)]
mod tests;
