//! The voice controls of the Chat (chat-voice design §5, §6.5): the
//! composer's voice group — the microphone, and beside it the voice menu
//! with "read replies aloud" and the audio devices (`menu.rs`) — and the
//! speaker button in each reply's action row.
//!
//! The group is one compact split control, so the composer keeps a usable
//! text box at every window size (WP7 review M3): two icon buttons where
//! there were three.
//!
//! Each control says before it is pressed where its audio or text would go:
//! the model in effect, a remote one named (the microphone carries a cloud
//! mark), and while the GPU hold or a benchmark run swaps it, the fallback
//! that would answer (§2.3) — the control turns amber for it. A blocker (no
//! model set, one that vanished) is said when the control is pressed, on the
//! status line.

mod menu;

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::dictation::{DictState, Trigger};
use super::keys::DICTATE_KEY_LABEL;
use super::page::{use_page_voice, PageVoice};
use super::read_aloud::{speaker_title, PlayState};
use super::state::{blocker, problem_link, remote_note, under_block, Note, NoteKind};
use super::{StageResolved, VoiceDraft};

/// A press held at least this long ends at its release; a shorter one is a
/// click that toggles.
const HOLD_MS: f64 = 350.0;

const ICON_MIC: &str = "M8 1.8a2.2 2.2 0 0 1 2.2 2.2v4a2.2 2.2 0 0 1-4.4 0V4A2.2 2.2 0 0 1 8 1.8z M3.8 7.6a4.2 4.2 0 0 0 8.4 0 M8 11.8v2.4 M5.8 14.2h4.4";
pub(super) const ICON_SPEAKER: &str =
    "M2.5 6.2h2.4L8.3 3.4v9.2L4.9 9.8H2.5z M10.6 5.6a3.4 3.4 0 0 1 0 4.8 M12.4 3.8a6 6 0 0 1 0 8.4";
const ICON_STOP: &str = "M4.5 4.5h7v7h-7z";
const ICON_CLOUD: &str =
    "M4.6 12.4h6.9a2.6 2.6 0 0 0 .3-5.2 3.6 3.6 0 0 0-6.9-1 2.9 2.9 0 0 0-.3 6.2z";

/// The stage `stage` (`asr` or `tts`) of the open thread, with `f` (tracked).
fn with_stage<T>(
    pv: PageVoice,
    stage: &'static str,
    f: impl FnOnce(&StageResolved) -> T,
) -> Option<T> {
    pv.current.with(|c| {
        let r = c.as_ref()?.voice_resolved.as_ref()?;
        Some(f(if stage == "asr" { &r.asr } else { &r.tts }))
    })
}

/// What a stage says before it is used: what a block (the hold, a benchmark
/// run) does to it, or that it is remote (tracked).
pub(super) fn before_use(pv: PageVoice, stage: &'static str) -> Memo<Option<String>> {
    Memo::new(move |_| {
        let b = pv.block.get();
        with_stage(pv, stage, |s| under_block(s, b).or_else(|| remote_note(s))).flatten()
    })
}

/// The stage is swapped to its fallback (or refused) now (tracked).
pub(super) fn blocked(pv: PageVoice, stage: &'static str) -> Memo<bool> {
    Memo::new(move |_| {
        let b = pv.block.get();
        with_stage(pv, stage, |s| b.blocks(s)).unwrap_or(false)
    })
}

/// The composer's voice group: the microphone, voice mode and the voice
/// menu, one split control (review M3; voice mode joins it, §9.7.3).
#[component]
pub(crate) fn VoiceControls(draft: VoiceDraft, on_saved: Callback<()>) -> impl IntoView {
    view! {
        <div class="voice-split" role="group" aria-label="Voice">
            <MicButton/>
            <VoiceModeButton/>
            <menu::VoiceMenu draft=draft on_saved=on_saved/>
        </div>
    }
}

const ICON_WAVE: &str = "M2 8h1.2 M4.6 5.6v4.8 M7 3.4v9.2 M9.4 5v6 M11.8 6.6v2.8 M14 8h-1";

