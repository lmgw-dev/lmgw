//! `/v1/messages` carries the seal in the thinking block's `signature`
//! behind the `lmgw-openai:` prefix (decision 17, §7.3.2, WP8 step 4),
//! written whole and streamed, read back into the seal, and checked against
//! the items the client's key was given.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use wiremock::Request;

use super::{answer, platform, posted, reasoning_items, SEALED};
use crate::chat_actions::{post, sse_events};
use crate::common::Gw;
use crate::support::responses_wire as rw;

/// A platform that answers each send with its own response and reasoning
/// item ids (`resp_<n>`, `rs_<n>`), a reasoning item then a call; and the
/// row's id.
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
            rw::call(
                &format!("fc_{i}"),
                &format!("call_{i}"),
                "get_weather",
                "{\"city\":\"Oslo\"}",
            ),
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

fn ask(stream: bool) -> Value {
    json!({"model": "my-model", "max_tokens": 256, "stream": stream,
           "messages": [{"role": "user", "content": "Weather in Oslo?"}],
           "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}]})
}

/// The second turn: the first turn's thinking block (with `signature`) and
/// call sent back, then the call's result.
fn second_turn(signature: &str, call_id: &str) -> Value {
    json!({"model": "my-model", "max_tokens": 256,
           "messages": [
               {"role": "user", "content": "Weather in Oslo?"},
               {"role": "assistant", "content": [
                   {"type": "thinking", "thinking": "Weighing it.", "signature": signature},
                   {"type": "tool_use", "id": call_id, "name": "get_weather",
                    "input": {"city": "Oslo"}}]},
               {"role": "user", "content": [
                   {"type": "tool_result", "tool_use_id": call_id, "content": "4 °C"}]}],
           "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}]})
}

async fn with_key(gw: &Gw, key: &str, body: Value) -> reqwest::Response {
    gw.anon()
        .post(format!("{gw}/v1/messages"))
        .bearer_auth(format!("lmgw-{key}"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_seal_is_the_thinking_signature_and_goes_back_to_its_row() {
    let (mock, _state, gw, up) = numbered().await;
    let v: Value = post(&gw, "/v1/messages", ask(false))
        .await
        .json()
        .await
        .unwrap();
    let carrier = format!("lmgw-openai:{up}:rs_1:{SEALED}");
    assert_eq!(
        v["content"][0],
        json!({"type": "thinking", "thinking": "Weighing it.", "signature": carrier}),
        "{v}"
    );
    assert_eq!(v["content"][1]["type"], "tool_use", "{v}");
    assert_eq!(
        v["content"].as_array().unwrap().len(),
        2,
        "one thinking block: {v}"
    );

    // Streamed: the thinking block ends with the seal's `signature_delta`.
    let text = post(&gw, "/v1/messages", ask(true))
        .await
        .text()
        .await
        .unwrap();
    let events = sse_events(&text);
    let sig = events
        .iter()
        .find(|(_, d)| d["delta"]["type"] == "signature_delta")
        .map(|(_, d)| d.clone())
        .unwrap_or_else(|| panic!("{text}"));
    assert_eq!(sig["index"], 0);
    assert_eq!(
        sig["delta"]["signature"],
        format!("lmgw-openai:{up}:rs_2:{SEALED}")
    );
    let stop_at = events
        .iter()
        .position(|(e, d)| e == "content_block_stop" && d["index"] == 0)
        .unwrap();
    let sig_at = events
        .iter()
        .position(|(_, d)| d["delta"]["type"] == "signature_delta")
        .unwrap();
    assert!(sig_at < stop_at, "{text}");

    // Sent back, the seal goes to the row as the reasoning item before the
    // call it preceded.
    let r = post(&gw, "/v1/messages", second_turn(&carrier, "call_1")).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let sent = posted(&mock).await;
    let input = sent[2]["input"].as_array().unwrap();
    let at = input.iter().position(|i| i["type"] == "reasoning").unwrap();
    assert_eq!(
        input[at],
        json!({"type": "reasoning", "id": "rs_1", "encrypted_content": SEALED,
               "summary": [{"type": "summary_text", "text": "Weighing it."}]})
    );
    assert_eq!(input[at + 1]["type"], "function_call");
    assert_eq!(input[at + 1]["call_id"], "call_1");
}

/// A client key sends back only the seals it was given: another key's, or
/// an item id it forged, is `no_stored_response` before anything goes
/// upstream (§7.3.4); a carrier that does not read is a 400.
#[tokio::test]
async fn a_client_key_may_send_back_only_its_own_seals() {
    let (mock, state, gw, up) = numbered().await;
    crate::platform_seals::client_key(&state, "k1").await;
    crate::platform_seals::client_key(&state, "k2").await;
    let v: Value = with_key(&gw, "k1", ask(false)).await.json().await.unwrap();
    let carrier = v["content"][0]["signature"].as_str().unwrap().to_string();
    assert_eq!(carrier, format!("lmgw-openai:{up}:rs_1:{SEALED}"));

    let r = with_key(&gw, "k2", second_turn(&carrier, "call_1")).await;
    assert_eq!(r.status(), 404);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["type"], "not_found_error", "{body}");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("rs_1"),
        "{body}"
    );
    // An item it was never given, its own item with other encrypted
    // reasoning behind it (another turn's, say), and its own item claimed
    // from another row (WP8 review S-R3).
    let config = &up.scope.config;
    for forged in [
        format!("lmgw-openai:{up}:rs_owner:{SEALED}"),
        format!("lmgw-openai:{up}:rs_1:gAAAAB-another-turn"),
        format!("lmgw-openai:{}:{config}:rs_1:{SEALED}", up.id + 1),
    ] {
        let r = with_key(&gw, "k1", second_turn(&forged, "call_1")).await;
        assert_eq!(r.status(), 404, "{forged}");
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["error"]["type"], "not_found_error", "{body}");
    }
    assert_eq!(
        posted(&mock).await.len(),
        1,
        "nothing refused went upstream"
    );

    let broken = with_key(&gw, "k1", second_turn("lmgw-openai:x", "call_1")).await;
    assert_eq!(broken.status(), 400);

    let r = with_key(&gw, "k1", second_turn(&carrier, "call_1")).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let sent = posted(&mock).await;
    assert_eq!(reasoning_items(&sent[1]).len(), 1, "{}", sent[1]);
}

/// Anthropic's own signature is not a seal: it stays the block's, and no
/// reasoning item goes to the platform for it (only an OpenAI seal could).
#[tokio::test]
async fn an_anthropic_signature_is_no_seal() {
    let (mock, _state, gw, _up) =
        platform(|req: &Request| answer(req, &[rw::message("msg_1", "Fine.")])).await;
    let r = post(&gw, "/v1/messages", second_turn("EqQBCkgIARAB", "call_1")).await;
    assert_eq!(r.status(), 200);
    assert!(reasoning_items(&posted(&mock).await[0]).is_empty());
}

/// A client's thinking block with no block after it in its turn (WP8 review
/// S-R1): OpenAI refuses a reasoning item without its following item, so
/// the seal is left out of the body; the text the client keeps is its own.
#[tokio::test]
async fn a_thinking_block_no_block_follows_is_left_out() {
    let (mock, _state, gw, up) = numbered().await;
    let carrier = format!("lmgw-openai:{up}:rs_1:{SEALED}");
    let body = json!({"model": "my-model", "max_tokens": 256, "messages": [
        {"role": "user", "content": "Weather in Oslo?"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "Weighing it.", "signature": carrier}]},
        {"role": "user", "content": "Go on."}]});
    let r = post(&gw, "/v1/messages", body).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let sent = posted(&mock).await;
    assert!(reasoning_items(&sent[0]).is_empty(), "{}", sent[0]);
    assert_eq!(sent[0]["input"].as_array().unwrap().len(), 2, "{}", sent[0]);
}

/// Two reasoning items around a message, the second unsealed (WP8 review
/// W-N1, W-N3): streamed, each is a thinking block of its own that starts
/// without the blank line joining the whole trace, and every block starts
/// with the empty `signature` Anthropic's own stream starts one with; whole,
/// the unsealed block beside the sealed one has that empty `signature` too.
#[tokio::test]
async fn each_item_is_its_own_block_with_a_signature() {
    let unsealed = json!({"type": "reasoning", "id": "rs_b",
                          "summary": [{"type": "summary_text", "text": "Second."}]});
    let items = [
        rw::reasoning("rs_a", "First."),
        rw::message("msg_a", "Hm. "),
        unsealed,
        rw::message("msg_b", "Done."),
    ];
    let (_mock, _state, gw, up) = platform(move |req: &Request| answer(req, &items)).await;
    let ask = |stream: bool| {
        json!({"model": "my-model", "max_tokens": 256, "stream": stream,
               "messages": [{"role": "user", "content": "Go"}]})
    };
    let text = post(&gw, "/v1/messages", ask(true))
        .await
        .text()
        .await
        .unwrap();
    let events = sse_events(&text);
    let starts: Vec<&Value> = events
        .iter()
        .filter(|(e, d)| e == "content_block_start" && d["content_block"]["type"] == "thinking")
        .map(|(_, d)| &d["content_block"])
        .collect();
    assert_eq!(starts.len(), 2, "{text}");
    for s in &starts {
        assert_eq!(s["signature"], "", "{text}");
    }
    let thinking: Vec<&str> = events
        .iter()
        .filter(|(_, d)| d["delta"]["type"] == "thinking_delta")
        .map(|(_, d)| d["delta"]["thinking"].as_str().unwrap())
        .collect();
    assert_eq!(thinking, ["First.", "Second."], "{text}");

    let v: Value = post(&gw, "/v1/messages", ask(false))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        v["content"],
        json!([
            {"type": "thinking", "thinking": "First.",
             "signature": format!("lmgw-openai:{up}:rs_a:{SEALED}")},
            {"type": "text", "text": "Hm. "},
            {"type": "thinking", "thinking": "Second.", "signature": ""},
            {"type": "text", "text": "Done."},
        ]),
        "{v}"
    );
}
