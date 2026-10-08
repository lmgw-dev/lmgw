//! One voice-mode session (chat-voice §9, §11): the socket, the microphone
//! at 24 kHz, the player's item per response, the state machine and the
//! captions, and what each server event does to them.
//!
//! **Audio out.** A response takes the page's one player with
//! [`Player::begin_owned`] at its first audio delta (§9.7.2), so another
//! playback taking the speakers is noticed as a stop. `response.output_audio.
//! done` ends the item; `speaking` is left when it has played out (`ended`),
//! never at an underrun (§11.1, review m9). A response that is done without
//! its audio's `done` (cancelled) ends its item at `response.done`.
//!
//! **Cuts.** A barge-in (`speech_started` while the reply plays, §9.2) and
//! stop talking flush the response's item; the flush answers what was
//! *heard* (`played` less the output latency the browser reports), and
//! `conversation.item.truncate {audio_end_ms}` carries exactly that (§11.1:
//! the ledger's count, not pacing's estimate). A barge-in's response the
//! server cancels itself. A stop sends the truncate first: while the item
//! is still being produced, the gateway's truncate cancels the rest of the
//! response in the same step (`lifecycle/truncate.rs`), so the reply's slot
//! gets the exact cut in one write (WP9 review NIT 13). Only a response the
//! truncate does not stop — none of it heard yet, or its audio already
//! complete while it is still open — gets a `response.cancel` after it.
//! Audio of a cut response that is still on its way is dropped here (and by
//! the player's ledger, counted).
//!
//! **Leaving** (Esc, Leave, another thread, the page going) sends the same
//! truncate for the reply still playing before the socket closes (review
//! m9): the gateway reads it before the close, so the thread keeps what was
//! heard, not what was sent.
//!
//! **Audio in.** The capture worklet posts 40 ms PCM16 chunks; each goes as
//! `input_audio_buffer.append`. Push-to-talk keeps the capture gated (a
//! pre-roll of `realtime.prefix_padding_ms`): Space down clears the buffer
//! and opens the gate, Space up closes it — every chunk of the utterance
//! delivered (`close_gate().await`) — then `commit` + `response.create`.
//! Mute disables the track: silence flows, so an open turn ends naturally.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use futures::FutureExt;
use leptos::prelude::*;
use lmgw_client::realtime::{
    self as protocol, ClientEvent, ErrorFacts, ServerEvent, SessionFacts, TurnDetection, Updates,
};
use lmgw_client::requests::realtime_page_url;
use lmgw_client::truncate::{Incoming, PlaybackCursor, Replies};
use serde_json::Value;

use super::super::super::chat::Msg;
use super::super::audio::capture::{self, Capture, CaptureEvent};
use super::super::audio::pcm::RATE;
use super::super::audio::player::{self, Awake, Player};
use super::super::spoken::VoiceTiming;
use super::super::state::{Note, NoteKind, TURN_KEY};
use super::machine::{Captions, Machine};
use super::socket::{self, SockEvent, Socket};
use super::{Mic, Phase, Realtime, Served};

mod errors;

/// How long the first player may take before the session gives up on it
/// (the player's own start wait is 5 s, its first route 6 s).
const PLAYER_WAIT_MS: u64 = 15_000;

/// Why a reply's audio is cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cut {
    /// The server heard the user start talking over it.
    BargeIn,
    /// Stop talking (the button, Space, push-to-talk pressed over it).
    Stop,
    /// Another playback took the speakers (already flushed by its begin).
    Taken,
}

