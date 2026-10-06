//! A request whose client goes away while lmgw is starting or stopping a
//! container for it (`docs/design/2026-10-06-registry-owns-start.md`).
//!
//! hyper drops the handler future when the connection closes. The start and
//! the stop are the registry's own tasks, so neither is dropped with it: a
//! start given up on mid-load still ends with its model `ready` and idle — not
//! a container running with no entry, whose memory every later admission
//! reads as somebody else's — and a stop given up on still removes its entry
//! instead of leaving it `stopping` for every later acquire to park on. The
//! request itself leaves a `499` `client_disconnected` row.

use lmgw_core::runtime::registry::RuntimeState;

use super::unheld_containers::entry;
use super::*;

/// Spin until `cond`, in 10 ms steps. Bounded, so a broken invariant fails
/// the test instead of hanging it.
pub(super) async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

fn ready_and_idle(f: &Fixture, model: &str) -> bool {
    entry(f, model).is_some_and(|v| v.state == RuntimeState::Ready && v.in_flight == 0)
}

/// The request-log rows for `chat-model`, as many as there are once
/// `cond` holds for them.
async fn rows_until(
    f: &Fixture,
    what: &str,
    cond: impl Fn(&[store::RequestLogRow]) -> bool,
) -> Vec<store::RequestLogRow> {
    for _ in 0..500 {
        let rows = store::query_logs(
            &f.state.db,
            &store::LogFilter {
                alias: Some("chat-model".into()),
                limit: 1000,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if cond(&rows) {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Measured 2026-10-06: a `POST /v1/count_tokens` for a model that was not
/// up, its client giving up after 30 s while the model was still loading,
/// left the container running with no registry entry. The next request for
/// that very model waited out the whole queue timeout behind memory it
/// could not see was its own model's.
#[tokio::test]
async fn a_count_given_up_on_mid_load_leaves_its_model_ready_and_idle() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.world().health_held = true;

    let gateway = f.gateway.clone();
    let counting = tokio::spawn(async move {
        gateway
            .client()
            .post(format!("{gateway}/v1/count_tokens"))
            .json(&json!({"model": "chat-model", "input": "one two three"}))
            .send()
            .await
            .map(|r| r.status())
    });
    until("the count's podman run", || {
        f.runs() == vec!["chat-model".to_string()]
    })
    .await;

    // The client goes away mid-load: aborting the task drops the request
    // future, and with it the connection.
    counting.abort();
    let _ = counting.await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The model finishes loading with nobody waiting for it.
    f.world().health_held = false;
    until("the model ready and idle", || {
        ready_and_idle(&f, "chat-model")
    })
    .await;

    // The count's own row says it went away — written once hyper dropped
    // the handler, naming where the count was.
    let rows = rows_until(&f, "the count's 499 row", |rows| {
        rows.iter().any(|r| r.status == 499)
    })
    .await;
    let row = rows.iter().find(|r| r.status == 499).unwrap();
    assert_eq!(row.error_kind.as_deref(), Some("client_disconnected"));
    assert!(
        row.error_msg
            .as_deref()
            .unwrap_or_default()
            .contains("container to start"),
        "the row names the stage the count was in: {:?}",
        row.error_msg
    );

    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert_eq!(
        f.runs(),
        vec!["chat-model".to_string()],
        "one podman run — the next request found the model the count started"
    );
    assert!(f.stops().is_empty(), "{:?}", f.stops());
    assert_eq!(f.state.telemetry.stats().active_requests, 0);
}

/// The same with a streaming chat: the start goes on without it, and the
/// request is a `499` row that names where it was, with the in-flight gauge
/// closed.
#[tokio::test]
async fn a_streaming_chat_given_up_on_mid_load_is_a_499_row_and_closes_the_gauge() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.world().health_held = true;

    let gateway = f.gateway.clone();
    let chatting = tokio::spawn(async move {
        chat_body(
            &gateway,
            json!({
                "model": "chat-model",
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}],
            }),
        )
        .await
        .status()
    });
    until("the chat's podman run", || {
        f.runs() == vec!["chat-model".to_string()]
    })
    .await;
    assert_eq!(
        f.state.telemetry.stats().active_requests,
        1,
        "the chat is in flight while its model loads"
    );

    chatting.abort();
    let _ = chatting.await;
    let rows = rows_until(&f, "the chat's 499 row", |rows| !rows.is_empty()).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row.status, 499);
    assert_eq!(row.error_kind.as_deref(), Some("client_disconnected"));
    assert!(row.streamed);
    assert!(
        row.error_msg
            .as_deref()
            .unwrap_or_default()
            .contains("waiting for admission"),
        "{:?}",
        row.error_msg
    );
    assert_eq!(
        f.state.telemetry.stats().active_requests,
        0,
        "the gauge closes with the row"
    );

    f.world().health_held = false;
    until("the model ready and idle", || {
        ready_and_idle(&f, "chat-model")
    })
    .await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
}

/// The stop's twin: a request evicting a model goes away while `podman wait`
/// is still waiting for the victim's process. Before, the stop ran inside
/// that request's future, and its entry stayed `stopping` for good — every
/// later request for the model parked on it, with nothing left to wake it.
#[tokio::test]
async fn a_request_given_up_on_mid_eviction_leaves_no_entry_stopping() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    let go = f.podman.hold_waits();

    let gateway = f.gateway.clone();
    let embedding = tokio::spawn(async move { embed(&gateway).await.status() });
    until("the eviction of chat-model", || {
        entry(&f, "chat-model").is_some_and(|v| v.state == RuntimeState::Stopping)
    })
    .await;
    embedding.abort();
    let _ = embedding.await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    go.send_replace(true);

    until("the stopped entry to go", || {
        entry(&f, "chat-model").is_none()
    })
    .await;
    let resp = tokio::time::timeout(Duration::from_secs(10), chat(&f.gateway))
        .await
        .expect("a request for the stopped model must not park on a stale entry");
    assert_eq!(resp.status(), 200);
    assert_eq!(f.stops(), vec!["chat-model".to_string()]);
}

/// A start whose requester went away still tells the rest of lmgw what came
/// up — the new container's PID (§4.7), as a start with a requester does on
/// the request path.
#[tokio::test]
async fn a_start_given_up_on_still_has_its_pid_learned() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(0);
    f.world().health_held = true;
    let gateway = f.gateway.clone();
    let chatting = tokio::spawn(async move { chat(&gateway).await.status() });
    until("the chat's podman run", || {
        f.runs() == vec!["chat-model".to_string()]
    })
    .await;
    chatting.abort();
    let _ = chatting.await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let before = f.pid_inspects().len();

    f.world().health_held = false;
    until("the model ready and idle", || {
        ready_and_idle(&f, "chat-model")
    })
    .await;
    let name = entry(&f, "chat-model").unwrap().container_name;
    until("its PID asked for", || {
        f.pid_inspects()[before..]
            .iter()
            .any(|names| names.contains(&name))
    })
    .await;
}
