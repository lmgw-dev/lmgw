//! One realtime session: the reader loop and the session core (realtime
//! design §4.1).
//!
//! The socket's reader (`inbox`) stamps each frame with when it came off the
//! socket; the session loop parses each text frame into a [`ClientEvent`]
//! and hands it to the [`Core`] (`events`), which owns the session object, the conversation
//! and the response state, and is the only writer of all three. A response's
//! model call runs on its own task (`responder`) and reports back over a
//! channel; the core turns what it hears into events (`output`). Every answer
//! is queued in the core's outbox and flushed to the writer before the next
//! frame or responder message is taken — the writer's flow control reaches
//! the reader there. The writer reports back too: when a response's output
//! has all left (the drained acknowledgement, §4.3). A frame that does not
//! parse is an `error` and the session continues, as with OpenAI.
//!
//! The session pings its client and ends when a ping goes unanswered for a
//! whole `realtime.ping_interval_s`, even while the core is stuck waiting
//! for a writer the client stopped reading from (`liveness`).
//!
//! **Scope today (WP4): text and spoken conversations.** `session.update`,
//! the `conversation.item.*` events and `response.create` / `.cancel` are
//! served (`lifecycle` holds the response rules of §4.3), and so is the input
//! audio buffer (`input`: turn detection, commits, ASR per turn and the
//! automatic response), audio output (`speech`: the TTS alias and voice;
//! the responder speaks, the writer paces, and `truncate` cuts what was
//! heard) and barge-in (§6.4: speech while the client plays the answer,
//! judged by the evidence gate, cancels it with `turn_detected`).
//!
//! [`ClientEvent`]: super::protocol::ClientEvent

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures::StreamExt;
use tokio::sync::mpsc;

use super::asr::{self, AsrResolution};
use super::audio_in::AudioIn;
use super::conversation::Conversation;
use super::handshake::{Limits, CLOSE_TOO_BIG};
use super::ids::Ids;
use super::inbox::Inbox;
use super::input::{barge_params, detector_params, log_turn_detection, OpenTurn};
use super::lifecycle::{Active, Pending, TurnTiming};
use super::liveness::{Answer, Beat, Liveness};
use super::mcp_tools::McpSession;
use super::merge;
use super::protocol::{ErrorObject, ServerEvent, Session};
use super::resolve::{self, ChatResolution};
use super::responder;
use super::scorer::Scorer;
use super::transcribe::{AsrMsg, Transcriber};
use super::warm::Warmer;
use super::writer::{self, Outbox, WriterHandle};
use crate::policy::ConcurrencyGuard;
use crate::proxy::RequestCtx;
use crate::state::SharedState;

mod events;
pub(crate) mod speech;
mod stop;

use stop::Stop;

pub(in crate::realtime) use events::set_resolved;
use speech::Speech;

/// Everything the handshake settled, handed to the session at the 101.
pub(crate) struct SessionInit {
    pub state: SharedState,
    pub ctx: RequestCtx,
    /// `?model=`, as the client sent it.
    pub requested_model: Option<String>,
    pub chat: ChatResolution,
    pub asr: AsrResolution,
    /// The TTS alias and the voice (§5.3).
    pub speech: Speech,
    /// The key's concurrency slot, taken from the gate (§10.3): held for the
    /// session's whole life, released when this task ends.
    pub slot: Option<ConcurrencyGuard>,
    pub limits: Limits,
    /// The Chat thread the session is bound to (chat-voice design §8), when
    /// the handshake named one.
    pub bound: Option<super::thread::Binding>,
    /// Held to the session's last line: a stopping server waits for it.
    /// Taken in the handshake, before the 101, so a stop that comes while
    /// the upgrade completes still counts it (review F-3).
    pub running: Option<crate::server::Running>,
}