pub(crate) struct Live {
    rt: Realtime,
    pub(crate) tid: i64,
    /// The thread's resolved turn detection (the automatic mode's).
    resolved: String,
    this: RefCell<Weak<Live>>,
    socket: RefCell<Option<Rc<Socket>>>,
    capture: RefCell<Option<Capture>>,
    player: RefCell<Option<Rc<Player>>>,
    awake: RefCell<Option<Awake>>,
    machine: RefCell<Machine>,
    captions: RefCell<Captions>,
    /// Each response's audio: its assistant item, ended, cut
    /// (`lmgw_client::truncate`).
    replies: RefCell<Replies>,
    /// The player's item of each response, from its first audio delta.
    items: RefCell<HashMap<String, u32>>,
    /// The reply bubble of each response (`bubbles.rs`).
    pub(super) bubbles: RefCell<HashMap<String, Msg>>,
    opened: Cell<bool>,
    created: Cell<bool>,
    over: Cell<bool>,
    last_error: RefCell<Option<ErrorFacts>>,
    /// `realtime.prefix_padding_ms`, push-to-talk's pre-roll.
    preroll_ms: Cell<u32>,
    /// `realtime.ping_interval_s`, which bounds the gateway's end (how long
    /// the read-back waits for its close, `reload.rs`); `None`: not read.
    ping_s: Cell<Option<u32>>,
    /// Bumped per capture opened: an open that lands after a newer one (or
    /// the end) is stopped.
    capture_gen: Cell<u64>,
    /// The timer that ends `interrupted`, pending.
    tick_at: Cell<Option<f64>>,
    /// `session.update`s sent and not answered yet: the session's echo is
    /// the session's word on push-to-talk only once none is in flight
    /// (review NIT 4).
    updates: RefCell<Updates>,
}

impl Live {
    pub(crate) fn new(rt: Realtime, tid: i64, resolved: String) -> Rc<Self> {
        let l = Rc::new(Live {
            rt,
            tid,
            resolved,
            this: RefCell::new(Weak::new()),
            socket: RefCell::new(None),
            capture: RefCell::new(None),
            player: RefCell::new(None),
            awake: RefCell::new(None),
            machine: RefCell::new(Machine::default()),
            captions: RefCell::new(Captions::default()),
            replies: RefCell::new(Replies::default()),
            items: RefCell::new(HashMap::new()),
            bubbles: RefCell::new(HashMap::new()),
            opened: Cell::new(false),
            created: Cell::new(false),
            over: Cell::new(false),
            last_error: RefCell::new(None),
            preroll_ms: Cell::new(0),
            ping_s: Cell::new(None),
            capture_gen: Cell::new(0),
            tick_at: Cell::new(None),
            updates: RefCell::new(Updates::default()),
        });
        *l.this.borrow_mut() = Rc::downgrade(&l);
        l
    }

    pub(super) fn rt(&self) -> Realtime {
        self.rt
    }

    /// The playback first (its context was made in the click), then the
    /// socket; the microphone once `session.created` came.
    pub(crate) async fn start(self: Rc<Self>, ready: player::Ready) {
        let timeout = player::sleep(PLAYER_WAIT_MS).fuse();
        let ready = ready.fuse();
        futures::pin_mut!(timeout, ready);
        let got = futures::select! {
            r = ready => r,
            _ = timeout => Err(format!(
                "lmgw's playback did not start within {} s",
                PLAYER_WAIT_MS / 1000
            )),
        };
        if self.over.get() {
            return;
        }
        let p = match got {
            Ok(p) => p,
            Err(e) => return self.end(format!("voice mode needs the speakers: {e}")),
        };
        *self.awake.borrow_mut() = Some(p.hold_awake());
        self.rt.output.try_set(Some(p.analyser()));
        *self.player.borrow_mut() = Some(p);
        // Push-to-talk's pre-roll is the owner's `realtime.prefix_padding_ms`;
        // `ping_interval_s` bounds the gateway's end.
        if let Ok(v) = crate::api::get::<Value>("/api/settings-full").await {
            if let Some(ms) = v["realtime"]["prefix_padding_ms"].as_u64() {
                self.preroll_ms.set(ms as u32);
            }
            if let Some(s) = v["realtime"]["ping_interval_s"].as_u64() {
                self.ping_s.set(Some(s.min(u64::from(u32::MAX)) as u32));
            }
        }
        if self.over.get() {
            return;
        }
        let loc = window().location();
        let url = realtime_page_url(
            &loc.protocol().unwrap_or_default(),
            &loc.host().unwrap_or_default(),
            self.tid,
        );
        let weak = Rc::downgrade(&self);
        match socket::open(&url, move |ev| {
            if let Some(me) = weak.upgrade() {
                me.on_socket(ev);
            }
        }) {
            Ok(s) => *self.socket.borrow_mut() = Some(s),
            Err(e) => self.end(e),
        }
    }

    /// `realtime.ping_interval_s` as read at the start.
    pub(super) fn ping_interval_s(&self) -> Option<u32> {
        self.ping_s.get()
    }

