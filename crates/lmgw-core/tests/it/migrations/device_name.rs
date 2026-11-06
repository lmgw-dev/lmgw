//! Migration 0089: a waiting key's self-description waits with it. v0.5.0
//! wrote the newcomer's time, address and client onto the main columns; a row
//! with a key waiting at the upgrade moves them to the `waiting_` twins and
//! clears the main ones, which never described the kept key. Rows without a
//! waiting key keep theirs.

use sqlx::SqlitePool;

use super::db_at_version;

type Described = (Option<String>, Option<String>, Option<String>);

async fn main_columns(pool: &SqlitePool, id: i64) -> Described {
    sqlx::query_as("SELECT enrolled_at, enrolled_from, client_info FROM api_keys WHERE id = ?1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn twins(pool: &SqlitePool, id: i64) -> Described {
    sqlx::query_as(
        "SELECT waiting_enrolled_at, waiting_enrolled_from, waiting_client_info
           FROM api_keys WHERE id = ?1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn some(at: &str, from: &str, client: &str) -> Described {
    (Some(at.into()), Some(from.into()), Some(client.into()))
}

#[tokio::test]
async fn migration_0089_moves_a_waiting_keys_description_to_the_twins() {
    let pool = db_at_version(88).await;
    sqlx::query(
        "INSERT INTO api_keys (id, name, key_hash, kind, confirmed_at, waiting_key_hash,
                               enrolled_at, enrolled_from, client_info)
         VALUES (70, 'device:waits', 'spki:aa', 'device', '2026-10-01T10:00:00Z', 'spki:bb',
                 '2026-10-20T10:00:00Z', '192.0.2.9', 'Newcomer 1.0'),
                (71, 'device:plain', 'spki:cc', 'device', '2026-10-01T10:00:00Z', NULL,
                 '2026-10-02T10:00:00Z', '192.0.2.2', 'Kai 0.1.0'),
                (72, 'other', 'h72', 'key', NULL, NULL,
                 '2026-10-03T10:00:00Z', '192.0.2.3', 'odd')",
    )
    .execute(&pool)
    .await
    .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    assert_eq!(
        twins(&pool, 70).await,
        some("2026-10-20T10:00:00Z", "192.0.2.9", "Newcomer 1.0")
    );
    assert_eq!(main_columns(&pool, 70).await, (None, None, None));
    assert_eq!(
        main_columns(&pool, 71).await,
        some("2026-10-02T10:00:00Z", "192.0.2.2", "Kai 0.1.0")
    );
    assert_eq!(twins(&pool, 71).await, (None, None, None));
    assert_eq!(
        main_columns(&pool, 72).await,
        some("2026-10-03T10:00:00Z", "192.0.2.3", "odd"),
        "only device rows are touched"
    );
}
