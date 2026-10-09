//! Who a forwarded call runs as (§5.5, L12): `_meta["lmgw/caller"]` and
//! `_meta["lmgw/approval"]` on every `tools/call` lmgw sends over a device
//! link, and on no other server's calls.
//!
//! `caller` is the principal the call runs as: the turn's starter for a Chat
//! turn (L4), the presenter for `/mcp`, the request's key for
//! `/v1/responses` and realtime, the gateway for its own runs. `approval` is
//! set when an approval decided the call, naming the approver (F, api WP9;
//! nothing decides a device call by approval before it, so it is `null`).
//!
//! It also carries what a call that becomes an MCP task needs (MCP Tasks
//! design T4, T20): where the task's result goes ([`CallFrom::late`]) and
//! who the task's request rows are written under ([`CallFrom::logged_as`]).

use lmgw_api_types::mcp_host::{Approval, CallerKind, Principal as Who};

use crate::config::ApiKeyKind;
use crate::principal::Principal;
use crate::proxy::RequestCtx;

/// Who a call runs as and who approved it, as a device reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallFrom {
    pub caller: Who,
    pub approval: Option<Approval>,
    /// The stored Chat thread a task this call starts belongs to: the late
    /// path. `None`: the bridge (MCP Tasks design T4).
    pub late: Option<crate::mcp::tasks::Late>,
    /// Who a task's request rows are written under (T20).
    pub logged_as: crate::mcp::tasks::LoggedAs,
}

impl CallFrom {
    /// The gateway's own runs on the owner's behalf: the dashboard's Chat
    /// turns, in-process agent runs.
    pub fn gateway() -> Self {
        Self {
            caller: Who {
                kind: CallerKind::Gateway,
                name: "lmgw".into(),
            },
            approval: None,
            late: None,
            logged_as: Default::default(),
        }
    }

    /// The principal of a request's context. A context with no key is
    /// `anonymous`, never the gateway: an anonymous request never reaches a
    /// device row (L16), and should it ever, a device must read it as the
    /// least trusted caller, not as the owner's own runs. The gateway's
    /// in-process callers, which carry no key either, say so themselves
    /// ([`Self::gateway`], `McpExecutor::with_caller`).
    pub fn of(ctx: &RequestCtx) -> Self {
        let caller = match &ctx.principal {
            Principal::Anonymous => Who {
                kind: CallerKind::Anonymous,
                name: "anonymous".into(),
            },
            Principal::Key {
                kind,
                name,
                agent_id,
                ..
            } => match kind {
                ApiKeyKind::Owner => Who {
                    kind: CallerKind::Owner,
                    name: name.strip_prefix("owner:").unwrap_or(name).to_string(),
                },
                ApiKeyKind::Device => Who {
                    kind: CallerKind::Device,
                    name: crate::devices::short_name(name).to_string(),
                },
                ApiKeyKind::Agent => Who {
                    kind: CallerKind::Agent,
                    name: agent_id.clone().unwrap_or_else(|| name.clone()),
                },
                ApiKeyKind::Key => Who {
                    kind: CallerKind::Key,
                    name: name.clone(),
                },
                ApiKeyKind::Internal => return Self::gateway(),
            },
        };
        Self {
            caller,
            approval: None,
            late: None,
            logged_as: crate::mcp::tasks::LoggedAs {
                client_key: ctx.client_key.clone(),
                ..Default::default()
            },
        }
    }

    /// With the approval that decided the call (F).
    pub fn approved_by(mut self, by: Who) -> Self {
        self.approval = Some(Approval {
            decision: "approved".into(),
            by,
        });
        self
    }
}
