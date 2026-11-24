use axum::http::{HeaderMap, HeaderValue};

use super::*;

fn headers(id: Option<&str>) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Some(id) = id {
        h.insert("request-id", HeaderValue::from_str(id).unwrap());
    }
    h
}

const MESSAGES: ClientProto = ClientProto::AnthropicMessages;

fn ids(header: Option<&str>, body: Option<&str>) -> UpstreamIds {
    UpstreamIds {
        header: header.map(String::from),
        body: body.map(String::from),
    }
}

/// The header's id and the error body's are kept apart: the response
/// relays the header's, the error body carries the body's.
#[test]
fn the_header_s_and_the_error_body_s_ids_are_kept_apart() {
    let body =
        br#"{"type":"error","error":{"type":"api_error","message":"x"},"request_id":"req_body"}"#;
    let h = headers(Some("req_header"));
    assert_eq!(
        of_answer(MESSAGES, Protocol::Anthropic, &h, Some(body)),
        ids(Some("req_header"), Some("req_body"))
    );
    let no_id = br#"{"type":"error","error":{"type":"api_error","message":"x"}}"#;
    assert_eq!(
        of_answer(MESSAGES, Protocol::Anthropic, &h, Some(no_id)),
        ids(Some("req_header"), None)
    );
    assert_eq!(
        of_answer(MESSAGES, Protocol::Anthropic, &h, Some(b"not json")),
        ids(Some("req_header"), None)
    );
    assert_eq!(
        of_answer(MESSAGES, Protocol::Anthropic, &h, None),
        ids(Some("req_header"), None)
    );
    assert_eq!(
        of_answer(MESSAGES, Protocol::Anthropic, &headers(None), Some(body)),
        ids(None, Some("req_body"))
    );
    assert_eq!(
        of_answer(MESSAGES, Protocol::Anthropic, &headers(None), None),
        UpstreamIds::default()
    );
    assert_eq!(
        of_answer(MESSAGES, Protocol::Anthropic, &headers(Some("")), None),
        UpstreamIds::default()
    );
}

/// Only a Messages client of an Anthropic-protocol upstream gets one.
#[test]
fn only_an_anthropic_upstream_s_id_on_the_messages_route() {
    let h = headers(Some("req_1"));
    assert_eq!(
        of_answer(MESSAGES, Protocol::OpenaiCompatible, &h, None),
        UpstreamIds::default()
    );
    assert_eq!(
        of_answer(ClientProto::OpenaiChat, Protocol::Anthropic, &h, None),
        UpstreamIds::default()
    );
}
