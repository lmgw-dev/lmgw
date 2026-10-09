//! The ids Gemini's calls get: unique across answers, not numbered per
//! answer, so a history of several Gemini steps repeats none — on Gemini
//! itself, where a result is paired with its call's name by id, and on an
//! Anthropic upstream (a fallback, an alias switched mid-conversation),
//! which refuses a repeated `tool_use` id.

use std::collections::HashSet;

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::egress::for_protocol;
use lmgw_core::ir::{wire_call_id, ContentPart, StreamDelta};
use lmgw_core::sse::SseEvent;
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    call_ids, calls_body, calls_sse, gemini_gateway, json_reply, minted, model_bodies,
    mount_replies, signatures, sse_reply, text_body, text_sse, Ingress, Turn, SIG,
};
use crate::chat_actions::gateway;

const SKIP: &str = "skip_thought_signature_validator";

/// A second step's answer: the same two tools, the other way round, the
/// first signed with a signature of its own.
const SIG2: &str = "QUJDRA==";

fn swapped_body() -> Value {
    json!({
        "modelVersion": "tgt-model",
        "candidates": [{"content": {"role": "model", "parts": [
            {"functionCall": {"name": "get_time", "args": {"city": "Paris"}},
             "thoughtSignature": SIG2},
            {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}},
        ]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 9, "candidatesTokenCount": 4},
    })
}

fn swapped_sse() -> String {
    format!("data: {}\n\n", swapped_body())
}

/// The `functionResponse` names of a Gemini request body, in order.
fn response_names(body: &Value) -> Vec<String> {
    body["contents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| c["parts"].as_array().unwrap())
        .filter_map(|p| {
            p.pointer("/functionResponse/name")?
                .as_str()
                .map(String::from)
        })
        .collect()
}

#[test]
fn each_answer_mints_ids_of_its_own() {
    let egress = for_protocol(Protocol::Gemini);
    let mut ids = Vec::new();
    for _ in 0..2 {
        let c = egress
            .parse_completion(calls_body().to_string().as_bytes())
            .unwrap();
        ids.extend(c.content.into_iter().filter_map(|p| match p {
            ContentPart::ToolUse { id, .. } => Some(wire_call_id(&id).to_string()),
            _ => None,
        }));
        let mut dec = egress.new_decoder();
        for block in calls_sse().split("\n\n").filter(|b| !b.is_empty()) {
            for d in dec.on_event(&SseEvent {
                event: None,
                data: block.strip_prefix("data: ").unwrap().to_string(),
            }) {
                if let StreamDelta::ToolCallStart { id, .. } = d {
                    ids.push(wire_call_id(&id).to_string());
                }
            }
        }
    }
    assert_eq!(ids.len(), 8);
    assert!(ids.iter().all(|i| minted(i)), "{ids:?}");
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 8, "{ids:?}");
}

/// Two Gemini steps in one tool loop, on each client shape, streamed or
/// not: four ids, none repeated; the whole history sent back to Gemini
/// pairs each result with its own call's name (the first step's results
/// with the first step's calls, though the second step called the same
/// tools the other way round) and signs each step's first call; and the
/// same history on an Anthropic upstream carries four distinct `tool_use`
/// ids, each result naming one of them.
#[tokio::test]
async fn two_steps_repeat_no_id() {
    for ingress in Ingress::ALL {
        for stream in [false, true] {
            let what = format!("{} stream={stream}", ingress.name());
            let mock = MockServer::start().await;
            let replies = if stream {
                vec![
                    sse_reply(calls_sse()),
                    sse_reply(swapped_sse()),
                    sse_reply(text_sse("Sunny, 14:00.")),
                ]
            } else {
                vec![
                    json_reply(calls_body()),
                    json_reply(swapped_body()),
                    json_reply(text_body("Sunny, 14:00.")),
                ]
            };
            mount_replies(&mock, replies).await;
            let gw = gemini_gateway(&mock).await;

            let q = Turn::User("Weather and time in Paris?");
            let first = call_ids(&ingress.send(&gw, std::slice::from_ref(&q), stream).await);
            assert_eq!(first.len(), 2, "{what}");
            let mut history = vec![
                q,
                Turn::Calls(vec![
                    (first[0].clone(), "get_weather"),
                    (first[1].clone(), "get_time"),
                ]),
                Turn::Results(vec![
                    (first[0].clone(), "Sunny"),
                    (first[1].clone(), "14:00"),
                ]),
            ];
            let second = call_ids(&ingress.send(&gw, &history, stream).await);
            assert_eq!(second.len(), 2, "{what}");
            let all: HashSet<&str> = first
                .iter()
                .chain(&second)
                .map(|i| wire_call_id(i))
                .collect();
            assert_eq!(all.len(), 4, "{what}: {first:?} {second:?}");

            history.push(Turn::Calls(vec![
                (second[0].clone(), "get_time"),
                (second[1].clone(), "get_weather"),
            ]));
            history.push(Turn::Results(vec![
                (second[0].clone(), "14:00"),
                (second[1].clone(), "Sunny"),
            ]));
            ingress.send(&gw, &history, stream).await;
            let sent = model_bodies(&mock).await.pop().unwrap();
            assert_eq!(
                response_names(&sent),
                ["get_weather", "get_time", "get_time", "get_weather"],
                "{what}: {sent:#}"
            );
            let first_calls: Vec<Option<String>> = signatures(&sent)
                .into_iter()
                .filter_map(|c| c.into_iter().next())
                .map(|(_, s)| s)
                .collect();
            assert_eq!(
                first_calls,
                [Some(SIG.to_string()), Some(SIG2.to_string())],
                "{what}"
            );

            // The same history on an Anthropic upstream.
            let anthropic = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "type": "message", "role": "assistant",
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 1, "output_tokens": 1},
                })))
                .mount(&anthropic)
                .await;
            let (_, agw) = gateway(&anthropic, UpstreamKind::Generic, Protocol::Anthropic).await;
            ingress.send(&agw, &history, false).await;
            let sent = model_bodies(&anthropic).await.pop().unwrap();
            let blocks: Vec<&Value> = sent["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["content"].as_array().into_iter().flatten())
                .collect();
            let uses: Vec<&str> = blocks
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| b["id"].as_str().unwrap())
                .collect();
            let results: Vec<&str> = blocks
                .iter()
                .filter_map(|b| b["tool_use_id"].as_str())
                .collect();
            assert_eq!(uses.len(), 4, "{what}: {sent:#}");
            assert_eq!(
                uses.iter().collect::<HashSet<_>>().len(),
                4,
                "{what}: {sent:#}"
            );
            assert!(uses.iter().all(|i| minted(i)), "{what}: {uses:?}");
            assert_eq!(results, uses, "{what}: each result after its call");
        }
    }
}

