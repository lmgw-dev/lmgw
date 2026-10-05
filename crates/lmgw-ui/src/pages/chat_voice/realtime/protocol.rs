//! The realtime protocol as the panel speaks it (chat-voice §8, realtime
//! §2.3): the server events it reads, parsed into [`Server`], and the client
//! events it sends. Pure, so it is tested natively.
//!
//! Only what the panel acts on is typed; every other event is
//! [`Server::Other`] by its `type`. The `lmgw.*` events are a bound
//! session's own (§8.7).

use serde_json::{json, Value};

use crate::pages::audio_stream::b64_decode;

/// One server event the panel acts on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Server {
    /// `session.created` / `session.updated`: the session in effect.
    Session(Box<SessionFacts>),
    SpeechStarted,
    SpeechStopped,
    Committed,
    /// `input_audio_buffer.cleared`: the open turn and its audio are gone.
    Cleared,
    /// `conversation.item.input_audio_transcription.delta`.
    HeardDelta(String),
    /// `….completed`: the user's words, final.
    Heard(String),
    /// `….failed`: its error's code (`asr_not_configured`, `gpu_hold`, …)
    /// and message.
    HeardFailed {
        code: Option<String>,
        message: String,
    },
    ResponseCreated(String),
    /// `response.output_audio.delta`, decoded: PCM16-LE at 24 kHz.
    Audio {
        response_id: String,
        item_id: String,
        pcm: Vec<u8>,
    },
    /// `response.output_audio.done`.
    AudioDone {
        response_id: String,
    },
    /// `response.output_audio_transcript.delta`: the reply's words, paced
    /// with its audio (the captions).
    Spoken {
        response_id: String,
        delta: String,
    },
    /// `response.done`: its id, status, and the reason or error it gives.
    ResponseDone {
        id: String,
        status: String,
        reason: Option<String>,
    },
    Error(ErrorFacts),
    /// `lmgw.chat.frame`: a chat-turn frame of a response, verbatim.
    ChatFrame {
        response_id: String,
        event: String,
        data: Value,
    },
    /// `lmgw.chat.user`: the user message a response's turns became —
    /// with the response's id when it heard them as audio, its row then
    /// racing its reply (voice-audio-input §3.3).
    ChatUser {
        message_id: i64,
        content: String,
        voice: Value,
        response_id: Option<String>,
    },
    /// `lmgw.chat.input`: how a response's turns reach the chat model —
    /// `audio` or `transcript` — and why the transcript.
    ChatInput {
        response_id: String,
        input: String,
        why: Option<String>,
    },
    /// `lmgw.chat.reply`: a spoken reply as finalized (`content`, `unheard`,
    /// `voice`; or `removed`; or `skipped`).
    ChatReply {
        message_id: i64,
        body: Value,
    },
    /// `lmgw.model.state`: a stage's model state (§4.3).
    ModelState(Value),
    /// `lmgw.response.timing`: a response's timing line as data.
    Timing(Value),
    /// `lmgw.chat.thread`: the thread as a response re-read it.
    Thread(ThreadFacts),
    /// `conversation.item.truncated`: the server's cut, at `audio_end_ms`.
    Truncated(u64),
    Other(String),
}

/// An `error` event's fields.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ErrorFacts {
    pub kind: String,
    pub code: Option<String>,
    pub message: String,
}

/// `session.lmgw.resolved.chat_thread` and `lmgw.chat.thread`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ThreadFacts {
    pub id: i64,
    pub title: String,
    pub admin_tools: bool,
}

/// What the panel reads of the session in effect.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SessionFacts {
    /// `session.updated`, the answer to a `session.update` (else
    /// `session.created`, the session before the page's first update).
    pub echo: bool,
    /// `audio.input.turn_detection.type`; `None`: push-to-talk (manual
    /// turns).
    pub turn_detection: Option<String>,
    /// `server_vad.prefix_padding_ms`, when the session runs it.
    pub prefix_padding_ms: Option<u32>,
    pub half_duplex: Option<bool>,
    /// `session.lmgw.resolved`: what answers.
    pub chat: Option<String>,
    pub asr: Option<String>,
    pub tts: Option<String>,
    pub voice: Option<String>,
    pub thread: Option<ThreadFacts>,
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

