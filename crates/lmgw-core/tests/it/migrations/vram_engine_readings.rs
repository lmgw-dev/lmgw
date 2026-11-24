//! Migration 0113: the engine's cost per card is kept as its last readings
//! (device-ledger design §4.2, owner, 2026-11-18). 0110's figure of each
//! card is carried over as that card's first reading, so a card keeps what
//! it had learned.

use lmgw_core::store::{
    list_vram_engine_costs, list_vram_engine_readings, put_vram_engine_reading,
};
use lmgw_core::vram::nvml::DeviceId;

use super::db_at_version;

const MIB: u64 = 1024 * 1024;

#[tokio::test]
async fn migration_0113_carries_each_cards_figure_over_as_its_first_reading() {
    let pool = db_at_version(112).await;
    sqlx::query(
        "INSERT INTO vram_engine_cost (device, bytes, learned_at)
         VALUES ('GPU-a', ?1, 1000), ('GPU-b', ?2, 2000)",
    )
    .bind(i64::try_from(621 * MIB).unwrap())
    .bind(i64::try_from(480 * MIB).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let a = DeviceId::new("GPU-a");
    let b = DeviceId::new("GPU-b");
    assert_eq!(
        list_vram_engine_readings(&pool).await.unwrap(),
        vec![(a.clone(), 621 * MIB, 1000), (b.clone(), 480 * MIB, 2000)]
    );
    // A lower reading does not lower the figure; the time is the newest's.
    put_vram_engine_reading(&pool, &a, 500 * MIB, 3000, 5)
        .await
        .unwrap();
    assert_eq!(
        list_vram_engine_costs(&pool).await.unwrap(),
        vec![(a.clone(), 621 * MIB, 3000), (b.clone(), 480 * MIB, 2000)]
    );
    // Five more: the carried-over one is the oldest and goes.
    for i in 0..5 {
        put_vram_engine_reading(&pool, &a, (550 + i) * MIB, 4000 + i as i64, 5)
            .await
            .unwrap();
    }
    let kept: Vec<u64> = list_vram_engine_readings(&pool)
        .await
        .unwrap()
        .into_iter()
        .filter(|(d, _, _)| d == &a)
        .map(|(_, bytes, _)| bytes / MIB)
        .collect();
    assert_eq!(kept, vec![550, 551, 552, 553, 554]);
    let table: Option<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'vram_engine_cost'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(table, None, "0110's table is gone");
}
