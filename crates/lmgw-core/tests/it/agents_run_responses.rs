//! A container's `/v1/responses` call stamped `X-Lmgw-Run` lands on its run
//! (container-runtime §3.1; review WP2b #5 and its second pass). Its turns
//! are written in-process (`record_in_process`), a native passthrough's row
//! by the free-form writer, and the tools lmgw runs for it by the scoped
//! executor — none by the `/v1` relay that put a chat call on the run, and
//! each used to be on no run at all: a run reported none of the model calls,
//! tool calls, tokens or cost its container spent through `/v1/responses`.

use lmgw_core::config::{PriceScope, PriceUnit, Protocol, UpstreamKind};
use lmgw_core::pricing::{PriceSource, Prices};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::agents_ledger::{doc, install, op, post_as, settled};
use crate::common::{serve, Gw};
use crate::support::mcp_stub::{answer, answering, register};

/// `alias` routed at `upstream`, priced 0.005 per answered request.
async fn priced_alias(state: &SharedState, alias: &str, upstream: &MockServer, native: bool) {
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: format!("{alias}-up"),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: upstream.uri().trim_end_matches('/').to_string(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: native,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: alias.into(),
            upstream_id: up,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        alias,
        PriceUnit::PerRequest,
        &Prices {
            source: PriceSource::Manual,
            ..Default::default()
        },
        Some(0.005),
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

fn chat_answer(usage: (u64, u64)) -> Value {
    json!({
        "id": "c1", "object": "chat.completion", "created": 1, "model": "tgt-model",
        "choices": [{ "index": 0, "finish_reason": "stop",
                      "message": { "role": "assistant", "content": "ok" } }],
        "usage": { "prompt_tokens": usage.0, "completion_tokens": usage.1,
                   "total_tokens": usage.0 + usage.1 }
    })
}

/// The labeler agent's run, opened through the ledger with its own token.
struct Run {
    base: Gw,
    token: String,
    id: i64,
}

impl Run {
    async fn open(state: &SharedState) -> Self {
        let base = serve(state.clone()).await;
        install(&base, &doc("labeler", 0)).await;
        let (_, a) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
        let token = a["token"].as_str().unwrap().to_string();
        let (_, opened) = post_as(
            &base,
            "/api/agents/labeler/runs",
            Some(&token),
            json!({ "phase": "run" }),
        )
        .await;
        let id = opened["run"].as_i64().unwrap();
        Self { base, token, id }
    }

    /// `POST /v1/responses` as the agent, stamped with this run or not.
    async fn respond(&self, body: Value, stamped: bool) -> u16 {
        let mut req = self
            .base
            .anon()
            .post(format!("{}/v1/responses", self.base))
            .header("authorization", format!("Bearer {}", self.token))
            .json(&body);
        if stamped {
            req = req.header("x-lmgw-run", self.id.to_string());
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let _ = resp.text().await;
        status
    }

    /// Close the run and read it back once it has settled.
    async fn close(&self) -> Value {
        post_as(
            &self.base,
            &format!("/api/agents/runs/{}/close", self.id),
            Some(&self.token),
            json!({ "status": "done" }),
        )
        .await;
        settled(&self.base, self.id).await
    }
}

#[tokio::test]
async fn a_stamped_responses_call_lands_its_turns_on_the_run() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_answer((11, 7))))
        .mount(&upstream)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    priced_alias(&state, "m1", &upstream, false).await;
    let run = Run::open(&state).await;

    let body = json!({ "model": "m1", "input": "hi" });
    assert_eq!(run.respond(body.clone(), true).await, 200);
    assert_eq!(
        run.respond(body, false).await,
        200,
        "not stamped: not the run's"
    );

    let done = run.close().await;
    let result = &done["result"];
    assert_eq!(result["model_calls"], json!(1), "{result}");
    assert_eq!(result["usage"]["prompt_tokens"], json!(11), "{result}");
    assert_eq!(result["usage"]["completion_tokens"], json!(7), "{result}");
    assert_eq!(
        result["cost_micro"],
        json!(5_000),
        "its row's fee: {result}"
    );
    // …and the Runs tab's summary says so.
    assert_eq!(done["job"]["model_calls"], json!(1), "{}", done["job"]);
    assert_eq!(done["job"]["cost_micro"], json!(5_000));
}

/// An upstream that implements `/v1/responses` itself gets the body as it
/// came, and its row carries no tokens: answered, it pays its fee on the run.
#[tokio::test]
async fn a_stamped_native_passthrough_lands_on_the_run() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_1", "object": "response", "status": "completed",
            "model": "tgt-model", "output": [],
        })))
        .mount(&upstream)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    priced_alias(&state, "native", &upstream, true).await;
    let run = Run::open(&state).await;

    assert_eq!(
        run.respond(json!({ "model": "native", "input": "hi" }), true)
            .await,
        200
    );
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        1,
        "passed through"
    );

    let done = run.close().await;
    let result = &done["result"];
    assert_eq!(result["model_calls"], json!(1), "{result}");
    assert_eq!(
        result["cost_micro"],
        json!(5_000),
        "the answered passthrough's fee, not a run without it: {result}"
    );
}

/// The tools lmgw runs for a stamped `/v1/responses` loop are the run's tool
/// calls, as a container's own `tools/call` on `/mcp` is.
#[tokio::test]
async fn a_stamped_responses_loop_counts_the_tools_lmgw_ran() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c0", "object": "chat.completion", "created": 1, "model": "tgt-model",
            "choices": [{ "index": 0, "finish_reason": "tool_calls",
                          "message": { "role": "assistant", "content": null,
                                       "tool_calls": [{ "id": "call_1", "type": "function",
                                                        "function": { "name": "gws__search",
                                                                      "arguments": "{}" } }] } }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7 }
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_answer((11, 7))))
        .mount(&upstream)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    priced_alias(&state, "m1", &upstream, false).await;
    let stub = answering(
        json!([{
            "name": "search",
            "description": "search",
            "inputSchema": {"type": "object", "properties": {}},
        }]),
        false,
        answer(|_name, _args| async {
            json!({ "content": [{ "type": "text", "text": "two unread" }], "isError": false })
        }),
    )
    .await;
    register(&state, "gws", "gws", &stub.url, true, None).await;
    let run = Run::open(&state).await;

    let body = json!({ "model": "m1", "input": "anything unread?",
                       "tools": [{ "type": "mcp", "server_label": "gws",
                                   "require_approval": "never" }] });
    assert_eq!(run.respond(body, true).await, 200);
    assert_eq!(stub.calls().len(), 1, "the tool ran");

    let done = run.close().await;
    let result = &done["result"];
    assert_eq!(result["tool_calls"], json!(1), "{result}");
    assert_eq!(result["model_calls"], json!(2), "{result}");
    assert_eq!(result["cost_micro"], json!(10_000), "{result}");
}
