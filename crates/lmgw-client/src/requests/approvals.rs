//! The requests of MCP approvals (client-apps design §6) and of a voice
//! answer kept out of the conversation.
//!
//! - [`approvals`]: `POST /chat/api/threads/{id}/approvals` with a verdict
//!   for every call the thread's last reply waits on. The answer is the
//!   resumed turn's frames, as a send streams them (`text/event-stream`;
//!   read it with [`crate::feed::SseDecoder`]); a refusal reads with
//!   [`super::read_refusal`]: `approval_missing`, `approval_not_found`,
//!   `approval_decided` (naming who decided first),
//!   `approval_starter_unavailable`, `approval_moved_on`,
//!   `approval_out_of_scope` (a device approving beyond its own reach).
//! - [`transcribe`]: `POST /chat/api/threads/{id}/transcribe`, a recording
//!   transcribed by the thread's speech-to-text model, written nowhere —
//!   how a client that answers an approval by voice keeps the spoken answer
//!   out of the conversation (a user message would decline the call).
//!   Read with [`read_transcription`].
//!
//! Where the calls come from: the `tool {event: "approval"}` frames and
//! `done.pending_approvals` of a turn, the thread's messages'
//! `pending_approvals`, the feed's `approval.requested`, or a bound voice
//! session's `mcp_approval_request` items ([`crate::realtime`]). Each but
//! the last carries the call's `call_id`, the id its `ready` and `result`
//! frames carry (`lmgw_api_types::mcp_apps`), to match an approval to its
//! call by; a bound session's `lmgw.chat.frame` relaying the `approval`
//! frame carries it for the item.

pub use lmgw_api_types::chat_approvals::{
    code, ApprovalDecidedEvent, ApprovalDecision, ApprovalRequest, ApprovalsRequest, MOVED_ON,
};
pub use lmgw_api_types::chat_voice::Dictation;

use super::{read_refusal, ApiError, Method, Request};

/// `POST /chat/api/threads/{id}/approvals` with `body`.
pub fn approvals(thread_id: i64, body: &ApprovalsRequest) -> Request {
    Request::new(
        Method::Post,
        format!("/chat/api/threads/{thread_id}/approvals"),
    )
    .header("Accept", "text/event-stream")
    .json(serde_json::to_string(body).unwrap_or_default())
}

/// `POST /chat/api/threads/{id}/transcribe`: send the recording's bytes as
/// the body, its container as `content_type` (`audio/wav`, `audio/webm`,
/// `audio/ogg`, `audio/mpeg`, `audio/mp4`, `audio/flac`). [`Request::body`]
/// is `None`: the bytes are the caller's to send.
pub fn transcribe(thread_id: i64, content_type: &str) -> Request {
    Request::new(
        Method::Post,
        format!("/chat/api/threads/{thread_id}/transcribe"),
    )
    .header("Content-Type", content_type)
}

reader!(
    /// [`transcribe`]'s answer: the words, and who heard them.
    read_transcription -> Dictation
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_approvals_request_carries_every_verdict() {
        let r = approvals(
            7,
            &ApprovalsRequest {
                decisions: vec![ApprovalDecision {
                    approval_request_id: "mcpr_1".into(),
                    approve: false,
                    reason: Some("not now".into()),
                }],
                speak: false,
            },
        );
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.path, "/chat/api/threads/7/approvals");
        let body: serde_json::Value = serde_json::from_str(r.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["decisions"][0]["reason"], "not now");
        assert!(r
            .headers
            .iter()
            .any(|h| h.name == "Accept" && h.value == "text/event-stream"));
    }

    #[test]
    fn a_transcription_reads_and_a_refusal_names_its_code() {
        let r = transcribe(3, "audio/wav");
        assert_eq!(r.path, "/chat/api/threads/3/transcribe");
        assert!(r.body.is_none());
        let d =
            read_transcription(200, r#"{"text": "ja", "alias": "asr", "audio_ms": 900}"#).unwrap();
        assert_eq!(d.text, "ja");
        assert_eq!(d.audio_ms, Some(900));
        let e = read_transcription(422, r#"{"code": "asr_not_configured", "message": "no"}"#)
            .unwrap_err();
        assert_eq!(e.code, "asr_not_configured");
    }
}
