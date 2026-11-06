//! llama.cpp's servers end to end, on an external `llama_cpp` row: a type
//! lmgw does not model read as `response_format_unsupported` (§11.8), the
//! PEG path's `500 JSON schema error at …` as `invalid_json_schema`
//! (§11.11), and the legacy `/v1/completions`, whose format goes as the
//! top-level `json_schema` the route reads (§11.22). The texts are llama.cpp's
//! own, from its source (§9.1).

use lmgw_core::config::{Protocol, UpstreamKind};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::routes::{
    assert_format_error, captured, chat_body, json_schema, openai_catalog, post, posted, schema,
    world, world_on,
};

async fn llama_world(mock: &MockServer) -> crate::common::Gw {
    world_on(
        &mock.uri(),
        Protocol::LlamaCpp,
        UpstreamKind::LlamaServer,
        false,
        None,
    )
    .await
    .1
}

/// `{"type": "regex"}` goes verbatim to an OpenAI-shaped upstream (§11.3);
/// its refusal of the type is `response_format_unsupported` there as on
/// Anthropic and Gemini — llama-server's text and OpenAI's alike. A schema
/// text is not read for it: the converter's words also cover tool schemas.
#[tokio::test]
async fn an_unmodelled_type_is_classified_on_every_openai_shaped_upstream() {
    let regex = json!({"type": "regex", "pattern": "ok"});
    for (protocol, kind, fixture) in [
        (
            Protocol::LlamaCpp,
            UpstreamKind::LlamaServer,
            "llama_unknown_type",
        ),
        (
            Protocol::OpenaiCompatible,
            UpstreamKind::Generic,
            "openai_unknown_type",
        ),
    ] {
        let mock = MockServer::start().await;
        openai_catalog(&mock).await;
        let (refusal, message) = captured(fixture);
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(refusal)
            .mount(&mock)
            .await;
        let (_state, gw) = world_on(&mock.uri(), protocol, kind, false, None).await;
        for stream in [false, true] {
            let (status, v) = post(
                &gw,
                "/v1/chat/completions",
                chat_body(Some(regex.clone()), stream),
            )
            .await;
            assert_eq!(status, 400, "{fixture}, stream {stream}: {v}");
            assert_format_error(&v, "response_format_unsupported", "response_format");
            assert_eq!(v["error"]["message"], message.as_str());
        }
        assert_eq!(posted(&mock).await[0]["response_format"], regex);
    }

    let mock = MockServer::start().await;
    openai_catalog(&mock).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(captured("llama_conversion_failed").0)
        .mount(&mock)
        .await;
    let (_state, gw) = world(&mock.uri(), Protocol::OpenaiCompatible, false, None).await;
    let (status, v) = post(&gw, "/v1/chat/completions", chat_body(Some(regex), false)).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "upstream", "{v}");
}

/// llama-server's PEG chat path answers a schema it cannot read with `500
/// JSON schema error at …`: the schema failed, not the server — `400
/// invalid_json_schema`, unary and streamed.
#[tokio::test]
async fn llamas_500_schema_error_is_invalid_json_schema_on_the_chat_route() {
    let mock = MockServer::start().await;
    let (refusal, message) = captured("llama_schema_error");
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(refusal)
        .mount(&mock)
        .await;
    let gw = llama_world(&mock).await;
    for stream in [false, true] {
        let (status, v) = post(
            &gw,
            "/v1/chat/completions",
            chat_body(Some(json_schema()), stream),
        )
        .await;
        assert_eq!(status, 400, "stream {stream}: {v}");
        assert_format_error(&v, "invalid_json_schema", "response_format");
        assert_eq!(v["error"]["message"], message.as_str());
    }
}

/// `/v1/completions` on llama.cpp reads no `response_format`: the format goes
/// as the top-level `json_schema`, and what the route would drop is refused
/// with nothing sent. An OpenAI row gets the body as it came.
#[tokio::test]
async fn legacy_completions_on_llama_cpp_send_the_schema_the_route_reads() {
    let completion = || {
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "cmpl-1", "object": "text_completion", "created": 1, "model": "tgt",
            "choices": [{"index": 0, "text": "{\"ok\":true}", "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 3, "total_tokens": 6},
        }))
    };
    let legacy = |rf: Value| json!({"model": "m", "prompt": "Reply with ok.", "max_tokens": 16, "response_format": rf});

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completions"))
        .respond_with(completion())
        .mount(&mock)
        .await;
    let gw = llama_world(&mock).await;
    let (status, v) = post(&gw, "/v1/completions", legacy(json_schema())).await;
    assert_eq!(status, 200, "{v}");
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["json_schema"], schema());
    assert!(sent[0].get("response_format").is_none(), "{}", sent[0]);

    let (status, v) = post(&gw, "/v1/completions", legacy(json!({"type": "regex"}))).await;
    assert_eq!(status, 400, "{v}");
    assert_format_error(&v, "response_format_unsupported", "response_format");
    let mut both = legacy(json!({"type": "json_object"}));
    both["grammar"] = json!("root ::= \"ok\"");
    let (status, v) = post(&gw, "/v1/completions", both).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "bad_request", "{v}");
    assert_eq!(posted(&mock).await.len(), 1, "neither refusal was sent");

    let mock = MockServer::start().await;
    openai_catalog(&mock).await;
    Mock::given(method("POST"))
        .and(path("/completions"))
        .respond_with(completion())
        .mount(&mock)
        .await;
    let (_state, gw) = world(&mock.uri(), Protocol::OpenaiCompatible, false, None).await;
    let (status, _) = post(&gw, "/v1/completions", legacy(json_schema())).await;
    assert_eq!(status, 200);
    assert_eq!(posted(&mock).await[0]["response_format"], json_schema());
}

