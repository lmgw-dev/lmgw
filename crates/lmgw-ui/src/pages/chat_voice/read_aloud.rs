//! Read-aloud in the page (chat-voice design §6.5, §11.1): the page's one
//! speech stream, played through the page's one player.
//!
//! A **stored reply** is read by its speaker button: `POST …/messages/{mid}/
//! speak`, an SSE of `state`, `voice`, `speech {seq, text, pcm}` and
//! `speech_done` (or `speech_error`). A **streaming reply** is read when the
//! thread's resolved `read_aloud` is on: the turn is sent with `speak: true`
//! and its own stream carries the speech frames beside the text
//! (`chat_turn` hands them here, [`LiveSpeech`]).
//!
//! The obligations of §6.5 (WP4 review m9):
//! - **A speech stream is read to its end** — `speech_done`, `speech_error`
//!   or EOF — never stopped at the text's `done`: aborting the fetch there
//!   would be the page going away, which stops the speech. The turn settles
//!   at `done` (the composer takes a new send) while its stream is still
//!   read for the speech.
//! - **One playback per page, so one speech stream per page.** Starting a
//!   playback — a speaker button, or a turn sent with `speak` — ends the one
//!   before: its fetch is aborted (or, for a reply whose text still streams,
//!   its speech is stopped with `speech/stop` so the text goes on), and its
//!   audio is flushed: [`Player::begin`] is the only way to an item.
//! - **Stop** on the speaker button, or "Stop speaking" for a streaming
//!   reply, ends it the same way. The page's Stop aborts the turn's fetch,
//!   text and speech.
//!
//! Audio that comes before the player is ready waits here and is pushed once
//! it is; the player is held awake for as long as a read-aloud lasts, so a
//! long pause before the first clause (a prefill, a tool, a TTS loading)
//! never suspends it in between (WP6 review m8).
//!
//! **Something else taking the player** (the devices popover's test tone, a
//! realtime response) ends the read-aloud as its Stop would: the player
//! tells it ([`Player::begin_owned`]), its fetch is aborted (or its speech
//! stopped) and its buttons go back to idle, with a note (WP7 review m7).

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use futures::future::{FutureExt, LocalBoxFuture};
use leptos::prelude::*;
use serde_json::{json, Value};

use super::super::audio_stream::b64_decode;
use super::super::chat::ChatThread;
use super::super::chat_stream::{send_stream, ChatEvent};
use super::audio::player::{self, Awake, Player};
use super::spoken::ms_text;
use super::state::{block_refusal, refusal_link, speech_lead, Note, NoteKind};
use super::status::VoiceStatus;

/// What a playback is doing, for its button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlayState {
    /// Asked for, no audio yet (the TTS may be loading).
    Loading,
    Playing,
}

/// The playback the buttons show.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Now {
    /// The message's key in the page's list.
    pub key: u64,
    pub tid: i64,
    /// A reply read as it streams.
    pub live: bool,
    pub state: PlayState,
    /// The reply's text is complete (always, for a stored one).
    pub text_done: bool,
}

/// What the last read-aloud of a message said about itself: its route, its
/// first audio, and the player's counts (the probes read them).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Facts {
    pub tts: Option<String>,
    pub voice: Option<String>,
    pub answered_by: Option<String>,
    pub first_audio_ms: Option<u64>,
    pub audio_ms: Option<u64>,
    /// Samples pushed to the player.
    pub pushed: u64,
    pub played: Option<u64>,
    /// The first audio came while the reply's text still streamed.
    pub before_done: Option<bool>,
    pub error: Option<String>,
}

#[derive(Default)]
struct Sink {
    player: Option<Rc<Player>>,
    item: Option<u32>,
    /// Audio that came before the player was ready.
    waiting: Vec<Vec<u8>>,
    /// The speech ended before the player was ready: end the item when it is.
    end_asked: bool,
    awake: Option<Awake>,
}

/// One read-aloud.
pub(crate) struct Playback {
    key: u64,
    tid: i64,
    live: bool,
    ctrl: Option<web_sys::AbortController>,
    text_done: Cell<bool>,
    /// Ended by the page: no audio is taken any more.
    stopped: Cell<bool>,
    /// Over: played out, stopped, or failed.
    over: Cell<bool>,
    /// `speech_done` or `speech_error` came.
    ended: Cell<bool>,
    any_speech: Cell<bool>,
    sink: RefCell<Sink>,
}

