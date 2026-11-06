//! Chaining and conversations on the native path are key-scoped (openai
//! egress design §6): an id another key made, stored by lmgw or recorded as a
//! native response, answers exactly as an unknown id; the owner may name ids
//! lmgw never saw. Items, files, vector stores and containers a body names
//! are scoped the same way (§6.1).

use super::*;

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

/// The error message with the named ids blanked, to compare two answers.
fn said(v: &Value, ids: &[&str]) -> String {
    let mut m = v["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    for id in ids {
        m = m.replace(id, "ID");
    }
    m
}

#[tokio::test]
async fn another_keys_native_id_as_previous_response_id_looks_unknown() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let alice = key(&state, "alice").await;
    let bob = key(&state, "bob").await;
    let (status, _) = create(&gw, Some(&alice), json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 200);
    assert_eq!(up.responses().len(), 1);

    let chain = |id: &str| json!({"model": ALIAS, "input": "and?", "previous_response_id": id});
    let theirs = create(&gw, Some(&bob), chain("resp_upstream")).await;
    let unknown = create(&gw, Some(&bob), chain("resp_nothing")).await;
    assert_eq!(theirs.0, 404, "{:?}", theirs.1);
    assert_eq!(
        (theirs.0, said(&theirs.1, &["resp_upstream"])),
        (unknown.0, said(&unknown.1, &["resp_nothing"]))
    );
    assert_eq!(up.responses().len(), 1, "nothing of bob's went upstream");

    // Alice continues her own natively.
    let (status, _) = create(&gw, Some(&alice), chain("resp_upstream")).await;
    assert_eq!(status, 200);
    assert_eq!(up.responses().len(), 2);
    assert_eq!(up.responses()[1]["previous_response_id"], "resp_upstream");
    assert_eq!(up.chats(), 0);
}

