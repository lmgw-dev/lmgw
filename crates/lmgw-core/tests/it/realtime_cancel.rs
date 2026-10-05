//! Cancelling a response over a real socket (realtime design §4.3 "Cancel",
//! §11): cooperative, so the model call still writes its row and counts;
//! the items close `incomplete` and keep their text; a cancel after the end
//! of generation in text mode, or with nothing active; a hang-up mid-answer;
//! a stream that fails after its finish; and a cancel before the request
//! went out, which bills no prompt.

use std::sync::Arc;
use std::time::Duration;

use lmgw_core::config::KeyPolicy;
use lmgw_core::state::SharedState;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::support::gpu_world::{Gpu, GIB};
use crate::support::realtime_fakes::{
    chat_fake, events_until, gateway, gpu_gateway, next_event, open, send, text_session, types,
    user_text, Step, Turn, Ws, KEY,
};

type Row = (
    i64,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

/// The `request_logs` rows of `chatty`: status, error kind and message,
/// prompt and completion tokens, cost — once there are `n`.
async fn rows(state: &SharedState, n: usize) -> Vec<Row> {
    for _ in 0..500 {
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT status, error_kind, error_msg, prompt_tokens, completion_tokens, cost_micro \
             FROM request_logs WHERE requested_alias = 'chatty' ORDER BY id",
        )
        .fetch_all(&state.db)
        .await
        .unwrap();
        if rows.len() >= n {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("fewer than {n} rows for chatty");
}

/// Wait for the in-flight gauge to read 0: the row is inserted before the
/// recorder closes the gauge (`request_finished`), so a visible row does not
/// mean a settled gauge yet (WP1c review #9).
async fn gauge_settles(state: &SharedState) {
    for _ in 0..500 {
        if state.telemetry.stats().active_requests == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "the in-flight gauge still reads {}",
        state.telemetry.stats().active_requests
    );
}

/// Every event up to the answer to a retrieve of a missing item: what the
/// client sent before it has been handled.
async fn until_sentinel(ws: &mut Ws) -> Vec<Value> {
    send(
        ws,
        json!({"type": "conversation.item.retrieve", "item_id": "nope", "event_id": "sentinel"}),
    )
    .await;
    let mut out = Vec::new();
    loop {
        let ev = next_event(ws).await;
        if ev["error"]["event_id"] == "sentinel" {
            return out;
        }
        out.push(ev);
    }
}

/// A stream that says `a`, `b` and then waits for a release that never
/// comes — or for the gateway to hang up.
fn stalls_after_ab() -> (Turn, Arc<Notify>) {
    let never = Arc::new(Notify::new());
    let turn = Turn::Stream(vec![
        Step::Text("a"),
        Step::Text("b"),
        Step::Wait(never.clone()),
        Step::Text("c"),
        Step::Finish("stop"),
    ]);
    (turn, never)
}

#[tokio::test]
async fn a_cancel_closes_the_items_incomplete_and_keeps_what_the_client_got() {
    let fake = chat_fake().await;
    let (turn, _never) = stalls_after_ab();
    fake.push(turn);
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    send(&mut ws, user_text("Erzähl was")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let mut got = events_until(&mut ws, "response.output_text.delta").await;
    got.extend(events_until(&mut ws, "response.output_text.delta").await);

    send(
        &mut ws,
        json!({"type": "response.cancel", "event_id": "c1"}),
    )
    .await;
    let ev = events_until(&mut ws, "response.done").await;
    assert_eq!(
        types(&ev),
        [
            "response.output_item.done",
            "conversation.item.done",
            "response.done"
        ]
    );
    let item = &ev[0]["item"];
    assert_eq!(item["status"], "incomplete");
    assert_eq!(ev[1]["item"], *item);
    // No text was taken back: the item is what the client was sent.
    let sent: String = got
        .iter()
        .filter(|e| e["type"] == "response.output_text.delta")
        .map(|e| e["delta"].as_str().unwrap())
        .collect();
    assert_eq!(sent, "ab");
    assert_eq!(item["content"][0]["text"], sent);
    let r = &ev[2]["response"];
    assert_eq!(r["status"], "cancelled");
    assert_eq!(
        r["status_details"],
        json!({"type": "cancelled", "reason": "client_cancelled"})
    );
    assert_eq!(r["output"][0], *item);
    assert_eq!(r["usage"], Value::Null, "the upstream reported none yet");

    // The model call was stopped, not left to run.
    tokio::time::timeout(Duration::from_secs(5), fake.seen.closed.notified())
        .await
        .expect("the upstream stream was not closed by the cancel");
    // Nothing of the cancelled response follows, and the next one renders
    // what the client got.
    assert_eq!(until_sentinel(&mut ws).await, Vec::<Value>::new());
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(events[0]["type"], "response.created", "{events:?}");
    // (After the user's turn; a text session has no default voice
    // instructions.)
    assert_eq!(
        fake.seen.chat(1)["messages"][1],
        json!({"role": "assistant", "content": "ab"})
    );
}

#[tokio::test]
async fn a_cancelled_call_writes_one_row_with_its_usage_so_far_and_it_counts() {
    let fake = chat_fake().await;
    let never = Arc::new(Notify::new());
    fake.push(Turn::Stream(vec![
        Step::Text("Es war einmal "),
        Step::Text("ein König"),
        Step::Wait(never.clone()),
        Step::Finish("stop"),
        Step::Usage(50, 30),
    ]));
    // One token a minute: the cancelled call's tokens use it up.
    let (state, addr) = gateway(
        &fake,
        true,
        Some(KeyPolicy {
            tpm_limit: 1,
            ..Default::default()
        }),
        |_| {},
    )
    .await;
    sqlx::query(
        "INSERT INTO prices (scope_kind, scope_key, price_in, price_out) VALUES ('alias', \
         'chatty', 1000.0, 1000.0)",
    )
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let bearer = format!("Bearer {KEY}");
    let mut ws = open(
        &addr,
        "/v1/realtime?model=chatty",
        &[("authorization", &bearer)],
    )
    .await;
    next_event(&mut ws).await;
    send(
        &mut ws,
        json!({"type": "session.update",
               "session": {"type": "realtime", "output_modalities": ["text"]}}),
    )
    .await;
    next_event(&mut ws).await;
    send(&mut ws, user_text("Erzähl was")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.output_text.delta").await;
    events_until(&mut ws, "response.output_text.delta").await;
    send(&mut ws, json!({"type": "response.cancel"})).await;
    let done = events_until(&mut ws, "response.done").await;
    assert_eq!(done.last().unwrap()["response"]["status"], "cancelled");

    // Exactly one row, `canceled`, with the tokens estimated and said so.
    let rows = rows(&state, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (status, kind, msg, prompt, completion, cost) = rows[0].clone();
    assert_eq!(status, 200);
    assert_eq!(kind.as_deref(), Some("canceled"));
    let msg = msg.unwrap();
    assert!(
        msg.contains("prompt and completion tokens estimated at ~4 characters a token"),
        "{msg}"
    );
    // "Es war einmal ein König" is 23 characters.
    assert_eq!(completion, Some(6));
    assert!(prompt.is_some_and(|p| p > 0), "{prompt:?}");
    assert!(
        cost.is_some_and(|c| c > 0),
        "priced like any call: {cost:?}"
    );
    gauge_settles(&state).await;

    // The tokens were counted against the key: the next call is over its
    // tokens-per-minute.
    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "r2"}),
    )
    .await;
    let events = events_until(&mut ws, "response.done").await;
    let error = events
        .iter()
        .find(|e| e["type"] == "error")
        .unwrap_or_else(|| panic!("{events:?}"));
    assert_eq!(error["error"]["code"], "key_rate", "{error}");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("tokens/minute"));
    assert_eq!(fake.seen.chat_count(), 1);
}

#[tokio::test]
async fn a_hang_up_mid_answer_stops_the_call_and_it_still_writes_its_row() {
    let fake = chat_fake().await;
    let (turn, _never) = stalls_after_ab();
    fake.push(turn);
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.output_text.delta").await;
    drop(ws);

    tokio::time::timeout(Duration::from_secs(5), fake.seen.closed.notified())
        .await
        .expect("the upstream stream was not closed by the hang-up");
    let rows = rows(&state, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].1.as_deref(), Some("canceled"));
    assert!(rows[0].4.is_some_and(|c| c >= 1), "{rows:?}");
    gauge_settles(&state).await;
}

