//! The Chat page's voice (chat-voice design §5, §6.5): its dictation, its one
//! read-aloud and the composer's status notes, made once by the page and
//! reachable from its components ([`use_page_voice`]) and from every turn
//! (`chat_turn`).
//!
//! The page-wide parts live here: Right Ctrl's listeners (`keys.rs`); what
//! blocks lmgw's own models now ([`Block`], from the titlebar's `vram`
//! frame); the microphone released and the read-aloud of another thread
//! stopped when the owner opens a different thread; and on leaving the page,
//! the microphone released and the playback stopped.

use leptos::html;
use leptos::prelude::*;
use serde_json::Value;
use wasm_bindgen::JsCast;

use super::super::chat::ChatThread;
use super::devices::use_voice_devices;
use super::dictation::{Dictation, DictationParts, Trigger};
use super::keys::{key_down, key_up_releases, KeyDown, DICTATE_KEY_LABEL};
use super::read_aloud::{LiveSpeech, ReadAloud};
use super::state::{
    refusal_link, remote_note, turn_refusal, under_block, Block, Note, Surface, TURN_KEY,
};
use super::status::VoiceStatus;
use super::StageResolved;

/// The page's voice. `Copy`.
#[derive(Clone, Copy)]
pub(crate) struct PageVoice {
    pub dictation: Dictation,
    pub read_aloud: ReadAloud,
    pub status: VoiceStatus,
    pub current: RwSignal<Option<ChatThread>>,
    /// What blocks lmgw's own models now (tracked): the GPU hold and a
    /// benchmark run's lease, both swapping a voice model to its fallback
    /// (WP7 review M2).
    pub block: Memo<Block>,
    /// Voice mode is on (the realtime panel shows, WP9): Right Ctrl's
    /// dictation listens to nothing, and no turn asks for `speak` — the
    /// session speaks its own replies (§9.7.1, §9.7.8).
    pub voice_mode: RwSignal<bool>,
    /// The composer's text box, focused again when voice mode ends.
    pub composer_ta: NodeRef<html::Textarea>,
    /// The thread's text-to-speech as every speaker button says it, made
    /// once per page rather than once per reply (WP11 UI review NIT 9):
    /// what a block or a remote alias does to it, whether it is swapped
    /// now, and its alias (tracked).
    pub tts_note: Memo<Option<String>>,
    pub tts_blocked: Memo<bool>,
    pub tts_alias: Memo<Option<String>>,
}

impl PageVoice {
    /// A turn of thread `tid` into message `key` is about to be sent:
    /// `speak: true` goes on its body when the thread reads its replies
    /// aloud, and the stream's speech frames go to the answer. An open
    /// microphone is finished first: nothing plays into it.
    pub(crate) fn for_turn(
        &self,
        tid: i64,
        key: u64,
        body: &mut Value,
        ctrl: Option<&web_sys::AbortController>,
    ) -> Option<LiveSpeech> {
        self.status.clear_info();
        // The last turn's refusal: this turn says its own.
        self.status.clear(TURN_KEY);
        // In voice mode the session speaks its replies: a text turn sent
        // meanwhile (an edit, a regenerate) is not read aloud as well, or
        // the reply would be spoken twice (§9.7.8).
        if self.voice_mode.get_untracked() {
            return None;
        }
        let speech = self.read_aloud.for_turn(tid, key, body, ctrl);
        if speech.is_some() {
            self.dictation.before_playback();
        }
        speech
    }

    /// A speaker button reads stored reply `mid` (message `key`): an open
    /// microphone is finished first (review m2).
    pub(crate) fn play_stored(&self, key: u64, tid: i64, mid: i64) {
        self.dictation.before_playback();
        self.read_aloud.play_stored(key, tid, mid);
    }

    /// A text turn failed with `message` (and the gateway's `code`): a
    /// refusal the page has words for goes on the composer's line, in
    /// place of the chat stage's `state` note — the hold is one amber chip,
    /// never two (WP11 UI review m6). The reply bubble keeps the message.
    pub(crate) fn turn_error(&self, code: Option<&str>, message: &str) {
        let Some(code) = code else { return };
        if let Some((kind, text)) = turn_refusal(code, Surface::Text, message) {
            self.status
                .set(Note::new(TURN_KEY, kind, text).linked(refusal_link(code)));
        }
    }

    /// A voice frame of a turn's stream: a read-aloud's own, or a `state`
    /// of the turn's model for the status line.
    pub(crate) fn turn_frame(&self, speech: Option<&LiveSpeech>, name: &str, data: &Value) {
        match speech {
            Some(s) => s.frame(name, data),
            None if name == "state" => self.status.state_frame(data),
            None => {}
        }
    }
}

