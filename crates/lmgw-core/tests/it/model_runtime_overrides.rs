//! Per-model container overrides (per-model-containers design §3.1, §6):
//! migration 0022's three new columns on `local_models`/`aux_models`/
//! `audio_models`, and the store round trip that reads/writes them.
//!
//! `db_at_version` mirrors `tests/it/migrations.rs`'s helper of the same name —
//! duplicated rather than shared, since each integration test file is its own
//! binary and this crate has no shared test-support module.

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use lmgw_core::store::{self, NewAuxModel, NewLocalModel};

/// An in-memory database migrated up to `version` and no further.
async fn db_at_version(version: i64) -> SqlitePool {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    let mut m = sqlx::migrate!("./migrations");
    m.migrations = m
        .migrations
        .iter()
        .filter(|x| x.version <= version)
        .cloned()
        .collect::<Vec<_>>()
        .into();
    m.run(&pool).await.unwrap();
    pool
}

// ---------------------------------------------------------------------------
// Migration shape
// ---------------------------------------------------------------------------

/// Asserts the three-column shape on one already-fetched `PRAGMA
/// table_info(...)` result. A free function rather than a loop over a
/// `format!`-built query string: sqlx flags dynamic SQL for injection review,
/// and there is nothing dynamic about three fixed table names worth routing
/// around that check for.
fn assert_override_columns(table: &str, cols: &[sqlx::sqlite::SqliteRow]) {
    let find = |name: &str| cols.iter().find(|c| c.get::<String, _>("name") == name);

    let image = find("image").unwrap_or_else(|| panic!("{table}.image missing"));
    assert_eq!(
        image.get::<i64, _>("notnull"),
        0,
        "{table}.image must be nullable (NULL = inherit)"
    );

    let extra = find("extra_run_args").unwrap_or_else(|| panic!("{table}.extra_run_args missing"));
    assert_eq!(
        extra.get::<i64, _>("notnull"),
        0,
        "{table}.extra_run_args must be nullable (NULL = inherit)"
    );

    let warm = find("warm_start").unwrap_or_else(|| panic!("{table}.warm_start missing"));
    assert_eq!(
        warm.get::<i64, _>("notnull"),
        1,
        "{table}.warm_start must be NOT NULL"
    );
    assert_eq!(
        warm.get::<String, _>("dflt_value"),
        "0",
        "{table}.warm_start must default to off"
    );
}

#[tokio::test]
async fn the_three_new_columns_exist_on_all_three_tables_with_the_right_defaults() {
    let pool = store::open_in_memory().await.unwrap();

    let local = sqlx::query("PRAGMA table_info(local_models)")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_override_columns("local_models", &local);

    let aux = sqlx::query("PRAGMA table_info(aux_models)")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_override_columns("aux_models", &aux);

    let audio = sqlx::query("PRAGMA table_info(audio_models)")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_override_columns("audio_models", &audio);

    // The fourth class ships with the same three columns from its own
    // migration (0033) rather than inheriting them later.
    let image = sqlx::query("PRAGMA table_info(image_models)")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_override_columns("image_models", &image);
}

