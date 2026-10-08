//! Gemini's thinking tokens are output tokens. Google reports
//! `thoughtsTokenCount` *beside* `candidatesTokenCount`, not inside it
//! (`totalTokenCount` is prompt + thoughts + candidates), and bills output as
//! their sum. The egress adds the two into the IR's completion, so a thinking
//! model is priced on all of its output and a client sees the same meaning of
//! `completion_tokens` / `output_tokens` whichever provider answered.
//!
//! The numbers are Google's own example: prompt 8, candidates 540, thoughts
//! 491, total 1039.

use lmgw_core::config::{PriceScope, Protocol, UpstreamKind};
use lmgw_core::egress::for_protocol;
use lmgw_core::ir::{StreamDelta, Usage};
use lmgw_core::pricing::{PriceSource, Prices};
use lmgw_core::sse::SseEvent;
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::e2e_proxy::setup_kind;
use crate::stream_usage::row_after;

fn google_example() -> Value {
    json!({
        "promptTokenCount": 8,
        "candidatesTokenCount": 540,
        "thoughtsTokenCount": 491,
        "totalTokenCount": 1039
    })
}

fn gemini_answer(usage: Value) -> Value {
    json!({
        "modelVersion": "tgt-model",
        "candidates": [{"content": {"role": "model", "parts": [{"text": "hi"}]},
                        "finishReason": "STOP"}],
        "usageMetadata": usage
    })
}

fn parsed(usage: Value) -> Usage {
    for_protocol(Protocol::Gemini)
        .parse_completion(gemini_answer(usage).to_string().as_bytes())
        .unwrap()
        .usage
}

#[test]
fn thoughts_are_added_to_the_candidates_as_completion() {
    let u = parsed(google_example());
    assert_eq!(u.prompt_tokens, Some(8));
    assert_eq!(
        u.completion_tokens,
        Some(1031),
        "540 candidates + 491 thoughts"
    );
    assert_eq!(
        u.reasoning_tokens,
        Some(491),
        "the thinking share, informational"
    );
    assert_eq!(
        u.prompt_tokens.unwrap() + u.completion_tokens.unwrap(),
        1039,
        "the IR total is Google's totalTokenCount"
    );
}

#[test]
fn either_count_alone_is_the_completion_and_neither_stays_unknown() {
    let only_thoughts = parsed(json!({"promptTokenCount": 8, "thoughtsTokenCount": 491}));
    assert_eq!(only_thoughts.completion_tokens, Some(491));
    assert_eq!(only_thoughts.reasoning_tokens, Some(491));

    let only_candidates = parsed(json!({"promptTokenCount": 8, "candidatesTokenCount": 540}));
    assert_eq!(only_candidates.completion_tokens, Some(540));
    assert_eq!(only_candidates.reasoning_tokens, None);

    let neither = parsed(json!({"promptTokenCount": 8}));
    assert_eq!(neither.completion_tokens, None, "unknown, never 0");
}

/// Google documents `toolUsePromptTokenCount` as a count of tool-use prompt
/// tokens, outside `totalTokenCount`, and not as billed input: it is not
/// folded into the prompt.
#[test]
fn tool_use_prompt_tokens_are_not_added_to_the_prompt() {
    let mut usage = google_example();
    usage["toolUsePromptTokenCount"] = json!(77);
    assert_eq!(parsed(usage).prompt_tokens, Some(8));
}

#[test]
fn the_stream_reports_the_sum_on_its_finishing_chunk() {
    let mut dec = for_protocol(Protocol::Gemini).new_decoder();
    let chunks = [
        json!({"candidates": [{"content": {"parts": [{"text": "Hel"}]}}],
               "usageMetadata": {"promptTokenCount": 8, "thoughtsTokenCount": 491}}),
        json!({"candidates": [{"content": {"parts": [{"text": "lo"}]}, "finishReason": "STOP"}],
               "usageMetadata": google_example()}),
    ];
    let usage: Vec<Usage> = chunks
        .iter()
        .flat_map(|c| {
            dec.on_event(&SseEvent {
                event: None,
                data: c.to_string(),
            })
        })
        .filter_map(|d| match d {
            StreamDelta::Usage(u) => Some(u),
            _ => None,
        })
        .collect();
    assert_eq!(
        usage,
        vec![Usage {
            prompt_tokens: Some(8),
            completion_tokens: Some(1031),
            reasoning_tokens: Some(491),
            ..Default::default()
        }]
    );
}

/// Through the gateway: the OpenAI client sees OpenAI's meaning
/// (`completion_tokens` includes `reasoning_tokens`), the Anthropic client
/// Anthropic's (`output_tokens` includes thinking), and the row is priced on
/// all 1031 output tokens — the reasoning column a subset, never added again.
#[tokio::test]
async fn a_thinking_gemini_answer_is_priced_and_reported_on_all_its_output() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/tgt-model:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_answer(google_example())))
        .mount(&mock)
        .await;
    let (state, base) = setup_kind(&mock.uri(), Protocol::Gemini, UpstreamKind::Generic).await;
    // 1.00 in, 10.00 out per 1M tokens: micro-units are tokens × rate.
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        "my-model",
        lmgw_core::config::PriceUnit::PerMtok,
        &Prices {
            price_in: Some(1.0),
            price_out: Some(10.0),
            source: PriceSource::Manual,
            ..Default::default()
        },
        None,
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let client = reqwest::Client::new();

    let v: Value = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        v["usage"],
        json!({"prompt_tokens": 8, "completion_tokens": 1031, "total_tokens": 1039,
               "completion_tokens_details": {"reasoning_tokens": 491}}),
        "{v}"
    );
    let row = row_after(&state, 0).await;
    assert_eq!(row.prompt_tokens, Some(8));
    assert_eq!(row.completion_tokens, Some(1031));
    assert_eq!(row.reasoning_tokens, Some(491));
    assert_eq!(row.cost_in_micro, Some(8));
    assert_eq!(
        row.cost_out_micro,
        Some(10_310),
        "1031 × 10, thoughts billed once"
    );
    assert_eq!(row.cost_micro, Some(10_318));

    let v: Value = client
        .post(format!("{base}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({"model": "my-model", "max_tokens": 2048,
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["usage"]["input_tokens"], 8, "{v}");
    assert_eq!(v["usage"]["output_tokens"], 1031, "{v}");
}
