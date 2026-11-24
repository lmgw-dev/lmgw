//! What the engine costs on each card (migrations 0110 and 0113,
//! `vram::place::engine`; device-ledger design §4.2, ruling 17): the last
//! readings of each card.

use lmgw_api_types::DeviceId;
use sqlx::{Row, SqlitePool};

use super::DbResult;

/// Every card's kept readings, each card's oldest first: (card, bytes, Unix
/// seconds).
pub async fn list_vram_engine_readings(pool: &SqlitePool) -> DbResult<Vec<(DeviceId, u64, i64)>> {
    let rows = sqlx::query(
        "SELECT device, bytes, learned_at FROM vram_engine_readings ORDER BY device, id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                DeviceId(r.get("device")),
                r.get::<i64, _>("bytes").max(0).unsigned_abs(),
                r.get("learned_at"),
            )
        })
        .collect())
}

/// Every card's figure as it is charged: the largest of its kept readings,
/// and when the newest was taken (card, bytes, Unix seconds).
pub async fn list_vram_engine_costs(pool: &SqlitePool) -> DbResult<Vec<(DeviceId, u64, i64)>> {
    let rows = sqlx::query(
        "SELECT device, MAX(bytes) AS bytes, MAX(learned_at) AS learned_at
           FROM vram_engine_readings GROUP BY device ORDER BY device",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                DeviceId(r.get("device")),
                r.get::<i64, _>("bytes").max(0).unsigned_abs(),
                r.get("learned_at"),
            )
        })
        .collect())
}

/// Add a reading of card `device`, keeping its newest `keep` readings.
pub async fn put_vram_engine_reading(
    pool: &SqlitePool,
    device: &DeviceId,
    bytes: u64,
    learned_at: i64,
    keep: usize,
) -> DbResult<()> {
    let mut tx = super::begin_write(pool).await?;
    sqlx::query("INSERT INTO vram_engine_readings (device, bytes, learned_at) VALUES (?1, ?2, ?3)")
        .bind(device.as_str())
        .bind(i64::try_from(bytes).unwrap_or(i64::MAX))
        .bind(learned_at)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM vram_engine_readings WHERE device = ?1 AND id NOT IN
           (SELECT id FROM vram_engine_readings WHERE device = ?1 ORDER BY id DESC LIMIT ?2)",
    )
    .bind(device.as_str())
    .bind(i64::try_from(keep).unwrap_or(i64::MAX))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