    /// The socket is open (or still opening): a close of the page's own is
    /// still to be answered.
    pub(super) fn socket_open(&self) -> bool {
        self.socket.borrow().as_ref().is_some_and(|s| !s.closed())
    }

    fn send(&self, text: &str) -> bool {
        self.socket.borrow().as_ref().is_some_and(|s| s.send(text))
    }

    fn send_event(&self, ev: ClientEvent) -> bool {
        self.send(&ev.to_json())
    }

    /// What the client owns of the session (§8.1, §12.1): the turn
    /// detection it runs with and half duplex for echo mode `none`.
    fn send_session_update(&self) {
        let sent = self.send_event(ClientEvent::SessionUpdate {
            turn_detection: TurnDetection::requested(
                self.rt.ptt.get_untracked(),
                Some(&self.resolved),
            ),
            half_duplex: self.rt.dev.echo.get_untracked().half_duplex(),
        });
        if sent {
            self.updates.borrow_mut().sent();
        }
    }

    fn on_socket(self: &Rc<Self>, ev: SockEvent) {
        if self.over.get() {
            return;
        }
        match ev {
            SockEvent::Open => {
                self.opened.set(true);
                self.send_session_update();
            }
            SockEvent::Message(text) => {
                if let Some(ev) = protocol::parse(&text) {
                    self.on_event(ev);
                }
            }
            SockEvent::Close { code, reason } => self.on_close(code, &reason),
        }
    }

    fn on_close(self: &Rc<Self>, code: u16, reason: &str) {
        let opened = self.opened.get();
        let words = socket::close_words(code, reason, opened, self.last_error.borrow().as_ref());
        if opened {
            return self.end(words);
        }
        // A refused handshake hides its status: the thread says the likely
        // reason (deleted, or an Admin Chat thread).
        let me = self.clone();
        self.rt.parts.scope.spawn(async move {
            let got = crate::api::get::<Value>(format!("/chat/api/threads/{}", me.tid)).await;
            // Left, or entered again, while the thread was read (review m6).
            if me.over.get() {
                return;
            }
            let why = match got {
                Err(crate::api::Error::Api(e)) if e.code == "not_found" => {
                    Some("this conversation no longer exists".to_string())
                }
                Ok(v) if v["thread"]["kind"] == "admin" => {
                    Some("Voice mode is not available in Admin Chat".to_string())
                }
                _ => None,
            };
            match why {
                Some(w) => me.rt.refused(w),
                None => me.end(words),
            }
        });
    }

    /// The session ended by itself: say why, release everything.
    fn end(&self, why: String) {
        if self.over.get() {
            return;
        }
        self.rt.ended(why);
    }

    /// Release everything, on every path: the socket, every track, the
    /// player's item (flushed), the hold on the player. The reply still
    /// playing is truncated at what was heard first (module doc). `drained`
    /// is called once the gateway answered the close: its session ended and
    /// its journal drained (`socket.rs`).
    pub(crate) fn shutdown(&self, drained: Option<Box<dyn FnOnce()>>) {
        if self.over.replace(true) {
            return;
        }
        self.cut_on_leave();
        self.capture_gen.set(self.capture_gen.get() + 1);
        if let Some(c) = self.capture.borrow_mut().take() {
            c.stop();
        }
        match self.socket.borrow_mut().take() {
            Some(s) => s.close_then(drained),
            None => {
                if let Some(f) = drained {
                    f();
                }
            }
        }
        self.replies.borrow_mut().clear();
        let items: Vec<u32> = self.items.borrow_mut().drain().map(|(_, i)| i).collect();
        if let Some(p) = self.player.borrow_mut().take() {
            for item in items {
                let p2 = p.clone();
                leptos::task::spawn_local(async move {
                    let _ = p2.flush(Some(item)).await;
                    p2.release(item);
                });
            }
        }
        self.awake.borrow_mut().take();
        self.rt.output.try_set(None);
        self.rt.input.try_set(None);
        self.rt.mic.try_set(Mic::Closed);
    }

    // ------------------------------------------------------------- events

