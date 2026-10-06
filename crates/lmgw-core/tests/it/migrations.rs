//! Upgrading an existing install: the one SQL migration that could refuse to,
//! and the settings-blob shape change per-model containers brought (§6).
//!
//! Migration 0018 renames the managed upstream row `llama-embed` → `llama-aux`
//! with a plain `UPDATE` on a `UNIQUE` column, inside a `-- no-transaction`
//! migration. An install that already had an upstream of its own by that name
//! hit the constraint with nothing to roll back: the file was left half
//! applied, sqlx never recorded it, and every later start replayed it and died
//! somewhere else. The gateway could not be started again.
//!
//! 0018 itself is frozen — sqlx validates the SHA-384 of every migration it has
//! already applied — so the repair runs ahead of the migrator, and these tests
//! drive it by building a database at the version an unlucky install was stuck
//! at and upgrading it for real.

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

mod llama_cpp;

/// An in-memory database migrated up to `version` and no further — the state an
/// install that has not taken the aux-router upgrade yet is sitting in.
pub(crate) async fn db_at_version(version: i64) -> SqlitePool {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    // sqlx's own runner, over a truncated migration list: the bookkeeping table
    // and its rows then look exactly like a real install's.
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

async fn name_of(pool: &SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT name FROM upstreams WHERE id = ?1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn exists(pool: &SqlitePool, id: i64) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT id FROM upstreams WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
        .is_some()
}

async fn id_of(pool: &SqlitePool, name: &str) -> Option<i64> {
    sqlx::query_scalar("SELECT id FROM upstreams WHERE name = ?1")
        .bind(name)
        .fetch_optional(pool)
        .await
        .unwrap()
}

async fn insert_upstream(pool: &SqlitePool, name: &str, port: u16) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO upstreams (name, protocol, kind, base_url, expose_all)
         VALUES (?1, 'openai', 'llama_server', ?2, 1) RETURNING id",
    )
    .bind(name)
    .bind(format!("http://127.0.0.1:{port}/v1"))
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn applied_versions(pool: &SqlitePool) -> Vec<i64> {
    sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// The collision itself: an owner who had already named an upstream `llama-aux`
/// must still be able to start the gateway. The pre-existing row keeps its id
/// (so the aliases pointing at it survive) and is renamed out of the way, the
/// managed row takes the name the migration wants — and then 0023 deletes the
/// managed row, because per-model containers serve that class from its own
/// table instead of through an upstream row (§5). The owner's row is the one
/// that comes out the other side.
#[tokio::test]
async fn an_upstream_already_named_llama_aux_still_upgrades() {
    let pool = db_at_version(17).await;
    let managed = insert_upstream(&pool, "llama-embed", 18081).await;
    let owners = insert_upstream(&pool, "llama-aux", 9999).await;

    lmgw_core::store::run_migrations(&pool)
        .await
        .expect("an install with this name taken must still start");

    assert!(
        !exists(&pool, managed).await,
        "the managed row took the name and 0023 then removed it"
    );
    assert_eq!(
        name_of(&pool, owners).await,
        "llama-aux-conflict",
        "the owner's row is renamed, not deleted — its id and everything \
         pointing at it survive"
    );
    assert!(id_of(&pool, "llama-embed").await.is_none());
    assert!(id_of(&pool, "llama-aux").await.is_none());

    // …and the rest of the upgrade really did run.
    let versions = applied_versions(&pool).await;
    assert!(
        versions.contains(&18) && versions.contains(&20),
        "{versions:?}"
    );
    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('aux_models') WHERE name = 'kind'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(kinds.len(), 1, "aux_models gained its class discriminant");
}

/// A second `llama-aux-conflict` does not collide either — the repair looks for
/// a free name rather than assuming one.
#[tokio::test]
async fn the_renamed_row_never_lands_on_another_taken_name() {
    let pool = db_at_version(17).await;
    insert_upstream(&pool, "llama-embed", 18081).await;
    let owners = insert_upstream(&pool, "llama-aux", 9999).await;
    insert_upstream(&pool, "llama-aux-conflict", 9998).await;

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    assert_eq!(name_of(&pool, owners).await, "llama-aux-conflict-2");
}

/// The install the reviewer reproduced: 0018 got past its first statement and
/// then died on the collision, leaving `aux_models` renamed, no `_sqlx_migrations`
/// row, and a file that can only fail differently on the next try. The next
/// start has to finish what 0018 started and record it, not replay it.
#[tokio::test]
async fn a_half_applied_aux_migration_recovers_on_the_next_start() {
    let pool = db_at_version(17).await;
    let managed = insert_upstream(&pool, "llama-embed", 18081).await;
    let owners = insert_upstream(&pool, "llama-aux", 9999).await;
    sqlx::query("INSERT INTO settings (key, value) VALUES ('settings', ?1)")
        .bind(r#"{"embed_router":{"listen_port":18081},"vram":{"enabled":true}}"#)
        .execute(&pool)
        .await
        .unwrap();

    // Exactly how far 0018 gets before the UNIQUE violation stops it.
    sqlx::query("ALTER TABLE embed_models RENAME TO aux_models")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!applied_versions(&pool).await.contains(&18));

    lmgw_core::store::run_migrations(&pool)
        .await
        .expect("a half-applied 0018 must be recoverable");

    // Every one of 0018's four steps, finished.
    assert!(
        sqlx::query("SELECT kind FROM aux_models LIMIT 1")
            .fetch_optional(&pool)
            .await
            .is_ok(),
        "step 1: the class discriminant"
    );
    let hf: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'hf_models'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(hf.contains("'aux'"), "step 2: the download target — {hf}");
    // Step 3 (the rename) is proved by what it left behind: no `llama-embed`
    // row anywhere, the owner's row moved aside to free the name — and then
    // 0023 removed the renamed managed row, so the name is free again.
    assert!(id_of(&pool, "llama-embed").await.is_none(), "step 3");
    assert!(!exists(&pool, managed).await, "step 3, then 0023");
    assert_eq!(name_of(&pool, owners).await, "llama-aux-conflict");
    let settings: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'settings'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        settings.contains("aux_router") && !settings.contains("embed_router"),
        "step 4: the settings key — {settings}"
    );

    // Recorded rather than replayed, with the checksum sqlx will validate on
    // the next start — so this is a one-time repair, not a permanent fixup.
    let versions = applied_versions(&pool).await;
    assert!(
        versions.contains(&18) && versions.contains(&20),
        "{versions:?}"
    );
    let recorded: Vec<u8> =
        sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 18")
            .fetch_one(&pool)
            .await
            .unwrap();
    let expected = sqlx::migrate!("./migrations")
        .migrations
        .iter()
        .find(|m| m.version == 18)
        .unwrap()
        .checksum
        .to_vec();
    assert_eq!(recorded, expected);

    // And starting again is a no-op, not a second repair.
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    assert_eq!(name_of(&pool, owners).await, "llama-aux-conflict");
}

