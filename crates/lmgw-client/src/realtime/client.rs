//! The client events: what a client owns in a bound session (chat-voice
//! design §8.1) — its turn detection and half duplex in `session.update`,
//! the input buffer, the response, the truncate, and the answer to a call
//! that waits for an approval (client-apps design §6.4). The thread owns
//! the rest (model, voice, tools), so `response.create` carries no
//! overrides.

use serde_json::{json, Value};

/// The turn detection a client asks for in automatic mode
/// (`audio.input.turn_detection.type`) — not a thread's setting, which is
/// [`crate::types::chat::TurnDetectionMode`] (push-to-talk among them) and
/// which [`TurnDetection::for_automatic`] reads by its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnDetection {
    ServerVad,
    SemanticVad,
}

impl TurnDetection {
    pub fn key(self) -> &'static str {
        match self {
            TurnDetection::ServerVad => "server_vad",
            TurnDetection::SemanticVad => "semantic_vad",
        }
    }

    /// Automatic mode's detection from the thread's resolved value
    /// (`server_vad`, `semantic_vad` or `push_to_talk`): `push_to_talk`
    /// there means the session's own default, `semantic_vad`.
    pub fn for_automatic(resolved: Option<&str>) -> TurnDetection {
        match resolved {
            Some("server_vad") => TurnDetection::ServerVad,
            _ => TurnDetection::SemanticVad,
        }
    }

    /// What a `session.update` asks for: nothing (`null`, manual turns) in
    /// push-to-talk, else [`Self::for_automatic`].
    pub fn requested(push_to_talk: bool, resolved: Option<&str>) -> Option<TurnDetection> {
        (!push_to_talk).then(|| Self::for_automatic(resolved))
    }
}

/// A client event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    /// `session.update` with what the client owns: `session.type:
    /// "realtime"`, `audio.input.turn_detection` (`None`: `null`,
    /// push-to-talk) and `session.lmgw.half_duplex` (a client without echo
    /// cancellation sets it).
    SessionUpdate {
        turn_detection: Option<TurnDetection>,
        half_duplex: bool,
    },
    /// `input_audio_buffer.append`: base64 of PCM16 little-endian, mono at
    /// 24 kHz ([`ClientEvent::append_pcm16`]).
    Append { audio: String },
    /// `input_audio_buffer.commit`.
    Commit,
    /// `input_audio_buffer.clear`.
    Clear,
    /// `response.create`, with no overrides.
    ResponseCreate,
    /// `response.cancel`, of one response or of the active one.
    ResponseCancel { response_id: Option<String> },
    /// `conversation.item.truncate`: the assistant item cut at what was
    /// heard ([`crate::truncate`]).
    Truncate {
        item_id: String,
        content_index: u32,
        audio_end_ms: u64,
    },
    /// `conversation.item.create` of an `mcp_approval_response`: the answer
    /// to a call that waits ([`super::ServerEvent::ApprovalRequest`]); a
    /// [`ClientEvent::ResponseCreate`] after it resumes the thread's turn.
    ApprovalResponse {
        approval_request_id: String,
        approve: bool,
        reason: Option<String>,
    },
}

impl ClientEvent {
    /// `input_audio_buffer.append` of `samples`.
    pub fn append_pcm16(samples: &[i16]) -> Self {
        ClientEvent::Append {
            audio: crate::base64::encode(&crate::base64::pcm16_le_bytes(samples)),
        }
    }

    /// The text frame to send.
    pub fn to_json(&self) -> String {
        match self {
            ClientEvent::SessionUpdate {
                turn_detection,
                half_duplex,
            } => {
                let td = match turn_detection {
                    None => Value::Null,
                    Some(t) => json!({"type": t.key()}),
                };
                json!({
                    "type": "session.update",
                    "session": {
                        "type": "realtime",
                        "audio": {"input": {"turn_detection": td}},
                        "lmgw": {"half_duplex": half_duplex},
                    }
                })
            }
            ClientEvent::Append { audio } => {
                json!({"type": "input_audio_buffer.append", "audio": audio})
            }
            ClientEvent::Commit => json!({"type": "input_audio_buffer.commit"}),
            ClientEvent::Clear => json!({"type": "input_audio_buffer.clear"}),
            ClientEvent::ResponseCreate => json!({"type": "response.create"}),
            ClientEvent::ResponseCancel { response_id } => match response_id {
                Some(id) => json!({"type": "response.cancel", "response_id": id}),
                None => json!({"type": "response.cancel"}),
            },
            ClientEvent::Truncate {
                item_id,
                content_index,
                audio_end_ms,
            } => json!({
                "type": "conversation.item.truncate",
                "item_id": item_id,
                "content_index": content_index,
                "audio_end_ms": audio_end_ms,
            }),
            ClientEvent::ApprovalResponse {
                approval_request_id,
                approve,
                reason,
            } => {
                let mut item = json!({
                    "type": "mcp_approval_response",
                    "approval_request_id": approval_request_id,
                    "approve": approve,
                });
                if let Some(r) = reason {
                    item["reason"] = json!(r);
                }
                json!({"type": "conversation.item.create", "item": item})
            }
        }
        .to_string()
    }
}

/// `session.update`s sent and not answered yet: the session's echo is its
/// word on push-to-talk only once none is in flight — an echo of an older
/// update would undo a newer choice.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Updates(u32);

impl Updates {
    /// A `session.update` went.
    pub fn sent(&mut self) {
        self.0 += 1;
    }

    /// A `session.updated` came: whether it is the answer to the last
    /// update sent (or to none: the server's own).
    pub fn echo(&mut self) -> bool {
        self.0 = self.0.saturating_sub(1);
        self.0 == 0
    }
}
