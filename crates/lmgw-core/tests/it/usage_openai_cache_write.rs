//! OpenAI's prompt-cache writes. Since GPT-5.6 OpenAI bills a write at 1.25x
//! the input rate, *in place of* the input rate (prompt-caching guide: "input
//! tokens use the uncached-input, cached-input, or cache-write rate"), and
//! reports it as `prompt_tokens_details.cache_write_tokens` on chat
//! completions — a subset of `prompt_tokens`, beside `cached_tokens`. That is
//! the IR's cache-write subset exactly, so it is read, priced at
//! `price_cache_write`, and shown to clients in OpenAI's own shape.
//!
//! The counts follow the guide's example: 12000 read, 3000 written.

use lmgw_core::config::{PriceScope, Protocol, UpstreamKind};
use lmgw_core::egress::for_protocol;
use lmgw_core::ingress::openai;
use lmgw_core::ir::{Completion, ContentPart, FinishReason, StreamDelta, Usage};
use lmgw_core::pricing::{PriceSource, Prices};
use lmgw_core::sse::SseEvent;
use lmgw_core::state::SharedState;
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::e2e_proxy::setup_kind;
use crate::stream_usage::row_after;

/// 20000 in: 5000 plain, 12000 read from the cache, 3000 written to it.
fn openai_usage() -> Value {
    json!({
        "prompt_tokens": 20000, "completion_tokens": 100, "total_tokens": 20100,
        "prompt_tokens_details": {"cached_tokens": 12000, "cache_write_tokens": 3000}
    })
}

fn chat_answer(usage: Value) -> Value {
    json!({
        "id": "chatcmpl-up", "object": "chat.completion", "model": "tgt-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "pong"},
                     "finish_reason": "stop"}],
        "usage": usage
    })
}

/// 1.00 in, 0.10 cache read, 1.25 cache write, 10.00 out per 1M tokens, so
/// micro-units are tokens × rate.
fn sheet(cache_write: Option<f64>) -> Prices {
    Prices {
        price_in: Some(1.0),
        price_out: Some(10.0),
        price_cache_read: Some(0.1),
        price_cache_write: cache_write,
        source: PriceSource::Manual,
    }
}

async fn price(state: &SharedState, p: Prices) {
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        "my-model",
        lmgw_core::config::PriceUnit::PerMtok,
        &p,
        None,
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

#[test]
fn a_whole_answer_reads_the_write_as_a_subset_of_the_prompt() {
    let u = for_protocol(Protocol::Openai)
        .parse_completion(chat_answer(openai_usage()).to_string().as_bytes())
        .unwrap()
        .usage;
    assert_eq!(u.prompt_tokens, Some(20000));
    assert_eq!(u.cached_input_tokens, Some(12000));
    assert_eq!(u.cache_write_tokens, Some(3000));
}

#[test]
fn an_answer_without_the_field_reports_no_write() {
    let u = for_protocol(Protocol::Openai)
        .parse_completion(
            chat_answer(json!({"prompt_tokens": 10, "completion_tokens": 1,
                               "prompt_tokens_details": {"cached_tokens": 0}}))
            .to_string()
            .as_bytes(),
        )
        .unwrap()
        .usage;
    assert_eq!(u.cache_write_tokens, None, "unreported, not 0");
}

#[test]
fn the_streamed_usage_chunk_carries_the_write() {
    let mut dec = for_protocol(Protocol::Openai).new_decoder();
    let usage: Vec<Usage> = dec
        .on_event(&SseEvent {
            event: None,
            data: json!({"object": "chat.completion.chunk", "choices": [],
                         "usage": openai_usage()})
            .to_string(),
        })
        .into_iter()
        .filter_map(|d| match d {
            StreamDelta::Usage(u) => Some(u),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].cache_write_tokens, Some(3000));
    assert_eq!(usage[0].cached_input_tokens, Some(12000));
}

fn completion_with(usage: Usage) -> Completion {
    Completion {
        content: vec![ContentPart::text("hi")],
        reasoning: String::new(),
        finish_reason: FinishReason::Stop,
        usage,
        model: "m".into(),
        timings: None,
    }
}

/// Each detail field only when it was reported — an Anthropic upstream's
/// `cache_creation_input_tokens` comes out here too.
#[test]
fn the_openai_ingress_shows_each_cache_count_only_when_reported() {
    let usage_of =
        |u: Usage| openai::serialize_completion("a", &completion_with(u))["usage"].clone();
    let base = Usage {
        prompt_tokens: Some(20000),
        completion_tokens: Some(100),
        ..Default::default()
    };
    let only_write = usage_of(Usage {
        cache_write_tokens: Some(3000),
        ..base
    });
    assert_eq!(
        only_write["prompt_tokens_details"],
        json!({"cache_write_tokens": 3000})
    );
    let both = usage_of(Usage {
        cached_input_tokens: Some(12000),
        cache_write_tokens: Some(3000),
        ..base
    });
    assert_eq!(
        both["prompt_tokens_details"],
        json!({"cached_tokens": 12000, "cache_write_tokens": 3000})
    );
    assert!(usage_of(base).get("prompt_tokens_details").is_none());
}

/// Through the gateway: the write reaches the client, the row, and the price
/// — billed at the write rate in place of the input rate, never on top.
#[tokio::test]
async fn a_cache_write_is_recorded_shown_and_priced_at_its_own_rate() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_answer(openai_usage())))
        .mount(&mock)
        .await;
    let (state, base) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    price(&state, sheet(Some(1.25))).await;

    let v: Value = reqwest::Client::new()
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
        openai_usage(),
        "OpenAI's own shape, as reported"
    );

    let row = row_after(&state, 0).await;
    assert_eq!(row.prompt_tokens, Some(20000));
    assert_eq!(row.cached_in_tokens, Some(12000));
    assert_eq!(row.cache_write_tokens, Some(3000));
    // 5000 × 1.00 + 12000 × 0.10 + 3000 × 1.25
    assert_eq!(row.cost_in_micro, Some(5000 + 1200 + 3750));
    assert_eq!(row.cost_out_micro, Some(1000));
}

