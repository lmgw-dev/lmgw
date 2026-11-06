//! Decision 12's check (openai egress design §6): on both protocols that
//! forward `/v1/responses`, a request only lmgw's own loop can serve stays
//! in the loop, and every other one goes native.

use crate::support::mcp_stub::{echo_stub, register};

use super::*;

/// A response lmgw's loop stored, to be continued.
pub(super) const STORED: &str = "resp_lmgw_stored";

pub(super) async fn store_response(state: &SharedState) {
    store::insert_response(
        &state.db,
        &store::StoredResponse {
            id: STORED.into(),
            chain_id: STORED.into(),
            previous_response_id: None,
            model: ALIAS.into(),
            status: "completed".into(),
            body: "{}".into(),
            input_items: "[]".into(),
            messages: "[]".into(),
            pending: None,
            input_tokens: None,
            output_tokens: None,
            created_at: String::new(),
            key_id: None,
        },
    )
    .await
    .unwrap();
}

/// The answer came from lmgw's loop: one chat turn upstream, no native call.
fn from_the_loop(up: &Upstream, body: &Value) {
    assert_eq!(up.responses().len(), 0, "nothing native: {body}");
    assert_eq!(up.chats(), 1, "one loop turn: {body}");
    assert_eq!(
        lmgw_core::ingress::responses::output_text(body),
        "from the loop",
        "{body}"
    );
}

#[tokio::test]
async fn an_mcp_label_lmgw_serves_stays_in_the_loop() {
    for protocol in NATIVE {
        let (base, up) = upstream(Says::Answers).await;
        let (state, gw) = gateway(protocol, &base).await;
        let mcp = echo_stub().await;
        register(&state, "stub", "tools", &mcp.url, true, None).await;

        let (status, body) = post(
            &gw,
            json!({"model": ALIAS, "input": "hi",
                   "tools": [{"type": "mcp", "server_label": "tools"}]}),
        )
        .await;
        assert_eq!(status, 200, "{protocol:?}: {body}");
        from_the_loop(&up, &body);
    }
}

