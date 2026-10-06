//! What a response runs of the session's MCP tools (realtime-server-tools
//! design §2.1, §2.4), decided at its launch from what it offers: its
//! output classifies a call of an offered name as server-side by the
//! tool's [`Owner`] — the label the client wrote and the wire name — and
//! its responder runs exactly those names, the built-in ones in-process and
//! a registered server's on the server it was listed from.

use std::collections::HashMap;

use super::super::responder::ServerTools;
use super::table::Owner;
use super::{McpOffer, McpTable};

/// A response's server-side tools.
pub(crate) struct Served {
    /// By exposed name: what the output classifies by.
    pub owners: HashMap<String, Owner>,
    /// What the responder runs them with.
    pub tools: ServerTools,
}

impl McpTable {
    /// What a response that offers `offer` runs server-side; `parallel` is
    /// its `parallel_tool_calls` (on unless the client said otherwise).
    pub fn served(&self, offer: &McpOffer, parallel: Option<bool>) -> Served {
        let owners: HashMap<String, Owner> = offer
            .tools
            .iter()
            .filter_map(|t| self.owner(&t.name).map(|o| (t.name.clone(), o.clone())))
            .collect();
        let tools = ServerTools {
            names: owners.keys().cloned().collect(),
            builtin: self.builtin(),
            servers: owners
                .iter()
                .filter_map(|(name, o)| Some((name.clone(), o.server?)))
                .collect(),
            parallel: parallel.unwrap_or(true),
        };
        Served { owners, tools }
    }
}