/// Voice mode (§9.1): the realtime panel in the composer's place. It says
/// in one line why it cannot open now — a reply streams, the thread has no
/// speech model, an Admin Chat thread — and that line is its tooltip and
/// accessible description; the button stays focusable so the reason can be
/// read, and a press says it on the status line.
#[component]
fn VoiceModeButton() -> impl IntoView {
    let Some(rt) = super::realtime::use_realtime() else {
        return ().into_any();
    };
    let reason = Memo::new(move |_| rt.blocked_reason());
    let title = move || match reason.get() {
        Some(why) => format!("Voice mode — not available now: {why}"),
        None => "Voice mode: talk with this conversation. The panel takes the composer's place; \
                 your replies and its answers are written into the thread as you speak."
            .to_string(),
    };
    view! {
        <button
            type="button"
            class="btn ghost composer-attach composer-voice-mode"
            class:off=move || reason.with(Option::is_some)
            aria-disabled=move || reason.with(Option::is_some).to_string()
            aria-label="Voice mode"
            aria-description=move || reason.get().unwrap_or_default()
            // Orca on WebKitGTK may not read `aria-description`; a hidden
            // element named by `aria-describedby` it does (review NIT 7).
            aria-describedby=move || reason.with(Option::is_some).then_some("voice-mode-why")
            title=title
            data-voice-mode-btn=""
            data-disabled-reason=move || reason.get().unwrap_or_default()
            on:mousedown=|ev| ev.prevent_default()
            on:click=move |_| rt.enter()
        >
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d=ICON_WAVE></path></svg>
        </button>
        <span class="sr-only" id="voice-mode-why">{move || reason.get().unwrap_or_default()}</span>
    }
    .into_any()
}

