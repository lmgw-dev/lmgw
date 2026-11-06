//! (b): every answer and stream event lmgw sends on its OpenAI- and
//! Anthropic-shaped `/v1` routes, against the provider's schema.
//!
//! A layer around the router [`crate::common::serve_on`] serves (and so
//! [`crate::common::serve`]): a whole JSON answer is read, checked and sent
//! on unchanged; an event stream is checked event by event as it passes,
//! nothing held back. Routes not listed in [`Route::of`] pass untouched, and
//! so does an answer lmgw relays as its upstream wrote it (the native
//! `/v1/responses` passthrough, `egress::watch::Relayed`): there it is the
//! test's fake upstream that would be checked, not lmgw.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, Method};
use axum::middleware::Next;
use axum::response::Response;
use futures::StreamExt;
use serde_json::Value;

use super::diagnose;
use super::schemas::{self, Entry, Provider, Side};
use super::{known, ledger};

/// A `/v1` route whose answers the check reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Chat,
    Responses,
    Embeddings,
    Models,
    Messages,
    MessagesCount,
    ResponsesCount,
}

fn entry(provider: Provider, name: &'static str) -> Entry {
    Entry {
        provider,
        side: Side::Answer,
        name,
    }
}

impl Route {
    fn of(method: &Method, path: &str) -> Option<Self> {
        match (method, path.trim_end_matches('/')) {
            (&Method::POST, "/v1/chat/completions") => Some(Self::Chat),
            (&Method::POST, "/v1/responses") => Some(Self::Responses),
            (&Method::POST, "/v1/embeddings") => Some(Self::Embeddings),
            (&Method::GET, "/v1/models") => Some(Self::Models),
            (&Method::POST, "/v1/messages") => Some(Self::Messages),
            (&Method::POST, "/v1/messages/count_tokens") => Some(Self::MessagesCount),
            (&Method::POST, "/v1/responses/input_tokens") => Some(Self::ResponsesCount),
            _ => None,
        }
    }

    /// The schema a whole answer `v` is checked against, `ok` (2xx) or not.
    ///
    /// The gateway's own rules where the route's shape is not the whole
    /// story: a client that says `anthropic-version` gets Anthropic's model
    /// list (`server.rs`, `/v1/models`); the gateway's gate refuses such a
    /// client in Anthropic's shape on every route (`server/gate.rs`'s
    /// `policy_refusal`) while an OpenAI-shaped route's handler answers in
    /// OpenAI's, so a refusal off the Messages routes is read by its shape
    /// (Anthropic's says `"type": "error"`), and one on them is Anthropic's
    /// for a client that said so. A non-2xx `/v1/responses` answer is the
    /// error envelope like every other route's: a failed run's Response
    /// object goes only in a stream's `response.failed`.
    fn whole(self, ok: bool, anthropic_client: bool, v: &Value) -> Entry {
        let anthropic_shaped = v.get("type").and_then(Value::as_str) == Some("error");
        match (self, ok) {
            (Self::Chat, true) => entry(Provider::Openai, "chat_response"),
            (Self::Responses, true) => entry(Provider::Openai, "response"),
            (Self::Embeddings, true) => entry(Provider::Openai, "embeddings_response"),
            (Self::Models, true) if anthropic_client => {
                entry(Provider::Anthropic, "models_response")
            }
            (Self::Models, true) => entry(Provider::Openai, "models_response"),
            (Self::Messages, true) => entry(Provider::Anthropic, "message"),
            (Self::MessagesCount, true) => entry(Provider::Anthropic, "count_tokens_response"),
            (Self::ResponsesCount, true) => {
                entry(Provider::Openai, "responses_input_tokens_response")
            }
            (Self::Messages | Self::MessagesCount, false) if anthropic_client => {
                entry(Provider::Anthropic, "error_response")
            }
            (_, false) if anthropic_shaped => entry(Provider::Anthropic, "error_response"),
            (_, false) => entry(Provider::Openai, "error_response"),
        }
    }

    /// The schema of one stream event, by its `event:` name and its data;
    /// `None` for an event the protocol defines without a body to check.
    fn event(self, name: Option<&str>, data: &Value) -> Option<Entry> {
        match self {
            // OpenAI's SDKs read a chunk carrying `error` (and no choices) as
            // the stream's error, in the error object's shape.
            Self::Chat if data.get("error").is_some() && data.get("choices").is_none() => {
                Some(entry(Provider::Openai, "error_response"))
            }
            Self::Chat => Some(entry(Provider::Openai, "chat_stream_chunk")),
            Self::Responses => Some(entry(Provider::Openai, "responses_stream_event")),
            Self::Messages => match name {
                Some("ping") => None,
                Some("error") => Some(entry(Provider::Anthropic, "error_response")),
                _ => Some(entry(Provider::Anthropic, "stream_event")),
            },
            // No other route streams.
            Self::Embeddings | Self::Models | Self::MessagesCount | Self::ResponsesCount => None,
        }
    }
}

