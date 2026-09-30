//! Chat auto-archive, purge and pinning (chat-archive-pin-attachments design
//! §1): the hourly sweep's math (direct `store::` calls, same shape as
//! `store/usage_tests.rs`'s own `prune_logs` test), and the three HTTP routes
//! (`/pin`, `/archive`, list ordering) driven through the real router.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common;
use common::{serve, Gw};

async fn setup(upstream_base: &str) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: upstream_base.trim_end_matches('/').to_string(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "my-model".into(),
            upstream_id: up_id,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

/// Push a thread's `updated_at` (and optionally `archived_at`) back in time,
/// the way a test simulates "idle for N days" without waiting N days.
async fn backdate(
    state: &SharedState,
    id: i64,
    updated_days_ago: i64,
    archived_days_ago: Option<i64>,
) {
    sqlx::query("UPDATE chat_threads SET updated_at = datetime('now', ?2) WHERE id = ?1")
        .bind(id)
        .bind(format!("-{updated_days_ago} days"))
        .execute(&state.db)
        .await
        .unwrap();
    if let Some(days) = archived_days_ago {
        sqlx::query("UPDATE chat_threads SET archived_at = datetime('now', ?2) WHERE id = ?1")
            .bind(id)
            .bind(format!("-{days} days"))
            .execute(&state.db)
            .await
            .unwrap();
    }
}

async fn thread_row(state: &SharedState, id: i64) -> store::ChatThread {
    store::get_chat_thread(&state.db, id)
        .await
        .unwrap()
        .unwrap()
}

// ---------------------------------------------------------------------------
// The sweep itself
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sweep_archives_an_idle_unpinned_thread() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::create_chat_thread(&state.db, "my-model", "chat")
        .await
        .unwrap();
    backdate(&state, id, 15, None).await;

    let (archived, purged) = store::sweep_chat_threads(&state.db, 14, 30).await.unwrap();
    assert_eq!((archived, purged), (1, 0));
    let t = thread_row(&state, id).await;
    assert!(t.archived_at.is_some(), "{t:?}");
}

#[tokio::test]
async fn sweep_never_archives_a_pinned_thread() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::create_chat_thread(&state.db, "my-model", "chat")
        .await
        .unwrap();
    store::set_chat_thread_pinned(&state.db, id, true)
        .await
        .unwrap();
    backdate(&state, id, 100, None).await;

    let (archived, purged) = store::sweep_chat_threads(&state.db, 14, 30).await.unwrap();
    assert_eq!((archived, purged), (0, 0));
    let t = thread_row(&state, id).await;
    assert!(
        t.archived_at.is_none(),
        "a pinned thread must never auto-archive: {t:?}"
    );
}

#[tokio::test]
async fn sweep_purges_only_once_archived_at_is_past_purge_days() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::create_chat_thread(&state.db, "my-model", "chat")
        .await
        .unwrap();
    // Archived 10 days ago: not yet past a 30-day purge window.
    backdate(&state, id, 40, Some(10)).await;
    let (_, purged) = store::sweep_chat_threads(&state.db, 14, 30).await.unwrap();
    assert_eq!(purged, 0, "10 days archived must not be purged at 30");
    assert!(thread_row(&state, id).await.archived_at.is_some());

    // Push the archive date back to 31 days: now past the window.
    backdate(&state, id, 40, Some(31)).await;
    let (_, purged) = store::sweep_chat_threads(&state.db, 14, 30).await.unwrap();
    assert_eq!(purged, 1);
    assert!(
        store::get_chat_thread(&state.db, id)
            .await
            .unwrap()
            .is_none(),
        "the thread must be gone"
    );
}

/// The purge clock is `archived_at`, not `updated_at` — a thread archived by
/// hand (long idle already) still gets the full `purge_days` from the moment
/// it was archived (design §1).
#[tokio::test]
async fn purge_clock_is_archived_at_not_updated_at() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::create_chat_thread(&state.db, "my-model", "chat")
        .await
        .unwrap();
    // Idle for a year, but archived (by hand) only yesterday.
    backdate(&state, id, 400, Some(1)).await;
    let (_, purged) = store::sweep_chat_threads(&state.db, 14, 30).await.unwrap();
    assert_eq!(
        purged, 0,
        "archived only yesterday — must survive a 30-day purge window"
    );
}

