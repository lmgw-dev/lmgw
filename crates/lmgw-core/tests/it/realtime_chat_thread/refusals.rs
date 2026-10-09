//! A chat turn's refusals keep their codes (WP11 server review M1): the GPU
//! hold, a benchmark's lease, a context overflow and a VRAM queue timeout
//! reach the page as `error {code}` on a text send — the hold and the
//! lease after a `held` state for the chat stage, as §4.3 promises, though
//! they refuse at the route's resolve, before any admission — and a bound
//! response fails with the code, type and message a stock `/v1/realtime`
//! session sends for the same refusal (not `upstream`, and not the
//! `permission_error` a generic refusal would be typed as).

use serde_json::{json, Value};

use super::{manual_update, of_type, say, until_type, world_on, World};
use crate::chat_voice_dictation::{resident, tweak};
use crate::support::gpu_world::{Gpu, GIB};
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::{next_event, send, user_text, Turn};

/// A GPU world of `total` bytes, a queue timeout of `queue_s`, with the
/// harness's fakes on its state and no warm on a stock session's connect.
async fn gpu(total: u64, queue_s: u64) -> (Gpu, World) {
    let g = Gpu::new(total, 3, queue_s).await;
    let w = world_on(g.state.clone(), |s| s.realtime.warm_on_connect = false).await;
    (g, w)
}