    fn on_event(self: &Rc<Self>, ev: ServerEvent) {
        match ev {
            ServerEvent::SessionCreated(f) => self.on_session(&f, false),
            ServerEvent::SessionUpdated(f) => self.on_session(&f, true),
            ServerEvent::SpeechStarted => {
                self.captions.borrow_mut().user_started();
                let playing = self.machine.borrow().playing().map(str::to_string);
                if let Some(rid) = playing {
                    // What `@openai/agents` does: flush, and truncate at what
                    // was heard (§9.2).
                    self.cut(&rid, Cut::BargeIn);
                }
                self.machine.borrow_mut().speech_started();
            }
            ServerEvent::SpeechStopped => self.machine.borrow_mut().speech_stopped(),
            ServerEvent::Committed => self.machine.borrow_mut().committed(),
            // Push-to-talk's own clear (Space down) is followed by its turn.
            ServerEvent::Cleared if !self.rt.talking.get_untracked() => {
                self.machine.borrow_mut().cleared()
            }
            ServerEvent::Cleared => {}
            ServerEvent::TranscriptDelta(d) => self.captions.borrow_mut().user_delta(&d),
            ServerEvent::Transcript(t) => {
                self.captions.borrow_mut().user_final(&t);
                let t = t.trim();
                if !t.is_empty() {
                    self.rt.announce.try_set(format!("You said: {t}"));
                }
            }
            ServerEvent::TranscriptFailed(e) => self.heard_failed(e.code.as_deref(), &e.message),
            ServerEvent::ResponseCreated { response_id: id } => {
                self.last_error.borrow_mut().take();
                self.machine.borrow_mut().response_created(&id);
                self.replies.borrow_mut().created(&id);
                // The last turn's notes, and the chat stage's (a refusal, a
                // `held`): this response says its own.
                self.rt.status.clear(errors::TURN_NOTE);
                self.rt.status.clear(TURN_KEY);
            }
            ServerEvent::AudioDelta {
                response_id,
                item_id,
                pcm,
            } => self.audio(&response_id, &item_id, &pcm),
            ServerEvent::AudioDone { response_id } => {
                self.replies.borrow_mut().audio_done(&response_id);
                self.end_audio(&response_id)
            }
            ServerEvent::SpokenDelta { response_id, delta } => {
                self.captions.borrow_mut().spoken(&response_id, &delta)
            }
            ServerEvent::ResponseDone {
                response_id: id,
                status,
                reason,
            } => {
                self.machine.borrow_mut().response_done(&id);
                // A cancelled response's audio gets no `done` of its own: what
                // is in the player plays out, and then `speaking` ends.
                self.end_audio(&id);
                // A heard push-to-talk turn with no words is vetoed quietly:
                // said as `empty_turn` is (WP3 review #4), which only a
                // push-to-talk turn gets with audio input off. Hands-free,
                // noise stays quiet, as server VAD keeps it out with the
                // setting off. One whose transcription failed said so
                // already (`heard_failed`).
                if status == "cancelled"
                    && reason.as_deref() == Some("no_words")
                    && self.rt.ptt.get_untracked()
                {
                    self.nothing_said();
                }
                if status == "failed" {
                    self.machine.borrow_mut().turn_failed();
                    // A failure says itself in its `error` event; one that
                    // came without it is said here.
                    if self.last_error.borrow().is_none() {
                        if let Some(r) = reason {
                            self.rt.status.set(Note::new(
                                "turn",
                                NoteKind::Warn,
                                format!("the reply failed: {r}"),
                            ));
                        }
                    }
                }
            }
            ServerEvent::Error(e) => self.on_error(e),
            ServerEvent::ChatFrame {
                response_id,
                event,
                data,
            } => super::bubbles::frame(self, &response_id, &event, data),
            ServerEvent::ChatUser {
                message_id,
                content,
                voice,
                response_id,
            } => super::bubbles::user(self, message_id, content, &voice, response_id.as_deref()),
            ServerEvent::ChatInput { input, why, .. } => {
                self.rt.said_input.try_set(Some((input, why)));
            }
            ServerEvent::ChatReply(reply) => super::bubbles::reply(self, &reply),
            ServerEvent::ModelState(m) => self.model_state(&m),
            ServerEvent::Timing(v) => {
                if let Ok(t) = serde_json::from_value::<VoiceTiming>(v) {
                    self.rt.served.try_update(|s| {
                        let ans = |m: &Option<super::super::spoken::Served>| {
                            m.as_ref()
                                .and_then(|m| m.answered_by.clone().filter(|b| *b != m.alias))
                        };
                        *s = Served {
                            asr: ans(&t.models.asr),
                            chat: ans(&t.models.chat),
                            tts: ans(&t.models.tts),
                        };
                    });
                    self.rt.timing.try_set(Some(t));
                }
            }
            ServerEvent::Thread(t) => {
                self.rt.admin_tools.try_set(Some(t.admin_tools));
                let title = t.title.clone();
                let tid = self.tid;
                let mut changed = false;
                self.rt.parts.current.try_update(|c| {
                    if let Some(c) = c.as_mut().filter(|c| c.id == tid && c.title != title) {
                        c.title = title.clone();
                        changed = true;
                    }
                });
                if changed {
                    self.rt.parts.refresh.run(());
                }
            }
            ServerEvent::Truncated { audio_end_ms, .. } => {
                self.rt
                    .probe
                    .try_update(|c| c.truncated_ack_ms = Some(audio_end_ms));
            }
            // Unknown, and whatever a newer crate types.
            _ => {}
        }
        self.refresh();
    }

