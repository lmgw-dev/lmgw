//! Pre-migration guard (migration 0070): refuse, by name, the price rows the
//! billable-units migration cannot carry into the rebuilt `prices` table
//! (billable-units design §5.4).
//!
//! 0070 drops the `per_second` and `per_char` placeholders: they named a scale
//! that `per_audio_minute` and `per_mchar` replace, for a quantity nothing
//! defined, so a rate stored under them has no unit to map to. And a
//! `per_image` or `per_request` row now keeps its rate in one `price` column,
//! so one that holds token rates instead has no answer either: whether a fee
//! entered as `price_in` or as `price_out` is the request's has none.
//!
//! lmgw has only ever written `per_mtok` rows, so only hand-written SQL could
//! have produced one of these. Like 0023's guard this exists for honesty about
//! what the migration can lose, not because anything is expected to trip it:
//! the copy would fail on the new table's CHECK, and without this the owner
//! would read a constraint error instead of the rows.

use sqlx::{Row, SqlitePool};

use super::{migration_applied, table_exists};

/// Version of `0070_billable_units.sql`. Also what `store::units_since`
/// reads the install time of.
pub(in crate::store) const BILLABLE_UNITS_MIGRATION: i64 = 70;

/// Refuse the upgrade, naming every price row 0070 could not carry over.
/// Nothing on a new database, one that ran 0070 already, or one from before
/// `prices` existed.
pub(in crate::store) async fn refuse_unmappable_price_rows(
    pool: &SqlitePool,
) -> anyhow::Result<()> {
    if !table_exists(pool, "_sqlx_migrations").await?
        || migration_applied(pool, BILLABLE_UNITS_MIGRATION).await?
        || !table_exists(pool, "prices").await?
    {
        return Ok(());
    }
    let rows = sqlx::query(
        "SELECT id, scope_kind, scope_key, source, unit FROM prices
         WHERE unit IN ('per_second', 'per_char')
            OR (unit <> 'per_mtok'
                AND COALESCE(price_in, price_out, price_cache_read, price_cache_write)
                    IS NOT NULL)
         ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let offenders: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "id {} ({} '{}', {}, {})",
                r.get::<i64, _>("id"),
                r.get::<String, _>("scope_kind"),
                r.get::<String, _>("scope_key"),
                r.get::<String, _>("source"),
                r.get::<String, _>("unit"),
            )
        })
        .collect();
    let ids: Vec<String> = rows
        .iter()
        .map(|r| r.get::<i64, _>("id").to_string())
        .collect();
    anyhow::bail!(
        "this upgrade gives prices units with a scale of their own (billable units), and {} \
         price row(s) cannot be carried into it: {}. 'per_second' and 'per_char' are gone — \
         'per_audio_minute' and 'per_mchar' replace them at a different scale — and a \
         'per_image' or 'per_request' row keeps its rate in one 'price' column now, where \
         these hold token rates. lmgw never writes such rows itself, so nothing has been \
         changed and nothing guessed. Delete them (sqlite3 on the database file, with lmgw \
         stopped: DELETE FROM prices WHERE id IN ({})), start lmgw again, and enter the rates \
         anew in per_audio_minute, per_mchar, per_image or per_request.",
        rows.len(),
        offenders.join(", "),
        ids.join(", "),
    )
}
