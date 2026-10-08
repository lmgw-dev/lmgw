//! Migration 0058: `upstreams.protocol` gains `llama_cpp`, and protocol and
//! kind can no longer disagree (llama.cpp egress design §5, decisions 11, 12
//! and 19) — every row shape, the CHECKs, ids, aliases and the id high-water
//! mark kept, a crash inside the rebuild and a replay after its commit, the
//! foreign-key check after the migrator, and the start notice.

use std::str::FromStr;

use lmgw_core::config::{Protocol, UpstreamKind};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{AssertSqlSafe, Connection, Row, SqlitePool};

use super::{applied_versions, db_at_version};

/// The migration under test, as sqlx runs it.
const MIGRATION: &str = include_str!("../../../migrations/0058_llama_cpp_protocol.sql");

/// One row of every shape a version-57 table can hold:
/// `(name, protocol, kind, supports_responses)`.
const SHAPES: [(&str, &str, &str, i64); 9] = [
    ("llama", "openai", "llama_server", 0),
    ("llama-responses", "openai", "llama_server", 1),
    ("gemini-llama", "gemini", "llama_server", 1),
    ("anthropic-llama", "anthropic", "llama_server", 1),
    ("cloud", "openai", "generic", 1),
    ("claude", "anthropic", "generic", 0),
    ("gemini", "gemini", "generic", 0),
    ("audio", "openai", "audio_cpp", 0),
    ("anthropic-audio", "anthropic", "audio_cpp", 0),
];

/// What 0058 stores for each of [`SHAPES`]: `(name, protocol, kind,
/// supports_responses)`.
const MAPPED: [(&str, &str, &str, i64); 9] = [
    ("llama", "llama_cpp", "llama_server", 0),
    ("llama-responses", "llama_cpp", "llama_server", 0),
    ("gemini-llama", "gemini", "generic", 1),
    ("anthropic-llama", "anthropic", "llama_server", 1),
    ("cloud", "openai", "generic", 1),
    ("claude", "anthropic", "generic", 0),
    ("gemini", "gemini", "generic", 0),
    ("audio", "openai", "audio_cpp", 0),
    ("anthropic-audio", "anthropic", "audio_cpp", 0),
];

/// Every column but the three 0058 may map, by id — what must survive the
/// rebuild verbatim.
type Kept = Vec<(
    i64,
    String,
    String,
    Option<String>,
    String,
    i64,
    i64,
    i64,
    String,
    String,
    String,
)>;

/// Insert [`SHAPES`] with distinct values in every other column, an alias on
/// each row, a hidden passthrough model on one, and then delete one more
/// row with the highest id — so the sequence is above `max(id)`.
async fn seed(pool: &SqlitePool) {
    for (i, (name, protocol, kind, responses)) in SHAPES.iter().enumerate() {
        sqlx::query(
            "INSERT INTO upstreams (name, protocol, kind, base_url, api_key, extra_headers,
                 timeout_ms, enabled, expose_all, expose_prefix, created_at, updated_at,
                 supports_responses)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )
        .bind(name)
        .bind(protocol)
        .bind(kind)
        .bind(format!("http://127.0.0.1:{}/v1", 9000 + i))
        .bind((i % 2 == 0).then(|| format!("sk-{i}")))
        .bind(format!("[[\"x-n\",\"{i}\"]]"))
        .bind(1000 + i as i64)
        .bind((i % 3 != 0) as i64)
        .bind((i % 2) as i64)
        .bind(format!("p{i}"))
        .bind(format!("2026-01-0{} 10:00:00", 1 + i % 9))
        .bind(format!("2026-02-0{} 11:00:00", 1 + i % 9))
        .bind(responses)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO models (alias, upstream_id, upstream_model_id)
             VALUES (?1, (SELECT id FROM upstreams WHERE name = ?2), 'm')",
        )
        .bind(format!("alias-{name}"))
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO hidden_passthrough_models (upstream_id, model_id)
         VALUES ((SELECT id FROM upstreams WHERE name = 'llama'), 'hidden')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO upstreams (name, protocol, kind, base_url) VALUES ('top', 'openai', 'generic', 'x')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM upstreams WHERE name = 'top'")
        .execute(pool)
        .await
        .unwrap();
}