    /// `session.created` (`echo: false`) or `session.updated`.
    fn on_session(self: &Rc<Self>, f: &SessionFacts, echo: bool) {
        if let Some(t) = &f.thread {
            self.rt.admin_tools.try_set(Some(t.admin_tools));
        }
        if echo {
            self.session_echo(f.turn_detection.is_none());
        }
        if let Some(ms) = f.prefix_padding_ms {
            self.preroll_ms.set(ms);
        }
        if !self.created.replace(true) {
            self.open_capture();
        }
    }

    fn model_state(&self, m: &protocol::ModelState) {
        self.rt.status.state(m);
        let stage = m.stage.as_str();
        if m.state == "fallback" {
            let by = m.answered_by.clone();
            self.rt.served.try_update(|s| match stage {
                "asr" => s.asr = by,
                "chat" => s.chat = by,
                "tts" => s.tts = by,
                _ => {}
            });
        }
    }

    /// Recompute what the panel shows from the facts.
    pub(super) fn refresh(&self) {
        // A session that ended writes nothing into the page's signals: they
        // may be a re-entered session's already (review NIT 3).
        if self.over.get() {
            return;
        }
        let now = js_sys::Date::now();
        let m = self.machine.borrow();
        let state = m.state(now);
        if self.rt.state.try_get_untracked() != Some(state) {
            self.rt.state.try_set(state);
        }
        let stoppable = m.current().is_some();
        if self.rt.stoppable.try_get_untracked() != Some(stoppable) {
            self.rt.stoppable.try_set(stoppable);
        }
        let line = self.captions.borrow().line(
            state,
            self.rt.muted.get_untracked(),
            self.rt.ptt.get_untracked(),
        );
        if self.rt.caption.try_with_untracked(|c| *c != line) == Some(true) {
            self.rt.caption.try_set(line);
        }
        {
            let c = self.captions.borrow();
            if self.rt.heard.try_with_untracked(|h| h != c.user()) == Some(true) {
                self.rt.heard.try_set(c.user().to_string());
            }
            if self.rt.spoken.try_with_untracked(|h| h != c.reply()) == Some(true) {
                self.rt.spoken.try_set(c.reply().to_string());
            }
        }
        // `interrupted` ends by itself: a timer reads the state again then.
        if let Some(at) = m.next_change(now) {
            if self.tick_at.get() != Some(at) {
                self.tick_at.set(Some(at));
                let weak = self.this.borrow().clone();
                set_timeout(
                    move || {
                        if let Some(me) = weak.upgrade() {
                            me.tick_at.set(None);
                            if !me.over.get() {
                                me.refresh();
                            }
                        }
                    },
                    std::time::Duration::from_millis((at - now).max(0.0) as u64 + 5),
                );
            }
        }
    }

    // --------------------------------------------------------- audio out

