//! Migration 0076: the owner's approval floor (client-apps design §6.6) on
//! every thread and folder, started from the rules stored today — written
//! before devices could only tighten them, so taken as the owner's. Text
//! that is not a JSON list starts with none and does not stop the upgrade.

use super::db_at_version;

#[tokio::test]
async fn migration_0076_starts_every_floor_from_the_stored_rules() {
    let pool = db_at_version(74).await;
    let tools = r#"[{"server_label":"docs","allowed_tools":null,"require_approval":"always"}]"#;
    for (id, mcp) in [(1, tools), (2, "[]"), (3, "not json"), (4, r#"{"a":1}"#)] {
        sqlx::query("INSERT INTO chat_threads (id, model_alias, mcp_tools) VALUES (?1, 'm', ?2)")
            .bind(id)
            .bind(mcp)
            .execute(&pool)
            .await
            .unwrap();
    }
    let with = format!(r#"{{"model_alias":"m","mcp_tools":{tools}}}"#);
    for (id, defaults) in [
        (1, with.as_str()),
        (2, r#"{"model_alias":"m"}"#),
        (3, "not json"),
        (4, r#"{"mcp_tools":"docs"}"#),
    ] {
        sqlx::query("INSERT INTO chat_folders (id, name, defaults) VALUES (?1, ?2, ?3)")
            .bind(id)
            .bind(format!("f{id}"))
            .bind(defaults)
            .execute(&pool)
            .await
            .unwrap();
    }

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    for (table, sql) in [
        (
            "chat_threads",
            "SELECT id, approval_floor FROM chat_threads ORDER BY id",
        ),
        (
            "chat_folders",
            "SELECT id, approval_floor FROM chat_folders ORDER BY id",
        ),
    ] {
        let floors: Vec<(i64, String)> = sqlx::query_as(sql).fetch_all(&pool).await.unwrap();
        let floors: Vec<(i64, serde_json::Value)> = floors
            .into_iter()
            .map(|(id, f)| (id, serde_json::from_str(&f).unwrap()))
            .collect();
        let tools: serde_json::Value = serde_json::from_str(tools).unwrap();
        let none = serde_json::json!([]);
        assert_eq!(
            floors,
            vec![(1, tools), (2, none.clone()), (3, none.clone()), (4, none)],
            "{table}"
        );
    }
    // A row made after it starts with none.
    sqlx::query("INSERT INTO chat_threads (id, model_alias) VALUES (9, 'm')")
        .execute(&pool)
        .await
        .unwrap();
    let floor: String = sqlx::query_scalar("SELECT approval_floor FROM chat_threads WHERE id = 9")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(floor, "[]");
}
