//! How a catalog read is shared and when a request waits for one (openai
//! egress design §14.2): requests that miss at once share one read, and a
//! stale list answers at once while its refresh runs in the background (the
//! owner's answer of 2026-10-25).

use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::config::UpstreamKind;
use crate::state::AppState;

/// A list naming `id`, after `delay`.
fn listing(id: &str, delay: Duration) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(json!({"object": "list", "data": [{"id": id, "object": "model"}]}))
        .set_delay(delay)
}

fn upstream(mock: &MockServer) -> Upstream {
    Upstream {
        id: 7,
        name: "listed".into(),
        protocol: Protocol::OpenaiCompatible,
        kind: UpstreamKind::Generic,
        base_url: format!("{}/v1", mock.uri()),
        api_key: None,
        extra_headers: vec![],
        timeout_ms: 30_000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

fn ids(read: Result<Vec<ModelInfo>, String>) -> Vec<String> {
    read.unwrap().into_iter().map(|m| m.id).collect()
}

#[tokio::test]
async fn requests_that_miss_at_once_share_one_read() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(listing("m1", Duration::from_millis(300)))
        .mount(&mock)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    let u = upstream(&mock);
    let reads = futures::future::join_all((0..5).map(|_| upstream_models(&state, &u))).await;
    for read in reads {
        assert_eq!(ids(read), ["m1"]);
    }
    assert_eq!(mock.received_requests().await.unwrap().len(), 1);
    // Cached from the one read: no other.
    assert_eq!(ids(upstream_models(&state, &u).await), ["m1"]);
    assert_eq!(mock.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_stale_list_answers_at_once_and_its_refresh_replaces_it() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(listing("old", Duration::ZERO))
        .up_to_n_times(1)
        .mount(&mock)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    let u = upstream(&mock);
    assert_eq!(ids(upstream_models(&state, &u).await), ["old"]);
    state.catalog.expire_for_tests(&u).await;
    // The provider now takes a minute to list.
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(listing("new", Duration::from_millis(600)))
        .mount(&mock)
        .await;
    let stale = tokio::time::timeout(Duration::from_millis(300), upstream_models(&state, &u))
        .await
        .expect("a stale list answers without waiting for the read");
    assert_eq!(ids(stale), ["old"]);
    // The background read is the one in flight; once it is in, the cache
    // has its answer.
    assert_eq!(ids(read_once(&state, &u).await), ["new"]);
    assert_eq!(ids(upstream_models(&state, &u).await), ["new"]);
    assert_eq!(mock.received_requests().await.unwrap().len(), 2);
}

/// A cached failure past its time is read again, and the request waits
/// for it: there is no list to answer with.
#[tokio::test]
async fn a_cached_failure_past_its_time_is_waited_for() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&mock)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    let u = upstream(&mock);
    assert!(upstream_models(&state, &u).await.is_err());
    state.catalog.expire_for_tests(&u).await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(listing("m1", Duration::from_millis(100)))
        .mount(&mock)
        .await;
    assert_eq!(ids(upstream_models(&state, &u).await), ["m1"]);
}

/// A read that `invalidate` dropped leaves no answer behind: the row's
/// configuration may have changed under it.
#[tokio::test]
async fn an_invalidated_read_caches_nothing() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(listing("m1", Duration::from_millis(200)))
        .mount(&mock)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    let u = upstream(&mock);
    let read = read_once(&state, &u);
    state.catalog.invalidate(u.id).await;
    assert_eq!(ids(read.await), ["m1"], "its waiters still get the answer");
    assert!(!state.catalog.holds_for_tests(&u).await);
}

/// A request that holds a snapshot from before a write changed the row (it
/// queued across the write) reads the old configuration's list, after the
/// write's `invalidate` ran. That answer is the old configuration's only: a
/// request holding the row as it is now reads its own.
#[tokio::test]
async fn a_read_from_a_snapshot_before_a_change_answers_that_configuration_only() {
    let (old, new) = (MockServer::start().await, MockServer::start().await);
    for (mock, id) in [(&old, "old"), (&new, "new")] {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(listing(id, Duration::ZERO))
            .mount(mock)
            .await;
    }
    let state = AppState::init_for_tests().await.unwrap();
    let before = upstream(&old);
    let mut after = before.clone();
    after.base_url = format!("{}/v1", new.uri());
    after.api_key = Some("sk-new".into());
    // The write's invalidate, then the queued request's read.
    state.catalog.invalidate(before.id).await;
    assert_eq!(ids(upstream_models(&state, &before).await), ["old"]);
    assert!(!state.catalog.holds_for_tests(&after).await);
    assert_eq!(ids(upstream_models(&state, &after).await), ["new"]);
    assert_eq!(new.received_requests().await.unwrap().len(), 1);
    // The next write forgets both.
    state.catalog.invalidate(before.id).await;
    assert!(!state.catalog.holds_for_tests(&before).await);
    assert!(!state.catalog.holds_for_tests(&after).await);
}

/// A read task that dies (a panic unwinding through it) leaves its place in
/// flight, whether or not anyone polls the shared read: the next request
/// starts another rather than awaiting one nobody runs.
#[tokio::test]
async fn a_read_task_that_dies_leaves_its_place() {
    let mock = MockServer::start().await;
    let state = AppState::init_for_tests().await.unwrap();
    let u = upstream(&mock);
    let key = read_key::ReadKey::of(&u);
    let n = u64::MAX;
    let never: Read =
        futures::FutureExt::shared(futures::FutureExt::boxed(futures::future::pending::<
            Result<Vec<ModelInfo>, String>,
        >()));
    state
        .catalog
        .reading
        .lock()
        .unwrap()
        .insert(key.clone(), (n, never));
    assert!(state.catalog.reading_for_tests(&u));
    let st = state.clone();
    let died = tokio::spawn(async move {
        let _leave = read_key::Leave { state: st, key, n };
        panic!("a catalog read that dies");
    })
    .await;
    assert!(died.is_err());
    assert!(!state.catalog.reading_for_tests(&u));
}
