//! Voice mode (chat-voice §9): the realtime panel that takes the composer's
//! place, bound to the open thread through `/v1/realtime?chat_thread=<id>`
//! (§8).
//!
//! [`Realtime`] is the page's one voice mode (one session per page, §9.6),
//! made by the Chat page ([`provide_realtime`]) and reachable from the
//! composer's voice button and the panel. What one session holds — the
//! socket, the microphone, the player's item, the state machine — is
//! [`live::Live`]; everything the panel shows is a signal here.
//!
//! **Entering** (the voice button, a gesture): any dictation is discarded and
//! any read-aloud stopped (§9.7), the page's playback context is made inside
//! the click, the player is held awake for the session, the socket opens,
//! and once `session.created` came the microphone opens with the window's
//! device and echo mode (§12.1). **Leaving** — Esc, Leave, another thread,
//! the page going — closes the socket, stops every track and lets the player
//! go, on every path; the composer comes back with its text and its
//! dictation mark as they were (they live in the page's signals, never
//! touched here). A session that ends by itself (a takeover, the network,
//! the gateway) releases the microphone at once and keeps the panel with
//! the reason and **Re-enter**: nothing reconnects by itself (§8.6).
//!
//! The keys (§9.2, `keys.rs`): Space held talks in push-to-talk mode and
//! stops the voice in automatic mode, M mutes, Esc leaves — only with the
//! focus on the panel or the page, never in a text field. Right Ctrl's
//! dictation is off while voice mode is on (`PageVoice::voice_mode`, §9.7).

mod bubbles;
mod chips;
mod keys;
mod live;
mod machine;
mod panel;
mod protocol;
mod reload;
mod socket;

use std::rc::Rc;

use leptos::prelude::*;
use serde_json::Value;
use wasm_bindgen::JsCast;

use super::super::chat::{ChatThread, Msg, MsgRow};
use super::devices::{use_voice_devices, VoiceDevices};
use super::page::PageVoice;
use super::spoken::VoiceTiming;
use super::state::{refusal_link, Note, NoteKind};
use super::status::VoiceStatus;
use super::VoiceDraft;
pub(crate) use machine::{Caption, VoiceState};
pub(crate) use panel::RealtimePanel;

/// Where voice mode is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Phase {
    /// Not in voice mode: the composer shows.
    #[default]
    Off,
    /// The playback, the socket and the microphone are being opened.
    Connecting,
    Live,
    /// The session ended by itself: why, and Re-enter is offered. The
    /// microphone is released.
    Ended(String),
}

impl Phase {
    pub(crate) fn key(&self) -> &'static str {
        match self {
            Phase::Off => "off",
            Phase::Connecting => "connecting",
            Phase::Live => "live",
            Phase::Ended(_) => "ended",
        }
    }
}

/// The microphone of the session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Mic {
    #[default]
    Closed,
    Opening,
    Open,
    /// The system mutes the track: it delivers silence.
    SystemMuted,
}

impl Mic {
    pub(crate) fn key(&self) -> &'static str {
        match self {
            Mic::Closed => "closed",
            Mic::Opening => "opening",
            Mic::Open => "open",
            Mic::SystemMuted => "muted",
        }
    }
}

/// Which models answered a stage, when another than the one asked for did
/// (a fallback, §4.4): `lmgw.model.state`'s `fallback`, and the timing's
/// `answered_by`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Served {
    pub asr: Option<String>,
    pub chat: Option<String>,
    pub tts: Option<String>,
}

/// Counters the probes read off the panel (`data-voice-*`). The two that
/// move with every chunk (25 a second) are signals of their own
/// (`Realtime::chunks`, `audio_bytes`), so the rest are not re-read with
/// them (WP11 UI review NIT 9).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Probe {
    /// Samples played to their end, over the session.
    pub played: u64,
    /// The last truncate's `audio_end_ms` (what was heard), what had been
    /// played of that item when it was cut, the server's echo of the cut,
    /// and how many truncates were sent.
    pub truncated_ms: Option<u64>,
    pub cut_played_ms: Option<u64>,
    pub truncated_ack_ms: Option<u64>,
    pub truncates: u64,
    /// Push-to-talk turns committed.
    pub commits: u64,
}