    fn audio(self: &Rc<Self>, rid: &str, item_id: &str, pcm: &[u8]) {
        let Some(p) = self.player.borrow().clone() else {
            return;
        };
        let incoming = self.replies.borrow_mut().audio(rid, item_id);
        let item = match incoming {
            Incoming::Drop => return,
            Incoming::Continue => match self.items.borrow().get(rid) {
                Some(i) => *i,
                None => return,
            },
            Incoming::Start => {
                let weak = Rc::downgrade(self);
                let id = rid.to_string();
                let i = p.begin_owned(Box::new(move || {
                    if let Some(me) = weak.upgrade() {
                        leptos::task::spawn_local(async move { me.cut(&id, Cut::Taken) });
                    }
                }));
                let weak = Rc::downgrade(self);
                let id = rid.to_string();
                p.watch_starved(
                    i,
                    Rc::new(move |dry| {
                        if let Some(me) = weak.upgrade() {
                            if !me.over.get() {
                                me.machine.borrow_mut().starved(&id, dry);
                                me.refresh();
                            }
                        }
                    }),
                );
                self.items.borrow_mut().insert(rid.to_string(), i);
                self.machine.borrow_mut().audio_started(rid);
                i
            }
        };
        p.push(item, pcm);
        self.rt.audio_bytes.try_update(|c| *c += pcm.len() as u64);
    }

    /// The response's audio is complete: `speaking` ends once it played out.
    fn end_audio(self: &Rc<Self>, rid: &str) {
        let Some(p) = self.player.borrow().clone() else {
            return;
        };
        if !self.replies.borrow_mut().end(rid) {
            return;
        }
        let Some(item) = self.items.borrow().get(rid).copied() else {
            return;
        };
        let me = self.clone();
        let rid = rid.to_string();
        leptos::task::spawn_local(async move {
            let got = p.end(item).await;
            if me.over.get() {
                return;
            }
            if let Some(c) = got {
                me.machine.borrow_mut().played_out(&rid);
                me.rt.probe.try_update(|x| x.played += c.played);
                p.release(item);
                me.refresh();
            }
        });
    }

    /// Cut response `rid`'s audio (module doc).
    fn cut(self: &Rc<Self>, rid: &str, why: Cut) {
        if self.over.get() {
            return;
        }
        if !self.replies.borrow_mut().cut(rid) {
            return;
        }
        let item = self.items.borrow().get(rid).copied();
        // A barge-in's response the server cancels itself.
        let stop = why != Cut::BargeIn;
        let (Some(p), Some(item)) = (self.player.borrow().clone(), item) else {
            // Nothing of it was heard: nothing to truncate, only the cancel.
            if stop && self.machine.borrow().is_open(rid) {
                self.send_event(ClientEvent::ResponseCancel {
                    response_id: Some(rid.to_string()),
                });
            }
            return;
        };
        // Shown at once; the count follows from the worklet.
        let heard_now = p.played(item).unwrap_or(0) > 0;
        self.machine
            .borrow_mut()
            .cut(rid, js_sys::Date::now(), heard_now);
        if heard_now {
            self.rt.announce.try_set("Interrupted".to_string());
        }
        self.refresh();
        if why == Cut::Taken {
            self.rt.status.set(Note::new(
                "turn",
                NoteKind::Info,
                "the voice stopped: another playback took the speakers",
            ));
        }
        let me = self.clone();
        let rid = rid.to_string();
        leptos::task::spawn_local(async move {
            let (heard, played) = if why == Cut::Taken {
                (p.heard(item), p.played(item))
            } else {
                match p
                    .flush(Some(item))
                    .await
                    .into_iter()
                    .find(|c| c.item == item)
                {
                    Some(c) => (Some(c.heard), Some(c.played)),
                    None => (p.heard(item), p.played(item)),
                }
            };
            p.release(item);
            if me.over.get() {
                return;
            }
            let truncated = me.truncate(&rid, heard, played);
            if stop {
                me.cancel_unless_truncated(&rid, truncated);
            }
        });
    }

    /// Truncate response `rid`'s item at `heard` samples (`played`, for the
    /// probes); whether it was sent.
    fn truncate(&self, rid: &str, heard: Option<u64>, played: Option<u64>) -> bool {
        let cursor = |samples| PlaybackCursor {
            heard_samples: samples,
            sample_rate: RATE,
        };
        let Some(ev) = self.replies.borrow().truncate(rid, heard.map(cursor)) else {
            return false;
        };
        let ClientEvent::Truncate { audio_end_ms, .. } = ev else {
            return false;
        };
        let sent = self.send_event(ev);
        if sent {
            self.rt.probe.try_update(|c| {
                c.truncated_ms = Some(audio_end_ms);
                c.cut_played_ms = played.map(|p| cursor(p).audio_end_ms());
                c.truncates += 1;
            });
        }
        sent
    }

