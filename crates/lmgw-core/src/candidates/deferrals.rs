//! Deferral counts for candidate aliases (candidate-aliases design §6:
//! "Each alias shows its deferrals over the last 24 h, counted from the
//! request log (`gpu_deferred`)").
//!
//! A background walk that finds nothing local it may use answers
//! [`crate::error::GatewayError::GpuDeferred`] — `503`, wire code
//! `gpu_hold`, but logged under its own kind, `gpu_deferred`
//! (`request_logs.error_kind`), precisely so this count can tell a real hold
//! from the alias just being busy (spec §12 entry 67). This module reads
//! that count back, for `lmgw__status`, `lmgw__models kind=alias` and
//! `GET /api/models/full` alike.
//!
//! Its own file rather than a new `store.rs` function, or a new one in
//! `ops/candidate_alias.rs`: `store.rs` and `ops.rs` are both past the
//! project's file-size flag already, `ops/candidate_alias.rs` is the
//! save-time patch path (not a read), and this query is read by three
//! unrelated builders — a small module next to [`super::derive`],
//! [`super::facets`] and [`super::validate`] is the natural home.

use std::collections::HashMap;

use sqlx::{Row, SqlitePool};

use crate::store::DbResult;

/// How many `gpu_deferred` refusals *every* candidate alias logged in the
/// last 24 hours, keyed by `lower(requested_alias)`. **One grouped query for
/// every alias at once** — a status page or a `kind=alias` listing asks this
/// once, not once per alias, however many candidate aliases are configured.
///
/// Grouped by the *lowercased* alias, not the literal column: `requested_
/// alias` is logged as the client spelled it, while `CandidateAlias::alias`
/// is matched case-insensitively everywhere else (`Snapshot::candidate_
/// alias`, §12 entry 55) — two requests spelled in different cases (`jobs`,
/// `Jobs`) are the same alias and must land in one bucket, not two rows
/// SQLite's own `GROUP BY` would otherwise keep apart. [`for_alias`]
/// lowercases its own lookup to match.
pub async fn deferrals_24h(pool: &SqlitePool) -> DbResult<HashMap<String, u64>> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::hours(24))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let rows = sqlx::query(
        "SELECT lower(requested_alias) AS alias, COUNT(*) AS n
           FROM request_logs
          WHERE error_kind = 'gpu_deferred' AND ts >= ?
          GROUP BY lower(requested_alias)",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<String, _>("alias"), r.get::<i64, _>("n") as u64))
        .collect())
}

/// One alias's count out of [`deferrals_24h`]'s map, matched
/// case-insensitively (this module's own doc comment says why) — `0` when
/// the alias logged no deferral in the window, exactly like the count for
/// any other alias not in the map. Always `Some` from a live builder: the
/// wire type is `Option<u64>` only so a cached frame from before this field
/// existed still decodes (`#[serde(default)]`), never to mean "unknown"
/// here.
pub fn for_alias(counts: &HashMap<String, u64>, alias: &str) -> Option<u64> {
    Some(counts.get(&alias.to_lowercase()).copied().unwrap_or(0))
}
