//! Who reaches a device-hosted label (client-apps design §5.6, L16), on
//! every plane that asks a [`ToolScope`](super::ToolScope): `/mcp`,
//! `/v1/responses` `mcp` blocks, realtime `mcp` tools, `/v1/mcp/servers`,
//! Chat turns.
//!
//! | Principal | Reaches the label |
//! |---|---|
//! | owner key, owner cookie | yes |
//! | the gateway's own runs (`ToolScope::gateway()`) | yes, as the owner's |
//! | the device that hosts it | yes (its own tools) |
//! | a client key or device key with tool scope `allow` | only if a pattern names the label: its literal text before the first wildcard starts with `<label>__` |
//! | a client key or device key with tool scope `all` or `deny` | no |
//! | an agent | only if its manifest's `tools[]` names the label |
//! | anonymous | **no**, whatever *Require API key* says |
//!
//! So a `List::All` — anonymous, an `all` key — carries "except the
//! device-hosted namespaces", read from the snapshot's device rows, and it
//! narrows for a device row it does not reach whole
//! (`ToolScope::narrows_for`), which is what closes review W3-9: an `all`
//! device writing another device's label into a thread or a folder default
//! is refused like any label out of its reach — and only that: its other
//! servers stay as an `all` scope has them.
//!
//! **A name is a device's by who serves it, not by how it is spelled.** The
//! aggregate's routes say which server a name runs on now
//! (`Aggregate::reverse`), and the name is a device's when that server is
//! its row. A name nothing serves now — the device offline, a server not
//! connected — is held back as a device's when it falls in a label's
//! namespace (`<label>__`, the longest such label): failing closed, since
//! the device could connect and own it before the call routes. So a bare
//! server's own `desktop__x` or a server prefixed `desktop_` (whose names
//! begin `desktop___`) is no device's while it serves them, and labels `a`
//! and `a_` cannot be mistaken for each other.

use std::collections::HashMap;

use lmgw_api_types::scope::fold;

use crate::config::{KeyPolicy, ScopeMode, Snapshot};

/// One device-hosted label in the snapshot.
#[derive(Debug, Clone)]
pub(super) struct HostedLabel {
    /// The label as granted: `desktop`.
    pub(super) label: String,
    /// `desktop__`, folded: what a name in the namespace starts with.
    ns: String,
    /// The hosting device's key.
    pub(super) key_id: i64,
    /// The device row.
    server_id: i64,
}

/// The device-hosted labels, and how far one caller reaches into them.
#[derive(Debug, Clone, Default)]
pub(super) struct Hosted {
    /// Every one of them: the owner and the gateway's own runs.
    pub(super) every: bool,
    /// The caller's own key: a device reaches the label it hosts.
    pub(super) own: Option<i64>,
    labels: Vec<HostedLabel>,
    /// Every exposed name the aggregate routes now, to its server's id
    /// (module doc). Empty when there is no device row to tell apart.
    routes: HashMap<String, i64>,
}

impl Hosted {
    /// Every label, for the owner and the gateway.
    pub(super) fn every() -> Self {
        Self {
            every: true,
            ..Self::default()
        }
    }

    /// The labels `snap` has device rows for, reached as `own`'s, with the
    /// aggregate's `routes` (exposed name → server id) telling whose a name
    /// is.
    pub(super) fn of(snap: &Snapshot, own: Option<i64>, routes: HashMap<String, i64>) -> Self {
        let labels = labels(snap);
        Self {
            every: false,
            own,
            routes: if labels.is_empty() {
                HashMap::new()
            } else {
                routes
            },
            labels,
        }
    }

    /// Whether `snap` has a device row at all: only then are the routes
    /// worth reading.
    pub(super) fn any_in(snap: &Snapshot) -> bool {
        snap.mcp_servers.values().any(|s| s.is_device())
    }

    /// The hosted label `name` belongs to, if any: the device whose row
    /// serves it now; for a name nothing serves, the label whose namespace
    /// it falls in (module doc).
    pub(super) fn of_name(&self, name: &str) -> Option<&HostedLabel> {
        if let Some(id) = self.routes.get(name) {
            return self.labels.iter().find(|h| h.server_id == *id);
        }
        let name = fold(name);
        self.labels
            .iter()
            .filter(|h| name.starts_with(&h.ns))
            .max_by_key(|h| h.ns.len())
    }