    /// After a stop's truncate (module doc): `response.cancel` for a
    /// response still open that the truncate did not stop — nothing of it
    /// was truncated, or its item was complete already.
    fn cancel_unless_truncated(&self, rid: &str, truncated: bool) {
        let open = self.machine.borrow().is_open(rid);
        if self
            .replies
            .borrow()
            .cancel_after_truncate(rid, open, truncated)
        {
            self.send_event(ClientEvent::ResponseCancel {
                response_id: Some(rid.to_string()),
            });
        }
    }

    /// Leaving while the voice speaks (module doc): the reply still playing
    /// is truncated at what was heard — `heard()` is at most one progress
    /// report (50 ms) old — and cancelled when the truncate does not stop
    /// it, before the socket closes.
    fn cut_on_leave(&self) {
        let Some(rid) = self.machine.borrow().playing().map(str::to_string) else {
            return;
        };
        let (Some(p), Some(item)) = (
            self.player.borrow().clone(),
            self.replies
                .borrow()
                .get(&rid)
                .filter(|r| !r.cut)
                .and_then(|_| self.items.borrow().get(&rid).copied()),
        ) else {
            return;
        };
        let truncated = self.truncate(&rid, p.heard(item), p.played(item));
        self.cancel_unless_truncated(&rid, truncated);
    }

    /// The answer to a `session.update` (review NIT 4): once none is in
    /// flight, the session's word on push-to-talk, and the capture's gate
    /// follows it. `session.created` comes before the page's first update
    /// and says nothing the page did not choose.
    fn session_echo(&self, ptt: bool) {
        let settled = self.updates.borrow_mut().echo();
        if !settled || self.rt.ptt.get_untracked() == ptt {
            return;
        }
        self.rt.ptt.try_set(ptt);
        if let Some(c) = self.capture.borrow().as_ref() {
            if !ptt {
                c.open_gate();
            } else if !self.rt.talking.get_untracked() {
                leptos::task::spawn_local(c.close_gate());
            }
        }
    }

    /// The tool a turn runs now (`None`: its result came, or the turn
    /// ended): the status line says it, and the state reads it (§8.5).
    pub(super) fn tool(&self, name: Option<String>) {
        self.machine.borrow_mut().set_tool(name.is_some());
        self.rt.tool.try_set(name);
    }

    pub(crate) fn stop_talking(self: &Rc<Self>) {
        let rid = self.machine.borrow().current().map(str::to_string);
        if let Some(rid) = rid {
            self.cut(&rid, Cut::Stop);
            self.refresh();
        }
    }

    // ---------------------------------------------------------- audio in

    fn open_capture(self: &Rc<Self>) {
        let gen = self.capture_gen.get() + 1;
        self.capture_gen.set(gen);
        if let Some(c) = self.capture.borrow_mut().take() {
            c.stop();
        }
        self.rt.mic.try_set(Mic::Opening);
        self.rt.input.try_set(None);
        let mut opts = capture::Options::new(
            RATE,
            self.rt.dev.echo.get_untracked(),
            self.rt.dev.input.get_untracked(),
        );
        opts.preroll_ms = self.preroll_ms.get();
        opts.gated = self.rt.ptt.get_untracked() && !self.rt.talking.get_untracked();
        let weak = Rc::downgrade(self);
        let on_chunk = Box::new(move |samples: Vec<i16>| {
            if let Some(me) = weak.upgrade() {
                if !me.over.get() && me.send_event(ClientEvent::append_pcm16(&samples)) {
                    me.rt.chunks.try_update(|c| *c += 1);
                }
            }
        });
        let weak = Rc::downgrade(self);
        let on_event = Box::new(move |ev: CaptureEvent| {
            let Some(me) = weak.upgrade() else { return };
            match ev {
                CaptureEvent::Muted => {
                    me.rt.mic.try_set(Mic::SystemMuted);
                    me.rt.status.set(Note::new(
                        "mic",
                        NoteKind::Warn,
                        "the system muted the microphone: it delivers silence",
                    ));
                }
                CaptureEvent::Unmuted => {
                    me.rt.mic.try_set(Mic::Open);
                    me.rt.status.clear("mic");
                }
                CaptureEvent::Ended(why) => {
                    me.end(format!("the microphone ended: {}", why.message()));
                }
            }
        });
        let me = self.clone();
        leptos::task::spawn_local(async move {
            let got = capture::open(opts, on_chunk, on_event).await;
            if me.over.get() || me.capture_gen.get() != gen {
                if let Ok(c) = got {
                    c.stop();
                }
                return;
            }
            match got {
                Ok(c) => {
                    if let Some(n) = c.info().note.clone() {
                        me.rt.status.set(Note::new("mic", NoteKind::Warn, n));
                    }
                    c.set_muted(me.rt.muted.get_untracked());
                    // Space went down while the microphone opened.
                    if me.rt.talking.get_untracked() {
                        c.open_gate();
                    }
                    me.rt.input.try_set(Some(c.analyser()));
                    me.rt.mic.try_set(if c.muted_by_system() {
                        Mic::SystemMuted
                    } else {
                        Mic::Open
                    });
                    *me.capture.borrow_mut() = Some(c);
                    if me.rt.phase.try_get_untracked() == Some(Phase::Connecting) {
                        me.rt.phase.try_set(Phase::Live);
                        me.rt.since.try_set(Some(js_sys::Date::now()));
                    }
                    me.refresh();
                }
                Err(e) => me.end(format!("the microphone could not be opened: {e}")),
            }
        });
    }

