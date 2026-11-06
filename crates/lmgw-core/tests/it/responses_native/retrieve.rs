//! `GET`, `DELETE /v1/responses/{id}` and `…/input_items` (openai egress
//! design §6): a response an upstream made natively is forwarded to that
//! upstream, and every response, stored or native, is read as its key's.

use super::*;

/// A client key named `name`; its bearer.
async fn key(state: &SharedState, name: &str) -> String {
    store::insert_api_key(
        &state.db,
        name,
        &lmgw_core::config::hash_api_key(&format!("lmgw-{name}")),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    format!("lmgw-{name}")
}

/// `method` on `path` as `token` (`None`: the dashboard's owner key).
async fn call(gw: &Gw, token: Option<&str>, method: reqwest::Method, path: &str) -> (u16, Value) {
    let rb = match token {
        Some(t) => gw
            .anon()
            .request(method, format!("{gw}{path}"))
            .bearer_auth(t),
        None => gw.client().request(method, format!("{gw}{path}")),
    };
    let resp = rb.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn create(gw: &Gw, token: Option<&str>, body: Value) -> (u16, Value) {
    let rb = match token {
        Some(t) => gw.anon().post(format!("{gw}/v1/responses")).bearer_auth(t),
        None => gw.client().post(format!("{gw}/v1/responses")),
    };
    let resp = rb.json(&body).send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// The response a client just made natively can be fetched, its input items
/// listed and it deleted, each forwarded to the upstream that made it.
#[tokio::test]
async fn a_native_response_is_fetched_from_its_upstream() {
    for stream in [false, true] {
        let (base, up) = upstream(Says::Answers).await;
        let (state, gw) = gateway(Protocol::Openai, &base).await;
        let alice = key(&state, "alice").await;
        let (status, _) = create(
            &gw,
            Some(&alice),
            json!({"model": ALIAS, "input": "hi", "stream": stream}),
        )
        .await;
        assert_eq!(status, 200);

        let (status, body) = call(
            &gw,
            Some(&alice),
            reqwest::Method::GET,
            "/v1/responses/resp_upstream?include[]=x",
        )
        .await;
        assert_eq!(status, 200, "stream {stream}: {body}");
        assert_eq!(
            body["upstream_saw"],
            "GET /v1/responses/resp_upstream?include[]=x"
        );
        let (status, body) = call(
            &gw,
            Some(&alice),
            reqwest::Method::GET,
            "/v1/responses/resp_upstream/input_items?limit=5",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body["upstream_saw"],
            "GET /v1/responses/resp_upstream/input_items?limit=5"
        );
        // The owner sees every key's.
        let (status, _) = call(
            &gw,
            None,
            reqwest::Method::GET,
            "/v1/responses/resp_upstream",
        )
        .await;
        assert_eq!(status, 200);

        let (status, body) = call(
            &gw,
            Some(&alice),
            reqwest::Method::DELETE,
            "/v1/responses/resp_upstream",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["upstream_saw"], "DELETE /v1/responses/resp_upstream");
        // Forgotten once the upstream deleted it.
        let (status, _) = call(
            &gw,
            Some(&alice),
            reqwest::Method::GET,
            "/v1/responses/resp_upstream",
        )
        .await;
        assert_eq!(status, 404);
        assert_eq!(up.fetched.lock().unwrap().len(), 4, "stream {stream}");
    }
}

/// A relayed answer is the upstream's, type and body as sent — and opened in
/// a window at the gateway's origin it runs nothing: a navigation carries the
/// owner's cookie, so the router's default sends `nosniff` and the sandbox
/// on it too (machine-check design WP13, D90).
#[tokio::test]
async fn a_relayed_page_keeps_its_type_and_body_and_is_sandboxed() {
    let (base, up) = upstream(Says::Answers).await;
    let (_state, gw) = gateway(Protocol::Openai, &base).await;
    let (status, _) = create(&gw, None, json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 200);

    for sub in ["", "/input_items"] {
        let resp = gw
            .client()
            .get(format!("{gw}/v1/responses/resp_upstream{sub}?as=html"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{sub}");
        assert_eq!(
            resp.headers()["content-type"],
            "text/html; charset=utf-8",
            "{sub}"
        );
        crate::support::stored::assert_sandboxed(resp.headers(), &format!("relayed {sub}"));
        let body = resp.text().await.unwrap();
        assert_eq!(
            body,
            format!(
                "<p>GET /v1/responses/resp_upstream{sub}?as=html</p>\
                 <script>fetch('/api/op/settings_set')</script>"
            ),
            "{sub}"
        );
    }
    assert_eq!(up.fetched.lock().unwrap().len(), 2);
}

/// `store: false` keeps nothing upstream, so nothing is remembered: the id
/// is lmgw's 404, and nothing is forwarded.
#[tokio::test]
async fn a_response_made_with_store_false_is_not_remembered() {
    let (base, up) = upstream(Says::Answers).await;
    let (_state, gw) = gateway(Protocol::Openai, &base).await;
    let (status, _) = create(
        &gw,
        None,
        json!({"model": ALIAS, "input": "hi", "store": false}),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = call(
        &gw,
        None,
        reqwest::Method::GET,
        "/v1/responses/resp_upstream",
    )
    .await;
    assert_eq!(status, 404);
    assert!(up.fetched.lock().unwrap().is_empty());
}

/// Another key's native response answers exactly as an unknown id: the same
/// status and the same body, on GET, input_items and DELETE, and nothing
/// reaches the upstream.
#[tokio::test]
async fn another_keys_native_response_looks_unknown() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let alice = key(&state, "alice").await;
    let bob = key(&state, "bob").await;
    create(&gw, Some(&alice), json!({"model": ALIAS, "input": "hi"})).await;

    for (method, sub) in [
        (reqwest::Method::GET, ""),
        (reqwest::Method::GET, "/input_items"),
        (reqwest::Method::DELETE, ""),
    ] {
        let theirs = call(
            &gw,
            Some(&bob),
            method.clone(),
            &format!("/v1/responses/resp_upstream{sub}"),
        )
        .await;
        let unknown = call(
            &gw,
            Some(&bob),
            method.clone(),
            &format!("/v1/responses/resp_nothing{sub}"),
        )
        .await;
        assert_eq!(theirs.0, 404, "{method} {sub}: {:?}", theirs.1);
        let said = |v: &Value| {
            v["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .replace("resp_upstream", "ID")
                .replace("resp_nothing", "ID")
        };
        assert_eq!(
            (theirs.0, said(&theirs.1)),
            (unknown.0, said(&unknown.1)),
            "{method} {sub}"
        );
    }
    assert!(up.fetched.lock().unwrap().is_empty());
}

/// A response lmgw's loop stored, made under `key_id`.
async fn stored_as(state: &SharedState, id: &str, key_id: Option<i64>) {
    store::insert_response(
        &state.db,
        &store::StoredResponse {
            id: id.into(),
            chain_id: id.into(),
            previous_response_id: None,
            model: ALIAS.into(),
            status: "completed".into(),
            body: json!({"id": id, "object": "response"}).to_string(),
            input_items: "[]".into(),
            messages: "[]".into(),
            pending: None,
            input_tokens: None,
            output_tokens: None,
            created_at: String::new(),
            key_id,
        },
    )
    .await
    .unwrap();
}

fn key_id(state: &SharedState, name: &str) -> i64 {
    state
        .snapshot()
        .api_keys
        .iter()
        .find(|k| k.name == name)
        .unwrap()
        .id
}

/// A stored response is its key's: another key reading, deleting or
/// continuing it gets what an unknown id gets; the owner gets every one.
#[tokio::test]
async fn a_stored_response_is_its_keys() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let alice = key(&state, "alice").await;
    let bob = key(&state, "bob").await;
    stored_as(&state, "resp_alice", Some(key_id(&state, "alice"))).await;

    let (status, body) = call(
        &gw,
        Some(&alice),
        reqwest::Method::GET,
        "/v1/responses/resp_alice",
    )
    .await;
    assert_eq!((status, &body["id"]), (200, &json!("resp_alice")));
    for (method, sub) in [
        (reqwest::Method::GET, ""),
        (reqwest::Method::GET, "/input_items"),
        (reqwest::Method::DELETE, ""),
    ] {
        let theirs = call(
            &gw,
            Some(&bob),
            method.clone(),
            &format!("/v1/responses/resp_alice{sub}"),
        )
        .await;
        let unknown = call(
            &gw,
            Some(&bob),
            method.clone(),
            &format!("/v1/responses/resp_nobody{sub}"),
        )
        .await;
        let said = |v: &Value| {
            v["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .replace("resp_alice", "ID")
                .replace("resp_nobody", "ID")
        };
        assert_eq!(theirs.0, 404, "{method} {sub}");
        assert_eq!(
            (theirs.0, said(&theirs.1)),
            (unknown.0, said(&unknown.1)),
            "{method} {sub}"
        );
    }

    // Continued by bob: alice's, so as unknown to him as an id nobody made
    // (`scoping.rs` has the whole matrix); it reaches neither path.
    let (status, _) = create(
        &gw,
        Some(&bob),
        json!({"model": ALIAS, "input": "and?", "previous_response_id": "resp_alice"}),
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(up.chats(), 0, "not the loop");
    assert!(up.responses().is_empty(), "not native either");
    // Continued by alice: her chain, in the loop.
    let (status, _) = create(
        &gw,
        Some(&alice),
        json!({"model": ALIAS, "input": "and?", "previous_response_id": "resp_alice"}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(up.chats(), 1, "the loop");

    // The owner reads and deletes every key's.
    let (status, _) = call(&gw, None, reqwest::Method::GET, "/v1/responses/resp_alice").await;
    assert_eq!(status, 200);
    let (status, _) = call(
        &gw,
        None,
        reqwest::Method::DELETE,
        "/v1/responses/resp_alice",
    )
    .await;
    assert_eq!(status, 200);
}

/// A row from before keys were kept (NULL) is not any client key's.
#[tokio::test]
async fn a_row_without_a_key_is_no_client_keys() {
    let (base, _up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let alice = key(&state, "alice").await;
    stored_as(&state, "resp_old", None).await;
    let (status, _) = call(
        &gw,
        Some(&alice),
        reqwest::Method::GET,
        "/v1/responses/resp_old",
    )
    .await;
    assert_eq!(status, 404);
    let (status, _) = call(&gw, None, reqwest::Method::GET, "/v1/responses/resp_old").await;
    assert_eq!(status, 200);
}

/// The loop stores its responses under the key that made them.
#[tokio::test]
async fn the_loop_stores_a_response_under_its_key() {
    let (base, _up) = upstream(Says::Answers).await;
    let (state, gw) = gateway_flagged(Protocol::OpenaiCompatible, &base, false).await;
    let alice = key(&state, "alice").await;
    let (status, body) = create(&gw, Some(&alice), json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 200, "{body}");
    let id = body["id"].as_str().unwrap();
    let row = store::get_response(&state.db, id, store::ResponseReader::Owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.key_id, Some(key_id(&state, "alice")));
    let (status, _) = call(
        &gw,
        Some(&alice),
        reqwest::Method::GET,
        &format!("/v1/responses/{id}"),
    )
    .await;
    assert_eq!(status, 200);
}