/// The composer's microphone.
#[component]
fn MicButton() -> impl IntoView {
    let Some(pv) = use_page_voice() else {
        return ().into_any();
    };
    let d = pv.dictation;
    let note = before_use(pv, "asr");
    let swapped_now = blocked(pv, "asr");
    let warn = Memo::new(move |_| swapped_now.get() || d.swapped.get());
    let remote =
        Memo::new(move |_| with_stage(pv, "asr", |s| s.local == Some(false)) == Some(true));
    let alias = Memo::new(move |_| with_stage(pv, "asr", |s| s.alias.clone()).flatten());
    let secure = super::audio::listing::secure();
    // Outside a secure context the button stays focusable, so its reason can
    // be read (review NIT 7): `aria-disabled`, and a press says it.
    const INSECURE: &str = "voice needs https or localhost: this page is not a secure context";
    let insecure = move || {
        pv.status.set(Note::new(
            "dictation",
            NoteKind::Error,
            format!("dictation: {INSECURE}"),
        ));
    };
    let down_at = StoredValue::new(None::<f64>);
    let title = move || match d.state.get() {
        DictState::Idle | DictState::Arming => {
            let mut t = format!(
                "Dictate: click to start and stop, or hold — here, or {DICTATE_KEY_LABEL} anywhere \
                 on the page. The text lands here for editing; nothing is sent until Enter."
            );
            if let Some(a) = alias.get() {
                t.push_str(&format!(" Speech-to-text: {a}."));
            }
            if let Some(n) = note.get() {
                t.push_str(&format!(" Before you speak: {n}."));
            }
            if !secure {
                t.push_str(&format!(" Not available: {INSECURE}."));
            }
            t
        }
        DictState::Opening => "Opening the microphone…".into(),
        DictState::Recording if d.holding() => {
            "Recording — release to transcribe, Esc discards".into()
        }
        DictState::Recording => "Recording — click to transcribe, Esc discards".into(),
        DictState::Finishing | DictState::Transcribing => "Transcribing… (Esc discards)".into(),
    };
    let end_press = move || {
        if let Some(t) = down_at.get_value() {
            down_at.set_value(None);
            if js_sys::Date::now() - t >= HOLD_MS {
                d.set_held(true);
                d.release();
            } else {
                d.set_held(false);
            }
        }
    };
    view! {
        <button
            type="button"
            class="btn ghost composer-attach composer-mic"
            class:rec=move || d.state.get().mic_open()
            class:busy=move || matches!(d.state.get(), DictState::Finishing | DictState::Transcribing)
            class:warn=move || warn.get()
            class:remote=move || remote.get()
            class:off=!secure
            aria-disabled=(!secure).to_string()
            aria-describedby=(!secure).then_some("mic-why")
            title=title
            aria-label="Dictate"
            aria-pressed=move || d.state.get().shown().to_string()
            data-mic-btn=""
            data-dictation=move || d.state.get().key()
            data-dictation-samples=move || d.samples.get().to_string()
            data-dictation-peak=move || d.peak.get().to_string()
            on:mousedown=|ev| ev.prevent_default()
            on:pointerdown=move |ev: web_sys::PointerEvent| {
                if ev.button() != 0 {
                    return;
                }
                ev.prevent_default();
                if !secure {
                    return insecure();
                }
                match d.state.get_untracked() {
                    DictState::Idle => {
                        // Its release reaches the button wherever it happens.
                        if let Some(el) = ev
                            .current_target()
                            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                        {
                            let _ = el.set_pointer_capture(ev.pointer_id());
                        }
                        let t = js_sys::Date::now();
                        down_at.set_value(Some(t));
                        d.press(Trigger::Pointer);
                        // Held past the rule: the hint says "release" while
                        // the press still lasts (review n3).
                        set_timeout(
                            move || {
                                if down_at.try_get_value() == Some(Some(t)) {
                                    d.set_held(true);
                                }
                            },
                            std::time::Duration::from_millis(HOLD_MS as u64),
                        );
                    }
                    // A toggled recording ends at the next press.
                    DictState::Opening | DictState::Recording if !d.holding() => d.release(),
                    _ => {}
                }
            }
            on:pointerup=move |_| end_press()
            on:pointercancel=move |_| end_press()
            on:click=move |ev: web_sys::MouseEvent| {
                // The keyboard's activation (Enter, Space on the button):
                // a toggle.
                if ev.detail() != 0 {
                    return;
                }
                if !secure {
                    return insecure();
                }
                match d.state.get_untracked() {
                    DictState::Idle => {
                        d.press(Trigger::Pointer);
                        d.set_held(false);
                    }
                    DictState::Opening | DictState::Recording => d.release(),
                    _ => {}
                }
            }
        >
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d=ICON_MIC></path></svg>
            // The speech-to-text is off this machine: recording sends the
            // audio there (review m6).
            {move || remote.get().then(|| view! {
                <svg class="mark-cloud" viewBox="0 0 16 16" aria-hidden="true"><path d=ICON_CLOUD></path></svg>
            })}
        </button>
        {(!secure).then(|| view! { <span class="sr-only" id="mic-why">{INSECURE}</span> })}
    }
    .into_any()
}

/// Why the speaker buttons rest in voice mode (WP11 UI review m3): a stored
/// reply read aloud would take the session's speakers and play into its open
/// microphone, where the session would hear it as the user's turn.
const SPEAK_IN_VOICE_MODE: &str =
    "leave voice mode to read a reply aloud: it would play into the open microphone";

