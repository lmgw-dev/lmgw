//! The resolver a chat send asks once (llama egress design §3.2).

use std::sync::Arc;

use crate::config::{Protocol, Route, Upstream};
use crate::egress::llama_cpp::props::LlamaFacts;
use crate::state::SharedState;
use crate::vram::LocalHold;

/// What a chat send knows about the llama-server it goes to (§3.2): what
/// `gate::fit_chat` decides the frozen tool-image decision on (§8.2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerFacts {
    /// The server's `/props` facts; `None` while unknown (decision 14).
    pub facts: Option<Arc<LlamaFacts>>,
    /// A managed row's projector ubatch advisory, as the container was
    /// started (`LlamaEntry::ubatch_advisory`). Always `None` on an external
    /// row, whose projector lmgw cannot read.
    pub ubatch_advisory: Option<String>,
}

/// What lmgw knows about the llama-server `route` goes to, for one chat send
/// — never waiting:
/// - **managed** (`hold` is the request's claim): the container's registry
///   entry, by its generation ([`LocalHold::llama`]);
/// - **an external `llama_cpp` row**: the background cache
///   ([`super::ExternalFacts::lookup`]), which answers unknown the first time
///   and asks the server behind the request's back;
/// - anything else: nothing.
pub fn resolve(state: &SharedState, hold: Option<&LocalHold>, route: &Route) -> ServerFacts {
    if let Some(hold) = hold {
        return hold
            .llama()
            .map(|entry| ServerFacts {
                facts: entry.facts().cloned(),
                ubatch_advisory: entry.ubatch_advisory,
            })
            .unwrap_or_default();
    }
    if !is_external_llama(&route.upstream) {
        return ServerFacts::default();
    }
    ServerFacts {
        facts: state
            .llama_facts
            .lookup(&state.http, &route.upstream, &route.upstream_model),
        ubatch_advisory: None,
    }
}

/// A stored `llama_cpp` row: one the owner points at a llama-server lmgw
/// does not run. The synthetic upstreams of managed models have negative ids
/// (`ROUTER_UPSTREAM_ID`, …) and are only ever reached through a hold.
pub fn is_external_llama(up: &Upstream) -> bool {
    up.protocol == Protocol::LlamaCpp && up.id > 0
}