    /// The server a name routes to now, as the scope was read: what a call
    /// checked against this scope is held to (`McpManager::call_listed`).
    pub(super) fn routed_to(&self, name: &str) -> Option<i64> {
        self.routes.get(name).copied()
    }

    /// The hosted label of the device row `server`, if it is one.
    pub(super) fn of_server(&self, server: &crate::config::McpServer) -> Option<&HostedLabel> {
        if !server.is_device() {
            return None;
        }
        self.labels.iter().find(|h| h.server_id == server.id)
    }

    /// The hosted label with this prefix, if it is one.
    pub(super) fn of_prefix(&self, prefix: &str) -> Option<&HostedLabel> {
        let ns = fold(&format!("{prefix}__"));
        self.labels.iter().find(|h| h.ns == ns)
    }

    /// Reached whatever the caller's list says: the owner's, the gateway's,
    /// the hosting device's own.
    pub(super) fn whole(&self, h: &HostedLabel) -> bool {
        self.every || self.own == Some(h.key_id)
    }
}

/// The device-hosted labels of `snap`'s device rows.
fn labels(snap: &Snapshot) -> Vec<HostedLabel> {
    snap.mcp_servers
        .values()
        .filter(|s| s.is_device())
        .filter_map(|s| {
            let label = s.tool_prefix.trim();
            (!label.is_empty()).then(|| HostedLabel {
                label: label.to_string(),
                ns: fold(&format!("{label}__")),
                key_id: s.device_key_id.unwrap_or_default(),
                server_id: s.id,
            })
        })
        .collect()
}

/// The patterns of an `allow` tool scope that name `h` explicitly: their
/// literal text before the first wildcard starts with `<label>__`. `*` or
/// `d*` names nothing; `desktop__*` and `desktop__see_screen` do. Empty
/// for any other mode.
fn naming<'a>(policy: &'a KeyPolicy, h: &HostedLabel) -> impl Iterator<Item = &'a str> + 'a {
    let ns = h.ns.clone();
    let allow = policy.tool_scope_mode == ScopeMode::Allow;
    policy
        .tool_scope_patterns
        .lines()
        .map(str::trim)
        .filter(move |p| allow && !p.is_empty())
        .filter(move |p| {
            let head = fold(p.split('*').next().unwrap_or_default());
            head.starts_with(&ns)
        })
}

/// Whether a key's tool scope names `h` at all (§5.6).
pub(super) fn names_label(policy: &KeyPolicy, h: &HostedLabel) -> bool {
    naming(policy, h).next().is_some()
}

/// Whether one of the patterns naming `h` matches `name`.
pub(super) fn admits_named(policy: &KeyPolicy, h: &HostedLabel, name: &str) -> bool {
    let named: Vec<&str> = naming(policy, h).collect();
    lmgw_api_types::scope::patterns_match(&named.join("\n"), name)
}

/// Whether the patterns naming `h` admit every name in it: one is
/// `<label>__*` itself.
pub(super) fn admits_whole(policy: &KeyPolicy, h: &HostedLabel) -> bool {
    naming(policy, h).any(|p| fold(p) == format!("{}*", h.ns))
}

