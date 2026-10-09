//! Which tools one caller may reach (key tool scope design).
//!
//! The owner's per-tool switch ([`Snapshot::tool_disabled`]) says what the
//! gateway offers at all. This says which of those one **credential** gets,
//! and it is decided from the request's principal the same way on every plane
//! that hands tools to a caller outside the process: `/mcp`
//! ([`ingress`](super::ingress)) and a `/v1/responses` run
//! ([`exec::resolve`](super::exec::resolve)).
//!
//! | Principal | Tools | `lmgw__*` self-admin |
//! | --- | --- | --- |
//! | client key | its own tool scope (`all` / `allow` / `deny` globs) | never |
//! | device key | its own tool scope, as a client key's | at the level the owner gave it (`ApiKey::self_admin`), capped by the gateway's |
//! | agent token | its manifest's `tools[]`, resolved against the surface | never |
//! | owner key | everything offered | yes, where the plane serves it |
//! | no credential | everything offered | never |
//!
//! Anything else — a key row that is gone by the time it is read, an agent row
//! without its agent identity, an internal identity — reaches **nothing**:
//! every unknown fails closed.
//!
//! The gateway's own runs — Admin Chat, a Chat thread, an in-process agent run
//! — are [`ToolScope::gateway`]: the owner attached their tools, or installed
//! the manifest that did.
//!
//! "No credential" sees everything because that is what *Require API key* off
//! means; the scope binds a key that is presented, and switching auth on is
//! what makes presenting one mandatory.
//!
//! **Except a device-hosted label** (client-apps design §5.6, L16): only the
//! owner, the gateway's own runs, the device that hosts it, and a key, device
//! or agent whose list names the label explicitly reach it — never an
//! anonymous caller, an `all` or a `deny` scope ([`hosted`]).

use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::{ToolExecutor, ToolOutcome};
use crate::config::{ApiKeyKind, KeyPolicy, McpServer, ScopeMode, SelfAdmin, Snapshot};
use crate::principal::{Cap, Principal};
use crate::proxy::RequestCtx;
use crate::state::SharedState;

use super::selfadmin;

mod hosted;
pub use hosted::reachers;
use hosted::Hosted;

/// One caller's reach into the tool plane.
#[derive(Debug, Clone)]
pub struct ToolScope {
    /// May this caller use the `lmgw__*` self-admin tools? An owner
    /// credential (principals §3.7), the gateway itself, and a device the
    /// owner allowed lmgw's admin tools (`ApiKey::self_admin`, client-apps
    /// design L5, 2026-10-07). Whether a plane serves them at all is the
    /// plane's business — `/mcp` never does — and the self-admin level in
    /// Settings bounds what they may do.
    self_admin: bool,
    /// What its admin tools may do at most before the gateway's own level
    /// caps it (`devices::admin_cap`): a device's own level (the pre-merge
    /// review's P-3), `Full` for every other caller.
    cap: SelfAdmin,
    list: List,
    /// The device-hosted labels and how far this caller reaches into them
    /// (L16).
    hosted: Hosted,
    /// For a deny list, every name each tool has or would have when it has
    /// more than one (`Aggregate::spellings`): a tool is refused when its
    /// list denies any of them ([`ToolScope::admits`]). Empty for every
    /// other list.
    spellings: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone)]
enum List {
    /// No list of its own: the owner's switch is the only filter.
    All,
    /// A client key's tool scope, never `ScopeMode::All` (that is [`List::All`]).
    Key { name: String, policy: KeyPolicy },
    /// An agent token: its manifest's labels, and the exposed names they
    /// resolve to (container-runtime §3.1). The manifest *is* the allow list,
    /// so an agent whose manifest could not be read has an empty one.
    Agent {
        agent_id: String,
        labels: Vec<String>,
        allowed: Vec<String>,
    },
    /// A principal with no list to read. `who` names it in a refusal.
    Nothing { who: String },
}

impl ToolScope {
    /// The gateway's own runs, whose tools the owner attached.
    pub fn gateway() -> Self {
        Self {
            self_admin: true,
            cap: SelfAdmin::Full,
            list: List::All,
            hosted: Hosted::every(),
            spellings: HashMap::new(),
        }
    }

    /// A caller that reaches no tool at all, named `who` in a refusal.
    pub fn nothing(who: impl Into<String>) -> Self {
        Self {
            self_admin: false,
            cap: SelfAdmin::Off,
            list: List::Nothing { who: who.into() },
            hosted: Hosted::default(),
            spellings: HashMap::new(),
        }
    }

