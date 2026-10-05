//! Dictation (chat-voice design §5): speech into the composer, for editing,
//! never sent on its own.
//!
//! **Press** — the composer's microphone (a short click toggles, a press held
//! records until release) or Right Ctrl held anywhere on the Chat page
//! (`keys.rs`). Right Ctrl is **armed** first ([`KEY_WARM_ARM_MS`]): nothing
//! opens until it has been held that long with no other key, pointer press or
//! wheel turn, so Right Ctrl used as a modifier (Right Ctrl+C, Ctrl+click)
//! opens no microphone, loads nothing and says nothing (WP7 review M1, m3).
//! Then, for either trigger:
//! 1. any read-aloud stops — nothing plays into the microphone; a pointer
//!    press (or the button's keyboard activation) also makes or resumes the
//!    playback context, being the gesture — Right Ctrl never does (a
//!    modifier-only key press is no user activation in a browser, review m4);
//! 2. the microphone opens with the window's device and the echo mode's
//!    constraints (§12.1), 16 kHz PCM16 to memory (§11.2), with a timer and
//!    a level meter on the status line;
//! 3. the thread's ASR is warmed (`voice/warm`), its `state` frames on the
//!    status line ("loading …", the hold, a fallback); a fallback taking the
//!    audio while it records turns the microphone amber (review m5). The
//!    warm is an Admit, which may evict an idle model, so Right Ctrl sends it
//!    only once the hold is a dictation for sure (WP11 UI review m4): the
//!    recording holds speech for [`SPEECH_HOLD_MS`], or the microphone was
//!    open a further [`KEY_WARM_ARM_MS`] with no other key. A slow Right
//!    Ctrl combination (the key held while reaching for the other one)
//!    opens the microphone and closes it again, but warms nothing. A
//!    pointer press warms at once.
//!
//! **Release** — the tracks stop first (the microphone is free), the last
//! partial chunk is handed over, and the recording goes up as a WAV to
//! `transcribe`. Its text lands at the composer's caret, the composer is
//! focused, and the composer is marked as dictated: Enter sends it, as typed
//! text, with `voice: {via: "dictation", …}` (`DictationMark`). Emptying the
//! composer drops the mark. A release before the microphone was open — a
//! Right Ctrl tap within its arm time, or a press let go while the
//! microphone still opened — uploads nothing and says so (review M1).
//!
//! **Nothing is uploaded silently that should not be:** a microphone that
//! ended on its own (unplugged, permission taken back, page hidden) is said
//! and nothing goes up; a recording of nothing but digital silence (a muted
//! track) is said and not sent; one over `max_body_mb` is said, naming the
//! setting. Esc discards, as does another key, a pointer press or the wheel
//! while Right Ctrl is held, a thread switch and leaving the page — the
//! microphone is released on every path. A send, a speaker button or a turn
//! read aloud while dictating finishes or discards the dictation explicitly
//! ([`Dictation::before_send`], [`Dictation::before_playback`], review m2).

mod upload;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use leptos::html;
use leptos::prelude::*;
use serde_json::Value;

use super::super::chat::ChatThread;
use super::audio::capture::{self, Capture, CaptureEvent};
use super::audio::{pcm, player};
use super::devices::VoiceDevices;
use super::keys::{Phase, DICTATE_KEY_LABEL};
use super::read_aloud::ReadAloud;
use super::spoken::{insert_dictated, ms_text, DictationMark, Transcript};
use super::state::{block_refusal, blocker, note_of, Note, NoteKind};
use super::status::VoiceStatus;
use crate::scope::Scope;

/// How long Right Ctrl is held before anything opens: a second key, a
/// pointer press or the wheel within it (Right Ctrl as a modifier) cancels
/// without a microphone, a warm or a word.
pub(crate) const KEY_WARM_ARM_MS: u64 = 300;

/// Speech, for Right Ctrl's warm: the recording's level at or above this
/// (RMS, dBFS) ...
const SPEECH_DBFS: f64 = -40.0;
/// ... for this long without a gap, so a key's click is no speech.
const SPEECH_HOLD_MS: f64 = 120.0;

/// What started a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trigger {
    Pointer,
    Key,
}

