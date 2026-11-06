//! An Anthropic text block's `citations` (gateway design §6.1, "Citations",
//! 2026-11-06): a Messages client on an Anthropic route gets them where they
//! stood, whole and streamed (`citations_delta` at the text block they
//! cite), and an Anthropic upstream gets them back with the block. A Chat
//! Completions or Responses client gets the text, and a whole answer's
//! headers name `answer.citations`; a route of another protocol is sent the
//! text and the request's headers name `content.citations`.

use lmgw_core::agent::TurnAccumulator;
use lmgw_core::config::{Protocol, Upstream, UpstreamKind};
use lmgw_core::egress::for_protocol;
use lmgw_core::ir::{ChatRequest, ContentPart, Message, Params, Role, Signer};
use lmgw_core::sse::SseEvent;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::messages_context_management::{bodies_at, world};

fn cite() -> Value {
    json!({"type": "web_search_result_location", "url": "https://example.com/kiwi",
           "title": "Kiwi", "encrypted_index": "EncIdx==", "cited_text": "The word is KIWI."})
}

fn content() -> Value {
    json!([
        {"type": "text", "text": "According to the page, "},
        {"type": "text", "text": "the word is KIWI.", "citations": [cite()]},
        {"type": "text", "text": " Anything else?"},
    ])
}

fn answer() -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
        "content": content(),
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 10, "output_tokens": 20},
    })
}

/// The same answer streamed: the citation after its block's text, before
/// the block's stop, as Anthropic streams one.
fn stream_events() -> Vec<Value> {
    let start =
        |i: u64, b: Value| json!({"type": "content_block_start", "index": i, "content_block": b});
    let delta = |i: u64, d: Value| json!({"type": "content_block_delta", "index": i, "delta": d});
    let stop = |i: u64| json!({"type": "content_block_stop", "index": i});
    let text = |t: &str| json!({"type": "text_delta", "text": t});
    vec![
        json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt", "content": [],
            "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        start(0, json!({"type": "text", "text": ""})),
        delta(0, text("According to the page, ")),
        stop(0),
        start(1, json!({"type": "text", "text": ""})),
        delta(1, text("the word is KIWI.")),
        delta(1, json!({"type": "citations_delta", "citation": cite()})),
        stop(1),
        start(2, json!({"type": "text", "text": ""})),
        delta(2, text(" Anything else?")),
        stop(2),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null},
               "usage": {"output_tokens": 20}}),
        json!({"type": "message_stop"}),
    ]
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

async fn anthropic_answering(answer: ResponseTemplate) -> (MockServer, crate::common::Gw) {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(answer)
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;
    (mock, gw)
}

async fn post(gw: &crate::common::Gw, route: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gw}{route}"))
        .header("anthropic-version", "2023-06-01")
        .json(body)
        .send()
        .await
        .unwrap()
}

fn ignored(resp: &reqwest::Response) -> String {
    resp.headers()
        .get("x-lmgw-params-ignored")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

#[tokio::test]
async fn a_whole_answer_s_citations_reach_the_messages_client_and_are_named_elsewhere() {
    let (_mock, gw) = anthropic_answering(ResponseTemplate::new(200).set_body_json(answer())).await;
    let ask = json!({"model": "m", "max_tokens": 64,
                     "messages": [{"role": "user", "content": "Code word?"}]});
    let resp = post(&gw, "/v1/messages", &ask).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp), "");
    let got: Value = resp.json().await.unwrap();
    assert_eq!(got["content"], content(), "{got}");

    let resp = post(
        &gw,
        "/v1/chat/completions",
        &json!({"model": "m", "messages": [{"role": "user", "content": "Code word?"}]}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp), "answer.citations");
    let got: Value = resp.json().await.unwrap();
    let text = got["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(text.contains("the word is KIWI."), "{got}");
}