#[tokio::test]
async fn the_owner_may_chain_from_an_id_lmgw_never_saw() {
    let (base, up) = upstream(Says::Answers).await;
    let (_state, gw) = gateway(Protocol::Openai, &base).await;
    let (status, _) = create(
        &gw,
        None,
        json!({"model": ALIAS, "input": "hi", "previous_response_id": "resp_outside"}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(up.responses()[0]["previous_response_id"], "resp_outside");
}

#[tokio::test]
async fn a_conversation_is_not_a_client_keys_but_the_owners_passes() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let bob = key(&state, "bob").await;
    let with = |c: Value| json!({"model": ALIAS, "input": "hi", "conversation": c});

    let by_id = create(&gw, Some(&bob), with(json!("conv_abc"))).await;
    let by_object = create(&gw, Some(&bob), with(json!({"id": "conv_abc"}))).await;
    let unknown = create(
        &gw,
        Some(&bob),
        json!({"model": ALIAS, "input": "and?", "previous_response_id": "resp_nothing"}),
    )
    .await;
    assert_eq!(by_id.0, 404, "{:?}", by_id.1);
    assert_eq!(by_object.0, 404);
    assert_eq!(
        said(&by_id.1, &["conv_abc"]),
        said(&unknown.1, &["resp_nothing"])
    );
    assert_eq!(
        said(&by_id.1, &["conv_abc"]),
        said(&by_object.1, &["conv_abc"])
    );
    assert!(up.responses().is_empty());

    let (status, _) = create(&gw, None, with(json!("conv_abc"))).await;
    assert_eq!(status, 200);
    assert_eq!(up.responses()[0]["conversation"], "conv_abc");
}

/// A later answer carrying an id that is already recorded does not take the
/// record over: a compatible upstream that reuses ids cannot hand one key's
/// response to another.
#[tokio::test]
async fn the_first_record_of_a_native_id_stays() {
    let (base, _up) = upstream(Says::Answers).await;
    let (state, _gw) = gateway(Protocol::Openai, &base).await;
    let record = |key_id| store::NativeResponse {
        id: "resp_reused".into(),
        upstream_id: 1,
        key_id: Some(key_id),
        model: ALIAS.into(),
        upstream_model: "gpt-test".into(),
        stored: true,
    };
    store::insert_native_response(&state.db, &record(1))
        .await
        .unwrap();
    store::insert_native_response(&state.db, &record(2))
        .await
        .unwrap();
    let kept = store::get_native_response(&state.db, "resp_reused", store::ResponseReader::Owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.key_id, Some(1));
}

/// An item reference names an item of a stored response: a client key may
/// name one only when a native answer it was given carried that item, and
/// otherwise gets the answer an unknown id gets, beside another key's chain
/// or its own alike. The owner passes (openai egress design §6.1).
#[tokio::test]
async fn an_item_reference_needs_an_item_the_key_was_given() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let alice = key(&state, "alice").await;
    let bob = key(&state, "bob").await;
    let (status, _) = create(&gw, Some(&alice), json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 200);
    let referencing = |prev: Option<&str>, item: &Value| {
        let mut b = json!({"model": ALIAS, "input": [
            item, {"role": "user", "content": "and?"}]});
        if let Some(p) = prev {
            b["previous_response_id"] = json!(p);
        }
        b
    };
    let typed = json!({"type": "item_reference", "id": "msg_1"});
    let untyped = json!({"id": "msg_1"});
    let unknown = create(
        &gw,
        Some(&bob),
        json!({"model": ALIAS, "input": "and?", "previous_response_id": "resp_nothing"}),
    )
    .await;

    for item in [&typed, &untyped] {
        let alone = create(&gw, Some(&bob), referencing(None, item)).await;
        assert_eq!(alone.0, 404, "{:?}", alone.1);
        assert_eq!(
            said(&alone.1, &["msg_1"]),
            said(&unknown.1, &["resp_nothing"])
        );
    }
    // Beside alice's chain, bob meets the chain's own refusal.
    let theirs = create(&gw, Some(&bob), referencing(Some("resp_upstream"), &typed)).await;
    assert_eq!(theirs.0, 404);
    assert_eq!(up.responses().len(), 1, "nothing of bob's went upstream");

    // Alice references an item her answer carried, with her chain or
    // without; an item no answer of hers carried is unknown to her too.
    let (status, body) = create(
        &gw,
        Some(&alice),
        referencing(Some("resp_upstream"), &typed),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(up.responses()[1]["input"][0], typed);
    let (status, body) = create(&gw, Some(&alice), referencing(None, &untyped)).await;
    assert_eq!(status, 200, "{body}");
    let other = json!({"type": "item_reference", "id": "msg_other"});
    let (status, _) = create(
        &gw,
        Some(&alice),
        referencing(Some("resp_upstream"), &other),
    )
    .await;
    assert_eq!(
        status, 404,
        "her chain does not vouch for an item it never carried"
    );
    // The owner any item.
    let (status, _) = create(&gw, None, referencing(None, &other)).await;
    assert_eq!(status, 200);
    assert_eq!(up.responses()[3]["input"][0], other);
    assert_eq!(up.chats(), 0);
}

/// An input item that carries an `id` is resolved by the upstream against
/// what it stores, as an item reference is: a `reasoning` item's `rs_…`
/// (with its `encrypted_content` too), a hosted call's. A
/// client key may replay the items its own answers carried, a `store: false`
/// answer's included, and gets the unknown-id answer for any other; the
/// owner passes (openai egress design §6.1).
#[tokio::test]
async fn an_input_items_id_needs_an_item_the_key_was_given() {
    let (base, up) = upstream(Says::AnswersWith(&["reasoning", "web_search_call"])).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let alice = key(&state, "alice").await;
    let bob = key(&state, "bob").await;
    // Alice's answer, not stored upstream: its items are hers all the same.
    let (status, _) = create(
        &gw,
        Some(&alice),
        json!({"model": ALIAS, "input": "hi", "store": false}),
    )
    .await;
    assert_eq!(status, 200);
    let replay = json!({"model": ALIAS, "input": [
        {"role": "user", "content": "hi"},
        {"type": "reasoning", "id": "call_0", "summary": [], "encrypted_content": "gAAA"},
        {"type": "web_search_call", "id": "call_1", "status": "completed"},
        {"type": "message", "id": "msg_1", "role": "assistant",
         "content": [{"type": "output_text", "text": "Hello"}]},
        {"role": "user", "content": "and?"},
    ]});
    let unknown = create(
        &gw,
        Some(&bob),
        json!({"model": ALIAS, "input": "and?", "previous_response_id": "resp_nothing"}),
    )
    .await;
    let theirs = create(&gw, Some(&bob), replay.clone()).await;
    assert_eq!(theirs.0, 404, "{:?}", theirs.1);
    assert_eq!(
        said(&theirs.1, &["call_0"]),
        said(&unknown.1, &["resp_nothing"])
    );
    let one = json!({"model": ALIAS, "input": [
        {"type": "reasoning", "id": "rs_owners", "summary": [], "encrypted_content": "gAAA"},
        {"role": "user", "content": "go on"}]});
    let (status, _) = create(&gw, Some(&alice), one).await;
    assert_eq!(status, 404, "a reasoning id no answer of hers carried");
    // Her own reasoning item's id with encrypted reasoning it never carried
    // (WP8 review S-R3).
    let mut forged = replay.clone();
    forged["input"][1]["encrypted_content"] = json!("gAAB");
    let (status, _) = create(&gw, Some(&alice), forged).await;
    assert_eq!(status, 404, "her id, another item's encrypted content");
    assert_eq!(up.responses().len(), 1, "nothing refused went upstream");
    // A reasoning item without encrypted content, whose id no answer of
    // hers carried, names nothing she may send: it is left out (WP8 verify
    // V-F1), as the loop's own summary-only items are.
    let summary_only = json!({"model": ALIAS, "input": [
        {"type": "reasoning", "id": "rs_owners", "summary": []},
        {"role": "user", "content": "go on"}]});
    let (status, body) = create(&gw, Some(&alice), summary_only).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        up.responses()[1]["input"],
        json!([{"role": "user", "content": "go on"}])
    );

    let (status, body) = create(&gw, Some(&alice), replay.clone()).await;
    assert_eq!(status, 200, "{body}");
    // An assistant message is whole, no reference: one whose id no answer
    // of hers carried goes without it (WP8 review W-R1, `loop_items`).
    let message = json!({"model": ALIAS, "input": [
        {"type": "message", "id": "msg_owners", "role": "assistant",
         "content": [{"type": "output_text", "text": "x"}]},
        {"role": "user", "content": "go on"}]});
    let (status, body) = create(&gw, Some(&alice), message).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        up.responses()[3]["input"][0],
        json!({"role": "assistant", "content": "x"})
    );
    let (status, _) = create(
        &gw,
        None,
        json!({"model": ALIAS, "input": [{"type": "reasoning", "id": "rs_outside", "summary": []},
                                         {"role": "user", "content": "go on"}]}),
    )
    .await;
    assert_eq!(status, 200, "the owner names any item");
    // One no answer lmgw relayed carried is unknown to her as well: left out.
    assert_eq!(
        up.responses()[4]["input"],
        json!([{"role": "user", "content": "go on"}])
    );
    assert_eq!(up.responses().len(), 5);

    // The unstored answer is no response to fetch or continue.
    let (status, _) = create(
        &gw,
        Some(&alice),
        json!({"model": ALIAS, "input": "and?", "previous_response_id": "resp_upstream"}),
    )
    .await;
    assert_eq!(status, 404);
}

