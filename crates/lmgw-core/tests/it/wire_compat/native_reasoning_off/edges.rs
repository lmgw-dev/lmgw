//! The edges of the native off's retry (the review of 30be5de7): a refusal
//! that depends on what else the body carried is retried for its request
//! but teaches the route nothing, on native and on chat; a `5xx` whose
//! client left while it was read is the plain hang-up; a refusal whose body
//! never arrived earns no retry.

use super::*;

/// OpenAI's words for a hosted tool the off's level does not allow.
const TOOLS_REFUSED: &str =
    "The following tools cannot be used with reasoning.effort 'none': web_search.";

/// A refusal about the off together with a hosted tool the body carries is
/// retried, and answers, but the form that answered is that request's only:
/// a plain request of the route still sends `"none"`, native and chat.
#[tokio::test]
async fn a_refusal_that_names_a_hosted_tool_is_retried_but_teaches_nothing() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_partial_json(json!({"reasoning": {"effort": "none"},
            "tools": [{"type": "web_search"}]})))
        .respond_with(refusal(TOOLS_REFUSED, None, None))
        .with_priority(1)
        .mount(&mock)
        .await;
    answering(&mock).await;
    let (state, gw) = gateway(&mock, Protocol::Openai).await;
    let mut body = json!({"model": "my-model", "input": "Look it up.",
        "tools": [{"type": "web_search"}]});
    let (status, _, text) = post_with(&gw, "/v1/responses", &OFF, &body).await;
    assert_eq!(status, 200, "{text}");
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 2, "{sent:#?}");
    assert!(sent[1].get("reasoning").is_none(), "{}", sent[1]);
    let r = rows(&state).await;
    let note = r[0].1.as_deref().unwrap();
    assert!(note.contains(TOOLS_REFUSED), "{note}");
    assert!(
        note.contains("keeps nothing for this model")
            && note.contains("it also names 'web_search', which this request carried"),
        "{note}"
    );

    // Nothing learned: a plain request, native and chat, sends the off.
    body.as_object_mut().unwrap().remove("tools");
    let (status, headers, _) = post_with(&gw, "/v1/responses", &OFF, &body).await;
    assert_eq!(status, 200);
    assert!(header(&headers, "x-lmgw-reasoning-ignored").is_none());
    let chat = json!({"model": "my-model",
        "messages": [{"role": "user", "content": "Reply with OK."}]});
    let (status, _, text) = post_with(&gw, "/v1/chat/completions", &OFF, &chat).await;
    assert_eq!(status, 200, "{text}");
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 4, "{sent:#?}");
    assert_eq!(sent[2]["reasoning"]["effort"], "none");
    assert_eq!(sent[3]["reasoning"]["effort"], "none");
}

/// A bare refusal that names no control is retried with no control, as the
/// last resort; on a body that carried more than the control it may be
/// about that, so nothing is kept. On a plain body it is (the Gemini
/// budget case, `reasoning_off_fit.rs`).
#[tokio::test]
async fn a_bare_refusal_of_a_body_with_more_is_retried_but_not_kept() {
    let bare = "Request contains an invalid argument.";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_partial_json(json!({"reasoning": {"effort": "none"},
            "text": {"verbosity": "low"}})))
        .respond_with(refusal(bare, None, None))
        .with_priority(1)
        .mount(&mock)
        .await;
    answering(&mock).await;
    let (state, gw) = gateway(&mock, Protocol::Openai).await;
    let mut body = json!({"model": "my-model", "input": "Reply with OK.",
        "text": {"verbosity": "low"}});
    let (status, _, text) = post_with(&gw, "/v1/responses", &OFF, &body).await;
    assert_eq!(status, 200, "{text}");
    let note = rows(&state).await[0].1.clone().unwrap();
    assert!(
        note.contains(
            "its words do not name the reasoning control, and the request carried 'text'"
        ),
        "{note}"
    );
    body.as_object_mut().unwrap().remove("text");
    post_with(&gw, "/v1/responses", &OFF, &body).await;
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 3, "{sent:#?}");
    assert_eq!(sent[2]["reasoning"]["effort"], "none", "nothing learned");
}

/// A chat request with tools: a refusal naming them is retried, not kept
/// (the same rule on chat's own sends, `proxy/reasoning_fit/send.rs`).
#[tokio::test]
async fn a_chat_refusal_that_names_its_tools_is_retried_but_teaches_nothing() {
    let refused = "reasoning_effort 'none' cannot be used together with tools.";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"reasoning_effort": "none"})))
        .and(body_string_contains("\"tools\""))
        .respond_with(refusal(refused, None, None))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c1", "object": "chat.completion", "created": 0, "model": "tgt-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "OK"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}})))
        .mount(&mock)
        .await;
    let (state, gw) = gateway(&mock, Protocol::OpenaiCompatible).await;
    let mut chat = json!({"model": "my-model",
        "messages": [{"role": "user", "content": "Reply with OK."}],
        "tools": [{"type": "function", "function": {"name": "f",
            "parameters": {"type": "object", "properties": {}}}}]});
    let (status, _, text) = post_with(&gw, "/v1/chat/completions", &OFF, &chat).await;
    assert_eq!(status, 200, "{text}");
    let note = rows(&state).await[0].1.clone().unwrap();
    assert!(
        note.contains("it also names 'tools', which this request carried"),
        "{note}"
    );
    chat.as_object_mut().unwrap().remove("tools");
    let (status, _, _) = post_with(&gw, "/v1/chat/completions", &OFF, &chat).await;
    assert_eq!(status, 200);
    let sent: Vec<Value> = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/chat/completions")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(sent.len(), 3, "{sent:#?}");
    assert!(sent[1].get("reasoning_effort").is_none(), "{}", sent[1]);
    assert_eq!(sent[2]["reasoning_effort"], "none", "nothing learned");
}

