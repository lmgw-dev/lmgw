//! The Responses-wire answers of lmgw's own sends, recorded like native ones
//! (openai egress design §7.2.3): an `openai` row's chat send on the
//! Responses wire answers with a response id and output item ids
//! ([`UpstreamItems`]), which go into `native_responses` (not stored: the
//! sends say `store: false`, so the upstream keeps nothing to fetch or
//! continue) and `native_response_items` under the request's key. That is
//! what lets a client key send back the items it was given (a reasoning
//! item's seal, §7.3) and nothing else: the key check of
//! `responses/references.rs` reads these rows.
//!
//! Every send site records before its answer leaves for the client, so a
//! client that sends the items straight back finds them: the whole answer's
//! before it is handed over, a public stream's as the event passes, ahead
//! of the stop. The write runs as a task of its own
//! ([`crate::detached::detached`]), so a caller dropped mid-way (a client
//! hang-up, a stopped turn) does not lose it; a failed write is a WARN, as
//! the native path's is: the answer itself is fine.

//!
//! An in-process caller without a key (the owner's Chat tab, lmgw's own
//! consumers: MCP sampling, the knowledge and quickdoc readers) records
//! nothing ([`remember_in_process`]): `NULL` is the anonymous principal's
//! scope, which an anonymous request's item check reads, and the owner's
//! reader checks no items at all (`ResponseReader::Owner`, the seal's
//! check of §7.3 alike), so a row under `NULL` would only let an anonymous
//! request (auth off) send back the owner's items. The one in-process
//! caller that is a client's own request, the `/v1/responses` loop
//! (`KeyRef::public`), records under its key as a public request does, none
//! included, so an anonymous loop client sends back its own seals.

use crate::config::Route;
use crate::ir::UpstreamItems;
use crate::state::SharedState;

/// Record `items`, the answer to a request by `key_id` for `alias` on
/// `route`.
pub(crate) async fn remember(
    state: &SharedState,
    route: &Route,
    key_id: Option<i64>,
    alias: &str,
    items: &UpstreamItems,
) {
    if items.response_id.is_empty() {
        return;
    }
    let db = state.db.clone();
    let record = crate::store::NativeResponse {
        id: items.response_id.clone(),
        upstream_id: route.upstream.id,
        key_id,
        model: alias.to_string(),
        upstream_model: route.upstream_model.clone(),
        stored: false,
    };
    let item_list = items.items.clone();
    let _ = crate::detached::detached("recording an answer's items", async move {
        let id = record.id.clone();
        if let Err(e) = crate::store::insert_native_response(&db, &record).await {
            tracing::warn!("recording the answer {id}: {e}");
            return;
        }
        if let Err(e) =
            crate::store::insert_native_response_items(&db, &id, key_id, &item_list).await
        {
            tracing::warn!("recording the items of the answer {id}: {e}");
        }
    })
    .await;
}

/// [`remember`] for an in-process caller: under its key, and not at all for
/// one of lmgw's own without a key (module doc).
pub(crate) async fn remember_in_process(
    state: &SharedState,
    route: &Route,
    key: &super::KeyRef,
    alias: &str,
    items: &UpstreamItems,
) {
    if key.id.is_some() || key.public {
        remember(state, route, key.id, alias, items).await;
    }
}
