//! Migration 0061: `api_keys.kind` gains `device`, with `last_seen_at` and
//! `hosts_label` (client-apps design §1.1) — a third rebuild of the table,
//! so every column of every kind must survive it, the id high-water mark too.

use sqlx::Row;

use super::db_at_version;

#[tokio::test]
async fn migration_0061_keeps_every_key_and_admits_a_device() {
    let pool = db_at_version(60).await;

    sqlx::query("INSERT INTO agents (id, manifest) VALUES ('labeler', '{}')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO api_keys (id, name, key_hash, key_plain, kind, agent_id)
         VALUES (42, 'agent:labeler', 'hash-42', 'lmgw-agent-deadbeef', 'agent', 'labeler')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO api_keys (id, name, key_hash, scope_mode, scope_patterns, budget_micro,
             tool_scope_mode, tool_scope_patterns, expires_at, note)
         VALUES (43, 'laptop', 'hash-43', 'allow', 'chatty', 5000000, 'deny', 'github__*',
                 '2027-01-01', 'my laptop')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO api_keys (id, name, key_hash, key_plain, kind)
         VALUES (44, 'owner:dashboard', 'hash-44', 'lmgw-owner-cafe', 'owner')",
    )
    .execute(&pool)
    .await
    .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    // Every column of every kind, the 0039 tool scope included.
    let r = sqlx::query("SELECT * FROM api_keys WHERE id = 43")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(r.get::<String, _>("scope_mode"), "allow");
    assert_eq!(r.get::<String, _>("scope_patterns"), "chatty");
    assert_eq!(r.get::<i64, _>("budget_micro"), 5_000_000);
    assert_eq!(r.get::<String, _>("tool_scope_mode"), "deny");
    assert_eq!(r.get::<String, _>("tool_scope_patterns"), "github__*");
    assert_eq!(
        r.get::<Option<String>, _>("expires_at").as_deref(),
        Some("2027-01-01")
    );
    assert_eq!(r.get::<String, _>("note"), "my laptop");
    assert_eq!(r.get::<Option<String>, _>("last_seen_at"), None);
    assert_eq!(r.get::<Option<String>, _>("hosts_label"), None);
    let (plain, agent): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT key_plain, agent_id FROM api_keys WHERE id = 42")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(plain.as_deref(), Some("lmgw-agent-deadbeef"));
    assert_eq!(agent.as_deref(), Some("labeler"));
    let plain: Option<String> = sqlx::query_scalar("SELECT key_plain FROM api_keys WHERE id = 44")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(plain.as_deref(), Some("lmgw-owner-cafe"));

    // The widened CHECK: a device, hash-only, with a hosting label.
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, kind, hosts_label)
         VALUES ('device:desk', 'h', 'device', 'desk')",
    )
    .execute(&pool)
    .await
    .expect("kind='device' is legal now");
    for (why, sql) in [
        (
            "a device keeping a plaintext",
            "INSERT INTO api_keys (name, key_hash, key_plain, kind)
             VALUES ('device:leaky', 'h', 'p', 'device')",
        ),
        (
            "a hosting label on a client key",
            "INSERT INTO api_keys (name, key_hash, hosts_label) VALUES ('c', 'h', 'tools')",
        ),
        (
            "two devices on one label",
            "INSERT INTO api_keys (name, key_hash, kind, hosts_label)
             VALUES ('device:twin', 'h', 'device', 'desk')",
        ),
        (
            "an agent row without a plaintext",
            "INSERT INTO api_keys (name, key_hash, kind, agent_id)
             VALUES ('agent:bad', 'h', 'agent', 'bad')",
        ),
        (
            "a second token for one agent",
            "INSERT INTO api_keys (name, key_hash, key_plain, kind, agent_id)
             VALUES ('agent:labeler-2', 'h', 'p', 'agent', 'labeler')",
        ),
    ] {
        assert!(
            sqlx::query(sql).execute(&pool).await.is_err(),
            "{why} must be refused"
        );
    }

    // The watermark: a deleted top id is never handed out again.
    let top: i64 = sqlx::query_scalar("SELECT MAX(id) FROM api_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM api_keys WHERE id = ?1")
        .bind(top)
        .execute(&pool)
        .await
        .unwrap();
    let next: i64 = sqlx::query_scalar(
        "INSERT INTO api_keys (name, key_hash) VALUES ('after', 'h') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(next > top, "id {next} re-used after {top} was deleted");
}

/// The id high-water mark across 0061's rebuild (review W2-12): a top id
/// deleted *before* the migration is never handed out after it.
#[tokio::test]
async fn migration_0061_carries_a_deleted_top_id_s_watermark() {
    let pool = db_at_version(60).await;
    sqlx::query("INSERT INTO api_keys (id, name, key_hash) VALUES (99, 'gone', 'h')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM api_keys WHERE id = 99")
        .execute(&pool)
        .await
        .unwrap();
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    let next: i64 = sqlx::query_scalar(
        "INSERT INTO api_keys (name, key_hash) VALUES ('after', 'h') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(next > 99, "id {next} re-used after 99 was deleted");
}

/// Migration 0062: a hosting label is unique whatever its case (review
/// W2-25), as every check that writes one reads it.
#[tokio::test]
async fn migration_0062_makes_the_hosting_label_unique_without_case() {
    let pool = db_at_version(61).await;
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, kind, hosts_label) VALUES ('device:a', 'h1', 'device', 'Desk')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let clash = sqlx::query(
        "INSERT INTO api_keys (name, key_hash, kind, hosts_label) VALUES ('device:b', 'h2', 'device', 'desk')",
    )
    .execute(&pool)
    .await;
    assert!(
        clash.is_err(),
        "a case variant of a label is the same label"
    );
}
