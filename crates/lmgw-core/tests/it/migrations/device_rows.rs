//! Migration 0072: every hosting grant gets its device row — except one
//! whose name an older server row has (`device:<name>`, from before the
//! prefix was the grant's alone). That row is left as it is, the grant gets
//! no row, and a write that sets or changes the grant then says which row is
//! in the way instead of the UNIQUE constraint's bare error (review finding
//! 6); any other write of the key still goes through.

use lmgw_core::config::{DeviceAdmin, KeyPolicy};
use lmgw_core::store;

use super::db_at_version;

#[tokio::test]
async fn migration_0072_skips_a_grant_whose_name_is_taken_and_says_so_later() {
    let pool = db_at_version(71).await;
    sqlx::query(
        "INSERT INTO api_keys (id, name, key_hash, kind, hosts_label)
         VALUES (50, 'device:desk', 'h50', 'device', 'desk'),
                (51, 'device:pad', 'h51', 'device', 'pad')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mcp_servers (id, name, transport, url, tool_prefix)
         VALUES (7, 'device:desk', 'http', 'http://127.0.0.1:1/mcp', 'old')",
    )
    .execute(&pool)
    .await
    .unwrap();

    store::run_migrations(&pool).await.unwrap();

    let rows: Vec<(i64, String, Option<i64>)> =
        sqlx::query_as("SELECT id, name, device_key_id FROM mcp_servers ORDER BY name")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(
        (rows[0].0, rows[0].2),
        (7, None),
        "the old row is left alone"
    );
    assert_eq!((rows[1].1.as_str(), rows[1].2), ("device:pad", Some(51)));

    // Setting the grant again names the row in the way, and writes nothing.
    let policy = KeyPolicy::default();
    let e = store::update_key_policy(
        &pool,
        50,
        &policy,
        true,
        "",
        Some("desk2"),
        (DeviceAdmin::Off, None),
    )
    .await
    .expect_err("the name is taken");
    let said = e.to_string();
    assert!(
        said.contains("'device:desk', is taken by MCP server 7"),
        "{said}"
    );
    let label: Option<String> =
        sqlx::query_scalar("SELECT hosts_label FROM api_keys WHERE id = 50")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(label.as_deref(), Some("desk"), "rolled back");

    // Disabling the key, its grant as it is, goes through.
    let n = store::update_key_policy(
        &pool,
        50,
        &policy,
        false,
        "",
        Some("desk"),
        (DeviceAdmin::Off, None),
    )
    .await
    .unwrap();
    assert_eq!(n, 1);
}
