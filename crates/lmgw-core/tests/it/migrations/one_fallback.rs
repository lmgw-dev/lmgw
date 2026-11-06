//! Migration 0095: one fallback per model and alias (openai egress design
//! §14.10, decisions 25 and 26). Every old row state — mode `alias` with a
//! name, a blank one or a whitespace one, `none` with and without a stale
//! name, `inherit` with the global set (with surrounding blanks), blank and
//! unset — on a chat row, a candidate alias and an aux, audio and image row
//! answers after the upgrade exactly what it answered before it, as the old
//! loader's rules read it (`before`, a copy of `hold_fallback_for` and
//! `alias_fallback` as they stood, an unknown mode read as `inherit`; those
//! functions are gone, so the copy is by hand). The global goes from the
//! settings blob and nothing else in it changes; a blob that is not JSON
//! starts; one the loader did not read but the SQL would is refused, naming
//! it; and the start says what it changed, line by line.

use lmgw_core::config::HopKey;
use lmgw_core::runtime::Class;
use serde_json::{json, Value};
use sqlx::{AssertSqlSafe, Row, SqlitePool};

use super::db_at_version;

async fn exec(pool: &SqlitePool, sql: &str) {
    sqlx::query(AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap();
}

async fn settings_blob(pool: &SqlitePool, blob: Option<&str>) {
    exec(pool, "DELETE FROM settings WHERE key = 'settings'").await;
    if let Some(b) = blob {
        sqlx::query("INSERT INTO settings (key, value) VALUES ('settings', ?1)")
            .bind(b)
            .execute(pool)
            .await
            .unwrap();
    }
}

/// One row of a version-94 table: `(table, name, mode, stored name)`.
type Old = (
    &'static str,
    &'static str,
    &'static str,
    Option<&'static str>,
);

/// Every state of every table.
const ROWS: [Old; 31] = [
    ("local_models", "c-alias", "alias", Some("cloud-a")),
    ("local_models", "c-alias-blank", "alias", Some("  ")),
    ("local_models", "c-alias-tab", "alias", Some("\t")),
    ("local_models", "c-none", "none", None),
    ("local_models", "c-none-stale", "none", Some("stale")),
    ("local_models", "c-inherit", "inherit", None),
    (
        "local_models",
        "c-inherit-stale",
        "inherit",
        Some("stale-2"),
    ),
    ("local_models", "c-local", "alias", Some("local-b")),
    ("local_models", "local-b", "none", None),
    // A mode this build does not know: the loader read it as `inherit`.
    ("local_models", "c-weird", "Alias", Some("w")),
    ("aux_models", "a-alias", "alias", Some("embed-cloud")),
    ("aux_models", "a-alias-blank", "alias", Some("")),
    ("aux_models", "a-none-stale", "none", Some("x")),
    ("aux_models", "a-inherit", "inherit", None),
    ("aux_models", "a-to-chat", "alias", Some("local-b")),
    ("aux_models", "a-weird", "bogus", None),
    ("aux_models", "a-rerank", "alias", Some("local-b")),
    ("aux_models", "a-decide", "alias", Some("cloud-d")),
    ("audio_models", "t-alias", "alias", Some("cloud-tts")),
    ("audio_models", "t-none", "none", None),
    ("audio_models", "t-inherit", "inherit", None),
    ("audio_models", "s-asr", "alias", Some("local-b")),
    ("image_models", "i-alias", "alias", Some("cloud-image")),
    ("image_models", "i-inherit-stale", "inherit", Some("y")),
    ("image_models", "i-none", "none", None),
    ("candidate_aliases", "k-alias", "alias", Some("cloud-k")),
    ("candidate_aliases", "k-alias-blank", "alias", Some(" ")),
    ("candidate_aliases", "k-none", "none", None),
    ("candidate_aliases", "k-none-stale", "none", Some("s")),
    ("candidate_aliases", "k-inherit", "inherit", None),
    (
        "candidate_aliases",
        "k-inherit-stale",
        "inherit",
        Some("s2"),
    ),
];

/// The aux row's kind and the audio row's task: `a-rerank` a reranker,
/// `a-decide` a decision row, `s-asr` speech-to-text; the others an
/// embedder and text-to-speech.
fn kind_of(name: &str) -> &'static str {
    match name {
        "a-rerank" => "rerank",
        "a-decide" => "decision",
        "s-asr" => "asr",
        n if n.starts_with("a-") => "embed",
        _ => "tts",
    }
}