/// Made by the Chat page, once.
pub(crate) fn provide_page_voice(
    current: RwSignal<Option<ChatThread>>,
    composer: RwSignal<String>,
    composer_ta: NodeRef<html::Textarea>,
) -> PageVoice {
    let status = VoiceStatus::new(current);
    let read_aloud = ReadAloud::new(status, current);
    let dictation = Dictation::new(DictationParts {
        composer,
        composer_ta,
        current,
        status,
        read_aloud,
        devices: use_voice_devices(),
    });
    let live = crate::live::use_live();
    let block = Memo::new(move |_| {
        live.vram.with(|v| match v {
            Some(v) => Block {
                hold: v.hold_active,
                benchmark: v.benchmark.is_some(),
                unknown: false,
            },
            None => Block {
                unknown: true,
                ..Default::default()
            },
        })
    });
    let voice_mode = RwSignal::new(false);
    let tts = move |f: &dyn Fn(&StageResolved, Block) -> Option<String>| {
        let b = block.get();
        current.with(|c| {
            c.as_ref()
                .and_then(|t| t.voice_resolved.as_ref())
                .and_then(|r| f(&r.tts, b))
        })
    };
    let tts_note = Memo::new(move |_| tts(&|s, b| under_block(s, b).or_else(|| remote_note(s))));
    let tts_blocked = Memo::new(move |_| tts(&|s, b| b.blocks(s).then(String::new)).is_some());
    let tts_alias = Memo::new(move |_| tts(&|s, _| s.alias.clone()));
    let pv = PageVoice {
        dictation,
        read_aloud,
        status,
        current,
        block,
        voice_mode,
        composer_ta,
        tts_note,
        tts_blocked,
        tts_alias,
    };
    provide_context(pv);
    install_keys(dictation, voice_mode);

    // Another thread: the microphone is released (what was said is
    // discarded), a read-aloud of the thread left stops, and its notes go.
    let tid = Memo::new(move |_| current.with(|c| c.as_ref().map(|t| t.id)));
    Effect::new(move |prev: Option<Option<i64>>| {
        let id = tid.get();
        if prev.is_some_and(|p| p != id) {
            dictation.cancel(Some("dictation discarded: another conversation was opened"));
            read_aloud.stop_unless(id);
            status.clear_info();
        }
        id
    });
    on_cleanup(move || {
        dictation.cancel(None);
        read_aloud.stop();
    });
    pv
}

/// The page's voice, from the Chat page's context.
pub(crate) fn use_page_voice() -> Option<PageVoice> {
    use_context::<PageVoice>()
}

/// Right Ctrl held anywhere on the page records; another key, a pointer
/// press or the wheel cancels (silently within the arm time: a shortcut);
/// Esc discards; the window losing focus ends a held recording as a release
/// would (its key-up never comes). Off in voice mode (§9.7.1): the session
/// has the microphone, and a second capture beside it is never opened.
fn install_keys(d: Dictation, voice_mode: RwSignal<bool>) {
    let combo = format!("dictation cancelled: another key was pressed with {DICTATE_KEY_LABEL}");
    let down = window_event_listener(leptos::ev::keydown, move |ev| {
        if voice_mode.try_get_untracked() == Some(true) {
            return;
        }
        match key_down(
            &ev.code(),
            &ev.key(),
            ev.repeat(),
            ev.is_composing(),
            d.phase(),
        ) {
            KeyDown::Nothing => {}
            KeyDown::Start => d.press(Trigger::Key),
            KeyDown::Discard => {
                // A native dialog open now closes on this Esc too (review
                // n10): its default is left alone.
                if !dialog_open() {
                    ev.prevent_default();
                }
                d.cancel(Some("dictation discarded"));
            }
            // The other key keeps its own effect (Ctrl+C copies).
            KeyDown::Cancel => d.cancel_combo(&combo),
        }
    });
    let up = window_event_listener(leptos::ev::keyup, move |ev| {
        if key_up_releases(&ev.code(), d.phase()) {
            d.release();
        }
    });
    // Ctrl+wheel (the app's zoom), Ctrl+click and Ctrl+drag are
    // combinations too (review M1).
    let wheel = window_event_listener(leptos::ev::wheel, move |ev| {
        if ev.ctrl_key() && d.phase().held() {
            d.cancel_combo(&format!(
                "dictation cancelled: the wheel was turned with {DICTATE_KEY_LABEL}"
            ));
        }
    });
    let pointer = window_event_listener(leptos::ev::pointerdown, move |_| {
        if d.phase().held() {
            d.cancel_combo(&format!(
                "dictation cancelled: the mouse was used with {DICTATE_KEY_LABEL}"
            ));
        }
    });
    let blur = window_event_listener(leptos::ev::blur, move |ev| {
        // The window's own blur, not a field's (it does not bubble, but a
        // capture listener would see them).
        let own = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Window>().ok())
            .is_some();
        if own && d.phase().held() {
            d.release();
        }
    });
    on_cleanup(move || {
        down.remove();
        up.remove();
        wheel.remove();
        pointer.remove();
        blur.remove();
    });
}

/// A native `<dialog>` is open (a modal of the page).
fn dialog_open() -> bool {
    document()
        .query_selector("dialog[open]")
        .ok()
        .flatten()
        .is_some()
}
