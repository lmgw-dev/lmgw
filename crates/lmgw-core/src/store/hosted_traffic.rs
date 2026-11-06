//! The traffic hosted-tool fees left unpriced (openai egress design §6.1):
//! the raw rows the Prices page's list of hosted-tool fees is read from.

use sqlx::{Row, SqlitePool};

use super::*;

/// One row that came back unpriced for its hosted-tool calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedFeeRow {
    /// The alias the request asked for.
    pub alias: String,
    pub upstream_id: Option<i64>,
    pub upstream_model: Option<String>,
    /// The row's `price_note`, which names the calls
    /// ([`crate::pricing::HostedCalls::from_note`]).
    pub price_note: String,
    /// When it was written.
    pub ts: String,
}

/// The query behind [`hosted_fee_traffic`]; `?1` is
/// [`crate::pricing::HOSTED_FEES_NOTE`]. Its `price_note IS NOT NULL` term
/// lets it read the partial index of noted rows (migration 0092) instead of
/// the whole log. The note stands anywhere in a row's `price_note` (behind a
/// tier note, say), hence `instr`. Not grouped: each note carries its own
/// token amount, so no two rows share one, and the calls it names are read
/// out of it per row (`ops::unpriced_hosted_tools`).
pub const HOSTED_FEE_TRAFFIC_SQL: &str =
    "SELECT requested_alias, upstream_id, upstream_model, price_note, ts
           FROM request_logs INDEXED BY idx_request_logs_price_note
          WHERE price_note IS NOT NULL AND price_source = 'unknown'
            AND instr(price_note, ?1) > 0
          ORDER BY ts";

/// Every row the hosted-tool fees left unpriced, oldest first. Raw rows, so
/// the log's retention bounds how far back it reaches; the caller groups
/// them per model and sums their calls (`ops::unpriced_hosted_tools`).
pub async fn hosted_fee_traffic(pool: &SqlitePool) -> DbResult<Vec<HostedFeeRow>> {
    let rows = sqlx::query(HOSTED_FEE_TRAFFIC_SQL)
        .bind(crate::pricing::HOSTED_FEES_NOTE)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| HostedFeeRow {
            alias: r.get("requested_alias"),
            upstream_id: r.get("upstream_id"),
            upstream_model: r.get("upstream_model"),
            price_note: r.get("price_note"),
            ts: r.get("ts"),
        })
        .collect())
}