/// Where dictation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DictState {
    Idle,
    /// Right Ctrl is down, its arm time not over: nothing is open yet.
    Arming,
    Opening,
    Recording,
    /// The tracks are stopped; the last chunk is being handed over.
    Finishing,
    Transcribing,
}

impl DictState {
    pub(crate) fn key(self) -> &'static str {
        match self {
            DictState::Idle => "idle",
            DictState::Arming => "arming",
            DictState::Opening => "opening",
            DictState::Recording => "recording",
            DictState::Finishing => "finishing",
            DictState::Transcribing => "transcribing",
        }
    }

    /// The microphone may be open (or about to be).
    pub(crate) fn mic_open(self) -> bool {
        matches!(self, DictState::Opening | DictState::Recording)
    }

    /// Shown on the status line (an armed key is not, yet).
    pub(crate) fn shown(self) -> bool {
        !matches!(self, DictState::Idle | DictState::Arming)
    }
}

/// One recording's resources.
struct Rec {
    tid: i64,
    /// The thread's ASR when it started, for the failure's words.
    asr: Option<String>,
    capture: Option<Capture>,
    samples: Rc<RefCell<Vec<i16>>>,
    warm: Option<web_sys::AbortController>,
    upload: Option<web_sys::AbortController>,
    started: f64,
    /// `max_body_mb`, read while recording (`None`: not read yet; `Some(None)`:
    /// it could not be read).
    limit: Option<Option<u32>>,
}

impl Rec {
    fn abort(&mut self) {
        if let Some(c) = self.capture.take() {
            c.stop();
        }
        for c in [self.warm.take(), self.upload.take()].into_iter().flatten() {
            c.abort();
        }
    }
}

/// The page's dictation. `Copy`; made by the Chat page.
#[derive(Clone, Copy)]
pub(crate) struct Dictation {
    pub state: RwSignal<DictState>,
    trigger: RwSignal<Option<Trigger>>,
    /// The recording ends when the press ends (held), not on a second click.
    held: RwSignal<bool>,
    /// The composer holds dictated text.
    pub mark: RwSignal<Option<DictationMark>>,
    pub level: RwSignal<f64>,
    pub elapsed_ms: RwSignal<u64>,
    /// The last recording's samples and peak, for the probes.
    pub samples: RwSignal<u64>,
    pub peak: RwSignal<u32>,
    /// A fallback took the audio while it records (the warm said so): the
    /// microphone turns amber (review m5).
    pub swapped: RwSignal<bool>,
    rec: StoredValue<Option<Rec>, LocalStorage>,
    generation: StoredValue<u64>,
    composer: RwSignal<String>,
    composer_ta: NodeRef<html::Textarea>,
    current: RwSignal<Option<ChatThread>>,
    status: VoiceStatus,
    read_aloud: ReadAloud,
    devices: VoiceDevices,
    alive: Scope,
}

pub(crate) struct DictationParts {
    pub composer: RwSignal<String>,
    pub composer_ta: NodeRef<html::Textarea>,
    pub current: RwSignal<Option<ChatThread>>,
    pub status: VoiceStatus,
    pub read_aloud: ReadAloud,
    pub devices: VoiceDevices,
}

impl Dictation {
    pub(crate) fn new(p: DictationParts) -> Self {
        let d = Self {
            state: RwSignal::new(DictState::Idle),
            trigger: RwSignal::new(None),
            held: RwSignal::new(false),
            mark: RwSignal::new(None),
            level: RwSignal::new(0.0),
            elapsed_ms: RwSignal::new(0),
            samples: RwSignal::new(0),
            peak: RwSignal::new(0),
            swapped: RwSignal::new(false),
            rec: StoredValue::new_local(None),
            generation: StoredValue::new(0),
            composer: p.composer,
            composer_ta: p.composer_ta,
            current: p.current,
            status: p.status,
            read_aloud: p.read_aloud,
            devices: p.devices,
            alive: Scope::new(),
        };
        // Emptying the composer drops the mark (§5).
        let (composer, mark) = (d.composer, d.mark);
        Effect::new(move |_| {
            let empty = composer.with(|c| c.trim().is_empty());
            if empty && mark.with_untracked(Option::is_some) {
                mark.set(None);
            }
        });
        d
    }