async fn seed(pool: &SqlitePool) {
    for (table, name, mode, stored) in ROWS {
        let q = match table {
            "local_models" => {
                "INSERT INTO local_models (model_id, gguf_path, params, args, idle_seconds, \
                 enabled, public, warm_start, hold_fallback_mode, hold_fallback) \
                 VALUES (?1, 'm.gguf', '{}', '[]', 300, 1, 1, 0, ?2, ?3)"
            }
            "aux_models" => {
                "INSERT INTO aux_models (model_id, gguf_path, kind, hold_fallback_mode, \
                 hold_fallback) VALUES (?1, 'e.gguf', ?4, ?2, ?3)"
            }
            "audio_models" => {
                "INSERT INTO audio_models (model_id, family, path, task, hold_fallback_mode, \
                 hold_fallback) VALUES (?1, 'kokoro', 'k', ?4, ?2, ?3)"
            }
            "image_models" => {
                "INSERT INTO image_models (model_id, hold_fallback_mode, hold_fallback) \
                 VALUES (?1, ?2, ?3)"
            }
            _ => {
                "INSERT INTO candidate_aliases (alias, candidates, fallback_mode, fallback) \
                 VALUES (?1, '[\"c-alias\"]', ?2, ?3)"
            }
        };
        let mut q = sqlx::query(q).bind(name).bind(mode).bind(stored);
        if matches!(table, "aux_models" | "audio_models") {
            q = q.bind(kind_of(name));
        }
        q.execute(pool).await.unwrap();
    }
}

/// What the row answered with before 0095, by today's rules
/// (`hold_fallback_for`, `alias_fallback`): `alias` with a name that is not
/// blank is that name as stored; `inherit` on a chat row or a candidate
/// alias is the global as the loader read it, not blank; nothing else.
fn before(row: &Old, global: Option<&str>) -> Option<String> {
    let (table, _, mode, stored) = row;
    let not_blank = |s: &str| !s.trim().is_empty();
    match *mode {
        "alias" => stored.filter(|s| not_blank(s)).map(String::from),
        "none" => None,
        // `inherit`, or a mode the loader did not know (`unwrap_or(Inherit)`).
        _ if matches!(*table, "local_models" | "candidate_aliases") => {
            global.filter(|s| not_blank(s)).map(String::from)
        }
        _ => None,
    }
}

/// The row's identity after the upgrade.
fn key_of(snap: &lmgw_core::config::Snapshot, row: &Old) -> HopKey {
    let (table, name, ..) = row;
    let local = |class| HopKey::Local(class, name.to_string());
    match *table {
        "local_models" => local(Class::Chat),
        "aux_models" => local(Class::Aux),
        "audio_models" => local(Class::Audio),
        "image_models" => local(Class::Image),
        _ => HopKey::Candidate(
            snap.candidate_aliases
                .values()
                .find(|c| c.alias == *name)
                .unwrap()
                .id,
        ),
    }
}

async fn has_column(pool: &SqlitePool, table: &str, column: &str) -> bool {
    let names: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info(?1)")
        .bind(table)
        .fetch_all(pool)
        .await
        .unwrap();
    names.iter().any(|n| n == column)
}

/// Upgrade a seeded version-94 database with `blob` as its settings and the
/// loader's reading of its global `global`; every row answers as before.
async fn upgrades_faithfully(blob: Option<&str>, global: Option<&str>) -> String {
    let pool = db_at_version(94).await;
    seed(&pool).await;
    settings_blob(&pool, blob).await;
    let (log, capturing) = crate::common::captured_log::capture_log();
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    drop(capturing);
    let snap = lmgw_core::store::load_snapshot(&pool).await.unwrap();
    for row in &ROWS {
        assert_eq!(
            snap.fallback_of(&key_of(&snap, row)),
            before(row, global).as_deref(),
            "{row:?} under the global {global:?}"
        );
    }
    for t in ["local_models", "aux_models", "audio_models", "image_models"] {
        assert!(!has_column(&pool, t, "hold_fallback").await, "{t}");
        assert!(!has_column(&pool, t, "hold_fallback_mode").await, "{t}");
        assert!(has_column(&pool, t, "fallback").await, "{t}");
    }
    assert!(!has_column(&pool, "candidate_aliases", "fallback_mode").await);
    assert!(has_column(&pool, "models", "fallback").await);
    assert_eq!(
        lmgw_core::store::get_kv(&pool, "fallback_type_check")
            .await
            .unwrap()
            .as_deref(),
        Some("pending"),
        "the first start types the cloud names"
    );
    log.text()
}

/// The settings blob after the upgrade, as JSON.
async fn blob_after(pool: &SqlitePool) -> Value {
    let raw: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'settings'")
        .fetch_one(pool)
        .await
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}

