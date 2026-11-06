//! `/mcp` while a tool call runs (chat-media-tools design §7.2, §7.3).
//!
//! **Progress.** A `tools/call` whose `params._meta.progressToken` is set,
//! from a client whose `Accept` takes `text/event-stream`, for a tool that
//! reports progress (the `image__*` and `audio__*` tools), is answered as an
//! SSE stream: `notifications/progress` while it runs, then the JSON-RPC
//! response, then the stream ends (Streamable HTTP lets a POST answer
//! either way). Without the token, the `Accept` or such a tool, the answer
//! stays one JSON body. MCP requires `progress` to rise with every
//! notification: it counts the job's steps over all its passes, and each
//! message before the first step (loading the model, unloading another,
//! the queue) moves it half-way to 1, so it rises and stays below the
//! first step. A report that would not raise it is not sent. `total` is
//! the pass's step count while the job has had one pass, and absent once
//! a second began. Previews do not travel: MCP progress has no image.
//!
//! The stream sends a keep-alive comment every `SSE_KEEPALIVE`, as the
//! `GET` stream does: a phase that reports nothing new for a while (a job
//! asked for no preview, a wait for room whose figures stand still) would
//! otherwise leave the connection silent, and a proxy that times out an
//! idle connection would drop it — which cancels the job (§7.4).
//!
//! **Cancelled.** Every `tools/call` of a session is registered by its
//! request id while it runs ([`Calls`]); `notifications/cancelled
//! {requestId}` from the same session raises that call's stop. The image
//! and audio tools read it and end as the Chat's Stop ends one; a
//! southbound server's call is not cancelled yet: nothing on that path
//! reads the stop, and the call runs to its answer (or its server's
//! `timeout_ms`). The response is still sent; MCP lets the client ignore
//! it.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, LazyLock, Mutex};