    /// The keys' view of it (untracked).
    pub(crate) fn phase(&self) -> Phase {
        match self.state.get_untracked() {
            DictState::Idle => Phase::Idle,
            DictState::Arming => Phase::Arming,
            DictState::Opening | DictState::Recording
                if self.trigger.get_untracked() == Some(Trigger::Key) =>
            {
                Phase::HeldByKey
            }
            _ => Phase::Busy,
        }
    }

    /// The recording ends when the press ends (tracked).
    pub(crate) fn holding(&self) -> bool {
        self.held.get()
    }

    /// A pointer press turned out held (or short: it toggles, the next
    /// click ends it).
    pub(crate) fn set_held(&self, held: bool) {
        self.held.set(held);
    }

    fn current_generation(&self, g: u64) -> bool {
        self.alive.alive() && self.generation.try_get_value() == Some(g)
    }

    /// Start: a pointer press opens at once; Right Ctrl arms first.
    pub(crate) fn press(&self, trigger: Trigger) {
        if self.state.get_untracked() != DictState::Idle
            || self.current.with_untracked(Option::is_none)
        {
            return;
        }
        let g = self.generation.get_value() + 1;
        self.generation.set_value(g);
        self.trigger.set(Some(trigger));
        self.held.set(trigger == Trigger::Key);
        match trigger {
            Trigger::Key => {
                self.state.set(DictState::Arming);
                let me = *self;
                set_timeout(
                    move || {
                        if me.current_generation(g) && me.state.get_untracked() == DictState::Arming
                        {
                            me.start(g);
                        }
                    },
                    Duration::from_millis(KEY_WARM_ARM_MS),
                );
            }
            Trigger::Pointer => {
                // The press is the gesture: the playback context is made (or
                // resumed) here, never on Right Ctrl (review m4).
                let ready = player::player();
                leptos::task::spawn_local(async move {
                    // A failure is said on the composer's voice button.
                    let _ = ready.await;
                });
                self.start(g);
            }
        }
    }

    /// Open the microphone and warm the ASR.
    fn start(&self, g: u64) {
        let Some(t) = self.current.get_untracked() else {
            return self.end(None);
        };
        self.status.clear_info();
        self.status.clear("dictation");
        // Nothing reads aloud into the microphone.
        self.read_aloud.stop();
        // A blocker is said where the feature is pressed (§2.3).
        if let Some(why) = blocker(t.voice_resolved.as_ref(), "asr") {
            return self.end(Some(Note::new(
                "dictation",
                NoteKind::Error,
                format!("dictation: {why}"),
            )));
        }
        self.state.set(DictState::Opening);
        self.level.set(0.0);
        self.elapsed_ms.set(0);
        self.samples.set(0);
        self.peak.set(0);
        self.swapped.set(false);
        let samples = Rc::new(RefCell::new(Vec::<i16>::new()));
        self.rec.set_value(Some(Rec {
            tid: t.id,
            asr: t.voice_resolved.as_ref().and_then(|r| r.asr.alias.clone()),
            capture: None,
            samples: samples.clone(),
            warm: None,
            upload: None,
            started: js_sys::Date::now(),
            limit: None,
        }));

        // The ASR's warm — Right Ctrl's once the hold is a dictation for sure
        // (module doc) — and the body limit for the release (read now,
        // while recording, not on the release's critical path: review n4).
        let key = self.trigger.get_untracked() == Some(Trigger::Key);
        if !key {
            self.start_warm(t.id, g);
        }
        let me = *self;
        leptos::task::spawn_local(async move {
            let limit = upload::max_body_mb().await;
            if me.current_generation(g) {
                me.rec.update_value(|r| {
                    if let Some(r) = r {
                        r.limit = Some(limit);
                    }
                });
            }
        });

        // The microphone.
        let opts = capture::Options::new(
            pcm::DICTATION_RATE,
            self.devices.echo.get_untracked(),
            self.devices.input.get_untracked(),
        );
        let on_chunk = Box::new(move |c: Vec<i16>| {
            samples.borrow_mut().extend_from_slice(&c);
        });
        let on_event = Box::new(move |e: CaptureEvent| {
            if !me.current_generation(g) {
                return;
            }
            match e {
                CaptureEvent::Muted => me.status.set(Note::new(
                    "mic",
                    NoteKind::Warn,
                    "the system muted the microphone: it records silence",
                )),
                CaptureEvent::Unmuted => me.status.clear("mic"),
                CaptureEvent::Ended(why) => {
                    me.cancel(None);
                    me.status.set(Note::new(
                        "dictation",
                        NoteKind::Error,
                        format!("dictation stopped: {}; nothing was sent", why.message()),
                    ));
                }
            }
        });
        // Not scoped: a microphone that opens after the page went must still
        // be released, so this task always sees its answer.
        leptos::task::spawn_local(async move {
            let opened = capture::open(opts, on_chunk, on_event).await;
            if !me.current_generation(g) {
                if let Ok(c) = opened {
                    c.stop();
                }
                return;
            }
            match opened {
                Ok(c) => {
                    if c.muted_by_system() {
                        me.status.set(Note::new(
                            "mic",
                            NoteKind::Warn,
                            "the system muted the microphone: it records silence",
                        ));
                    }
                    if let Some(n) = &c.info().note {
                        me.status.set(Note::new("mic", NoteKind::Warn, n.clone()));
                    }
                    me.rec.update_value(|r| {
                        if let Some(r) = r {
                            r.started = js_sys::Date::now();
                            r.capture = Some(c);
                        }
                    });
                    me.state.set(DictState::Recording);
                    me.meter(g);
                    if key {
                        // Held a further arm time with no other key.
                        set_timeout(
                            move || me.warm_once(g),
                            Duration::from_millis(KEY_WARM_ARM_MS),
                        );
                    }
                }
                Err(e) => {
                    me.cancel(None);
                    me.status.set(Note::new(
                        "dictation",
                        NoteKind::Error,
                        format!("dictation: {e}"),
                    ));
                }
            }
        });
    }