    /// The scope of one HTTP request, from the principal the router resolved.
    ///
    /// Read from the current snapshot every time it is asked for: a key's
    /// scope is edited on a live gateway, and neither a session nor a running
    /// tool loop that outlived the edit may keep the old answer.
    pub async fn of_request(state: &SharedState, ctx: &RequestCtx) -> Self {
        let snap = state.snapshot();
        let self_admin = self_admin_of(&ctx.principal, &snap);
        let cap = crate::devices::admin_cap(&snap, &ctx.principal);
        // An agent token, whether or not `auth_enabled` is on (§3.1).
        let list = match &ctx.agent {
            Some(agent) => agent_list(state, &agent.agent_id).await,
            None => principal_list(&ctx.principal, &snap),
        };
        // The surface, read only when there is something to read it for:
        // whose each name is, by the server that serves it, when there is a
        // device row to tell apart (`hosted`); every name of each tool, for
        // a deny list.
        let hosts = Hosted::any_in(&snap);
        let mut agg = if hosts || denies(&list) {
            Some(state.mcp.aggregate(&snap).await)
        } else {
            None
        };
        let spellings = spellings_for(&list, agg.as_mut());
        let routes = match agg {
            Some(agg) if hosts => agg
                .reverse
                .into_iter()
                .map(|(name, (server, _))| (name, server))
                .collect(),
            _ => Default::default(),
        };
        let hosted = match &ctx.agent {
            Some(_) => Hosted::of(&snap, None, routes),
            None => hosted_of(&ctx.principal, &snap, routes),
        };
        Self {
            self_admin,
            cap,
            list,
            hosted,
            spellings,
        }
    }

    /// The same scope against the surface as it is *now*. Only an agent's
    /// list depends on the surface — a label with no `allowed` is whatever its
    /// server offers — so a run that just woke a service agent re-reads it.
    pub async fn refresh(&self, state: &SharedState) -> Self {
        match &self.list {
            List::Agent { agent_id, .. } => Self {
                self_admin: self.self_admin,
                cap: self.cap,
                list: agent_list(state, agent_id).await,
                hosted: self.hosted.clone(),
                spellings: HashMap::new(),
            },
            _ => self.clone(),
        }
    }

    /// Is `name` within this caller's reach? Says nothing about whether the
    /// gateway offers it — that is `tool_disabled` and the source's own state.
    ///
    /// The `lmgw__*` tools are the caller's only when it may use them
    /// ([`Self::self_admin`]). A device's switch "may use lmgw's admin
    /// tools" grants the whole label, and the device's own list narrows it
    /// where it names the namespace (client-apps design L5, review P-6): an
    /// allow pattern that begins with `lmgw__`, or a deny pattern that can
    /// match such a name. A list that says nothing about it leaves the
    /// label whole. The self-admin level bounds which of them exist.
    ///
    /// A deny list refuses a tool under any name it has or would have
    /// (`Aggregate::spellings`; client-apps design §7.6): denying `delete`
    /// keeps out the `alpha__delete` a collision moved it to, and denying
    /// `alpha__delete` keeps it out once the collision ends. An allow list
    /// admits the name it names only — allowing `delete` admits no tool a
    /// collision moved away from it.
    pub fn admits(&self, name: &str) -> bool {
        self.admits_as(name) && !self.denies_spelling(name)
    }

    /// Whether this caller's deny list denies another name of `name`'s tool.
    fn denies_spelling(&self, name: &str) -> bool {
        let List::Key { policy, .. } = &self.list else {
            return false;
        };
        policy.tool_scope_mode == ScopeMode::Deny
            && self
                .spellings
                .get(name)
                .is_some_and(|all| all.iter().any(|n| !policy.admits_tool(n)))
    }

    /// [`Self::admits`] by the name as spelt.
    fn admits_as(&self, name: &str) -> bool {
        if selfadmin::owns(name) {
            return self.self_admin
                && match &self.list {
                    List::Key { policy, .. } if names_self_admin(policy) => {
                        policy.admits_tool(name)
                    }
                    _ => true,
                };
        }
        // A device-hosted label (L16): reached whole by the owner, the
        // gateway and the hosting device; otherwise only through a list
        // that names it.
        if let Some(h) = self.hosted.of_name(name) {
            if self.hosted.whole(h) {
                return true;
            }
            return match &self.list {
                List::Key { policy, .. } => hosted::admits_named(policy, h, name),
                List::Agent {
                    labels, allowed, ..
                } => {
                    labels.iter().any(|l| l.eq_ignore_ascii_case(&h.label))
                        && allowed.iter().any(|a| a == name)
                }
                List::All | List::Nothing { .. } => false,
            };
        }
        match &self.list {
            List::All => true,
            List::Key { policy, .. } => policy.admits_tool(name),
            List::Agent { allowed, .. } => allowed.iter().any(|a| a == name),
            List::Nothing { .. } => false,
        }
    }

