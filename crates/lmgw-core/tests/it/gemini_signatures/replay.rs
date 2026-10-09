//! Replay: what Gemini receives when a client sends a history back — a
//! captured signature on its own part, the skip value on the first call of
//! any step that has none (an older turn's too) or whose id a client
//! damaged, nothing on a later parallel call — and what any other upstream
//! receives (the bare id).

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::ir::{call_id_with_signature, THOUGHT_SIGNATURE_MARKER};
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    call_ids, calls_body, calls_sse, check, gemini_gateway, json_reply, model_bodies,
    mount_replies, signatures, sse_reply, text_body, text_sse, Ingress, Turn, SIG,
};
use crate::chat_actions::gateway;

const SKIP: &str = "skip_thought_signature_validator";

fn sig(name: &str, s: Option<&str>) -> (String, Option<String>) {
    (name.to_string(), s.map(String::from))
}

/// The client asks, Gemini calls two tools in parallel, the client sends
/// the calls and their results back: the signed call goes back signed, on
/// every client shape, streamed or not, and every shape sends Gemini the
/// same `contents`.
#[tokio::test]
async fn a_captured_signature_goes_back_on_its_own_part() {
    let mut seen: Vec<(String, Value)> = Vec::new();
    for ingress in Ingress::ALL {
        for stream in [false, true] {
            let mock = MockServer::start().await;
            let (calls, text) = if stream {
                (sse_reply(calls_sse()), sse_reply(text_sse("Sunny, 14:00.")))
            } else {
                (
                    json_reply(calls_body()),
                    json_reply(text_body("Sunny, 14:00.")),
                )
            };
            mount_replies(&mock, vec![calls, text]).await;
            let gw = gemini_gateway(&mock).await;
            let question = Turn::User("Weather and time in Paris?");
            let answer = ingress
                .send(&gw, std::slice::from_ref(&question), stream)
                .await;
            let ids = call_ids(&answer);
            assert_eq!(ids.len(), 2, "{answer}");
            let history = [
                question,
                Turn::Calls(vec![
                    (ids[0].clone(), "get_weather"),
                    (ids[1].clone(), "get_time"),
                ]),
                Turn::Results(vec![(ids[0].clone(), "Sunny"), (ids[1].clone(), "14:00")]),
            ];
            ingress.send(&gw, &history, stream).await;
            let bodies = model_bodies(&mock).await;
            assert_eq!(bodies.len(), 2);
            let sent = &bodies[1];
            let what = format!("{} stream={stream}", ingress.name());
            assert_eq!(
                signatures(sent),
                vec![
                    vec![],
                    vec![sig("get_weather", Some(SIG)), sig("get_time", None)],
                    vec![],
                ],
                "{what}: {sent:#}"
            );
            if !stream {
                check(&format!("round_trip__{}", ingress.name()), sent);
            }
            seen.push((what, sent["contents"].clone()));
        }
    }
    let (first, want) = &seen[0];
    for (what, got) in &seen[1..] {
        assert_eq!(got, want, "{what} differs from {first}");
    }
}

/// A longer history: a call from an earlier turn without a signature (the
/// skip value, though Gemini checks only the current turn), the current
/// turn's parallel pair with the first signed, and a step whose call came
/// from a model without signatures (the skip value).
fn mixed_history() -> Vec<Turn> {
    let signed = call_id_with_signature("call_0", SIG);
    vec![
        Turn::User("What was the weather?"),
        Turn::Calls(vec![("toolu_01old".into(), "get_weather")]),
        Turn::Results(vec![("toolu_01old".into(), "Rain")]),
        Turn::Assistant("It rained."),
        Turn::User("And now, with the time?"),
        Turn::Calls(vec![
            (signed.clone(), "get_weather"),
            ("call_1".into(), "get_time"),
        ]),
        Turn::Results(vec![(signed, "Sunny"), ("call_1".into(), "14:00")]),
        Turn::Calls(vec![("call_x9".into(), "get_weather")]),
        Turn::Results(vec![("call_x9".into(), "Still sunny")]),
    ]
}

#[tokio::test]
async fn a_step_without_a_signature_gets_the_skip_value() {
    let mut seen: Vec<(&str, Value)> = Vec::new();
    for ingress in Ingress::ALL {
        let mock = MockServer::start().await;
        mount_replies(&mock, vec![sse_reply(text_sse("Sunny."))]).await;
        let gw = gemini_gateway(&mock).await;
        ingress.send(&gw, &mixed_history(), true).await;
        let sent = model_bodies(&mock).await.pop().unwrap();
        assert_eq!(
            signatures(&sent),
            vec![
                vec![],
                vec![sig("get_weather", Some(SKIP))],
                vec![],
                vec![],
                vec![],
                vec![sig("get_weather", Some(SIG)), sig("get_time", None)],
                vec![],
                vec![sig("get_weather", Some(SKIP))],
                vec![],
            ],
            "{}: {sent:#}",
            ingress.name()
        );
        check(&format!("skip__{}", ingress.name()), &sent);
        seen.push((ingress.name(), sent["contents"].clone()));
    }
    let (first, want) = &seen[0];
    for (what, got) in &seen[1..] {
        assert_eq!(got, want, "{what} differs from {first}");
    }
}

