//! The verdict for a candidate alias the resolve did not swap (§2.2,
//! candidate-aliases design §4.2–§4.3): read from the gate's own pick
//! (`candidates::derive::cached_pick`), as the walk reads it.
//!
//! - **Its guards.** Only a routable candidate can answer: a disabled or
//!   missing guarded row says nothing about the alias.
//! - **Who may answer**: the routable candidates, by the public names a turn
//!   names its pick with — what the session's refusal memory is matched
//!   against. The alias fallback the walk may take is not among them: it
//!   answers only when no candidate can, and a memory that waited for it
//!   too would keep sending audio to candidates that refused it.
//!
//! What the walk's fallback hears needs no forecast of its own (changed
//! 2026-10-06, capability not locality): with the Audio facet on, a
//! fallback that does not take audio — an Anthropic one included — counts
//! as none (§4.6), so the models the walk can reach hear as published; with
//! it off, the alias does not. A running server's `/props` may still say
//! otherwise: the send judges the pick itself, and a mismatch is refused
//! there and retried as the transcript.

use std::sync::Arc;

use crate::candidates::derive::{cached_pick, CandidatePick};
use crate::config::{CandidateAlias, Snapshot};
use crate::state::SharedState;

/// One candidate alias and the gate's pick for it.
pub(super) struct Walk {
    pick: CandidatePick,
}

impl Walk {
    pub(super) async fn read(
        state: &SharedState,
        snap: &Arc<Snapshot>,
        ca: &CandidateAlias,
    ) -> Walk {
        Walk {
            pick: cached_pick(state, snap, ca).await,
        }
    }

    /// Whether the walk may take the alias fallback at all: it is usable as
    /// a fallback (`Snapshot::alias_fallback`), and takes every facet the
    /// alias enables.
    pub(super) fn fallback_usable(&self) -> bool {
        self.pick.fallback_usable
    }

    /// The first routable candidate that guards its context, or `None`.
    pub(super) fn guard(&self, snap: &Snapshot) -> Option<String> {
        self.pick
            .routable
            .iter()
            .find_map(|id| super::guard(snap, id))
    }

    /// The models an audio turn may reach: the routable candidates, by
    /// their public names, as a turn names its pick.
    pub(super) fn via(&self, snap: &Snapshot) -> Vec<String> {
        self.pick
            .routable
            .iter()
            .map(|id| snap.local_public_name(id))
            .collect()
    }
}