#[tokio::test]
async fn zero_disables_archiving_and_zero_disables_purging() {
    let state = AppState::init_for_tests().await.unwrap();
    let idle = store::create_chat_thread(&state.db, "my-model", "chat")
        .await
        .unwrap();
    backdate(&state, idle, 999, None).await;
    let (archived, _) = store::sweep_chat_threads(&state.db, 0, 30).await.unwrap();
    assert_eq!(archived, 0, "archive_days=0 must never auto-archive");

    let archived_thread = store::create_chat_thread(&state.db, "my-model", "chat")
        .await
        .unwrap();
    backdate(&state, archived_thread, 999, Some(999)).await;
    let (_, purged) = store::sweep_chat_threads(&state.db, 14, 0).await.unwrap();
    assert_eq!(
        purged, 0,
        "purge_days=0 must never delete an archived thread"
    );
}

// ---------------------------------------------------------------------------
// Pin / archive / restore over HTTP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pinning_an_archived_thread_restores_it_and_bumps_updated_at() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri()).await;
    let client = base.client();
    let thread: Value = client
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": "my-model" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = thread["id"].as_i64().unwrap();

    store::archive_chat_thread(&state.db, id).await.unwrap();
    // Backdate `updated_at` so the bump is observable.
    sqlx::query("UPDATE chat_threads SET updated_at = datetime('now', '-1 day') WHERE id = ?1")
        .bind(id)
        .execute(&state.db)
        .await
        .unwrap();
    let before = thread_row(&state, id).await;
    assert!(before.archived_at.is_some());

    let resp: Value = client
        .post(format!("{base}/chat/api/threads/{id}/pin"))
        .json(&json!({ "pinned": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["pinned"], true);
    assert!(resp["archived_at"].is_null(), "{resp:#}");

    let after = thread_row(&state, id).await;
    assert!(after.archived_at.is_none(), "pinning must restore it");
    assert!(
        after.updated_at > before.updated_at,
        "restoring must bump updated_at: before={} after={}",
        before.updated_at,
        after.updated_at
    );
}

#[tokio::test]
async fn archive_route_restores_on_false() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri()).await;
    let client = base.client();
    let thread: Value = client
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": "my-model" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = thread["id"].as_i64().unwrap();

    let resp: Value = client
        .post(format!("{base}/chat/api/threads/{id}/archive"))
        .json(&json!({ "archived": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(resp["archived_at"].is_string(), "{resp:#}");
    assert!(thread_row(&state, id).await.archived_at.is_some());

    let resp: Value = client
        .post(format!("{base}/chat/api/threads/{id}/archive"))
        .json(&json!({ "archived": false }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(resp["archived_at"].is_null(), "{resp:#}");
    assert!(thread_row(&state, id).await.archived_at.is_none());
}

/// Sending into an archived thread restores it (design §1) — a message is a
/// clearer "bring this back" signal than an explicit button, so it acts like
/// one.
#[tokio::test]
async fn sending_into_an_archived_thread_restores_it() {
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock)
        .await;
    let (state, base) = setup(&mock.uri()).await;
    let client = base.client();
    let thread: Value = client
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": "my-model" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = thread["id"].as_i64().unwrap();
    store::archive_chat_thread(&state.db, id).await.unwrap();
    assert!(thread_row(&state, id).await.archived_at.is_some());

    let _ = client
        .post(format!("{base}/chat/api/threads/{id}/send"))
        .json(&json!({ "content": "hello again" }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        thread_row(&state, id).await.archived_at.is_none(),
        "sending into it must restore it"
    );
}

// ---------------------------------------------------------------------------
// Listing: ordering, archived toggle, purge_at, archived_count
// ---------------------------------------------------------------------------

#[tokio::test]
async fn active_list_is_pinned_first_then_most_recently_active() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri()).await;
    let client = base.client();
    async fn new_thread(base: &Gw, client: &reqwest::Client) -> i64 {
        client
            .post(format!("{base}/chat/api/threads"))
            .json(&json!({ "model_alias": "my-model" }))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["id"]
            .as_i64()
            .unwrap()
    }
    let old = new_thread(&base, &client).await;
    let pinned = new_thread(&base, &client).await;
    let newest = new_thread(&base, &client).await;

    // Oldest of the three by `updated_at`, but pinned — must still sort first.
    sqlx::query("UPDATE chat_threads SET updated_at = datetime('now', '-10 days') WHERE id = ?1")
        .bind(old)
        .execute(&state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE chat_threads SET updated_at = datetime('now', '-5 days') WHERE id = ?1")
        .bind(pinned)
        .execute(&state.db)
        .await
        .unwrap();
    store::set_chat_thread_pinned(&state.db, pinned, true)
        .await
        .unwrap();

    let resp: Value = client
        .get(format!("{base}/chat/api/threads"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<i64> = resp["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![pinned, newest, old], "{resp:#}");
    assert_eq!(resp["archived_count"], 0);
}

#[tokio::test]
async fn archived_query_lists_archived_newest_first_with_purge_at_and_count() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri()).await;
    let client = base.client();
    let t1: Value = client
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": "my-model" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id1 = t1["id"].as_i64().unwrap();
    let t2: Value = client
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": "my-model" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id2 = t2["id"].as_i64().unwrap();

    store::archive_chat_thread(&state.db, id1).await.unwrap();
    sqlx::query("UPDATE chat_threads SET archived_at = datetime('now', '-1 day') WHERE id = ?1")
        .bind(id1)
        .execute(&state.db)
        .await
        .unwrap();
    store::archive_chat_thread(&state.db, id2).await.unwrap();

    let resp: Value = client
        .get(format!("{base}/chat/api/threads?archived=1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<i64> = resp["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![id2, id1],
        "most-recently-archived first: {resp:#}"
    );
    assert_eq!(resp["archived_count"], 2);
    for t in resp["threads"].as_array().unwrap() {
        assert!(t["purge_at"].is_string(), "{t:#}");
    }
}

/// `?archived=all` (review finding 1: the Agent Runs tab needs an agent's
/// archived threads too, and it filters this same list client-side by
/// `agent_id`) — active and archived threads together, in the active list's
/// own order: pinned first, then `updated_at DESC`. Any other non-empty value
/// besides the literal `all` still means "archived only" (the contract the
/// Chat page's own `?archived=1` already relies on).
#[tokio::test]
async fn archived_all_lists_active_and_archived_together_pinned_first() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri()).await;
    let client = base.client();
    async fn new_thread(base: &Gw, client: &reqwest::Client) -> i64 {
        client
            .post(format!("{base}/chat/api/threads"))
            .json(&json!({ "model_alias": "my-model" }))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["id"]
            .as_i64()
            .unwrap()
    }
    let active = new_thread(&base, &client).await;
    let archived = new_thread(&base, &client).await;
    let pinned = new_thread(&base, &client).await;

    store::archive_chat_thread(&state.db, archived)
        .await
        .unwrap();
    // Explicit, distinct `updated_at`s rather than relying on wall-clock
    // creation order (SQLite's `datetime('now')` is only second-resolution,
    // so three threads created microseconds apart can tie) — same technique
    // `active_list_is_pinned_first_then_most_recently_active` uses above.
    // `active` is the oldest of the two non-pinned threads, `archived` the
    // most recently active; `pinned` is the oldest of all three by that clock
    // but must still sort first.
    sqlx::query("UPDATE chat_threads SET updated_at = datetime('now', '-5 days') WHERE id = ?1")
        .bind(active)
        .execute(&state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE chat_threads SET updated_at = datetime('now', '-10 days') WHERE id = ?1")
        .bind(pinned)
        .execute(&state.db)
        .await
        .unwrap();
    store::set_chat_thread_pinned(&state.db, pinned, true)
        .await
        .unwrap();

    let resp: Value = client
        .get(format!("{base}/chat/api/threads?archived=all"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<i64> = resp["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![pinned, archived, active], "{resp:#}");
    assert_eq!(resp["archived_count"], 1);

    // A value other than the literal "all" is still "archived only".
    let resp: Value = client
        .get(format!("{base}/chat/api/threads?archived=yes"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<i64> = resp["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![archived], "{resp:#}");
}

// ---------------------------------------------------------------------------
// Archiving a pinned thread (review finding 6)
// ---------------------------------------------------------------------------

/// A pinned thread left pinned after a hand archive would never purge, and a
/// later unpin would purge it immediately against a stale `archived_at` —
/// archiving must win over pinning.
#[tokio::test]
async fn archiving_a_pinned_thread_unpins_it() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::create_chat_thread(&state.db, "my-model", "chat")
        .await
        .unwrap();
    store::set_chat_thread_pinned(&state.db, id, true)
        .await
        .unwrap();
    assert!(thread_row(&state, id).await.pinned);

    store::archive_chat_thread(&state.db, id).await.unwrap();
    let t = thread_row(&state, id).await;
    assert!(t.archived_at.is_some(), "{t:?}");
    assert!(!t.pinned, "archiving must unpin: {t:?}");
}

#[tokio::test]
async fn archive_route_reports_the_thread_as_unpinned() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri()).await;
    let client = base.client();
    let thread: Value = client
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": "my-model" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = thread["id"].as_i64().unwrap();
    store::set_chat_thread_pinned(&state.db, id, true)
        .await
        .unwrap();

    let resp: Value = client
        .post(format!("{base}/chat/api/threads/{id}/archive"))
        .json(&json!({ "archived": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["pinned"], false, "{resp:#}");
    assert!(resp["archived_at"].is_string(), "{resp:#}");
}

// ---------------------------------------------------------------------------
// `chat_thread_purge_at` overflow (review finding 4)
// ---------------------------------------------------------------------------

/// `chrono::Duration::days` and `NaiveDateTime`'s `Add` both panic on
/// overflow; a `purge_days` this large used to 500 every listing that had an
/// archived thread. The checked path must return `None` instead.
#[test]
fn chat_thread_purge_at_is_none_for_unrepresentable_purge_days() {
    let archived_at = "2026-01-01 00:00:00";
    assert!(store::chat_thread_purge_at(archived_at, 99_999_999).is_none());
    assert!(store::chat_thread_purge_at(archived_at, i64::MAX).is_none());
    assert!(store::chat_thread_purge_at(archived_at, i64::MAX - 1).is_none());
    // A normal value still works.
    assert_eq!(
        store::chat_thread_purge_at(archived_at, 30).unwrap(),
        "2026-01-31 00:00:00"
    );
}

/// The HTTP path that used to 500: listing an archived thread while
/// `chat_purge_days` is set to an unrepresentable value must answer 200 with
/// `purge_at: null`, not crash.
#[tokio::test]
async fn listing_with_an_unrepresentable_purge_days_does_not_500() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri()).await;
    let client = base.client();
    let thread: Value = client
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": "my-model" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = thread["id"].as_i64().unwrap();
    store::archive_chat_thread(&state.db, id).await.unwrap();

    let mut settings = state.snapshot().settings.clone();
    settings.chat_purge_days = 99_999_999;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let resp = client
        .get(format!("{base}/chat/api/threads?archived=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "must not 500 (review finding 4)");
    let body: Value = resp.json().await.unwrap();
    assert!(body["threads"][0]["purge_at"].is_null(), "{body:#}");
}

/// The sweep's own arithmetic never went through `chrono::Duration` (it hands
/// SQLite a `datetime('now', '-N days')` modifier string), but the finding
/// asked to confirm it too: SQLite quietly returns `NULL` for an offset this
/// large, so the comparison it feeds never matches — no purge, no error.
#[tokio::test]
async fn sweep_with_an_unrepresentable_purge_days_purges_nothing_and_does_not_error() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::create_chat_thread(&state.db, "my-model", "chat")
        .await
        .unwrap();
    backdate(&state, id, 400, Some(365)).await;

    let (archived, purged) = store::sweep_chat_threads(&state.db, 14, 99_999_999)
        .await
        .unwrap();
    assert_eq!((archived, purged), (0, 0));
    assert!(thread_row(&state, id).await.archived_at.is_some());
}
