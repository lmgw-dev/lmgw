//! The composer's voice status line (chat-voice design §4.3, §5): what
//! dictation is doing (recording with its timer and level, transcribing),
//! the composer's dictated mark, a read-aloud of a streaming reply with its
//! "stop speaking", and the notes the voice models leave — loading, a
//! fallback that answers, the GPU hold or a benchmark run (amber, never an
//! error), and errors said where the feature was pressed.
//!
//! Screen readers hear it through a polite live region that is always
//! mounted and says state changes only — a state, a note, a block line as it
//! comes — never the ticking clock or the level meter (WP7 review m8).

use leptos::prelude::*;

use super::super::chat::ChatThread;
use super::dictation::DictState;
use super::page::PageVoice;
use super::state::{cpu_stages, note_of, under_block, ModelState, Note, NoteKind};

/// The key of the note a turn's `reasoning_note` sets ([`VoiceStatus::reasoning`]).
pub(crate) const REASONING_KEY: &str = "reasoning";

/// The notes, newest last; one per key.
#[derive(Clone, Copy)]
pub(crate) struct VoiceStatus {
    notes: RwSignal<Vec<Note>>,
    current: RwSignal<Option<ChatThread>>,
}

impl VoiceStatus {
    pub(crate) fn new(current: RwSignal<Option<ChatThread>>) -> Self {
        Self {
            notes: RwSignal::new(Vec::new()),
            current,
        }
    }

    /// Say `note`, in place of the one with its key.
    pub(crate) fn set(&self, note: Note) {
        self.notes.try_update(|v| {
            v.retain(|n| n.key != note.key);
            v.push(note);
        });
    }

    pub(crate) fn clear(&self, key: &str) {
        if self
            .notes
            .try_with_untracked(|v| v.iter().any(|n| n.key == key))
            .unwrap_or(false)
        {
            self.notes.try_update(|v| v.retain(|n| n.key != key));
        }
    }

    /// Drop the key's note if it says something is under way: the thing
    /// it waited for ended without a word.
    pub(crate) fn clear_busy(&self, key: &str) {
        if self
            .notes
            .try_with_untracked(|v| v.iter().any(|n| n.key == key && n.kind == NoteKind::Busy))
            .unwrap_or(false)
        {
            self.notes
                .try_update(|v| v.retain(|n| !(n.key == key && n.kind == NoteKind::Busy)));
        }
    }

    /// A new press or send: passing notes go; warnings, the hold and errors
    /// stay until they are dismissed or said again.
    pub(crate) fn clear_info(&self) {
        if self
            .notes
            .try_with_untracked(|v| v.iter().any(|n| n.kind == NoteKind::Info))
            .unwrap_or(false)
        {
            self.notes
                .try_update(|v| v.retain(|n| n.kind != NoteKind::Info));
        }
    }

    /// A chat turn's `done`: its `reasoning_note` — the model reasoned
    /// although off was asked, a voice turn's own off included
    /// (model-capabilities design §5.6) — said once, in place of the last
    /// turn's; none clears it.
    pub(crate) fn reasoning(&self, done: &serde_json::Value) {
        match done["reasoning_note"].as_str().filter(|n| !n.is_empty()) {
            Some(n) => self.set(Note::new(REASONING_KEY, NoteKind::Info, n)),
            None => self.clear(REASONING_KEY),
        }
    }

    /// A `state` frame of a warm, a turn or a read-aloud.
    pub(crate) fn state_frame(&self, data: &serde_json::Value) {
        let Ok(s) = serde_json::from_value::<ModelState>(data.clone()) else {
            return;
        };
        let cpu = self
            .current
            .try_with_untracked(|c| cpu_stages(c.as_ref().and_then(|t| t.voice_resolved.as_ref())))
            .unwrap_or_default();
        match note_of(&s, &cpu) {
            Some(n) => self.set(n),
            None => self.clear(&s.stage),
        }
    }

    pub(crate) fn notes(&self) -> RwSignal<Vec<Note>> {
        self.notes
    }

    /// The notes as the line shows them (tracked): one hold chip however
    /// many stages the hold holds ([`one_hold`]).
    pub(crate) fn shown(&self) -> Vec<Note> {
        self.notes.with(|v| one_hold(v))
    }

    /// The hold chip's ✕: every hold note goes, not only the one shown.
    pub(crate) fn clear_holds(&self) {
        if self
            .notes
            .try_with_untracked(|v| v.iter().any(|n| n.kind == NoteKind::Hold))
            .unwrap_or(false)
        {
            self.notes
                .try_update(|v| v.retain(|n| n.kind != NoteKind::Hold));
        }
    }

