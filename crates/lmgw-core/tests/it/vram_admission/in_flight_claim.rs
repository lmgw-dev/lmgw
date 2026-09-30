//! The in-flight claim

use super::*;

/// `/slots` is the cross-check, not the authority. A model with no generation
/// running on the container but a request lmgw itself is holding open must
/// survive an eviction pass — otherwise every in-process caller (sampling,
/// ingestion, the Chat tab) is stopped out from under itself.
#[tokio::test]
async fn eviction_refuses_a_model_a_hold_is_keeping_alive() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;

    // Exactly what an in-process caller does before it forwards.
    let route = f.state.snapshot().resolve("chat-model").unwrap();
    let hold = lmgw_core::vram::admit(&f.state, &route, "chat-model")
        .await
        .expect("the chat model is admitted")
        .expect("a managed local model always gets a hold");

    // The ledger says so out loud, which is what the eviction pass reads.
    let v = vram_status(&f.gateway).await;
    assert_eq!(v["resident"][0]["model"], "chat-model");
    assert_eq!(v["resident"][0]["in_flight"], 1);
    assert!(
        v["resident"][0]["idle_seconds"].is_u64(),
        "a held model has a real LRU stamp: {v}"
    );

    let resp = embed(&f.gateway).await;
    assert_eq!(resp.status(), 503, "at capacity, not a client error");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "vram_queue_timeout");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("chat-model"),
        "the refusal names what is holding the GPU: {body}"
    );
    assert!(
        f.stops().is_empty(),
        "a model with a request in flight is never a victim"
    );

    // Dropping the hold is what makes it evictable again.
    drop(hold);
    assert_eq!(embed(&f.gateway).await.status(), 200);
    assert_eq!(f.stops(), vec!["chat-model".to_string()]);
    assert_eq!(
        f.runs(),
        vec!["chat-model".to_string(), "embed-model".to_string()]
    );
}

/// A tool loop is many turns against one model with tool calls in between, and
/// the model has to survive those gaps: admitting per turn would leave it
/// unclaimed — and therefore first in line for eviction — for most of the run.
///
/// Admin Chat is the loop under test because it needs nothing outside this
/// process: the model calls a real `lmgw__*` tool, the gateway runs it
/// in-process, and the second turn answers. The competing embed request arrives
/// while the loop is between turns.
#[tokio::test]
async fn a_tool_loop_holds_the_gpu_across_its_turns_and_releases_at_the_end() {
    use lmgw_core::config::{SelfAdmin, Settings};

    // Turn 1 calls the tool; turn 2 answers, slowly enough that the competing
    // request below runs out its whole queue budget while the loop is still up.
    let call = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",",
        "\"type\":\"function\",\"function\":{\"name\":\"lmgw__status\",\"arguments\":\"\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
        "\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let answer = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"all good\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );

    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    // The chat model is the first thing started here, so it lands on `first`.
    for (i, (body, delay_ms)) in [(call, 0u64), (answer, 3_000)].into_iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream")
                    .set_delay(Duration::from_millis(delay_ms)),
            )
            .up_to_n_times(1)
            // Ahead of the container's own default-priority chat responder.
            .with_priority((i + 1) as u8)
            .mount(&f.first)
            .await;
    }

    let s = Settings {
        self_admin: SelfAdmin::ReadOnly,
        ..f.state.snapshot().settings.clone()
    };
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();

    let tid = f
        .gateway
        .client()
        .post(format!("{}/chat/api/threads", f.gateway))
        .json(&json!({"model_alias": "chat-model", "kind": "admin"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();

    let gateway = f.gateway.clone();
    let chatting = tokio::spawn(async move {
        gateway
            .client()
            .post(format!("{gateway}/chat/api/threads/{tid}/send"))
            .json(&json!({"content": "how is the gateway doing?"}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    });

    // Long enough for turn 1 to have finished and the tool call to have run:
    // the loop is now between turns, which is exactly where an unclaimed model
    // would be evicted.
    tokio::time::sleep(Duration::from_millis(900)).await;
    let v = vram_status(&f.gateway).await;
    let chat = v["resident"]
        .as_array()
        .and_then(|r| r.iter().find(|r| r["model"] == "chat-model"))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        chat["in_flight"], 1,
        "the loop's claim spans its turns, not one turn: {v}"
    );

    assert_eq!(embed(&f.gateway).await.status(), 503);
    assert!(
        f.stops().is_empty(),
        "a running tool loop must not be stopped between its turns"
    );

    let body = chatting.await.unwrap();
    assert!(body.contains("lmgw__status"), "the tool did run: {body}");
    assert!(body.contains("all good"), "and the loop finished: {body}");

    // The claim ends with the loop, not one turn earlier and not one later.
    let v = vram_status(&f.gateway).await;
    let chat = v["resident"]
        .as_array()
        .and_then(|r| r.iter().find(|r| r["model"] == "chat-model"))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(chat["in_flight"], 0, "released when the loop ended: {v}");

    assert_eq!(
        embed(&f.gateway).await.status(),
        200,
        "and now it is evictable again"
    );
    assert_eq!(f.stops(), vec!["chat-model".to_string()]);
}
