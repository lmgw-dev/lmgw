//! Migration 0090: `openai` becomes the OpenAI platform only and every other
//! OpenAI-shaped row `openai_compatible` (openai egress design §4.1,
//! decisions 2, 7, 8 and 21) — every row shape, the CHECKs, ids, aliases,
//! prices and the id high-water mark kept, a crash inside the rebuild and a
//! replay after its commit, the foreign-key check after the migrator, and the
//! start notice line by line.

use lmgw_core::config::{Protocol, UpstreamKind};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{AssertSqlSafe, Connection, Row, SqlitePool};
use std::str::FromStr;

use super::llama_cpp::{file_db_at_version, file_pool};
use super::{applied_versions, db_at_version};

/// The migration under test, as sqlx runs it.
const MIGRATION: &str = include_str!("../../../migrations/0090_openai_compatible_protocol.sql");

/// One row of every shape a version-89 table can hold: `(name, protocol,
/// kind, base_url, supports_responses)`.
const SHAPES: [(&str, &str, &str, &str, i64); 11] = [
    (
        "platform",
        "openai",
        "generic",
        "https://api.openai.com/v1",
        1,
    ),
    (
        "regional",
        "openai",
        "generic",
        "https://eu.api.openai.com/v1",
        0,
    ),
    (
        "aggregator",
        "openai",
        "generic",
        "https://router.example/api/v1",
        1,
    ),
    (
        "audio",
        "openai",
        "audio_cpp",
        "http://192.0.2.10:8080/v1",
        0,
    ),
    (
        "claude",
        "anthropic",
        "generic",
        "https://api.anthropic.example",
        1,
    ),
    (
        "claude-plain",
        "anthropic",
        "generic",
        "https://api.anthropic.example",
        0,
    ),
    (
        "llama-messages",
        "anthropic",
        "llama_server",
        "http://192.0.2.11:8080",
        1,
    ),
    ("aistudio", "gemini", "generic", "https://gemini.example", 1),
    (
        "aistudio-plain",
        "gemini",
        "generic",
        "https://gemini.example",
        0,
    ),
    (
        "llama",
        "llama_cpp",
        "llama_server",
        "http://192.0.2.12:8080/v1",
        0,
    ),
    (
        "anthropic-audio",
        "anthropic",
        "audio_cpp",
        "http://192.0.2.13:8080",
        0,
    ),
];

/// What 0090 stores for each of [`SHAPES`]: `(name, protocol, kind,
/// supports_responses)`.
const MAPPED: [(&str, &str, &str, i64); 11] = [
    ("platform", "openai_compatible", "generic", 1),
    ("regional", "openai_compatible", "generic", 0),
    ("aggregator", "openai_compatible", "generic", 1),
    ("audio", "openai_compatible", "audio_cpp", 0),
    ("claude", "anthropic", "generic", 0),
    ("claude-plain", "anthropic", "generic", 0),
    ("llama-messages", "anthropic", "llama_server", 0),
    ("aistudio", "gemini", "generic", 0),
    ("aistudio-plain", "gemini", "generic", 0),
    ("llama", "llama_cpp", "llama_server", 0),
    ("anthropic-audio", "anthropic", "generic", 0),
];

/// Every column but the two 0090 may map and `kind` (which [`shapes`]
/// compares), by id — what must survive the rebuild verbatim.
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
    i64,
)>;