/// The streamed legacy route takes the same rewrite (§11.23): the schema goes
/// top-level, an empty `grammar` is dropped so llama applies it, and a set one
/// is refused with nothing sent.
#[tokio::test]
async fn streamed_legacy_completions_take_the_same_rewrite() {
    let chunk =
        "data: {\"id\":\"c\",\"object\":\"text_completion\",\"created\":1,\"model\":\"tgt\",\
                 \"choices\":[{\"index\":0,\"text\":\"{}\",\"finish_reason\":\"stop\"}]}\n\n\
                 data: [DONE]\n\n";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(chunk),
        )
        .mount(&mock)
        .await;
    let gw = llama_world(&mock).await;
    let body = json!({"model": "m", "prompt": "Reply with ok.", "max_tokens": 16,
                      "stream": true, "grammar": "", "response_format": json_schema()});
    let (status, v) = post(&gw, "/v1/completions", body.clone()).await;
    assert_eq!(status, 200, "{v}");
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["stream"], true);
    assert_eq!(sent[0]["json_schema"], schema());
    assert!(sent[0].get("response_format").is_none(), "{}", sent[0]);
    assert!(sent[0].get("grammar").is_none(), "{}", sent[0]);

    let mut both = body;
    both["grammar"] = json!("root ::= \"ok\"");
    let (status, v) = post(&gw, "/v1/completions", both).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "bad_request", "{v}");
    assert_eq!(posted(&mock).await.len(), 1, "the refusal was not sent");
}

/// On a local row the legacy format's refusal comes before admission:
/// nothing is started for a body the route cannot take; a format it can
/// take loads the model and goes as the top-level `json_schema`.
#[tokio::test]
async fn a_refused_legacy_format_starts_nothing_on_a_local_row() {
    use crate::support::gpu_world::{Gpu, GIB};
    let g = Gpu::new(24 * GIB, 1, 2).await;
    let gw = crate::common::serve(g.state.clone()).await;
    g.model("p", GIB).await;
    let body = |rf: Value| json!({"model": "p", "prompt": "Reply with ok.", "max_tokens": 16, "response_format": rf});
    let (status, v) = post(&gw, "/v1/completions", body(json!({"type": "regex"}))).await;
    assert_eq!(status, 400, "{v}");
    assert_format_error(&v, "response_format_unsupported", "response_format");
    assert!(g.runs().is_empty(), "refused before admission");

    let (status, v) = post(&gw, "/v1/completions", body(json_schema())).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(g.runs(), ["p"]);
    let sent = g.posted().await;
    let sent: Vec<Value> = sent
        .iter()
        .filter(|r| r.url.path() == "/v1/completions")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["json_schema"], schema());
    assert!(sent[0].get("response_format").is_none(), "{}", sent[0]);
}

/// llama.cpp's legacy route fails a schema it cannot convert, and the error
/// names the field the client sent: `response_format` when that is what it
/// put in the body (lmgw rewrote it to `json_schema` on the way), the
/// top-level `json_schema` when it sent that.
#[tokio::test]
async fn a_legacy_conversion_failure_names_the_field_the_client_sent() {
    let mock = MockServer::start().await;
    let (refusal, message) = captured("llama_conversion_failed");
    Mock::given(method("POST"))
        .and(path("/completions"))
        .respond_with(refusal)
        .mount(&mock)
        .await;
    let gw = llama_world(&mock).await;
    let base = || json!({"model": "m", "prompt": "Reply with ok.", "max_tokens": 16});
    let mut as_response_format = base();
    as_response_format["response_format"] = json_schema();
    let mut as_json_schema = base();
    as_json_schema["json_schema"] = schema();
    for (body, param) in [
        (as_response_format, "response_format"),
        (as_json_schema, "json_schema"),
    ] {
        let (status, v) = post(&gw, "/v1/completions", body).await;
        assert_eq!(status, 400, "{param}: {v}");
        assert_format_error(&v, "invalid_json_schema", param);
        assert_eq!(v["error"]["message"], message.as_str());
    }
}
