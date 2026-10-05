//! SSE over a POST body — the chat send stream. EventSource cannot POST, so
//! this parses the raw SSE text off a fetch ReadableStream, exactly like the
//! old chat-app.js did, but typed.

use futures::StreamExt;
use serde_json::Value;
use wasm_bindgen::JsCast;

/// One parsed frame from `/chat/api/threads/{id}/send`.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// First frame of a send, an edited or a regenerated user message: the
    /// stored id of the user message this turn answers.
    Turn(i64),
    Delta(String),
    Reasoning(String),
    /// `event` is `start | args | ready | result`; fields vary (see payload).
    Tool(Value),
    Usage {
        prompt_tokens: Option<i64>,
        completion_tokens: Option<i64>,
    },
    /// llama.cpp per-token timings (raw block, `timings` keys).
    Stats(Value),
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
    Done(Value),
    /// A voice frame (chat-voice §4.3, §6.3): a model's `state`, the
    /// read-aloud's `voice`, `speech`, `speech_done` or `speech_error` —
    /// the name and its data, for `chat_voice` to read.
    Voice(&'static str, Value),
}

fn parse_record(record: &str) -> Option<ChatEvent> {
    let mut event = "message";
    let mut data = String::new();
    for line in record.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
        // ":" keep-alive comments fall through untouched
    }
    let v: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
    frame(event, v)
}

/// One frame by its event name and data: an SSE record's, or a chat-turn
/// frame a bound realtime session relays verbatim (`lmgw.chat.frame`,
/// chat-voice §8.7) — the same bubble code renders both.
pub fn frame(event: &str, v: Value) -> Option<ChatEvent> {
    Some(match event {
        "turn" => ChatEvent::Turn(v["user_message_id"].as_i64()?),
        "delta" => ChatEvent::Delta(v["text"].as_str().unwrap_or_default().to_string()),
        "reasoning" => ChatEvent::Reasoning(v["text"].as_str().unwrap_or_default().to_string()),
        "tool" => ChatEvent::Tool(v),
        "usage" => ChatEvent::Usage {
            prompt_tokens: v["prompt_tokens"].as_i64(),
            completion_tokens: v["completion_tokens"].as_i64(),
        },
        "stats" => ChatEvent::Stats(v),
        "retrieval" => match serde_json::from_value(v) {
            Ok(c) => ChatEvent::Retrieval(c),
            Err(_) => return None,
        },
        "stop" => ChatEvent::Stop,
        "error" => {
            let msg = v["message"].as_str().unwrap_or("stream failed").to_string();
            match v["code"].as_str() {
                Some("superseded") => ChatEvent::Superseded,
                Some("not_saved") => ChatEvent::NotSaved(msg),
                code => ChatEvent::Error {
                    message: msg,
                    code: code.filter(|c| !c.is_empty()).map(str::to_string),
                },
            }
        }
        "done" => ChatEvent::Done(v),
        "state" => ChatEvent::Voice("state", v),
        "voice" => ChatEvent::Voice("voice", v),
        "speech" => ChatEvent::Voice("speech", v),
        "speech_done" => ChatEvent::Voice("speech_done", v),
        "speech_error" => ChatEvent::Voice("speech_error", v),
        _ => return None,
    })
}

/// POST `body` to `url` and feed every parsed SSE frame to `on_event`.
/// Returns Err only for transport/setup failures and an outright refusal
/// (non-2xx, so no SSE frame was ever produced — the caller can tell this
/// apart from a mid-stream failure by whether `on_event` ran at all); an
/// in-stream `error` frame still arrives through `on_event` and this
/// resolves `Ok`. `signal` aborts the fetch (Stop button).
pub async fn send_stream(
    url: &str,
    body: &Value,
    signal: &web_sys::AbortSignal,
    mut on_event: impl FnMut(ChatEvent),
) -> Result<(), String> {
    let resp = gloo_net::http::Request::post(url)
        .abort_signal(Some(signal))
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !(200..300).contains(&resp.status()) {
        let status = resp.status();
        // A refusal here (`unsupported_attachment`, `model_no_vision`, a
        // plain body_limit 413…) is still worth reading: without this the
        // send just shows "HTTP 413"/"HTTP 415" and the caller never learns
        // why (review finding: error text never reaches the user).
        let text = resp.text().await.unwrap_or_default();
        return Err(crate::api::parse_error_body(status, &text).message);
    }
    let raw = resp
        .body()
        .ok_or("response had no body")?
        .unchecked_into::<web_sys::ReadableStream>();
    let mut stream = wasm_streams::ReadableStream::from_raw(raw).into_stream();

    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break }; // aborted or network drop
        let bytes = js_sys::Uint8Array::from(chunk).to_vec();
        buf.extend_from_slice(&bytes);
        // SSE records end at a blank line; the terminator is ASCII, so byte
        // scanning never splits UTF-8 inside a record.
        while let Some(pos) = find_record_end(&buf) {
            let record: Vec<u8> = buf.drain(..pos + 2).collect();
            let text = String::from_utf8_lossy(&record);
            let text = text.trim();
            if text.is_empty() || text.starts_with(':') {
                continue;
            }
            if let Some(ev) = parse_record(text) {
                on_event(ev);
            }
        }
    }
    Ok(())
}

fn find_record_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
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