/// Insert [`SHAPES`] with distinct values in every other column, an alias and
/// a price on each row, a hidden passthrough model on one, and then delete
/// one more row with the highest id — so the sequence is above `max(id)`.
async fn seed(pool: &SqlitePool) {
    for (i, (name, protocol, kind, base_url, responses)) in SHAPES.iter().enumerate() {
        sqlx::query(
            "INSERT INTO upstreams (name, protocol, kind, base_url, api_key, extra_headers,
                 timeout_ms, enabled, expose_all, expose_prefix, created_at, updated_at,
                 supports_responses, catalog_all_modalities)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        )
        .bind(name)
        .bind(protocol)
        .bind(kind)
        .bind(base_url)
        .bind((i % 2 == 0).then(|| format!("sk-{i}")))
        .bind(format!("[[\"x-n\",\"{i}\"]]"))
        .bind(1000 + i as i64)
        .bind((i % 3 != 0) as i64)
        .bind((i % 2) as i64)
        .bind(format!("p{i}"))
        .bind(format!("2026-01-0{} 10:00:00", 1 + i % 9))
        .bind(format!("2026-02-0{} 11:00:00", 1 + i % 9))
        .bind(responses)
        .bind((i % 4 != 1) as i64)
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
        sqlx::query(
            "INSERT INTO prices (scope_kind, scope_key, price_in, price_out)
             VALUES ('upstream_model', (SELECT id FROM upstreams WHERE name = ?1) || ':m', ?2, ?3)",
        )
        .bind(name)
        .bind(1.0 + i as f64)
        .bind(2.0 + i as f64)
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO hidden_passthrough_models (upstream_id, model_id)
         VALUES ((SELECT id FROM upstreams WHERE name = 'aggregator'), 'hidden')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO upstreams (name, protocol, kind, base_url)
         VALUES ('top', 'openai', 'generic', 'x')",
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
        "SELECT id, name, base_url, api_key, extra_headers, timeout_ms, enabled,
                expose_all, expose_prefix, created_at, updated_at, catalog_all_modalities
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
            r.get(11),
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

/// `(alias, upstream name)` of every alias, `(upstream name, model id)` of
/// every hidden passthrough model, and `(scope_key, upstream name)` of every
/// price.
type Refs = (
    Vec<(String, String)>,
    Vec<(String, String)>,
    Vec<(String, String)>,
);

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
    let prices = sqlx::query_as(
        "SELECT p.scope_key, u.name FROM prices p
         JOIN upstreams u ON p.scope_key = u.id || ':m' ORDER BY p.scope_key",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    (aliases, hidden, prices)
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

/// The whole outcome of 0090 on a seeded database, against what the seed
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
        "aliases, hidden models and prices kept"
    );
    assert_eq!(sequence(pool).await, seq, "the id high-water mark kept");
    assert!(applied_versions(pool).await.contains(&90));
    let leftover: Option<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE name = 'upstreams_new'")
            .fetch_optional(pool)
            .await
            .unwrap();
    assert_eq!(leftover, None);
}

/// Every row shape a version-89 table can hold, mapped as decisions 2 and 8
/// say; every other column, id, alias, price and hidden model kept; a row
/// inserted afterwards does not take the id of the row deleted at the top;
/// and the start names every row it changed, once.
#[tokio::test]
async fn migration_0090_maps_every_row_shape_and_keeps_everything_else() {
    let pool = db_at_version(89).await;
    seed(&pool).await;
    let (before, refs, seq) = (
        kept(&pool).await,
        references(&pool).await,
        sequence(&pool).await,
    );
    assert_eq!(seq, Some(12), "eleven rows and the deleted top one");

    let (log, capturing) = crate::common::captured_log::capture_log();
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    drop(capturing);
    assert_migrated(&pool, &before, &refs, seq).await;

    // The store reads the new spelling back.
    let ups = lmgw_core::store::list_upstreams(&pool).await.unwrap();
    let audio = ups.iter().find(|u| u.name == "audio").unwrap();
    assert_eq!(
        (audio.protocol, audio.kind),
        (Protocol::OpenaiCompatible, UpstreamKind::AudioCpp)
    );
    let aggregator = ups.iter().find(|u| u.name == "aggregator").unwrap();
    assert!(aggregator.forwards_responses(), "its flag stands");

    // Insert after deleting the top id: the next id is above it.
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO upstreams (name, protocol, kind, base_url)
         VALUES ('new', 'openai', 'generic', 'https://api.openai.com/v1') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(id, 13, "the deleted top id 12 is never handed out again");

    // The start that ran 0090 named every row it changed, once; the second
    // start had nothing left to say.
    let log = log.text();
    for (name, what) in [
        ("platform", "now spelled openai_compatible"),
        ("platform", "looks like the OpenAI platform"),
        ("regional", "now spelled openai_compatible"),
        ("regional", "looks like the OpenAI platform"),
        ("aggregator", "now spelled openai_compatible"),
        ("audio", "now spelled openai_compatible"),
        ("claude", "had native /v1/responses on"),
        ("llama-messages", "had native /v1/responses on"),
        ("aistudio", "had native /v1/responses on"),
        ("anthropic-audio", "kind audio_cpp"),
    ] {
        let line = format!("migration 0090: upstream '{name}' (id ");
        let named = log
            .lines()
            .filter(|l| l.contains(&line) && l.contains(what))
            .count();
        assert_eq!(named, 1, "{name}: {what}\n{log}");
    }
    for name in ["aggregator", "audio"] {
        let line = format!("migration 0090: upstream '{name}' (id ");
        assert!(
            !log.lines()
                .any(|l| l.contains(&line) && l.contains("looks like the OpenAI platform")),
            "{name} is not on OpenAI's host\n{log}"
        );
    }
    for unchanged in ["'claude-plain'", "'aistudio-plain'", "'llama'"] {
        assert!(
            !log.lines()
                .any(|l| l.contains("migration 0090") && l.contains(unchanged)),
            "{unchanged} is not changed and not named\n{log}"
        );
    }
}

