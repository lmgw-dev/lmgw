//! What a session on a WebRTC call has beyond a WebSocket's (realtime calls
//! design §2.6, §2.8, §8.2): it hears lmgw ask it to end the call — a
//! hangup, a stop of the listener the call came in on — and a bound call's
//! last event says why it ends.
//!
//! - **Asked** ([`CallSide`]): the call's control is the writer's sink, so
//!   the session watches a copy of its question. A hangup closes with
//!   `Close::HungUp`, a stop of the call's listener as a stopping server's
//!   session does (`Close::GoingAway`).
//! - **`lmgw.session.closed`** ([`Core::say_closed`]): on a bound call,
//!   queued before the close, behind every event before it, in the
//!   WebSocket's close order (§8.2), and delivered before `Rtc::close` (the
//!   call's sink closes once SCTP acknowledged it, at most one
//!   `realtime.ping_interval_s` later). On a dead or stalled verdict it is
//!   queued only if the writer takes it at once ([`Core::say_closed_now`]):
//!   the output is what waits. An unbound call closes with nothing in band,
//!   as OpenAI's do.
//! - **Letting go** ([`CallSide::let_go`]): as the session ends, its call
//!   gives its slot back and its hangup's `Released` resolves, as a
//!   WebSocket session's slot goes back with its core, while the writer
//!   still delivers the last events.
//! - **A stray `lmgw.session.take_over`** (§2.8): one that crossed an
//!   automatic bind, or a second one, reaches the running session and is
//!   ignored with a DEBUG line.
//! - **A bound call's own client events** ([`Core::call_event`], §6.4):
//!   `lmgw.output_audio.playout` ([`Core::playout_report`], §5.3), the
//!   phone's report of what it plays, which measures the playout delay its
//!   playing window and drained follow; `lmgw.response.stop`, the user's
//!   stop (`lifecycle::stop`, §6.2). Anywhere else — an unbound call, a
//!   WebSocket — each of them, a `lmgw.session.take_over` outside a pending
//!   bind and an `lmgw.event.part` are answered `error` `invalid_event`
//!   naming the bound call they apply to. A bound call's parts never reach
//!   the session: its reader joins them (`calls::sizes`).
//! - **No lead** ([`lead_none`], §5.1): a call's frame clock pulls the
//!   audio, so `session.lmgw.output_lead_ms` echoes 0, whatever an update
//!   asks.

use std::sync::Arc;

use futures::FutureExt;
use lmgw_api_types::realtime::{
    OutputAudioPlayout, ResponseStop, EVENT_PART, OUTPUT_AUDIO_PLAYOUT, RESPONSE_STOP, TAKE_OVER,
};
use serde_json::Value;

use super::super::calls::{self, Asked, Ending, LetGo};
use super::super::link::{Close, Transport};
use super::super::protocol::{ErrorObject, ServerEvent};
use super::Core;

/// A call's side of its session (module doc).
pub(crate) struct CallSide {
    /// The call's id (`rtc_…`).
    pub id: Arc<str>,
    /// Where lmgw asks the session to end the call.
    pub asked: Asked,
    /// Lets the call go as the session ends (module doc).
    pub let_go: LetGo,
}

/// The next time lmgw asks the session to end its call; never for a
/// WebSocket's.
pub(super) async fn asked(call: Option<&mut CallSide>) -> Ending {
    match call {
        Some(c) => c.asked.ending().await,
        None => std::future::pending().await,
    }
}

/// The close for an ask (module doc).
pub(super) fn close_for(why: Ending) -> Close {
    match why {
        Ending::HungUp { by } => Close::HungUp { by },
        Ending::ShuttingDown => Close::GoingAway,
    }
}

impl Core {
    /// A bound call's last event before it closes for `close` (module doc);
    /// nothing on a WebSocket or an unbound call.
    pub(super) fn say_closed(&mut self, transport: &Transport, close: &Close) {
        if matches!(transport, Transport::WebRtc(_)) && self.bound.is_some() {
            self.ob
                .send(ServerEvent::LmgwSessionClosed(calls::link::closed_event(
                    close,
                )));
        }
    }