/// What the page passes in (all `Copy`).
#[derive(Clone, Copy)]
pub(crate) struct Parts {
    pub msgs: RwSignal<Vec<Msg>>,
    pub current: RwSignal<Option<ChatThread>>,
    pub owner: StoredValue<WeakOwner>,
    pub next_key: StoredValue<u64>,
    pub scope: crate::scope::Scope,
    /// The thread whose text reply streams (the voice button waits for it).
    pub streaming: RwSignal<Option<i64>>,
    /// The page's model choice: the model chip writes it, and the page saves
    /// it on the thread.
    pub model_sel: RwSignal<String>,
    /// The settings drawer's Voice draft, kept in step with a chip's write.
    pub draft: VoiceDraft,
    /// Re-read the thread list.
    pub refresh: Callback<()>,
    /// Show these rows as the open thread's messages, in place of the
    /// page's (`reload.rs` reads them back when a session ends, and loads
    /// afresh only when it cannot patch them in place).
    pub load: Callback<Vec<MsgRow>>,
    /// Messages for these rows, owned by the page (what `reload.rs` adds).
    pub make: Callback<Vec<MsgRow>, Vec<Msg>>,
}

/// The page's voice mode. `Copy`.
#[derive(Clone, Copy)]
pub(crate) struct Realtime {
    pub phase: RwSignal<Phase>,
    pub state: RwSignal<VoiceState>,
    pub muted: RwSignal<bool>,
    /// Push-to-talk mode (else automatic turn detection).
    pub ptt: RwSignal<bool>,
    /// Push-to-talk is held.
    pub talking: RwSignal<bool>,
    pub caption: RwSignal<Caption>,
    /// The panel's own status notes (§9.7.7): the page's `VoiceStatus`
    /// clears its passing notes on every press and send.
    pub status: VoiceStatus,
    /// The last turn's `lmgw.response.timing`.
    pub timing: RwSignal<Option<VoiceTiming>>,
    /// How the last response's turns reached the chat model, as the session
    /// said it (`lmgw.chat.input`: the path and why the transcript): the
    /// INPUT chip's word over the thread's verdict (voice-audio-input §2.3).
    pub said_input: RwSignal<Option<(String, Option<String>)>>,
    pub served: RwSignal<Served>,
    /// The thread carries lmgw's admin tools (§9.4): the session's word,
    /// refreshed by `lmgw.chat.thread`.
    pub admin_tools: RwSignal<Option<bool>>,
    /// The tool a turn runs now.
    pub tool: RwSignal<Option<String>>,
    /// The larger focus view (the transcript steps aside).
    pub focus: RwSignal<bool>,
    pub mic: RwSignal<Mic>,
    pub probe: RwSignal<Probe>,
    /// `input_audio_buffer.append`s sent.
    pub chunks: RwSignal<u64>,
    /// Bytes of reply audio received.
    pub audio_bytes: RwSignal<u64>,
    /// The playback's output tap and the microphone's tap (§10's inputs).
    pub output: RwSignal<Option<web_sys::AnalyserNode>, LocalStorage>,
    pub input: RwSignal<Option<web_sys::AnalyserNode>, LocalStorage>,
    /// When the session went live (the page's clock, ms).
    pub since: RwSignal<Option<f64>>,
    /// The captions' words, for the probes.
    pub heard: RwSignal<String>,
    pub spoken: RwSignal<String>,
    /// A response is open or plays: a stop has something to stop.
    pub stoppable: RwSignal<bool>,
    /// What the panel's live region says (review m8): the user's words once
    /// final, a cut. The captions line itself is not announced: it changes
    /// with every paced word.
    pub announce: RwSignal<String>,
    /// A session left, whose gateway side has not closed yet: its journal
    /// still drains, and the thread is still bound (Keep waits, review NIT 9).
    pub closing: RwSignal<bool>,
    /// Sessions whose close is awaited (`reload.rs`): `closing` while any.
    pending_closes: StoredValue<u32>,
    /// Sessions entered: a read-back of an older one's thread never replaces
    /// the messages a newer one streams into (`reload.rs`, review m5).
    sessions: StoredValue<u64>,
    pub pv: PageVoice,
    pub dev: VoiceDevices,
    pub parts: Parts,
    live: StoredValue<Option<Rc<live::Live>>, LocalStorage>,
}

