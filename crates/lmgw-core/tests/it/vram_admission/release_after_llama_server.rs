//! Release only once llama-server has let go (second review, finding 6)

use super::*;

/// A stream the client hung up on keeps its reservation — marked `releasing`
/// — for as long as the container's `/slots` still shows a slot processing:
/// llama-server only notices a closed connection on its next poll, and until
/// then the cells are in use. The next waiter is granted only once `/slots`
/// reads idle.
#[tokio::test]
async fn a_hung_up_stream_keeps_its_cells_until_the_slot_is_idle() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;
    let port = warm_pool(&f, 64, Duration::from_millis(200)).await;
    set_queue_timeout(&f, 0).await;

    set_slot_busy(&f, true);
    stream_and_hang_up(&f, &words("a", 9), 30).await;
    until_pool(&f, "A releasing", |p| {
        p["releasing"] == 1 && p["in_flight"] == 0 && p["reserved_tokens"] == 40
    })
    .await;

    // 1 + 29 + 30 = 60: fits only once A's 40 is back.
    let b = {
        let gw = f.gateway.clone();
        tokio::spawn(async move { sized_chat(&gw, &words("b", 29), 30).await.status().as_u16() })
    };
    until_pool(&f, "B queued", |p| queued(p) == 1).await;
    // Not a sleep: wait until the deferred release has read the busy slot
    // three more times, and B is still waiting behind it.
    let looked = f.world().slots_calls.get(&port).copied().unwrap_or(0);
    for _ in 0..500 {
        if f.world().slots_calls.get(&port).copied().unwrap_or(0) >= looked + 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(f.world().slots_calls[&port] >= looked + 3);
    let v = vram_status(&f.gateway).await;
    assert_eq!(pools(&v)[0]["releasing"], 1, "{v}");
    assert_eq!(queued(&pools(&v)[0]), 1, "B still waits: {v}");

    set_slot_busy(&f, false);
    assert_eq!(b.await.unwrap(), 200, "granted once the slot let go");
    assert_eq!(f.world().pool_overflows, 0);
    until_pools_empty(&f).await;
}

/// The deferred release is bounded: a slot that never reads idle keeps the
/// reservation for `vram.queue_timeout_seconds` at most, then lets it go (and
/// says so in the log) — never a reservation stranded forever.
#[tokio::test]
async fn a_release_that_never_sees_the_slot_idle_ends_at_its_bound() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;
    assert_eq!(sized_chat(&f.gateway, "hi", 8).await.status(), 200);
    set_queue_timeout(&f, 1).await;

    set_slot_busy(&f, true);
    stream_and_hang_up(&f, &words("a", 9), 30).await;
    until_pool(&f, "A releasing", |p| p["releasing"] == 1).await;
    until_pools_empty(&f).await;
    assert!(
        f.world().busy.contains("chat-model"),
        "released at the bound, with the slot still reading busy"
    );
}