    /// Right Ctrl's warm, once (module doc): while this recording is still
    /// the one, and nothing warmed it yet.
    fn warm_once(&self, g: u64) {
        if !self.current_generation(g) || self.state.get_untracked() != DictState::Recording {
            return;
        }
        let tid = self
            .rec
            .try_with_value(|r| r.as_ref().filter(|r| r.warm.is_none()).map(|r| r.tid))
            .flatten();
        if let Some(tid) = tid {
            self.start_warm(tid, g);
        }
    }

    fn start_warm(&self, tid: i64, g: u64) {
        let Ok(ctrl) = web_sys::AbortController::new() else {
            return;
        };
        let signal = ctrl.signal();
        self.rec.update_value(|r| {
            if let Some(r) = r {
                r.warm = Some(ctrl);
            }
        });
        let me = *self;
        let on_state = move |data: &Value| me.warm_frame(g, data);
        leptos::task::spawn_local(upload::warm(tid, on_state, signal));
    }

    /// A `state` frame of the press's warm: said on the status line; a
    /// fallback that takes the audio while the microphone is open turns it
    /// amber, and the note says how to keep the audio here (review m5).
    fn warm_frame(&self, g: u64, data: &Value) {
        let fallback = data["stage"].as_str() == Some("asr")
            && data["state"].as_str() == Some("fallback")
            && self.current_generation(g)
            && self.state.get_untracked().mic_open();
        if !fallback {
            return self.status.state_frame(data);
        }
        self.swapped.set(true);
        let s = serde_json::from_value(data.clone()).unwrap_or_default();
        if let Some(mut n) = note_of(&s, &[]) {
            n.text.push_str(" — Esc discards the recording");
            self.status.set(n);
        }
    }

