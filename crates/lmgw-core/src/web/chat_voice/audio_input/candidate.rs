//! The verdict for a candidate alias the resolve did not swap (§2.2,
//! candidate-aliases design §4.2–§4.3): read from the gate's own pick
//! (`candidates::derive::cached_pick`), as the walk reads it.
//!
//! - **Its fallback by design.** A background alias goes to its alias
//!   fallback whenever its primary is not loaded and cannot be started into
//!   room that disturbs nobody; an owner alias whose primary it cannot use
//!   at all (missing, disabled, lacking a facet the alias enables) goes there
//!   whenever no other candidate is loaded. With a fallback the walk may
//!   take — it resolves, lmgw does not run it, and it takes every facet the
//!   alias enables — such a turn may reach a model lmgw does not run, and
//!   gets the transcript.
//! - **Its guards.** Only a routable candidate can answer: a disabled or
//!   missing guarded row says nothing about the alias.
//! - **Who may answer**: the routable candidates, by the public names a turn
//!   names its pick with — what the session's refusal memory is matched
//!   against.

use std::sync::Arc;

use crate::candidates::derive::{cached_pick, CandidatePick};
use crate::config::{CandidateAlias, FallbackRoute, Snapshot};
use crate::state::SharedState;

/// One candidate alias and the gate's pick for it.
pub(super) struct Walk<'a> {
    ca: &'a CandidateAlias,
    pick: CandidatePick,
}

impl<'a> Walk<'a> {
    pub(super) async fn read(
        state: &SharedState,
        snap: &Arc<Snapshot>,
        ca: &'a CandidateAlias,
    ) -> Walk<'a> {
        Walk {
            ca,
            pick: cached_pick(state, snap, ca).await,
        }
    }

    /// Why the walk may end at the alias fallback by design (module doc), or
    /// `None`.
    pub(super) fn may_fall_back(&self, snap: &Snapshot) -> Option<String> {
        let fallback = match snap.alias_fallback(self.ca) {
            FallbackRoute::Usable { alias, .. } if self.pick.fallback_usable => alias,
            _ => return None,
        };
        let lead = if self.ca.background {
            "as background traffic this".to_string()
        } else {
            let (primary, reason) = self.pick.primary_skipped.as_ref()?;
            format!("its primary '{primary}' {}, so this", reason.describe())
        };
        Some(format!(
            "{lead} may go to {fallback}, {}{}",
            super::NOT_RUN,
            super::STAYS
        ))
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
