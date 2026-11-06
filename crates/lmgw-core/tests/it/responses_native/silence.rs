//! A native stream whose upstream falls silent for longer than the row's
//! timeout is ended by the relay (`proxy::silence`, as `stream_silence`
//! shows for the other relays): its upstream connection closed, the client
//! told with `response.failed`, the row a `timeout`. Once the answer shows,
//! keep-alive comments do not break a silence; inside a reasoning item or a
//! hosted tool's call they do. A steady stream is never cut, and a timeout of 0 cuts nothing. A
//! failure after half an event still reaches the client as an event of its
//! own, and an upstream that said `error` itself is not answered twice.
//!
//! Cut cases run at [`TIMEOUT_MS`]; the ones that must not be cut at
//! [`LONG_TIMEOUT_MS`], with gaps of at most a quarter of it, so a loaded
//! box does not cut them.

use std::time::Instant;

use super::*;

/// The row's `timeout_ms` where the test waits for a cut.
const TIMEOUT_MS: u64 = 300;

/// The row's `timeout_ms` where nothing may be cut.
const LONG_TIMEOUT_MS: u64 = 1_000;

/// The gap between an uncut upstream's frames.
const GAP: Duration = Duration::from_millis(250);

/// What the client and the row are told.
const SILENT: &str = "the upstream sent nothing for 300 ms mid-stream";

async fn open(gw: &Gw) -> reqwest::Response {
    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": ALIAS, "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp
}

/// The data of the stream's last event, named `name`.
fn last_event(text: &str, name: &str) -> Value {
    let events = lmgw_core::sse::SseDecoder::new().feed(text.as_bytes());
    let last = events.last().expect("an event");
    assert_eq!(last.event.as_deref(), Some(name), "{text}");
    serde_json::from_str(&last.data).unwrap()
}

/// The whole stream, which must end within a few seconds.
async fn read_ending(gw: &Gw) -> String {
    tokio::time::timeout(Duration::from_secs(5), open(gw).await.text())
        .await
        .expect("the stream ended")
        .unwrap()
}

#[tokio::test]
async fn a_stalled_stream_ends_with_response_failed() {
    for says in [Says::Holds, Says::HoldsAlive] {
        let (base, up) = upstream(says).await;
        let (state, gw) = gateway_timing_out(Protocol::Openai, &base, TIMEOUT_MS).await;
        let started = Instant::now();
        let text = read_ending(&gw).await;
        assert!(started.elapsed() >= Duration::from_millis(TIMEOUT_MS));
        assert!(text.contains("response.output_text.delta"), "{text}");
        let failed = last_event(&text, "response.failed");
        assert_eq!(failed["sequence_number"], 2, "{failed}");
        assert_eq!(failed["response"]["id"], "resp_upstream", "{failed}");
        assert_eq!(failed["response"]["status"], "failed");
        assert_eq!(failed["response"]["output"], json!([]), "{failed}");
        assert_eq!(
            failed["response"]["error"],
            json!({"code": "server_error", "message": SILENT})
        );
        let row = row(&state, 1).await;
        assert_eq!(
            (row.error_kind.as_deref(), row.error_msg.as_deref()),
            (Some("timeout"), Some(SILENT)),
            "{row:?}"
        );
        assert_eq!(row.status, 200, "{row:?}");
        patience::until("the upstream stream to close", || {
            up.closed.load(Ordering::SeqCst) == 1
        })
        .await;
    }
}

/// Half an event relayed, then a silence or a broken body: the client still
/// gets a `response.failed` it can parse, as an event of its own.
#[tokio::test]
async fn a_failure_after_half_an_event_is_an_event_of_its_own() {
    for (says, kind) in [
        (Says::HalfThenHolds, "timeout"),
        (Says::HalfThenBreaks, "transport"),
    ] {
        let (base, _up) = upstream(says).await;
        let (state, gw) = gateway_timing_out(Protocol::Openai, &base, TIMEOUT_MS).await;
        let text = read_ending(&gw).await;
        assert!(text.contains(HALF_EVENT), "{text}");
        let failed = last_event(&text, "response.failed");
        assert_eq!(failed["response"]["status"], "failed", "{text}");
        let row = row(&state, 1).await;
        assert_eq!(row.error_kind.as_deref(), Some(kind), "{row:?}");
    }
}

