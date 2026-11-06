//! SSE over a POST body — the chat send stream. EventSource cannot POST, so
//! this reads the records off the answer's byte stream, which the page's
//! transport hands it (the dashboard's: the fetch body), exactly like the
//! old chat-app.js did, but typed.

mod read;

use futures::StreamExt;
use lmgw_api_types::chat_frames as frames;
use lmgw_ui_kit::http::{sse, Body, Http, Read, Request};
use serde_json::Value;

pub use read::{ToolEvent, VoiceEvent};

/// One parsed frame from `/chat/api/threads/{id}/send`.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// First frame of a send, an edited or a regenerated user message: the
    /// stored id of the user message this turn answers.
    Turn(i64),
    Delta(String),
    Reasoning(String),
    /// A tool call's step: `start`, `args`, `ready`, `approval` or `result`.
    Tool(ToolEvent),
    Usage {
        prompt_tokens: Option<i64>,
        completion_tokens: Option<i64>,
    },
    /// llama.cpp per-token timings, live while the reply streams.
    Stats(frames::Timings),
    /// What the turn retrieved from the thread's knowledge bases, before the
    /// first delta (chat-complete §9.3).
    Retrieval(super::chat_retrieval::KbContext),
    Stop,
    /// The turn failed: the message, and the gateway's code when a gateway
    /// error is behind it (`gpu_hold`, `context_length_exceeded`, …; none for
    /// a stream that broke mid-way).
    Error {
        message: String,
        code: Option<String>,
    },
    /// A newer turn of the thread (or a rewrite of its history) stopped this
    /// reply — not a failure, and it is not saved.
    Superseded,
    /// The reply's save was refused; `done` follows with `saved: false`.
    NotSaved(String),
    /// The turn's last frame. The voice warm-up's empty `done` reads as one
    /// that saved nothing and did not abort.
    Done(frames::DoneFrame),
    /// A voice frame (chat-voice §4.3, §6.3): a model's `state`, or the
    /// read-aloud's `voice`, `speech`, `speech_done` or `speech_error`, for
    /// `chat_voice` to read.
    Voice(VoiceEvent),
}

/// The text of a `delta` or `reasoning` frame.
fn text_of(v: Value) -> String {
    serde_json::from_value::<frames::TextFrame>(v)
        .unwrap_or_default()
        .text
}

/// One frame by its event name and data: an SSE record's, or a chat-turn
/// frame a bound realtime session relays verbatim (`lmgw.chat.frame`,
/// chat-voice §8.7) — the same bubble code renders both.
pub fn frame(event: &str, v: Value) -> Option<ChatEvent> {
    Some(match event {
        "turn" => match serde_json::from_value::<frames::TurnStarted>(v) {
            Ok(t) => ChatEvent::Turn(t.user_message_id),
            Err(_) => return None,
        },
        "delta" => ChatEvent::Delta(text_of(v)),
        "reasoning" => ChatEvent::Reasoning(text_of(v)),
        "tool" => ChatEvent::Tool(read::tool(v)),
        "usage" => {
            let u: frames::UsageFrame = serde_json::from_value(v).unwrap_or_default();
            ChatEvent::Usage {
                prompt_tokens: u.prompt_tokens.map(|n| n as i64),
                completion_tokens: u.completion_tokens.map(|n| n as i64),
            }
        }
        "stats" => match serde_json::from_value(v) {
            Ok(t) => ChatEvent::Stats(t),
            Err(_) => return None,
        },
        "retrieval" => match serde_json::from_value::<frames::RetrievalFrame>(v) {
            Ok(f) => ChatEvent::Retrieval(f.into()),
            Err(_) => return None,
        },
        "stop" => ChatEvent::Stop,
        "error" => {
            // A frame that is not a whole `ErrorFrame` (no message) still
            // keeps the code it carries, which is what the page branches on.
            let e: frames::ErrorFrame =
                serde_json::from_value(v.clone()).unwrap_or_else(|_| frames::ErrorFrame {
                    message: "stream failed".into(),
                    code: v.get("code").and_then(|c| c.as_str()).map(str::to_string),
                    ..Default::default()
                });
            match e.code.as_deref() {
                Some("superseded") => ChatEvent::Superseded,
                Some("not_saved") => ChatEvent::NotSaved(e.message),
                code => ChatEvent::Error {
                    code: code.filter(|c| !c.is_empty()).map(str::to_string),
                    message: e.message,
                },
            }
        }
        "done" => ChatEvent::Done(read::done(v)?),
        "state" | "voice" | "speech" | "speech_done" | "speech_error" => {
            ChatEvent::Voice(read::voice(event, v)?)
        }
        _ => return None,
    })
}