/// The hazard behind all of the above, kept from spreading.
///
/// sqlx decides whether a migration runs outside a transaction by looking at
/// the *start of the file*: `sql.starts_with("-- no-transaction")`. It is a
/// prefix test on the whole text, not a match on a directive line, so a comment
/// that merely begins that way opts a migration out of its transaction with
/// nothing to say it did — and a migration without a transaction is the thing
/// that leaves a database half migrated with no way back.
///
/// The check is deliberately wider than the prefix sqlx actually looks at: any
/// migration whose first line starts with `-- n` must be exactly the directive.
/// A future `-- note: …` opening line is then a failing test rather than a
/// class of bug nobody notices until an upgrade bricks an install.
#[test]
fn a_migration_that_opens_with_a_dash_dash_n_comment_is_the_directive_itself() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let mut checked = 0;
    for crate_name in ["lmgw-core", "quickdoc-core"] {
        let dir = root.join(crate_name).join("migrations");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "sql"))
            .collect();
        files.sort();
        assert!(!files.is_empty(), "no migrations under {}", dir.display());
        for path in files {
            let sql = std::fs::read_to_string(&path).unwrap();
            let first = sql.lines().next().unwrap_or_default();
            if first.starts_with("-- n") {
                assert_eq!(
                    first,
                    "-- no-transaction",
                    "{}: a migration opening with `-- n` is read by sqlx as the \
                     no-transaction directive — say exactly that, or start the file \
                     with a different word",
                    path.display()
                );
            }
            checked += 1;
        }
    }
    assert!(checked >= 20, "only {checked} migration files were read");
}

/// From 0058 on, `-- no-transaction` is the first line of a migration or
/// nowhere in it. sqlx reads the directive only as the file's prefix, so one
/// written lower down is a comment: the file runs inside sqlx's transaction,
/// where its `PRAGMA foreign_keys = OFF` does nothing and a table rebuild's
/// `DROP TABLE` cascades through every row that references it. 0013 is such
/// a file; the files before 0058 are applied history and stay as they are.
#[test]
fn from_0058_on_no_transaction_is_the_first_line_or_nowhere() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
        if !name.ends_with(".sql") || digits.parse::<u32>().map_or(true, |v| v < 58) {
            continue;
        }
        let sql = std::fs::read_to_string(&path).unwrap();
        for (n, line) in sql.lines().enumerate().skip(1) {
            assert!(
                !line.trim_start().starts_with("-- no-transaction"),
                "{name}:{}: `-- no-transaction` below the first line is a comment to sqlx — \
                 move it to line 1, or say something else",
                n + 1
            );
        }
        checked += 1;
    }
    assert!(checked >= 1, "0058 itself was not read");
}

/// The 0018 repair must not touch a database that has nothing wrong with it,
/// and a fresh install has no tables at all when it runs.
#[tokio::test]
async fn a_healthy_database_is_left_alone() {
    let fresh = lmgw_core::store::open_in_memory().await.unwrap();
    assert!(applied_versions(&fresh).await.contains(&18));
    assert!(applied_versions(&fresh).await.contains(&23));
    // Nothing to delete on a database that never had the managed rows.
    assert!(id_of(&fresh, "llama-aux").await.is_none());
}

/// The other side of the same coin, stated plainly because it is a real
/// consequence: after 0023 the two managed names belong to the gateway. 0023
/// matches on the name alone — the only thing that ever distinguished those
/// rows — so a row an owner created under one of them post-0018 is removed
/// too. It is logged (`store::removal_notice`), and anything *pointing* at it
/// would have refused the upgrade outright rather than cascading.
#[tokio::test]
async fn a_row_holding_a_managed_name_is_removed_by_the_synthetic_migration() {
    let pool = db_at_version(22).await;
    let owners = insert_upstream(&pool, "llama-aux", 9999).await;
    let audio = insert_upstream(&pool, "audiocpp", 9294).await;
    let mine = insert_upstream(&pool, "groq", 443).await;

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    assert!(!exists(&pool, owners).await);
    assert!(!exists(&pool, audio).await);
    assert!(
        exists(&pool, mine).await,
        "every other upstream is untouched"
    );
}

/// The guard in front of 0023: `models.upstream_id` cascades, so deleting the
/// managed rows would delete any alias pointing at one *silently*. The upgrade
/// stops instead, and the error names the aliases so the owner knows exactly
/// what is in the way. Nothing is changed by the refusal.
#[tokio::test]
async fn an_alias_pinned_to_a_managed_upstream_refuses_the_upgrade_by_name() {
    let pool = db_at_version(22).await;
    let managed = insert_upstream(&pool, "llama-aux", 9293).await;
    sqlx::query(
        "INSERT INTO models (alias, upstream_id, upstream_model_id) VALUES ('my-embedder', ?1, 'bge-m3')",
    )
    .bind(managed)
    .execute(&pool)
    .await
    .unwrap();

    let err = lmgw_core::store::run_migrations(&pool)
        .await
        .expect_err("an alias pinned to a deleted row must stop the upgrade");
    let msg = err.to_string();
    assert!(
        msg.contains("my-embedder") && msg.contains("llama-aux"),
        "the refusal has to name the alias and what it points at: {msg}"
    );

    // Refused, not half-done: the row and its alias are both still there, and
    // 0023 is not recorded as applied.
    assert!(exists(&pool, managed).await);
    let alias: Option<String> =
        sqlx::query_scalar("SELECT alias FROM models WHERE alias = 'my-embedder'")
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(alias.as_deref(), Some("my-embedder"));
    assert!(!applied_versions(&pool).await.contains(&23));
}

/// A clean deployment — the shape every real install is in — migrates straight
/// through, managed rows and all.
#[tokio::test]
async fn a_deployment_without_alias_rows_migrates_the_managed_rows_away() {
    let pool = db_at_version(22).await;
    insert_upstream(&pool, "llama-aux", 9293).await;
    insert_upstream(&pool, "audiocpp", 9294).await;
    // A hide-preference for the passthrough surface that is going away: it
    // cascades in silence on purpose (0023's comment says why).
    sqlx::query(
        "INSERT INTO hidden_passthrough_models (upstream_id, model_id)
         SELECT id, 'bge-m3' FROM upstreams WHERE name = 'llama-aux'",
    )
    .execute(&pool)
    .await
    .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    assert!(applied_versions(&pool).await.contains(&23));
    assert!(id_of(&pool, "llama-aux").await.is_none());
    assert!(id_of(&pool, "audiocpp").await.is_none());
    let hidden: i64 = sqlx::query_scalar("SELECT count(*) FROM hidden_passthrough_models")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hidden, 0, "the hide-preference went with its subject");
}

/// A row named `llama-aux` on an install that has no `llama-embed` row to
/// rename is nobody's business but the 0018 repair's to leave alone: 0018's
/// `UPDATE` matches nothing there, so there is no collision to pre-empt.
///
/// The row itself does not survive the rest of the chain — 0023 claims the
/// name (see `a_row_holding_a_managed_name_is_removed_by_the_synthetic_migration`)
/// — but the *repair* never touched it, and the proof is that no
/// `llama-aux-conflict` row was ever created.
#[tokio::test]
async fn without_a_row_to_rename_nothing_is_renamed() {
    let pool = db_at_version(17).await;
    insert_upstream(&pool, "llama-aux", 9999).await;

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    assert!(
        id_of(&pool, "llama-aux-conflict").await.is_none(),
        "the repair renamed a row it had no business renaming"
    );
    assert!(applied_versions(&pool).await.contains(&18));
}