/// An upstream that sent its own `error` event and then fell silent: the
/// client's stream ends on that event, with no second failure after it, and
/// the row says what the upstream said.
#[tokio::test]
async fn an_upstream_error_event_is_not_answered_twice() {
    let (base, _up) = upstream(Says::ErrsThenHolds).await;
    let (state, gw) = gateway_timing_out(Protocol::Openai, &base, TIMEOUT_MS).await;
    let text = read_ending(&gw).await;
    let error = last_event(&text, "error");
    assert_eq!(error["message"], "overloaded", "{text}");
    assert!(!text.contains("response.failed"), "{text}");
    let row = row(&state, 1).await;
    assert_eq!(
        (row.error_kind.as_deref(), row.error_msg.as_deref()),
        (Some("upstream"), Some("overloaded")),
        "{row:?}"
    );
}

/// Deltas a quarter of the timeout apart, more than three timeouts in all:
/// relayed to `response.completed`, the row whole.
#[tokio::test]
async fn a_steady_stream_is_not_cut() {
    let (base, _up) = upstream(Says::Trickles(GAP)).await;
    let (state, gw) = gateway_timing_out(Protocol::Openai, &base, LONG_TIMEOUT_MS).await;
    let started = Instant::now();
    let text = open(&gw).await.text().await.unwrap();
    assert!(started.elapsed() > Duration::from_millis(LONG_TIMEOUT_MS * 3));
    last_event(&text, "response.completed");
    let row = row(&state, 1).await;
    assert_eq!(row.error_kind, None, "{row:?}");
}

/// A reasoning item open, and only keep-alive comments for more than three
/// timeouts, after the answer had already shown: the model thinks out of
/// sight, so the comments keep the stream alive to its end.
#[tokio::test]
async fn keep_alives_inside_a_reasoning_item_are_not_silence() {
    let (base, _up) = upstream(Says::ReasonsHidden(GAP)).await;
    let (state, gw) = gateway_timing_out(Protocol::Openai, &base, LONG_TIMEOUT_MS).await;
    let started = Instant::now();
    let text = open(&gw).await.text().await.unwrap();
    assert!(started.elapsed() > Duration::from_millis(LONG_TIMEOUT_MS * 3));
    assert!(text.contains("response.output_item.done"), "{text}");
    last_event(&text, "response.completed");
    let row = row(&state, 1).await;
    assert_eq!(row.error_kind, None, "{row:?}");
}

/// A hosted `web_search_call` added after the answer had shown, and only
/// keep-alive comments for more than three timeouts until it is done: the
/// search runs out of sight, so the comments keep the stream alive to its
/// end.
#[tokio::test]
async fn keep_alives_while_a_hosted_tool_runs_are_not_silence() {
    let (base, _up) = upstream(Says::SearchesHidden(GAP)).await;
    let (state, gw) = gateway_timing_out(Protocol::Openai, &base, LONG_TIMEOUT_MS).await;
    let started = Instant::now();
    let text = open(&gw).await.text().await.unwrap();
    assert!(started.elapsed() > Duration::from_millis(LONG_TIMEOUT_MS * 3));
    assert!(text.contains("web_search_call"), "{text}");
    last_event(&text, "response.completed");
    let row = row(&state, 1).await;
    assert_eq!(row.error_kind, None, "{row:?}");
}

/// A row whose timeout is 0 bounds no silence.
#[tokio::test]
async fn a_timeout_of_zero_cuts_nothing() {
    let (base, up) = upstream(Says::HoldsAlive).await;
    let (_state, gw) = gateway_timing_out(Protocol::Openai, &base, 0).await;
    let read = tokio::time::timeout(
        Duration::from_millis(TIMEOUT_MS * 4),
        open(&gw).await.text(),
    )
    .await;
    assert!(read.is_err(), "the stream ended: {read:?}");
    assert_eq!(up.closed.load(Ordering::SeqCst), 0);
}
