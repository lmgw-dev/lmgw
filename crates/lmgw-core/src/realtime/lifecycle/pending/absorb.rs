//! The follow-up after an abandoned call (realtime-server-tools design
//! §2.5). `@openai/agents` sends `response.create` on every `mcp_call`'s
//! `conversation.item.done`, whatever the call's outcome — so also after a
//! barge-in abandoned one. When that barge-in already carried the cut
//! response's own create again (nobody heard any of it, `interrupt`), the
//! one-held-create rule would refuse the SDK's as
//! `conversation_already_has_active_response`: the error the module doc
//! says a stock client must not get on a barge-in. So a client's
//! `response.create` that arrives while the held create is such a re-carry
//! is **absorbed** into it, with no error: the held create answers the same
//! turn and renders the same conversation, the abandoned call included.
//!
//! **What it never swallows** — a create that asks for something the held
//! one would not give. It is absorbed when it carries:
//! - no overrides of its own (the SDK's follow-up has none): the held
//!   create keeps its own;
//! - the held create's own overrides;
//! - overrides where the held create had none: the newer request is the
//!   client's, and it is judged now, as a create held for a turn is when it
//!   arrives.
//!
//! Overrides that differ from the held create's are two requests, and the
//! newer is refused as before: visibly, never dropped. The held create takes
//! the absorbed one's `event_id`: that is the create the client waits on
//! (`@openai/agents` matches `response.created` and errors to its id), the
//! older one's response having ended already.
//!
//! It absorbs once. The held create is then the client's newest, and a
//! further create meanwhile is refused as any second one is.
//!
//! **It keys on the re-carry, not on tools** (final review #7): a create
//! sent while any cut nobody heard is carried joins it, whether or not the
//! cut response called a tool. That is safe — the held create has not
//! rendered yet, so it answers the newer request too — and it is what the
//! realtime design's §4.3 says.

use super::super::super::protocol::{ErrorObject, ResponseCreateParams};
use super::super::{Core, Create};

impl Core {
    /// A client's `create` while one is held for the user's turn: absorbed
    /// into a re-carried one (module doc), or refused.
    pub(in crate::realtime::lifecycle) fn pending_second(&mut self, create: Create) {
        match self.absorbed_params(&create) {
            None => self.pending_refuse(create),
            Some(Err(e)) => self.error(e.for_event(create.event_id.as_deref())),
            Some(Ok(params)) => {
                tracing::debug!(
                    "realtime {}: the client's response.create joins the cut response's own, \
                     held for the turn — it answers the same turn (an abandoned call's \
                     follow-up)",
                    self.id()
                );
                if let Some(p) = self.pending.as_mut() {
                    if let Some(held) = p.carried.as_mut() {
                        held.event_id = create.event_id;
                        held.params = params;
                    }
                    p.again = false;
                }
            }
        }
    }

    /// The overrides the held create starts with once `create` is absorbed
    /// into it; `None` when it is not (no re-carry is held, or the two ask
    /// for different things), `Some(Err)` when `create`'s own cannot start.
    fn absorbed_params(
        &self,
        create: &Create,
    ) -> Option<Result<Option<ResponseCreateParams>, ErrorObject>> {
        let held = self
            .pending
            .as_ref()
            .filter(|p| p.again)?
            .carried
            .as_ref()?;
        match (overrides(held), overrides(create)) {
            (kept, None) => Some(Ok(kept)),
            (Some(a), Some(b)) if a == b => Some(Ok(Some(a))),
            (None, Some(b)) => Some(self.snapshot(Some(&b)).map(|_| Some(b))),
            (Some(_), Some(_)) => None,
        }
    }
}

/// What `c` asks for beyond the session's config: `response: {}` asks for
/// nothing.
fn overrides(c: &Create) -> Option<ResponseCreateParams> {
    c.params
        .clone()
        .filter(|p| *p != ResponseCreateParams::default())
}
