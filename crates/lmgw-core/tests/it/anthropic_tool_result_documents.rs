//! A `tool_result`'s `document` and `search_result` blocks (gateway design
//! §6.1, changed 2026-11-06) go to an Anthropic-protocol row as the client
//! wrote them, blocks and not JSON text. Every other row is sent what of
//! them reads as text (a plain-text document's text, a search result's
//! content, each after its title), never a URL or Files API id and never
//! the block's JSON; a PDF, URL or Files API document is left out and named
//! `content.document`. `/v1/messages/count_tokens` counts what the row is
//! sent.

use lmgw_core::config::{Protocol, Upstream, UpstreamKind};
use lmgw_core::egress::for_protocol;
use lmgw_core::ir::{ChatRequest, Params};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::messages_context_management::{bodies_at, world};

fn text_doc() -> Value {
    json!({"type": "document", "title": "Forecast",
           "source": {"type": "text", "media_type": "text/plain", "data": "15 degrees, dry."},
           "citations": {"enabled": true}})
}

fn search() -> Value {
    json!({"type": "search_result", "source": "kb://article-1234", "title": "Timeouts",
           "content": [{"type": "text", "text": "Set timeout_ms on the row."}],
           "citations": {"enabled": true}})
}

fn file_doc() -> Value {
    json!({"type": "document", "source": {"type": "file", "file_id": "file_doc9"}})
}

fn pdf_doc() -> Value {
    json!({"type": "document",
           "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0xLjQK"}})
}

/// A tool loop whose result is `content`.
fn tool_loop(content: Value) -> Value {
    json!({"model": "m", "max_tokens": 64, "messages": [
        {"role": "user", "content": "Look it up."},
        {"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {}},
        ]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": content},
        ]},
    ]})
}

fn every_block() -> Value {
    json!([{"type": "text", "text": "found:"}, text_doc(), search(), file_doc(), pdf_doc()])
}

/// The tool text every row off an Anthropic route is sent for
/// [`every_block`].
const FLAT: &str = "found:\nForecast\n\n15 degrees, dry.\n\nTimeouts\nkb://article-1234\n\nSet \
                    timeout_ms on the row.";

async fn post(gw: &crate::common::Gw, route: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gw}{route}"))
        .header("anthropic-version", "2023-06-01")
        .json(body)
        .send()
        .await
        .unwrap()
}

fn header(resp: &reqwest::Response, name: &str) -> String {
    resp.headers()
        .get(name)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

fn ir(body: &Value) -> ChatRequest {
    lmgw_core::ingress::anthropic::parse_messages_request(body).unwrap()
}

fn row(protocol: Protocol) -> Upstream {
    Upstream {
        id: 1,
        name: "row".into(),
        protocol,
        kind: match protocol {
            Protocol::LlamaCpp => UpstreamKind::LlamaServer,
            _ => UpstreamKind::Generic,
        },
        base_url: "http://h:1/v1".into(),
        api_key: Some("sk-test".into()),
        extra_headers: vec![],
        timeout_ms: 1000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

fn body(up: &Upstream, ir: &ChatRequest) -> Value {
    let req = for_protocol(up.protocol)
        .build_chat(
            &reqwest::Client::new(),
            up,
            "tgt",
            ir,
            &Params::default(),
            false,
        )
        .unwrap()
        .build()
        .unwrap();
    serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap()
}

#[tokio::test]
async fn an_anthropic_row_gets_the_blocks_as_written() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn", "usage": {"input_tokens": 3, "output_tokens": 1},
        })))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;
    let resp = post(&gw, "/v1/messages", &tool_loop(every_block())).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lmgw-params-ignored"), "");
    let sent = bodies_at(&mock, "/v1/messages").await;
    assert_eq!(
        sent[0]["messages"][2]["content"][0]["content"],
        every_block()
    );
}

/// Chat Completions, Responses, Gemini and llama.cpp bodies carry the
/// documents' and the search result's text and none of their JSON, file
/// id or PDF.
#[test]
fn every_other_egress_carries_their_text_and_no_block_json() {
    let ir = ir(&tool_loop(every_block()));
    for protocol in [
        Protocol::Openai,
        Protocol::OpenaiCompatible,
        Protocol::Gemini,
        Protocol::LlamaCpp,
    ] {
        let sent = body(&row(protocol), &ir);
        let text = sent.to_string();
        for absent in [
            "file_doc9",
            "JVBERi0x",
            r#"\"type\":\"document\""#,
            r#""type":"document""#,
            "search_result",
        ] {
            assert!(!text.contains(absent), "{protocol:?} has {absent}: {text}");
        }
        let tool_text = match protocol {
            Protocol::Openai => sent["input"][2]["output"].clone(),
            Protocol::Gemini => {
                sent["contents"][2]["parts"][0]["functionResponse"]["response"]["result"].clone()
            }
            _ => sent["messages"][2]["content"].clone(),
        };
        assert_eq!(tool_text, FLAT, "{protocol:?}: {text}");
    }
}

#[tokio::test]
async fn another_row_names_the_document_it_cannot_send() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "model": "tgt",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::OpenaiCompatible).await;

    // Text alone loses no content; the citations they asked for are named.
    let resp = post(
        &gw,
        "/v1/messages",
        &tool_loop(json!([text_doc(), search()])),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lmgw-params-ignored"), "content.citations");

    let resp = post(&gw, "/v1/messages", &tool_loop(every_block())).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-lmgw-params-ignored"),
        "content.citations,content.document"
    );
    let sent = bodies_at(&mock, "/chat/completions").await;
    assert_eq!(sent[1]["messages"][2]["role"], "tool");
    assert_eq!(sent[1]["messages"][2]["content"], FLAT);
}