    /// Dismiss the note `n` (its ✕).
    pub(crate) fn dismiss(&self, n: &Note) {
        if n.kind == NoteKind::Hold {
            self.clear_holds();
        } else {
            self.clear(&n.key);
        }
    }
}

/// One amber chip per hold (WP11 UI review m6): the GPU hold, or a
/// benchmark run, holds every stage it holds at once — the connect warm says
/// `held` for the chat model and a GPU voice, a press's warm and its refusal
/// for the ASR — and the line shows the newest of those notes, the most
/// specific (a refusal follows its stage's `held`). The others stay, and
/// come back if it is cleared by its stage.
pub(crate) fn one_hold(notes: &[Note]) -> Vec<Note> {
    let last = notes.iter().rposition(|n| n.kind == NoteKind::Hold);
    notes
        .iter()
        .enumerate()
        .filter(|(i, n)| n.kind != NoteKind::Hold || Some(*i) == last)
        .map(|(_, n)| n.clone())
        .collect()
}

/// `m:ss` of a recording.
fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

const ICON_MIC: &str = "M8 1.8a2.2 2.2 0 0 1 2.2 2.2v4a2.2 2.2 0 0 1-4.4 0V4A2.2 2.2 0 0 1 8 1.8z M3.8 7.6a4.2 4.2 0 0 0 8.4 0 M8 11.8v2.4";