/// Made by the Chat page, once, after its turn plumbing.
pub(crate) fn provide_realtime(pv: PageVoice, parts: Parts) -> Realtime {
    let rt = Realtime {
        phase: RwSignal::new(Phase::Off),
        state: RwSignal::new(VoiceState::Idle),
        muted: RwSignal::new(false),
        ptt: RwSignal::new(false),
        talking: RwSignal::new(false),
        caption: RwSignal::new(Caption::default()),
        status: VoiceStatus::new(parts.current),
        timing: RwSignal::new(None),
        said_input: RwSignal::new(None),
        served: RwSignal::new(Served::default()),
        admin_tools: RwSignal::new(None),
        tool: RwSignal::new(None),
        focus: RwSignal::new(false),
        mic: RwSignal::new(Mic::Closed),
        probe: RwSignal::new(Probe::default()),
        chunks: RwSignal::new(0),
        audio_bytes: RwSignal::new(0),
        output: RwSignal::new_local(None),
        input: RwSignal::new_local(None),
        since: RwSignal::new(None),
        heard: RwSignal::new(String::new()),
        spoken: RwSignal::new(String::new()),
        stoppable: RwSignal::new(false),
        announce: RwSignal::new(String::new()),
        closing: RwSignal::new(false),
        pending_closes: StoredValue::new(0),
        sessions: StoredValue::new(0),
        pv,
        dev: use_voice_devices(),
        parts,
        live: StoredValue::new_local(None),
    };
    provide_context(rt);
    install_keys(rt);

    // Another thread: voice mode ends (§9.6).
    let tid = Memo::new(move |_| parts.current.with(|c| c.as_ref().map(|t| t.id)));
    Effect::new(move |prev: Option<Option<i64>>| {
        let id = tid.get();
        if prev.is_some_and(|p| p != id) && rt.on_untracked() {
            rt.leave();
        }
        id
    });
    // A different microphone or echo mode chosen while a session runs: the
    // capture reopens with it, and half duplex follows the mode (§12.1).
    Effect::new(move |prev: Option<(Option<String>, &'static str)>| {
        let now = (
            rt.dev.input.with(|p| p.as_ref().map(|p| p.id.clone())),
            rt.dev.echo.get().key(),
        );
        if prev.is_some_and(|p| p != now) {
            if let Some(l) = rt.live.get_value() {
                l.reopen_capture();
            }
        }
        now
    });
    on_cleanup(move || {
        if let Some(l) = rt.live.try_update_value(Option::take).flatten() {
            l.shutdown(None);
        }
    });
    rt
}

/// The page's voice mode, from the Chat page's context.
pub(crate) fn use_realtime() -> Option<Realtime> {
    use_context::<Realtime>()
}

impl Realtime {
    fn on_untracked(&self) -> bool {
        self.pv.voice_mode.get_untracked()
    }