    /// Could any tool of `server` be within reach?
    ///
    /// Asked before a server is woken or described. A service agent offers no
    /// tools until its container runs, so waiting to ask [`admits`] would
    /// mean starting it for every caller that names it. And a caller that can
    /// never use a server gets the same "no such label" answer as a label that
    /// does not exist — not its disabled flag or its connection error.
    ///
    /// Answered from the namespace for a client key: a server with a tool
    /// prefix exposes `<prefix>__<tool>`, so an allow pattern that cannot
    /// begin with that, or a deny pattern that swallows all of it, settles the
    /// question without the tool list. A bare server has no namespace to read,
    /// so it is always worth asking about.
    pub fn may_reach(&self, server: &McpServer) -> bool {
        if let Some(h) = self.hosted.of_server(server) {
            if self.hosted.whole(h) {
                return true;
            }
            return match &self.list {
                List::Key { policy, .. } => hosted::names_label(policy, h),
                List::Agent { labels, .. } => labels
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case(&h.label) || *l == server.name),
                List::All | List::Nothing { .. } => false,
            };
        }
        match &self.list {
            List::All => true,
            List::Nothing { .. } => false,
            List::Agent { labels, .. } => labels
                .iter()
                .any(|l| *l == super::exec::server_label(server) || *l == server.name),
            List::Key { policy, .. } => match server.tool_prefix.trim() {
                "" => true,
                prefix => namespace_reachable(policy, prefix),
            },
        }
    }

    /// Is **every** name in `<prefix>__…` within this caller's reach — the
    /// question a written label must answer when its server cannot list its
    /// tools now (client-apps design L5). Conservative where a glob could
    /// match only part of the namespace: an allow list admits it whole only
    /// through a pattern `<head>*` whose head the namespace starts with; a
    /// deny list only when none of its patterns can match a name in it.
    pub fn admits_namespace(&self, prefix: &str) -> bool {
        if let Some(h) = self.hosted.of_prefix(prefix) {
            if self.hosted.whole(h) {
                return true;
            }
            return match &self.list {
                List::Key { policy, .. } => hosted::admits_whole(policy, h),
                _ => false,
            };
        }
        match &self.list {
            List::All => true,
            List::Nothing { .. } | List::Agent { .. } => false,
            List::Key { policy, .. } => namespace_admitted(policy, prefix),
        }
    }

    /// May this caller attach the `lmgw` self-admin toolset?
    pub fn self_admin(&self) -> bool {
        self.self_admin
    }

    /// What this caller's admin tools may do under the gateway's `global`
    /// level: `Off` for a caller that may not use them, a device's own level
    /// capped by `global` (the pre-merge review's P-3), `global` for the
    /// owner and the gateway.
    pub fn admin_level(&self, global: SelfAdmin) -> SelfAdmin {
        if self.self_admin {
            self.cap.min(global)
        } else {
            SelfAdmin::Off
        }
    }

    /// Does this caller have a list of its own, beyond the owner's switch?
    pub fn narrows(&self) -> bool {
        !matches!(self.list, List::All)
    }

    /// Does this caller's reach into `server` need checking, tool by tool?
    /// When its list narrows, or `server` is a device row it does not reach
    /// whole (L16, review W3-9): an `All` holds another device's label back
    /// and nothing else, so its other servers — a bare one not connected
    /// included — stay as an `All` has them.
    pub fn narrows_for(&self, server: &McpServer) -> bool {
        self.narrows()
            || self
                .hosted
                .of_server(server)
                .is_some_and(|h| !self.hosted.whole(h))
    }

    /// The server `name` routed to when this scope was read, when there are
    /// device rows to tell apart (else `None`): a call this scope admitted
    /// is held to it (`McpManager::call_listed`), so a name that changes
    /// hands between the check and the call — to a device the caller does
    /// not reach — is refused rather than run there.
    pub fn routed(&self, name: &str) -> Option<i64> {
        self.hosted.routed_to(name)
    }

    /// Who is asking, and what their list says — the half of a refusal that
    /// lets the owner find the setting.
    ///
    /// A **deny** list is not quoted back: its patterns name exactly the
    /// servers the key is meant not to know about.
    pub fn describe(&self) -> String {
        match &self.list {
            List::All => "this caller".to_string(),
            List::Key { name, policy } => match policy.tool_scope_mode {
                ScopeMode::Deny => format!("{name} (tool scope: a deny list)"),
                _ => format!(
                    "{name} (tool scope: allow {})",
                    if policy.tool_scope_patterns.is_empty() {
                        "— an empty list".to_string()
                    } else {
                        policy.tool_scope_patterns.replace('\n', ", ")
                    }
                ),
            },
            List::Agent {
                agent_id, allowed, ..
            } => format!(
                "agent '{agent_id}' (may call: {})",
                if allowed.is_empty() {
                    "nothing; its manifest declares no tools".to_string()
                } else {
                    allowed.join(", ")
                }
            ),
            List::Nothing { who } => who.clone(),
        }
    }

    /// Why `name` is refused, for a caller [`admits`](Self::admits) said no to.
    pub fn refusal(&self, name: &str) -> String {
        if let Some(h) = self.hosted.of_name(name).filter(|h| !self.hosted.whole(h)) {
            let named = matches!(&self.list, List::Key { policy, .. }
                if hosted::names_label(policy, h));
            if !named {
                return format!(
                    "{name} — '{}' is a paired device's hosted tools: only the owner, that \
                     device, and a key, device or agent whose tool scope names '{}__' \
                     explicitly reach them; {} does not",
                    h.label,
                    h.label,
                    self.describe()
                );
            }
        }
        if selfadmin::owns(name) && !self.self_admin {
            return format!(
                "{name} — the lmgw__* self-admin tools need an owner credential or a device \
                 the owner allowed lmgw's admin tools; a client key, an agent token or any \
                 other device does not reach the gateway's configuration"
            );
        }
        match &self.list {
            // Kept word for word from before the key scope existed: an agent
            // reading its refusal is the one caller that has always had one.
            List::Agent {
                agent_id, allowed, ..
            } => format!(
                "{name} — agent '{agent_id}' may call: {}",
                if allowed.is_empty() {
                    "nothing; its manifest declares no tools".to_string()
                } else {
                    allowed.join(", ")
                }
            ),
            List::Nothing { who } => format!("{name} — {who} may not use any tool"),
            _ => format!("{name} — outside the tool scope of {}", self.describe()),
        }
    }
}

