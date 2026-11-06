//! The traffic a service tier left unpriced (openai egress design §5.6): the
//! raw rows the Prices page's list of tiers without a price is read from.

use sqlx::{Row, SqlitePool};

use super::*;

/// One (alias, upstream model, reported tier) whose answers came back
/// unpriced with a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierTraffic {
    /// The alias the requests asked for.
    pub alias: String,
    /// The alias they were priced under when a fallback answered.
    pub priced_alias: Option<String>,
    pub upstream_id: Option<i64>,
    pub upstream_model: Option<String>,
    /// The `service_tier` the answers reported, verbatim; `None` for the
    /// answers that named none.
    pub service_tier: Option<String>,
    /// How many such rows the log holds.
    pub requests: i64,
    /// When the first of them was written.
    pub first_ts: String,
}

/// The query behind [`unpriced_tier_traffic`]; `?1` is the prefix of a note
/// for an answer that named no tier. Its `price_note IS NOT NULL` term is what
/// lets it read the partial index of noted rows (migration 0092) instead of
/// the whole log.
pub const UNPRICED_TIER_TRAFFIC_SQL: &str =
    "SELECT requested_alias, priced_alias, upstream_id, upstream_model, service_tier,
                COUNT(*) AS requests, MIN(ts) AS first_ts
           FROM request_logs
          WHERE price_note IS NOT NULL AND egress_proto = 'openai'
            AND price_source = 'unknown'
            AND (service_tier IS NOT NULL OR price_note LIKE ?1 || '%')
          GROUP BY requested_alias, priced_alias, upstream_id, upstream_model, service_tier
          ORDER BY requests DESC, first_ts";

/// Every `openai` row's group that came back unpriced with a tier note,
/// busiest first: answered at a reported tier (whatever it names, `auto`
/// included) or at none ([`crate::pricing::NO_TIER_NOTE`]). Raw rows, so the log's
/// retention bounds how far back it reaches. Whether a group still lacks a
/// price is the caller's question, asked of today's price rows
/// (`ops::unpriced_tiers`).
pub async fn unpriced_tier_traffic(pool: &SqlitePool) -> DbResult<Vec<TierTraffic>> {
    let rows = sqlx::query(UNPRICED_TIER_TRAFFIC_SQL)
        .bind(crate::pricing::NO_TIER_NOTE)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| TierTraffic {
            alias: r.get("requested_alias"),
            priced_alias: r.get("priced_alias"),
            upstream_id: r.get("upstream_id"),
            upstream_model: r.get("upstream_model"),
            service_tier: r.get("service_tier"),
            requests: r.get("requests"),
            first_ts: r.get("first_ts"),
        })
        .collect())
}