    /// Why the voice button cannot enter now, in one line (tracked):
    /// `None` when it can (§9.1).
    pub(crate) fn blocked_reason(&self) -> Option<String> {
        if !super::audio::listing::secure() {
            return Some(
                "voice needs https or localhost: this page is not a secure context".into(),
            );
        }
        let current = self.parts.current;
        let streaming = self.parts.streaming.get();
        current.with(|c| {
            let Some(t) = c.as_ref() else {
                return Some("open a conversation first".into());
            };
            if streaming == Some(t.id) {
                return Some("a reply is streaming: wait for it, or stop it".into());
            }
            let Some(r) = t.voice_resolved.as_ref() else {
                return Some("the conversation's voice settings are still loading".into());
            };
            if !r.realtime.ok {
                return Some(
                    r.realtime
                        .reason
                        .clone()
                        .unwrap_or_else(|| "Voice mode is not available in Admin Chat".into()),
                );
            }
            r.problems
                .iter()
                .find(|p| p.stage == "asr" || p.stage == "tts")
                .map(|p| p.message.clone())
        })
    }

    /// Enter voice mode: call it in the click (the playback context is made
    /// inside the gesture).
    pub(crate) fn enter(&self) {
        if self.on_untracked() && !matches!(self.phase.get_untracked(), Phase::Ended(_)) {
            return;
        }
        if let Some(why) = untrack(|| self.blocked_reason()) {
            // Re-enter shows the panel, not the composer and its line
            // (review m7).
            let line = if self.on_untracked() {
                self.status
            } else {
                self.pv.status
            };
            // The problem said, when it is one fixed elsewhere in lmgw (a
            // clip without a transcript): a link there.
            let link = self.parts.current.with_untracked(|c| {
                let r = c.as_ref()?.voice_resolved.as_ref()?;
                r.problems
                    .iter()
                    .find(|p| p.message == why)
                    .and_then(|p| refusal_link(&p.code))
            });
            line.set(
                Note::new("realtime", NoteKind::Error, format!("voice mode: {why}")).linked(link),
            );
            return;
        }
        let Some(t) = self.parts.current.get_untracked() else {
            return;
        };
        // A session that ended keeps nothing open; Re-enter starts afresh.
        if let Some(l) = self.live.try_update_value(Option::take).flatten() {
            l.shutdown(None);
        }
        self.sessions.update_value(|n| *n += 1);
        // Voice mode lives beside the page's dictation and read-aloud
        // (§9.7): neither may keep a microphone or the speakers. A
        // dictation that showed is said on the panel's line, the one that
        // shows now (review m5); an armed Right Ctrl had opened nothing.
        let dictating = self.pv.dictation.state.get_untracked().shown();
        self.pv.dictation.cancel(None);
        self.pv.read_aloud.stop();
        self.pv.status.clear("realtime");
        self.reset();
        if dictating {
            self.status.set(Note::new(
                "dictation",
                NoteKind::Info,
                "dictation discarded: voice mode started",
            ));
        }
        // The composer is about to be hidden: its focus goes with it, and
        // so do its open popovers — the voice menu's would keep a running
        // Test microphone beside the session (review NIT 5).
        if let Some(ta) = self.pv.composer_ta.get_untracked() {
            let _ = ta.blur();
        }
        close_composer_popovers();
        self.pv.voice_mode.set(true);
        self.focus_panel();
        self.phase.set(Phase::Connecting);
        let resolved = t
            .voice_resolved
            .as_ref()
            .map(|r| r.turn_detection.value.clone())
            .unwrap_or_default();
        self.ptt.set(resolved == "push_to_talk");
        self.admin_tools
            .set(t.voice_resolved.as_ref().map(|r| r.realtime.admin_tools));
        // The context is made here, in the gesture.
        let ready = super::audio::player::player();
        let l = live::Live::new(*self, t.id, resolved);
        self.live.set_value(Some(l.clone()));
        leptos::task::spawn_local(async move { l.start(ready).await });
    }