#[tokio::test]
async fn a_cancel_after_the_end_of_generation_finishes_the_text_response_as_completed() {
    let fake = chat_fake().await;
    let release = Arc::new(Notify::new());
    fake.push(Turn::Stream(vec![
        Step::Text("Fertig."),
        Step::Finish("stop"),
        // Generation is over; the stream's end is not.
        Step::Wait(release.clone()),
        Step::Usage(3, 2),
    ]));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let ev = events_until(&mut ws, "conversation.item.done").await;
    assert_eq!(ev.last().unwrap()["item"]["status"], "completed");

    send(
        &mut ws,
        json!({"type": "response.cancel", "event_id": "late"}),
    )
    .await;
    assert_eq!(
        until_sentinel(&mut ws).await,
        Vec::<Value>::new(),
        "nothing to cancel, and nothing refused"
    );
    release.notify_one();
    let ev = events_until(&mut ws, "response.done").await;
    assert_eq!(types(&ev), ["response.done"]);
    let r = &ev[0]["response"];
    assert_eq!(r["status"], "completed");
    assert_eq!(r["usage"]["total_tokens"], 5);
    // The call ran to its end: an ordinary row.
    let rows = rows(&state, 1).await;
    assert_eq!((rows[0].0, rows[0].1.clone()), (200, None));
}