/// The layer's function (`axum::middleware::from_fn`).
pub async fn check(req: Request, next: Next) -> Response {
    let route = Route::of(req.method(), req.uri().path());
    let what = format!("{} {}", req.method(), req.uri().path());
    let anthropic_client = req.headers().contains_key("anthropic-version");
    let resp = next.run(req).await;
    let Some(route) = route else {
        return resp;
    };
    if resp
        .extensions()
        .get::<lmgw_core::egress::watch::Relayed>()
        .is_some()
    {
        return resp;
    }
    let ok = resp.status().is_success();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let (parts, body) = resp.into_parts();
    if content_type.starts_with("text/event-stream") {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Events {
            route,
            what,
            buf: Vec::new(),
        }));
        let at_end = std::sync::Arc::clone(&events);
        let stream = body
            .into_data_stream()
            .map(move |chunk| {
                if let Ok(bytes) = &chunk {
                    events.lock().unwrap_or_else(|e| e.into_inner()).feed(bytes);
                }
                chunk
            })
            // When the stream ends on its own (not when the client leaves
            // early), a last event without its blank line is checked too.
            .chain(
                futures::stream::once(async move {
                    at_end.lock().unwrap_or_else(|e| e.into_inner()).finish();
                })
                .filter_map(|()| async { None }),
            );
        return Response::from_parts(parts, Body::from_stream(stream));
    }
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        // The body failed on its way out: the client sees that failure as it
        // would have without the check.
        Err(e) => {
            return Response::from_parts(
                parts,
                Body::from_stream(futures::stream::once(async move {
                    Err::<axum::body::Bytes, _>(e)
                })),
            )
        }
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) => validate(route.whole(ok, anthropic_client, &v), &what, &v),
        Err(_) if bytes.is_empty() && !ok => {}
        Err(_) => ledger::record(format!(
            "{what} answered {} with a body that is not JSON ({content_type}): {}",
            parts.status,
            String::from_utf8_lossy(&bytes[..bytes.len().min(200)])
        )),
    }
    Response::from_parts(parts, Body::from(bytes))
}

/// An event stream, split into events as it passes.
struct Events {
    route: Route,
    what: String,
    buf: Vec<u8>,
}

impl Events {
    fn feed(&mut self, bytes: &[u8]) {
        // Bytes, not text: a chunk may end inside a character.
        self.buf.extend_from_slice(bytes);
        while let Some(end) = self.buf.windows(2).position(|w| w == b"\n\n") {
            let event: Vec<u8> = self.buf.drain(..end + 2).collect();
            self.one(&String::from_utf8_lossy(&event));
        }
    }

    /// The stream ended: whatever is left in the buffer is a last event the
    /// server did not terminate with a blank line.
    fn finish(&mut self) {
        let rest = std::mem::take(&mut self.buf);
        if !rest.iter().all(u8::is_ascii_whitespace) {
            self.one(&String::from_utf8_lossy(&rest));
        }
    }

    fn one(&self, event: &str) {
        let mut name = None;
        let mut data = Vec::new();
        for line in event.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                name = Some(v.trim());
            } else if let Some(v) = line.strip_prefix("data:") {
                data.push(v.strip_prefix(' ').unwrap_or(v));
            }
        }
        if data.is_empty() {
            return;
        }
        let data = data.join("\n");
        if data == "[DONE]" && matches!(self.route, Route::Chat) {
            return;
        }
        let v: Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(e) => {
                return ledger::record(format!(
                    "{} streamed an event whose data is not JSON ({e}): {data}",
                    self.what
                ))
            }
        };
        // A named event says what it is twice: by its name and its `type`.
        if let (Some(n), Some(t)) = (name, v.get("type").and_then(Value::as_str)) {
            if n != t && matches!(self.route, Route::Messages | Route::Responses) {
                ledger::record(format!(
                    "{} streamed `event: {n}` whose data says type {t}",
                    self.what
                ));
            }
        }
        if let Some(entry) = self.route.event(name, &v) {
            validate(entry, &self.what, &v);
        }
    }
}

fn validate(entry: Entry, what: &str, v: &Value) {
    let mut bad = Vec::new();
    for leaf in diagnose::leaves(schemas::validator(entry), v) {
        match known::matching(entry, &leaf) {
            Some(k) => ledger::note(format!("{entry} on {what} — {}\n  {leaf}", k.reason)),
            None => bad.push(format!("  {leaf}")),
        }
    }
    if !bad.is_empty() {
        ledger::record(format!(
            "{what} answered what {entry} refuses:\n{}\n  answer: {}",
            bad.join("\n"),
            diagnose::short(v)
        ));
    }
}