/// Run one session until the client closes, the socket fails, a size limit
/// is tripped, or the client stops answering pings.
pub(crate) async fn run(socket: WebSocket, mut init: SessionInit) {
    // Held to the session's last line (`SessionInit::running`).
    let _running = init.running.take();
    // A device's session is one of its connections (client-apps design
    // §1.6): online while it lives, `last_seen_at` at both ends, and closed
    // with 4003 when the device is revoked. `None` for every other caller.
    let mut device = crate::devices::connect(
        &init.state,
        &init.ctx.principal,
        init.ctx.revocation_mark,
        crate::devices::LinkKind::Voice,
    )
    .await;
    let (sink, stream) = socket.split();
    let ids = Arc::new(Ids::new());
    let (out, mut drained, mut writer_task) = writer::spawn(sink, ids.clone());
    let limits = init.limits;
    // Reads ahead at most the largest frame a client may send (`inbox`).
    let mut inbox = Inbox::spawn(stream, limits.max_frame, out.round_trip());
    let ping_interval_s = init.state.snapshot().settings.realtime.ping_interval_s;
    // The server this session's upgrade came in on stopping closes it
    // (1001): the drain does not wait for an upgraded connection, so
    // without this a restarted gateway's old sessions would run on. Its
    // generation is the request's (review F-3), not the one after a stop
    // that came during the handshake.
    let server_stopped = init
        .state
        .stops
        .stopped_after(init.state.stops.at_or_now(init.ctx.served_at));
    tokio::pin!(server_stopped);
    // A device's admin-tools switch moved: its `lmgw` label is listed again
    // (review P-8).
    let mut reach_moves = init.state.devices.reach_moves.watch();
    let mut stopping = false;
    let mut live = Liveness::new(ping_interval_s);
    // A "round trip" as long as the ping interval answered no ping of its
    // own (E3). With pings off there are none to bound.
    if ping_interval_s > 0 {
        out.round_trip()
            .bound(std::time::Duration::from_secs(u64::from(ping_interval_s)));
    }
    let (responder_tx, mut from_responder) = mpsc::unbounded_channel();
    let (asr_tx, mut from_asr) = mpsc::unbounded_channel();
    let (score_tx, mut from_scorer) = mpsc::unbounded_channel();
    let mut core = Core::new(init, ids, out.clone(), responder_tx, asr_tx, score_tx);
    // The session's MCP listings report here (realtime-server-tools §1.2).
    let (mcp_tx, mut from_mcp) = mpsc::unbounded_channel();
    core.mcp.listen(mcp_tx);
    // A bound session's journal and connect warm say what they did here
    // (chat-voice §8.3, §8.7); an unbound session's are never fed.
    let (bound_tx, mut from_journal) = mpsc::unbounded_channel();
    let (states_tx, mut from_warm) = mpsc::unbounded_channel();
    let (verdicts_tx, mut from_verdict) = mpsc::unbounded_channel();
    core.start_bound(bound_tx, states_tx, verdicts_tx);
    core.vet_owner_check_alias().await;
    core.start();
    // One ping on the client's first frame, not after a whole interval: the
    // barge-in window's margin wants the round trip before the first answer
    // plays (§6.4). A pong to it lifts nothing — no verdict is pending — and
    // a client that never says anything is never probed.
    let mut probed = !live.on();
    // Another window binding the session's thread takes it over (§8.1).
    let taken = core.bound.as_ref().map(|b| b.taken.clone());
    // Its thread's approvals decided elsewhere (client-apps design §6.4);
    // an unbound session listens to no thread's wakes.
    let bound = core.bound.is_some();
    let mut approval_wakes = bound.then(|| core.state.chat_live.approval_wakes());
    // Its thread's MCP task results entered or were answered (MCP Tasks
    // design §3.4), read off this loop and taken at `from_results`; read
    // once now that the session listens, for a result that entered between
    // the bind's read and here.
    let mut result_wakes = bound.then(|| core.state.chat_live.result_wakes());
    let (results_tx, mut from_results) = mpsc::unbounded_channel();
    if bound {
        core.results_listen(results_tx);
        core.results_woken(false);
    }
    let mut taken_over = false;
    let mut out_of_reach = false;
    let mut revoked = None;

    let mut stop = flush_alive(&mut core, &out, &mut live, &mut inbox)
        .await
        .err();
    while stop.is_none() {
        // No bias: the core's state is always ahead of what the client has
        // seen (it writes every event itself), so the order the sources are
        // taken in cannot reorder anything — and a cancel must not wait for
        // a fast stream to pause.
        tokio::select! {
            Some((gen, msg)) = from_responder.recv() => core.on_responder(gen, msg),
            Some(asr) = from_asr.recv() => match asr {
                AsrMsg::Turn(done) => core.on_transcript(done),
                AsrMsg::Check(done) => core.on_word_check(done),
            },
            Some(score) = from_scorer.recv() => core.on_turn_score(score),
            Some(listed) = from_mcp.recv() => core.on_listed(listed),
            Ok(()) = reach_moves.changed() => core.mcp_reach_moved(),
            Some(gen) = drained.recv() => core.on_drained(gen),
            Some(ev) = from_journal.recv() => core.journal_event(ev),
            Some(state) = from_warm.recv() => core.model_state(state),
            Some((seq, verdict)) = from_verdict.recv() => core.audio_verdict(seq, verdict),
            // A wake for this thread — or one missed in a lag, which may
            // have been — reads its approvals again.
            woke = wake(approval_wakes.as_mut()) => {
                let thread = core.bound.as_ref().map(|b| b.thread_id);
                match woke {
                    Ok(id) if Some(id) != thread => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                    _ => {
                        core.approvals_woken().await;
                        // A history write: a result whose reply went is
                        // owed again.
                        core.results_woken(true);
                    }
                }
            }
            // The same for its job results (`thread::tasks`).
            woke = wake(result_wakes.as_mut()) => {
                let thread = core.bound.as_ref().map(|b| b.thread_id);
                match woke {
                    Ok(id) if Some(id) != thread => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                    _ => core.results_woken(false),
                }
            }
            Some(read) = from_results.recv() => core.results_read(read),
            () = super::thread::taken(taken.as_ref()) => {
                // The same stop ends a session whose thread left its
                // device's reach (client-apps design L3, review W3-1).
                if core.bound.as_ref().is_some_and(|b| b.out_of_reach()) {
                    core.out_of_reach();
                    out_of_reach = true;
                } else {
                    core.taken_over();
                    taken_over = true;
                }
                break;
            }
            () = &mut server_stopped => {
                stopping = true;
                break;
            }
            reason = crate::devices::revoked(device.as_mut()) => {
                revoked = Some(reason);
                if let Some(b) = core.bound.as_ref() {
                    b.revoked();
                }
                break;
            }
            beat = live.beat() => match beat {
                Beat::Ping => ping(&out, core.id()),
                Beat::Dead(reason) => {
                    stop = verdict(&mut live, &mut inbox, reason);
                    if stop.is_some() {
                        break;
                    }
                }
            },
            read = inbox.next() => match read.frame {
                // When it came off the socket: the detector places appended
                // audio on the wall clock by it (§6.4).
                Some(Ok(Message::Text(text))) => {
                    if !std::mem::replace(&mut probed, true) {
                        out.ping();
                    }
                    core.on_text(text.as_str(), read.at).await
                }
                Some(Ok(Message::Binary(_))) => core.error(ErrorObject::invalid(
                    "invalid_event",
                    "binary frames are not part of the Realtime protocol; send each event as a \
                     JSON text frame",
                )),
                Some(Ok(Message::Close(_))) | None => break,
                // tungstenite answers pings itself.
                Some(Ok(Message::Ping(_))) => {}
                Some(Ok(Message::Pong(_))) => live.pong(),
                Some(Err(e)) => {
                    match limits.close_reason(e) {
                        Some(reason) => {
                            tracing::info!("realtime {}: closed — {reason}", core.id());
                            stop = flush_alive(&mut core, &out, &mut live, &mut inbox)
                                .await
                                .err();
                            out.close(CLOSE_TOO_BIG, reason);
                        }
                        None => tracing::debug!("realtime {}: read failed, closing", core.id()),
                    }
                    break;
                }
            },
        }
        stop = flush_alive(&mut core, &out, &mut live, &mut inbox)
            .await
            .err();
    }
    let id = core.id().to_string();
    // Ending from here: a client's `takeover=never` rebind (after this
    // close, or a link that died) is not refused by it (review F-5).
    if let Some(b) = core.bound.as_ref() {
        b.closing();
    }
    if taken_over {
        stop = flush_alive(&mut core, &out, &mut live, &mut inbox)
            .await
            .err();
        if stop.is_none() {
            let reason = super::thread::taken_over_reason(core.taken_by().as_deref());
            tracing::info!("realtime {id}: closed — {reason}");
            out.close(super::thread::CLOSE_TAKEN_OVER, reason);
        }
    }
    if out_of_reach {
        stop = flush_alive(&mut core, &out, &mut live, &mut inbox)
            .await
            .err();
        if stop.is_none() {
            let why =
                super::thread::out_of_reach_reason(core.bound.as_ref().map_or(0, |b| b.thread_id));
            tracing::info!("realtime {id}: closed — {why}");
            out.close(super::thread::CLOSE_OUT_OF_REACH, why);
        }
    }
    if stopping {
        stop = flush_alive(&mut core, &out, &mut live, &mut inbox)
            .await
            .err();
        if stop.is_none() {
            tracing::info!("realtime {id}: closed — {}", crate::server::STOPPING);
            out.close(
                crate::server::CLOSE_GOING_AWAY,
                crate::server::STOPPING.to_string(),
            );
        }
    }
    if let (Some(reason), Some(conn)) = (revoked, device.as_ref()) {
        stop = flush_alive(&mut core, &out, &mut live, &mut inbox)
            .await
            .err();
        if stop.is_none() {
            let why = crate::devices::close_reason(reason, conn.name(), conn.is_device());
            tracing::info!("realtime {id}: closed — {why}");
            out.close(crate::devices::CLOSE_REVOKED, why);
        }
    }
    if let Some(stop) = stop {
        let (close, log) = stop.close(&limits);
        tracing::info!("realtime {id}: {log}");
        if let Some((code, reason)) = close {
            out.close(code, reason);
        }
    }

    // A bound session's turn still running saves its partial reply, and
    // the journal writes everything it holds before the session ends
    // (chat-voice §8.6).
    core.end_bound(&mut from_asr, live.grace()).await;
    // The core holds the concurrency slot and the active response; dropping
    // it gives the slot back and stops the response's model call — which
    // still writes its row (§4.3) — the moment the client is gone, not when
    // the writer has finished.
    drop(core);
    // The reader goes with it: a client that never closes keeps no socket.
    drop(inbox);
    drop(out);
    // The writer sends what is queued, then closes; a client that stopped
    // reading gets one ping interval for it, not forever.
    match live.grace() {
        Some(grace) => {
            if tokio::time::timeout(grace, &mut writer_task).await.is_err() {
                tracing::info!(
                    "realtime {id}: the client did not take the last events within {} s \
                     (realtime.ping_interval_s); the socket is dropped without them",
                    grace.as_secs_f64()
                );
                writer_task.abort();
            }
        }
        None => {
            let _ = writer_task.await;
        }
    }
    tracing::info!("realtime {id}: session ended");
}