/// The speaker button in a reply's action row: read it aloud, and stop
/// while it plays. One playback per page: starting it ends any other. It
/// rests in voice mode, with its reason (focusable: `aria-disabled`).
#[component]
pub(crate) fn SpeakerButton(
    key: u64,
    db_id: RwSignal<Option<i64>>,
    #[prop(into)] editing: Signal<bool>,
) -> impl IntoView {
    let Some(pv) = use_page_voice() else {
        return ().into_any();
    };
    let ra = pv.read_aloud;
    let state = Memo::new(move |_| ra.state_of(key));
    // One memo per reply for what is about this reply alone; what is about
    // the thread is the page's (`PageVoice::tts_*`).
    let facts = Memo::new(move |_| ra.facts_of(key));
    let resting = Memo::new(move |_| pv.voice_mode.get() && state.get().is_none());
    let title = move || {
        if resting.get() {
            return format!("Read aloud — {SPEAK_IN_VOICE_MODE}");
        }
        if db_id.get().is_none() {
            return "not stored yet — reload the conversation".to_string();
        }
        let extra: Vec<String> = pv.tts_note.get().into_iter().collect();
        facts.with(|f| {
            speaker_title(
                state.get(),
                pv.tts_alias.get().as_deref(),
                f.as_ref(),
                &extra,
            )
        })
    };
    let click = move |_| {
        if state.get_untracked().is_some() {
            ra.stop();
            return;
        }
        if resting.get_untracked() {
            if let Some(rt) = super::realtime::use_realtime() {
                rt.status
                    .set(Note::new("read-aloud", NoteKind::Info, SPEAK_IN_VOICE_MODE));
            }
            return;
        }
        let Some(mid) = db_id.get_untracked() else {
            return;
        };
        let Some(t) = pv.current.get_untracked() else {
            return;
        };
        if let Some(why) = blocker(t.voice_resolved.as_ref(), "tts") {
            pv.status.set(
                Note::new("read-aloud", NoteKind::Error, format!("read-aloud: {why}"))
                    .linked(problem_link(t.voice_resolved.as_ref(), &["tts"])),
            );
            return;
        }
        pv.play_stored(key, t.id, mid);
    };
    let attr_state = move || match state.get() {
        None => "idle",
        Some(PlayState::Loading) => "loading",
        Some(PlayState::Playing) => "playing",
    };
    let why_id = format!("speak-why-{key}");
    view! {
        <button
            type="button"
            class="msg-act msg-speak"
            class:on=move || state.get().is_some()
            class:off=move || resting.get()
            // The text goes to a fallback now (the hold, a benchmark run):
            // amber, as the composer's controls (review m6).
            class:warn=move || pv.tts_blocked.get()
            title=title
            aria-label=move || if state.get().is_some() { "Stop reading aloud" } else { "Read aloud" }
            aria-pressed=move || state.get().is_some().to_string()
            aria-disabled=move || resting.get().to_string()
            aria-describedby={
                let id = why_id.clone();
                move || resting.get().then(|| id.clone())
            }
            disabled=move || editing.get() || db_id.get().is_none()
            data-speak=attr_state
            data-speak-resting=move || resting.get().to_string()
            data-speak-pushed=move || facts.with(|f| f.as_ref().map(|f| f.pushed.to_string()).unwrap_or_default())
            data-speak-played=move || facts.with(|f| f.as_ref().and_then(|f| f.played).map(|p| p.to_string()).unwrap_or_default())
            data-speak-first=move || facts.with(|f| f.as_ref().and_then(|f| f.first_audio_ms).map(|p| p.to_string()).unwrap_or_default())
            on:click=click
        >
            {move || {
                let path = if state.get() == Some(PlayState::Playing) { ICON_STOP } else { ICON_SPEAKER };
                view! { <svg viewBox="0 0 16 16" aria-hidden="true"><path d=path></path></svg> }
            }}
        </button>
        {move || resting.get().then(|| view! {
            <span class="sr-only" id=why_id.clone()>{SPEAK_IN_VOICE_MODE}</span>
        })}
    }
    .into_any()
}

/// The composer's text box carries the dictation mark's facts as its
/// tooltip (§9.5: "dictation shows `asr_ms` on the inserted text's
/// tooltip"), and `data-dictated` for the probes (tracked).
pub(crate) fn composer_title(pv: Option<PageVoice>) -> impl Fn() -> Option<String> + Copy {
    move || pv.and_then(|pv| pv.dictation.mark.with(|m| m.as_ref().map(|m| m.title())))
}