    /// Once the panel is in the page: the focus on it (Space, M and Esc act
    /// there), and its keys said through the live region, which was in the
    /// page before the words came (review m5).
    fn focus_panel(&self) {
        let rt = *self;
        let tries = std::rc::Rc::new(std::cell::Cell::new(0u8));
        fn attempt(rt: Realtime, tries: std::rc::Rc<std::cell::Cell<u8>>) {
            request_animation_frame(move || {
                if rt.pv.voice_mode.try_get_untracked() != Some(true) {
                    return;
                }
                let panel = document()
                    .query_selector("[data-rt-panel]")
                    .ok()
                    .flatten()
                    .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok());
                match panel {
                    Some(p) => {
                        let _ = p.focus();
                        rt.announce.try_set(ENTRY_WORDS.to_string());
                    }
                    None if tries.get() < 3 => {
                        tries.set(tries.get() + 1);
                        attempt(rt, tries);
                    }
                    None => {}
                }
            });
        }
        attempt(rt, tries);
    }

    /// Leave voice mode: the socket closed, every track stopped, the player
    /// let go, the composer back with its focus.
    pub(crate) fn leave(&self) {
        if let Some(l) = self.live.try_update_value(Option::take).flatten() {
            // The journal drains as the session ends: what it wrote last (a
            // turn said just before leaving, a reply cut to what was heard)
            // is read back once, when the gateway's side closed, so the
            // thread shows exactly what was stored (`reload.rs`). The
            // session is this one, whenever its close comes (review m1).
            let ended = self.ended_of(&l);
            l.shutdown(Some(reload::at_close(*self, ended)));
        }
        if self.pv.voice_mode.try_get_untracked() != Some(true) {
            return;
        }
        self.pv.voice_mode.set(false);
        self.reset();
        // The next entry starts in the chat panel, beside the transcript.
        self.focus.set(false);
        self.phase.set(Phase::Off);
        let ta = self.pv.composer_ta;
        request_animation_frame(move || {
            if let Some(el) = ta.try_get_untracked().flatten() {
                let _ = el.focus();
            }
        });
    }

    /// The session ended by itself (`why`): everything is released, the
    /// panel stays with the reason and Re-enter.
    pub(crate) fn ended(&self, why: String) {
        if let Some(l) = self.live.try_update_value(Option::take).flatten() {
            let ended = self.ended_of(&l);
            if l.socket_open() {
                // An end of the page's own (the microphone ended, the page
                // hidden): its close is answered after the drain, as a
                // leave's is.
                l.shutdown(Some(reload::at_close(*self, ended)));
            } else {
                // The gateway closed it: no close of the page's own says
                // when it drained.
                l.shutdown(None);
                reload::read(
                    *self,
                    ended.tid,
                    ended.session,
                    reload::Until::Settled {
                        last_after_ms: ended.wait_ms,
                    },
                );
            }
        }
        self.reset_session();
        self.phase.try_set(Phase::Ended(why));
    }

    /// The session cannot go on in this thread at all (an Admin Chat thread,
    /// one deleted): voice mode ends and the composer says why.
    pub(crate) fn refused(&self, why: String) {
        self.leave();
        self.pv.status.set(Note::new(
            "realtime",
            NoteKind::Error,
            format!("voice mode ended: {why}"),
        ));
    }

    /// What the read-back of session `l` needs, taken as it ends.
    fn ended_of(&self, l: &live::Live) -> reload::Ended {
        reload::Ended {
            tid: l.tid,
            session: self.sessions.get_value(),
            wait_ms: reload::close_wait_ms(l.ping_interval_s()),
        }
    }

    /// Re-enter after a session ended (in the click).
    pub(crate) fn reenter(&self) {
        self.enter();
    }

    pub(crate) fn talk_down(&self) {
        if let Some(l) = self.live.get_value() {
            l.talk_down();
        }
    }

    pub(crate) fn talk_up(&self) {
        if let Some(l) = self.live.get_value() {
            l.talk_up();
        }
    }

    /// Stop the voice: the player flushed, `response.cancel` and a truncate
    /// at what was heard (§9.2).
    pub(crate) fn stop_talking(&self) {
        if let Some(l) = self.live.get_value() {
            l.stop_talking();
        }
    }

    pub(crate) fn toggle_mute(&self) {
        if let Some(l) = self.live.get_value() {
            l.set_muted(!self.muted.get_untracked());
        }
    }

    /// Push-to-talk on or off (the session's turn detection, §9.2).
    pub(crate) fn set_ptt(&self, ptt: bool) {
        if let Some(l) = self.live.get_value() {
            l.set_ptt(ptt);
        }
    }

    /// Can a stop act now (a response is open or plays)? Thinking about a
    /// turn whose response was not created yet has nothing to stop (review
    /// NIT 8). (tracked)
    pub(crate) fn can_stop(&self) -> bool {
        self.stoppable.get()
            && matches!(
                self.state.get(),
                VoiceState::Speaking | VoiceState::Thinking
            )
    }

    fn reset_session(&self) {
        self.talking.try_set(false);
        self.mic.try_set(Mic::Closed);
        self.input.try_set(None);
        self.tool.try_set(None);
        self.state.try_set(VoiceState::Idle);
        self.stoppable.try_set(false);
        self.caption.try_set(Caption::default());
    }

    fn reset(&self) {
        self.reset_session();
        self.muted.try_set(false);
        self.timing.try_set(None);
        self.said_input.try_set(None);
        self.served.try_set(Served::default());
        self.probe.try_set(Probe::default());
        self.chunks.try_set(0);
        self.audio_bytes.try_set(0);
        self.since.try_set(None);
        self.heard.try_set(String::new());
        self.spoken.try_set(String::new());
        self.announce.try_set(String::new());
        self.status.notes().try_set(Vec::new());
    }
}