/// A relabelled row keeps everything that pointed at it: the failure the
/// rename-in-place avoids is aliases silently losing their upstream.
#[tokio::test]
async fn aliases_follow_the_renamed_row_because_its_id_never_moves() {
    let pool = db_at_version(17).await;
    insert_upstream(&pool, "llama-embed", 18081).await;
    let owners = insert_upstream(&pool, "llama-aux", 9999).await;
    sqlx::query(
        "INSERT INTO models (alias, upstream_id, upstream_model_id) VALUES ('mine', ?1, 'm')",
    )
    .bind(owners)
    .execute(&pool)
    .await
    .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let row = sqlx::query("SELECT upstream_id FROM models WHERE alias = 'mine'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<i64, _>("upstream_id"), owners);
}

// ---------------------------------------------------------------------------
// The settings blob: per-model containers §6
// ---------------------------------------------------------------------------

/// The four router-mode fields are dropped by deserialization alone (there is
/// no `deny_unknown_fields` on `Settings`), but `container_name` is the only
/// record of what a pre-upgrade install left running — nothing derives it, and
/// the containers carry no labels to find them by. So it is lifted, at load,
/// into `legacy_container_names`, before any save can rewrite the blob.
#[tokio::test]
async fn loading_a_router_mode_settings_blob_lifts_its_container_names() {
    let pool = lmgw_core::store::open_in_memory().await.unwrap();
    let raw = serde_json::json!({
        "bind_addr": "127.0.0.1:8001",
        "router": {
            "container_name": "acme-chat",
            "listen_port": 9292,
            "models_dir": "/srv/models",
            "models_max": 3,
            "auto_start": true,
            "public_prefix": "local",
        },
        "aux_router": { "container_name": "acme-embed", "listen_port": 9293 },
        "audio": { "container_name": "acme-audio", "listen_port": 9294 },
    })
    .to_string();
    lmgw_core::store::set_kv(&pool, "settings", &raw)
        .await
        .unwrap();

    let s = lmgw_core::store::load_settings(&pool).await.unwrap();
    assert_eq!(
        s.legacy_container_names,
        vec![
            "acme-chat".to_string(),
            "acme-embed".to_string(),
            "acme-audio".to_string()
        ]
    );
    // The rest of the blob still loads — this is a field drop, not a reset.
    assert_eq!(s.router.models_dir, "/srv/models");
    assert_eq!(s.router.public_prefix, "local");
    assert_eq!(s.bind_addr, "127.0.0.1:8001");

    // Saving rewrites the blob into the new shape: the four keys are gone, the
    // captured names are what survives.
    lmgw_core::store::save_settings(&pool, &s).await.unwrap();
    let stored = lmgw_core::store::get_kv(&pool, "settings")
        .await
        .unwrap()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&stored).unwrap();
    for section in ["router", "aux_router", "audio"] {
        for gone in ["container_name", "listen_port", "models_max", "auto_start"] {
            assert!(
                v[section].get(gone).is_none(),
                "`{section}.{gone}` survived the save: {stored}"
            );
        }
    }
    assert_eq!(
        v["legacy_container_names"],
        serde_json::json!(["acme-chat", "acme-embed", "acme-audio"])
    );

    // Idempotent: re-loading the rewritten blob neither re-captures nor
    // duplicates, and a blob that never had the old keys captures nothing.
    let again = lmgw_core::store::load_settings(&pool).await.unwrap();
    assert_eq!(again.legacy_container_names, s.legacy_container_names);
}

/// The pre-0018 spelling has to be swept too: a restored backup can still name
/// its aux container under `embed_router`, and that container is just as real.
/// An empty list is not persisted at all, so a fresh install carries nothing.
#[tokio::test]
async fn the_pre_rename_spelling_is_swept_and_an_empty_list_is_not_stored() {
    let pool = lmgw_core::store::open_in_memory().await.unwrap();
    let raw = serde_json::json!({
        "embed_router": { "container_name": "acme-embed" },
    })
    .to_string();
    lmgw_core::store::set_kv(&pool, "settings", &raw)
        .await
        .unwrap();
    let s = lmgw_core::store::load_settings(&pool).await.unwrap();
    assert_eq!(s.legacy_container_names, vec!["acme-embed".to_string()]);

    let fresh = lmgw_core::config::Settings::default();
    assert!(fresh.legacy_container_names.is_empty());
    lmgw_core::store::save_settings(&pool, &fresh)
        .await
        .unwrap();
    let stored = lmgw_core::store::get_kv(&pool, "settings")
        .await
        .unwrap()
        .unwrap();
    assert!(
        !stored.contains("legacy_container_names"),
        "an empty list must not be written: {stored}"
    );
}

// ---------------------------------------------------------------------------
// 0025: capabilities_override (model-capabilities design §7)
// ---------------------------------------------------------------------------