/// Flush the core's outbox, keeping the liveness clock running while the
/// writer has no room: a client that has not taken the session's output for
/// a whole ping interval after a ping has stopped reading. `Err`: why the
/// session ends.
///
/// Only a pong lifts a verdict here. A read side that ended is the end too
/// (§10.4's one-interval bound): the flush cannot finish for a peer that
/// stopped reading, and the end it read would be read again on every tick.
async fn flush_alive(
    core: &mut Core,
    out: &WriterHandle,
    live: &mut Liveness,
    inbox: &mut Inbox,
) -> Result<(), Stop> {
    let id = core.id().to_string();
    let flush = core.flush();
    tokio::pin!(flush);
    loop {
        tokio::select! {
            () = &mut flush => return Ok(()),
            beat = live.beat() => match beat {
                Beat::Ping => ping(out, &id),
                Beat::Dead(reason) => {
                    if let Some(stop) = verdict(live, inbox, reason) {
                        return Err(stop);
                    }
                }
            },
        }
    }
}

/// The next wake of a bound session's thread wake channel; never, for an
/// unbound session (it listens to none).
async fn wake(
    rx: Option<&mut tokio::sync::broadcast::Receiver<i64>>,
) -> Result<i64, tokio::sync::broadcast::error::RecvError> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// The interval's ping — and, once, that no round trip was measured on
/// the pings so far (`liveness::round_trip`, fix package B6).
fn ping(out: &WriterHandle, session_id: &str) {
    if let Some(n) = out.round_trip().unmeasured_once() {
        tracing::info!(
            "realtime {session_id}: {n} pings went out and no pong echoed one's payload (RFC \
             6455 has a client echo it), so no round trip is measured — the barge-in window's \
             margin is echo_tail_ms alone (§6.4); said once"
        );
    }
    out.ping();
}