/// What the live region says when voice mode opens.
const ENTRY_WORDS: &str = "voice mode on: M mutes, Esc leaves; in push-to-talk mode hold Space \
                           while you speak, in automatic mode Space stops the voice";

/// Hide the composer's open popovers (it is about to be hidden): each
/// popover's own `toggle` handler sets its signal back, so what it holds
/// unmounts.
fn close_composer_popovers() {
    let Ok(list) = document().query_selector_all(".composer-area [popover]:popover-open") else {
        return;
    };
    for i in 0..list.length() {
        if let Some(el) = list
            .item(i)
            .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
        {
            let _ = el.hide_popover();
        }
    }
}

/// Where the focus is, for the keys.
fn place() -> keys::Place {
    let Some(el) = document().active_element() else {
        return keys::Place::Page;
    };
    let tag = el.tag_name().to_ascii_lowercase();
    if tag == "body" || tag == "html" {
        return keys::Place::Page;
    }
    // A focused element that is not shown — the composer's text box, hidden
    // while the panel has its place — is no place the keys belong to:
    // WebKitGTK keeps it as the active element after it is hidden (measured
    // by the WebKit probe), where Chrome lets it go.
    let r = el.get_bounding_client_rect();
    if r.width() == 0.0 && r.height() == 0.0 {
        return keys::Place::Page;
    }
    let editable = match tag.as_str() {
        "textarea" | "select" => true,
        "input" => !matches!(
            el.get_attribute("type").unwrap_or_default().as_str(),
            "button" | "checkbox" | "radio" | "range" | "submit" | "reset" | "color" | "file"
        ),
        _ => el
            .dyn_ref::<web_sys::HtmlElement>()
            .is_some_and(|h| h.is_content_editable()),
    };
    if editable {
        keys::Place::Text
    } else if el.closest("[data-rt-panel]").ok().flatten().is_some() {
        keys::Place::Panel
    } else {
        keys::Place::Elsewhere
    }
}

