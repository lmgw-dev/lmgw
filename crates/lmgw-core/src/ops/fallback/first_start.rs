//! The type check migration 0095 left to the first start after it (openai
//! egress design §14.10): the guard ran before the gateway did, with no
//! upstream catalogs, so it typed only the fallbacks that name a local row.
//! 0095 writes the marker row `fallback_type_check`; the first start that
//! sees it types every other fallback the way the save check does
//! ([`super::validate`]'s item 4), logs one WARN per fallback of another
//! type, and removes the marker — a start that stopped before it checks
//! again. A type lmgw cannot know is not named: the per-request endpoint
//! check guards it.

use crate::config::HopKey;
use crate::state::SharedState;
use crate::store;

use super::types::{facts_of, local_type, ModelType};

/// The marker row 0095 writes into the `settings` table.
pub(crate) const TYPE_CHECK_KEY: &str = "fallback_type_check";

/// Run the check once, if 0095 asked for it (module doc).
pub(crate) async fn first_start_type_check(state: &SharedState) {
    match store::get_kv(&state.db, TYPE_CHECK_KEY).await {
        Ok(Some(_)) => {}
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("reading the fallback type check's marker failed: {e}");
            return;
        }
    }
    let snap = state.snapshot();
    for o in snap.fallback_owners() {
        // A local target the guard typed already.
        if matches!(snap.hop_key(&o.fallback), Some(HopKey::Local(..))) {
            continue;
        }
        let owner = match &o.key {
            HopKey::Local(class, id) => local_type(&snap, *class, id),
            HopKey::Candidate(_) => Some(ModelType::chat()),
            key => match snap.hop_name(key) {
                Some(name) => facts_of(state, &snap, &name).await.ty,
                None => None,
            },
        };
        let target = facts_of(state, &snap, &o.fallback).await.ty;
        if let (Some(owner), Some(target)) = (owner, target) {
            if owner != target {
                tracing::warn!(
                    "migration 0095: {} falls back to '{}', {}; a fallback must be the same type \
                     of model ({} here), so the next save of the row refuses it until it is \
                     changed. It is kept as it is: a request that reaches it is refused as one \
                     its endpoint cannot serve",
                    o.label,
                    o.fallback,
                    target.describe(),
                    owner.describe()
                );
            }
        }
    }
    if let Err(e) = store::delete_kv(&state.db, TYPE_CHECK_KEY).await {
        tracing::warn!("removing the fallback type check's marker failed: {e}");
    }
}
