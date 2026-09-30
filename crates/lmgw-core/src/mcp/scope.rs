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

use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::{ToolExecutor, ToolOutcome};
use crate::config::{ApiKeyKind, KeyPolicy, McpServer, ScopeMode, Snapshot};
use crate::principal::{Cap, Principal};
use crate::proxy::RequestCtx;
use crate::state::SharedState;

use super::selfadmin;

/// One caller's reach into the tool plane.
#[derive(Debug, Clone)]
pub struct ToolScope {
    /// May this caller use the `lmgw__*` self-admin tools? Only an owner
    /// credential (principals §3.7) or the gateway itself. Whether a plane
    /// serves them at all is the plane's business — `/mcp` never does.
    self_admin: bool,
    list: List,
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
            list: List::All,
        }
    }

    /// The scope of one HTTP request, from the principal the router resolved.
    ///
    /// Read from the current snapshot every time it is asked for: a key's
    /// scope is edited on a live gateway, and neither a session nor a running
    /// tool loop that outlived the edit may keep the old answer.
    pub async fn of_request(state: &SharedState, ctx: &RequestCtx) -> Self {
        let snap = state.snapshot();
        let self_admin = ctx.principal.holds(Cap::Admin, &snap);
        // An agent token, whether or not `auth_enabled` is on (§3.1).
        if let Some(agent) = &ctx.agent {
            return Self {
                self_admin,
                list: agent_list(state, &agent.agent_id).await,
            };
        }
        Self {
            self_admin,
            list: principal_list(&ctx.principal, &snap),
        }
    }

    /// The same scope against the surface as it is *now*. Only an agent's
    /// list depends on the surface — a label with no `allowed` is whatever its
    /// server offers — so a run that just woke a service agent re-reads it.
    pub async fn refresh(&self, state: &SharedState) -> Self {
        match &self.list {
            List::Agent { agent_id, .. } => Self {
                self_admin: self.self_admin,
                list: agent_list(state, agent_id).await,
            },
            _ => self.clone(),
        }
    }

    /// Is `name` within this caller's reach? Says nothing about whether the
    /// gateway offers it — that is `tool_disabled` and the source's own state.
    pub fn admits(&self, name: &str) -> bool {
        if selfadmin::owns(name) && !self.self_admin {
            return false;
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

    /// May this caller attach the `lmgw` self-admin toolset?
    pub fn self_admin(&self) -> bool {
        self.self_admin
    }

    /// Does this caller have a list of its own, beyond the owner's switch?
    pub fn narrows(&self) -> bool {
        !matches!(self.list, List::All)
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
                ScopeMode::Deny => format!("key '{name}' (tool scope: a deny list)"),
                _ => format!(
                    "key '{name}' (tool scope: allow {})",
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
        if selfadmin::owns(name) && !self.self_admin {
            return format!(
                "{name} — the lmgw__* self-admin tools need an owner credential; a client key \
                 or an agent token does not reach the gateway's configuration"
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

/// The list for every principal that is not an agent identity. Each shape is
/// named: a default here would be a fail-open the day a new one appears.
fn principal_list(principal: &Principal, snap: &Snapshot) -> List {
    match principal {
        Principal::Anonymous
        | Principal::Key {
            kind: ApiKeyKind::Owner,
            ..
        } => List::All,
        Principal::Key {
            id,
            name,
            kind: ApiKeyKind::Key,
            ..
        } => match snap.api_keys.iter().find(|k| k.id == *id) {
            Some(k) if k.policy.tool_scope_mode == ScopeMode::All => List::All,
            Some(k) => List::Key {
                name: k.name.clone(),
                policy: k.policy.clone(),
            },
            // Resolved at the door, deleted since: the owner took it away.
            None => List::Nothing {
                who: format!("key '{name}', which no longer exists,"),
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
    pub fn new(inner: E, state: SharedState, ctx: RequestCtx) -> Self {
        Self {
            inner,
            state,
            ctx,
            proto: crate::telemetry::RESPONSES_TOOL_PROTO,
        }
    }
}

#[async_trait]
impl<E: ToolExecutor> ToolExecutor for ScopedExecutor<E> {
    async fn call(&self, name: &str, args: &Value) -> ToolOutcome {
        let started = Instant::now();
        let scope = ToolScope::of_request(&self.state, &self.ctx).await;
        if scope.admits(name) {
            return self.inner.call(name, args).await;
        }
        let why = scope.refusal(name);
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
        });
        snap
    }

    fn principal(kind: ApiKeyKind) -> Principal {
        Principal::Key {
            id: 7,
            name: "ci".into(),
            kind,
            agent_id: None,
        }
    }

    fn scope_in(snap: &Snapshot, kind: ApiKeyKind) -> ToolScope {
        let p = principal(kind);
        ToolScope {
            self_admin: p.holds(Cap::Admin, snap),
            list: principal_list(&p, snap),
        }
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
            list: principal_list(&Principal::Anonymous, &snap),
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