fn opt(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

impl ThreadFacts {
    fn of(v: &Value) -> Option<Self> {
        v.is_object().then(|| ThreadFacts {
            id: v["id"].as_i64().unwrap_or_default(),
            title: s(v, "title"),
            admin_tools: v["admin_tools"].as_bool().unwrap_or(false),
        })
    }
}

impl SessionFacts {
    pub(crate) fn of(session: &Value) -> Self {
        let td = &session["audio"]["input"]["turn_detection"];
        let r = &session["lmgw"]["resolved"];
        SessionFacts {
            echo: false,
            turn_detection: opt(&td["type"]),
            prefix_padding_ms: td["prefix_padding_ms"].as_u64().map(|n| n as u32),
            half_duplex: session["lmgw"]["half_duplex"].as_bool(),
            chat: opt(&r["chat"]),
            asr: opt(&r["asr"]),
            tts: opt(&r["tts"]),
            voice: opt(&r["voice"]),
            thread: ThreadFacts::of(&r["chat_thread"]),
        }
    }
}

/// Parse one text frame; `None` for one that is no JSON object with a
/// `type`.
pub(crate) fn parse(text: &str) -> Option<Server> {
    let v: Value = serde_json::from_str(text).ok()?;
    let kind = v["type"].as_str()?.to_string();
    Some(match kind.as_str() {
        "session.created" | "session.updated" => {
            let mut f = SessionFacts::of(&v["session"]);
            f.echo = kind == "session.updated";
            Server::Session(Box::new(f))
        }
        "input_audio_buffer.speech_started" => Server::SpeechStarted,
        "input_audio_buffer.speech_stopped" => Server::SpeechStopped,
        "input_audio_buffer.committed" => Server::Committed,
        "input_audio_buffer.cleared" => Server::Cleared,
        "conversation.item.input_audio_transcription.delta" => Server::HeardDelta(s(&v, "delta")),
        "conversation.item.input_audio_transcription.completed" => {
            Server::Heard(s(&v, "transcript"))
        }
        "conversation.item.input_audio_transcription.failed" => Server::HeardFailed {
            code: opt(&v["error"]["code"]),
            message: s(&v["error"], "message"),
        },
        "response.created" => Server::ResponseCreated(s(&v["response"], "id")),
        "response.output_audio.delta" => Server::Audio {
            response_id: s(&v, "response_id"),
            item_id: s(&v, "item_id"),
            pcm: b64_decode(v["delta"].as_str().unwrap_or_default()).unwrap_or_default(),
        },
        "response.output_audio.done" => Server::AudioDone {
            response_id: s(&v, "response_id"),
        },
        "response.output_audio_transcript.delta" => Server::Spoken {
            response_id: s(&v, "response_id"),
            delta: s(&v, "delta"),
        },
        "response.done" => {
            let r = &v["response"];
            let d = &r["status_details"];
            Server::ResponseDone {
                id: s(r, "id"),
                status: s(r, "status"),
                // The message says it in words; the code is the fallback
                // (WP11 UI review m6).
                reason: opt(&d["reason"])
                    .or_else(|| opt(&d["error"]["message"]))
                    .or_else(|| opt(&d["error"]["code"])),
            }
        }
        "error" => {
            let e = &v["error"];
            Server::Error(ErrorFacts {
                kind: s(e, "type"),
                code: opt(&e["code"]),
                message: s(e, "message"),
            })
        }
        "lmgw.chat.frame" => Server::ChatFrame {
            response_id: s(&v, "response_id"),
            event: s(&v, "event"),
            data: v["data"].clone(),
        },
        "lmgw.chat.user" => Server::ChatUser {
            message_id: v["message_id"].as_i64().unwrap_or_default(),
            content: s(&v, "content"),
            voice: v["voice"].clone(),
            response_id: opt(&v["response_id"]),
        },
        "lmgw.chat.input" => Server::ChatInput {
            response_id: s(&v, "response_id"),
            input: s(&v, "input"),
            why: opt(&v["why"]),
        },
        "lmgw.chat.reply" => {
            let mut body = v.clone();
            if let Some(o) = body.as_object_mut() {
                o.remove("type");
                o.remove("event_id");
                o.remove("message_id");
            }
            Server::ChatReply {
                message_id: v["message_id"].as_i64().unwrap_or_default(),
                body,
            }
        }
        "conversation.item.truncated" => {
            Server::Truncated(v["audio_end_ms"].as_u64().unwrap_or_default())
        }
        "lmgw.model.state" => Server::ModelState(strip(v)),
        "lmgw.response.timing" => Server::Timing(strip(v)),
        "lmgw.chat.thread" => match ThreadFacts::of(&v["chat_thread"]) {
            Some(t) => Server::Thread(t),
            None => Server::Other(kind),
        },
        _ => Server::Other(kind),
    })
}

/// A flat event's own fields, without `type` and `event_id`.
fn strip(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.remove("type");
        o.remove("event_id");
    }
    v
}