/// A price row with no write rate bills the write at the input rate — what
/// OpenAI charges on the models before GPT-5.6, and no discount invented.
#[tokio::test]
async fn a_write_without_a_write_rate_is_billed_at_the_input_rate() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_answer(openai_usage())))
        .mount(&mock)
        .await;
    let (state, base) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    price(&state, sheet(None)).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let row = row_after(&state, 0).await;
    // 5000 × 1.00 + 12000 × 0.10 + 3000 × 1.00
    assert_eq!(row.cost_in_micro, Some(5000 + 1200 + 3000));
}

/// Legacy `/v1/completions` relays the body untouched and reads its usage
/// with the chat route's reader.
#[tokio::test]
async fn legacy_completions_record_the_write_too() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "text_completion", "model": "tgt-model",
            "choices": [{"index": 0, "text": "pong", "finish_reason": "stop"}],
            "usage": openai_usage()
        })))
        .mount(&mock)
        .await;
    let (state, base) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/completions"))
        .json(&json!({"model": "my-model", "prompt": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let row = row_after(&state, 0).await;
    assert_eq!(row.cached_in_tokens, Some(12000));
    assert_eq!(row.cache_write_tokens, Some(3000));
}

/// `/v1/responses` carries `cache_write_tokens` beside `cached_tokens`, both
/// required in OpenAI's `ResponseUsage`: the reported count, else 0.
#[tokio::test]
async fn the_responses_api_shows_the_write_beside_the_read() {
    let mock = MockServer::start().await;
    crate::responses_api::mount_sequence(
        &mock,
        vec![
            chat_answer(openai_usage()),
            chat_answer(json!({"prompt_tokens": 10, "completion_tokens": 4})),
        ],
    )
    .await;
    let (_state, base) = crate::responses_api::setup(&mock.uri()).await;

    let (_, resp) =
        crate::responses_api::post(&base, json!({"model": "my-model", "input": "ping"})).await;
    assert_eq!(
        resp["usage"]["input_tokens_details"],
        json!({"cached_tokens": 12000, "cache_write_tokens": 3000}),
        "{resp}"
    );
    let (_, resp) =
        crate::responses_api::post(&base, json!({"model": "my-model", "input": "ping"})).await;
    assert_eq!(
        resp["usage"]["input_tokens_details"],
        json!({"cached_tokens": 0, "cache_write_tokens": 0}),
        "{resp}"
    );
}