/// A `<dialog>` or a popover of the page is open and shown: Esc is theirs.
/// One inside the hidden composer (its voice menu, left open when voice mode
/// came) is not shown, and is not asked (the WebKit probe found it).
fn dialog_open() -> bool {
    let Ok(list) = document().query_selector_all("dialog[open], [popover]:popover-open") else {
        return false;
    };
    (0..list.length()).any(|i| {
        list.item(i)
            .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|el| {
                let r = el.get_bounding_client_rect();
                r.width() > 0.0 || r.height() > 0.0
            })
    })
}

/// Space, M and Esc while voice mode is on (`keys.rs`).
fn install_keys(rt: Realtime) {
    let down = window_event_listener(leptos::ev::keydown, move |ev| {
        if rt.pv.voice_mode.try_get_untracked() != Some(true) {
            return;
        }
        let act = keys::key_down(&keys::Down {
            key: &ev.key(),
            code: &ev.code(),
            repeat: ev.repeat(),
            composing: ev.is_composing(),
            // AltGr (EurKEY's Right Alt) + Space is a character, not Space
            // (review NIT 1), as Right Ctrl's rule has it.
            modified: ev.ctrl_key()
                || ev.alt_key()
                || ev.meta_key()
                || ev.get_modifier_state("AltGraph"),
            place: place(),
            dialog_open: dialog_open(),
        });
        let live = matches!(rt.phase.get_untracked(), Phase::Live | Phase::Connecting);
        match act {
            keys::Act::Pass => {}
            keys::Act::Swallow => ev.prevent_default(),
            keys::Act::TalkDown => {
                ev.prevent_default();
                if !live {
                    return;
                }
                if rt.ptt.get_untracked() {
                    rt.talk_down();
                } else if rt.state.get_untracked() == VoiceState::Speaking {
                    rt.stop_talking();
                }
            }
            keys::Act::Mute => {
                ev.prevent_default();
                if live {
                    rt.toggle_mute();
                }
            }
            keys::Act::Leave => {
                ev.prevent_default();
                rt.leave();
            }
        }
    });
    let up = window_event_listener(leptos::ev::keyup, move |ev| {
        if keys::key_up_ends_talk(
            &ev.key(),
            &ev.code(),
            rt.talking.try_get_untracked() == Some(true),
        ) {
            ev.prevent_default();
            rt.talk_up();
        }
    });
    // The window losing focus while Space is held: its key-up never comes.
    let blur = window_event_listener(leptos::ev::blur, move |ev| {
        let own = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Window>().ok())
            .is_some();
        if own && rt.talking.try_get_untracked() == Some(true) {
            rt.talk_up();
        }
    });
    on_cleanup(move || {
        down.remove();
        up.remove();
        blur.remove();
    });
}

/// The panel's colours (§10's `inputs.palette`): the `--rt-*` tokens of
/// voice.css, read off the panel, each role as its deep, base and lit shades.
pub(crate) fn palette(el: &web_sys::Element) -> Value {
    let Some(style) = window().get_computed_style(el).ok().flatten() else {
        return Value::Null;
    };
    let get = |k: &str| {
        style
            .get_property_value(k)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let role = |name: &str| {
        let base = get(&format!("--rt-{name}"));
        match (
            get(&format!("--rt-{name}-deep")),
            base,
            get(&format!("--rt-{name}-hi")),
        ) {
            (Some(deep), Some(base), Some(hi)) => {
                serde_json::json!({"deep": deep, "base": base, "hi": hi})
            }
            (_, Some(base), _) => Value::String(base),
            _ => Value::Null,
        }
    };
    let mut p = serde_json::json!({
        "bg": get("--rt-bg"),
        "fg": get("--rt-fg"),
        "accent": role("accent"),
        "accent2": role("think"),
        "warn": role("user"),
        "muted": role("idle"),
    });
    if let Some(o) = p.as_object_mut() {
        o.retain(|_, v| !v.is_null());
    }
    p
}