/// Why a stream was never opened, or broke: the gateway's `code` when a
/// refusal body carried one (`approval_moved_on`, `key_rate`, …), none for a
/// transport failure.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub code: Option<String>,
    pub message: String,
}

/// [`send_stream_coded`] with the refusal reduced to its message.
pub async fn send_stream(
    http: Http,
    url: &str,
    body: &Value,
    signal: Option<&web_sys::AbortSignal>,
    on_event: impl FnMut(ChatEvent),
) -> Result<(), String> {
    send_stream_coded(http, url, body, signal, on_event)
        .await
        .map_err(|r| r.message)
}

/// POST `body` to `url` and feed every parsed SSE frame to `on_event`.
/// Returns Err only for transport/setup failures and an outright refusal
/// (non-2xx, so no SSE frame was ever produced — the caller can tell this
/// apart from a mid-stream failure by whether `on_event` ran at all); an
/// in-stream `error` frame still arrives through `on_event` and this
/// resolves `Ok`. `signal` aborts the request (Stop button); whether an
/// `Err` is that stop is the signal's to say (`aborted()`), not the
/// error's text, which each transport words its own way.
pub async fn send_stream_coded(
    http: Http,
    url: &str,
    body: &Value,
    signal: Option<&web_sys::AbortSignal>,
    mut on_event: impl FnMut(ChatEvent),
) -> Result<(), Refusal> {
    post_sse(http, url, body, signal, |event, data| {
        if let Some(ev) = frame(event, data) {
            on_event(ev);
        }
    })
    .await
}

/// POST `body` to `url` through the page's transport and hand every SSE
/// record to `on_record` as its event name and its data, whatever the
/// stream's events are (a Chat turn's, or an image card action's:
/// `chat_media::actions`). Refusals and failures as [`send_stream_coded`]
/// says.
pub async fn post_sse(
    http: Http,
    url: &str,
    body: &Value,
    signal: Option<&web_sys::AbortSignal>,
    mut on_record: impl FnMut(&str, Value),
) -> Result<(), Refusal> {
    let transport = |e: String| Refusal {
        code: None,
        message: e,
    };
    let req = Request::post(url)
        .body(Body::json(body).map_err(transport)?)
        .read(Read::Stream)
        .signal(signal);
    let resp = match http.send(req).await {
        Ok(r) => r,
        Err(e) => return Err(transport(e.to_string())),
    };
    if !resp.ok() {
        let status = resp.status;
        // A refusal here (`unsupported_attachment`, `model_no_vision`, a
        // plain body_limit 413…) is still worth reading: without this the
        // send just shows "HTTP 413"/"HTTP 415" and the caller never learns
        // why (review finding: error text never reaches the user).
        let text = resp.text().await.unwrap_or_default();
        let e = lmgw_ui_kit::http::parse_error_body(status, &text);
        return Err(Refusal {
            code: Some(e.code).filter(|c| !c.is_empty() && c != "http_error"),
            message: e.message,
        });
    }
    let mut stream = resp.stream();
    let mut records = sse::Decoder::default();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break }; // aborted or network drop
        for r in records.push(&chunk) {
            on_record(
                &r.event,
                serde_json::from_str(&r.data).unwrap_or(Value::Null),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_error_frame_keeps_its_code() {
        match frame("error", json!({"message": "held", "code": "gpu_hold"})) {
            Some(ChatEvent::Error { message, code }) => {
                assert_eq!(
                    (message.as_str(), code.as_deref()),
                    ("held", Some("gpu_hold"))
                );
            }
            other => panic!("{other:?}"),
        }
        // A stream that broke mid-way has no gateway error behind it.
        match frame("error", json!({"message": "stream failed"})) {
            Some(ChatEvent::Error { code: None, .. }) => {}
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            frame("error", json!({"message": "m", "code": "superseded"})),
            Some(ChatEvent::Superseded)
        ));
        assert!(matches!(
            frame("error", json!({"message": "m", "code": "not_saved"})),
            Some(ChatEvent::NotSaved(_))
        ));
    }
}