/// A built-in toolset is a label lmgw serves too.
#[tokio::test]
async fn a_builtin_label_stays_in_the_loop() {
    let (base, up) = upstream(Says::Answers).await;
    let (_state, gw) = gateway(Protocol::Openai, &base).await;
    let (status, body) = post(
        &gw,
        json!({"model": ALIAS, "input": "hi",
               "tools": [{"type": "mcp", "server_label": "docs"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    from_the_loop(&up, &body);
}

#[tokio::test]
async fn a_chain_lmgw_stored_continues_in_the_loop() {
    for protocol in NATIVE {
        let (base, up) = upstream(Says::Answers).await;
        let (state, gw) = gateway(protocol, &base).await;
        store_response(&state).await;

        let (status, body) = post(
            &gw,
            json!({"model": ALIAS, "input": "and then?", "previous_response_id": STORED}),
        )
        .await;
        assert_eq!(status, 200, "{protocol:?}: {body}");
        from_the_loop(&up, &body);
    }
}

/// An id lmgw does not hold is the upstream's own chain: forwarded with the
/// body, so the upstream continues it.
#[tokio::test]
async fn a_chain_only_the_upstream_holds_goes_native() {
    for protocol in NATIVE {
        let (base, up) = upstream(Says::Answers).await;
        let (_state, gw) = gateway(protocol, &base).await;
        let (status, body) = post(
            &gw,
            json!({"model": ALIAS, "input": "and then?",
                   "previous_response_id": "resp_upstream_earlier"}),
        )
        .await;
        assert_eq!(status, 200, "{protocol:?}: {body}");
        assert_eq!(up.chats(), 0, "{protocol:?}");
        let sent = up.responses();
        assert_eq!(sent.len(), 1, "{protocol:?}");
        assert_eq!(sent[0]["previous_response_id"], "resp_upstream_earlier");
    }
}

/// A label lmgw does not serve is the upstream's hosted MCP tool: the block
/// goes with the body.
#[tokio::test]
async fn an_mcp_label_lmgw_does_not_serve_goes_native() {
    let (base, up) = upstream(Says::Answers).await;
    let (_state, gw) = gateway(Protocol::Openai, &base).await;
    let tool = json!({"type": "mcp", "server_label": "wiki",
                      "server_url": "https://mcp.example/sse"});
    let (status, body) = post(
        &gw,
        json!({"model": ALIAS, "input": "hi", "tools": [tool.clone()]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(up.chats(), 0);
    let sent = up.responses();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["tools"], json!([tool]));
}

/// Tools only the loop can run, on a chain only the upstream holds: neither
/// path serves both, and the refusal names both.
#[tokio::test]
async fn loop_tools_on_an_upstream_chain_are_refused() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let mcp = echo_stub().await;
    register(&state, "stub", "tools", &mcp.url, true, None).await;

    let (status, body) = post(
        &gw,
        json!({"model": ALIAS, "input": "hi", "previous_response_id": "resp_upstream_earlier",
               "tools": [{"type": "mcp", "server_label": "tools"}]}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("'tools'"), "names the label: {msg}");
    assert!(
        msg.contains("resp_upstream_earlier"),
        "names the chain: {msg}"
    );
    assert_eq!((up.responses().len(), up.chats()), (0, 0));
    let row = row(&state, 1).await;
    assert_eq!(row.status, 400, "the refusal has its row: {row:?}");
}

/// A client key named `name` (plaintext `lmgw-<name>`) whose tool scope
/// allows `patterns`.
async fn scoped_key(state: &SharedState, gw: &Gw, name: &str, patterns: &str) -> String {
    let id = store::insert_api_key(
        &state.db,
        name,
        &lmgw_core::config::hash_api_key(&format!("lmgw-{name}")),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/key_set"))
        .json(&json!({"id": id, "tool_scope_mode": "allow", "tool_scope_patterns": patterns}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    format!("lmgw-{name}")
}

/// `POST /v1/responses` as `token`.
async fn post_as(gw: &Gw, token: &str, body: Value) -> (u16, Value) {
    let resp = gw
        .anon()
        .post(format!("{gw}/v1/responses"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    (
        resp.status().as_u16(),
        resp.json().await.unwrap_or(Value::Null),
    )
}

/// A bare server (no tool prefix) that lists a scoped key nothing is not
/// there for that key, as `resolve` answers it: its label goes native like
/// one nothing registers, so the path does not tell the key it exists. The
/// same server listing the key its tool keeps the request in the loop.
#[tokio::test]
async fn a_bare_server_a_scoped_key_cannot_use_looks_like_an_unknown_label() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let mcp = echo_stub().await;
    register(&state, "bare", "", &mcp.url, true, None).await;
    let shut_out = scoped_key(&state, &gw, "narrow", "nothing_here").await;

    for label in ["bare", "never-registered"] {
        let (status, body) = post_as(
            &gw,
            &shut_out,
            json!({"model": ALIAS, "input": "hi",
                   "tools": [{"type": "mcp", "server_label": label}]}),
        )
        .await;
        assert_eq!(status, 200, "{label}: {body}");
    }
    assert_eq!(up.chats(), 0, "neither label went to the loop");
    assert_eq!(up.responses().len(), 2, "both went native");

    let let_in = scoped_key(&state, &gw, "wide", "echo").await;
    let (status, body) = post_as(
        &gw,
        &let_in,
        json!({"model": ALIAS, "input": "hi",
               "tools": [{"type": "mcp", "server_label": "bare"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        up.chats(),
        1,
        "a key the server lists something stays in the loop"
    );
    assert_eq!(up.responses().len(), 2);
}

/// An `mcp` block with a `server_url` that is not lmgw's, or a
/// `connector_id`, is the upstream's to dial even when its label is one lmgw
/// serves: it goes native with the body, never to lmgw's same-label server.
#[tokio::test]
async fn a_foreign_server_url_or_connector_goes_native_whatever_its_label() {
    let blocks = [
        json!({"type": "mcp", "server_label": "tools", "server_url": "https://mcp.example/mcp"}),
        json!({"type": "mcp", "server_label": "tools", "connector_id": "connector_dropbox"}),
        json!({"type": "mcp", "server_label": "docs", "server_url": "https://mcp.example/mcp"}),
    ];
    for tool in blocks {
        let (base, up) = upstream(Says::Answers).await;
        let (state, gw) = gateway(Protocol::Openai, &base).await;
        let mcp = echo_stub().await;
        register(&state, "stub", "tools", &mcp.url, true, None).await;
        let (status, body) = post(
            &gw,
            json!({"model": ALIAS, "input": "hi", "tools": [tool.clone()]}),
        )
        .await;
        assert_eq!(status, 200, "{tool}: {body}");
        assert_eq!(up.chats(), 0, "{tool}");
        let sent = up.responses();
        assert_eq!(sent.len(), 1, "{tool}");
        assert_eq!(sent[0]["tools"], json!([tool]));
    }
}

/// A `server_url` naming lmgw's own `/mcp` reads as no url: the label is
/// lmgw's, and the request stays in the loop.
#[tokio::test]
async fn lmgw_s_own_mcp_url_is_read_by_its_label() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let mcp = echo_stub().await;
    register(&state, "stub", "tools", &mcp.url, true, None).await;
    let (status, body) = post(
        &gw,
        json!({"model": ALIAS, "input": "hi",
               "tools": [{"type": "mcp", "server_label": "tools",
                          "server_url": format!("{gw}/mcp")}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    from_the_loop(&up, &body);
}

/// On an upstream that does not take `/v1/responses` natively the loop
/// serves the request, and a block only an upstream could serve is refused
/// naming it, not answered by lmgw's same-label server.
#[tokio::test]
async fn a_foreign_server_url_on_the_loop_path_is_refused() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway_flagged(Protocol::OpenaiCompatible, &base, false).await;
    let mcp = echo_stub().await;
    register(&state, "stub", "tools", &mcp.url, true, None).await;
    let (status, body) = post(
        &gw,
        json!({"model": ALIAS, "input": "hi",
               "tools": [{"type": "mcp", "server_label": "tools",
                          "server_url": "https://mcp.example/mcp"}]}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("'tools'"), "names the block: {msg}");
    assert!(msg.contains("server_url"), "says what is foreign: {msg}");
    assert!(
        msg.contains("native-up"),
        "says why it is in the loop: {msg}"
    );
    assert_eq!((up.responses().len(), up.chats()), (0, 0));
}

/// The same refusal when the loop is needed by another block.
#[tokio::test]
async fn a_foreign_block_beside_a_loop_block_is_refused() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let mcp = echo_stub().await;
    register(&state, "stub", "tools", &mcp.url, true, None).await;
    let (status, body) = post(
        &gw,
        json!({"model": ALIAS, "input": "hi",
               "tools": [{"type": "mcp", "server_label": "tools"},
                         {"type": "mcp", "server_label": "wiki",
                          "connector_id": "connector_dropbox"}]}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("'wiki'") && msg.contains("connector_id"),
        "{msg}"
    );
    assert!(msg.contains("'tools'"), "names what needs the loop: {msg}");
    assert_eq!((up.responses().len(), up.chats()), (0, 0));
}