#[tokio::test]
async fn every_row_state_answers_as_before_with_the_global_set() {
    let blob = json!({
        "retention_days": 30,
        "hold": {"active": true, "fallback_alias": " cloud "},
        "router": {"public_prefix": ""},
    })
    .to_string();
    let log = upgrades_faithfully(Some(&blob), Some(" cloud ")).await;

    let lines = |needle: &str| log.lines().filter(|l| l.contains(needle)).count();
    // One per row that followed the global: two chat rows, two candidate
    // aliases. An aux, audio or image row on `inherit` is not named.
    assert_eq!(
        lines("followed the global hold fallback ' cloud '"),
        5,
        "{log}"
    );
    assert!(
        log.contains("chat model 'c-inherit' followed the global"),
        "{log}"
    );
    assert!(
        log.contains("candidate alias 'k-inherit' followed the global"),
        "{log}"
    );
    assert!(!log.contains("'a-inherit' followed"), "{log}");
    assert!(
        log.contains("chat model 'c-weird' followed the global"),
        "{log}"
    );
    assert!(!log.contains("'a-weird'"), "{log}");
    // One per stale name that was never read.
    assert_eq!(lines("which was never read; it is dropped"), 7, "{log}");
    assert!(
        log.contains("chat model 'c-none-stale' had 'stale' stored"),
        "{log}"
    );
    assert!(
        log.contains("image model 'i-inherit-stale' had 'y' stored"),
        "{log}"
    );
    // One per fallback that names a local model, which is a hop now.
    assert_eq!(lines("which lmgw refused to use until now"), 4, "{log}");
    assert!(
        log.contains("chat model 'c-local' falls back to the local chat model 'local-b'"),
        "{log}"
    );
    // And one per local fallback of another type: an embedder onto a chat
    // model, kept, refused at the row's next save.
    assert_eq!(
        lines("a fallback must be the same type of model"),
        3,
        "{log}"
    );
    assert!(
        log.contains("embedding model 'a-to-chat' falls back to 'local-b', a chat model"),
        "{log}"
    );
    assert!(
        log.contains("reranker 'a-rerank' falls back to 'local-b', a chat model"),
        "{log}"
    );
    assert!(
        log.contains(
            "audio model 's-asr' falls back to 'local-b', a chat model; a fallback must be the \
             same type of model (a speech-to-text model here)"
        ),
        "{log}"
    );
    assert!(
        log.contains("the global hold fallback ' cloud ' was copied onto 3 model(s) and 2 candidate alias(es)"),
        "{log}"
    );
}

#[tokio::test]
async fn the_global_goes_and_nothing_else_in_the_settings_changes() {
    let pool = db_at_version(94).await;
    let before = json!({
        "retention_days": 30,
        "hold": {"active": true, "fallback_alias": "cloud"},
        "audio": {"public_prefix": "audio"},
        "chat_system_prompt": "Be terse.",
    });
    settings_blob(&pool, Some(&before.to_string())).await;
    lmgw_core::store::run_migrations(&pool).await.unwrap();
    let after = blob_after(&pool).await;
    let mut expected = before.clone();
    expected["hold"]
        .as_object_mut()
        .unwrap()
        .remove("fallback_alias");
    assert_eq!(after, expected);
}

#[tokio::test]
async fn an_unset_or_blank_global_gives_the_inheriting_rows_none() {
    upgrades_faithfully(None, None).await;
    let blob = json!({"hold": {"active": false}}).to_string();
    upgrades_faithfully(Some(&blob), None).await;
    let blob = json!({"hold": {"active": false, "fallback_alias": ""}}).to_string();
    upgrades_faithfully(Some(&blob), Some("")).await;
    // Blank to the loader but not to SQL's TRIM: evened out first.
    let blob = json!({"hold": {"active": false, "fallback_alias": "\t\n"}}).to_string();
    upgrades_faithfully(Some(&blob), Some("\t\n")).await;
}

/// A blob that is not JSON is a default `Settings` to the loader: nothing
/// inherits, and the SQL reads nothing out of it either.
#[tokio::test]
async fn a_blob_that_is_not_json_gives_none_everywhere_and_starts() {
    upgrades_faithfully(Some("{not json"), None).await;
}

/// JSON the loader did not read as `Settings` (so no row inherited its
/// global) while the SQL would copy the name: the one state the upgrade
/// cannot carry over, refused by name, nothing changed.
#[tokio::test]
async fn a_global_the_loader_never_read_is_refused_by_name() {
    let pool = db_at_version(94).await;
    seed(&pool).await;
    let blob = json!({
        "retention_days": "thirty",
        "hold": {"fallback_alias": "cloud"},
    })
    .to_string();
    settings_blob(&pool, Some(&blob)).await;
    let err = lmgw_core::store::run_migrations(&pool)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("'cloud'") && err.contains("Nothing has been changed"),
        "{err}"
    );
    assert!(has_column(&pool, "local_models", "hold_fallback_mode").await);
    let still: String = sqlx::query("SELECT value FROM settings WHERE key = 'settings'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("value");
    assert_eq!(still, blob);
}

/// A global that is JSON but not a name (a number, a boolean): the loader
/// read no `Settings` out of the blob, the SQL would copy its text. The
/// guard's refusal says so, rather than a database type error.
#[tokio::test]
async fn a_global_that_is_not_a_name_is_the_guards_refusal() {
    for global in [json!(5), json!(true)] {
        let pool = db_at_version(94).await;
        seed(&pool).await;
        let blob = json!({"hold": {"fallback_alias": global}}).to_string();
        settings_blob(&pool, Some(&blob)).await;
        let err = lmgw_core::store::run_migrations(&pool)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&format!("hold {global} (not a name)"))
                && err.contains("Nothing has been changed"),
            "{err}"
        );
        assert!(has_column(&pool, "local_models", "hold_fallback_mode").await);
    }
}