    /// The level and the clock, per frame while recording.
    fn meter(&self, g: u64) {
        let me = *self;
        let mut buf = Vec::new();
        let key = self.trigger.get_untracked() == Some(Trigger::Key);
        let speech = pcm::dbfs_level(SPEECH_DBFS);
        // Since when the recording holds speech, while it does.
        let mut speaking_since: Option<f64> = None;
        super::devices::run_meter(self.alive, move || {
            if !me.current_generation(g) || me.state.get_untracked() != DictState::Recording {
                me.level.try_set(0.0);
                return false;
            }
            let (analyser, started, n) = match me.rec.try_with_value(|r| {
                r.as_ref().map(|r| {
                    (
                        r.capture.as_ref().map(Capture::analyser),
                        r.started,
                        r.samples.borrow().len(),
                    )
                })
            }) {
                Some(Some(v)) => v,
                _ => return false,
            };
            if let Some(a) = analyser {
                buf.resize(a.fft_size() as usize, 0.0);
                a.get_float_time_domain_data(&mut buf);
                let level = pcm::meter_level(&buf);
                me.level.set(level);
                if key {
                    let now = js_sys::Date::now();
                    match (level >= speech, speaking_since) {
                        (true, None) => speaking_since = Some(now),
                        (true, Some(t)) if now - t >= SPEECH_HOLD_MS => me.warm_once(g),
                        (false, _) => speaking_since = None,
                        _ => {}
                    }
                }
            }
            me.elapsed_ms
                .set((js_sys::Date::now() - started).max(0.0) as u64);
            me.samples.set(n as u64);
            true
        });
    }

    /// End the recording and transcribe it. Before the microphone was open
    /// nothing was recorded: nothing goes up, and that is said (review M1).
    pub(crate) fn release(&self) {
        match self.state.get_untracked() {
            DictState::Arming => {
                return self.cancel(Some(&format!(
                    "nothing was recorded: hold {DICTATE_KEY_LABEL} until the red dot shows, \
                     then speak"
                )))
            }
            DictState::Opening => {
                return self.cancel(Some(
                    "the microphone was not open yet: nothing was recorded — hold until the \
                     red dot shows",
                ))
            }
            DictState::Recording => {}
            _ => return,
        }
        let g = self.generation.get_value();
        self.state.set(DictState::Finishing);
        self.level.set(0.0);
        let me = *self;
        leptos::task::spawn_local(async move {
            let capture = me
                .rec
                .try_update_value(|r| r.as_mut().and_then(|r| r.capture.take()))
                .flatten();
            // The tracks first, then the last partial chunk (§11.2).
            if let Some(c) = &capture {
                c.finish().await;
            }
            drop(capture);
            if !me.current_generation(g) {
                return;
            }
            me.transcribe(g).await;
        });
    }

    async fn transcribe(&self, g: u64) {
        let Some((tid, asr, samples, limit)) = self
            .rec
            .try_with_value(|r| {
                r.as_ref()
                    .map(|r| (r.tid, r.asr.clone(), r.samples.borrow().clone(), r.limit))
            })
            .flatten()
        else {
            return;
        };
        let peak = samples
            .iter()
            .map(|s| (*s as i32).unsigned_abs())
            .max()
            .unwrap_or(0);
        self.samples.set(samples.len() as u64);
        self.peak.set(peak);
        self.elapsed_ms
            .set(pcm::ms_of(samples.len() as u64, pcm::DICTATION_RATE));
        if samples.is_empty() {
            return self.end(Some(Note::new(
                "dictation",
                NoteKind::Info,
                format!(
                    "nothing was recorded: hold the microphone (or {DICTATE_KEY_LABEL}) while \
                     you speak"
                ),
            )));
        }
        if peak == 0 {
            return self.end(Some(Note::new(
                "dictation",
                NoteKind::Warn,
                "the microphone delivered only silence (muted, or no signal): nothing was sent",
            )));
        }
        let wav = pcm::wav(&samples, pcm::DICTATION_RATE);
        let limit = match limit {
            Some(l) => l,
            None => upload::max_body_mb().await,
        };
        if !self.current_generation(g) {
            return;
        }
        if let Some(why) = limit.and_then(|l| upload::over_body_limit(wav.len(), l)) {
            return self.end(Some(Note::new("dictation", NoteKind::Error, why)));
        }
        let Ok(ctrl) = web_sys::AbortController::new() else {
            return self.end(None);
        };
        let signal = ctrl.signal();
        self.rec.update_value(|r| {
            if let Some(r) = r {
                r.upload = Some(ctrl);
            }
        });
        self.state.set(DictState::Transcribing);
        let res = upload::transcribe(tid, &wav, &signal).await;
        if !self.current_generation(g) {
            return;
        }
        // The composer is the page's: a transcript of a thread left in the
        // moment before its switch was seen goes nowhere (review m9).
        if self.current.with_untracked(|c| c.as_ref().map(|t| t.id)) != Some(tid) {
            return self.end(Some(Note::new(
                "dictation",
                NoteKind::Info,
                "dictation discarded: another conversation was opened",
            )));
        }
        match res {
            Ok(t) if t.text.trim().is_empty() => self.end(Some(Note::new(
                "dictation",
                NoteKind::Warn,
                format!(
                    "{} heard no words (the recording peaked at {}): nothing was inserted",
                    t.alias.as_deref().unwrap_or("the speech-to-text model"),
                    pcm_peak_dbfs(peak)
                ),
            ))),
            Ok(t) => {
                self.insert(&t);
                let note = Note::new("dictation", NoteKind::Info, inserted_text(&t));
                self.end(Some(note));
            }
            Err(f) => self.end(Some(
                match block_refusal(&f.code, "dictation", &f.message) {
                    Some(text) => Note::new("dictation", NoteKind::Hold, text),
                    None => Note::new("dictation", NoteKind::Error, f.text(asr.as_deref())),
                },
            )),
        }
    }

