//! The `transfer.updated` record (device transfers design §6): a transfer
//! between devices was offered, became `ready` or ended, written in the
//! transaction of that change.
//!
//! **Who hears of it** is decided at delivery (`web::chat_feed::transfers`)
//! from what the record names: the sender's and the receiver's key ids, and
//! whether the receiver had been offered the transfer by this write (`ready`
//! at least once). The owner reads every one. The record names no thread or
//! folder, its level is 0, and its `by` is empty: it shows no device to the
//! readers `device.revoked` counts (`store::feed::devices`).

use serde_json::{json, Value};
use sqlx::SqliteConnection;

use super::super::DbResult;
use super::{kind, Record};

/// What a `transfer.updated` record names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferFacts {
    pub id: String,
    /// The sender's key id.
    pub from: i64,
    /// The receiver's key id.
    pub to: i64,
    /// The receiver had been offered it by this write: it hears of it.
    pub offered: bool,
}

/// Record `transfer.updated` for `facts`, on `conn` — the transaction of
/// the change.
pub async fn record_transfer(conn: &mut SqliteConnection, facts: &TransferFacts) -> DbResult<()> {
    super::record(
        conn,
        super::Change {
            kind: kind::TRANSFER_UPDATED,
            admin: 0,
            by: None,
            detail: Some(
                json!({
                    "transfer_id": facts.id,
                    "from": facts.from,
                    "to": facts.to,
                    "offered": facts.offered,
                })
                .to_string(),
            ),
            ..Default::default()
        },
    )
    .await
}

impl Record {
    /// A `transfer.updated` record's facts; `None` for any other record.
    pub fn transfer(&self) -> Option<TransferFacts> {
        if self.kind != kind::TRANSFER_UPDATED {
            return None;
        }
        let v: Value = serde_json::from_str(self.detail.as_deref()?).ok()?;
        Some(TransferFacts {
            id: v.get("transfer_id")?.as_str()?.to_string(),
            from: v.get("from")?.as_i64()?,
            to: v.get("to")?.as_i64()?,
            offered: v.get("offered").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}