impl Playback {
    fn aborted(&self) -> bool {
        self.ctrl.as_ref().is_some_and(|c| c.signal().aborted())
    }
}

/// The page's read-aloud. `Copy`; made by the Chat page.
#[derive(Clone, Copy)]
pub(crate) struct ReadAloud {
    now: RwSignal<Option<Now>>,
    facts: RwSignal<Option<(u64, Facts)>>,
    cur: StoredValue<Option<Rc<Playback>>, LocalStorage>,
    status: VoiceStatus,
    current: RwSignal<Option<ChatThread>>,
}

/// The speech of one streaming turn, fed by `chat_turn`.
pub(crate) struct LiveSpeech {
    ra: ReadAloud,
    pb: Rc<Playback>,
}

impl LiveSpeech {
    /// A voice frame of the turn's stream.
    pub(crate) fn frame(&self, name: &str, data: &Value) {
        self.ra.frame(&self.pb, name, data);
    }

    /// The text's `done`: the reply is complete; the speech may go on.
    pub(crate) fn text_done(&self) {
        self.pb.text_done.set(true);
        self.ra.now.try_update(|n| {
            if let Some(n) = n.as_mut().filter(|n| n.key == self.pb.key) {
                n.text_done = true;
            }
        });
    }

    /// The stream ended.
    pub(crate) fn finish(&self) {
        self.ra.finish(&self.pb);
    }
}

impl ReadAloud {
    pub(crate) fn new(status: VoiceStatus, current: RwSignal<Option<ChatThread>>) -> Self {
        Self {
            now: RwSignal::new(None),
            facts: RwSignal::new(None),
            cur: StoredValue::new_local(None),
            status,
            current,
        }
    }

    /// The playback of message `key`, if it is the one playing (tracked).
    pub(crate) fn state_of(&self, key: u64) -> Option<PlayState> {
        self.now
            .with(|n| n.as_ref().filter(|n| n.key == key).map(|n| n.state))
    }

    /// The last read-aloud's facts of message `key` (tracked).
    pub(crate) fn facts_of(&self, key: u64) -> Option<Facts> {
        self.facts.with(|f| {
            f.as_ref()
                .filter(|(k, _)| *k == key)
                .map(|(_, f)| f.clone())
        })
    }

    /// A streaming reply is being read (tracked).
    pub(crate) fn live_playing(&self) -> bool {
        self.now.with(|n| n.as_ref().is_some_and(|n| n.live))
    }