/// Whether `list` is a deny list, which reads every name of a tool.
fn denies(list: &List) -> bool {
    matches!(list, List::Key { policy, .. } if policy.tool_scope_mode == ScopeMode::Deny)
}

/// What [`ToolScope::spellings`] holds for `list`, from the surface `agg`.
fn spellings_for(list: &List, agg: Option<&mut super::Aggregate>) -> HashMap<String, Vec<String>> {
    match agg {
        Some(agg) if denies(list) => std::mem::take(&mut agg.spellings),
        _ => HashMap::new(),
    }
}

/// An agent token's list, read from its manifest now.
async fn agent_list(state: &SharedState, agent_id: &str) -> List {
    let grant = match crate::agents::tool_grant(state, agent_id).await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(
                "agent '{agent_id}' presented its token but its allow list could not be read \
                 ({e}) — it sees no tools"
            );
            crate::agents::ToolGrant::default()
        }
    };
    List::Agent {
        agent_id: agent_id.to_string(),
        labels: grant.labels,
        allowed: grant.names,
    }
}

/// How far `principal` reaches into the device-hosted labels (L16): every
/// one for an owner credential, its own for a device, otherwise none but
/// what its list names.
fn hosted_of(
    principal: &Principal,
    snap: &Snapshot,
    routes: std::collections::HashMap<String, i64>,
) -> Hosted {
    match principal {
        Principal::Key {
            kind: ApiKeyKind::Owner,
            ..
        } => Hosted::every(),
        Principal::Key {
            id,
            kind: ApiKeyKind::Device,
            ..
        } => Hosted::of(snap, Some(*id), routes),
        _ => Hosted::of(snap, None, routes),
    }
}

/// Whether `principal` may use the `lmgw__*` tools: an owner credential
/// (principals §3.7), or a device the owner allowed lmgw's admin tools
/// (`ApiKey::self_admin`, client-apps design L5, 2026-10-07) while the
/// gateway's self-admin level is not `off` — what it sees of the toolset's
/// threads follows the same capped level (L3's note).
fn self_admin_of(principal: &Principal, snap: &Snapshot) -> bool {
    principal.holds(Cap::Admin, snap)
        || matches!(
            principal,
            Principal::Key {
                id,
                kind: ApiKeyKind::Device,
                ..
            } if crate::devices::may_do(snap, *id).allows_read()
        )
}

/// The list for every principal that is not an agent identity. Each shape is
/// named: a default here would be a fail-open the day a new one appears.
fn principal_list(principal: &Principal, snap: &Snapshot) -> List {
    match principal {
        Principal::Anonymous
        | Principal::Key {
            kind: ApiKeyKind::Owner,
            ..
        } => List::All,
        // A device's tool scope is a client key's (client-apps design §1.1):
        // the same list, the same modes.
        Principal::Key {
            id,
            name,
            kind: ApiKeyKind::Key | ApiKeyKind::Device,
            ..
        } => match snap.api_keys.iter().find(|k| k.id == *id) {
            Some(k) if k.policy.tool_scope_mode == ScopeMode::All => List::All,
            Some(k) => List::Key {
                // As a refusal names it: "device 'phone'", "key 'laptop'".
                name: k.described(),
                policy: k.policy.clone(),
            },
            // Resolved at the door, deleted since: the owner took it away.
            // Named as every refusal names it (review W4-20): a device by
            // its short name.
            None => List::Nothing {
                who: format!(
                    "{}, which no longer exists,",
                    match principal {
                        Principal::Key {
                            kind: ApiKeyKind::Device,
                            ..
                        } => format!("device '{}'", crate::devices::short_name(name)),
                        _ => format!("key '{name}'"),
                    }
                ),
            },
        },
        // An agent row reaches here only without its agent identity, and an
        // internal one never authenticates. Neither has a list to read.
        Principal::Key {
            name,
            kind: ApiKeyKind::Agent | ApiKeyKind::Internal,
            ..
        } => List::Nothing {
            who: format!("'{name}'"),
        },
    }
}

