//! The hourly record of VRAM outside lmgw, per card (migrations 0106 and
//! 0109, `vram::headroom::growth`; device-ledger design §9.1).

use lmgw_api_types::DeviceId;
use sqlx::{Row, SqlitePool};

use super::DbResult;

/// One hour's readings of one card: the lowest, the highest, and the
/// largest rise within the hour.
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

/// Every card's hours from `since` on, oldest first.
pub async fn list_vram_outside_hours(
    pool: &SqlitePool,
    since: i64,
) -> DbResult<Vec<(DeviceId, OutsideHour)>> {
    let rows = sqlx::query(
        "SELECT hour, device, low_bytes, high_bytes, rise_bytes FROM vram_outside_hours
          WHERE hour >= ?1 ORDER BY hour, device",
    )
    .bind(since)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                DeviceId(r.get("device")),
                OutsideHour {
                    hour: r.get("hour"),
                    low_bytes: r.get::<i64, _>("low_bytes").max(0).unsigned_abs(),
                    high_bytes: r.get::<i64, _>("high_bytes").max(0).unsigned_abs(),
                    rise_bytes: r.get::<i64, _>("rise_bytes").max(0).unsigned_abs(),
                },
            )
        })
        .collect())
}

/// Write `h` over its hour of card `device`, and drop every card's hours
/// before `keep_since`.
pub async fn put_vram_outside_hour(
    pool: &SqlitePool,
    device: &DeviceId,
    h: OutsideHour,
    keep_since: i64,
) -> DbResult<()> {
    sqlx::query(
        "INSERT INTO vram_outside_hours (hour, device, low_bytes, high_bytes, rise_bytes)
              VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(hour, device) DO UPDATE SET low_bytes = excluded.low_bytes,
                                                 high_bytes = excluded.high_bytes,
                                                 rise_bytes = excluded.rise_bytes",
    )
    .bind(h.hour)
    .bind(device.as_str())
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

/// Forget the hours of card `device`, or of every card with `None` (the
/// Forget learned growth action, device-ledger design ruling 10).
pub async fn delete_vram_outside_hours(
    pool: &SqlitePool,
    device: Option<&DeviceId>,
) -> DbResult<()> {
    match device {
        Some(d) => {
            sqlx::query("DELETE FROM vram_outside_hours WHERE device = ?1")
                .bind(d.as_str())
                .execute(pool)
                .await?;
        }
        None => {
            sqlx::query("DELETE FROM vram_outside_hours")
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}
