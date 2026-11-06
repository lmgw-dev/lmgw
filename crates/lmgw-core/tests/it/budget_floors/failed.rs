//! A whole Response the platform answered with a 2xx and `status: "failed"`
//! (openai egress design §7.2.3): the client gets OpenAI's own error, and
//! the row counts what the Response reported, or the prompt's local count
//! when it reported nothing (decision 22), as a stream's `response.failed`
//! does.

use super::*;

/// A failed Response, with [`usage`] when `reported`, served at `default`.
fn failed(reported: bool) -> Value {
    let mut v = json!({
        "id": "resp_up", "object": "response", "status": "failed", "model": "tgt-model",
        "service_tier": "default", "output": [],
        "error": {"code": "server_error", "message": "The model broke."},
    });
    if reported {
        v["usage"] = usage();
    }
    v
}

#[tokio::test]
async fn a_failed_whole_response_keeps_its_usage_and_says_openai_s_error() {
    let (_m, state, gw) = platform(ResponseTemplate::new(200).set_body_json(failed(true))).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    let (status, body) = chat(&gw, json!({}), None).await;
    assert_eq!(status, 502, "{body}");
    assert_eq!(body["error"]["code"], "server_error", "{body}");
    assert_eq!(
        body["error"]["message"],
        "the upstream's response failed: The model broke. (server_error)"
    );
    let row = row_after(&state, 0).await;
    assert_eq!(row.status, 502, "{row:?}");
    assert_eq!(row.error_kind.as_deref(), Some("upstream"));
    assert_eq!(
        (row.prompt_tokens, row.completion_tokens, row.cost_micro),
        (Some(1000), Some(100), Some(STANDARD_MICRO)),
        "the failed Response's usage, priced: {row:?}"
    );
    assert_eq!(spend(&state).await, STANDARD_MICRO);
}

#[tokio::test]
async fn a_failed_whole_response_without_usage_floors_its_prompt() {
    let (_m, state, gw) = platform(ResponseTemplate::new(200).set_body_json(failed(false))).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    let (status, _) = chat(
        &gw,
        json!({"messages": [{"role": "user", "content": "Say hello."}]}),
        None,
    )
    .await;
    assert_eq!(status, 502);
    let row = row_after(&state, 0).await;
    assert_eq!((row.prompt_tokens, row.cost_micro), (None, None), "{row:?}");
    let p = tiktoken_rs::bpe_for_model("tgt-model")
        .or_else(|_| tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase))
        .unwrap()
        .encode_ordinary("Say hello.")
        .len() as i64;
    let micro = p * 2;
    assert_eq!(
        row.price_note,
        Some(format!(
            "the upstream's answer reported no usage; budgets count a local count of {p} prompt \
             and 0 output tokens at the standard price, {:.6} USD, a lower bound",
            micro as f64 / 1_000_000.0
        ))
    );
    assert_eq!(floor_of(&state, row.id).await, Some(micro));
    assert_eq!(spend(&state).await, micro);
}