/// `session.update`s sent and not answered yet (WP9 review NIT 4): the
/// session's echo is its word on push-to-talk only once none is in flight —
/// an echo of an older update would undo a newer choice.
#[derive(Debug, Default)]
pub(crate) struct Updates(u32);

impl Updates {
    pub(crate) fn sent(&mut self) {
        self.0 += 1;
    }

    /// A `session.updated` came: whether it is the answer to the last
    /// update sent (or to none: the server's own).
    pub(crate) fn echo(&mut self) -> bool {
        self.0 = self.0.saturating_sub(1);
        self.0 == 0
    }
}

/// The turn detection the panel asks for: the thread's resolved value in
/// automatic mode (`push_to_talk` there means the session's own default,
/// `semantic_vad`), `null` for push-to-talk.
pub(crate) fn turn_detection(ptt: bool, resolved: &str) -> Value {
    if ptt {
        return Value::Null;
    }
    match resolved {
        "server_vad" => json!({"type": "server_vad"}),
        _ => json!({"type": "semantic_vad"}),
    }
}

/// `session.update` with what the client owns (§8.1): the turn detection
/// and half duplex (echo mode `none`, §12.1). The rest is the thread's.
pub(crate) fn session_update(turn_detection: Value, half_duplex: bool) -> String {
    json!({
        "type": "session.update",
        "session": {
            "type": "realtime",
            "audio": {"input": {"turn_detection": turn_detection}},
            "lmgw": {"half_duplex": half_duplex},
        }
    })
    .to_string()
}

pub(crate) fn append(b64: &str) -> String {
    json!({"type": "input_audio_buffer.append", "audio": b64}).to_string()
}

pub(crate) fn commit() -> String {
    json!({"type": "input_audio_buffer.commit"}).to_string()
}

pub(crate) fn clear() -> String {
    json!({"type": "input_audio_buffer.clear"}).to_string()
}

pub(crate) fn response_create() -> String {
    json!({"type": "response.create"}).to_string()
}

pub(crate) fn response_cancel(response_id: Option<&str>) -> String {
    match response_id {
        Some(id) => json!({"type": "response.cancel", "response_id": id}),
        None => json!({"type": "response.cancel"}),
    }
    .to_string()
}

/// Cut the assistant item at what was heard (§8.4, §11.1: `heard`, not
/// `played`).
pub(crate) fn truncate(item_id: &str, audio_end_ms: u64) -> String {
    json!({
        "type": "conversation.item.truncate",
        "item_id": item_id,
        "content_index": 0,
        "audio_end_ms": audio_end_ms,
    })
    .to_string()
}