/// The SSE events of a text send on thread `tid`: `(event, data)`.
async fn send_text(w: &World, tid: i64) -> Vec<(String, Value)> {
    let r = w
        .post(
            &format!("/chat/api/threads/{tid}/send"),
            json!({"content": "Hallo?"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    body.split("\n\n")
        .filter_map(|block| {
            let event = block.lines().find_map(|l| l.strip_prefix("event: "))?;
            let data = block.lines().find_map(|l| l.strip_prefix("data: "))?;
            Some((event.to_string(), serde_json::from_str(data).unwrap()))
        })
        .collect()
}

/// A bound response on thread `tid`: every event up to its
/// `response.done`.
async fn bound_turn(w: &World, tid: i64) -> Vec<Value> {
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Hallo?"));
    say(&mut ws).await;
    until_type(&mut ws, "response.done").await
}

/// [`bound_turn`] once the session's connect warm of its chat model has
/// ended (its last `lmgw.model.state` for the chat stage is no longer
/// `loading`).
///
/// A stock session here warms nothing on connect; a bound one warms its
/// thread's model through the request admission, and when that model has
/// to wait for room the warm holds the admission gate for the whole queue
/// timeout. A response launched meanwhile waited at the gate and, by tens
/// of milliseconds under load, ran out of time there ("another request was
/// still being admitted") rather than where the stock session's does
/// ("held by …"). After the warm, the response is admitted alone, as the
/// stock one is.
async fn bound_turn_after_warm(w: &World, tid: i64) -> Vec<Value> {
    let (mut ws, _) = w.bind(tid).await;
    send(&mut ws, manual_update(60_000)).await;
    let (mut updated, mut chat) = (false, None);
    while !updated || chat.as_ref().is_none_or(|s| *s == "loading") {
        let ev = next_event(&mut ws).await;
        match ev["type"].as_str() {
            Some("session.updated") => updated = true,
            Some("lmgw.model.state") if ev["stage"] == "chat" => chat = Some(ev["state"].clone()),
            _ => {}
        }
    }
    w.asr.push(Asr::Text("Hallo?"));
    say(&mut ws).await;
    until_type(&mut ws, "response.done").await
}

/// The `error` object a stock session on `model` fails its response with.
async fn stock_error(w: &World, model: &str) -> Value {
    let bearer = format!("Bearer {}", w.gw.key);
    let mut ws = w
        .connect(&format!("model={model}"), &[("authorization", &bearer)])
        .await
        .unwrap_or_else(|(s, b)| panic!("a stock session: {s} {b}"));
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    send(
        &mut ws,
        json!({"type": "session.update",
               "session": {"type": "realtime", "output_modalities": ["text"]}}),
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
    send(&mut ws, user_text("Hallo?")).await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = until_type(&mut ws, "response.done").await;
    let e = of_type(&events, "error");
    assert_eq!(e.len(), 1, "{events:?}");
    e[0]["error"].clone()
}

/// The bound response's error, and that it is the stock session's.
async fn same_as_stock(w: &World, events: &[Value], model: &str, code: &str) {
    let e = of_type(events, "error");
    assert_eq!(e.len(), 1, "{events:?}");
    let bound = &e[0]["error"];
    assert_eq!(bound["code"], code, "{bound}");
    let stock = stock_error(w, model).await;
    for key in ["type", "code", "message"] {
        assert_eq!(bound[key], stock[key], "{key}: bound {bound} stock {stock}");
    }
    let done = events.last().unwrap();
    assert_eq!(done["response"]["status"], "failed", "{done}");
    assert_eq!(done["response"]["status_details"]["error"]["code"], code);
}

/// The chat stage's `state` frames among `frames`.
fn chat_states(frames: &[(String, Value)]) -> Vec<Value> {
    frames
        .iter()
        .filter(|(e, d)| e == "state" && d["stage"] == "chat")
        .map(|(_, d)| d.clone())
        .collect()
}

/// A text send's last frames: the `error` with `code`, then `done
/// {aborted}`.
fn ends_refused(frames: &[(String, Value)], code: &str) {
    let n = frames.len();
    assert!(n >= 2, "{frames:?}");
    assert_eq!(frames[n - 2].0, "error", "{frames:?}");
    assert_eq!(frames[n - 2].1["code"], code, "{frames:?}");
    assert_eq!(frames[n - 1].0, "done", "{frames:?}");
    assert_eq!(frames[n - 1].1["aborted"], true, "{frames:?}");
}

#[tokio::test]
async fn under_the_hold_a_chat_turn_is_held_and_refused_with_the_stock_code() {
    let (g, w) = gpu(10 * GIB, 2).await;
    g.model("talk", 2 * GIB).await;
    tweak(&g.state, |s| s.hold.active = true).await;

    // A text send, plain and with tools: `held`, then the refusal.
    let tid = w.thread("talk", json!({})).await;
    let frames = send_text(&w, tid).await;
    let held = chat_states(&frames);
    assert_eq!(held.len(), 1, "{frames:?}");
    assert_eq!(
        (&held[0]["state"], &held[0]["cause"]),
        (&json!("held"), &json!("gpu_hold")),
        "{held:?}"
    );
    ends_refused(&frames, "gpu_hold");
    let stub = crate::chat_golden::mcp_stub(std::time::Duration::ZERO).await;
    crate::chat_golden::register_stub(&g.state, &stub).await;
    let tools = w.thread("talk", json!({})).await;
    w.set(tools, json!({"mcp_tools": [{"server_label": "stub"}]}))
        .await;
    let frames = send_text(&w, tools).await;
    assert_eq!(chat_states(&frames)[0]["state"], "held", "{frames:?}");
    ends_refused(&frames, "gpu_hold");

    // A bound response: the same `held`, as the session's model state, and
    // the stock session's error.
    let events = bound_turn(&w, tid).await;
    let states: Vec<&Value> = of_type(&events, "lmgw.model.state")
        .into_iter()
        .filter(|s| s["stage"] == "chat" && s["state"] == "held")
        .collect();
    assert!(!states.is_empty(), "{events:?}");
    assert_eq!(states[0]["cause"], "gpu_hold");
    let frame = of_type(&events, "lmgw.chat.frame")
        .into_iter()
        .find(|f| f["event"] == "error")
        .cloned()
        .unwrap_or_else(|| panic!("no error frame: {events:?}"));
    assert_eq!(frame["data"]["code"], "gpu_hold", "{frame}");
    same_as_stock(&w, &events, "talk", "gpu_hold").await;
    assert!(g.runs().is_empty(), "nothing started: {:?}", g.runs());
    assert_eq!(w.chat.seen.chat_count(), 0);
}

#[tokio::test]
async fn under_a_benchmark_a_chat_turn_is_held_and_refused_with_the_stock_code() {
    let (g, w) = gpu(10 * GIB, 2).await;
    g.model("talk", 2 * GIB).await;
    g.state
        .set_gpu_lease(Some(lmgw_core::bench::lease::lease(7, "qwen")));
    let tid = w.thread("talk", json!({})).await;
    let frames = send_text(&w, tid).await;
    let held = chat_states(&frames);
    assert_eq!(
        (&held[0]["state"], &held[0]["cause"]),
        (&json!("held"), &json!("benchmark")),
        "{frames:?}"
    );
    ends_refused(&frames, "gpu_benchmark");
    let events = bound_turn(&w, tid).await;
    same_as_stock(&w, &events, "talk", "gpu_benchmark").await;
    g.state.set_gpu_lease(None);
}

#[tokio::test]
async fn a_context_overflow_is_refused_with_the_stock_code() {
    let (_g, w) = gpu(10 * GIB, 2).await;
    // llama-server's own refusal, which the gateway maps to the stable code.
    let overflow = || {
        Turn::Status(
            400,
            json!({"error": {"code": 400, "type": "exceed_context_size_error",
                             "message": "the request exceeds the available context size",
                             "n_prompt_tokens": 5000, "n_ctx": 4096}}),
        )
    };
    let tid = w.thread("chatty", json!({})).await;
    w.chat.push(overflow());
    let frames = send_text(&w, tid).await;
    assert!(chat_states(&frames).is_empty(), "a cloud route says none");
    ends_refused(&frames, "context_length_exceeded");
    w.chat.push(overflow());
    let events = bound_turn(&w, tid).await;
    w.chat.push(overflow());
    same_as_stock(&w, &events, "chatty", "context_length_exceeded").await;
    // A request error, as on the stock path — not the `permission_error` a
    // generic refusal is typed as.
    assert_eq!(
        of_type(&events, "error")[0]["error"]["type"],
        "invalid_request_error"
    );
}

#[tokio::test]
async fn a_vram_queue_timeout_is_refused_with_the_stock_code() {
    let (g, w) = gpu(10 * GIB, 1).await;
    g.model("other", 8 * GIB).await;
    g.model("talk", 6 * GIB).await;
    // Up and busy on its own port: never evicted, so `talk` waits out the
    // queue timeout.
    resident(&g, "other").await;
    g.world().busy.insert("other".into());
    let tid = w.thread("talk", json!({})).await;
    let frames = send_text(&w, tid).await;
    let states: Vec<Value> = chat_states(&frames)
        .iter()
        .map(|s| s["state"].clone())
        .collect();
    assert_eq!(states, [json!("loading"), json!("failed")], "{frames:?}");
    ends_refused(&frames, "vram_queue_timeout");
    let events = bound_turn_after_warm(&w, tid).await;
    same_as_stock(&w, &events, "talk", "vram_queue_timeout").await;
    assert_eq!(g.stops(), Vec::<String>::new(), "the busy model stays");
}