/// A [`Beat::Dead`] verdict, checked against what the reader already
/// passed on (`Liveness::answered_meanwhile`): `None` when the client's
/// pong was there after all.
fn verdict(live: &mut Liveness, inbox: &mut Inbox, reason: String) -> Option<Stop> {
    match live.answered_meanwhile(inbox) {
        Answer::Pong => None,
        Answer::Ended(end) => Some(Stop::Gone(end)),
        Answer::Nothing => Some(Stop::NoPong(reason)),
    }
}

/// The session core: the only writer of session state (§4.1).
pub(super) struct Core {
    pub(super) state: SharedState,
    pub(super) ctx: RequestCtx,
    pub(super) ids: Arc<Ids>,
    pub(super) out: WriterHandle,
    /// Events decided but not yet handed to the writer.
    pub(super) ob: Outbox,
    pub(super) session: Session,
    pub(super) chat: ChatResolution,
    pub(super) asr: AsrResolution,
    /// The TTS alias and voice audio responses speak with (§5.3).
    pub(super) speech: Speech,
    pub(super) conversation: Conversation,
    /// The input audio buffer and its turn detector (§4.1, §6).
    pub(super) input: AudioIn,
    /// The turn the detector has open: the item it will become.
    pub(super) turn: Option<OpenTurn>,
    /// The ASR calls of committed turns, one at a time (§4.2).
    pub(super) transcriber: Transcriber,
    /// Smart Turn, for `semantic_vad`'s pauses (§6.3).
    pub(super) scorer: Scorer,
    /// A pause Smart Turn could not score was warned about (`input::
    /// semantic`): once per session.
    pub(super) score_warned: bool,
    /// A committed turn's automatic response, before it is created (§4.3).
    pub(super) pending: Option<Pending>,
    /// The one response in flight (§4.3).
    pub(super) active: Option<Active>,
    /// The session's `mcp` labels and what they listed
    /// (realtime-server-tools §1.2).
    pub(super) mcp: McpSession,
    /// The latest committed turn no response has taken yet: the next
    /// response's timing line starts from it (§11).
    pub(super) last_turn: Option<TurnTiming>,
    pub(super) warmer: Warmer,
    /// The last response generation handed out; generations start at 1.
    pub(super) generation: u64,
    pub(super) responder_tx: responder::Tx,
    /// The barge-in window's margin last logged (E3).
    pub(super) margin_seen: Option<std::time::Duration>,
    /// The transcription language's script was warned about
    /// (`input::word_check`): once per session.
    pub(super) scripts_warned: bool,
    /// The settings snapshot the voice was last resolved against
    /// (`speech::refresh_voice`, B2 review 5).
    pub(super) voice_seen: std::sync::Weak<crate::config::Snapshot>,
    /// What the owner configured when a response read the list kept as
    /// `speech.facts.seen` (`voice::list_key`, R6): a snapshot that changes
    /// it drops that list, any other keeps it.
    pub(super) list_key: String,
    /// The TTS seed lmgw drew for this session (WP10 D6): a designed voice
    /// stays the same across its responses — a new session gets another
    /// one, unless the client pins `speech_seed`.
    pub(super) seed: u32,
    /// The Chat thread this session is bound to (chat-voice design §8);
    /// `None` for every other session.
    pub(super) bound: Option<super::thread::Bound>,
    /// The stages the bound session's connect warm loads (§4.1), until it
    /// starts.
    connect_warm: Vec<super::warm::Warm>,
    _slot: Option<ConcurrencyGuard>,
}

