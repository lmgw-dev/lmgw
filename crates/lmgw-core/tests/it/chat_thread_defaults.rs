//! Chat threads' defaults and overrides: the default system prompt a new
//! thread starts from (Settings → Chat), its `{{model}}`/`{{date}}`
//! placeholders filled in at send time, and the per-thread reasoning
//! overrides that go out at the `x-lmgw-reasoning*` headers' tier.

use lmgw_core::config::{Protocol, UpstreamKind, BUILTIN_CHAT_SYSTEM_PROMPT};
use lmgw_core::ir::{Params, ReasoningControl};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{serve, Gw};

const SSE_OK: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n",
    "data: [DONE]\n\n"
);

/// A gateway with one alias, `my-model`, on a generic OpenAI upstream at
/// `upstream_base` — a route that sends `reasoning_effort` and drops a budget.
async fn setup(upstream_base: &str) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: upstream_base.trim_end_matches('/').to_string(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
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
            alias: "my-model".into(),
            upstream_id: up_id,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

async fn mock_upstream() -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(SSE_OK, "text/event-stream"),
        )
        .mount(&mock)
        .await;
    mock
}

async fn new_thread(base: &Gw, kind: &str) -> Value {
    base.client()
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": "my-model", "kind": kind }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn thread(base: &Gw, tid: i64) -> Value {
    base.client()
        .get(format!("{base}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["thread"]
        .clone()
}

async fn patch(base: &Gw, tid: i64, body: Value) -> (u16, Value) {
    let resp = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/settings"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn op(base: &Gw, name: &str, args: Value) -> (u16, Value) {
    let resp = base
        .client()
        .post(format!("{base}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// Send one message and read the stream to its end (the turn is persisted
/// when it ends); the SSE text back.
async fn send(base: &Gw, tid: i64, content: &str) -> String {
    let body = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": content }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("event: done"), "{body}");
    body
}

/// The last request the upstream saw, as JSON.
async fn last_upstream_body(mock: &MockServer) -> Value {
    let seen = mock.received_requests().await.unwrap();
    serde_json::from_slice(&seen.last().expect("the upstream was called").body).unwrap()
}

/// The `done` frame's JSON out of a send's SSE text.
fn done_frame(sse: &str) -> Value {
    let rec = sse
        .split("\n\n")
        .find(|r| r.contains("event: done"))
        .expect("a done frame");
    let data = rec
        .lines()
        .find_map(|l| l.strip_prefix("data:"))
        .expect("done carries data");
    serde_json::from_str(data.trim()).unwrap()
}

#[tokio::test]
async fn a_new_chat_thread_starts_from_the_default_prompt() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri()).await;

    // Out of the box: the built-in default, which the settings read names.
    let t = new_thread(&base, "chat").await;
    assert_eq!(t["system_prompt"], BUILTIN_CHAT_SYSTEM_PROMPT, "{t:#}");
    let s: Value = base
        .client()
        .get(format!("{base}/api/settings-full"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["chat_system_prompt"], BUILTIN_CHAT_SYSTEM_PROMPT);
    assert_eq!(s["chat_system_prompt_builtin"], BUILTIN_CHAT_SYSTEM_PROMPT);

    // The owner's own: the next thread starts from it, the first keeps its copy.
    let (status, res) = op(
        &base,
        "settings_set_full",
        json!({ "chat_system_prompt": "  Be terse.  " }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let t2 = new_thread(&base, "chat").await;
    assert_eq!(t2["system_prompt"], "Be terse.");
    let first = thread(&base, t["id"].as_i64().unwrap()).await;
    assert_eq!(first["system_prompt"], BUILTIN_CHAT_SYSTEM_PROMPT);

    // An Admin Chat thread has its built-in prompt already: nothing seeded.
    let admin = new_thread(&base, "admin").await;
    assert_eq!(admin["system_prompt"], "", "{admin:#}");

    // Saving the built-in text back is the Reset: the stored override goes.
    let (status, res) = op(
        &base,
        "settings_set_full",
        json!({ "chat_system_prompt": BUILTIN_CHAT_SYSTEM_PROMPT }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert!(state.snapshot().settings.chat_system_prompt.is_none());

    // And an empty default is no prompt at all.
    let (status, _) = op(
        &base,
        "settings_set_full",
        json!({ "chat_system_prompt": "" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(new_thread(&base, "chat").await["system_prompt"], "");
}

#[tokio::test]
async fn the_prompt_placeholders_are_filled_in_at_send() {
    let mock = mock_upstream().await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "chat").await["id"].as_i64().unwrap();
    let (status, _) = patch(
        &base,
        tid,
        json!({ "system_prompt": "You are {{model}}. Today is {{date}}. {{kept}}" }),
    )
    .await;
    assert_eq!(status, 200);
    send(&base, tid, "hi").await;

    let body = last_upstream_body(&mock).await;
    let sys = body["messages"][0].clone();
    assert_eq!(sys["role"], "system", "{body:#}");
    let today = chrono::Local::now()
        .date_naive()
        .format("%A, %-d %B %Y")
        .to_string();
    assert_eq!(
        sys["content"],
        format!("You are my-model. Today is {today}. {{{{kept}}}}")
    );
    // The thread's own copy still holds the placeholders.
    assert_eq!(
        thread(&base, tid).await["system_prompt"],
        "You are {{model}}. Today is {{date}}. {{kept}}"
    );
}

#[tokio::test]
async fn thread_reasoning_overrides_go_out_like_the_headers() {
    let mock = mock_upstream().await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "chat").await["id"].as_i64().unwrap();

    // None set: no reasoning key goes out, and nothing is reported ignored.
    let done = done_frame(&send(&base, tid, "one").await);
    let body = last_upstream_body(&mock).await;
    assert!(body.get("reasoning_effort").is_none(), "{body:#}");
    assert_eq!(done["reasoning_ignored"], json!([]), "{done:#}");

    // An effort and a budget: a generic OpenAI route sends the effort and has
    // no field for the budget — which the turn says.
    let (status, res) = patch(
        &base,
        tid,
        json!({ "reasoning_effort": " high ", "reasoning_budget": 1024 }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let t = thread(&base, tid).await;
    assert_eq!(t["reasoning_effort"], "high", "trimmed: {t:#}");
    assert_eq!(t["reasoning_budget"], 1024);
    assert!(t["reasoning_enabled"].is_null());
    let done = done_frame(&send(&base, tid, "two").await);
    let body = last_upstream_body(&mock).await;
    assert_eq!(body["reasoning_effort"], "high", "{body:#}");
    assert_eq!(done["reasoning_ignored"], json!(["budget"]), "{done:#}");

    // Off: sent as effort "none", the other two cleared by `null`.
    let (status, res) = patch(
        &base,
        tid,
        json!({ "reasoning_enabled": false, "reasoning_effort": null, "reasoning_budget": null }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    send(&base, tid, "three").await;
    let body = last_upstream_body(&mock).await;
    assert_eq!(body["reasoning_effort"], "none", "{body:#}");

    // A patch that leaves them out leaves them alone.
    patch(&base, tid, json!({ "temperature": 0.2 })).await;
    let t = thread(&base, tid).await;
    assert_eq!(t["reasoning_enabled"], false, "{t:#}");
}

/// "Not sent" is judged on what goes out, the alias's defaults included: a
/// thread's bare "on" rides on the alias's own effort on a generic OpenAI
/// route, so it is sent and must not be reported otherwise.
#[tokio::test]
async fn an_override_the_alias_defaults_carry_is_not_reported_ignored() {
    let mock = mock_upstream().await;
    let (state, base) = setup(&mock.uri()).await;
    let up_id = store::list_upstreams(&state.db).await.unwrap()[0].id;
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "with-effort".into(),
            upstream_id: up_id,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Params {
                reasoning: Some(ReasoningControl {
                    effort: Some("medium".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let tid = new_thread(&base, "chat").await["id"].as_i64().unwrap();
    let (status, res) = patch(
        &base,
        tid,
        json!({ "model_alias": "with-effort", "reasoning_enabled": true }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let done = done_frame(&send(&base, tid, "hi").await);
    let body = last_upstream_body(&mock).await;
    assert_eq!(body["reasoning_effort"], "medium", "{body:#}");
    assert_eq!(done["reasoning_ignored"], json!([]), "{done:#}");

    // On the plain alias the same bare "on" has no field to go out in.
    patch(&base, tid, json!({ "model_alias": "my-model" })).await;
    let done = done_frame(&send(&base, tid, "again").await);
    assert_eq!(done["reasoning_ignored"], json!(["enabled"]), "{done:#}");
}

#[tokio::test]
async fn contradicting_overrides_are_refused_and_nothing_is_written() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "chat").await["id"].as_i64().unwrap();

    for body in [
        json!({ "reasoning_enabled": true, "reasoning_effort": "none" }),
        json!({ "reasoning_enabled": true, "reasoning_budget": 0 }),
        json!({ "reasoning_enabled": false, "reasoning_effort": "high" }),
        json!({ "reasoning_enabled": false, "reasoning_budget": 256 }),
        json!({ "reasoning_budget": -5 }),
    ] {
        let (status, res) = patch(&base, tid, body.clone()).await;
        assert_eq!(status, 400, "{body} → {res}");
        assert_eq!(res["code"], "bad_request", "{res}");
    }
    // Judged against what is stored, too: "off" beside a stored effort.
    let (status, _) = patch(&base, tid, json!({ "reasoning_effort": "low" })).await;
    assert_eq!(status, 200);
    let (status, res) = patch(
        &base,
        tid,
        json!({ "reasoning_enabled": false, "system_prompt": "x" }),
    )
    .await;
    assert_eq!(status, 400, "{res}");
    let t = thread(&base, tid).await;
    assert!(t["reasoning_enabled"].is_null(), "{t:#}");
    assert_ne!(
        t["system_prompt"], "x",
        "a refused patch writes nothing: {t:#}"
    );
}