/// The handshake's URL: this page's origin, `ws:` or `wss:` as the page is
/// `http:` or `https:`.
pub(crate) fn socket_url(protocol: &str, host: &str, thread: i64) -> String {
    let scheme = if protocol == "https:" { "wss" } else { "ws" };
    format!("{scheme}://{host}/v1/realtime?chat_thread={thread}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_says_its_turn_detection_and_thread() {
        let ev = json!({"type": "session.created", "event_id": "e1", "session": {
            "type": "realtime",
            "audio": {"input": {"turn_detection": {"type": "server_vad", "prefix_padding_ms": 300}}},
            "lmgw": {"half_duplex": false, "resolved": {"chat": "gemma", "asr": "parakeet",
                "tts": "supertonic", "voice": "F2",
                "chat_thread": {"id": -3, "title": "Plan", "temporary": true, "admin_tools": true}}}
        }});
        let Some(Server::Session(f)) = parse(&ev.to_string()) else {
            panic!("a session")
        };
        assert_eq!(f.turn_detection.as_deref(), Some("server_vad"));
        assert!(!f.echo, "session.created is no answer to an update");
        assert_eq!(f.prefix_padding_ms, Some(300));
        assert_eq!(f.asr.as_deref(), Some("parakeet"));
        assert_eq!(
            f.thread,
            Some(ThreadFacts {
                id: -3,
                title: "Plan".into(),
                admin_tools: true
            })
        );
        // Manual turns: a null turn detection.
        let ev = json!({"type": "session.updated", "session": {"audio": {"input": {"turn_detection": null}}}});
        let Some(Server::Session(f)) = parse(&ev.to_string()) else {
            panic!("a session")
        };
        assert_eq!(f.turn_detection, None);
        assert!(f.echo);
        assert_eq!(f.thread, None);
        assert_eq!(
            parse(r#"{"type":"input_audio_buffer.cleared","event_id":"e"}"#),
            Some(Server::Cleared)
        );
    }

    #[test]
    fn only_the_echo_of_the_last_update_is_the_session_s_word() {
        let mut u = Updates::default();
        u.sent();
        u.sent();
        assert!(!u.echo(), "an older update's answer");
        assert!(u.echo(), "the last one's");
        assert!(u.echo(), "one nobody here sent");
    }

    #[test]
    fn audio_is_decoded_and_tagged_with_its_response() {
        let ev = json!({"type": "response.output_audio.delta", "event_id": "e", "response_id": "resp_1",
            "item_id": "item_9", "output_index": 0, "content_index": 0, "delta": "AAEC"});
        assert_eq!(
            parse(&ev.to_string()),
            Some(Server::Audio {
                response_id: "resp_1".into(),
                item_id: "item_9".into(),
                pcm: vec![0, 1, 2],
            })
        );
    }

    #[test]
    fn a_done_response_says_why_it_ended() {
        let ev = json!({"type": "response.done", "response": {"id": "r", "status": "cancelled",
            "status_details": {"type": "cancelled", "reason": "turn_detected"}}});
        assert_eq!(
            parse(&ev.to_string()),
            Some(Server::ResponseDone {
                id: "r".into(),
                status: "cancelled".into(),
                reason: Some("turn_detected".into())
            })
        );
        let ev = json!({"type": "response.done", "response": {"id": "r", "status": "failed",
            "status_details": {"type": "failed", "error": {"code": "empty_turn"}}}});
        let Some(Server::ResponseDone { reason, .. }) = parse(&ev.to_string()) else {
            panic!()
        };
        assert_eq!(reason.as_deref(), Some("empty_turn"));
        // Words before a code.
        let ev = json!({"type": "response.done", "response": {"id": "r", "status": "failed",
            "status_details": {"type": "failed",
                               "error": {"code": "gpu_hold", "message": "held"}}}});
        let Some(Server::ResponseDone { reason, .. }) = parse(&ev.to_string()) else {
            panic!()
        };
        assert_eq!(reason.as_deref(), Some("held"));
    }

    #[test]
    fn a_failed_transcription_keeps_its_code() {
        let ev = json!({"type": "conversation.item.input_audio_transcription.failed",
            "item_id": "i", "content_index": 0,
            "error": {"type": "invalid_request_error", "code": "asr_not_configured",
                      "message": "this turn cannot be transcribed"}});
        assert_eq!(
            parse(&ev.to_string()),
            Some(Server::HeardFailed {
                code: Some("asr_not_configured".into()),
                message: "this turn cannot be transcribed".into(),
            })
        );
        // The code is left out when there is none (§5.2's shape).
        let ev = json!({"type": "conversation.item.input_audio_transcription.failed",
            "item_id": "i", "content_index": 0, "error": {"type": "server_error", "message": "m"}});
        assert!(matches!(
            parse(&ev.to_string()),
            Some(Server::HeardFailed { code: None, .. })
        ));
    }

    #[test]
    fn the_extension_events_are_read() {
        let ev = json!({"type": "lmgw.chat.reply", "event_id": "e", "message_id": 812,
            "content": "Heard", "unheard": "rest", "voice": {"via": "realtime"}});
        let Some(Server::ChatReply { message_id, body }) = parse(&ev.to_string()) else {
            panic!()
        };
        assert_eq!(message_id, 812);
        assert_eq!(
            body,
            json!({"content": "Heard", "unheard": "rest", "voice": {"via": "realtime"}})
        );
        let ev = json!({"type": "lmgw.model.state", "event_id": "e", "stage": "asr", "alias": "a",
            "state": "loading", "ms": null});
        assert_eq!(
            parse(&ev.to_string()),
            Some(Server::ModelState(
                json!({"stage": "asr", "alias": "a", "state": "loading", "ms": null})
            ))
        );
        let ev = json!({"type": "lmgw.chat.thread", "chat_thread": {"id": 4, "title": "T",
            "temporary": false, "admin_tools": false}});
        assert_eq!(
            parse(&ev.to_string()),
            Some(Server::Thread(ThreadFacts {
                id: 4,
                title: "T".into(),
                admin_tools: false
            }))
        );
        let ev = json!({"type": "error", "error": {"type": "invalid_request_error",
            "code": "chat_thread_taken_over", "message": "voice mode moved", "param": null}});
        assert_eq!(
            parse(&ev.to_string()),
            Some(Server::Error(ErrorFacts {
                kind: "invalid_request_error".into(),
                code: Some("chat_thread_taken_over".into()),
                message: "voice mode moved".into()
            }))
        );
        assert_eq!(
            parse(r#"{"type":"rate_limits.updated"}"#),
            Some(Server::Other("rate_limits.updated".into()))
        );
        assert_eq!(parse("not json"), None);
    }

    #[test]
    fn the_client_events_are_what_the_server_takes() {
        let v: Value =
            serde_json::from_str(&session_update(turn_detection(true, "x"), true)).unwrap();
        assert_eq!(v["session"]["type"], "realtime");
        assert!(v["session"]["audio"]["input"]["turn_detection"].is_null());
        assert_eq!(v["session"]["lmgw"]["half_duplex"], true);
        assert_eq!(
            turn_detection(false, "server_vad"),
            json!({"type": "server_vad"})
        );
        // A thread resolved to push-to-talk, switched to automatic.
        assert_eq!(
            turn_detection(false, "push_to_talk"),
            json!({"type": "semantic_vad"})
        );
        let v: Value = serde_json::from_str(&truncate("item_1", 1234)).unwrap();
        assert_eq!(
            v,
            json!({"type": "conversation.item.truncate", "item_id": "item_1",
                "content_index": 0, "audio_end_ms": 1234})
        );
        let v: Value = serde_json::from_str(&response_cancel(Some("r1"))).unwrap();
        assert_eq!(v, json!({"type": "response.cancel", "response_id": "r1"}));
        assert_eq!(
            socket_url("https:", "gw.example:8443", -2),
            "wss://gw.example:8443/v1/realtime?chat_thread=-2"
        );
        assert_eq!(
            socket_url("http:", "127.0.0.1:8001", 7),
            "ws://127.0.0.1:8001/v1/realtime?chat_thread=7"
        );
    }
}