impl Core {
    pub(super) fn new(
        init: SessionInit,
        ids: Arc<Ids>,
        out: WriterHandle,
        responder_tx: responder::Tx,
        asr_tx: super::transcribe::Tx,
        score_tx: super::scorer::Tx,
    ) -> Self {
        let snap = init.state.snapshot();
        let id = ids.session();
        // The session's `model` is what the client asked for, or — for a
        // model-less handshake — the alias it started on (§5.1).
        let model = init
            .requested_model
            .clone()
            .or_else(|| init.chat.alias.clone());
        let mut session = merge::initial_session(&snap.settings.realtime, id, model);
        let mut binding = init.bound;
        if let Some(b) = &binding {
            // What the thread chose, as the session's own (§8.1).
            super::thread::shape_session(&mut session, b, &snap.settings.realtime);
        }
        // The session's own TTS seed (WP10 D6), unless the client pins one.
        let seed = rand::random::<u32>();
        set_resolved(
            &mut session,
            &init.chat,
            &init.asr,
            &init.speech,
            (&snap.settings.realtime, seed),
        );
        if let Some(b) = &binding {
            super::thread::set_resolved(&mut session, b);
        }
        let sid = session.id.clone().unwrap_or_default();
        if let Some(b) = &binding {
            tracing::info!(
                "realtime {sid}: bound to chat thread {} ('{}'{})",
                b.thread_id,
                b.title,
                if b.temporary { ", temporary" } else { "" }
            );
        }
        resolve::log_resolution(&sid, init.requested_model.as_deref(), &init.chat);
        asr::log_resolution(&sid, None, &init.asr);
        speech::log(&sid, &init.speech, &speech::requested_voice(&session));
        super::input::warn_unusable_semantic_rows(&sid, &snap.settings.realtime);
        let params = detector_params(&session, &snap.settings.realtime);
        log_turn_detection(&sid, None, &session, params.as_ref());
        let mut input = AudioIn::new(params.as_ref()).unwrap_or_else(|e| {
            // Only a stored setting can be out of range here.
            tracing::error!(
                "realtime {sid}: the turn-detection settings are unusable ({e}); this session \
                 starts on OpenAI's defaults"
            );
            AudioIn::new(Some(&Default::default())).expect("the defaults are valid")
        });
        let barge = barge_params(&session, &snap.settings.realtime);
        let check = super::input::check_alias(&session, init.asr.alias.as_deref());
        input.set_barge_in(&barge, check.is_some());
        if let Some(name) =
            super::turn::scripts::unknown(&snap.settings.realtime.barge_in_check_scripts)
        {
            tracing::warn!(
                "realtime {sid}: realtime.barge_in_check_scripts names '{name}', a script lmgw \
                 does not know (known: {}); no word in it counts for the barge-in word check",
                super::turn::scripts::known().join(", ")
            );
        }
        let mut transcriber =
            Transcriber::new(init.state.clone(), init.ctx.clone(), asr_tx, sid.clone());
        if let Some(b) = &binding {
            transcriber.bind(b.thread_id);
        }
        Self {
            transcriber,
            scorer: Scorer::new(init.state.clone(), score_tx),
            score_warned: false,
            state: init.state,
            ctx: init.ctx,
            ids,
            out,
            ob: Outbox::default(),
            session,
            chat: init.chat,
            asr: init.asr,
            speech: init.speech,
            conversation: Conversation::default(),
            input,
            turn: None,
            pending: None,
            active: None,
            mcp: McpSession::default(),
            last_turn: None,
            warmer: Warmer::default(),
            generation: 0,
            margin_seen: None,
            scripts_warned: false,
            voice_seen: Arc::downgrade(&snap),
            list_key: String::new(),
            seed,
            responder_tx,
            connect_warm: binding
                .as_mut()
                .map(|b| std::mem::take(&mut b.warm))
                .unwrap_or_default(),
            bound: binding.as_mut().map(super::thread::Bound::new),
            _slot: init.slot,
        }
    }

    /// Hand the outbox to the writer, waiting for room (flow control).
    pub(super) async fn flush(&mut self) {
        self.ob.flush(&self.out).await;
    }

    pub(super) fn id(&self) -> &str {
        self.session.id.as_deref().unwrap_or("?")
    }

    /// `session.created`, immediately on connect (§2.3) — and the session's
    /// models warmed, unless the owner said not to (§9.1).
    fn start(&mut self) {
        self.ob.send(ServerEvent::SessionCreated {
            session: Box::new(self.session.clone()),
        });
        if let Some(b) = &self.bound {
            // Entering voice mode is the press (chat-voice §4.1): the
            // thread's models, admitted as one group, said as
            // `lmgw.model.state`.
            let states = b.states.clone();
            let models = std::mem::take(&mut self.connect_warm);
            let sid = self.session.id.clone().unwrap_or_default();
            self.warmer.warm_to(
                &self.state,
                &sid,
                super::warm::WarmMode::Admit,
                models,
                states,
            );
        } else if self.state.snapshot().settings.realtime.warm_on_connect {
            self.warm();
        }
        self.warn_check_scripts();
    }

    pub(super) fn error(&mut self, e: ErrorObject) {
        self.ob.send(ServerEvent::error(e));
    }
}
