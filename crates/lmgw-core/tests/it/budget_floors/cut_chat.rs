//! A platform chat stream cut before its usage floors from a local count
//! (openai egress design §13.6, the owner's yes of 2026-10-24): OpenAI
//! reports a stream's usage only at its end (the Responses wire's terminal
//! event, which the platform's chat sends take since WP7, or the Chat
//! wire's last chunk), so a stream that ends before it is unpriced; the
//! budgets count the request's text and the deltas relayed, counted with
//! the platform's tokenizer, the prompt at the cache-read rate.

use super::*;

/// Counted as the platform counts: tiktoken, `o200k_base` for a model it
/// does not know.
fn tokens(text: &str) -> i64 {
    tiktoken_rs::bpe_for_model("tgt-model")
        .or_else(|_| tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase))
        .unwrap()
        .encode_ordinary(text)
        .len() as i64
}

/// A platform stream that ends after its answer and before its terminal
/// event, which carries the usage.
fn cut_stream() -> ResponseTemplate {
    let chunks = crate::support::responses_wire::sse(&[
        json!({"type": "response.created", "response": {"service_tier": "default"}}),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "Hel"}),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "lo there"}),
    ]);
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(chunks)
}

/// The floor under [`price`]'s standard sheet (2 in, 8 out, no cache-read
/// rate, so the plain input rate stands in for it): the clause and the
/// micro-units.
fn floor(prompt: &str, relayed: &str) -> (String, i64) {
    let (p, o) = (tokens(prompt), tokens(relayed));
    let micro = p * 2 + o * 8;
    (
        format!(
            "budgets count a local count of {p} prompt and {o} output tokens at the standard \
             price, {:.6} USD, a lower bound",
            micro as f64 / 1_000_000.0
        ),
        micro,
    )
}

#[tokio::test]
async fn a_public_chat_stream_cut_before_its_usage_floors_from_a_local_count() {
    let (_m, state, gw) = platform(cut_stream()).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    let resp = gw
        .anon()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({"model": ALIAS, "stream": true,
                      "messages": [{"role": "system", "content": "Be brief."},
                                   {"role": "user", "content": "Say hello."}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    // Cut, not finished: the client hears the error, never a clean stop.
    assert!(
        text.contains("upstream stream ended before its terminal event"),
        "{text}"
    );
    assert!(!text.contains("\"finish_reason\":\"stop\""), "{text}");
    let row = row_after(&state, 0).await;
    assert!(row.streamed);
    assert_eq!(row.error_kind.as_deref(), Some("upstream"), "{row:?}");
    assert_eq!(
        (row.prompt_tokens, row.completion_tokens, row.cost_micro),
        (None, None, None),
        "a local count is never shown as usage: {row:?}"
    );
    let (clause, micro) = floor("Be brief.\nSay hello.", "Hello there");
    assert_eq!(
        row.price_note,
        Some(format!(
            "stream ended before the upstream reported usage; {clause}"
        ))
    );
    assert_eq!(floor_of(&state, row.id).await, Some(micro));
    assert_eq!(spend(&state).await, micro, "the reseed counts it");
}

/// The local count feeds the key's tokens-per-minute window as reported
/// usage would: a key that cuts its streams does not slip past its limit.
#[tokio::test]
async fn a_cut_stream_s_local_count_counts_toward_the_tpm_limit() {
    let (_m, state, gw) = platform(cut_stream()).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, enabled, tpm_limit) VALUES ('tpm', ?1, 1, 4)",
    )
    .bind(lmgw_core::config::hash_api_key("lmgw-tpm"))
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let send = || {
        gw.anon()
            .post(format!("{gw}/v1/chat/completions"))
            .bearer_auth("lmgw-tpm")
            .json(&json!({"model": ALIAS, "stream": true,
                          "messages": [{"role": "system", "content": "Be brief."},
                                       {"role": "user", "content": "Say hello."}]}))
            .send()
    };
    let first = send().await.unwrap();
    assert_eq!(first.status(), 200);
    first.text().await.unwrap();
    row_after(&state, 0).await;
    assert!(tokens("Be brief.\nSay hello.") + tokens("Hello there") > 4);
    let second = send().await.unwrap();
    assert_eq!(second.status(), 429, "the counted tokens fill the window");
    let body: Value = second.json().await.unwrap();
    assert_eq!(body["error"]["code"], "key_rate", "{body}");
}

/// A whole platform answer that reported no usage is counted the same way:
/// what was sent and what it answered.
#[tokio::test]
async fn a_whole_answer_without_usage_floors_from_a_local_count() {
    let mut body = answer(Some("default"));
    body.as_object_mut().unwrap().remove("usage");
    let (_m, state, gw) = platform(ResponseTemplate::new(200).set_body_json(body)).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    let (status, body) = chat(&gw, json!({}), None).await;
    assert_eq!(status, 200, "{body}");
    let row = row_after(&state, 0).await;
    assert_eq!((row.prompt_tokens, row.cost_micro), (None, None), "{row:?}");
    let (clause, micro) = floor("hi", "pong");
    assert_eq!(
        row.price_note,
        Some(format!("the upstream's answer reported no usage; {clause}"))
    );
    assert_eq!(floor_of(&state, row.id).await, Some(micro));
}

/// An in-process turn on the platform (the `/v1/responses` loop's, kept in
/// the loop by a label lmgw serves) whose stream ends before its usage:
/// the same floor on the turn's row.
#[tokio::test]
async fn an_in_process_turn_cut_before_its_usage_floors_too() {
    let (_m, state, gw) = platform(cut_stream()).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    let resp = gw
        .anon()
        .post(format!("{gw}/v1/responses"))
        .json(
            &json!({"model": ALIAS, "stream": true, "input": "Say hello.",
                      "tools": [{"type": "mcp", "server_label": "docs"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.text().await.unwrap();
    let row = row_after(&state, 0).await;
    assert_eq!(row.ingress_proto, "responses", "{row:?}");
    assert_eq!(row.cost_micro, None, "{row:?}");
    let note = row.price_note.clone().unwrap_or_default();
    assert!(
        note.starts_with(
            "stream ended before the upstream reported usage; budgets count a \
                          local count of "
        ),
        "{note}"
    );
    let floor = floor_of(&state, row.id).await;
    assert!(floor.is_some_and(|f| f > 0), "{floor:?}");
    assert_eq!(spend(&state).await, floor.unwrap());
}

/// A `/v1/messages` stream cut the same way ends in Anthropic's `error`
/// event, with no clean `message_delta` or `message_stop` after it, and its
/// row floors as the chat one does.
#[tokio::test]
async fn a_messages_stream_cut_before_its_terminal_event_ends_in_an_error() {
    let (_m, state, gw) = platform(cut_stream()).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    let text = gw
        .anon()
        .post(format!("{gw}/v1/messages"))
        .json(&json!({"model": ALIAS, "stream": true, "max_tokens": 100,
                      "messages": [{"role": "user", "content": "Say hello."}]}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.contains("event: error"), "{text}");
    assert!(
        text.contains("upstream stream ended before its terminal event"),
        "{text}"
    );
    assert!(!text.contains("message_stop"), "{text}");
    let row = row_after(&state, 0).await;
    assert!(
        floor_of(&state, row.id).await.is_some_and(|f| f > 0),
        "{row:?}"
    );
}
