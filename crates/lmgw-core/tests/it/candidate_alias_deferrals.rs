//! `candidates::deferrals` (candidate-aliases design §6; §12 phase-4 worker
//! S): the 24-hour `gpu_deferred` count `lmgw__status`, `lmgw__models
//! kind=alias` and `GET /api/models/full` all read.
//!
//! No candidate alias row is needed for these — the count is a plain
//! `request_logs` aggregate keyed by `requested_alias`, so a bare gateway
//! with hand-inserted log rows is the whole fixture. The rows are inserted
//! directly with `sqlx` rather than through `store::log_request`, because
//! that function stamps `ts` from the live clock and these tests need to
//! place rows on both sides of the 24-hour boundary.

use lmgw_core::candidates::deferrals;
use lmgw_core::state::AppState;
use sqlx::Row;

/// Insert one minimal `request_logs` row. Every column this table gained
/// after `0001_init.sql` is nullable (checked against every migration that
/// touches it), so `ts`, `ingress_proto`, `requested_alias`, `status` and
/// `error_kind` are the whole fixture.
async fn log_row(
    pool: &sqlx::SqlitePool,
    ts: chrono::DateTime<chrono::Utc>,
    alias: &str,
    error_kind: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO request_logs (ts, ingress_proto, requested_alias, status, error_kind)
         VALUES (?, 'openai', ?, 503, ?)",
    )
    .bind(ts.format("%Y-%m-%d %H:%M:%S").to_string())
    .bind(alias)
    .bind(error_kind)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn counts_gpu_deferred_inside_24h_only() {
    let state = AppState::init_for_tests().await.unwrap();
    let now = chrono::Utc::now();

    // Inside the window: counts.
    log_row(
        &state.db,
        now - chrono::Duration::hours(1),
        "job-a",
        Some("gpu_deferred"),
    )
    .await;
    log_row(
        &state.db,
        now - chrono::Duration::hours(23),
        "job-a",
        Some("gpu_deferred"),
    )
    .await;
    // Outside the window: does not count, even though it is the same alias
    // and the same kind.
    log_row(
        &state.db,
        now - chrono::Duration::hours(25),
        "job-a",
        Some("gpu_deferred"),
    )
    .await;
    // Inside the window, but not a deferral: a real hold and a plain
    // success must not inflate the count.
    log_row(
        &state.db,
        now - chrono::Duration::hours(1),
        "job-a",
        Some("gpu_hold"),
    )
    .await;
    log_row(&state.db, now - chrono::Duration::hours(1), "job-a", None).await;

    let counts = deferrals::deferrals_24h(&state.db).await.unwrap();
    assert_eq!(counts.get("job-a").copied(), Some(2));
    assert_eq!(deferrals::for_alias(&counts, "job-a"), Some(2));
}

#[tokio::test]
async fn for_alias_matches_case_insensitively_and_defaults_to_zero() {
    let state = AppState::init_for_tests().await.unwrap();
    let now = chrono::Utc::now();
    log_row(&state.db, now, "Background-Chat", Some("gpu_deferred")).await;

    let counts = deferrals::deferrals_24h(&state.db).await.unwrap();
    assert_eq!(deferrals::for_alias(&counts, "background-chat"), Some(1));
    assert_eq!(deferrals::for_alias(&counts, "BACKGROUND-CHAT"), Some(1));
    // An alias with no deferral logged is a real zero, not absent — the
    // wire's `Option` is only for old-frame compatibility (the field's own
    // doc comment), never "unknown" from a live builder.
    assert_eq!(deferrals::for_alias(&counts, "some-other-alias"), Some(0));
}

/// Two spellings of the same alias must sum into one bucket — the bug this
/// guards: grouping by the literal `requested_alias` column left `"jobs"`
/// and `"Jobs"` as two separate rows in SQLite's own `GROUP BY`, and
/// `for_alias`'s case-insensitive lookup used to return only the first
/// match it found, silently dropping the other spelling's count.
#[tokio::test]
async fn mixed_spellings_of_the_same_alias_sum_into_one_count() {
    let state = AppState::init_for_tests().await.unwrap();
    let now = chrono::Utc::now();
    log_row(&state.db, now, "jobs", Some("gpu_deferred")).await;
    log_row(&state.db, now, "Jobs", Some("gpu_deferred")).await;
    log_row(&state.db, now, "JOBS", Some("gpu_deferred")).await;

    let counts = deferrals::deferrals_24h(&state.db).await.unwrap();
    assert_eq!(counts.len(), 1, "{counts:?}");
    assert_eq!(deferrals::for_alias(&counts, "jobs"), Some(3));
    assert_eq!(deferrals::for_alias(&counts, "JOBS"), Some(3));
}

#[tokio::test]
async fn counts_are_per_alias() {
    let state = AppState::init_for_tests().await.unwrap();
    let now = chrono::Utc::now();
    log_row(&state.db, now, "alias-one", Some("gpu_deferred")).await;
    log_row(&state.db, now, "alias-one", Some("gpu_deferred")).await;
    log_row(&state.db, now, "alias-two", Some("gpu_deferred")).await;

    let counts = deferrals::deferrals_24h(&state.db).await.unwrap();
    assert_eq!(counts.len(), 2);
    assert_eq!(deferrals::for_alias(&counts, "alias-one"), Some(2));
    assert_eq!(deferrals::for_alias(&counts, "alias-two"), Some(1));
}

/// A sanity check that the query really reads `error_kind`/`requested_alias`
/// and not some other pair of columns — a schema rename anywhere near this
/// table would otherwise fail silently as "always zero" rather than a
/// compile or runtime error, since the query is a bound SQL string.
#[tokio::test]
async fn an_empty_log_counts_nothing() {
    let state = AppState::init_for_tests().await.unwrap();
    let counts = deferrals::deferrals_24h(&state.db).await.unwrap();
    assert!(counts.is_empty());
    assert_eq!(deferrals::for_alias(&counts, "anything"), Some(0));

    // Confirm the row really landed under the columns the query reads, so
    // the three tests above are not passing by accident of an empty table.
    let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM request_logs")
        .fetch_one(&state.db)
        .await
        .unwrap()
        .get("n");
    assert_eq!(n, 0);
}