/// The other half: a stream that ran to its end releases at once — even with
/// `/slots` reporting a slot busy (a direct client, say), because llama-server
/// finished this task before it closed the stream. On `stream_chat` and on
/// the legacy `/v1/completions` relay alike (second review, finding 12); the legacy
/// relay's hang-up defers like the chat one's.
#[tokio::test]
async fn a_stream_that_ends_normally_releases_at_once_on_both_relays() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;
    assert_eq!(sized_chat(&f.gateway, "hi", 8).await.status(), 200);
    // No bound on the deferral: only a normal end can empty the pool below.
    set_queue_timeout(&f, 0).await;
    set_slot_busy(&f, true);

    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse, "text/event-stream"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&f.first)
        .await;
    let resp = chat_body(
        &f.gateway,
        json!({
            "model": "chat-model",
            "messages": [{"role": "user", "content": words("a", 9)}],
            "max_tokens": 30,
            "stream": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("done"), "{text}");
    // The relay releases before it writes the stream's last frame, so by the
    // client's end of stream the pool is already empty — no poll needed.
    let v = vram_status(&f.gateway).await;
    assert!(pools(&v).is_empty(), "released at its end: {v}");

    // The legacy relay: a normal end…
    let legacy_sse = concat!(
        "data: {\"choices\":[{\"text\":\"ok\",\"index\":0,\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/v1/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(legacy_sse, "text/event-stream"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&f.first)
        .await;
    let legacy =
        |max_tokens: u64| {
            f.gateway
            .client()
            .post(format!("{}/v1/completions", f.gateway))
            .json(&json!({"model": "chat-model", "prompt": words("p", 9), "max_tokens": max_tokens,
                          "stream": true}))
            .send()
        };
    let resp = legacy(30).await.unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("[DONE]"), "{text}");
    let v = vram_status(&f.gateway).await;
    assert!(
        pools(&v).is_empty(),
        "the legacy relay released at its end: {v}"
    );

    // …and a hang-up, which keeps the cells until the slot lets go.
    let long: String = "data: {\"choices\":[{\"text\":\"x\",\"index\":0}]}\n\n".repeat(200_000);
    Mock::given(method("POST"))
        .and(path("/v1/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(long, "text/event-stream"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&f.first)
        .await;
    let resp = legacy(30).await.unwrap();
    assert_eq!(resp.status(), 200);
    drop(resp);
    until_pool(&f, "the legacy hang-up releasing", |p| p["releasing"] == 1).await;
    set_slot_busy(&f, false);
    until_pools_empty(&f).await;
}

/// NeverFits, end to end (second review, finding 12): a request larger than the whole
/// pool — only possible with several completions, since one sequence above
/// the pool is above the per-request limit first — is a 400 at once, never
/// queued. The pool it would have waited for is held by the test the whole
/// time (a hung-up stream whose slot reads busy), and the refusal arrives
/// anyway.
#[tokio::test]
async fn a_request_larger_than_the_whole_pool_is_refused_at_once() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;
    assert_eq!(sized_chat(&f.gateway, "hi", 8).await.status(), 200);
    let port = chat_port(&f);
    set_queue_timeout(&f, 0).await;
    set_slot_busy(&f, true);
    stream_and_hang_up(&f, &words("a", 9), 30).await;
    until_pool(&f, "A releasing", |p| p["releasing"] == 1).await;

    // One slot: 1 + 20 + 30 = 51 ≤ 64. Two completions: 21 + 2 × 30 = 81 > 64.
    let resp = chat_body(
        &f.gateway,
        json!({
            "model": "chat-model",
            "messages": [{"role": "user", "content": words("n", 20)}],
            "max_tokens": 30,
            "n": 2,
        }),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "context_length_exceeded", "{body}");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("21") && msg.contains("60") && msg.contains("64"),
        "prompt, output and the pool are named: {msg}"
    );

    let v = vram_status(&f.gateway).await;
    assert_eq!(queued(&pools(&v)[0]), 0, "never queued: {v}");
    assert_eq!(
        pools(&v)[0]["releasing"],
        1,
        "and it did not wait for the pool ahead to clear: {v}"
    );
    assert_eq!(
        f.world().chat_bodies[&port].len(),
        1,
        "only the warm-up was ever generated"
    );
    set_slot_busy(&f, false);
    until_pools_empty(&f).await;
}

/// Second review, finding 7: an in-process caller's own deadline covers the gate. A
/// `/v1/responses` turn's deadline is the loop's wall clock
/// (`responses_timeout_seconds`, 1 s here); with the pool held by the test
/// and the queue timeout off, the turn's pool wait ends at that deadline — a
/// named 503 — instead of lasting as long as the pool stays full. MCP
/// sampling's `SAMPLING_DEADLINE` reaches the same code (`sample_once`) the
/// same way.
#[tokio::test]
async fn an_in_process_deadline_ends_a_pool_wait() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;
    assert_eq!(sized_chat(&f.gateway, "hi", 8).await.status(), 200);
    let mut s = f.state.snapshot().settings.clone();
    s.vram.queue_timeout_seconds = 0;
    s.responses_timeout_seconds = 1;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();

    set_slot_busy(&f, true);
    stream_and_hang_up(&f, &words("a", 9), 30).await;
    until_pool(&f, "A releasing", |p| p["releasing"] == 1).await;

    // 1 + 9 + 30 = 40 beside A's 40: 80 > 64, so it waits — until its
    // deadline, since nothing else bounds this wait. The outer timeout only
    // turns a regression into a failure instead of a hang.
    let resp = tokio::time::timeout(
        Duration::from_secs(20),
        f.gateway
            .client()
            .post(format!("{}/v1/responses", f.gateway))
            .json(&json!({"model": "chat-model", "input": words("r", 9),
                          "max_output_tokens": 30}))
            .send(),
    )
    .await
    .expect("the turn's deadline ended the pool wait")
    .unwrap();
    assert_eq!(resp.status(), 503);
    let body = resp.text().await.unwrap();
    assert!(body.contains("shared KV pool"), "the pool is named: {body}");

    let v = vram_status(&f.gateway).await;
    assert_eq!(queued(&pools(&v)[0]), 0, "the waiter left the queue: {v}");
    assert_eq!(
        pools(&v)[0]["releasing"],
        1,
        "and nothing had made room — it was the deadline that ended it: {v}"
    );
    set_slot_busy(&f, false);
    until_pools_empty(&f).await;
}