/// A bare refusal on a native body whose only extra is a hosted tool may be
/// about that tool: retried, answered, not kept. (Function tools are another
/// matter: `a_bare_refusal_on_a_chat_request_with_function_tools_is_learned`.)
#[tokio::test]
async fn a_bare_refusal_of_a_body_with_a_hosted_tool_is_retried_but_not_kept() {
    let bare = "Request contains an invalid argument.";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_partial_json(json!({"reasoning": {"effort": "none"},
            "tools": [{"type": "web_search"}]})))
        .respond_with(refusal(bare, None, None))
        .with_priority(1)
        .mount(&mock)
        .await;
    answering(&mock).await;
    let (state, gw) = gateway(&mock, Protocol::Openai).await;
    let mut body = json!({"model": "my-model", "input": "Look it up.",
        "tools": [{"type": "web_search"}]});
    let (status, _, text) = post_with(&gw, "/v1/responses", &OFF, &body).await;
    assert_eq!(status, 200, "{text}");
    let note = rows(&state).await[0].1.clone().unwrap();
    assert!(
        note.contains(
            "its words do not name the reasoning control, and the request carried 'web_search'"
        ),
        "{note}"
    );
    body.as_object_mut().unwrap().remove("tools");
    post_with(&gw, "/v1/responses", &OFF, &body).await;
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 3, "{sent:#?}");
    assert_eq!(sent[2]["reasoning"]["effort"], "none", "nothing learned");
}

/// Function tools, a tool choice and a response format are on nearly every
/// chat request: a bare refusal of the off on one is still the control's, so
/// the route learns the form that answered and the next request goes out
/// fitted, with no refused attempt.
#[tokio::test]
async fn a_bare_refusal_on_a_chat_request_with_function_tools_is_learned() {
    let bare = "Request contains an invalid argument.";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"reasoning_effort": "none"})))
        .respond_with(refusal(bare, None, None))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c1", "object": "chat.completion", "created": 0, "model": "tgt-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "OK"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}})))
        .mount(&mock)
        .await;
    let (state, gw) = gateway(&mock, Protocol::OpenaiCompatible).await;
    let chat = json!({"model": "my-model",
        "messages": [{"role": "user", "content": "Reply with OK."}],
        "tool_choice": "auto",
        "tools": [{"type": "function", "function": {"name": "f",
            "parameters": {"type": "object", "properties": {}}}}]});
    for _ in 0..2 {
        let (status, _, text) = post_with(&gw, "/v1/chat/completions", &OFF, &chat).await;
        assert_eq!(status, 200, "{text}");
    }
    let note = rows(&state).await[0].1.clone().unwrap();
    assert!(note.contains("keeps that for this model"), "{note}");
    let sent: Vec<Value> = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/chat/completions")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(sent.len(), 3, "{sent:#?}");
    assert_eq!(sent[0]["reasoning_effort"], "none");
    assert!(sent[1].get("reasoning_effort").is_none(), "{}", sent[1]);
    assert!(sent[2].get("reasoning_effort").is_none(), "{}", sent[2]);
}

/// A `5xx` whose client left while it was read may have been worked on: the
/// plain hang-up, not the free refusal's row.
#[tokio::test]
async fn a_5xx_whose_stream_client_left_while_it_was_read_is_the_plain_hang_up() {
    let release = Arc::new(tokio::sync::Notify::new());
    let sends = Arc::new(AtomicUsize::new(0));
    let base = refusing_slowly(release.clone(), sends.clone(), 500).await;
    let (state, gw) =
        crate::e2e_proxy::setup_kind(&base, Protocol::Openai, UpstreamKind::Generic).await;
    let sent = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap()
        .post(format!("{gw}/v1/responses"))
        .header("x-lmgw-reasoning", "off")
        .json(&json!({"model": "my-model", "input": "Reply with OK.", "stream": true}))
        .send()
        .await;
    assert!(sent.is_err(), "the client gave up first: {sent:?}");
    let mut r = Vec::new();
    for _ in 0..200 {
        r = rows(&state).await;
        if !r.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    release.notify_waiters();
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].0, 499);
    let msg = r[0].1.as_deref().unwrap();
    assert!(msg.contains("before the upstream answered"), "{msg}");
    assert_eq!(sends.load(Ordering::SeqCst), 1);
}

/// A refusal whose body is empty gives nothing to retry by: it stands after
/// one send, whole and streamed.
#[tokio::test]
async fn a_refusal_with_no_body_earns_no_retry() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&mock)
        .await;
    let (state, gw) = gateway(&mock, Protocol::Openai).await;
    for stream in [false, true] {
        let (status, _, text) = post_with(&gw, "/v1/responses", &OFF, &native_body(stream)).await;
        assert_eq!(status, 400, "stream {stream}: {text}");
    }
    assert_eq!(posted(&mock).await.len(), 2, "one send each");
    assert_eq!(rows(&state).await.len(), 2);
}