/// A history whose ids repeat across steps (a thread stored before ids were
/// unique, an upstream that numbers its calls per answer): each result is
/// paired with the nearest call of its id before it, so the first step's
/// results keep the first step's names, and each step's first call gets the
/// skip value.
#[tokio::test]
async fn a_repeated_id_pairs_with_its_own_step() {
    let history = [
        Turn::User("Weather and time?"),
        Turn::Calls(vec![
            ("call_0".into(), "get_weather"),
            ("call_1".into(), "get_time"),
        ]),
        Turn::Results(vec![("call_0".into(), "Sunny"), ("call_1".into(), "14:00")]),
        Turn::Calls(vec![
            ("call_0".into(), "get_time"),
            ("call_1".into(), "get_weather"),
        ]),
        Turn::Results(vec![
            ("call_0".into(), "14:01"),
            ("call_1".into(), "Still sunny"),
        ]),
    ];
    for ingress in Ingress::ALL {
        let mock = MockServer::start().await;
        mount_replies(&mock, vec![json_reply(text_body("Done."))]).await;
        let gw = gemini_gateway(&mock).await;
        ingress.send(&gw, &history, false).await;
        let sent = model_bodies(&mock).await.pop().unwrap();
        assert_eq!(
            response_names(&sent),
            ["get_weather", "get_time", "get_time", "get_weather"],
            "{}: {sent:#}",
            ingress.name()
        );
        let skip = |name: &str| (name.to_string(), Some(SKIP.to_string()));
        let none = |name: &str| (name.to_string(), None);
        assert_eq!(
            signatures(&sent)
                .into_iter()
                .filter(|c| !c.is_empty())
                .collect::<Vec<_>>(),
            [
                vec![skip("get_weather"), none("get_time")],
                vec![skip("get_time"), none("get_weather")],
            ],
            "{}",
            ingress.name()
        );
    }
}