/// The line above the composer, and its live region.
#[component]
pub(crate) fn VoiceStatusLine(pv: PageVoice) -> impl IntoView {
    let d = pv.dictation;
    let ra = pv.read_aloud;
    let notes = pv.status.notes();
    // Where the audio and the text go while the GPU hold or a benchmark run
    // swaps them, said before anything is recorded or spoken (§2.3): the
    // microphone's, and the read-aloud's while the thread reads its replies
    // aloud.
    let line = move |stage: &'static str, what: &'static str| {
        Memo::new(move |_| {
            let b = pv.block.get();
            pv.current.with(|c| {
                let r = c.as_ref()?.voice_resolved.as_ref()?;
                if stage == "tts" && !r.read_aloud.value {
                    return None;
                }
                let s = if stage == "asr" { &r.asr } else { &r.tts };
                b.blocks(s)
                    .then(|| under_block(s, b))
                    .flatten()
                    .map(|l| format!("{what}: {l}"))
            })
        })
    };
    let asr_hold = line("asr", "dictation");
    let tts_hold = line("tts", "read-aloud");
    let recording = Memo::new(move |_| d.state.get().shown());
    let marked = Memo::new(move |_| d.mark.with(Option::is_some));
    let reading = Memo::new(move |_| ra.live_playing());
    let shown = Memo::new(move |_| {
        recording.get()
            || marked.get()
            || reading.get()
            || notes.with(|n| !n.is_empty())
            || asr_hold.with(Option::is_some)
            || tts_hold.with(Option::is_some)
    });
    // What the live region says: each line once, as it comes.
    let announce = Memo::new(move |_| {
        let mut out = Vec::new();
        match d.state.get() {
            DictState::Opening => out.push("opening the microphone".to_string()),
            DictState::Recording => out.push("recording".to_string()),
            DictState::Finishing | DictState::Transcribing => out.push("transcribing".to_string()),
            DictState::Idle | DictState::Arming => {}
        }
        out.extend(asr_hold.get());
        out.extend(tts_hold.get());
        if marked.get() && !recording.get() {
            out.push("the message holds dictated text".to_string());
        }
        if reading.get() {
            out.push("reading the reply aloud".to_string());
        }
        out.extend(pv.status.shown().into_iter().map(|n| n.text));
        // One line per text: the region's rows are keyed by it.
        let mut seen = std::collections::HashSet::new();
        out.retain(|l| seen.insert(l.clone()));
        out
    });
    view! {
        <div class="sr-only" aria-live="polite" data-voice-live="">
            <For each=move || announce.get() key=|l| l.clone() let:l>
                <p>{l}</p>
            </For>
        </div>
        <Show when=move || shown.get()>
            <div
                class="voice-status"
                data-voice-status=""
                data-dictation=move || d.state.get().key()
                data-dictated=move || marked.get().to_string()
                data-read-aloud=move || ra.state_key()
                data-read-aloud-first=move || ra.first_key()
            >
                <Show when=move || recording.get()>
                    <span class="vs-rec" class:on=move || d.state.get() == DictState::Recording>
                        <i class="vs-dot"></i>
                        {move || match d.state.get() {
                            DictState::Opening => "opening the microphone…".to_string(),
                            DictState::Recording => format!("recording {}", clock(d.elapsed_ms.get())),
                            DictState::Finishing => "finishing…".to_string(),
                            DictState::Transcribing => format!(
                                "transcribing {} of speech…",
                                clock(d.elapsed_ms.get())
                            ),
                            DictState::Idle | DictState::Arming => String::new(),
                        }}
                    </span>
                    <Show when=move || d.state.get() == DictState::Recording>
                        <span class="vu vs-vu" role="meter" aria-label="microphone level"
                            aria-valuemin="0" aria-valuemax="1"
                            aria-valuenow=move || format!("{:.2}", d.level.get())>
                            <i style=move || format!("width:{:.1}%", d.level.get() * 100.0)></i>
                        </span>
                        <span class="dim vs-hint">
                            {move || if d.holding() {
                                "release to transcribe · Esc discards"
                            } else {
                                "click the microphone to transcribe · Esc discards"
                            }}
                        </span>
                    </Show>
                </Show>
                {move || asr_hold.get().map(|l| view! { <span class="vs-note warn" data-hold-line="asr">{l}</span> })}
                {move || tts_hold.get().map(|l| view! { <span class="vs-note warn" data-hold-line="tts">{l}</span> })}
                <Show when=move || marked.get() && !recording.get()>
                    <span class="vs-mark" title=move || d.mark.with(|m| m.as_ref().map(|m| m.title()).unwrap_or_default())>
                        <svg viewBox="0 0 16 16" aria-hidden="true"><path d=ICON_MIC></path></svg>
                        "dictated"
                        <button
                            type="button"
                            class="vs-x"
                            title="Send it as typed text instead (drops the dictation mark)"
                            aria-label="Drop the dictation mark"
                            on:click=move |_| d.mark.set(None)
                        >
                            "✕"
                        </button>
                    </span>
                </Show>
                <Show when=move || reading.get()>
                    <span class="vs-note busy">
                        "reading the reply aloud"
                        <button type="button" class="link-btn" data-stop-speaking=""
                            title="Stop the voice; the text goes on"
                            on:click=move |_| ra.stop()>
                            "Stop speaking"
                        </button>
                    </span>
                </Show>
                <For each=move || pv.status.shown() key=|n| (n.key.clone(), n.text.clone()) let:n>
                    {
                        let dismissable = matches!(n.kind, NoteKind::Error | NoteKind::Warn | NoteKind::Hold);
                        view! {
                            <span class=format!("vs-note {}", n.kind.class()) data-note=n.key.clone()>
                                {n.text.clone()}
                                {n.link.map(|(href, label)| view! {
                                    " " <a class="link-btn" href=href data-note-link="">{label}</a>
                                })}
                                {dismissable.then(|| {
                                    let n = n.clone();
                                    view! {
                                        <button type="button" class="vs-x" aria-label="Dismiss"
                                            on:click=move |_| pv.status.dismiss(&n)>
                                            "✕"
                                        </button>
                                    }
                                })}
                            </span>
                        }
                    }
                </For>
            </div>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::{clock, one_hold};
    use crate::pages::chat_voice::state::{Note, NoteKind};

    #[test]
    fn a_hold_is_one_chip_the_newest() {
        let notes = vec![
            Note::new("chat", NoteKind::Hold, "chat held"),
            Note::new("asr", NoteKind::Busy, "loading"),
            Note::new("tts", NoteKind::Hold, "tts held"),
            Note::new("turn", NoteKind::Error, "e"),
        ];
        let shown: Vec<String> = one_hold(&notes).into_iter().map(|n| n.key).collect();
        assert_eq!(shown, ["asr", "tts", "turn"]);
        let none = vec![Note::new("asr", NoteKind::Info, "i")];
        assert_eq!(one_hold(&none), none);
    }

    #[test]
    fn a_turn_that_reasoned_although_off_was_asked_is_said_once() {
        use leptos::prelude::*;
        let status = super::VoiceStatus::new(RwSignal::new(None));
        let note = "gpt cannot switch reasoning off; it reasons at its lowest level (minimal)";
        let said = || {
            status
                .notes()
                .get_untracked()
                .iter()
                .filter(|n| n.key == super::REASONING_KEY)
                .map(|n| (n.kind, n.text.clone()))
                .collect::<Vec<_>>()
        };
        status.reasoning(&serde_json::json!({"reasoning_note": note}));
        status.reasoning(&serde_json::json!({"reasoning_note": note}));
        assert_eq!(said(), [(NoteKind::Info, note.to_string())]);
        // A turn that took the off clears it.
        status.reasoning(&serde_json::json!({"reasoning_note": null}));
        assert!(said().is_empty());
    }

    #[test]
    fn the_recording_clock_reads_minutes_and_seconds() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(4_900), "0:04");
        assert_eq!(clock(65_000), "1:05");
    }
}