/// A client that cut the signed id short (a length limit of its own) or
/// changed a character in it: the call goes back with the skip value, not
/// with a corrupt signature Gemini would refuse, and its result still pairs
/// with it by the bare id.
#[tokio::test]
async fn a_damaged_id_goes_back_with_the_skip_value() {
    let signed = call_id_with_signature("call_0", SIG);
    let mut flipped = signed.clone().into_bytes();
    let last = flipped.len() - 1;
    flipped[last] = if flipped[last] == b'A' { b'B' } else { b'A' };
    let damaged = [
        signed[..signed.len() - 3].to_string(),
        signed[..signed.find(THOUGHT_SIGNATURE_MARKER).unwrap() + 17].to_string(),
        String::from_utf8(flipped).unwrap(),
    ];
    for id in damaged {
        for ingress in Ingress::ALL {
            let mock = MockServer::start().await;
            mount_replies(&mock, vec![json_reply(text_body("Sunny."))]).await;
            let gw = gemini_gateway(&mock).await;
            let history = [
                Turn::User("Weather and time?"),
                Turn::Calls(vec![
                    (id.clone(), "get_weather"),
                    ("call_1".into(), "get_time"),
                ]),
                // The result keeps the whole id: paired by the bare one.
                Turn::Results(vec![(signed.clone(), "Sunny"), ("call_1".into(), "14:00")]),
            ];
            ingress.send(&gw, &history, false).await;
            let sent = model_bodies(&mock).await.pop().unwrap();
            assert_eq!(
                signatures(&sent),
                vec![
                    vec![],
                    vec![sig("get_weather", Some(SKIP)), sig("get_time", None)],
                    vec![],
                ],
                "{id} via {}: {sent:#}",
                ingress.name()
            );
            let names: Vec<_> = sent["contents"][2]["parts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["functionResponse"]["name"].clone())
                .collect();
            assert_eq!(names, [json!("get_weather"), json!("get_time")], "{id}");
        }
    }
}

/// Every tool call id in an OpenAI- or Anthropic-shaped request body.
fn wire_ids(body: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for m in body["messages"].as_array().unwrap() {
        for c in m["tool_calls"].as_array().into_iter().flatten() {
            out.push(c["id"].as_str().unwrap().to_string());
        }
        if let Some(id) = m["tool_call_id"].as_str() {
            out.push(id.to_string());
        }
        for b in m["content"].as_array().into_iter().flatten() {
            for k in ["id", "tool_use_id"] {
                if let Some(id) = b[k].as_str() {
                    out.push(id.to_string());
                }
            }
        }
    }
    out
}

/// The same history sent to an OpenAI-shaped and an Anthropic upstream: the
/// signed call's id goes out bare, as Gemini minted it, on the call and on
/// its result.
#[tokio::test]
async fn another_upstream_sees_the_bare_id() {
    let openai = json!({"choices": [{"message": {"role": "assistant", "content": "ok"},
                                     "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1}});
    let anthropic = json!({"type": "message", "role": "assistant",
                           "content": [{"type": "text", "text": "ok"}],
                           "stop_reason": "end_turn",
                           "usage": {"input_tokens": 1, "output_tokens": 1}});
    for (protocol, reply) in [(Protocol::Openai, openai), (Protocol::Anthropic, anthropic)] {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .mount(&mock)
            .await;
        let (_, gw) = gateway(&mock, UpstreamKind::Generic, protocol).await;
        for ingress in Ingress::ALL {
            ingress.send(&gw, &mixed_history(), false).await;
            let sent = model_bodies(&mock).await.pop().unwrap();
            let ids = wire_ids(&sent);
            assert_eq!(
                ids,
                [
                    "toolu_01old",
                    "toolu_01old",
                    "call_0",
                    "call_1",
                    "call_0",
                    "call_1",
                    "call_x9",
                    "call_x9"
                ],
                "{protocol:?} via {}: {sent:#}",
                ingress.name()
            );
            assert!(
                !sent.to_string().contains(THOUGHT_SIGNATURE_MARKER),
                "{sent:#}"
            );
        }
    }
}