/// Whether a key's tool scope says anything about the `lmgw__*` names
/// (review P-6): an allow pattern that begins with the namespace, or a deny
/// pattern that can match a name in it. Only such a list narrows the label
/// a device's admin-tools switch grants ([`ToolScope::admits`]).
fn names_self_admin(policy: &KeyPolicy) -> bool {
    use lmgw_api_types::scope::fold;
    let ns = fold(selfadmin::PREFIX);
    let mut patterns = policy
        .tool_scope_patterns
        .lines()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(fold);
    match policy.tool_scope_mode {
        ScopeMode::All => false,
        ScopeMode::Allow => patterns.any(|p| p.starts_with(&ns)),
        ScopeMode::Deny => patterns.any(|p| match p.split_once('*') {
            Some((head, _)) => head.starts_with(&ns) || ns.starts_with(head),
            None => p.starts_with(&ns),
        }),
    }
}

/// Could a client key's tool scope admit any name in `<prefix>__…`?
fn namespace_reachable(policy: &KeyPolicy, prefix: &str) -> bool {
    use lmgw_api_types::scope::fold;
    let ns = fold(&format!("{prefix}__"));
    let patterns = policy
        .tool_scope_patterns
        .lines()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(fold);
    match policy.tool_scope_mode {
        ScopeMode::All => true,
        // Some pattern could match a name that starts with the namespace:
        // its literal head and the namespace agree for as long as both run.
        ScopeMode::Allow => patterns.into_iter().any(|p| {
            let head = p.split('*').next().unwrap_or_default();
            if p.contains('*') {
                head.starts_with(&ns) || ns.starts_with(head)
            } else {
                p.starts_with(&ns)
            }
        }),
        // Unless one pattern is "<a prefix of the namespace>*": that one
        // denies every name the server can expose.
        ScopeMode::Deny => !patterns.into_iter().any(|p| {
            let head = p.trim_end_matches('*');
            head.len() < p.len() && !head.contains('*') && ns.starts_with(head)
        }),
    }
}

/// Does a client key's tool scope admit **every** name in `<prefix>__…`?
/// ([`ToolScope::admits_namespace`].)
fn namespace_admitted(policy: &KeyPolicy, prefix: &str) -> bool {
    use lmgw_api_types::scope::fold;
    let ns = fold(&format!("{prefix}__"));
    let patterns: Vec<String> = policy
        .tool_scope_patterns
        .lines()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(fold)
        .collect();
    match policy.tool_scope_mode {
        ScopeMode::All => true,
        // One pattern that is a literal head and a single trailing `*`, the
        // head a prefix of the namespace: it matches every name in it.
        ScopeMode::Allow => patterns.iter().any(|p| {
            p.strip_suffix('*')
                .is_some_and(|head| !head.contains('*') && ns.starts_with(head))
        }),
        // No pattern could match a name that starts with the namespace.
        ScopeMode::Deny => !patterns.iter().any(|p| match p.split_once('*') {
            Some((head, _)) => head.starts_with(&ns) || ns.starts_with(head),
            None => p.starts_with(&ns),
        }),
    }
}

/// A run's executor, refusing by name what the caller's scope does not admit
/// **at the moment of the call**.
///
/// A fresh turn cannot reach an out-of-scope name through resolving alone,
/// but two things can: a resumed approval executes the calls a *previous*
/// request stored, and a run that lasts minutes outlives an owner narrowing or
/// deleting the key mid-run. So the scope is re-read per call, like the
/// owner's per-tool switch is. A refusal is logged like any other tool call.
pub struct ScopedExecutor<E> {
    inner: E,
    state: SharedState,
    ctx: RequestCtx,
    proto: &'static str,
}

impl<E> ScopedExecutor<E> {
    /// A device's `lmgw__*` call, checked against its key row as well as
    /// the snapshot (review P-8): between a key write's commit and the
    /// snapshot's reload, the snapshot still holds the switch the owner
    /// just turned off, or a key just disabled. The gateway's self-admin
    /// level is read from the stored settings for the same reason
    /// (2026-10-07): a global level lowered is in force from its commit.
    /// A read that fails refuses.
    async fn device_row_refuses(&self, name: &str) -> Option<String> {
        let Principal::Key {
            id,
            kind: ApiKeyKind::Device,
            name: key,
            ..
        } = &self.ctx.principal
        else {
            return None;
        };
        if !selfadmin::owns(name) {
            return None;
        }
        let who = crate::devices::short_name(key);
        match crate::store::device_admin_now(&self.state.db, *id).await {
            Ok(crate::config::DeviceAdmin::Off) => Some(format!(
                "{name} — device '{who}' is no longer allowed lmgw's admin tools"
            )),
            Ok(level) if selfadmin::writes(name) && level != crate::config::DeviceAdmin::Full => {
                Some(selfadmin::device_level_refusal(name, level))
            }
            Ok(_) => match crate::store::gateway_self_admin_now(&self.state.db).await {
                Ok(global) => crate::ops::check_mode(global, selfadmin::writes(name))
                    .err()
                    .map(|e| format!("{name} — {e}")),
                Err(e) => Some(format!(
                    "{name} — the gateway's self-admin level could not be read ({e}), so the \
                     call was not made"
                )),
            },
            Err(e) => Some(format!(
                "{name} — whether device '{who}' may still use lmgw's admin tools could not be \
                 read ({e}), so the call was not made"
            )),
        }
    }