/// A table whose every row was deleted keeps its high-water mark too.
#[tokio::test]
async fn migration_0090_keeps_the_high_water_mark_of_an_emptied_table() {
    let pool = db_at_version(89).await;
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
        "INSERT INTO upstreams (name, protocol, base_url)
         VALUES ('d', 'openai_compatible', 'x') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(id, 4);
}

/// The CHECKs: `openai` is always generic, native `/v1/responses` stands on
/// `openai_compatible` only, llama_cpp is always a llama-server, and a
/// llama-server speaks llama_cpp or anthropic — on insert and on update.
#[tokio::test]
async fn migration_0090_checks_refuse_a_shape_the_rules_do_not_allow() {
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
        // decision 7
        ("openai", "audio_cpp", 0),
        ("openai", "llama_server", 0),
        ("anthropic", "audio_cpp", 0),
        ("gemini", "audio_cpp", 0),
        // decision 8
        ("openai", "generic", 1),
        ("anthropic", "generic", 1),
        ("anthropic", "llama_server", 1),
        ("gemini", "generic", 1),
        ("llama_cpp", "llama_server", 1),
        // 0058's, kept
        ("openai_compatible", "llama_server", 0),
        ("gemini", "llama_server", 0),
        ("llama_cpp", "generic", 0),
        ("llama_cpp", "audio_cpp", 0),
        // spellings that do not exist
        ("openaicompatible", "generic", 0),
        ("openai_compatible", "sd_cpp", 0),
    ] {
        let e = insert(p, k, r).await.unwrap_err().to_string();
        assert!(e.contains("CHECK constraint failed"), "{p}+{k}+{r}: {e}");
    }
    for (p, k, r) in [
        ("openai", "generic", 0),
        ("openai_compatible", "generic", 1),
        ("openai_compatible", "generic", 0),
        ("openai_compatible", "audio_cpp", 0),
        ("anthropic", "llama_server", 0),
        ("anthropic", "generic", 0),
        ("gemini", "generic", 0),
        ("llama_cpp", "llama_server", 0),
    ] {
        insert(p, k, r)
            .await
            .unwrap_or_else(|e| panic!("{p}+{k}+{r}: {e}"));
    }
    for (set, row) in [
        ("protocol = 'openai'", "openai_compatible-audio_cpp-0"),
        ("protocol = 'openai'", "openai_compatible-generic-1"),
        ("kind = 'audio_cpp'", "openai-generic-0"),
        ("kind = 'audio_cpp'", "anthropic-generic-0"),
        ("supports_responses = 1", "openai-generic-0"),
        ("supports_responses = 1", "anthropic-generic-0"),
        ("protocol = 'gemini'", "openai_compatible-generic-1"),
    ] {
        let sql = format!("UPDATE upstreams SET {set} WHERE name = '{row}'");
        let e = sqlx::query(AssertSqlSafe(sql))
            .execute(&pool)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("CHECK constraint failed"), "{set} on {row}: {e}");
    }
}