/// Every principal that reaches `label`, hosted by device key `key_id`, as
/// the Devices card lists them (§5.6): the owner, the device, each key and
/// device whose scope names the label, each agent whose manifest does.
pub async fn reachers(
    state: &crate::state::SharedState,
    snap: &Snapshot,
    key_id: i64,
    label: &str,
) -> Vec<String> {
    use crate::config::ApiKeyKind;
    let h = HostedLabel {
        label: label.to_string(),
        ns: fold(&format!("{label}__")),
        key_id,
        server_id: 0,
    };
    let mut out = vec!["owner".to_string()];
    let mut keys: Vec<&crate::config::ApiKey> = snap.api_keys.iter().collect();
    keys.sort_by_key(|k| (k.id != key_id, k.name.clone()));
    for k in keys {
        match k.kind {
            ApiKeyKind::Device if k.id == key_id => {
                out.push(format!(
                    "device '{}' (hosts it)",
                    crate::devices::short_name(&k.name)
                ));
            }
            ApiKeyKind::Device | ApiKeyKind::Key if names_label(&k.policy, &h) => {
                out.push(k.described());
            }
            ApiKeyKind::Agent => {
                let Some(agent) = k.agent_id.as_deref() else {
                    continue;
                };
                let grant = crate::agents::tool_grant(state, agent)
                    .await
                    .unwrap_or_default();
                if grant.labels.iter().any(|l| fold(l) == fold(label)) {
                    out.push(format!("agent '{agent}'"));
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::{List, ToolScope};
    use super::*;
    use crate::config::{
        ApiKey, ApiKeyKind, DeviceAdmin, McpServer, McpTransport, ScopeMode, SelfAdmin,
    };

    fn row(id: i64, name: &str, prefix: &str, device_key_id: Option<i64>) -> McpServer {
        McpServer {
            id,
            name: name.into(),
            enabled: true,
            transport: if device_key_id.is_some() {
                McpTransport::Device
            } else {
                McpTransport::Http
            },
            command: None,
            args: vec![],
            env: vec![],
            cwd: None,
            container_image: None,
            extra_run_args: vec![],
            url: None,
            headers: vec![],
            tool_prefix: prefix.into(),
            timeout_ms: 60_000,
            autostart: false,
            idle_seconds: 0,
            allow_sampling: false,
            sampling_alias: None,
            agent_id: None,
            device_key_id,
        }
    }

    fn snap() -> Snapshot {
        let mut snap = Snapshot::default();
        snap.mcp_servers
            .insert(9, row(9, "device:desktop", "desktop", Some(3)));
        snap
    }

    /// `routes` as the aggregate would hold them: name → server id.
    fn routes(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
        pairs.iter().map(|(n, id)| (n.to_string(), *id)).collect()
    }

    fn agent(labels: &[&str], allowed: &[&str]) -> ToolScope {
        ToolScope {
            self_admin: false,
            cap: SelfAdmin::Off,
            list: List::Agent {
                agent_id: "board".into(),
                labels: labels.iter().map(|l| l.to_string()).collect(),
                allowed: allowed.iter().map(|l| l.to_string()).collect(),
            },
            hosted: Hosted::of(&snap(), None, HashMap::new()),
            spellings: HashMap::new(),
        }
    }

    fn anonymous(snap: &Snapshot, routes: HashMap<String, i64>) -> ToolScope {
        ToolScope {
            self_admin: false,
            cap: SelfAdmin::Off,
            list: List::All,
            hosted: Hosted::of(snap, None, routes),
            spellings: HashMap::new(),
        }
    }

    /// Device key `key_id`, as its own caller, with an `allow` list.
    fn device(snap: &Snapshot, key_id: i64, allow: &str, r: HashMap<String, i64>) -> ToolScope {
        let key = ApiKey {
            id: key_id,
            name: format!("device:d{key_id}"),
            key_hash: String::new(),
            enabled: true,
            kind: ApiKeyKind::Device,
            key_plain: None,
            agent_id: None,
            policy: KeyPolicy {
                tool_scope_mode: ScopeMode::Allow,
                tool_scope_patterns: allow.into(),
                ..KeyPolicy::default()
            },
            note: String::new(),
            hosts_label: None,
            self_admin: DeviceAdmin::Off,
        };
        ToolScope {
            self_admin: false,
            cap: SelfAdmin::Off,
            list: List::Key {
                name: key.described(),
                policy: key.policy.clone(),
            },
            hosted: Hosted::of(snap, Some(key_id), r),
            spellings: HashMap::new(),
        }
    }

    /// An agent reaches a device's label only when its manifest names it.
    #[test]
    fn an_agent_reaches_a_device_label_only_through_its_manifest() {
        let s = snap();
        let row = &s.mcp_servers[&9];
        let with = agent(&["desktop"], &["desktop__echo"]);
        assert!(with.may_reach(row) && with.admits("desktop__echo"));
        let without = agent(&["github"], &["github__search"]);
        assert!(!without.may_reach(row) && !without.admits("desktop__echo"));
        assert!(without
            .refusal("desktop__echo")
            .contains("a paired device's hosted tools"));
    }

    /// `All` holds a device's label back unless it is the owner's, the
    /// gateway's or the hosting device's — and narrows for that row alone
    /// (W3-9): its other servers, a bare one included, stay unchecked.
    #[test]
    fn all_holds_the_label_back_and_narrows_for_that_row_only() {
        let mut s = snap();
        s.mcp_servers.insert(4, row(4, "bare", "", None));
        s.mcp_servers.insert(5, row(5, "github", "github", None));
        let anon = anonymous(&s, routes(&[("desktop__echo", 9)]));
        assert!(!anon.admits("desktop__echo") && anon.admits("github__search"));
        assert!(!anon.may_reach(&s.mcp_servers[&9]));
        assert!(anon.narrows_for(&s.mcp_servers[&9]));
        assert!(!anon.narrows_for(&s.mcp_servers[&4]) && !anon.narrows_for(&s.mcp_servers[&5]));
        assert!(!anon.narrows());
        let host = ToolScope {
            hosted: Hosted::of(&s, Some(3), routes(&[("desktop__echo", 9)])),
            spellings: HashMap::new(),
            ..anon.clone()
        };
        assert!(host.admits("desktop__echo") && !host.narrows_for(&s.mcp_servers[&9]));
        assert!(ToolScope::gateway().admits("desktop__echo"));
        assert!(!ToolScope::gateway().narrows_for(&s.mcp_servers[&9]));
    }

    /// A name is the device's by the server that serves it (review finding
    /// 2): a server prefixed `desktop_` (names `desktop___…`), one prefixed
    /// `desktop__x` and a bare server's own `desktop__x` are not the
    /// device's while they serve those names — neither held back from an
    /// `all` caller nor handed whole to the hosting device.
    #[test]
    fn a_name_is_the_device_s_by_who_serves_it_not_by_its_spelling() {
        let mut s = snap();
        s.mcp_servers.insert(4, row(4, "under", "desktop_", None));
        s.mcp_servers.insert(5, row(5, "deep", "desktop__x", None));
        s.mcp_servers.insert(6, row(6, "bare", "", None));
        let r = routes(&[
            ("desktop__echo", 9),
            ("desktop___x", 4),
            ("desktop__x__y", 5),
            ("desktop__x", 6),
        ]);
        let anon = anonymous(&s, r.clone());
        for name in ["desktop___x", "desktop__x__y", "desktop__x"] {
            assert!(anon.admits(name), "{name} is not the device's");
        }
        assert!(!anon.admits("desktop__echo"));
        // The hosting device, its own list naming nothing of them: its own
        // tool whole, the others not at all.
        let host = device(&s, 3, "github__*", r);
        assert!(host.admits("desktop__echo"));
        for name in ["desktop___x", "desktop__x__y", "desktop__x"] {
            assert!(!host.admits(name), "{name} is not the host's");
        }
    }

    /// A name nothing serves now is held back by the namespace it falls in,
    /// the longest label's: failing closed while the device is offline.
    #[test]
    fn a_name_nothing_serves_is_held_back_by_its_namespace() {
        let mut s = snap();
        s.mcp_servers.insert(10, row(10, "device:a", "a", Some(20)));
        s.mcp_servers
            .insert(11, row(11, "device:a_", "a_", Some(21)));
        let anon = anonymous(&s, HashMap::new());
        assert!(!anon.admits("desktop__echo"));
        assert!(anon.refusal("desktop__echo").contains("'desktop'"));
        // `a___x` is in both `a__` and `a___`: the longer is the one.
        let a = device(&s, 20, "zzz", HashMap::new());
        let a_ = device(&s, 21, "zzz", HashMap::new());
        assert!(!a.admits("a___x") && a_.admits("a___x"));
    }

    /// Labels `a` and `a_` (older rows; a new label ending in `_` is
    /// refused): each device reaches what its own row serves, whatever the
    /// map's order.
    #[test]
    fn labels_a_and_a_underscore_are_told_apart_by_their_rows() {
        let mut s = Snapshot::default();
        s.mcp_servers.insert(10, row(10, "device:a", "a", Some(20)));
        s.mcp_servers
            .insert(11, row(11, "device:a_", "a_", Some(21)));
        // `a`'s tool `_x` and `a_`'s tool `x` are both `a___x`: the aggregate
        // keeps one; whichever it is, only its own device reaches it whole.
        for owner in [10, 11] {
            let r = routes(&[("a___x", owner)]);
            let a = device(&s, 20, "zzz", r.clone());
            let a_ = device(&s, 21, "zzz", r);
            assert_eq!(a.admits("a___x"), owner == 10, "served by {owner}");
            assert_eq!(a_.admits("a___x"), owner == 11, "served by {owner}");
        }
    }
}
