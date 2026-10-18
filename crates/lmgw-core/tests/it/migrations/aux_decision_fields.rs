//! Migration 0079: decision models (decision-models design §3.1, §3.3) — the
//! `aux_models` rebuild that admits kind `decision` and adds the five row
//! fields, each NULL on every existing row, with its CHECKs.

use lmgw_core::config::{AuxKind, HoldFallbackMode};
use sqlx::{AssertSqlSafe, SqlitePool};

use super::db_at_version;

async fn exec(pool: &SqlitePool, sql: &str) -> Result<(), sqlx::Error> {
    sqlx::query(AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .map(|_| ())
}

#[tokio::test]
async fn migration_0079_keeps_every_aux_row_and_its_ids_and_adds_the_fields_as_null() {
    let pool = db_at_version(78).await;
    exec(
        &pool,
        "INSERT INTO aux_models (id, model_id, gguf_path, kind, pooling, ctx_size, args, \
         idle_seconds, enabled, image, extra_run_args, warm_start, hold_fallback_mode, \
         hold_fallback, created_at, updated_at) VALUES \
         (1, 'e5', 'e5.gguf', 'embed', 'mean', 512, '[\"--n-gpu-layers\",\"0\"]', 30, 0, \
          'localhost/llama:cpu', '[\"--cpus\",\"2\"]', 1, 'alias', 'cloud-embed', \
          '2026-01-02 03:04:05', '2026-02-03 04:05:06'), \
         (2, 'bge-rr', 'rr.gguf', 'rerank', NULL, NULL, '[]', 0, 1, NULL, NULL, 0, 'none', \
          NULL, '2026-01-01 00:00:00', '2026-01-01 00:00:00'), \
         (3, 'gone', 'g.gguf', 'embed', NULL, NULL, '[]', 0, 1, NULL, NULL, 0, 'inherit', NULL, \
          '2026-01-01 00:00:00', '2026-01-01 00:00:00')",
    )
    .await
    .unwrap();
    // The top id is deleted before the upgrade: the rebuild must not hand it
    // out again.
    exec(&pool, "DELETE FROM aux_models WHERE id = 3")
        .await
        .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let rows = lmgw_core::store::list_aux_models(&pool).await.unwrap();
    assert_eq!(rows.len(), 2);
    let e5 = rows.iter().find(|m| m.model_id == "e5").unwrap();
    assert_eq!((e5.id, e5.kind), (1, AuxKind::Embed));
    assert_eq!(e5.pooling.as_deref(), Some("mean"));
    assert_eq!(e5.ctx_size, Some(512));
    assert_eq!(e5.args, ["--n-gpu-layers", "0"], "args are left for WP2b");
    assert_eq!(
        e5.extra_run_args.as_deref(),
        Some(&["--cpus".to_string(), "2".into()][..])
    );
    assert_eq!((e5.idle_seconds, e5.warm_start), (30, true));
    assert!(!e5.enabled);
    assert_eq!(e5.image.as_deref(), Some("localhost/llama:cpu"));
    assert_eq!(e5.hold_fallback_mode, HoldFallbackMode::Alias);
    assert_eq!(e5.hold_fallback.as_deref(), Some("cloud-embed"));
    let rr = rows.iter().find(|m| m.model_id == "bge-rr").unwrap();
    assert!(rr.enabled);
    assert_eq!(rr.hold_fallback_mode, HoldFallbackMode::None);
    let stamps: Vec<(String, String)> =
        sqlx::query_as("SELECT created_at, updated_at FROM aux_models ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        stamps,
        [
            ("2026-01-02 03:04:05".into(), "2026-02-03 04:05:06".into()),
            ("2026-01-01 00:00:00".into(), "2026-01-01 00:00:00".into()),
        ]
    );
    for m in &rows {
        assert_eq!(
            (
                m.ubatch_size,
                m.batch_size,
                &m.mmproj_path,
                m.backend,
                m.threads
            ),
            (None, None, &None, None, None),
            "{}",
            m.model_id
        );
    }
    assert_eq!(
        rows.iter().find(|m| m.model_id == "bge-rr").unwrap().kind,
        AuxKind::Rerank
    );

    exec(
        &pool,
        "INSERT INTO aux_models (model_id, gguf_path, kind, backend, threads, ubatch_size, \
         batch_size, mmproj_path) VALUES ('laya', 'l.gguf', 'decision', 'cpu', 8, 8192, 8192, \
         'mm.gguf')",
    )
    .await
    .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM aux_models WHERE model_id = 'laya'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(id, 4, "the deleted top id is not reused");
}

/// The CHECKs: a kind, backend or count no renderer could serve is refused by
/// the table itself.
#[tokio::test]
async fn the_aux_table_refuses_a_kind_backend_or_count_it_cannot_render() {
    let pool = db_at_version(79).await;
    let insert = |cols: &str, vals: &str| {
        format!(
            "INSERT INTO aux_models (model_id, gguf_path, {cols}) VALUES ('m', 'm.gguf', {vals})"
        )
    };
    for (cols, vals) in [
        ("kind", "'classify'"),
        ("backend", "'cuda'"),
        ("backend", "''"),
        ("threads", "0"),
        ("ubatch_size", "-512"),
        ("batch_size", "0"),
        ("mmproj_path", "''"),
    ] {
        let err = exec(&pool, &insert(cols, vals)).await.unwrap_err();
        assert!(
            err.to_string().contains("CHECK constraint failed"),
            "{cols} = {vals}: {err}"
        );
    }
    exec(
        &pool,
        &insert("kind, backend, threads", "'decision', 'cpu', 1"),
    )
    .await
    .unwrap();
}