/// A row written before 0025 has no `capabilities_override` column at all;
/// the migration must add it nullable so that row reads back as `None`
/// rather than failing to load or being defaulted to some invented value —
/// and a value written after the upgrade must round-trip.
#[tokio::test]
async fn capabilities_override_column_lands_nullable_on_local_models_and_aliases() {
    let pool = db_at_version(24).await;
    let up_id = insert_upstream(&pool, "cloud", 9).await;
    sqlx::query(
        "INSERT INTO local_models (model_id, gguf_path) VALUES ('legacy-chat', 'legacy.gguf')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO models (alias, upstream_id, upstream_model_id) \
         VALUES ('legacy-alias', ?1, 'tgt')",
    )
    .bind(up_id)
    .execute(&pool)
    .await
    .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let locals = lmgw_core::store::list_local_models(&pool).await.unwrap();
    let row = locals
        .iter()
        .find(|m| m.model_id == "legacy-chat")
        .expect("legacy row survives the upgrade");
    assert_eq!(row.capabilities_override, None);

    let aliases = lmgw_core::store::list_aliases(&pool).await.unwrap();
    let alias = aliases
        .iter()
        .find(|a| a.alias == "legacy-alias")
        .expect("legacy alias survives the upgrade");
    assert_eq!(alias.capabilities_override, None);

    // The column is real and a value written into it round-trips through the
    // same store functions a fresh install uses.
    sqlx::query(
        "UPDATE local_models SET capabilities_override = ?1 WHERE model_id = 'legacy-chat'",
    )
    .bind(r#"{"max_output_tokens":4096}"#)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE models SET capabilities_override = ?1 WHERE alias = 'legacy-alias'")
        .bind(r#"{"notes":["hand-set"]}"#)
        .execute(&pool)
        .await
        .unwrap();

    let locals = lmgw_core::store::list_local_models(&pool).await.unwrap();
    let row = locals.iter().find(|m| m.model_id == "legacy-chat").unwrap();
    assert_eq!(
        row.capabilities_override,
        Some(serde_json::json!({"max_output_tokens": 4096}))
    );
    let aliases = lmgw_core::store::list_aliases(&pool).await.unwrap();
    let alias = aliases.iter().find(|a| a.alias == "legacy-alias").unwrap();
    assert_eq!(
        alias.capabilities_override,
        Some(serde_json::json!({"notes": ["hand-set"]}))
    );
}

/// Migration 0031 (agent-catalog §3) on an install that has been running the
/// mail workflow: the identity row it logged under is **renamed**, not
/// replaced, so the spend history the agent catalog inherits is the same row.
/// The chat threads it upgrades gain the nullable `agent_id` the `chat` run
/// kind lists by.
#[tokio::test]
async fn migration_0031_renames_the_workflow_identity_and_keeps_its_history() {
    let pool = db_at_version(30).await;

    // An install with a mail workflow that has been spending money.
    let key_id: i64 =
        sqlx::query_scalar("SELECT id FROM api_keys WHERE name = 'internal:workflow-mail'")
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query("INSERT INTO chat_threads (model_alias, kind) VALUES ('local-a', 'chat')")
        .execute(&pool)
        .await
        .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    // Same row id: every request_logs row pointing at it still resolves.
    let renamed: (i64, String, String) =
        sqlx::query_as("SELECT id, name, note FROM api_keys WHERE name = 'internal:agents'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(renamed.0, key_id);
    assert!(renamed.2.contains("Agent runs"), "{}", renamed.2);
    let leftovers: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE name = 'internal:workflow-mail'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(leftovers, 0);
    // ... and it is the row every agent run now logs under, so the spend the
    // mail workflow booked and the spend the agents that replaced it book are
    // one continuous line.
    assert_eq!(
        lmgw_core::telemetry::internal_identity(lmgw_core::telemetry::AGENT_PROTO),
        Some("internal:agents")
    );

    // The catalog table and the thread column exist, and an existing thread is
    // simply not attached to an agent.
    let threads: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM chat_threads WHERE agent_id IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(threads, 1);
    let agents: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(agents, 0, "migrations must not seed; AppState::init does");

    // The CHECK on `source` is real.
    let bad =
        sqlx::query("INSERT INTO agents (id, manifest, source) VALUES ('x', '{}', 'nonsense')")
            .execute(&pool)
            .await;
    assert!(bad.is_err(), "the source CHECK did not hold");
}

/// 0032 rebuilds `api_keys` to widen the `kind` CHECK and add `key_plain` /
/// `agent_id` (container-runtime §5). A rebuild is a copy, and two soft links
/// point at these ids — `request_logs.key_id` (0027) and `usage_hourly.key_id`
/// (0028) — so the ids have to come through the copy verbatim, and the ids that
/// come *after* must not collide with a deleted one.
#[tokio::test]
async fn migration_0032_preserves_every_api_key_id_a_request_log_points_at() {
    let pool = db_at_version(31).await;

    // Three keys with ids a log row names, past the internal identities 0029
    // seeds, and with gaps where others were deleted — the state any install
    // that has ever revoked a key is in.
    for (id, name) in [(101_i64, "k-one"), (102, "k-two"), (107, "k-seven")] {
        sqlx::query("INSERT INTO api_keys (id, name, key_hash, note) VALUES (?1, ?2, ?3, ?4)")
            .bind(id)
            .bind(name)
            .bind(format!("hash-{id}"))
            .bind(format!("note {id}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO request_logs (ts, ingress_proto, requested_alias, status, streamed, key_id)
             VALUES (datetime('now'), 'openai', 'm1', 200, 0, ?1)",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    }
    let before: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT id, name, key_hash FROM api_keys ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();

    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let after: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT id, name, key_hash FROM api_keys ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        before, after,
        "every id, name and hash survived the rebuild"
    );

    // Every log row still resolves to the key it named.
    let orphans: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_logs r
          WHERE r.key_id IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM api_keys k WHERE k.id = r.key_id)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(orphans, 0);

    // AUTOINCREMENT, kept deliberately: without it SQLite hands out max(id)+1
    // and re-uses a deleted row's id — and `agent_delete` now deletes key rows,
    // so the next key would silently inherit the deleted agent's usage history.
    sqlx::query("DELETE FROM api_keys WHERE id = 107")
        .execute(&pool)
        .await
        .unwrap();
    let fresh: i64 = sqlx::query_scalar(
        "INSERT INTO api_keys (name, key_hash) VALUES ('k-new', 'h') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        fresh > 107,
        "a deleted id must never be handed out again (got {fresh})"
    );

    // The widened CHECK, and the two invariants tying the new columns to it.
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, key_plain, kind, agent_id)
         VALUES ('agent:labeler', 'h2', 'plain', 'agent', 'labeler')",
    )
    .execute(&pool)
    .await
    .expect("kind='agent' is legal now");
    assert!(
        sqlx::query(
            "INSERT INTO api_keys (name, key_hash, kind, agent_id)
             VALUES ('agent:bad', 'h3', 'agent', 'bad')"
        )
        .execute(&pool)
        .await
        .is_err(),
        "an agent row without a plaintext is refused"
    );
    assert!(
        sqlx::query(
            "INSERT INTO api_keys (name, key_hash, key_plain, kind)
             VALUES ('plain-key', 'h4', 'leaked', 'key')"
        )
        .execute(&pool)
        .await
        .is_err(),
        "a non-agent row may not carry a plaintext"
    );
    assert!(
        sqlx::query(
            "INSERT INTO api_keys (name, key_hash, key_plain, kind, agent_id)
             VALUES ('agent:labeler-2', 'h5', 'p', 'agent', 'labeler')"
        )
        .execute(&pool)
        .await
        .is_err(),
        "one token per agent"
    );

    // And the two catalog columns the runtime needs.
    sqlx::query("INSERT INTO agents (id, manifest) VALUES ('a', '{}')")
        .execute(&pool)
        .await
        .unwrap();
    let (provenance, dev_url): (String, Option<String>) =
        sqlx::query_as("SELECT provenance, dev_url FROM agents WHERE id = 'a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(provenance, "{}");
    assert_eq!(dev_url, None);

    // ---------------------------------------------------------------------
    // And the watermark, which the rows alone do not carry.
    // ---------------------------------------------------------------------
    //
    // A second install, because this one's ids are in the hundreds and the
    // question needs a highest id that was deleted *before* the migration ran.
    // `DROP TABLE api_keys` discards that table's `sqlite_sequence` row, so
    // without the carry-over the rebuilt table starts again from `max(id)` and
    // hands the deleted key's id straight back — inheriting its
    // `request_logs` / `usage_hourly` history, which is the whole reason
    // AUTOINCREMENT is on this table.
    let pool = db_at_version(31).await;
    for (id, name) in [(9_i64, "k-nine"), (10, "k-ten"), (11, "k-eleven")] {
        sqlx::query("INSERT INTO api_keys (id, name, key_hash) VALUES (?1, ?2, ?3)")
            .bind(id)
            .bind(name)
            .bind(format!("hash-{id}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    // Revoked while the install was still at 0031 — the id is spent, and the
    // usage rows that named it are still in the log.
    sqlx::query(
        "INSERT INTO request_logs (ts, ingress_proto, requested_alias, status, streamed, key_id)
         VALUES (datetime('now'), 'openai', 'm1', 200, 0, 11)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM api_keys WHERE id = 11")
        .execute(&pool)
        .await
        .unwrap();

    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let next: i64 = sqlx::query_scalar(
        "INSERT INTO api_keys (name, key_hash) VALUES ('k-next', 'h') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        next, 12,
        "the rebuild must carry the AUTOINCREMENT watermark, not restart from max(id): id 11 was \
         deleted before the migration and a request_log still points at it"
    );
}

/// The DB file is clamped to `0600` because it holds plaintext secrets — and in
/// WAL mode a committed page lives in `lmgw.sqlite-wal` until the next
/// checkpoint, so every one of those secrets is readable there too. SQLite
/// creates the sidecars with the process umask rather than inheriting the main
/// file's mode, so clamping one path and calling it done left the pages in the
/// clear (final review).
#[cfg(unix)]
#[tokio::test]
async fn opening_the_database_clamps_the_wal_sidecars_to_0600_as_well() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("lmgw-perm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("lmgw.sqlite");

    let pool = lmgw_core::store::open(&path).await.unwrap();
    // A write, so the WAL is not merely created but has something in it.
    sqlx::query("INSERT INTO api_keys (name, key_hash) VALUES ('k', 'h')")
        .execute(&pool)
        .await
        .unwrap();
    lmgw_core::store::open(&path).await.unwrap(); // the clamp runs on every open

    let mut checked = 0;
    for suffix in ["", "-wal", "-shm"] {
        let p = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        if !p.exists() {
            continue;
        }
        checked += 1;
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o600,
            "{} is {mode:o}, and it holds the same secrets the database does",
            p.display()
        );
    }
    assert!(checked >= 2, "the WAL sidecars were never created");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Migrations 0033/0034 on an install that already has models of the other
/// three classes: the `image_models` table arrives with the columns the
/// runtime reads, `hf_models.target` accepts `'image'`, and nothing that was
/// there before moves.
#[tokio::test]
async fn migration_0033_adds_the_image_class_without_disturbing_the_others() {
    let pool = db_at_version(32).await;
    sqlx::query(
        "INSERT INTO local_models (model_id, gguf_path) VALUES ('legacy-chat', 'legacy.gguf')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO hf_models (repo, file, dest_path, target) \
         VALUES ('o/r', 'f.gguf', 'o/r/f.gguf', 'chat')",
    )
    .execute(&pool)
    .await
    .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    // The pre-existing rows survive both the table creation and the
    // hf_models rebuild (which drops and recreates the table).
    let locals = lmgw_core::store::list_local_models(&pool).await.unwrap();
    assert!(locals.iter().any(|m| m.model_id == "legacy-chat"));
    let targets: Vec<String> = sqlx::query_scalar("SELECT target FROM hf_models")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(targets, vec!["chat".to_string()]);

    // The fourth download target is now legal, and a fifth still is not.
    sqlx::query(
        "INSERT INTO hf_models (repo, file, dest_path, target) \
         VALUES ('leejet/Z-Image-Turbo-GGUF', 'z.gguf', 'leejet/z.gguf', 'image')",
    )
    .execute(&pool)
    .await
    .expect("'image' passes the widened CHECK");
    let refused = sqlx::query(
        "INSERT INTO hf_models (repo, file, dest_path, target) \
         VALUES ('o/r', 'v.gguf', 'o/r/v.gguf', 'video')",
    )
    .execute(&pool)
    .await;
    assert!(refused.is_err(), "the CHECK still names the four it knows");
}

/// One image row through the store, end to end: the JSON columns round-trip,
/// the defaults are the ones the runtime expects, and `idle_seconds` matches
/// what `local_models` has carried since 0001 — this class does not inherit
/// audio's missing idle column.
#[tokio::test]
async fn an_image_row_round_trips_through_the_store_with_its_defaults() {
    let pool = lmgw_core::store::open_in_memory().await.unwrap();

    // Straight SQL first, so the *column* defaults are what is under test
    // rather than the struct's.
    sqlx::query("INSERT INTO image_models (model_id) VALUES ('bare')")
        .execute(&pool)
        .await
        .unwrap();
    let bare = lmgw_core::store::list_image_models(&pool).await.unwrap();
    let bare = &bare[0];
    assert_eq!(bare.idle_seconds, 300, "as local_models since 0001");
    assert_eq!(bare.modes, vec!["img_gen".to_string()]);
    assert!(!bare.edit);
    assert!(bare.enabled);
    assert!(!bare.warm_start);
    assert!(bare.files.is_empty() && bare.args.is_empty());
    assert_eq!(bare.image, None, "NULL inherits the class image");
    assert_eq!(bare.extra_run_args, None);
    assert_eq!(bare.capabilities_override, None);
    assert_eq!(
        bare.hold_fallback_mode,
        lmgw_core::config::HoldFallbackMode::Inherit
    );

    let new = lmgw_core::store::NewImageModel {
        model_id: "z-image-turbo".into(),
        files: serde_json::json!({
            "diffusion_model": "leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf",
            "vae": "Comfy-Org/z_image_turbo/ae.safetensors"
        })
        .as_object()
        .cloned()
        .unwrap(),
        args: serde_json::json!({"diffusion_fa": true, "cfg_scale": 1.0, "steps": 8})
            .as_object()
            .cloned()
            .unwrap(),
        modes: vec!["img_gen".into()],
        edit: true,
        enabled: true,
        image: Some("localhost/sdcpp@sha256:8771c2d5".into()),
        extra_run_args: Some(vec!["--device".into(), "nvidia.com/gpu=all".into()]),
        warm_start: true,
        idle_seconds: 900,
        hold_fallback_mode: lmgw_core::config::HoldFallbackMode::Alias,
        hold_fallback: Some("openai/gpt-image-1".into()),
        capabilities_override: Some(serde_json::json!({"notes": ["hand-set"]})),
    };
    let id = lmgw_core::store::insert_image_model(&pool, &new)
        .await
        .unwrap();

    let row = lmgw_core::store::get_image_model(&pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.model_id, "z-image-turbo");
    assert_eq!(
        row.files.get("vae").and_then(|v| v.as_str()),
        Some("Comfy-Org/z_image_turbo/ae.safetensors")
    );
    assert_eq!(row.args.get("steps").and_then(|v| v.as_i64()), Some(8));
    assert_eq!(
        row.args.get("diffusion_fa").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(row.edit && row.warm_start);
    assert_eq!(row.idle_seconds, 900);
    assert_eq!(row.hold_fallback.as_deref(), Some("openai/gpt-image-1"));
    assert_eq!(
        row.capabilities_override,
        Some(serde_json::json!({"notes": ["hand-set"]}))
    );

    // And it reaches the snapshot the runtime derives its descriptors from.
    let snap = lmgw_core::store::load_snapshot(&pool).await.unwrap();
    assert_eq!(snap.image_models.len(), 2);
    assert!(snap
        .image_models
        .iter()
        .any(|m| m.model_id == "z-image-turbo"));

    let mut updated = new;
    updated.enabled = false;
    updated.modes = vec!["img_gen".into(), "vid_gen".into()];
    lmgw_core::store::update_image_model(&pool, id, &updated)
        .await
        .unwrap();
    let row = lmgw_core::store::get_image_model(&pool, id)
        .await
        .unwrap()
        .unwrap();
    assert!(!row.enabled);
    assert_eq!(
        row.modes,
        vec!["img_gen".to_string(), "vid_gen".to_string()]
    );

    lmgw_core::store::delete_image_model(&pool, id)
        .await
        .unwrap();
    assert!(lmgw_core::store::get_image_model(&pool, id)
        .await
        .unwrap()
        .is_none());
}

/// A settings blob written before the image class existed still loads, and
/// the section it gains carries the class defaults — including the sd.cpp
/// image and the `image` prefix — which then round-trip through a save.
#[tokio::test]
async fn the_settings_blob_round_trips_through_the_new_image_section() {
    let pool = lmgw_core::store::open_in_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES ('settings', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(r#"{"bind_addr":"127.0.0.1:8001","audio":{"models_dir":"/srv/audio"}}"#)
    .execute(&pool)
    .await
    .unwrap();

    let loaded = lmgw_core::store::load_settings(&pool).await.unwrap();
    assert_eq!(loaded.audio.models_dir, "/srv/audio");
    assert_eq!(
        loaded.image.image,
        "ghcr.io/leejet/stable-diffusion.cpp:master-cuda"
    );
    assert_eq!(loaded.image.public_prefix, "image");
    assert_eq!(loaded.image.models_dir, "", "no path is invented");
    assert!(loaded
        .image
        .extra_run_args
        .iter()
        .any(|a| a == "nvidia.com/gpu=all"));

    let mut settings = loaded;
    settings.image.models_dir = "/srv/sdcpp".into();
    settings.image.public_prefix = "img".into();
    lmgw_core::store::save_settings(&pool, &settings)
        .await
        .unwrap();

    let reloaded = lmgw_core::store::load_settings(&pool).await.unwrap();
    assert_eq!(reloaded.image.models_dir, "/srv/sdcpp");
    assert_eq!(reloaded.image.public_prefix, "img");
    assert_eq!(reloaded.audio.models_dir, "/srv/audio");
}

/// Migration 0035 on an install that already has image rows: the learned-peak
/// columns arrive NULL, the rows keep everything else, and an upgraded
/// `image_models` has exactly the shape a fresh one does.
///
/// The last part is the one worth pinning. `peak_extra_bytes` is written by
/// the sampler and read by the ledger, so a column that exists only on fresh
/// installs would leave every *upgraded* gateway charging nothing for its
/// image pipelines — silently, because NULL is also the honest value for a row
/// that has never generated.
#[tokio::test]
async fn migration_0035_adds_the_learned_peak_columns_as_null() {
    let pool = db_at_version(34).await;
    sqlx::query(
        "INSERT INTO image_models (model_id, files, idle_seconds) \
         VALUES ('z-image-turbo', '{\"diffusion_model\":\"z.gguf\"}', 900)",
    )
    .execute(&pool)
    .await
    .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let rows = lmgw_core::store::list_image_models(&pool).await.unwrap();
    let row = rows.iter().find(|m| m.model_id == "z-image-turbo").unwrap();
    assert_eq!(row.idle_seconds, 900, "the pre-existing row is untouched");
    assert_eq!(
        row.files.get("diffusion_model").and_then(|v| v.as_str()),
        Some("z.gguf")
    );
    assert_eq!(
        row.peak_extra_bytes, None,
        "nothing has been measured on this box, so nothing is claimed"
    );
    assert_eq!(row.peak_learned_at, None);

    let columns = |pool: SqlitePool| async move {
        let mut names: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('image_models')")
                .fetch_all(&pool)
                .await
                .unwrap();
        names.sort();
        names
    };
    let upgraded = columns(pool).await;
    let fresh = columns(lmgw_core::store::open_in_memory().await.unwrap()).await;
    assert_eq!(
        upgraded, fresh,
        "an upgraded install and a fresh one have the same columns"
    );
    assert!(upgraded.iter().any(|c| c == "peak_extra_bytes"));
    assert!(upgraded.iter().any(|c| c == "peak_learned_at"));
}

/// The learned peak is written by its own statement and read back with the
/// row — and an owner saving the row does not erase a measurement, because
/// `update_image_model` does not name the column.
#[tokio::test]
async fn a_learned_peak_is_stored_beside_the_row_without_being_part_of_it() {
    let pool = lmgw_core::store::open_in_memory().await.unwrap();
    let new = lmgw_core::store::NewImageModel {
        model_id: "z-image-turbo".into(),
        files: serde_json::json!({"diffusion_model": "z.gguf"})
            .as_object()
            .cloned()
            .unwrap(),
        enabled: true,
        idle_seconds: 300,
        ..Default::default()
    };
    let id = lmgw_core::store::insert_image_model(&pool, &new)
        .await
        .unwrap();
    let peak = 6_400_000_000u64;
    lmgw_core::store::set_image_model_peak(&pool, id, Some(peak))
        .await
        .unwrap();

    let row = lmgw_core::store::get_image_model(&pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.peak_extra_bytes, Some(peak));
    assert!(
        row.peak_learned_at.is_some_and(|t| t.len() >= 19),
        "a figure without a date cannot be judged for staleness"
    );

    // An ordinary save of the row leaves the measurement alone: the two write
    // paths are separate on purpose (the owner owns the configuration, the
    // sampler owns this one number).
    let mut updated = new;
    updated.idle_seconds = 60;
    lmgw_core::store::update_image_model(&pool, id, &updated)
        .await
        .unwrap();
    let row = lmgw_core::store::get_image_model(&pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.idle_seconds, 60);
    assert_eq!(row.peak_extra_bytes, Some(peak));

    // And clearing it clears the date with it — a learned_at without a figure
    // would read as "measured 0".
    lmgw_core::store::set_image_model_peak(&pool, id, None)
        .await
        .unwrap();
    let row = lmgw_core::store::get_image_model(&pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.peak_extra_bytes, None);
    assert_eq!(row.peak_learned_at, None);
}

/// 0037 rebuilds `api_keys` again, to let `kind` be `owner` (principals §6).
///
/// A rebuild is a copy, and this one copies a table that — unlike the one 0032
/// rebuilt — already holds credentials in `key_plain` and agent links in
/// `agent_id`. 0032's copy listed neither column, because it was adding them;
/// repeating that list here would silently drop every agent token's plaintext,
/// which is the value a container is handed on its *second* run. That is the
/// regression this test exists for.
#[tokio::test]
async fn migration_0037_keeps_every_agent_tokens_plaintext_and_its_agent_link() {
    let pool = db_at_version(36).await;

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
    sqlx::query("INSERT INTO api_keys (id, name, key_hash) VALUES (43, 'laptop', 'hash-43')")
        .execute(&pool)
        .await
        .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let (name, plain, agent, kind): (String, Option<String>, Option<String>, String) =
        sqlx::query_as("SELECT name, key_plain, agent_id, kind FROM api_keys WHERE id = 42")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(name, "agent:labeler");
    assert_eq!(
        plain.as_deref(),
        Some("lmgw-agent-deadbeef"),
        "the token a running container is holding survived the rebuild"
    );
    assert_eq!(agent.as_deref(), Some("labeler"));
    assert_eq!(kind, "agent");

    // The client key beside it is untouched, and still carries no plaintext.
    let plain: Option<String> = sqlx::query_scalar("SELECT key_plain FROM api_keys WHERE id = 43")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(plain, None);

    // The widened CHECK: an owner row keeps a plaintext and has no agent.
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, key_plain, kind)
         VALUES ('owner:dashboard', 'h', 'lmgw-owner-cafe', 'owner')",
    )
    .execute(&pool)
    .await
    .expect("kind='owner' is legal now");
    for (why, sql) in [
        (
            "an owner row without a plaintext",
            "INSERT INTO api_keys (name, key_hash, kind) VALUES ('owner:x', 'h', 'owner')",
        ),
        (
            "an owner row with an agent id",
            "INSERT INTO api_keys (name, key_hash, key_plain, kind, agent_id)
             VALUES ('owner:y', 'h', 'p', 'owner', 'labeler')",
        ),
        (
            "an agent row without a plaintext",
            "INSERT INTO api_keys (name, key_hash, kind, agent_id)
             VALUES ('agent:bad', 'h', 'agent', 'bad')",
        ),
        (
            "a client row carrying a plaintext",
            "INSERT INTO api_keys (name, key_hash, key_plain, kind)
             VALUES ('leaky', 'h', 'p', 'key')",
        ),
    ] {
        assert!(
            sqlx::query(sql).execute(&pool).await.is_err(),
            "{why} must be refused"
        );
    }

    // The partial unique index is recreated with the table — `upsert_agent_key`
    // names it in an `ON CONFLICT (agent_id)`, so losing it would turn every
    // token rotation into a second row for the same agent.
    assert!(
        sqlx::query(
            "INSERT INTO api_keys (name, key_hash, key_plain, kind, agent_id)
             VALUES ('agent:labeler-2', 'h', 'p', 'agent', 'labeler')"
        )
        .execute(&pool)
        .await
        .is_err(),
        "one token per agent"
    );

    // And the watermark, for 0032's reason: `request_logs.key_id` is a soft
    // link, so a re-used id inherits a deleted key's spend and history.
    sqlx::query("DELETE FROM api_keys WHERE id = 43")
        .execute(&pool)
        .await
        .unwrap();
    let fresh: i64 = sqlx::query_scalar(
        "INSERT INTO api_keys (name, key_hash) VALUES ('k-new', 'h') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(fresh > 43, "a deleted id must never be handed out again");
}

/// Migration 0043 (ladder design §4.1, §6): `local_models.ladder` defaults to
/// an empty JSON array — "not a ladder" — on a row inserted before the
/// column existed, and `request_logs.rung` is nullable rather than
/// backfilled.
#[tokio::test]
async fn migration_0043_adds_ladder_and_rung_with_the_right_defaults() {
    let pool = lmgw_core::store::open_in_memory().await.unwrap();
    let versions = applied_versions(&pool).await;
    assert!(versions.contains(&43), "{versions:?}");

    sqlx::query(
        "INSERT INTO local_models (model_id, gguf_path, params, args, idle_seconds, enabled, \
         public, warm_start, hold_fallback_mode)
         VALUES ('m', 'm.gguf', '{}', '[]', 300, 1, 1, 0, 'inherit')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let ladder: String = sqlx::query_scalar("SELECT ladder FROM local_models WHERE model_id='m'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ladder, "[]");

    sqlx::query(
        "INSERT INTO request_logs (ts, ingress_proto, requested_alias, status)
         VALUES (datetime('now'), 'openai', 'm', 200)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let rung: Option<i64> =
        sqlx::query_scalar("SELECT rung FROM request_logs WHERE requested_alias='m'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rung, None);
}

/// Migration 0044 (candidate-aliases design §4.1): the new table's defaults
/// leave a minimal insert as "not usable yet" rather than failing outright —
/// an empty candidate list and an empty enabled-facets set, `fallback_mode`
/// defaulting to `inherit`, and `alias` unique like every other name column
/// this phase's uniqueness rule reads.
#[tokio::test]
async fn migration_0044_adds_candidate_aliases_with_the_right_defaults() {
    let pool = lmgw_core::store::open_in_memory().await.unwrap();
    let versions = applied_versions(&pool).await;
    assert!(versions.contains(&44), "{versions:?}");

    sqlx::query("INSERT INTO candidate_aliases (alias) VALUES ('smart')")
        .execute(&pool)
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT candidates, background, fallback_mode, fallback, capabilities_disabled, \
         capabilities_enabled, enabled, notes FROM candidate_aliases WHERE alias='smart'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("candidates"), "[]");
    assert_eq!(row.get::<i64, _>("background"), 0);
    assert_eq!(row.get::<String, _>("fallback_mode"), "inherit");
    assert_eq!(row.get::<Option<String>, _>("fallback"), None);
    assert_eq!(row.get::<String, _>("capabilities_disabled"), "[]");
    assert_eq!(row.get::<String, _>("capabilities_enabled"), "[]");
    assert_eq!(row.get::<i64, _>("enabled"), 1);
    assert_eq!(row.get::<String, _>("notes"), "");

    assert!(
        sqlx::query("INSERT INTO candidate_aliases (alias) VALUES ('smart')")
            .execute(&pool)
            .await
            .is_err(),
        "alias must stay unique"
    );
}

/// Migration 0053 (realtime design §9.4): the learned audio residency is three
/// new columns, NULL on every row an existing install already has — nothing
/// has been measured on that box yet, so nothing is claimed — and an
/// upgraded install ends with the same columns as a fresh one.
#[tokio::test]
async fn migration_0053_adds_the_audio_residency_columns_as_null() {
    let pool = db_at_version(52).await;
    sqlx::query(
        "INSERT INTO audio_models (model_id, family, path, task, mode, enabled) \
         VALUES ('pocket', 'pocket_tts', 'pocket', 'tts', 'offline', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let rows = lmgw_core::store::list_audio_models(&pool).await.unwrap();
    let row = rows.iter().find(|m| m.model_id == "pocket").unwrap();
    assert_eq!(
        row.family, "pocket_tts",
        "the pre-existing row is untouched"
    );
    assert_eq!(row.residency, None, "nothing learned on this box yet");

    let columns = |pool: SqlitePool| async move {
        let mut names: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('audio_models')")
                .fetch_all(&pool)
                .await
                .unwrap();
        names.sort();
        names
    };
    let upgraded = columns(pool.clone()).await;
    let fresh = columns(lmgw_core::store::open_in_memory().await.unwrap()).await;
    assert_eq!(upgraded, fresh);
    for c in ["resident_bytes", "resident_learned_at", "resident_key"] {
        assert!(upgraded.iter().any(|u| u == c), "{c} in {upgraded:?}");
    }

    // Stored by its own statement, read back with the row, untouched by the
    // owner's save, and cleared by the owner's reset.
    lmgw_core::store::set_audio_model_residency(&pool, row.id, Some((3 << 30, "0123456789ab")))
        .await
        .unwrap();
    let get = |pool: SqlitePool, id: i64| async move {
        lmgw_core::store::get_audio_model(&pool, id)
            .await
            .unwrap()
            .unwrap()
    };
    let learned = get(pool.clone(), row.id).await.residency.unwrap();
    assert_eq!(learned.bytes, 3 << 30);
    assert_eq!(learned.key, "0123456789ab");
    assert!(!learned.learned_at.is_empty());
    let r = get(pool.clone(), row.id).await;
    let save = lmgw_core::store::NewAudioModel {
        model_id: r.model_id.clone(),
        family: r.family.clone(),
        path: r.path.clone(),
        task: r.task.clone(),
        mode: r.mode.clone(),
        lazy: r.lazy,
        busy_timeout_ms: r.busy_timeout_ms,
        backend: None,
        threads: None,
        load_options: r.load_options.clone(),
        session_options: r.session_options.clone(),
        default_request_options: r.default_request_options.clone(),
        model_spec_override: r.model_spec_override.clone(),
        config_id: r.config_id.clone(),
        weight_id: r.weight_id.clone(),
        voice_presets: r.voice_presets.clone(),
        default_voice_preset: r.default_voice_preset.clone(),
        enabled: r.enabled,
        image: r.image.clone(),
        extra_run_args: r.extra_run_args.clone(),
        warm_start: true,
        hold_fallback_mode: r.hold_fallback_mode,
        hold_fallback: r.hold_fallback.clone(),
    };
    lmgw_core::store::update_audio_model(&pool, row.id, &save)
        .await
        .unwrap();
    assert_eq!(get(pool.clone(), row.id).await.residency, Some(learned));
    lmgw_core::store::set_audio_model_residency(&pool, row.id, None)
        .await
        .unwrap();
    assert_eq!(get(pool.clone(), row.id).await.residency, None);
}

/// Migration 0055: a per-model `extra_run_args` of `[]` — saved with the
/// field left blank, and meaning "run with no extra args", which dropped the
/// class's GPU and SELinux flags — becomes NULL, the class's run args, in all
/// four model tables. A row with args of its own and a NULL row are left as
/// they are, and so is `mcp_servers` (not a per-model override).
#[tokio::test]
async fn migration_0055_turns_an_empty_run_args_override_into_inherit() {
    let pool = db_at_version(54).await;
    let rows = [
        "INSERT INTO local_models (model_id, gguf_path, extra_run_args) \
         VALUES ('chat-empty', 'a.gguf', '[]'), ('chat-own', 'a.gguf', '[\"--cpus\",\"2\"]'), \
         ('chat-null', 'a.gguf', NULL)",
        "INSERT INTO aux_models (model_id, gguf_path, extra_run_args) \
         VALUES ('aux-empty', 'e.gguf', '[]'), ('aux-own', 'e.gguf', '[\"--cpus\",\"2\"]')",
        "INSERT INTO audio_models (model_id, family, path, task, mode, enabled, extra_run_args) \
         VALUES ('audio-empty', 'f', 'p', 'asr', 'offline', 1, '[]'), \
         ('audio-own', 'f', 'p', 'asr', 'offline', 1, '[\"--cpus\",\"2\"]')",
        "INSERT INTO image_models (model_id, extra_run_args) \
         VALUES ('img-empty', '[]'), ('img-own', '[\"--cpus\",\"2\"]')",
        "INSERT INTO mcp_servers (name, transport, extra_run_args) VALUES ('m', 'stdio', '[]')",
    ];
    for sql in rows {
        sqlx::query(sql).execute(&pool).await.unwrap();
    }

    // The upgrade names every row it moves, once: the SQL cannot.
    let (log, capturing) = crate::common::captured_log::capture_log();
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    drop(capturing);
    let log = log.text();
    for (class, id) in [
        ("chat", "chat-empty"),
        ("aux", "aux-empty"),
        ("audio", "audio-empty"),
        ("image", "img-empty"),
    ] {
        let line = format!("migration 0055: {class} model '{id}' had an empty run-args override");
        assert_eq!(log.matches(&line).count(), 1, "{log}");
    }
    assert!(
        !log.contains("-own'") && !log.contains("chat-null"),
        "{log}"
    );
    assert!(log.contains("names at least one flag of its own"), "{log}");

    let own = Some("[\"--cpus\",\"2\"]".to_string());
    for (table, prefix) in [
        ("local_models", "chat"),
        ("aux_models", "aux"),
        ("audio_models", "audio"),
        ("image_models", "img"),
    ] {
        let empty = run_args_of(&pool, table, &format!("{prefix}-empty")).await;
        assert_eq!(empty, None, "{table}");
        let kept = run_args_of(&pool, table, &format!("{prefix}-own")).await;
        assert_eq!(kept, own, "{table}");
    }
    assert_eq!(run_args_of(&pool, "local_models", "chat-null").await, None);
    assert_eq!(
        run_args_of(&pool, "mcp_servers", "m").await.as_deref(),
        Some("[]"),
        "not a per-model override"
    );
    // Read back through the store: inherit, the class's run args.
    let audio = lmgw_core::store::list_audio_models(&pool).await.unwrap();
    let row = audio.iter().find(|m| m.model_id == "audio-empty").unwrap();
    assert_eq!(row.extra_run_args, None);
}

/// The stored `extra_run_args` of the row named `id` in `table`.
async fn run_args_of(pool: &SqlitePool, table: &str, id: &str) -> Option<String> {
    let sql = match table {
        "local_models" => "SELECT extra_run_args FROM local_models WHERE model_id = ?1",
        "aux_models" => "SELECT extra_run_args FROM aux_models WHERE model_id = ?1",
        "audio_models" => "SELECT extra_run_args FROM audio_models WHERE model_id = ?1",
        "image_models" => "SELECT extra_run_args FROM image_models WHERE model_id = ?1",
        "mcp_servers" => "SELECT extra_run_args FROM mcp_servers WHERE name = ?1",
        other => panic!("no run args column in {other}"),
    };
    sqlx::query_scalar::<_, Option<String>>(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Migration 0056: every audio row an install already has reads `backend`
/// and `threads` as `None` — inherit the class, so it renders the
/// `server.json` it rendered before — and a row written with both reads them
/// back.
#[tokio::test]
async fn migration_0056_leaves_every_audio_row_on_the_class_backend() {
    let pool = db_at_version(55).await;
    sqlx::query(
        "INSERT INTO audio_models (model_id, family, path, task, mode, enabled) \
         VALUES ('asr', 'parakeet', 'p', 'asr', 'offline', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let audio = lmgw_core::store::list_audio_models(&pool).await.unwrap();
    let row = audio.iter().find(|m| m.model_id == "asr").unwrap();
    assert_eq!((row.backend.as_deref(), row.threads), (None, None));
    sqlx::query("UPDATE audio_models SET backend = 'cpu', threads = 8 WHERE model_id = 'asr'")
        .execute(&pool)
        .await
        .unwrap();
    let audio = lmgw_core::store::list_audio_models(&pool).await.unwrap();
    let row = audio.iter().find(|m| m.model_id == "asr").unwrap();
    assert_eq!(
        (row.backend.as_deref(), row.threads),
        (Some("cpu"), Some(8))
    );
}
