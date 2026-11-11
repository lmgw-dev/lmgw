//! The hourly record of VRAM outside lmgw (migration 0106,
//! `vram::headroom::growth`).

use sqlx::{Row, SqlitePool};

use super::DbResult;

/// One hour's readings: the lowest, the highest, and the largest rise within
/// the hour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OutsideHour {
    /// Unix time / 3600.
    pub hour: i64,
    pub low_bytes: u64,
    pub high_bytes: u64,
    pub rise_bytes: u64,
}

fn int(b: u64) -> i64 {
    i64::try_from(b).unwrap_or(i64::MAX)
}

/// Every hour from `since` on, oldest first.
pub async fn list_vram_outside_hours(pool: &SqlitePool, since: i64) -> DbResult<Vec<OutsideHour>> {
    let rows = sqlx::query(
        "SELECT hour, low_bytes, high_bytes, rise_bytes FROM vram_outside_hours
          WHERE hour >= ?1 ORDER BY hour",
    )
    .bind(since)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| OutsideHour {
            hour: r.get("hour"),
            low_bytes: r.get::<i64, _>("low_bytes").max(0).unsigned_abs(),
            high_bytes: r.get::<i64, _>("high_bytes").max(0).unsigned_abs(),
            rise_bytes: r.get::<i64, _>("rise_bytes").max(0).unsigned_abs(),
        })
        .collect())
}

/// Write `h` over its hour, and drop the hours before `keep_since`.
pub async fn put_vram_outside_hour(
    pool: &SqlitePool,
    h: OutsideHour,
    keep_since: i64,
) -> DbResult<()> {
    sqlx::query(
        "INSERT INTO vram_outside_hours (hour, low_bytes, high_bytes, rise_bytes)
              VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(hour) DO UPDATE SET low_bytes = excluded.low_bytes,
                                         high_bytes = excluded.high_bytes,
                                         rise_bytes = excluded.rise_bytes",
    )
    .bind(h.hour)
    .bind(int(h.low_bytes))
    .bind(int(h.high_bytes))
    .bind(int(h.rise_bytes))
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM vram_outside_hours WHERE hour < ?1")
        .bind(keep_since)
        .execute(pool)
        .await?;
    Ok(())
}

/// Forget every hour (the owner's "forget learned growth").
pub async fn delete_vram_outside_hours(pool: &SqlitePool) -> DbResult<()> {
    sqlx::query("DELETE FROM vram_outside_hours")
        .execute(pool)
        .await?;
    Ok(())
}