/// Rows written before migration 0022 existed — no `image`/`extra_run_args`/
/// `warm_start` columns in the schema at all — must read back exactly like
/// every row created since: an unset per-model override (inherit) and
/// warm_start off, not an error or a NULL that panics on read.
#[tokio::test]
async fn old_rows_migrate_to_inherit_and_warm_start_off() {
    let pool = db_at_version(21).await;
    sqlx::query(
        "INSERT INTO local_models (model_id, gguf_path, params, args, idle_seconds, enabled, public)
         VALUES ('legacy-chat', 'legacy.gguf', '{}', '[]', 300, 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO aux_models (model_id, gguf_path, kind, args, idle_seconds, enabled)
         VALUES ('legacy-aux', 'legacy-aux.gguf', 'embed', '[]', 0, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO audio_models (model_id, family, path, task, mode, enabled)
         VALUES ('legacy-audio', 'pocket_tts', 'legacy', 'tts', 'offline', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    store::run_migrations(&pool).await.unwrap();

    let local = store::list_local_models(&pool).await.unwrap();
    let m = local.iter().find(|m| m.model_id == "legacy-chat").unwrap();
    assert_eq!(m.image, None);
    assert_eq!(m.extra_run_args, None);
    assert!(!m.warm_start);

    let aux = store::list_aux_models(&pool).await.unwrap();
    let a = aux.iter().find(|a| a.model_id == "legacy-aux").unwrap();
    assert_eq!(a.image, None);
    assert_eq!(a.extra_run_args, None);
    assert!(!a.warm_start);

    let audio = store::list_audio_models(&pool).await.unwrap();
    let d = audio.iter().find(|d| d.model_id == "legacy-audio").unwrap();
    assert_eq!(d.image, None);
    assert_eq!(d.extra_run_args, None);
    assert!(!d.warm_start);
}

// ---------------------------------------------------------------------------
// Store round trip: unset (inherit) -> overridden -> cleared back to unset
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_model_round_trips_the_per_model_container_overrides() {
    let pool = store::open_in_memory().await.unwrap();
    let base = NewLocalModel {
        model_id: "m1".into(),
        gguf_path: "m1.gguf".into(),
        params: Default::default(),
        args: vec![],
        idle_seconds: 300,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
        ladder: vec![],
    };
    let id = store::insert_local_model(&pool, &base).await.unwrap();
    let get = || async { store::list_local_models(&pool).await.unwrap() };

    let m = get().await;
    let m = m.iter().find(|m| m.id == id).unwrap();
    assert_eq!(m.image, None, "an unset override stays unset (inherit)");
    assert_eq!(m.extra_run_args, None);
    assert!(!m.warm_start);

    let overridden = NewLocalModel {
        image: Some("my/own-image".into()),
        extra_run_args: Some(vec!["--device".into(), "nvidia.com/gpu=all".into()]),
        warm_start: true,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        ..base
    };
    store::update_local_model(&pool, id, &overridden)
        .await
        .unwrap();
    let m = get().await;
    let m = m.iter().find(|m| m.id == id).unwrap();
    assert_eq!(m.image.as_deref(), Some("my/own-image"));
    assert_eq!(
        m.extra_run_args,
        Some(vec![
            "--device".to_string(),
            "nvidia.com/gpu=all".to_string()
        ])
    );
    assert!(m.warm_start);

    // Clearing the override goes back to NULL/inherit, not an empty vec —
    // the two must stay distinguishable through a real write, not just in
    // the type.
    let cleared = NewLocalModel {
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        ..overridden
    };
    store::update_local_model(&pool, id, &cleared)
        .await
        .unwrap();
    let m = get().await;
    let m = m.iter().find(|m| m.id == id).unwrap();
    assert_eq!(m.image, None);
    assert_eq!(m.extra_run_args, None);
    assert!(!m.warm_start);
}

#[tokio::test]
async fn aux_model_round_trips_the_per_model_container_overrides() {
    use lmgw_core::config::AuxKind;

    let pool = store::open_in_memory().await.unwrap();
    let base = NewAuxModel {
        model_id: "a1".into(),
        gguf_path: "a1.gguf".into(),
        kind: AuxKind::Embed,
        pooling: None,
        ctx_size: None,
        args: vec![],
        idle_seconds: 0,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    };
    let id = store::insert_aux_model(&pool, &base).await.unwrap();

    let m = store::get_aux_model(&pool, id).await.unwrap().unwrap();
    assert_eq!(m.image, None);
    assert_eq!(m.extra_run_args, None);
    assert!(!m.warm_start);

    let overridden = NewAuxModel {
        image: Some("my/own-aux-image".into()),
        extra_run_args: Some(vec!["--security-opt".into(), "label=disable".into()]),
        warm_start: true,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        ..base
    };
    store::update_aux_model(&pool, id, &overridden)
        .await
        .unwrap();
    let m = store::get_aux_model(&pool, id).await.unwrap().unwrap();
    assert_eq!(m.image.as_deref(), Some("my/own-aux-image"));
    assert_eq!(
        m.extra_run_args,
        Some(vec![
            "--security-opt".to_string(),
            "label=disable".to_string()
        ])
    );
    assert!(m.warm_start);

    let cleared = NewAuxModel {
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        ..overridden
    };
    store::update_aux_model(&pool, id, &cleared).await.unwrap();
    let m = store::get_aux_model(&pool, id).await.unwrap().unwrap();
    assert_eq!(m.image, None);
    assert_eq!(m.extra_run_args, None);
    assert!(!m.warm_start);
}

/// The image class resolves the same three overrides the same way — unset is
/// inherit, a value wins, and clearing it goes back to inherit — and its
/// descriptor is what a start would actually run.
#[tokio::test]
async fn image_model_round_trips_the_per_model_container_overrides() {
    use lmgw_core::runtime::descriptor::model_runtime;
    use lmgw_core::runtime::Class;

    let pool = store::open_in_memory().await.unwrap();
    let mut base = lmgw_core::store::NewImageModel {
        model_id: "z-image".into(),
        files: serde_json::json!({"diffusion_model": "z/z.gguf"})
            .as_object()
            .cloned()
            .unwrap(),
        modes: vec!["img_gen".into()],
        enabled: true,
        idle_seconds: 300,
        ..Default::default()
    };
    let id = lmgw_core::store::insert_image_model(&pool, &base)
        .await
        .unwrap();

    let snap = store::load_snapshot(&pool).await.unwrap();
    let rt = model_runtime(&snap, Class::Image, "z-image").unwrap();
    assert_eq!(
        rt.image, "ghcr.io/leejet/stable-diffusion.cpp:master-cuda",
        "an unset override inherits the class image"
    );
    assert_eq!(rt.extra_run_args, snap.settings.image.extra_run_args);
    assert!(!rt.warm_start);

    base.image = Some("localhost/sdcpp@sha256:8771c2d5".into());
    base.extra_run_args = Some(vec!["--device".into(), "amd.com/gpu=all".into()]);
    base.warm_start = true;
    lmgw_core::store::update_image_model(&pool, id, &base)
        .await
        .unwrap();

    let snap = store::load_snapshot(&pool).await.unwrap();
    let rt = model_runtime(&snap, Class::Image, "z-image").unwrap();
    assert_eq!(rt.image, "localhost/sdcpp@sha256:8771c2d5");
    assert_eq!(
        rt.extra_run_args,
        vec!["--device".to_string(), "amd.com/gpu=all".to_string()]
    );
    assert!(rt.warm_start);

    base.image = None;
    base.extra_run_args = None;
    lmgw_core::store::update_image_model(&pool, id, &base)
        .await
        .unwrap();
    let snap = store::load_snapshot(&pool).await.unwrap();
    let rt = model_runtime(&snap, Class::Image, "z-image").unwrap();
    assert_eq!(
        rt.image, "ghcr.io/leejet/stable-diffusion.cpp:master-cuda",
        "clearing an override goes back to inherit"
    );
    assert_eq!(rt.extra_run_args, snap.settings.image.extra_run_args);
}