#[tokio::test]
async fn a_streamed_answer_s_citations_reach_the_messages_client_at_their_block() {
    let (_mock, gw) = anthropic_answering(
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse(&stream_events())),
    )
    .await;
    let ask = json!({"model": "m", "max_tokens": 64, "stream": true,
                     "messages": [{"role": "user", "content": "Code word?"}]});
    let text = post(&gw, "/v1/messages", &ask).await.text().await.unwrap();
    let frames: Vec<Value> = text
        .split("\n\n")
        .filter_map(|f| {
            let data = f.lines().find_map(|l| l.strip_prefix("data: "))?;
            serde_json::from_str::<Value>(data).ok()
        })
        .filter(|e| {
            e["type"]
                .as_str()
                .unwrap_or("")
                .starts_with("content_block")
        })
        .collect();
    let upstream: Vec<Value> = stream_events()
        .into_iter()
        .filter(|e| e["type"].as_str().unwrap().starts_with("content_block"))
        .collect();
    assert_eq!(frames, upstream, "{text}");

    // The documented order, whatever the fixture's: the citations docs'
    // "Streaming support" example
    // (https://platform.claude.com/docs/en/build-with-claude/citations)
    // streams a text block's `text_delta`, then its `citations_delta`, then
    // its `content_block_stop`; the streaming docs' text blocks open as
    // `{"type": "text", "text": ""}`, without a `citations` key
    // (https://platform.claude.com/docs/en/build-with-claude/streaming).
    let cited: Vec<&str> = frames
        .iter()
        .filter(|f| f["index"] == 1)
        .map(|f| {
            f["delta"]["type"]
                .as_str()
                .unwrap_or_else(|| f["type"].as_str().unwrap())
        })
        .collect();
    assert_eq!(
        cited,
        [
            "content_block_start",
            "text_delta",
            "citations_delta",
            "content_block_stop"
        ],
        "{text}"
    );
    for start in frames.iter().filter(|f| f["type"] == "content_block_start") {
        assert_eq!(
            start["content_block"],
            json!({"type": "text", "text": ""}),
            "{text}"
        );
    }
}