    pub fn new(inner: E, state: SharedState, ctx: RequestCtx) -> Self {
        Self {
            inner,
            state,
            ctx,
            proto: crate::telemetry::RESPONSES_TOOL_PROTO,
        }
    }

    /// Log a refusal under another surface's tool proto, as the executors it
    /// wraps do (`McpExecutor::with_proto`): a `/v1/realtime` session's calls
    /// are `realtime-tool` rows, refused ones included.
    pub fn with_proto(mut self, proto: &'static str) -> Self {
        self.proto = proto;
        self
    }
}

#[async_trait]
impl<E: ToolExecutor> ToolExecutor for ScopedExecutor<E> {
    async fn call(&self, name: &str, args: &Value) -> ToolOutcome {
        let started = Instant::now();
        let scope = ToolScope::of_request(&self.state, &self.ctx).await;
        let why = if !scope.admits(name) {
            scope.refusal(name)
        } else if let Some(why) = self.device_row_refuses(name).await {
            why
        } else {
            let outcome = self.inner.call(name, args).await;
            // A call made for a request stamped `X-Lmgw-Run` (a container's
            // `/v1/responses` or realtime turn) is one of that run's tool
            // calls, as its `tools/call` on `/mcp` would be (§3.1). A name
            // refused above never reached a tool, so it is none.
            if let Some(run) = self.ctx.run {
                self.state.agent_meters.note_tool_call(run);
            }
            return outcome;
        };
        super::ingress::record_tool_call(
            &self.state,
            &self.ctx,
            self.proto,
            name,
            None,
            started,
            Some(why.clone()),
        )
        .await;
        ToolOutcome::error(why)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApiKey;

    fn snap_with(kind: ApiKeyKind, mode: ScopeMode, patterns: &str) -> Snapshot {
        let mut snap = Snapshot::default();
        snap.api_keys.push(ApiKey {
            id: 7,
            name: "ci".into(),
            key_hash: String::new(),
            enabled: true,
            kind,
            key_plain: None,
            agent_id: None,
            policy: KeyPolicy {
                tool_scope_mode: mode,
                tool_scope_patterns: patterns.into(),
                ..KeyPolicy::default()
            },
            note: String::new(),
            hosts_label: None,
            self_admin: crate::config::DeviceAdmin::Off,
        });
        snap
    }

    fn principal(kind: ApiKeyKind) -> Principal {
        Principal::Key {
            id: 7,
            name: "ci".into(),
            kind,
            agent_id: None,
            fingerprint: String::new(),
        }
    }

    fn scope_in(snap: &Snapshot, kind: ApiKeyKind) -> ToolScope {
        let p = principal(kind);
        ToolScope {
            self_admin: self_admin_of(&p, snap),
            cap: crate::devices::admin_cap(snap, &p),
            list: principal_list(&p, snap),
            hosted: hosted_of(&p, snap, Default::default()),
            spellings: HashMap::new(),
        }
    }

    /// A device the owner allowed lmgw's admin tools reaches the whole
    /// `lmgw` label when its own list says nothing about it; without the
    /// switch, and for a client key, never (client-apps design L5,
    /// 2026-10-07).
    #[test]
    fn the_admin_tools_switch_takes_in_the_whole_label_beside_a_device_s_list() {
        let mut snap = snap_with(ApiKeyKind::Device, ScopeMode::Allow, "docs__*");
        let without = scope_in(&snap, ApiKeyKind::Device);
        assert!(without.admits("docs__search") && !without.admits("lmgw__status"));
        assert!(!without.self_admin());
        snap.api_keys[0].self_admin = crate::config::DeviceAdmin::ReadOnly;
        let with = scope_in(&snap, ApiKeyKind::Device);
        assert!(with.self_admin() && with.admits("lmgw__status"));
        assert!(with.admits("docs__search") && !with.admits("github__search"));
        let client = scope(ApiKeyKind::Key, ScopeMode::All, "");
        assert!(!client.self_admin() && !client.admits("lmgw__status"));
    }

    /// Review P-6: a switched device's own list narrows the label where it
    /// names the namespace — an allow pattern beginning with `lmgw__`, a
    /// deny pattern that can match such a name — and never widens it.
    #[test]
    fn a_switched_device_s_own_patterns_narrow_the_admin_tools() {
        let switched = |mode, patterns: &str| {
            let mut snap = snap_with(ApiKeyKind::Device, mode, patterns);
            snap.api_keys[0].self_admin = crate::config::DeviceAdmin::ReadOnly;
            scope_in(&snap, ApiKeyKind::Device)
        };
        let allow = switched(ScopeMode::Allow, "docs__*\nLMGW__status");
        assert!(allow.admits("lmgw__status") && !allow.admits("lmgw__settings_set"));
        assert!(allow.admits("docs__search"));
        let deny = switched(ScopeMode::Deny, "lmgw__settings_*\ngithub__*");
        assert!(deny.admits("lmgw__status") && !deny.admits("lmgw__settings_set"));
        let broad = switched(ScopeMode::Deny, "*_set");
        assert!(broad.admits("lmgw__status") && !broad.admits("lmgw__settings_set"));
        // Silent about the namespace: the whole label.
        let silent = switched(ScopeMode::Deny, "github__*");
        assert!(silent.admits("lmgw__settings_set"));
        let silent = switched(ScopeMode::Allow, "*search*");
        assert!(silent.admits("lmgw__settings_set"));
        // The refusal names the list, for the owner to find.
        assert!(
            allow
                .refusal("lmgw__settings_set")
                .contains("outside the tool scope of"),
            "{}",
            allow.refusal("lmgw__settings_set")
        );
    }

    /// A client key's scope over a surface where `servers` (id, name,
    /// prefix, tools) offer their tools, as `of_request` reads it.
    fn scope_over(
        mode: ScopeMode,
        patterns: &str,
        servers: &[(i64, &str, &str, &[&str])],
    ) -> ToolScope {
        use rmcp::model::Tool;
        let snap = snap_with(ApiKeyKind::Key, mode, patterns);
        let lists: Vec<Vec<Tool>> = servers
            .iter()
            .map(|(_, _, _, names)| {
                names
                    .iter()
                    .map(|n| Tool::new(n.to_string(), "t", rmcp::model::JsonObject::new()))
                    .collect()
            })
            .collect();
        let ov = HashMap::new();
        let mut inputs: Vec<super::super::AggServerInput<'_>> = servers
            .iter()
            .zip(&lists)
            .map(
                |((id, name, prefix, _), tools)| super::super::AggServerInput {
                    server_id: *id,
                    server_name: name,
                    tool_prefix: prefix,
                    tools,
                    overrides: &ov,
                    device: false,
                },
            )
            .collect();
        let mut agg = super::super::build_aggregate(&mut inputs);
        let mut s = scope_in(&snap, ApiKeyKind::Key);
        s.spellings = spellings_for(&s.list, Some(&mut agg));
        s
    }

    /// A deny list holds for a tool under every name it has or would have
    /// (client-apps design §7.6, review 1): denying `delete` keeps out both
    /// tools a collision moved from it, a pattern over one prefix keeps out
    /// its twins' moved names, and a name denied in its moved spelling keeps
    /// the tool out once nothing collides. An allow list is not carried.
    #[test]
    fn a_deny_list_holds_for_every_name_of_a_tool_and_an_allow_list_for_its_own() {
        let both: &[(i64, &str, &str, &[&str])] = &[
            (1, "alpha", "", &["delete", "read"]),
            (2, "beta", "", &["delete"]),
        ];
        let deny = scope_over(ScopeMode::Deny, "delete", both);
        assert!(!deny.admits("alpha__delete") && !deny.admits("beta__delete"));
        assert!(deny.admits("read"));
        assert!(
            deny.refusal("alpha__delete").contains("a deny list"),
            "{}",
            deny.refusal("alpha__delete")
        );

        let alone: &[(i64, &str, &str, &[&str])] = &[(1, "alpha", "", &["delete", "read"])];
        let moved = scope_over(ScopeMode::Deny, "alpha__delete", alone);
        assert!(
            !moved.admits("delete"),
            "denied as it was called while moved"
        );
        assert!(moved.admits("read"));

        let twins: &[(i64, &str, &str, &[&str])] = &[
            (1, "gh work", "gh", &["search"]),
            (2, "gh-home", "gh", &["search"]),
        ];
        let pattern = scope_over(ScopeMode::Deny, "gh__*", twins);
        assert!(!pattern.admits("gh_work__search") && !pattern.admits("gh-home__search"));

        let allow = scope_over(ScopeMode::Allow, "delete", both);
        assert!(!allow.admits("alpha__delete") && !allow.admits("beta__delete"));
        let allow = scope_over(ScopeMode::Allow, "alpha__delete", both);
        assert!(allow.admits("alpha__delete") && !allow.admits("beta__delete"));
        let allow = scope_over(ScopeMode::Allow, "delete", alone);
        assert!(allow.admits("delete"));
    }

    fn scope(kind: ApiKeyKind, mode: ScopeMode, patterns: &str) -> ToolScope {
        scope_in(&snap_with(kind, mode, patterns), kind)
    }

    fn server(prefix: &str) -> McpServer {
        McpServer {
            id: 1,
            name: format!("{prefix}-server"),
            enabled: true,
            transport: crate::config::McpTransport::Http,
            command: None,
            args: vec![],
            env: vec![],
            cwd: None,
            container_image: None,
            extra_run_args: vec![],
            url: Some("http://127.0.0.1:1/mcp".into()),
            headers: vec![],
            tool_prefix: prefix.into(),
            timeout_ms: 1_000,
            autostart: false,
            idle_seconds: 0,
            allow_sampling: false,
            sampling_alias: None,
            agent_id: None,
            device_key_id: None,
        }
    }

    #[test]
    fn a_client_key_allow_list_admits_its_globs_and_nothing_else() {
        let s = scope(ApiKeyKind::Key, ScopeMode::Allow, "docs__*\ngithub__search");
        assert!(s.admits("docs__query"));
        assert!(s.admits("github__search"));
        assert!(!s.admits("github__delete_repo"));
        assert!(
            s.refusal("github__delete_repo")
                .contains("key 'ci' (tool scope: allow docs__*, github__search)"),
            "{}",
            s.refusal("github__delete_repo")
        );
    }

    #[test]
    fn a_client_key_deny_list_admits_everything_but_its_globs_and_does_not_quote_them() {
        let s = scope(ApiKeyKind::Key, ScopeMode::Deny, "github__*");
        assert!(s.admits("docs__query"));
        assert!(!s.admits("github__search"));
        let r = s.refusal("github__search");
        assert!(r.contains("a deny list"), "{r}");
        assert!(
            !r.contains("github__*"),
            "the deny list names what it hides: {r}"
        );
    }

    #[test]
    fn a_device_is_scoped_like_a_client_key_and_never_reaches_self_admin() {
        // Client-apps design §1.1: the client key's arm, its own list, and
        // no `Admin` to derive self-admin from.
        let s = scope(ApiKeyKind::Device, ScopeMode::Allow, "docs__*\nlmgw__*");
        assert!(s.admits("docs__query"));
        assert!(!s.admits("github__search"));
        assert!(!s.admits("lmgw__status"));
        let all = scope(ApiKeyKind::Device, ScopeMode::All, "");
        assert!(all.admits("github__search"));
        assert!(!all.admits("lmgw__status"));
    }

    #[test]
    fn a_client_key_never_reaches_self_admin_even_when_its_list_names_it() {
        let s = scope(ApiKeyKind::Key, ScopeMode::Allow, "lmgw__*");
        assert!(!s.admits("lmgw__status"));
        assert!(s.refusal("lmgw__status").contains("owner credential"));
        let all = scope(ApiKeyKind::Key, ScopeMode::All, "");
        assert!(all.admits("github__search"));
        assert!(!all.admits("lmgw__status"));
    }

    #[test]
    fn an_owner_key_is_not_scoped_even_if_a_row_says_otherwise() {
        // `key_set` refuses a tool scope on an owner row; a row that carries
        // one anyway (hand-edited DB) is still not a client.
        let s = scope(ApiKeyKind::Owner, ScopeMode::Allow, "nothing");
        assert!(s.admits("github__search"));
        assert!(s.admits("lmgw__status"));
    }

    #[test]
    fn no_credential_sees_everything_but_self_admin() {
        let snap = Snapshot::default();
        let s = ToolScope {
            self_admin: Principal::Anonymous.holds(Cap::Admin, &snap),
            cap: SelfAdmin::Full,
            list: principal_list(&Principal::Anonymous, &snap),
            hosted: hosted_of(&Principal::Anonymous, &snap, Default::default()),
            spellings: HashMap::new(),
        };
        assert!(s.admits("github__search"));
        assert!(!s.admits("lmgw__status"));
        assert!(ToolScope::gateway().admits("lmgw__status"));
    }

    #[test]
    fn every_principal_without_a_list_to_read_reaches_nothing() {
        // Deleted between the door and the read: the row is gone.
        let gone = scope_in(&Snapshot::default(), ApiKeyKind::Key);
        assert!(!gone.admits("docs__query"));
        assert!(
            gone.refusal("docs__query").contains("no longer exists"),
            "{}",
            gone.refusal("docs__query")
        );
        for kind in [ApiKeyKind::Agent, ApiKeyKind::Internal] {
            let s = scope(kind, ScopeMode::All, "");
            assert!(!s.admits("docs__query"), "{kind:?}");
            assert!(!s.may_reach(&server("github")), "{kind:?}");
        }
    }

    #[test]
    fn a_server_is_worth_waking_only_when_the_namespace_can_match() {
        let allow = scope(
            ApiKeyKind::Key,
            ScopeMode::Allow,
            "docs__*\ngit*\nboard__pin",
        );
        assert!(
            allow.may_reach(&server("github")),
            "git* can match github__…"
        );
        assert!(allow.may_reach(&server("board")), "board__pin is in it");
        assert!(!allow.may_reach(&server("slack")));
        let star = scope(ApiKeyKind::Key, ScopeMode::Allow, "*__search");
        assert!(
            star.may_reach(&server("slack")),
            "a leading * can match anything"
        );
        assert!(
            star.may_reach(&server("")),
            "a bare server has no namespace to read"
        );

        let deny = scope(ApiKeyKind::Key, ScopeMode::Deny, "git*\nslack__post");
        assert!(
            !deny.may_reach(&server("github")),
            "git* denies all of github__…"
        );
        assert!(
            deny.may_reach(&server("slack")),
            "one tool denied is not the server"
        );
        assert!(ToolScope::gateway().may_reach(&server("slack")));
    }
}