    /// Put the text at the composer's caret, focus it, and mark it.
    fn insert(&self, t: &Transcript) {
        let ta = self.composer_ta.get_untracked();
        let text = self.composer.get_untracked();
        let len = text.encode_utf16().count() as u32;
        let (start, end) = ta
            .as_ref()
            .map(|el| {
                (
                    el.selection_start().ok().flatten().unwrap_or(len),
                    el.selection_end().ok().flatten().unwrap_or(len),
                )
            })
            .unwrap_or((len, len));
        let (next, caret) = insert_dictated(&text, start, end, &t.text);
        // The box first, then the page's copy: the binding writes the same
        // value again, which leaves the caret where it is put here.
        if let Some(el) = &ta {
            el.set_value(&next);
        }
        self.composer.set(next);
        self.mark
            .update(|m| *m = Some(DictationMark::add(m.take(), t)));
        if let Some(el) = ta {
            let _ = el.focus();
            let _ = el.set_selection_range(caret, caret);
        }
    }

    /// The recording is over (sent or not): release what is left.
    fn end(&self, note: Option<Note>) {
        self.rec.try_update_value(|r| {
            if let Some(mut r) = r.take() {
                r.abort();
            }
        });
        self.status.clear_busy("asr");
        self.state.try_set(DictState::Idle);
        self.trigger.try_set(None);
        self.level.try_set(0.0);
        self.swapped.try_set(false);
        if let Some(n) = note {
            self.status.set(n);
        }
    }

    /// Discard the recording, on whatever path: Esc, another key, a thread
    /// switch, leaving the page, a microphone that ended. The microphone is
    /// released at once; nothing is sent.
    pub(crate) fn cancel(&self, note: Option<&str>) {
        if self
            .state
            .try_get_untracked()
            .is_none_or(|s| s == DictState::Idle)
        {
            return;
        }
        self.generation.try_update_value(|g| *g += 1);
        self.end(note.map(|n| Note::new("dictation", NoteKind::Info, n)));
    }

    /// Another key, a pointer press or the wheel while Right Ctrl is held:
    /// within its arm time that is a shortcut, cancelled without a word
    /// (nothing was opened); later the recording is discarded with `note`
    /// (review m3).
    pub(crate) fn cancel_combo(&self, note: &str) {
        if self.state.get_untracked() == DictState::Arming {
            self.cancel(None);
        } else {
            self.cancel(Some(note));
        }
    }