    /// [`Self::say_closed`] for a verdict's close (module doc): handed to
    /// the writer only if it takes it now — a client that stopped reading
    /// would hold the flush for good — and said at DEBUG when it could not
    /// be. What the outbox held goes with it, or not at all.
    pub(super) fn say_closed_now(&mut self, transport: &Transport, close: &Close) {
        if !matches!(transport, Transport::WebRtc(_)) || self.bound.is_none() {
            return;
        }
        self.say_closed(transport, close);
        if self.flush().now_or_never().is_none() {
            tracing::debug!(
                "realtime {}: the writer has no room for lmgw.session.closed; the call closes \
                 without it",
                self.id()
            );
        }
    }
}

/// Whether `kind` is one of a bound call's own client events (module doc).
pub(super) fn is_call_event(kind: &str) -> bool {
    [OUTPUT_AUDIO_PLAYOUT, RESPONSE_STOP, TAKE_OVER, EVENT_PART].contains(&kind)
}

/// A call's session runs with no lead (module doc).
pub(super) fn lead_none(session: &mut super::super::protocol::Session) {
    session
        .lmgw
        .get_or_insert_with(Default::default)
        .output_lead_ms = Some(0);
}

impl Core {
    /// One of a bound call's own client events (module doc), `kind` being
    /// its `type`, as it came off the link at `arrived`.
    pub(super) fn call_event(
        &mut self,
        kind: &str,
        value: Value,
        event_id: Option<&str>,
        arrived: tokio::time::Instant,
    ) {
        // A take-over or a part that reaches a bound call's session is no
        // event of its own: the stray take-over is ignored before (the
        // session loop), and the reader joins the parts.
        let served = self.on_call && self.bound.is_some();
        match kind {
            OUTPUT_AUDIO_PLAYOUT if served => self.playout_report(value, event_id, arrived),
            RESPONSE_STOP if served => match serde_json::from_value::<ResponseStop>(value) {
                Ok(stop) => self.response_stop(stop.playout_ntp_ms),
                Err(e) => self.error(
                    ErrorObject::invalid("invalid_event", format!("{RESPONSE_STOP}: {e}"))
                        .for_event(event_id),
                ),
            },
            _ => self.error(
                ErrorObject::invalid(
                    "invalid_event",
                    format!(
                        "{kind} applies to a call bound to a Chat thread (POST \
                         /v1/realtime/calls with chat_thread or chat_folder)"
                    ),
                )
                .with_param("type")
                .for_event(event_id),
            ),
        }
    }

    /// A bound call's `lmgw.output_audio.playout` (module doc), as it came
    /// off the link at `arrived`.
    fn playout_report(
        &mut self,
        value: Value,
        event_id: Option<&str>,
        arrived: tokio::time::Instant,
    ) {
        let report: OutputAudioPlayout = match serde_json::from_value(value) {
            Ok(r) => r,
            Err(e) => {
                return self.error(
                    ErrorObject::invalid("invalid_event", format!("{OUTPUT_AUDIO_PLAYOUT}: {e}"))
                        .for_event(event_id),
                )
            }
        };
        let notes = self
            .out
            .playout(arrived, report.playout_ntp_ms, report.jitter_buffer_ms);
        for n in notes {
            calls::audio::out::note(&format!("realtime {}", self.id()), &n);
        }
    }
}

/// Whether `text` is a `lmgw.session.take_over` a bound call's session
/// ignores (module doc): said at DEBUG, and nothing answers it. An unbound
/// call's, like a WebSocket's, is the core's to refuse.
pub(super) fn stray_take_over(core: &Core, transport: &Transport, text: &str) -> bool {
    if !matches!(transport, Transport::WebRtc(_)) || core.bound.is_none() {
        return false;
    }
    if !text.contains(lmgw_api_types::realtime::TAKE_OVER) {
        return false;
    }
    let kind = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string));
    if kind.as_deref() != Some(lmgw_api_types::realtime::TAKE_OVER) {
        return false;
    }
    tracing::debug!(
        "realtime {}: lmgw.session.take_over outside a pending bind (the call is bound \
         already); ignored",
        core.id()
    );
    true
}