fn row(protocol: Protocol) -> Upstream {
    Upstream {
        id: 1,
        name: "up".into(),
        protocol,
        kind: UpstreamKind::Generic,
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

fn sent(up: &Upstream, ir: &ChatRequest) -> Value {
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

/// The in-process assembler (the agent loop's, the Chat's tool turns')
/// records each cited block as its own text part with its citations, and
/// the replay sends them back to Anthropic as the answer had them; another
/// protocol's row gets the text, its citations named.
/// The recorded turn of [`stream_events`], answered by `up`, as the
/// in-process assembler records it.
fn recorded(up: &Upstream) -> Vec<ContentPart> {
    let mut dec = for_protocol(Protocol::Anthropic).decoder_for(up);
    let mut acc = TurnAccumulator::default();
    for e in stream_events() {
        for d in dec.on_event(&SseEvent {
            event: Some(e["type"].as_str().unwrap().into()),
            data: e.to_string(),
        }) {
            acc.on_delta(&d);
        }
    }
    acc.finish("tgt".into()).content
}

fn asked_again(parts: Vec<ContentPart>) -> ChatRequest {
    ChatRequest {
        model_alias: "m".into(),
        messages: vec![
            Message::text(Role::User, "Code word?"),
            Message {
                role: Role::Assistant,
                content: parts,
            },
            Message::text(Role::User, "Thanks."),
        ],
        params: Params::default(),
        tools: vec![],
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        response_format: None,
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
        context_management: None,
        compaction: None,
        thinking_display: None,
    }
}

#[test]
fn a_recorded_answer_keeps_its_citations_for_anthropic() {
    let anthropic = row(Protocol::Anthropic);
    let cited = ContentPart::cited(
        "the word is KIWI.",
        vec![cite()],
        Some(Signer::of(&anthropic)),
    );
    let parts = vec![
        ContentPart::text("According to the page, "),
        cited.clone(),
        ContentPart::text(" Anything else?"),
    ];
    assert_eq!(recorded(&anthropic), parts, "as the whole answer's blocks");
    let whole = for_protocol(Protocol::Anthropic)
        .parse_completion_from(&anthropic, answer().to_string().as_bytes())
        .unwrap();
    assert_eq!(whole.content, parts);

    let ir = asked_again(parts);
    assert_eq!(sent(&anthropic, &ir)["messages"][1]["content"], content());
    assert!(ir.blocks_ignored(&anthropic).is_empty());
    let other = row(Protocol::OpenaiCompatible);
    let body = sent(&other, &ir);
    assert!(
        body["messages"][1]["content"]
            .to_string()
            .contains("the word is KIWI."),
        "{body}"
    );
    assert!(!body.to_string().contains("EncIdx=="), "{body}");
    assert_eq!(ir.blocks_ignored(&other), ["content.citations"]);
}

/// Citations lmgw recorded are the scope's that answered, as a signature
/// is: an Anthropic-protocol row at another scope (another path behind one
/// gateway's origin) gets the text without them, and the request names
/// `content.citations`; a row at the same scope with another key and model
/// gets them.
#[test]
fn recorded_citations_stay_with_their_scope() {
    let at = |base: &str, key: &str| Upstream {
        base_url: base.into(),
        api_key: Some(key.into()),
        ..row(Protocol::Anthropic)
    };
    let answered = at(
        "https://gateway.ai.cloudflare.com/v1/acct/gw/anthropic",
        "k1",
    );
    let ir = asked_again(recorded(&answered));

    let same = at(
        "https://gateway.ai.cloudflare.com/v1/acct/gw/anthropic/",
        "k2",
    );
    assert_eq!(sent(&same, &ir)["messages"][1]["content"], content());
    assert!(ir.blocks_ignored(&same).is_empty());

    let other = at(
        "https://gateway.ai.cloudflare.com/v1/acct/gw2/anthropic",
        "k1",
    );
    let body = sent(&other, &ir);
    assert_eq!(
        body["messages"][1]["content"],
        json!([
            {"type": "text", "text": "According to the page, "},
            {"type": "text", "text": "the word is KIWI."},
            {"type": "text", "text": " Anything else?"},
        ]),
        "{body}"
    );
    assert!(!body.to_string().contains("EncIdx=="), "{body}");
    assert_eq!(ir.blocks_ignored(&other), ["content.citations"]);

    // A client's citations have no scope: they go as written.
    let client = asked_again(vec![ContentPart::cited(
        "the word is KIWI.",
        vec![cite()],
        None,
    )]);
    assert_eq!(
        sent(&other, &client)["messages"][1]["content"][0]["citations"],
        json!([cite()])
    );
    assert!(client.blocks_ignored(&other).is_empty());
}

/// A citation that came with no text (a text block that streamed none) is
/// not sent to Anthropic, which refuses an empty text block
/// (`TextBlockParam.text` has `minLength: 1`, Messages API reference), and
/// the request names it, and the send's INFO line counts it (changed
/// 2026-11-06).
#[tokio::test]
async fn a_citation_without_text_is_left_out_and_named() {
    let (log, _guard) = crate::common::captured_log::capture_log();
    let anthropic = row(Protocol::Anthropic);
    let ir = asked_again(vec![
        ContentPart::cited("", vec![cite()], Some(Signer::of(&anthropic))),
        ContentPart::text("Done."),
    ]);
    assert_eq!(
        sent(&anthropic, &ir)["messages"][1]["content"],
        json!([{"type": "text", "text": "Done."}])
    );
    assert_eq!(ir.blocks_ignored(&anthropic), ["content.citations"]);
    let text = log.text();
    assert!(
        text.contains("INFO") && text.contains("textless_citations=1"),
        "{text}"
    );
}

/// A Messages client's cited block sent back reaches Anthropic with its
/// citations.
#[tokio::test]
async fn a_client_s_cited_block_goes_back_to_anthropic() {
    let (mock, gw) = anthropic_answering(ResponseTemplate::new(200).set_body_json(answer())).await;
    let ask = json!({"model": "m", "max_tokens": 64, "messages": [
        {"role": "user", "content": "Code word?"},
        {"role": "assistant", "content": content()},
        {"role": "user", "content": "Thanks."},
    ]});
    assert_eq!(post(&gw, "/v1/messages", &ask).await.status(), 200);
    let bodies = bodies_at(&mock, "/v1/messages").await;
    assert_eq!(bodies[0]["messages"][1]["content"], content());
}
