//! The `/v1/responses` loop writes a reasoning item's seal as its
//! `encrypted_content`, in the carrier lmgw reads back (§7.3.2, WP8 step
//! 5): whole and streamed, read back from a stateless client's input on the
//! loop and on the native path, checked against the items the key was given,
//! and an anonymous loop client (auth off) sends back its own.

mod no_id;
mod unsealed;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use wiremock::Request;

use super::{platform, posted, reasoning_items, SEALED};
use crate::chat_actions::{post, sse_events};
use crate::common::Gw;
use crate::support::responses_wire as rw;

/// A platform that answers each send with its own ids (`resp_<n>`,
/// `rs_<n>`, `msg_<n>`): a reasoning item, then a message.
async fn numbered() -> (
    wiremock::MockServer,
    lmgw_core::state::SharedState,
    Gw,
    super::Up,
) {
    let n = Arc::new(AtomicUsize::new(0));
    platform(move |req: &Request| {
        let i = n.fetch_add(1, Ordering::Relaxed) + 1;
        let items = [
            rw::reasoning(&format!("rs_{i}"), "Weighing it."),
            rw::message(&format!("msg_{i}"), "Hello"),
        ];
        let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let id = format!("resp_{i}");
        match body["stream"] == true {
            true => wiremock::ResponseTemplate::new(200)
                .set_body_raw(rw::stream(&id, &items), "text/event-stream"),
            false => wiremock::ResponseTemplate::new(200).set_body_json(rw::response(
                &id,
                "completed",
                json!(items),
            )),
        }
    })
    .await
}

/// A built-in `mcp` label keeps the request in lmgw's loop
/// (`responses/path.rs`).
fn in_the_loop(input: Value, stream: bool) -> Value {
    json!({"model": "my-model", "input": input, "store": false, "stream": stream,
           "tools": [{"type": "mcp", "server_label": "docs"}]})
}

