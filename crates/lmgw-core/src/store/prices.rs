//! Prices (usage-analytics §2.2; billable-units §2.2)

use sqlx::SqlitePool;

use crate::config::{PriceRow, PriceScope, PriceUnit};
use crate::pricing::Prices;

use super::*;

pub async fn list_prices(pool: &SqlitePool) -> DbResult<Vec<PriceRow>> {
    let rows = sqlx::query("SELECT * FROM prices ORDER BY scope_kind, scope_key, source, unit")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(price_from_row).collect())
}

/// Insert or update one price row.
///
/// Keyed on (scope, source, unit), so a catalog refresh updates only its own
/// rows and can never clobber a number the owner typed, and one scope holds a
/// row per unit side by side.
///
/// `p` carries the source, and the four token rates of a `per_mtok` row;
/// `price` is the rate of any other unit, per its scale. A row in another unit
/// leaves the token rates `None` and a `per_mtok` row leaves `price` `None`:
/// the table's CHECK refuses a row that mixes the two.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_price(
    pool: &SqlitePool,
    scope_kind: PriceScope,
    scope_key: &str,
    unit: PriceUnit,
    p: &Prices,
    price: Option<f64>,
    note: Option<&str>,
) -> DbResult<()> {
    sqlx::query(
        "INSERT INTO prices (scope_kind, scope_key, unit, price_in, price_out,
             price_cache_read, price_cache_write, price, source, note, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10, datetime('now'))
         ON CONFLICT(scope_kind, scope_key, source, unit) DO UPDATE SET
             price_in = excluded.price_in,
             price_out = excluded.price_out,
             price_cache_read = excluded.price_cache_read,
             price_cache_write = excluded.price_cache_write,
             price = excluded.price,
             note = excluded.note,
             updated_at = datetime('now')",
    )
    .bind(scope_kind.as_str())
    .bind(scope_key)
    .bind(unit.as_str())
    .bind(p.price_in)
    .bind(p.price_out)
    .bind(p.price_cache_read)
    .bind(p.price_cache_write)
    .bind(price)
    .bind(p.source.as_str())
    .bind(note)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_price(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("DELETE FROM prices WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
