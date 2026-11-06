//! The `transfer.updated` record as a reader receives it (device transfers
//! design §6): the transfer as it is at delivery, for its sender, for its
//! receiver once it was offered (`ready` at the record's write), and for the
//! owner; for no other device. A transfer whose row is gone (its retention
//! passed) renders as nothing.

use serde_json::Value;

use crate::state::AppState;
use crate::store::feed::Record;

/// The data of `r` (a `transfer.updated` record) for the reader of device
/// key `reader` (`None`: the owner); `None` when the reader does not hear
/// of it.
pub(super) async fn render(
    state: &AppState,
    r: &Record,
    reader: Option<i64>,
) -> Result<Option<Value>, crate::error::GatewayError> {
    let Some(facts) = r.transfer() else {
        return Ok(None);
    };
    let hears = match reader {
        None => true,
        Some(k) => k == facts.from || (k == facts.to && facts.offered),
    };
    if !hears {
        return Ok(None);
    }
    let Some(row) = crate::store::transfers::get(&state.db, &facts.id).await? else {
        return Ok(None);
    };
    Ok(Some(super::super::chat_wire::wire(
        &crate::transfers::wire(state, &row).await,
    )))
}
