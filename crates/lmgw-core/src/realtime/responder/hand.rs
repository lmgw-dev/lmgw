//! A response's chat call handed on at the send as the session's request
//! (openai egress design §14.4): an exhausted quota or an unreachable
//! upstream goes to the hop's fallback through the realtime route's check
//! — a fallback that cannot answer `/v1/realtime`'s turn is never taken —
//! as the owner's traffic, along the chain the call's route was opened
//! with, and a call the gate re-routes carries that route's chain on.

use crate::erased::Erased;
use crate::error::GatewayError;
use crate::gate::{GateHeaders, RouteCheck};
use crate::ir::ChatRequest;
use crate::proxy::reasoning_fit::Handover;
use crate::proxy::PerRoute;
use crate::runtime::registry::Origin;

/// The call's say at each route it lands on: the request as it is, and the
/// session's hand-over (module doc).
pub(super) struct Session {
    hand: Handover,
}

impl Session {
    /// For a call whose route was opened with `headers`.
    pub(super) fn opened(headers: &GateHeaders) -> Self {
        Self {
            hand: Handover {
                check: RouteCheck::Text(super::super::PATH),
                origin: Origin::Owner,
                chain: headers.chain().clone(),
                deferred: None,
            },
        }
    }
}

impl PerRoute for Session {
    fn request<'a>(
        &'a self,
        _route: &'a crate::config::Route,
        _hold: Option<&'a crate::vram::LocalHold>,
        _rerouted: Option<&'a GateHeaders>,
        _ir: &'a ChatRequest,
    ) -> Erased<'a, Result<Option<ChatRequest>, GatewayError>> {
        Box::pin(async { Ok(None) })
    }

    fn handover(&self, rerouted: Option<&GateHeaders>) -> Handover {
        let mut hand = self.hand.clone();
        if let Some(h) = rerouted {
            hand.chain = h.chain().clone();
        }
        hand
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HopKey;
    use crate::gate::FallbackReason;

    #[test]
    fn a_call_hands_on_with_the_realtime_check_and_its_routes_chain() {
        let mut chain = crate::gate::chain::Chain::default();
        chain.leave("a", HopKey::Alias(1));
        chain.push("b".into(), HopKey::Alias(2), FallbackReason::Quota);
        let opened = GateHeaders::along(chain);
        let s = Session::opened(&opened);
        let hand = s.handover(None);
        assert_eq!(hand.check, RouteCheck::Text(super::super::super::PATH));
        assert_eq!(hand.origin, Origin::Owner);
        assert_eq!(hand.chain, *opened.chain());

        let rerouted = GateHeaders::default();
        assert_eq!(s.handover(Some(&rerouted)).chain, *rerouted.chain());
    }
}
