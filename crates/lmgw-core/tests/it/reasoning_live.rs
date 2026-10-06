//! Live check that a replayed reasoning trace reaches a real llama-server
//! *through the gateway* and grows the prompt — the thing `--reasoning-preserve`
//! is for, and the thing every ingress used to drop.
//!
//! Needs a running llama-server whose template supports preserved reasoning
//! (Qwen3.8 does; Qwen3.5 strips historic thinking whatever you pass). Point
//! `LMGW_LIVE_UPSTREAM` at its OpenAI base — e.g. the port `lmgw__status`
//! reports for a per-model container:
//!
//! ```text
//! LMGW_LIVE_UPSTREAM=http://127.0.0.1:43813/v1 \
//!   cargo test -p lmgw-core --test it reasoning_live:: -- --ignored --nocapture
//! ```
//!
//! Ignored by default: it is a measurement against your hardware, not a unit
//! test, and the number it prints is the evidence.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::ir::Params;
use lmgw_core::server::build_router;
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};

const TRACE: &str = "The user wants a colour. I will pick teal because it is unusual, \
                     and I must answer with the single word teal.";

fn conversation(assistant: Value) -> Value {
    json!({
        "model": "live",
        "stream": false,
        "max_tokens": 1,
        "messages": [
            {"role": "user", "content": "Pick a random colour and answer with only its name."},
            assistant,
            {"role": "user", "content": "Which colour did you pick?"}
        ]
    })
}

async fn prompt_tokens(client: &reqwest::Client, url: &str, body: &Value) -> u64 {
    let v: Value = client
        .post(url)
        .json(body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["usage"]["prompt_tokens"]
        .as_u64()
        .unwrap_or_else(|| panic!("no usage in {v}"))
}

#[tokio::test]
#[ignore = "needs LMGW_LIVE_UPSTREAM pointing at a running llama-server"]
async fn replayed_reasoning_grows_the_prompt_through_the_gateway() {
    let upstream = std::env::var("LMGW_LIVE_UPSTREAM")
        .expect("LMGW_LIVE_UPSTREAM=http://host:port/v1 of a running llama-server");

    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "live".into(),
            protocol: Protocol::LlamaCpp,
            kind: UpstreamKind::LlamaServer,
            base_url: upstream.trim_end_matches('/').to_string(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 600_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "live".into(),
            upstream_id: up_id,
            // Ignored by a single-model llama-server; set it for a router.
            upstream_model_id: std::env::var("LMGW_LIVE_MODEL").unwrap_or_default(),
            param_overrides: Params::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = reqwest::Client::new();
    let chat = format!("{base}/v1/chat/completions");
    let plain = prompt_tokens(
        &client,
        &chat,
        &conversation(json!({"role": "assistant", "content": "Teal"})),
    )
    .await;
    let with_content = prompt_tokens(
        &client,
        &chat,
        &conversation(json!({"role": "assistant", "content": "Teal", "reasoning_content": TRACE})),
    )
    .await;
    let with_reasoning = prompt_tokens(
        &client,
        &chat,
        &conversation(json!({"role": "assistant", "content": "Teal", "reasoning": TRACE})),
    )
    .await;
    // The Anthropic shape: the unsigned thinking block the gateway hands out.
    let messages = format!("{base}/v1/messages");
    let v: Value = client
        .post(&messages)
        .json(&json!({
            "model": "live",
            "max_tokens": 1,
            "messages": [
                {"role": "user", "content": "Pick a random colour and answer with only its name."},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": TRACE},
                    {"type": "text", "text": "Teal"}
                ]},
                {"role": "user", "content": "Which colour did you pick?"}
            ]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let with_thinking = v["usage"]["input_tokens"]
        .as_u64()
        .unwrap_or_else(|| panic!("no usage in {v}"));

    println!("prompt_tokens  plain={plain}  reasoning_content={with_content}  reasoning={with_reasoning}  anthropic thinking={with_thinking}");
    assert!(
        with_content > plain,
        "reasoning_content did not reach the template"
    );
    assert_eq!(
        with_reasoning, with_content,
        "`reasoning` is not normalised to reasoning_content"
    );
    assert_eq!(
        with_thinking, with_content,
        "a thinking block is not replayed as reasoning_content"
    );
}
