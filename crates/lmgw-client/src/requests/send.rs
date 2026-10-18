//! The send of a chat turn.
//!
//! - [`send`](fn@send): `POST /chat/api/threads/{id}/send`, a user message (its
//!   text, the draft attachments it binds, the knowledge bases picked for
//!   it, a dictation's origin) and the reply it starts. The answer is the
//!   turn's frames (`turn`, `retrieval`, `delta`, `usage`, `tool`, `error`,
//!   `done`; with `speak` the speech frames too) as `text/event-stream`:
//!   read it with [`crate::feed::SseDecoder`]. A refusal comes before the
//!   stream, with a non-2xx status, and reads with
//!   [`read_refusal`](crate::requests::read_refusal): `empty_message`,
//!   `not_found`, `attachment_not_draft`, `model_no_vision`,
//!   `attachment_blocked` (422), `bad_request`.
//! - Each frame types with [`TurnEvent::parse`]. An MCP label the thread or
//!   its folder attached that offers the turn no tools gets an `error` frame
//!   with its `server_label` and a code [`ErrorFrame::label_code`] reads
//!   ([`LabelCode`]: `tool_unreachable` — a desktop's tools this device is
//!   not granted —, `tool_offline`, `tool_disabled`, `tool_unknown`,
//!   `tool_failed`); the turn goes on without it.

pub use lmgw_api_types::chat_frames::{ErrorFrame, LabelCode, TurnEvent};
pub use lmgw_api_types::chat_turn::SendRequest;

use super::{Method, Request};

/// `POST /chat/api/threads/{id}/send` with `body`; the answer streams (see
/// the module).
pub fn send(thread_id: i64, body: &SendRequest) -> Request {
    Request::new(Method::Post, format!("/chat/api/threads/{thread_id}/send"))
        .header("Accept", "text/event-stream")
        .json(serde_json::to_string(body).expect("a send request always serializes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(r: &Request, name: &str, value: &str) -> bool {
        r.headers.iter().any(|h| h.name == name && h.value == value)
    }

    #[test]
    fn a_send_carries_the_message_and_asks_for_a_stream() {
        let r = send(
            9,
            &SendRequest {
                content: "hi".into(),
                attachments: vec![4, 5],
                kb_refs: vec![2],
                speak: true,
                ..Default::default()
            },
        );
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.path, "/chat/api/threads/9/send");
        let body: serde_json::Value = serde_json::from_str(r.body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "content": "hi", "attachments": [4, 5], "kb_refs": [2], "speak": true
            })
        );
        assert!(has(&r, "Accept", "text/event-stream"));
        assert!(has(&r, "Content-Type", "application/json"));
    }

    #[test]
    fn a_plain_send_leaves_the_optional_fields_out() {
        let r = send(
            1,
            &SendRequest {
                content: "x".into(),
                ..Default::default()
            },
        );
        assert_eq!(r.body.as_deref(), Some(r#"{"content":"x"}"#));
    }

    /// A turn's stream, decoded and typed as a client reads it: the frame
    /// about an attached label it may not use carries the label and its
    /// code, and the turn goes on.
    #[test]
    fn a_label_frame_reads_with_its_code_and_the_turn_goes_on() {
        let stream = b"event: turn\ndata: {\"user_message_id\":4}\n\n\
            event: error\ndata: {\"code\":\"tool_unreachable\",\"message\":\"MCP server 'kai': \
            this device may not use the tools of device 'kai desktop' (label kai): grant it under \
            the device's policy (Device tools)\",\"server_label\":\"kai\"}\n\n\
            event: delta\ndata: {\"text\":\"Hi\"}\n\n";
        let mut d = crate::feed::SseDecoder::new();
        let events: Vec<TurnEvent> = d
            .push(stream)
            .into_iter()
            .filter_map(|item| match item {
                crate::feed::SseItem::Frame(f) => {
                    Some(TurnEvent::parse(&f.event, &f.data).unwrap())
                }
                _ => None,
            })
            .collect();
        let TurnEvent::Error(e) = &events[1] else {
            panic!("{events:?}")
        };
        assert_eq!(e.label_code(), Some(LabelCode::Unreachable));
        assert_eq!(e.server_label.as_deref(), Some("kai"));
        assert!(matches!(events[2], TurnEvent::Delta(_)), "{events:?}");
    }

    #[test]
    fn a_refusal_reads_with_its_code() {
        let e = crate::requests::read_refusal(
            400,
            r#"{"code": "empty_message", "message": "the message is empty"}"#,
        );
        assert_eq!(e.code, "empty_message");
    }
}