fn reasoning_output(v: &Value) -> Value {
    v["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "reasoning")
        .cloned()
        .unwrap_or_else(|| panic!("{v}"))
}

async fn as_key(gw: &Gw, key: Option<&str>, body: Value) -> reqwest::Response {
    let mut req = gw.anon().post(format!("{gw}/v1/responses")).json(&body);
    if let Some(k) = key {
        req = req.bearer_auth(format!("lmgw-{k}"));
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn the_loop_writes_the_seal_as_encrypted_content_and_reads_it_back() {
    let (mock, _state, gw, up) = numbered().await;
    let v: Value = post(&gw, "/v1/responses", in_the_loop(json!("hi"), false))
        .await
        .json()
        .await
        .unwrap();
    let item = reasoning_output(&v);
    assert_eq!(
        item["encrypted_content"],
        format!("lmgw-openai:{up}:rs_1:{SEALED}"),
        "{v}"
    );
    assert_eq!(item["summary"][0]["text"], "Weighing it.");

    // Streamed: the reasoning item's `done` carries it.
    let text = post(&gw, "/v1/responses", in_the_loop(json!("hi"), true))
        .await
        .text()
        .await
        .unwrap();
    let done = sse_events(&text)
        .into_iter()
        .find(|(_, d)| d["type"] == "response.output_item.done" && d["item"]["type"] == "reasoning")
        .map(|(_, d)| d)
        .unwrap_or_else(|| panic!("{text}"));
    assert_eq!(
        done["item"]["encrypted_content"],
        format!("lmgw-openai:{up}:rs_2:{SEALED}")
    );

    // A stateless client sends the output back: the platform gets its own
    // item again, before the message that followed it.
    let mut input = vec![json!({"role": "user", "content": "hi"})];
    // Its reasoning and message items (the loop's tool listing is the
    // loop's own record, which the native path alone reads).
    input.extend(
        v["output"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["type"] == "reasoning" || i["type"] == "message")
            .cloned(),
    );
    input.push(json!({"role": "user", "content": "and?"}));
    let r = post(&gw, "/v1/responses", in_the_loop(json!(input), false)).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let sent = posted(&mock).await;
    let body = &sent[2];
    let items = body["input"].as_array().unwrap();
    let at = items.iter().position(|i| i["type"] == "reasoning").unwrap();
    assert_eq!(
        items[at],
        json!({"type": "reasoning", "id": "rs_1", "encrypted_content": SEALED,
               "summary": [{"type": "summary_text", "text": "Weighing it."}]}),
        "{body}"
    );
    assert_eq!(
        items[at + 1],
        json!({"role": "assistant", "content": "Hello"})
    );
}

/// On the native path (no label lmgw serves) the loop's item goes as the
/// platform wrote it: its own id and `encrypted_content`.
#[tokio::test]
async fn the_native_path_sends_the_loop_s_item_as_the_platform_wrote_it() {
    let (mock, _state, gw, up) = numbered().await;
    let carrier = format!("lmgw-openai:{up}:rs_up:{SEALED}");
    let other = format!(
        "lmgw-openai:{}:{}:rs_x:{SEALED}",
        up.id + 1,
        up.scope.config
    );
    let body = json!({"model": "my-model", "store": false, "input": [
        {"role": "user", "content": "hi"},
        {"type": "reasoning", "id": "rs_lmgw", "summary": [], "encrypted_content": carrier},
        {"type": "reasoning", "id": "rs_other", "summary": [],
         "encrypted_content": other},
        {"role": "assistant", "content": "Hello"},
        {"role": "user", "content": "and?"},
    ]});
    let r = post(&gw, "/v1/responses", body).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let sent = posted(&mock).await;
    assert_eq!(
        reasoning_items(&sent[0]),
        [json!({"type": "reasoning", "id": "rs_up", "summary": [],
                "encrypted_content": SEALED})],
        "another row's seal is left out: {}",
        sent[0]
    );
}

/// On the native path a reasoning item as OpenAI wrote it goes upstream for
/// a client key only with the encrypted content the key was given under that
/// id (WP8 review S-R3): its own id with another item's encrypted reasoning
/// behind it is `no_stored_response`.
#[tokio::test]
async fn a_client_key_s_own_item_id_with_other_encrypted_content_is_refused() {
    let (mock, state, gw, _up) = numbered().await;
    crate::platform_seals::client_key(&state, "k1").await;
    let native = json!({"model": "my-model", "store": false, "input": "hi"});
    let r = as_key(&gw, Some("k1"), native).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let back = |data: &str| {
        json!({"model": "my-model", "store": false, "input": [
            {"role": "user", "content": "hi"},
            {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": data},
            {"role": "assistant", "content": "Hello"},
            {"role": "user", "content": "and?"}]})
    };
    let r = as_key(&gw, Some("k1"), back("gAAAAB-another-turn")).await;
    assert_eq!(r.status(), 404, "{}", r.text().await.unwrap());
    assert_eq!(
        posted(&mock).await.len(),
        1,
        "nothing refused went upstream"
    );
    let r = as_key(&gw, Some("k1"), back(SEALED)).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
}

/// A client key sends back only its own seals, on the loop and natively;
/// another key's is `no_stored_response` before anything goes upstream.
#[tokio::test]
async fn a_client_key_may_send_back_only_its_own_seals() {
    let (mock, state, gw, _up) = numbered().await;
    crate::platform_seals::client_key(&state, "k1").await;
    crate::platform_seals::client_key(&state, "k2").await;
    let v: Value = as_key(&gw, Some("k1"), in_the_loop(json!("hi"), false))
        .await
        .json()
        .await
        .unwrap();
    let item = reasoning_output(&v);
    let said = v["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "message")
        .cloned()
        .unwrap();
    let back = |tools: bool| {
        // The item and the loop's message that followed it, as the loop
        // answered them: OpenAI refuses a reasoning item without its
        // following item, and the message's id is the loop's own (W-R1).
        let input = json!([{"role": "user", "content": "hi"}, item.clone(), said.clone(),
                           {"role": "user", "content": "and?"}]);
        match tools {
            true => in_the_loop(input, false),
            false => json!({"model": "my-model", "store": false, "input": input}),
        }
    };
    for tools in [true, false] {
        let r = as_key(&gw, Some("k2"), back(tools)).await;
        assert_eq!(r.status(), 404, "loop {tools}");
        let e: Value = r.json().await.unwrap();
        assert!(
            e["error"]["message"].as_str().unwrap().contains("rs_1"),
            "{e}"
        );
    }
    assert_eq!(
        posted(&mock).await.len(),
        1,
        "nothing refused went upstream"
    );
    for tools in [true, false] {
        let r = as_key(&gw, Some("k1"), back(tools)).await;
        assert_eq!(r.status(), 200, "loop {tools}: {}", r.text().await.unwrap());
    }
    let sent = posted(&mock).await;
    for body in &sent[1..] {
        assert_eq!(reasoning_items(body)[0]["id"], "rs_1", "{body}");
        let input = body["input"].as_array().unwrap();
        let at = input.iter().position(|i| i["type"] == "reasoning").unwrap();
        assert_eq!(
            input[at + 1],
            json!({"role": "assistant", "content": "Hello"}),
            "{body}"
        );
    }
}

/// A client key takes a loop answer's whole `output` to the native path
/// (WP8 review W-R1): the loop's reasoning item goes as the platform wrote
/// it, and the message and call items whose ids the loop made up go without
/// them (`responses/loop_items.rs`), not refused as unknown items.
#[tokio::test]
async fn a_client_key_takes_a_loop_transcript_to_the_native_path() {
    let n = Arc::new(AtomicUsize::new(0));
    let (mock, state, gw, _up) = platform(move |req: &Request| {
        let items = match n.fetch_add(1, Ordering::Relaxed) {
            0 => vec![
                rw::reasoning("rs_1", "Weighing it."),
                rw::message("msg_1", "Hello"),
            ],
            1 => vec![
                rw::reasoning("rs_2", "Look it up."),
                rw::message("msg_2", "Checking."),
                rw::call("fc_2", "call_2", "get_weather", "{\"city\":\"Oslo\"}"),
            ],
            _ => vec![rw::message("msg_3", "4 °C there.")],
        };
        super::answer(req, &items)
    })
    .await;
    crate::platform_seals::client_key(&state, "k1").await;
    let weather = json!([{"type": "function", "name": "get_weather",
                          "parameters": {"type": "object"}}]);
    // Turn one in the loop (its `mcp` tool), stored by lmgw; turn two
    // continues it there, with the client's own function tool.
    let mut stored = in_the_loop(json!("hi"), false);
    stored["store"] = json!(true);
    let first: Value = as_key(&gw, Some("k1"), stored).await.json().await.unwrap();
    let second: Value = as_key(
        &gw,
        Some("k1"),
        json!({"model": "my-model", "input": "Weather in Oslo?", "tools": weather,
               "previous_response_id": first["id"]}),
    )
    .await
    .json()
    .await
    .unwrap();
    let output = second["output"].as_array().unwrap().clone();
    let kinds: Vec<&str> = output.iter().map(|i| i["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["reasoning", "message", "function_call"], "{second}");

    // Turn three, stateless and native: the whole output, then the result.
    let mut input = vec![json!({"role": "user", "content": "Weather in Oslo?"})];
    input.extend(output.iter().cloned());
    input.push(json!({"type": "function_call_output", "call_id": "call_2", "output": "4 °C"}));
    let r = as_key(
        &gw,
        Some("k1"),
        json!({"model": "my-model", "store": false, "tools": weather, "input": input}),
    )
    .await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 3);
    assert_eq!(
        sent[2]["input"],
        json!([
            {"role": "user", "content": "Weather in Oslo?"},
            {"type": "reasoning", "id": "rs_2", "encrypted_content": SEALED,
             "summary": [{"type": "summary_text", "text": "Look it up."}]},
            {"role": "assistant", "content": "Checking."},
            {"type": "function_call", "call_id": "call_2", "name": "get_weather",
             "arguments": "{\"city\":\"Oslo\"}", "status": "completed"},
            {"type": "function_call_output", "call_id": "call_2", "output": "4 °C"},
        ]),
        "{}",
        sent[2]
    );
}

/// An anonymous loop client (auth off): its answers' items are recorded
/// under no key, as a public request's are, so it sends its own seal back.
#[tokio::test]
async fn an_anonymous_loop_client_sends_back_its_own_seal() {
    let (mock, state, gw, _up) = numbered().await;
    let r = as_key(&gw, None, in_the_loop(json!("hi"), false)).await;
    assert_eq!(r.status(), 200, "auth off: {}", r.text().await.unwrap());
    let v: Value = as_key(&gw, None, in_the_loop(json!("hi"), false))
        .await
        .json()
        .await
        .unwrap();
    let keyless: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM native_response_items WHERE key_id IS NULL")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(keyless, 4, "two answers' reasoning and message items");
    let input = json!([{"role": "user", "content": "hi"}, reasoning_output(&v),
                       {"role": "assistant", "content": "Hello"},
                       {"role": "user", "content": "and?"}]);
    let r = as_key(&gw, None, in_the_loop(input, false)).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    assert_eq!(reasoning_items(&posted(&mock).await[2])[0]["id"], "rs_2");
}

/// Two reasoning items around a message (WP8 review W-N1): streamed by the
/// loop, the second item's summary starts without the blank line that
/// joins the whole trace, as the whole answer's does.
#[tokio::test]
async fn each_loop_reasoning_item_starts_without_the_trace_s_blank_line() {
    let items = [
        rw::reasoning("rs_a", "First."),
        rw::message("msg_a", "Hm. "),
        rw::reasoning("rs_b", "Second."),
        rw::message("msg_b", "Done."),
    ];
    let (_mock, _state, gw, _up) = platform(move |req: &Request| super::answer(req, &items)).await;
    let text = post(&gw, "/v1/responses", in_the_loop(json!("hi"), true))
        .await
        .text()
        .await
        .unwrap();
    let deltas: Vec<String> = sse_events(&text)
        .into_iter()
        .filter(|(_, d)| d["type"] == "response.reasoning_summary_text.delta")
        .map(|(_, d)| d["delta"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(deltas, ["First.", "Second."], "{text}");
    let v: Value = post(&gw, "/v1/responses", in_the_loop(json!("hi"), false))
        .await
        .json()
        .await
        .unwrap();
    let summaries: Vec<&Value> = v["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "reasoning")
        .map(|i| &i["summary"][0]["text"])
        .collect();
    assert_eq!(summaries, ["First.", "Second."], "{v}");
}
