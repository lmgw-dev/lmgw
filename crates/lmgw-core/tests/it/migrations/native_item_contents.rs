//! Migration 0100: every encrypted content a recorded output item was
//! answered with (api-compat design §9, finding 9). The one hash 0096
//! kept per item moves into `native_response_item_contents`, and a returned
//! item passes with it as before.

use lmgw_core::store::{self, ItemClaim};

use super::db_at_version;

#[tokio::test]
async fn migration_0100_keeps_the_recorded_hash_as_one_of_the_items_contents() {
    let pool = db_at_version(99).await;
    let upstream: i64 = sqlx::query_scalar(
        "INSERT INTO upstreams (name, protocol, kind, base_url)
         VALUES ('platform', 'openai', 'generic', 'x') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO native_responses (id, upstream_id, key_id, model, upstream_model)
         VALUES ('resp_1', ?1, 7, 'm', 'm')",
    )
    .bind(upstream)
    .execute(&pool)
    .await
    .unwrap();
    let hash = lmgw_core::ir::encrypted_sha256("gAAA-done");
    sqlx::query(
        "INSERT INTO native_response_items (item_id, response_id, key_id, encrypted_sha256)
         VALUES ('rs_1', 'resp_1', 7, ?1), ('msg_1', 'resp_1', 7, NULL)",
    )
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    store::run_migrations(&pool).await.unwrap();

    let contents: Vec<(String, String)> =
        sqlx::query_as("SELECT item_id, encrypted_sha256 FROM native_response_item_contents")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(contents, [("rs_1".to_string(), hash.clone())]);
    let claim = |data: &str| ItemClaim {
        id: "rs_1".into(),
        upstream_id: None,
        encrypted_sha256: Some(lmgw_core::ir::encrypted_sha256(data)),
    };
    assert_eq!(
        store::first_item_not_of(&pool, &[claim("gAAA-done")], Some(7))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store::first_item_not_of(&pool, &[claim("gAAA-other")], Some(7))
            .await
            .unwrap()
            .as_deref(),
        Some("rs_1")
    );
}