/// A crash inside the rebuild — after the old table was dropped, before the
/// new one took its name — leaves the old table: the rebuild is one
/// transaction, and an uncommitted one is rolled back. The next start runs
/// 0090 from the top.
#[tokio::test]
async fn a_crash_inside_the_0090_rebuild_leaves_the_old_table_whole() {
    let dir = std::env::temp_dir().join(format!("lmgw-0090-crash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("lmgw.sqlite");

    let pool = file_db_at_version(&path, 89).await;
    seed(&pool).await;
    let (before, refs, seq) = (
        kept(&pool).await,
        references(&pool).await,
        sequence(&pool).await,
    );
    let old_shapes = shapes(&pool).await;
    pool.close().await;

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
    assert!(!applied_versions(&pool).await.contains(&90));

    lmgw_core::store::run_migrations(&pool).await.unwrap();
    assert_migrated(&pool, &before, &refs, seq).await;
    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A crash after the rebuild's COMMIT but before sqlx recorded 0090 replays
/// the whole file against the new table on the next start: the `CASE`s map
/// nothing twice, and the ids, aliases, prices and high-water mark survive
/// the second rebuild too.
#[tokio::test]
async fn a_replay_of_0090_after_its_commit_maps_nothing_twice() {
    let pool = db_at_version(89).await;
    seed(&pool).await;
    let (before, refs, seq) = (
        kept(&pool).await,
        references(&pool).await,
        sequence(&pool).await,
    );

    sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();
    assert!(!applied_versions(&pool).await.contains(&90));
    assert_eq!(shapes(&pool).await, expected());

    let (log, capturing) = crate::common::captured_log::capture_log();
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    drop(capturing);
    assert_migrated(&pool, &before, &refs, seq).await;
    assert!(
        !log.text().contains("migration 0090"),
        "a replay has nothing left to name\n{}",
        log.text()
    );
}

/// The rebuild runs with foreign keys off, so the start that applies it
/// checks them after: a reference left pointing at nothing refuses the start
/// and names the row.
#[tokio::test]
async fn a_dangling_reference_across_0090_refuses_the_start() {
    let pool = db_at_version(89).await;
    sqlx::query("INSERT INTO upstreams (name, protocol, base_url) VALUES ('up', 'openai', 'x')")
        .execute(&pool)
        .await
        .unwrap();
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
    assert!(applied_versions(&pool).await.contains(&90));
}

/// A database from before 0023 still holds the managed `llama-aux` and
/// `audiocpp` rows, which 0023 deletes in the same start: the 0090 notice
/// names the owner's own row and not those; and a row 0058 respells as
/// `llama_cpp` in that start is 0058's to name.
#[tokio::test]
async fn the_notice_leaves_out_rows_an_earlier_migration_of_the_same_start_takes() {
    let pool = db_at_version(22).await;
    for (name, kind) in [
        ("llama-aux", "generic"),
        ("audiocpp", "generic"),
        ("mine", "generic"),
        ("my-llama", "llama_server"),
    ] {
        sqlx::query(
            "INSERT INTO upstreams (name, protocol, kind, base_url)
             VALUES (?1, 'openai', ?2, 'http://192.0.2.20:9/v1')",
        )
        .bind(name)
        .bind(kind)
        .execute(&pool)
        .await
        .unwrap();
    }
    let (log, capturing) = crate::common::captured_log::capture_log();
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    drop(capturing);
    let text = log.text();
    assert!(text.contains("migration 0090: upstream 'mine'"), "{text}");
    for other in ["llama-aux", "audiocpp", "my-llama"] {
        assert!(
            !text.contains(&format!("migration 0090: upstream '{other}'")),
            "{text}"
        );
    }
}