#[tokio::test]
async fn a_cancel_with_nothing_active_is_an_error_echoing_its_event_id() {
    let fake = chat_fake().await;
    let release = Arc::new(Notify::new());
    fake.push(Turn::Stream(vec![
        Step::Text("eins"),
        Step::Wait(release.clone()),
        Step::Finish("stop"),
    ]));
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    send(
        &mut ws,
        json!({"type": "response.cancel", "event_id": "c0"}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["type"], "error");
    assert_eq!(e["error"]["code"], "response_cancel_not_active");
    assert_eq!(e["error"]["event_id"], "c0");

    // Naming a response that is not the active one is the same answer, and
    // leaves the active one alone.
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.output_text.delta").await;
    send(
        &mut ws,
        json!({"type": "response.cancel", "event_id": "c1", "response_id": "resp_old"}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["error"]["code"], "response_cancel_not_active", "{e}");
    assert_eq!(e["error"]["event_id"], "c1");
    release.notify_one();
    let done = events_until(&mut ws, "response.done").await;
    assert_eq!(done.last().unwrap()["response"]["status"], "completed");
}

#[tokio::test]
async fn a_stream_that_fails_after_its_finish_completes_the_response_and_logs_the_failure() {
    let fake = chat_fake().await;
    let go = Arc::new(Notify::new());
    fake.push(Turn::Stream(vec![
        Step::CallStart {
            index: 0,
            id: Some("call_t"),
            name: "f",
        },
        Step::CallArgs {
            index: 0,
            args: "{}",
        },
        Step::Finish("tool_calls"),
        Step::Wait(go.clone()),
        Step::Fail,
    ]));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    // The call was announced completed — a client may already be running it.
    let ev = events_until(&mut ws, "conversation.item.done").await;
    assert_eq!(ev.last().unwrap()["item"]["status"], "completed");
    go.notify_one();

    let ev = events_until(&mut ws, "response.done").await;
    assert_eq!(types(&ev), ["response.done"], "no error event: {ev:?}");
    let r = &ev[0]["response"];
    assert_eq!(r["status"], "completed");
    assert_eq!(r["output"][0]["status"], "completed");
    // The failure is the row's (and the log's).
    let rows = rows(&state, 1).await;
    assert_eq!(rows[0].0, 502);
    assert_eq!(rows[0].1.as_deref(), Some("transport"));
}

#[tokio::test]
async fn a_cancel_before_the_upstream_answered_is_a_200_canceled_row_counting_its_prompt() {
    // WP1c review #5: the request went out, and the cancel came while the
    // upstream had not answered yet — a row like a mid-stream cancel's.
    let fake = chat_fake().await;
    let never = Arc::new(Notify::new());
    fake.push(Turn::Held(never.clone(), Box::new(Turn::text(&["late"]))));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    send(&mut ws, user_text("Erzähl was")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.created").await;
    for _ in 0..500 {
        if fake.seen.chat_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        fake.seen.chat_count(),
        1,
        "the request is with the upstream"
    );
    send(&mut ws, json!({"type": "response.cancel"})).await;
    let done = events_until(&mut ws, "response.done").await;
    assert_eq!(done.last().unwrap()["response"]["status"], "cancelled");

    let rows = rows(&state, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (status, kind, msg, prompt, completion, _) = rows[0].clone();
    assert_eq!(status, 200);
    assert_eq!(kind.as_deref(), Some("canceled"));
    let msg = msg.unwrap();
    assert!(msg.contains("prompt tokens estimated"), "{msg}");
    assert!(prompt.is_some_and(|p| p > 0), "{prompt:?}");
    assert_eq!(completion, Some(0));
    gauge_settles(&state).await;
}

#[tokio::test]
async fn a_cancel_before_the_request_went_out_bills_no_prompt() {
    // Package A review #3: the prompt counted as sent as soon as the send
    // was set up, so a stop while it still waited — here for the candidate
    // a re-pick moved it to, still starting — billed an estimated prompt
    // the upstream never got.
    use lmgw_core::config::{HoldFallbackMode, LlamaParams};
    use lmgw_core::runtime::registry::RuntimeState;
    use lmgw_core::store::{NewCandidateAlias, NewLocalModel};
    let g = Gpu::new(24 * GIB, 3, 5).await;
    // `p` serves one request of at most 64 tokens from a guarded pool.
    g.row(
        NewLocalModel {
            model_id: "p".into(),
            gguf_path: "p.gguf".into(),
            params: LlamaParams {
                ctx_size: Some(64),
                parallel: Some(2),
                kv_unified: Some(true),
                n_predict: Some(16),
                ..Default::default()
            },
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            ladder: vec![],
        },
        4 * GIB,
    )
    .await;
    g.model("a", 4 * GIB).await;
    g.candidate(NewCandidateAlias {
        alias: "jobs".into(),
        candidates: vec!["p".into(), "a".into()],
        background: true,
        fallback_mode: HoldFallbackMode::None,
        fallback: None,
        capabilities_disabled: vec![],
        capabilities_enabled: vec![],
        enabled: true,
        notes: String::new(),
    })
    .await;
    let addr = gpu_gateway(&g, |_| {}).await;
    drop(
        lmgw_core::vram::admit(&g.state, &g.route("p"), "p")
            .await
            .unwrap(),
    );
    // A prompt over `p`'s limit: the fit finds it out before anything is
    // sent, and the guest is picked again — onto `a`, whose start waits at
    // the run gate.
    g.world().prompt_tokens = 100;
    let open_runs = g.gate_runs();
    let starting = tokio::spawn({
        let (state, route) = (g.state.clone(), g.route("a"));
        async move { lmgw_core::vram::admit(&state, &route, "a").await.is_ok() }
    });
    let a_starting = || {
        g.state
            .runtime()
            .list()
            .iter()
            .any(|v| v.model_id == "a" && v.state == RuntimeState::Starting)
    };
    for _ in 0..500 {
        if a_starting() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(a_starting(), "the alternate's start is held");

    let mut ws = open(&addr, "/v1/realtime?model=jobs", &[]).await;
    next_event(&mut ws).await;
    send(
        &mut ws,
        json!({"type": "session.update",
               "session": {"type": "realtime", "output_modalities": ["text"]}}),
    )
    .await;
    next_event(&mut ws).await;
    send(&mut ws, user_text("Erzähl was")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.created").await;
    // The call is fitted on `p`, re-picked, and waits for `a`'s start.
    tokio::time::sleep(Duration::from_millis(300)).await;
    send(&mut ws, json!({"type": "response.cancel"})).await;
    let done = events_until(&mut ws, "response.done").await;
    assert_eq!(done.last().unwrap()["response"]["status"], "cancelled");

    // The call's one row: `canceled` on the candidate it was fitted on,
    // with no prompt — nothing had gone out.
    let mut row = None;
    for _ in 0..500 {
        row = sqlx::query_as::<_, (i64, Option<String>, Option<String>, Option<i64>)>(
            "SELECT status, error_kind, error_msg, prompt_tokens FROM request_logs WHERE \
             requested_alias = 'jobs' AND ingress_proto = 'realtime'",
        )
        .fetch_optional(&g.state.db)
        .await
        .unwrap();
        if row.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let row = row.expect("the call wrote its row");
    assert_eq!(row.0, 200);
    assert_eq!(row.1.as_deref(), Some("canceled"));
    let msg = row.2.unwrap();
    assert!(!msg.contains("estimated"), "{msg}");
    assert_eq!(row.3, None, "no prompt went out");
    assert!(g.world().chats.is_empty(), "no model saw the request");

    open_runs.send(true).unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(10), starting)
        .await
        .expect("the start ended")
        .unwrap());
}

#[tokio::test]
async fn a_cancel_after_a_held_back_refusal_bills_no_prompt() {
    // A2 review 2: the request went out to `p`, whose context refusal the
    // guest's send holds back, and the gate picks again — onto `a`, still
    // starting. A stop there bills no prompt: `p` ran nothing, and `a` has
    // not been sent anything yet.
    let g = Gpu::new(24 * GIB, 3, 5).await;
    g.model("p", 4 * GIB).await;
    g.model("a", 4 * GIB).await;
    g.background_alias("jobs", &["p", "a"]).await;
    let addr = gpu_gateway(&g, |_| {}).await;
    drop(
        lmgw_core::vram::admit(&g.state, &g.route("p"), "p")
            .await
            .unwrap(),
    );
    g.world().refuse_context.insert("p".into());
    let open_runs = g.gate_runs();
    let starting = tokio::spawn({
        let (state, route) = (g.state.clone(), g.route("a"));
        async move { lmgw_core::vram::admit(&state, &route, "a").await.is_ok() }
    });
    let a_starting = || {
        g.state.runtime().list().iter().any(|v| {
            v.model_id == "a" && v.state == lmgw_core::runtime::registry::RuntimeState::Starting
        })
    };
    for _ in 0..500 {
        if a_starting() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(a_starting(), "the alternate's start is held");

    let mut ws = open(&addr, "/v1/realtime?model=jobs", &[]).await;
    next_event(&mut ws).await;
    send(
        &mut ws,
        json!({"type": "session.update",
               "session": {"type": "realtime", "output_modalities": ["text"]}}),
    )
    .await;
    next_event(&mut ws).await;
    send(&mut ws, user_text("Erzähl was")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.created").await;
    // Sent to `p`, refused, re-picked, and waiting for `a`'s start.
    tokio::time::sleep(Duration::from_millis(300)).await;
    send(&mut ws, json!({"type": "response.cancel"})).await;
    let done = events_until(&mut ws, "response.done").await;
    assert_eq!(done.last().unwrap()["response"]["status"], "cancelled");

    let mut row = None;
    for _ in 0..500 {
        row = sqlx::query_as::<_, (i64, Option<String>, Option<String>, Option<i64>)>(
            "SELECT status, error_kind, error_msg, prompt_tokens FROM request_logs WHERE \
             requested_alias = 'jobs' AND ingress_proto = 'realtime'",
        )
        .fetch_optional(&g.state.db)
        .await
        .unwrap();
        if row.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let row = row.expect("the call wrote its row");
    assert_eq!(row.0, 200);
    assert_eq!(row.1.as_deref(), Some("canceled"));
    let msg = row.2.unwrap();
    assert!(!msg.contains("estimated"), "{msg}");
    assert_eq!(row.3, None, "no prompt is being worked on");
    assert!(g.world().chats.is_empty(), "no model answered the request");

    open_runs.send(true).unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(10), starting)
        .await
        .expect("the start ended")
        .unwrap());
}
