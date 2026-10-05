//! A stopped plain (no-tools) chat turn writes its request row with what
//! it cost so far (WP11 server review M2), as the stock realtime path's
//! cooperative stop does: status 200, `canceled`, the upstream's usage or
//! the ~4 characters a token estimate, named in the row's message — and on
//! a priced (cloud) alias a cost. Every way the binding stops the turn is
//! covered: the page's Stop of a text send, a cancel (a barge-in is one),
//! the page's truncate-first stop, leaving mid-reply, a takeover, and a
//! stop before the upstream answered (the prompt alone). The stopped turn
//! still says `done` at once — the journal's slot, the next response's
//! barrier and the drain wait for it — without waiting for the upstream's
//! next chunk, which never comes here.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use super::{eventually, of_type, say, until, until_type, world, World};
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::{send, Step, Turn, Ws};

type Row = (
    i64,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

/// A world whose `chatty` is priced, as a cloud model is.
async fn priced() -> World {
    let w = world(|_| {}).await;
    sqlx::query(
        "INSERT INTO prices (scope_kind, scope_key, price_in, price_out) VALUES ('alias', \
         'chatty', 1000.0, 1000.0)",
    )
    .execute(&w.state.db)
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    w
}

/// A reply that says one sentence, then never goes on.
fn endless(never: &Arc<Notify>) -> Turn {
    Turn::Stream(vec![
        Step::Text("Es war einmal ein König. "),
        Step::Wait(never.clone()),
        Step::Text("Er war alt."),
        Step::Finish("stop"),
    ])
}

/// The Chat's rows of `chatty`: status, error kind and message, prompt and
/// completion tokens, cost — once there are `n`.
async fn rows(w: &World, n: usize) -> Vec<Row> {
    for _ in 0..500 {
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT status, error_kind, error_msg, prompt_tokens, completion_tokens, cost_micro \
             FROM request_logs WHERE requested_alias = 'chatty' AND ingress_proto = 'chat' \
             ORDER BY id",
        )
        .fetch_all(&w.state.db)
        .await
        .unwrap();
        if rows.len() >= n {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("fewer than {n} chat rows for chatty");
}

/// The one row is a stop's, priced, with its output estimated from the
/// sentence that was streamed.
async fn one_stopped_row(w: &World) {
    let rows = rows(w, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (status, kind, msg, prompt, completion, cost) = rows[0].clone();
    assert_eq!(
        (status, kind.as_deref()),
        (200, Some("canceled")),
        "{rows:?}"
    );
    let msg = msg.unwrap_or_default();
    assert!(
        msg.contains("prompt and completion tokens estimated at ~4 characters a token"),
        "{msg}"
    );
    assert!(prompt.is_some_and(|p| p > 0), "{prompt:?}");
    // "Es war einmal ein König. " is 25 characters.
    assert_eq!(completion, Some(7), "{rows:?}");
    assert!(
        cost.is_some_and(|c| c > 0),
        "priced like any call: {cost:?}"
    );
}

/// A bound session on a new thread, a turn said, and the reply streaming:
/// the socket and the events so far.
async fn streaming(w: &World, never: &Arc<Notify>) -> (i64, Ws, Vec<Value>) {
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Erzähl was."));
    w.chat.push(endless(never));
    say(&mut ws).await;
    let events = until(&mut ws, |e| e["type"] == "response.output_audio.delta").await;
    (tid, ws, events)
}

#[tokio::test]
async fn a_text_send_the_page_stops_writes_its_priced_row() {
    let w = priced().await;
    let never = Arc::new(Notify::new());
    let tid = w.thread("chatty", json!({})).await;
    w.chat.push(endless(&never));
    let mut resp = w
        .post(
            &format!("/chat/api/threads/{tid}/send"),
            json!({"content": "Erzähl was."}),
        )
        .await;
    crate::chat_golden::read_until(&mut resp, "event: delta").await;
    drop(resp);
    one_stopped_row(&w).await;
}

#[tokio::test]
async fn a_cancelled_bound_turn_writes_its_priced_row_and_says_done_at_once() {
    let w = priced().await;
    let never = Arc::new(Notify::new());
    let (_tid, mut ws, _) = streaming(&w, &never).await;
    // A barge-in cancels the same way.
    send(&mut ws, json!({"type": "response.cancel"})).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let done = of_type(&events, "response.done");
    assert_eq!(done[0]["response"]["status"], "cancelled", "{events:?}");
    // The turn's `done` named its saved partial reply, with the upstream
    // still silent.
    let saved = of_type(&events, "lmgw.chat.frame")
        .into_iter()
        .find(|f| f["event"] == "done")
        .cloned()
        .unwrap_or_else(|| panic!("no done frame: {events:?}"));
    assert_eq!(saved["data"]["saved"], true, "{saved}");
    one_stopped_row(&w).await;
}

#[tokio::test]
async fn a_truncate_stop_writes_its_priced_row() {
    let w = priced().await;
    let never = Arc::new(Notify::new());
    let (_tid, mut ws, events) = streaming(&w, &never).await;
    let item = of_type(&events, "response.output_item.added")
        .iter()
        .find(|e| e["item"]["type"] == "message")
        .map(|e| e["item"]["id"].as_str().unwrap().to_string())
        .unwrap();
    // The page's stop: the truncate alone cancels a reply still produced.
    send(
        &mut ws,
        json!({"type": "conversation.item.truncate", "item_id": item, "content_index": 0,
               "audio_end_ms": 0}),
    )
    .await;
    until_type(&mut ws, "lmgw.response.timing").await;
    one_stopped_row(&w).await;
}

#[tokio::test]
async fn leaving_mid_reply_writes_its_priced_row() {
    let w = priced().await;
    let never = Arc::new(Notify::new());
    let (tid, ws, _) = streaming(&w, &never).await;
    drop(ws);
    one_stopped_row(&w).await;
    // The partial reply was saved, as for any stop.
    eventually("the partial reply saved", || async {
        w.messages(tid).await.len() == 2
    })
    .await;
}

#[tokio::test]
async fn a_takeover_mid_reply_writes_the_older_turn_s_priced_row() {
    let w = priced().await;
    let never = Arc::new(Notify::new());
    let (tid, _first, _) = streaming(&w, &never).await;
    let (_second, _) = w.bind(tid).await;
    one_stopped_row(&w).await;
}

#[tokio::test]
async fn a_stop_before_the_upstream_answered_counts_the_prompt() {
    let w = priced().await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    let prefill = Arc::new(Notify::new());
    w.asr.push(Asr::Text("Erzähl was."));
    w.chat
        .push(Turn::Held(prefill.clone(), Box::new(Turn::text(&["Hm."]))));
    say(&mut ws).await;
    until_type(&mut ws, "response.created").await;
    super::eventually("the request to go out", || async {
        w.chat.seen.chat_count() == 1
    })
    .await;
    send(&mut ws, json!({"type": "response.cancel"})).await;
    until_type(&mut ws, "response.done").await;
    let rows = rows(&w, 1).await;
    let (status, kind, msg, prompt, completion, cost) = rows[0].clone();
    assert_eq!(
        (status, kind.as_deref()),
        (200, Some("canceled")),
        "{rows:?}"
    );
    assert!(
        msg.unwrap_or_default()
            .contains("before the upstream answered"),
        "{rows:?}"
    );
    assert!(prompt.is_some_and(|p| p > 0), "{rows:?}");
    assert_eq!(completion, Some(0));
    assert!(
        cost.is_some_and(|c| c > 0),
        "the prompt is priced: {cost:?}"
    );
    prefill.notify_one();
}