async fn kept(pool: &SqlitePool) -> Kept {
    sqlx::query(
        "SELECT id, name, base_url, api_key, extra_headers, timeout_ms, enabled, expose_all,
                expose_prefix, created_at, updated_at
         FROM upstreams ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .iter()
    .map(|r| {
        (
            r.get(0),
            r.get(1),
            r.get(2),
            r.get(3),
            r.get(4),
            r.get(5),
            r.get(6),
            r.get(7),
            r.get(8),
            r.get(9),
            r.get(10),
        )
    })
    .collect()
}

async fn shapes(pool: &SqlitePool) -> Vec<(String, String, String, i64)> {
    sqlx::query_as("SELECT name, protocol, kind, supports_responses FROM upstreams ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// `(alias, upstream name)` of every alias, and `(upstream name, model id)` of
/// every hidden passthrough model.
type Refs = (Vec<(String, String)>, Vec<(String, String)>);

async fn references(pool: &SqlitePool) -> Refs {
    let aliases = sqlx::query_as(
        "SELECT m.alias, u.name FROM models m JOIN upstreams u ON u.id = m.upstream_id
         ORDER BY m.alias",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let hidden = sqlx::query_as(
        "SELECT u.name, h.model_id FROM hidden_passthrough_models h
         JOIN upstreams u ON u.id = h.upstream_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    (aliases, hidden)
}

async fn sequence(pool: &SqlitePool) -> Option<i64> {
    sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name = 'upstreams'")
        .fetch_optional(pool)
        .await
        .unwrap()
}

fn expected() -> Vec<(String, String, String, i64)> {
    MAPPED
        .iter()
        .map(|(n, p, k, r)| (n.to_string(), p.to_string(), k.to_string(), *r))
        .collect()
}

/// The whole outcome of 0058 on a seeded database, against what the seed
/// held before it.
async fn assert_migrated(pool: &SqlitePool, before: &Kept, refs: &Refs, seq: Option<i64>) {
    assert_eq!(shapes(pool).await, expected());
    assert_eq!(
        &kept(pool).await,
        before,
        "every other column and every id kept"
    );
    assert_eq!(
        &references(pool).await,
        refs,
        "aliases and hidden models kept"
    );
    assert_eq!(sequence(pool).await, seq, "the id high-water mark kept");
    assert!(applied_versions(pool).await.contains(&58));
    let leftover: Option<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE name = 'upstreams_new'")
            .fetch_optional(pool)
            .await
            .unwrap();
    assert_eq!(leftover, None);
}

/// Every row shape a version-57 table can hold, mapped as decision 12 says;
/// every other column, id, alias and hidden model kept; and a row inserted
/// afterwards does not take the id of the row deleted at the top.
#[tokio::test]
async fn migration_0058_maps_every_row_shape_and_keeps_everything_else() {
    let pool = db_at_version(57).await;
    seed(&pool).await;
    let (before, refs, seq) = (
        kept(&pool).await,
        references(&pool).await,
        sequence(&pool).await,
    );
    assert_eq!(seq, Some(10), "nine rows and the deleted top one");

    let (log, capturing) = crate::common::captured_log::capture_log();
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    drop(capturing);
    assert_migrated(&pool, &before, &refs, seq).await;

    // The store reads the new spelling back.
    let ups = lmgw_core::store::list_upstreams(&pool).await.unwrap();
    let llama = ups.iter().find(|u| u.name == "llama").unwrap();
    assert_eq!(
        (llama.protocol, llama.kind),
        (Protocol::LlamaCpp, UpstreamKind::LlamaServer)
    );

    // Insert after deleting the top id: the next id is above it.
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO upstreams (name, protocol, kind, base_url)
         VALUES ('new', 'llama_cpp', 'llama_server', 'x') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(id, 11, "the deleted top id 10 is never handed out again");

    // The start that ran 0058 named every row it changed, once; the second
    // start had nothing left to say.
    let log = log.text();
    for (name, what) in [
        ("llama", "now spelled protocol llama_cpp"),
        ("llama-responses", "now spelled protocol llama_cpp"),
        ("llama-responses", "had native /v1/responses on"),
        ("gemini-llama", "becomes kind generic"),
    ] {
        let line = format!("migration 0058: upstream '{name}' (id ");
        let named = log
            .lines()
            .filter(|l| l.contains(&line) && l.contains(what))
            .count();
        assert_eq!(named, 1, "{name}: {what}\n{log}");
    }
    for unchanged in ["anthropic-llama", "cloud", "claude", "'gemini'", "audio"] {
        assert!(
            !log.lines()
                .any(|l| l.contains("migration 0058") && l.contains(unchanged)),
            "{unchanged} is not changed and not named\n{log}"
        );
    }
}

/// A table whose every row was deleted keeps its high-water mark too: the
/// copy creates no sequence row for an empty table.
#[tokio::test]
async fn migration_0058_keeps_the_high_water_mark_of_an_emptied_table() {
    let pool = db_at_version(57).await;
    for name in ["a", "b", "c"] {
        sqlx::query("INSERT INTO upstreams (name, protocol, base_url) VALUES (?1, 'openai', 'x')")
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM upstreams")
        .execute(&pool)
        .await
        .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO upstreams (name, protocol, base_url) VALUES ('d', 'openai', 'x') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(id, 4);
}

/// The CHECKs: llama_cpp is always a llama-server without native
/// `/v1/responses`, and a llama-server speaks llama_cpp or anthropic — on
/// insert and on update.
#[tokio::test]
async fn migration_0058_checks_refuse_a_protocol_and_kind_that_disagree() {
    let pool = lmgw_core::store::open_in_memory().await.unwrap();
    let insert = |p: &'static str, k: &'static str, r: i64| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO upstreams (name, protocol, kind, base_url, supports_responses)
                 VALUES (?1, ?2, ?3, 'x', ?4)",
            )
            .bind(format!("{p}-{k}-{r}"))
            .bind(p)
            .bind(k)
            .bind(r)
            .execute(&pool)
            .await
        }
    };
    for (p, k, r) in [
        ("openai", "llama_server", 0),
        ("gemini", "llama_server", 0),
        ("llama_cpp", "generic", 0),
        ("llama_cpp", "audio_cpp", 0),
        ("llama_cpp", "llama_server", 1),
        ("llama", "llama_server", 0),
        ("openai", "sd_cpp", 0),
    ] {
        let e = insert(p, k, r).await.unwrap_err().to_string();
        assert!(e.contains("CHECK constraint failed"), "{p}+{k}+{r}: {e}");
    }
    for (p, k, r) in [
        ("llama_cpp", "llama_server", 0),
        ("anthropic", "llama_server", 1),
        ("openai", "generic", 1),
        ("gemini", "generic", 0),
        ("openai", "audio_cpp", 0),
    ] {
        insert(p, k, r)
            .await
            .unwrap_or_else(|e| panic!("{p}+{k}+{r}: {e}"));
    }
    for set in [
        "protocol = 'openai'",
        "kind = 'generic'",
        "supports_responses = 1",
    ] {
        let sql = format!("UPDATE upstreams SET {set} WHERE protocol = 'llama_cpp'");
        let e = sqlx::query(AssertSqlSafe(sql))
            .execute(&pool)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("CHECK constraint failed"), "{set}: {e}");
    }
}

/// A file database at `version`, as an install sitting there has it.
async fn file_db_at_version(path: &std::path::Path, version: i64) -> SqlitePool {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(lmgw_core::store::BUSY_TIMEOUT);
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

async fn file_pool(path: &std::path::Path) -> SqlitePool {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .unwrap()
        .foreign_keys(true)
        .busy_timeout(lmgw_core::store::BUSY_TIMEOUT);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap()
}

/// A crash inside the rebuild — after the old table was dropped, before the
/// new one took its name — leaves the old table: the rebuild is one
/// transaction, and an uncommitted one is rolled back. The next start runs
/// 0058 from the top.
#[tokio::test]
async fn a_crash_inside_the_0058_rebuild_leaves_the_old_table_whole() {
    let dir = std::env::temp_dir().join(format!("lmgw-0058-crash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("lmgw.sqlite");

    let pool = file_db_at_version(&path, 57).await;
    seed(&pool).await;
    let (before, refs, seq) = (
        kept(&pool).await,
        references(&pool).await,
        sequence(&pool).await,
    );
    let old_shapes = shapes(&pool).await;
    pool.close().await;

    // Everything up to the rename, on a connection of its own — and then the
    // process is gone: the connection closes without a COMMIT.
    let cut = MIGRATION
        .find("ALTER TABLE upstreams_new RENAME TO upstreams;")
        .unwrap();
    let mut conn = sqlx::SqliteConnection::connect_with(
        &SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql(AssertSqlSafe(MIGRATION[..cut].to_string()))
        .execute(&mut conn)
        .await
        .unwrap();
    let dropped: Option<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE name = 'upstreams'")
            .fetch_optional(&mut conn)
            .await
            .unwrap();
    assert_eq!(dropped, None, "the crash is after the DROP");
    conn.close().await.unwrap();

    let pool = file_pool(&path).await;
    assert_eq!(shapes(&pool).await, old_shapes, "the old table, whole");
    assert_eq!(kept(&pool).await, before);
    assert!(!applied_versions(&pool).await.contains(&58));

    lmgw_core::store::run_migrations(&pool).await.unwrap();
    assert_migrated(&pool, &before, &refs, seq).await;
    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A crash after the rebuild's COMMIT but before sqlx recorded 0058 replays
/// the whole file against the new table on the next start: the `CASE`s map
/// nothing twice, and the ids, aliases and high-water mark survive the second
/// rebuild too.
#[tokio::test]
async fn a_replay_of_0058_after_its_commit_maps_nothing_twice() {
    let pool = db_at_version(57).await;
    seed(&pool).await;
    let (before, refs, seq) = (
        kept(&pool).await,
        references(&pool).await,
        sequence(&pool).await,
    );

    // The file ran and committed; the record of it never landed.
    sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();
    assert!(!applied_versions(&pool).await.contains(&58));
    assert_eq!(shapes(&pool).await, expected());

    let (log, capturing) = crate::common::captured_log::capture_log();
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    drop(capturing);
    assert_migrated(&pool, &before, &refs, seq).await;
    assert!(
        !log.text().contains("migration 0058"),
        "a replay has nothing left to name\n{}",
        log.text()
    );
}

/// A database from before 0023 still holds the managed `llama-aux` and
/// `audiocpp` rows, which 0023 deletes in the same start: the 0058 notice
/// names the owner's own row and not those.
#[tokio::test]
async fn the_notice_leaves_out_the_managed_rows_migration_0023_deletes() {
    let pool = db_at_version(22).await;
    for name in ["llama-aux", "audiocpp", "mine"] {
        sqlx::query(
            "INSERT INTO upstreams (name, protocol, kind, base_url)
             VALUES (?1, 'openai', 'llama_server', 'http://127.0.0.1:9/v1')",
        )
        .bind(name)
        .execute(&pool)
        .await
        .unwrap();
    }
    let (log, capturing) = crate::common::captured_log::capture_log();
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    drop(capturing);
    let text = log.text();
    assert!(text.contains("migration 0058: upstream 'mine'"), "{text}");
    for managed in ["llama-aux", "audiocpp"] {
        assert!(
            !text.contains(&format!("migration 0058: upstream '{managed}'")),
            "{text}"
        );
    }
}

/// The marker that keeps the foreign-key check running until it is clean.
async fn fk_check_pending(pool: &SqlitePool) -> bool {
    lmgw_core::store::get_kv(pool, "store:foreign_key_check_pending")
        .await
        .unwrap()
        .is_some()
}

/// A start that applies a migration (here 0058) refuses when a foreign key
/// points at a missing row, and names the row — the twelve-step rebuild's
/// step 10, in Rust. The migration is committed by then, so a restart has
/// nothing to apply: it refuses again all the same, until the row is fixed.
#[tokio::test]
async fn an_applied_migration_and_a_dangling_reference_refuse_every_start_until_fixed() {
    let pool = db_at_version(57).await;
    sqlx::query("INSERT INTO upstreams (name, protocol, base_url) VALUES ('up', 'openai', 'x')")
        .execute(&pool)
        .await
        .unwrap();
    // Written with the checks off, as a table rebuild runs.
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO models (alias, upstream_id, upstream_model_id) VALUES ('ghost', 99, 'm')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&pool)
        .await
        .unwrap();

    let e = lmgw_core::store::run_migrations(&pool)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("models row 1 → upstreams (missing)"), "{e}");
    assert!(e.contains("deleted nothing"), "{e}");
    assert!(
        e.contains("refuses on every start until they are fixed"),
        "{e}"
    );
    assert!(applied_versions(&pool).await.contains(&58));
    assert!(fk_check_pending(&pool).await);

    // A restart: nothing left to apply, and still refused.
    let again = lmgw_core::store::run_migrations(&pool)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        again.contains("models row 1 → upstreams (missing)"),
        "{again}"
    );
    let ghost: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM models WHERE alias = 'ghost'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ghost, 1, "nothing is deleted");

    // Repointed, the database starts, and the marker is gone.
    sqlx::query("UPDATE models SET upstream_id = (SELECT id FROM upstreams WHERE name = 'up')")
        .execute(&pool)
        .await
        .unwrap();
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    assert!(!fk_check_pending(&pool).await);
}

/// A fully migrated database whose last check was clean carries no marker,
/// and a start that applies nothing does not check: with foreign keys on at
/// runtime only a migration can leave a dangling reference, and an old row no
/// migration touched must not stop the gateway.
#[tokio::test]
async fn a_start_with_nothing_to_apply_never_refuses_on_an_old_dangling_row() {
    let pool = lmgw_core::store::open_in_memory().await.unwrap();
    assert!(
        !fk_check_pending(&pool).await,
        "a clean first start clears it"
    );
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO models (alias, upstream_id, upstream_model_id) VALUES ('ghost', 99, 'm')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&pool)
        .await
        .unwrap();

    lmgw_core::store::run_migrations(&pool)
        .await
        .expect("nothing was applied, so nothing is checked");
    let ghost: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM models WHERE alias = 'ghost'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ghost, 1, "and nothing is deleted either");
    assert!(!fk_check_pending(&pool).await);
}