/// lmgw creates no prompt templates, so a `prompt` is never a client key's:
/// it answers as an unknown id, as a `conversation` does, and a file in its
/// `variables` is the owner's like any other. The owner passes.
#[tokio::test]
async fn a_prompt_template_is_not_a_client_keys_but_the_owners_passes() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let bob = key(&state, "bob").await;
    let unknown = create(
        &gw,
        Some(&bob),
        json!({"model": ALIAS, "input": "and?", "previous_response_id": "resp_nothing"}),
    )
    .await;
    let prompt = json!({"model": ALIAS, "input": "hi",
                        "prompt": {"id": "pmpt_example", "version": "2"}});
    let (status, answer) = create(&gw, Some(&bob), prompt.clone()).await;
    assert_eq!(status, 404, "{answer}");
    assert_eq!(
        said(&answer, &["pmpt_example"]),
        said(&unknown.1, &["resp_nothing"])
    );
    assert!(up.responses().is_empty());
    let (status, _) = create(&gw, None, prompt).await;
    assert_eq!(status, 200);
    assert_eq!(up.responses()[0]["prompt"]["id"], "pmpt_example");
}

/// lmgw proxies none of the Files, Vector Stores, Containers or Skills APIs,
/// so no client key made one: a client key that names one, anywhere in the
/// body, gets the answer an unknown id gets, beside its own chain too. The
/// owner passes.
#[tokio::test]
async fn files_vector_stores_and_containers_are_the_owners() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let alice = key(&state, "alice").await;
    let (status, _) = create(&gw, Some(&alice), json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 200);
    let unknown = create(
        &gw,
        Some(&alice),
        json!({"model": ALIAS, "input": "and?", "previous_response_id": "resp_nothing"}),
    )
    .await;
    let part = |p: Value| {
        json!({"model": ALIAS, "previous_response_id": "resp_upstream",
               "input": [{"role": "user", "content": [
                   {"type": "input_text", "text": "read this"}, p]}]})
    };
    let tool = |t: Value| json!({"model": ALIAS, "input": "go", "tools": [t]});
    let cases = [
        (
            part(json!({"type": "input_file", "file_id": "file-example1"})),
            "file-example1",
        ),
        (
            part(json!({"type": "input_image", "file_id": "file-example2"})),
            "file-example2",
        ),
        (
            tool(json!({"type": "file_search", "vector_store_ids": ["vs_example"]})),
            "vs_example",
        ),
        (
            tool(json!({"type": "code_interpreter", "container": "cntr_example"})),
            "cntr_example",
        ),
        (
            tool(
                json!({"type": "shell", "environment": {"type": "container_auto",
                "skills": [{"type": "skill_reference", "skill_id": "skill_example"}]}}),
            ),
            "skill_example",
        ),
        (
            json!({"model": ALIAS, "input": "go", "text": {"verbosity": "low"},
                   "metadata": {"note": "x"},
                   "tools": [{"type": "function", "name": "f", "parameters": {
                       "type": "object", "properties": {}}}],
                   "include": ["file_search_call.results"],
                   "reasoning": {"effort": "low"},
                   "safety_identifier": "u1",
                   "x": {"deep": [{"type": "input_file", "file_id": "file-example3"}]}}),
            "file-example3",
        ),
    ];
    for (body, id) in &cases {
        let (status, answer) = create(&gw, Some(&alice), body.clone()).await;
        assert_eq!(status, 404, "{id}: {answer}");
        assert_eq!(
            said(&answer, &[id]),
            said(&unknown.1, &["resp_nothing"]),
            "{id}"
        );
    }
    assert_eq!(up.responses().len(), 1, "nothing of alice's went upstream");
    for (body, id) in &cases {
        let (status, answer) = create(&gw, None, body.clone()).await;
        assert_eq!(status, 200, "the owner's {id}: {answer}");
    }
    assert_eq!(up.responses().len(), 1 + cases.len());
}