    /// For the probes: `idle`, `loading` or `playing` (tracked).
    pub(crate) fn state_key(&self) -> &'static str {
        self.now.with(|n| match n.as_ref().map(|n| n.state) {
            None => "idle",
            Some(PlayState::Loading) => "loading",
            Some(PlayState::Playing) => "playing",
        })
    }

    /// For the probes: whether the last read-aloud's first audio came
    /// before the text's `done` (tracked).
    pub(crate) fn first_key(&self) -> &'static str {
        self.facts
            .with(|f| match f.as_ref().and_then(|(_, f)| f.before_done) {
                Some(true) => "before-done",
                Some(false) => "after-done",
                None => "",
            })
    }

    /// Stop whatever reads aloud now.
    pub(crate) fn stop(&self) {
        let Some(pb) = self.cur.try_get_value().flatten() else {
            return;
        };
        if let Some(f) = self.halt(&pb) {
            leptos::task::spawn_local(f);
        }
    }

    /// Stop a read-aloud of a thread other than `tid` (the owner left it).
    pub(crate) fn stop_unless(&self, tid: Option<i64>) {
        let other = self
            .cur
            .try_with_value(|c| c.as_ref().is_some_and(|p| Some(p.tid) != tid))
            .unwrap_or(false);
        if other {
            self.stop();
        }
    }

    /// Read the stored reply `mid` (message `key`) of thread `tid`. Called
    /// in the click: the player's context is made inside the gesture.
    pub(crate) fn play_stored(&self, key: u64, tid: i64, mid: i64) {
        self.status.clear_info();
        self.status.clear("read-aloud");
        // A reply whose text still streams has its speech stopped first: the
        // stop ends every read-aloud of its thread, this one's included if it
        // came before.
        let before = self.cur.get_value().and_then(|old| self.halt(&old));
        let ctrl = web_sys::AbortController::new().ok();
        let pb = self.begin(key, tid, false, ctrl.clone());
        let me = *self;
        leptos::task::spawn_local(async move {
            if let Some(f) = before {
                f.await;
            }
            if pb.stopped.get() {
                return;
            }
            let Some(signal) = ctrl.as_ref().map(|c| c.signal()) else {
                return;
            };
            let url = format!("/chat/api/threads/{tid}/messages/{mid}/speak");
            let feed = pb.clone();
            let res = send_stream(&url, &json!({}), &signal, move |ev| {
                if let ChatEvent::Voice(name, data) = ev {
                    me.frame(&feed, name, &data);
                }
            })
            .await;
            if let Err(e) = res {
                if !pb.aborted() {
                    me.failed(&pb, format!("read-aloud failed: {e}"), false);
                }
            }
            me.finish(&pb);
        });
    }

    /// A turn of thread `tid` streams into message `key`: read it aloud when
    /// the thread's resolved `read_aloud` is on, marking the request (§6.4).
    pub(crate) fn for_turn(
        &self,
        tid: i64,
        key: u64,
        body: &mut Value,
        ctrl: Option<&web_sys::AbortController>,
    ) -> Option<LiveSpeech> {
        let on = self
            .current
            .with_untracked(|c| {
                c.as_ref()
                    .filter(|t| t.id == tid)
                    .and_then(|t| t.voice_resolved.as_ref())
                    .map(|r| r.read_aloud.value)
            })
            .unwrap_or(false);
        if !on || !body.is_object() {
            return None;
        }
        body["speak"] = json!(true);
        self.status.clear("read-aloud");
        // A new turn read aloud ends the playback before it (§6.5).
        if let Some(f) = self.cur.get_value().and_then(|old| self.halt(&old)) {
            leptos::task::spawn_local(f);
        }
        let pb = self.begin(key, tid, true, ctrl.cloned());
        Some(LiveSpeech { ra: *self, pb })
    }

    /// Make `pb` the page's playback and get the player for it.
    fn begin(
        &self,
        key: u64,
        tid: i64,
        live: bool,
        ctrl: Option<web_sys::AbortController>,
    ) -> Rc<Playback> {
        let pb = Rc::new(Playback {
            key,
            tid,
            live,
            ctrl,
            text_done: Cell::new(!live),
            stopped: Cell::new(false),
            over: Cell::new(false),
            ended: Cell::new(false),
            any_speech: Cell::new(false),
            sink: RefCell::new(Sink::default()),
        });
        self.cur.set_value(Some(pb.clone()));
        self.now.set(Some(Now {
            key,
            tid,
            live,
            state: PlayState::Loading,
            text_done: !live,
        }));
        self.facts.set(Some((key, Facts::default())));
        // Made now: in a click or a send's key press, inside its gesture.
        let ready = player::player();
        let me = *self;
        let attach = pb.clone();
        leptos::task::spawn_local(async move {
            match ready.await {
                Ok(p) => me.attach(&attach, p),
                Err(e) => me.failed(&attach, format!("read-aloud: {e}"), true),
            }
        });
        pb
    }

    fn is_cur(&self, pb: &Rc<Playback>) -> bool {
        self.cur
            .try_with_value(|c| c.as_ref().is_some_and(|c| Rc::ptr_eq(c, pb)))
            .unwrap_or(false)
    }

    /// The player is ready: an item of its own (whatever played is flushed),
    /// awake for as long as this lasts, and the audio that waited.
    fn attach(&self, pb: &Rc<Playback>, p: Rc<Player>) {
        if pb.stopped.get() || pb.over.get() {
            return;
        }
        let me = *self;
        let weak: Weak<Playback> = Rc::downgrade(pb);
        let item = p.begin_owned(Box::new(move || {
            if let Some(pb) = weak.upgrade() {
                me.superseded(&pb);
            }
        }));
        let end_asked = {
            let mut s = pb.sink.borrow_mut();
            s.awake = Some(p.hold_awake());
            for b in s.waiting.drain(..) {
                p.push(item, &b);
            }
            s.player = Some(p);
            s.item = Some(item);
            s.end_asked
        };
        if end_asked {
            self.end_item(pb);
        }
    }

    /// Another playback took the player from `pb` (its audio is flushed
    /// already): stop fetching what nobody will hear, and say so.
    fn superseded(&self, pb: &Rc<Playback>) {
        if pb.stopped.get() || pb.over.get() {
            return;
        }
        if let Some(f) = self.halt(pb) {
            leptos::task::spawn_local(f);
        }
        self.status.set(Note::new(
            "read-aloud",
            NoteKind::Info,
            "read-aloud stopped: another playback took the speakers",
        ));
    }

    /// One voice frame of `pb`'s stream.
    fn frame(&self, pb: &Rc<Playback>, name: &str, data: &Value) {
        if name == "state" {
            // A turn's own model (`chat`) is said whatever the speech does.
            let tts = data["stage"].as_str() == Some("tts");
            if !(tts && pb.stopped.get()) {
                self.status.state_frame(data);
            }
            return;
        }
        if pb.stopped.get() || pb.over.get() {
            return;
        }
        let key = pb.key;
        match name {
            "voice" => {
                let tts = data["tts"].as_str().map(str::to_string);
                let by = data["tts_answered_by"].as_str().map(str::to_string);
                if let (Some(t), Some(b)) = (&tts, &by) {
                    if t != b {
                        self.status.set(Note::new(
                            "tts",
                            NoteKind::Warn,
                            format!("read-aloud: {b} speaks in place of {t}"),
                        ));
                    }
                }
                self.facts_update(key, |f| {
                    f.tts = tts;
                    f.voice = data["voice"].as_str().map(str::to_string);
                    f.answered_by = by;
                });
            }
            "speech" => {
                let Some(pcm) = data["pcm"].as_str().and_then(b64_decode) else {
                    return;
                };
                if !pb.any_speech.replace(true) {
                    let before = !pb.text_done.get();
                    self.facts_update(key, |f| f.before_done = Some(before));
                    self.now.try_update(|n| {
                        if let Some(n) = n.as_mut().filter(|n| n.key == key) {
                            n.state = PlayState::Playing;
                        }
                    });
                }
                let samples = (pcm.len() / 2) as u64;
                self.facts_update(key, |f| f.pushed += samples);
                let mut s = pb.sink.borrow_mut();
                match (&s.player, s.item) {
                    (Some(p), Some(item)) => p.push(item, &pcm),
                    _ => s.waiting.push(pcm),
                }
            }
            "speech_done" => {
                pb.ended.set(true);
                self.facts_update(key, |f| {
                    f.first_audio_ms = data["first_audio_ms"].as_u64();
                    f.audio_ms = data["audio_ms"].as_u64();
                    if f.tts.is_none() {
                        f.tts = data["tts"].as_str().map(str::to_string);
                    }
                });
                self.end_item(pb);
            }
            "speech_error" => {
                pb.ended.set(true);
                let code = data["code"].as_str().unwrap_or_default();
                let message = data["message"].as_str().unwrap_or("the voice failed");
                let (kind, text) = match (
                    block_refusal(code, "read-aloud", message),
                    speech_lead(code),
                ) {
                    (Some(t), _) => (NoteKind::Hold, t),
                    // Fixed elsewhere in lmgw: said so, with a link there.
                    (None, Some(lead)) => {
                        (NoteKind::Error, format!("read-aloud: {lead} — {message}"))
                    }
                    (None, None) => (NoteKind::Error, format!("read-aloud: {message}")),
                };
                self.status
                    .set(Note::new("read-aloud", kind, text.clone()).linked(refusal_link(code)));
                self.facts_update(key, |f| f.error = Some(text));
                // What was said before it still plays.
                self.end_item(pb);
            }
            _ => {}
        }
    }

    fn facts_update(&self, key: u64, f: impl FnOnce(&mut Facts)) {
        self.facts.try_update(|cur| match cur {
            Some((k, facts)) if *k == key => f(facts),
            _ => {
                let mut facts = Facts::default();
                f(&mut facts);
                *cur = Some((key, facts));
            }
        });
    }

    /// The speech is complete: play out what was queued, then it is over.
    fn end_item(&self, pb: &Rc<Playback>) {
        if pb.over.get() || pb.stopped.get() {
            return;
        }
        let (p, item) = {
            let mut s = pb.sink.borrow_mut();
            s.end_asked = true;
            match (&s.player, s.item) {
                (Some(p), Some(item)) => (p.clone(), item),
                // The player is not ready yet: `attach` ends it.
                _ => return,
            }
        };
        let me = *self;
        let pb = pb.clone();
        leptos::task::spawn_local(async move {
            let count = p.end(item).await;
            p.release(item);
            if let Some(c) = count {
                me.facts_update(pb.key, |f| f.played = Some(c.played));
            }
            me.over(&pb);
        });
    }

    /// The stream ended. A speech that never said its end is played out,
    /// unless the fetch was aborted — then it is flushed.
    fn finish(&self, pb: &Rc<Playback>) {
        if pb.ended.get() || pb.over.get() || pb.stopped.get() {
            return;
        }
        if pb.aborted() {
            if let Some(f) = self.halt(pb) {
                leptos::task::spawn_local(f);
            }
        } else {
            self.end_item(pb);
        }
    }

    /// End `pb` now: no more audio, what is queued flushed, its fetch
    /// aborted — or, for a reply whose text still streams, its speech
    /// stopped (the future to await: `speech/stop`), so the text goes on.
    fn halt(&self, pb: &Rc<Playback>) -> Option<LocalBoxFuture<'static, ()>> {
        if pb.stopped.replace(true) || pb.over.get() {
            return None;
        }
        let (p, item, awake) = {
            let mut s = pb.sink.borrow_mut();
            (s.player.clone(), s.item, s.awake.take())
        };
        if let (Some(p), Some(item)) = (p, item) {
            leptos::task::spawn_local(async move {
                p.flush(Some(item)).await;
                p.release(item);
                drop(awake);
            });
        }
        let stop = if pb.live && !pb.text_done.get() {
            let tid = pb.tid;
            Some(
                async move {
                    if let Err(e) = crate::api::post::<Value, _>(
                        format!("/chat/api/threads/{tid}/speech/stop"),
                        &json!({}),
                    )
                    .await
                    {
                        leptos::logging::warn!("read-aloud: speech/stop failed: {e}");
                    }
                }
                .boxed_local(),
            )
        } else {
            if let Some(c) = &pb.ctrl {
                c.abort();
            }
            None
        };
        self.over(pb);
        stop
    }

    fn failed(&self, pb: &Rc<Playback>, text: String, halt: bool) {
        self.status
            .set(Note::new("read-aloud", NoteKind::Error, text.clone()));
        self.facts_update(pb.key, |f| f.error = Some(text));
        if halt {
            if let Some(f) = self.halt(pb) {
                leptos::task::spawn_local(f);
            }
        }
    }

    fn over(&self, pb: &Rc<Playback>) {
        if pb.over.replace(true) {
            return;
        }
        pb.sink.borrow_mut().awake.take();
        if self.is_cur(pb) {
            self.cur.try_set_value(None);
            self.now.try_set(None);
        }
    }
}

