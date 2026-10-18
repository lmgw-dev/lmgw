//! Pending (calls design §2.8, `takeover=ask`): the call's thread is held
//! when `oai-events` opens. No session runs; the client hears
//! `lmgw.session.bind_pending {held_by}` and the call waits for one of:
//!
//! - **the client's `lmgw.session.take_over`**: the bind runs once more with
//!   `takeover=always` (the checks again, for a folder its current thread
//!   decided again under its lock), taking the thread; its holder closes as
//!   any session taken over does;
//! - **the holder's end** (its session starts to end, or its binding goes):
//!   the bind runs again under `ask` and the session starts with no word
//!   from the client — or, when a third client took the thread meanwhile,
//!   another `bind_pending` names the new holder;
//! - **the client's decline**: closing `oai-events` (nothing is sent), or a
//!   hangup (`lmgw.session.closed {kind: "closed", by}`);
//! - **the wait's bound**: one `realtime.ping_interval_s` after the latest
//!   `bind_pending` (none at 0), then `lmgw.session.closed {kind:
//!   "thread_bound", by}` naming the holder;
//! - a revocation of the key (`revoked`), a stop of the call's listener or
//!   server (`shutting_down`).
//!
//! Every other client event is answered `error` `bind_pending`, naming its
//! type, and dropped; the audio is discarded, the audio kept since the
//! first frame included (§4.6): what the user says while deciding is not
//! the thread's.
//!
//! Nothing the wait says is waited for in one of its arms (`open`'s module
//! doc): while an answer is not taken yet, no client event is read, and
//! every other way out of the wait stays open.

use std::time::Duration;

use futures::StreamExt;
use lmgw_api_types::realtime::{BindPending, ClosedKind, SessionClosed, TAKE_OVER};
use serde_json::Value;
use tokio::time::Instant;

use super::super::handle::{CallEvent, Message};
use super::super::link;
use super::{revoked, Call, Came, Over};
use crate::realtime::protocol::{ErrorObject, ServerEvent};
use crate::realtime::thread::{self, Pending};
use crate::realtime::Takeover;

/// Wait while pending (module doc).
pub(super) async fn wait(mut call: Call, mut p: Pending) {
    let mut held_by = p.held_by().to_string();
    let interval = call.state.snapshot().settings.realtime.ping_interval_s;
    let bound = (interval > 0).then(|| Duration::from_secs(interval.into()));
    let mut until = asked_again(&mut call, &held_by, bound);
    let stopped = call.server_stopped();
    tokio::pin!(stopped);
    // The call's question as a copy: no arm below borrows its control.
    let mut asked = call.handle.control.asked();
    loop {
        tokio::select! {
            () = p.released() => match call.check_and_bind(p.target(), Takeover::Ask).await {
                Came::Bound(b) => return call.bound(*b, None).await,
                Came::Pending(next) => {
                    if next.held_by() != held_by {
                        held_by = next.held_by().to_string();
                        until = asked_again(&mut call, &held_by, bound);
                    }
                    p = next;
                }
                Came::Ends(e) => return call.ends(e).await,
            },
            // What the wait said, taken by the driver (module doc).
            Some(_) = call.said.next() => {}
            e = call.handle.events.next(), if call.said.is_empty() => match e {
                Some(CallEvent::Message { message: Message::Text(t), .. }) => {
                    let (kind, event_id) = kind_of(&t);
                    if kind.as_deref() == Some(TAKE_OVER) {
                        return take_over(call, p.target()).await;
                    }
                    let said = kind.map_or_else(|| "an event without a type".into(), |k| format!("'{k}'"));
                    refuse(&mut call, &said, &held_by, event_id.as_deref());
                }
                Some(CallEvent::Message { message: Message::Binary(_), .. }) => {
                    refuse(&mut call, "a binary message", &held_by, None);
                }
                // Discarded while the user decides (module doc).
                Some(
                    CallEvent::Audio(_)
                    | CallEvent::Connected
                    | CallEvent::Open
                    | CallEvent::SenderReport(_),
                ) => {}
                // The client declined by closing: nothing more to it.
                Some(CallEvent::ChannelClosed) => {
                    return call.end("the client closed oai-events while pending", None).await
                }
                // What it sent waited unread past the call's budget (a
                // flood while an answer was not taken): ended, saying why.
                Some(CallEvent::TooBig { queued, limit }) => {
                    let message = link::QueueOverrun { queued, limit }.to_string();
                    let closed = SessionClosed {
                        kind: ClosedKind::Closed,
                        by: None,
                        message: message.clone(),
                    };
                    return call.end(&message, Some(closed)).await;
                }
                Some(CallEvent::Ended(_)) | None => return,
            },
            why = asked.ending() => return call.end_over(Over::Asked(why)).await,
            reason = revoked(call.revocation.as_mut()) => return call.end_revoked(reason).await,
            () = deadline(until) => {
                let secs = bound.map_or(0, |b| b.as_secs());
                let message = format!(
                    "voice is in use on {held_by}: this call waited {secs} s \
                     (realtime.ping_interval_s) for lmgw.session.take_over or the holder's end"
                );
                let closed = SessionClosed {
                    kind: ClosedKind::ThreadBound,
                    by: Some(held_by),
                    message: message.clone(),
                };
                return call.end(&message, Some(closed)).await;
            }
            () = &mut stopped => {
                return call.end_over(Over::Asked(super::Ending::ShuttingDown)).await
            }
        }
    }
}

/// The wait's end; never without a bound.
async fn deadline(until: Option<Instant>) {
    match until {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// Say `bind_pending` naming `held_by`, and note it: the wait's new end.
fn asked_again(call: &mut Call, held_by: &str, bound: Option<Duration>) -> Option<Instant> {
    call.state
        .calls
        .note_pending(&call.id, Some(held_by.to_string()));
    tracing::info!(
        "WebRTC call {}: its thread is held by {held_by}; asking the client \
         (lmgw.session.bind_pending)",
        call.id
    );
    call.say(ServerEvent::LmgwSessionBindPending(BindPending {
        held_by: held_by.to_string(),
    }));
    bound.map(|b| Instant::now() + b)
}

/// The client's `take_over`: bind with `takeover=always` (module doc).
async fn take_over(mut call: Call, target: thread::Target) {
    tracing::info!("WebRTC call {}: the client takes the thread over", call.id);
    match call.check_and_bind(target, Takeover::Always).await {
        Came::Bound(b) => call.bound(*b, None).await,
        // A bind that takes over never waits.
        Came::Pending(p) => {
            tracing::error!(
                "WebRTC call {}: a bind that takes over came back pending on thread {}",
                call.id,
                p.thread_id()
            );
            call.end("internal: a take-over bind came back pending", None)
                .await
        }
        Came::Ends(e) => call.ends(e).await,
    }
}

/// An event's `type` and `event_id`, when it parses.
fn kind_of(text: &str) -> (Option<String>, Option<String>) {
    let v: Value = serde_json::from_str(text).unwrap_or_default();
    let field = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    (field("type"), field("event_id"))
}

/// `error` `bind_pending` for an event that came while pending.
fn refuse(call: &mut Call, said: &str, held_by: &str, event_id: Option<&str>) {
    let e = ErrorObject::invalid(
        "bind_pending",
        format!(
            "{said} was dropped: this call waits, its thread held by {held_by}, and no session \
             runs until lmgw.session.take_over or the holder's end (lmgw.session.bind_pending)"
        ),
    )
    .for_event(event_id);
    call.say(ServerEvent::error(e));
}