use axum::http::HeaderMap;
use axum::response::sse::{Event as SseFrame, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::stream::{self, StreamExt};
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use super::{rpc_err, rpc_ok, CallCx, ToolPlane};
use crate::mcp::CallError;

use crate::mcp::media::Progress;
use crate::proxy::image_call::{MediaProgress, Steps};
use crate::proxy::StopHandle;

/// The running calls of every session, by `(session id, request id)` — the
/// id as its JSON, so `2` and `"2"` stay two ids.
static CALLS: LazyLock<Mutex<Running>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// `(session id, request id)` → the call's stop.
type Running = HashMap<(String, String), Arc<StopHandle>>;

fn calls() -> std::sync::MutexGuard<'static, Running> {
    CALLS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A running call's registration, taken off when dropped.
pub(super) struct Calls {
    key: (String, String),
    stop: Arc<StopHandle>,
}

impl Calls {
    /// Request `id` of session `sid` runs until the returned value drops;
    /// a `notifications/cancelled` for it raises `stop`.
    pub(super) fn register(sid: &str, id: &Value, stop: StopHandle) -> Self {
        let key = (sid.to_string(), id.to_string());
        let stop = Arc::new(stop);
        calls().insert(key.clone(), stop.clone());
        Self { key, stop }
    }
}

impl Drop for Calls {
    fn drop(&mut self) {
        let mut map = calls();
        if map
            .get(&self.key)
            .is_some_and(|s| Arc::ptr_eq(s, &self.stop))
        {
            map.remove(&self.key);
        }
    }
}

/// A `notifications/cancelled` with `params`, sent with `headers`: the
/// call it names is stopped when the headers name a live session and the
/// call is that session's (module doc).
pub(super) fn cancelled_notification(headers: &HeaderMap, params: Option<&Value>) {
    let sid = super::hget(headers, "mcp-session-id").filter(|s| super::session_exists(s));
    if let Some(sid) = sid {
        cancelled(sid, params.unwrap_or(&Value::Null));
    }
}

/// `notifications/cancelled`'s `params` on session `sid`: the call it
/// names is stopped. Whether one was.
pub(super) fn cancelled(sid: &str, params: &Value) -> bool {
    let Some(id) = params.get("requestId") else {
        return false;
    };
    let stop = calls().get(&(sid.to_string(), id.to_string())).cloned();
    match stop {
        Some(s) => {
            s.stop();
            true
        }
        None => false,
    }
}

/// Whether a `tools/call` is answered as a stream (module doc): its
/// progress token when it is.
pub(super) fn token(params: &Value, accepts_sse: bool) -> Option<Value> {
    if !accepts_sse {
        return None;
    }
    params
        .get("_meta")
        .and_then(|m| m.get("progressToken"))
        .filter(|t| t.is_string() || t.is_number())
        .cloned()
}

/// A call's `notifications/progress`, sent down `tx` (module doc).
pub(super) struct Notifications {
    token: Value,
    tx: mpsc::UnboundedSender<Value>,
    count: Mutex<Count>,
}

/// Where the count stands.
#[derive(Default)]
struct Count {
    /// The last progress sent.
    last: f64,
    /// Messages sent before the first step.
    before: u32,
    /// The pass the job is in, its step count, and the steps of the passes
    /// before it.
    pass: Option<(u32, u32)>,
    done: u64,
    passes: u32,
    /// An engine download's share of the count (machine-check design §5):
    /// where it began and the half-step it may fill, by its bytes — one
    /// before-step however many byte reports it sends.
    download: Option<(f64, f64)>,
}

impl Notifications {
    pub(super) fn new(token: Value, tx: mpsc::UnboundedSender<Value>) -> Self {
        Self {
            token,
            tx,
            count: Mutex::new(Count::default()),
        }
    }
}

impl Count {
    /// The progress, total and message of `p`; `None` when it would not
    /// rise.
    fn next(&mut self, p: &MediaProgress) -> Option<(f64, Option<u32>, String)> {
        let (progress, total, message) = match p {
            MediaProgress::Generating {
                model,
                steps: Some(Steps { pass, step, total }),
                ..
            } => {
                match self.pass {
                    Some((at, steps)) if at != *pass => {
                        self.done += u64::from(steps);
                        self.passes += 1;
                    }
                    None => self.passes = 1,
                    _ => {}
                }
                self.pass = Some((*pass, *total));
                let pass_word = if self.passes > 1 {
                    format!(", pass {pass}")
                } else {
                    String::new()
                };
                (
                    (self.done + u64::from(*step)) as f64,
                    (self.passes == 1).then_some(*total),
                    format!("{model}: step {step} / {total}{pass_word}"),
                )
            }
            MediaProgress::Downloading { done, total, .. } if self.pass.is_none() => {
                let (base, span) = match self.download {
                    Some(d) => d,
                    None => {
                        self.before += 1;
                        let d = (self.last, (1.0 - self.last) / 2.0);
                        self.download = Some(d);
                        d
                    }
                };
                let share = total
                    .filter(|t| *t > 0)
                    .map_or(0.0, |t| (*done as f64 / t as f64).min(1.0));
                (base + span * (1.0 + share) / 2.0, None, message(p))
            }
            other => {
                // Before the first step: half-way to 1 each time (module
                // doc); after it (a job without steps), as before.
                let progress = if self.pass.is_none() {
                    self.before += 1;
                    1.0 - 0.5f64.powi(self.before as i32)
                } else {
                    self.last
                };
                (progress, None, message(other))
            }
        };
        if progress <= self.last {
            return None;
        }
        self.last = progress;
        Some((progress, total, message))
    }
}

/// What a report before the steps says.
fn message(p: &MediaProgress) -> String {
    match p {
        MediaProgress::Downloading {
            model,
            image,
            done,
            total,
            size_note,
        } => {
            let fmt = crate::hf::fmt_bytes;
            let so_far = (*done > 0).then(|| format!("{} so far", fmt(*done)));
            match total {
                Some(t) => format!(
                    "{model}: downloading {image} ({} of {})",
                    fmt(*done),
                    fmt(*t)
                ),
                // The size not known is said, with why, rather than left
                // to read as a download that has not begun.
                None => match [so_far, size_note.clone()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                {
                    said if said.is_empty() => format!("{model}: downloading {image}"),
                    said => format!("{model}: downloading {image} ({})", said.join("; ")),
                },
            }
        }
        MediaProgress::Stage { phase, model, room } => match (phase.as_str(), room) {
            ("unloading", _) => format!("unloading {model}"),
            ("loading", _) => format!("loading {model}"),
            ("waiting_for_room", Some((needs, free))) => format!(
                "{model}: waiting for room (needs {}, {} free)",
                crate::hf::fmt_bytes(*needs),
                crate::hf::fmt_bytes(*free)
            ),
            (phase, _) => format!("{model}: {}", phase.replace('_', " ")),
        },
        MediaProgress::Queued {
            model, position, ..
        } => format!("{model}: queued at {position}"),
        MediaProgress::Generating { model, .. } => format!("{model}: generating"),
        MediaProgress::Upscaling { model, .. } => format!("{model}: upscaling"),
    }
}

impl Progress for Notifications {
    fn report(&self, p: &MediaProgress) {
        let next = self.count.lock().unwrap_or_else(|e| e.into_inner()).next(p);
        let Some((progress, total, message)) = next else {
            return;
        };
        let mut params = json!({
            "progressToken": self.token,
            "progress": progress,
            "message": message,
        });
        if let Some(t) = total {
            params["total"] = json!(t);
        }
        let _ = self.tx.send(json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": params,
        }));
    }
}