/// The speaker button's title, from the thread's TTS and the last read.
pub(crate) fn speaker_title(
    state: Option<PlayState>,
    tts: Option<&str>,
    facts: Option<&Facts>,
    extra: &[String],
) -> String {
    let mut t = match state {
        Some(PlayState::Loading) => format!(
            "Starting to read aloud{} — click to stop",
            tts.map(|a| format!(" with {a}")).unwrap_or_default()
        ),
        Some(PlayState::Playing) => "Stop reading aloud".to_string(),
        None => format!(
            "Read this reply aloud{}",
            tts.map(|a| format!(" with {a}")).unwrap_or_default()
        ),
    };
    for e in extra {
        t.push_str(&format!(" — {e}"));
    }
    if let Some(f) = facts {
        if let Some(ms) = f.first_audio_ms {
            t.push_str(&format!(" · last read: first audio after {}", ms_text(ms)));
        }
        if let (Some(t2), Some(b)) = (&f.tts, &f.answered_by) {
            if t2 != b {
                t.push_str(&format!(", said by {b} in place of {t2}"));
            }
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_speaker_title_says_what_a_click_does_and_how_the_last_read_went() {
        assert_eq!(
            speaker_title(None, Some("supertonic"), None, &[]),
            "Read this reply aloud with supertonic"
        );
        assert_eq!(
            speaker_title(Some(PlayState::Playing), None, None, &[]),
            "Stop reading aloud"
        );
        let f = Facts {
            tts: Some("qwen3-tts".into()),
            answered_by: Some("openai/gpt-4o-mini-tts".into()),
            first_audio_ms: Some(412),
            ..Default::default()
        };
        assert_eq!(
            speaker_title(
                None,
                Some("qwen3-tts"),
                Some(&f),
                &["under the GPU hold this goes to openai/gpt-4o-mini-tts (remote)".into()]
            ),
            "Read this reply aloud with qwen3-tts — under the GPU hold this goes to \
             openai/gpt-4o-mini-tts (remote) · last read: first audio after 412 ms, said by \
             openai/gpt-4o-mini-tts in place of qwen3-tts"
        );
    }
}