    /// A send is about to take the composer (Enter, Send). Dictation is
    /// finished or discarded explicitly, never left to land in the emptied
    /// composer as the start of the next message (review m2). `false`: the
    /// send waits, and the status line says why.
    pub(crate) fn before_send(&self) -> bool {
        let key = self.trigger.get_untracked() == Some(Trigger::Key);
        match self.state.get_untracked() {
            DictState::Idle => true,
            // Right Ctrl+Enter within the arm time: a shortcut, nothing open.
            DictState::Arming => {
                self.cancel(None);
                true
            }
            // Enter with Right Ctrl held: the other key keeps its effect.
            DictState::Opening | DictState::Recording if key => {
                self.cancel(Some(&format!(
                    "dictation cancelled: Enter was pressed with {DICTATE_KEY_LABEL}"
                )));
                true
            }
            DictState::Opening => {
                self.release();
                true
            }
            DictState::Recording => {
                self.release();
                self.status.set(Note::new(
                    "send",
                    NoteKind::Info,
                    "Enter finished the dictation: its text lands in the box — Enter again sends",
                ));
                false
            }
            DictState::Finishing | DictState::Transcribing => {
                self.status.set(Note::new(
                    "send",
                    NoteKind::Info,
                    "the dictation is still being transcribed: Enter again once its text is in \
                     (Esc discards it)",
                ));
                false
            }
        }
    }

    /// Something is about to play (a speaker button, a turn read aloud): an
    /// open microphone is finished first — its tracks stop at once — so
    /// nothing plays into it (review m2); one still opening is discarded.
    pub(crate) fn before_playback(&self) {
        match self.state.get_untracked() {
            DictState::Arming => self.cancel(None),
            DictState::Opening | DictState::Recording => self.release(),
            _ => {}
        }
    }

    /// The send takes the mark (§5); `None` for typed text.
    pub(crate) fn take_mark(&self) -> Option<DictationMark> {
        let m = self.mark.get_untracked();
        if m.is_some() {
            self.mark.set(None);
        }
        m
    }

    /// A send refused outright gives the text back, and with it the mark.
    pub(crate) fn restore_mark(&self, m: Option<DictationMark>) {
        if m.is_some() {
            self.mark.try_set(m);
        }
    }

    /// The send's `voice` for a dictated composer (§5).
    pub(crate) fn voice_for_send(&self, m: Option<&DictationMark>, body: &mut Value) {
        if let Some(m) = m {
            body["voice"] = m.send_json();
        }
    }
}

/// A recording's peak in dBFS, for the "heard no words" note: information,
/// not a gate (review n11).
fn pcm_peak_dbfs(peak: u32) -> String {
    let db = (20.0 * (f64::from(peak.max(1)) / 32768.0).log10()).round() as i64;
    format!("{db} dBFS").replace('-', "−")
}

/// The note after a dictation landed.
fn inserted_text(t: &Transcript) -> String {
    let mut s = format!(
        "dictated with {}",
        t.alias.as_deref().unwrap_or("the speech-to-text model")
    );
    if let Some(b) = &t.asr_answered_by {
        s.push_str(&format!(" — answered by {b}"));
    }
    if let Some(ms) = t.asr_ms {
        s.push_str(&format!(" in {}", ms_text(ms)));
    }
    s.push_str(" · edit it, Enter sends");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_inserted_note_names_the_model_and_its_fallback() {
        let t = Transcript {
            text: "Hallo".into(),
            alias: Some("audio/parakeet".into()),
            asr_answered_by: Some("openai/whisper-1".into()),
            asr_ms: Some(702),
            ..Default::default()
        };
        assert_eq!(
            inserted_text(&t),
            "dictated with audio/parakeet — answered by openai/whisper-1 in 702 ms · edit it, \
             Enter sends"
        );
    }

    #[test]
    fn the_peak_is_said_in_dbfs() {
        assert_eq!(pcm_peak_dbfs(32767), "0 dBFS");
        assert_eq!(pcm_peak_dbfs(820), "−32 dBFS");
        assert_eq!(pcm_peak_dbfs(0), "−90 dBFS");
    }

    #[test]
    fn an_armed_key_is_not_shown_and_opens_nothing() {
        assert!(!DictState::Arming.shown() && !DictState::Arming.mic_open());
        assert!(DictState::Opening.mic_open() && DictState::Recording.mic_open());
        assert!(!DictState::Transcribing.mic_open() && DictState::Transcribing.shown());
    }
}