/// A `tools/call`'s JSON-RPC answer. §14: unknown tool → -32601;
/// transport/timeout → internal. Always HTTP 200 with a JSON-RPC error
/// object (the surface is JSON-RPC); the *log row* is what's mapped non-ok
/// (§10).
pub(super) fn call_answer(result: Result<Value, CallError>, id: Value) -> Value {
    match result {
        Ok(result) => rpc_ok(id, result),
        Err(e) => rpc_err(id, e.rpc_code(), &e.to_string()),
    }
}

/// A `tools/call` answered as SSE (chat-media-tools design §7.2): its
/// `notifications/progress` while it runs, then its answer, then the end.
/// The call runs as the stream is read: a client that goes away drops it,
/// which cancels a running image job (§7.4).
pub(super) fn progress_stream<P: ToolPlane + Clone + 'static>(
    plane: P,
    (id, name, args): (Value, String, Option<Map<String, Value>>),
    token: Value,
    stop: crate::proxy::StopSignal,
    registered: Option<Calls>,
) -> Response {
    type Call = std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>>;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let call: Call = Box::pin(async move {
        let _registered = registered;
        let cx = CallCx {
            progress: Some(Box::new(Notifications::new(token, tx))),
            stop: Some(stop),
        };
        call_answer(plane.call_tool(&name, args, cx).await, id)
    });
    // The notifications as they come; once the call has answered, what is
    // left of them, then the answer — never a notification after it.
    let frames = stream::unfold(
        (Some(call), rx, None::<Value>),
        |(mut call, mut rx, mut answer)| async move {
            if let Some(c) = call.as_mut() {
                tokio::select! {
                    biased;
                    Some(n) = rx.recv() => return Some((n, (call, rx, answer))),
                    a = c => {
                        call = None;
                        answer = Some(a);
                    }
                }
            }
            if let Ok(n) = rx.try_recv() {
                return Some((n, (call, rx, answer)));
            }
            answer.take().map(|a| (a, (call, rx, None)))
        },
    )
    .map(|v| Ok::<_, Infallible>(SseFrame::default().data(v.to_string())));
    Sse::new(frames)
        .keep_alive(KeepAlive::new().interval(super::SSE_KEEPALIVE))
        .into_response()
}

#[cfg(test)]
mod tests;