/// The count off an Anthropic route is the count of what the row is sent:
/// the result counts as the same text in a text block would, and a Files
/// API or PDF document adds nothing.
#[tokio::test]
async fn the_count_counts_what_the_row_is_sent() {
    let mock = MockServer::start().await;
    let gw = world(&mock.uri(), Protocol::OpenaiCompatible).await;
    let count = |body: Value| {
        let gw = &gw;
        async move {
            let resp = post(gw, "/v1/messages/count_tokens", &body).await;
            assert_eq!(resp.status(), 200);
            let v: Value = resp.json().await.unwrap();
            v["input_tokens"].as_u64().unwrap()
        }
    };
    let blocks = count(tool_loop(every_block())).await;
    let as_text = count(tool_loop(json!([{"type": "text", "text": FLAT}]))).await;
    assert_eq!(blocks, as_text);
    let docs = count(tool_loop(json!([text_doc()]))).await;
    let with_files = count(tool_loop(json!([text_doc(), file_doc(), pdf_doc()]))).await;
    assert_eq!(docs, with_files);
}

/// The tool text every egress carries for a tool loop whose result is
/// `content`.
fn tool_text_of(protocol: Protocol, content: Value) -> (String, String) {
    let sent = body(&row(protocol), &ir(&tool_loop(content)));
    let text = match protocol {
        Protocol::Openai => sent["input"][2]["output"].clone(),
        Protocol::Gemini => {
            sent["contents"][2]["parts"][0]["functionResponse"]["response"]["result"].clone()
        }
        _ => sent["messages"][2]["content"].clone(),
    };
    (
        text.as_str().unwrap_or_default().to_string(),
        sent.to_string(),
    )
}

#[test]
fn a_url_document_is_dropped_and_its_url_never_sent() {
    let ir = ir(&tool_loop(json!([
        {"type": "text", "text": "found:"},
        {"type": "document", "title": "Paper",
         "source": {"type": "url", "url": "https://e.example/SECRET.pdf"}},
    ])));
    for protocol in [
        Protocol::Openai,
        Protocol::OpenaiCompatible,
        Protocol::Gemini,
    ] {
        let sent = body(&row(protocol), &ir).to_string();
        assert!(!sent.contains("e.example"), "{protocol:?}: {sent}");
        assert!(!sent.contains("Paper"), "{protocol:?}: {sent}");
    }
    let names = lmgw_core::ir::ignored_in_messages("content", &ir.messages);
    assert_eq!(names, ["content.document"]);
}

#[test]
fn a_content_document_sends_its_text_and_names_a_nested_image() {
    let doc = json!({"type": "document", "title": "Report",
    "source": {"type": "content", "content": [
        {"type": "text", "text": "Chart:"},
        {"type": "image", "source": {"type": "url", "url": "https://e.example/c.png"}},
    ]}});
    let ir = ir(&tool_loop(json!([doc])));
    for protocol in [
        Protocol::Openai,
        Protocol::OpenaiCompatible,
        Protocol::Gemini,
        Protocol::LlamaCpp,
    ] {
        let (text, all) = tool_text_of(protocol, json!([doc]));
        assert_eq!(text, "Report\n\nChart:", "{protocol:?}: {all}");
        assert!(!all.contains("e.example"), "{protocol:?}: {all}");
    }
    assert_eq!(
        lmgw_core::ir::ignored_in_messages("content", &ir.messages),
        ["content.document"]
    );
}

#[test]
fn gemini_with_a_single_unknown_block_gets_its_json_or_nothing() {
    let response = |content: Value| {
        body(&row(Protocol::Gemini), &ir(&tool_loop(content)))["contents"][2]["parts"][0]
            ["functionResponse"]["response"]
            .clone()
    };
    assert_eq!(
        response(
            json!([{"type": "browser_state", "tabs": [], "cache_control": {"type": "ephemeral"}}])
        ),
        json!({"type": "browser_state", "tabs": []})
    );
    let sent = response(json!([{"type": "future", "encrypted_content": "ENC"}])).to_string();
    assert!(!sent.contains("ENC") && !sent.contains("future"), "{sent}");
}

#[test]
fn blocks_that_cannot_be_read_are_named_and_citations_follow() {
    let ir = ir(&tool_loop(json!([
        text_doc(),
        {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": ""}},
        {"type": "future", "data": "AAAA"},
    ])));
    assert_eq!(
        lmgw_core::ir::ignored_in_messages("content", &ir.messages),
        ["content.citations", "content.document", "content.future"]
    );
}

#[test]
fn an_empty_document_sends_its_title_or_is_named() {
    let empty = |title: Option<&str>| {
        let mut d = json!({"type": "document",
            "source": {"type": "text", "media_type": "text/plain", "data": ""}});
        if let Some(t) = title {
            d["title"] = json!(t);
        }
        d
    };
    let (text, _) = tool_text_of(
        Protocol::OpenaiCompatible,
        json!([empty(Some("Only title"))]),
    );
    assert_eq!(text, "Only title");
    let ir_empty = ir(&tool_loop(json!([empty(Some("Only title"))])));
    assert!(lmgw_core::ir::ignored_in_messages("content", &ir_empty.messages).is_empty());
    let ir_none = ir(&tool_loop(json!([empty(None)])));
    assert_eq!(
        lmgw_core::ir::ignored_in_messages("content", &ir_none.messages),
        ["content.document"]
    );
}