/// OpenAI encrypts a reasoning item anew each time it writes it (api-compat
/// design §9, finding 9): a stream's `response.output_item.done` and
/// its `response.completed` carry different `encrypted_content` for the
/// same `rs_…` id. A stateless client replays whichever it kept — the SDK's
/// final response is the terminal event's — so either passes; encrypted
/// content the answer never carried still answers as an unknown id.
#[tokio::test]
async fn a_streamed_reasoning_item_replays_with_either_of_its_encryptions() {
    use crate::support::responses_wire as rw;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let reasoning = |enc: &str| json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": enc});
    let mock = MockServer::start().await;
    let events = [
        json!({"type": "response.created", "response": {"id": "resp_s", "object": "response",
            "status": "in_progress", "output": []}}),
        json!({"type": "response.output_item.done", "output_index": 0,
            "item": reasoning("gAAA-done")}),
        json!({"type": "response.output_item.done", "output_index": 1,
            "item": rw::message("msg_1", "Hello")}),
        json!({"type": "response.completed", "response": rw::response("resp_s", "completed",
            json!([reasoning("gAAA-final"), rw::message("msg_1", "Hello")]))}),
    ];
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(rw::sse(&events), "text/event-stream"),
        )
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(rw::response(
            "resp_w",
            "completed",
            json!([rw::message("msg_2", "Again")]),
        )))
        .with_priority(2)
        .mount(&mock)
        .await;
    let (state, gw) = gateway(Protocol::Openai, &mock.uri()).await;
    let alice = key(&state, "alice").await;

    let resp = gw
        .anon()
        .post(format!("{gw}/v1/responses"))
        .bearer_auth(&alice)
        .json(&json!({"model": ALIAS, "input": "hi", "stream": true, "store": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("response.completed"), "{text}");

    let replay = |enc: &str| {
        json!({"model": ALIAS, "store": false, "input": [
            {"role": "user", "content": "hi"},
            reasoning(enc),
            {"type": "message", "id": "msg_1", "role": "assistant",
             "content": [{"type": "output_text", "text": "Hello"}]},
            {"role": "user", "content": "and?"},
        ]})
    };
    for enc in ["gAAA-final", "gAAA-done"] {
        let (status, body) = create(&gw, Some(&alice), replay(enc)).await;
        assert_eq!(status, 200, "{enc}: {body}");
    }
    let (status, body) = create(&gw, Some(&alice), replay("gAAA-other")).await;
    assert_eq!(status, 404, "{body}");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("rs_1"),
        "{body}"
    );
}