    /// The window's microphone or echo mode changed: reopen with it.
    pub(crate) fn reopen_capture(self: &Rc<Self>) {
        if self.over.get() || !self.created.get() {
            return;
        }
        self.send_session_update();
        self.open_capture();
    }

    pub(crate) fn talk_down(self: &Rc<Self>) {
        if !self.rt.ptt.get_untracked() || self.rt.talking.get_untracked() {
            return;
        }
        let playing = self.machine.borrow().current().map(str::to_string);
        if let Some(rid) = playing {
            // Talking over the voice stops it first (§9.2).
            self.cut(&rid, Cut::Stop);
        }
        self.send_event(ClientEvent::Clear);
        if let Some(c) = self.capture.borrow().as_ref() {
            c.open_gate();
        }
        self.rt.talking.try_set(true);
        self.captions.borrow_mut().user_started();
        self.machine.borrow_mut().ptt_down();
        self.refresh();
    }

    pub(crate) fn talk_up(self: &Rc<Self>) {
        if !self.rt.talking.get_untracked() {
            return;
        }
        self.rt.talking.try_set(false);
        let me = self.clone();
        leptos::task::spawn_local(async move {
            // Every chunk of the utterance, its last partial one included,
            // went out before the commit.
            let closed = me.capture.borrow().as_ref().map(Capture::close_gate);
            let Some(f) = closed else {
                // Let go before the microphone opened: nothing was said,
                // and an empty commit would only be refused (review NIT 5).
                me.machine.borrow_mut().ptt_up(false);
                me.refresh();
                return;
            };
            f.await;
            if me.over.get() {
                return;
            }
            let asked =
                me.send_event(ClientEvent::Commit) && me.send_event(ClientEvent::ResponseCreate);
            if asked {
                me.rt.probe.try_update(|c| c.commits += 1);
            }
            me.machine.borrow_mut().ptt_up(asked);
            me.refresh();
        });
    }

    pub(crate) fn set_ptt(self: &Rc<Self>, ptt: bool) {
        if self.rt.ptt.get_untracked() == ptt {
            return;
        }
        if self.rt.talking.get_untracked() {
            self.talk_up();
        }
        self.rt.ptt.try_set(ptt);
        self.send_session_update();
        if ptt {
            // The open turn ends with no commit (the server says
            // `speech_stopped`, then `cleared` for the page's clear).
            self.machine.borrow_mut().turn_failed();
        }
        if let Some(c) = self.capture.borrow().as_ref() {
            if ptt {
                // Gated: nothing flows until Space; what the server holds
                // of an unfinished turn goes.
                let f = c.close_gate();
                let me = self.clone();
                leptos::task::spawn_local(async move {
                    f.await;
                    me.send_event(ClientEvent::Clear);
                });
            } else {
                c.open_gate();
            }
        }
        self.refresh();
    }

    pub(crate) fn set_muted(&self, muted: bool) {
        self.rt.muted.try_set(muted);
        if let Some(c) = self.capture.borrow().as_ref() {
            c.set_muted(muted);
        }
        self.refresh();
    }
}
