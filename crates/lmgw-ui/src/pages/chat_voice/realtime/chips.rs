//! The realtime panel's chips (chat-voice §9.4): the chat model, whether it
//! hears the turn or reads its transcript (voice-audio-input §2.3), the
//! speech-to-text, the text-to-speech with its voice, the echo mode, and the
//! thread's tools.
//!
//! - The model chip opens the thread's model picker (the page's own choice,
//!   saved on the thread as the header's picker saves it); the voice chip
//!   opens the text-to-speech and voice picker, which writes the thread's
//!   `voice`. Both apply from the next turn (§8.1: the session re-reads the
//!   thread before each one).
//! - A **cloud mark** on an alias off this machine; **amber** for one a
//!   block swaps now (the GPU hold, a benchmark run: `state.rs`'s `Block`,
//!   §9.7.6), and for the alias that actually served when a fallback
//!   answered — named on the chip.
//! - The thread's **tools**, and when it carries lmgw's own admin tools an
//!   amber chip that informs and never blocks: "admin tools: can change
//!   lmgw" (§8.1, the owner's ruling).

use leptos::html;
use leptos::prelude::*;
use serde_json::{json, Value};

use super::super::devices::EchoChip;
use super::super::state::{remote_note, under_block, Note, NoteKind};
use super::super::{apply_answer, StageResolved, ThreadVoice};
use super::Realtime;
use crate::widgets::voice_picker::{use_voices, VoiceInput};
use crate::widgets::{ModelPicker, Popover};

const ICON_CLOUD: &str =
    "M4.6 12.4h6.9a2.6 2.6 0 0 0 .3-5.2 3.6 3.6 0 0 0-6.9-1 2.9 2.9 0 0 0-.3 6.2z";

fn cloud() -> impl IntoView {
    view! { <svg class="rt-cloud" viewBox="0 0 16 16" aria-hidden="true"><path d=ICON_CLOUD></path></svg> }
}

/// The open thread's resolution of a stage (tracked).
fn stage(rt: Realtime, which: &'static str) -> Memo<Option<StageResolved>> {
    Memo::new(move |_| {
        rt.parts.current.with(|c| {
            let r = c.as_ref()?.voice_resolved.as_ref()?;
            Some(if which == "asr" {
                r.asr.clone()
            } else {
                r.tts.clone()
            })
        })
    })
}

#[component]
pub(super) fn Chips(rt: Realtime) -> impl IntoView {
    view! {
        <div class="rt-chips" data-rt-chips="">
            <ModelChip rt=rt/>
            <InputChip rt=rt/>
            <StageChip rt=rt which="asr"/>
            <VoiceChip rt=rt/>
            <EchoPick rt=rt/>
            <ToolsChips rt=rt/>
        </div>
    }
}

#[component]
fn ModelChip(rt: Realtime) -> impl IntoView {
    let open = RwSignal::new(false);
    let anchor: NodeRef<html::Button> = NodeRef::new();
    let alias = Memo::new(move |_| {
        rt.parts
            .current
            .with(|c| c.as_ref().map(|t| t.model_alias.clone()))
            .unwrap_or_default()
    });
    let served = Memo::new(move |_| rt.served.with(|s| s.chat.clone()));
    // Off this machine, as the catalog says.
    let catalog = crate::catalog::use_model_catalog();
    let remote = Memo::new(move |_| {
        let a = alias.get();
        catalog
            .entries
            .with(|e| e.iter().find(|m| m.id == a).is_some_and(|m| !m.local))
    });
    view! {
        <button
            type="button"
            node_ref=anchor
            class="chip rt-chip"
            class:warn=move || served.with(Option::is_some)
            data-rt-chip="model"
            title=move || {
                let mut t = format!(
                    "Chat model: {}{} — click to choose another for this conversation (from the next turn)",
                    alias.get(),
                    if remote.get() { " (remote)" } else { "" }
                );
                if let Some(s) = served.get() {
                    t.push_str(&format!(". The last turn was answered by {s}, its fallback"));
                }
                t
            }
            on:mousedown=|ev| ev.prevent_default()
            on:click=move |_| open.update(|o| *o = !*o)
        >
            <i class="rt-dot"></i>
            <span class="rt-chip-text">{move || alias.get()}</span>
            {move || remote.get().then(cloud)}
            {move || served.get().map(|s| view! { <span class="rt-served">{format!("→ {s}")}</span> })}
        </button>
        <Popover open=open anchor=anchor class="rt-pop" min_width=320>
            <div class="vd-section">
                <div class="vd-head">"Chat model of this conversation"</div>
                <ModelPicker value=rt.parts.model_sel tasks=&["chat"] recent_key="chat"/>
                <div class="vd-note dim">"Applies from the next turn."</div>
            </div>
        </Popover>
    }
}

/// Whether the chat model hears the turn or reads its transcript, from the
/// thread's resolution and then what the session said of each response
/// (`lmgw.chat.input`; the tooltip says why) — and, as the STT and TTS chips
/// do, the live block: under the GPU hold or a benchmark run the chip shows
/// the fallback's own verdict, amber (WP1 review M2; changed 2026-10-06:
/// a fallback that takes audio hears you).
#[component]
fn InputChip(rt: Realtime) -> impl IntoView {
    let pv = rt.pv;
    let input = Memo::new(move |_| {
        let resolved = rt.parts.current.with(|c| {
            c.as_ref()
                .and_then(|t| t.voice_resolved.as_ref())
                .and_then(|r| r.audio_input.clone())
        });
        let said = match (resolved, rt.said_input.get()) {
            (Some(mut a), Some((path, why))) => {
                a.path = path;
                a.why = why;
                Some(a)
            }
            (a, _) => a,
        };
        said.map(|a| a.blocked(pv.block.get()).unwrap_or(a))
    });
    move || {
        input.get().map(|a| {
            view! {
                <span
                    class="chip rt-chip"
                    class:warn=a.block.is_some()
                    data-rt-chip="input"
                    data-audio-path=a.path.clone()
                    title=a.chip_title()
                    tabindex="0"
                >
                    <span class="rt-chip-k">"INPUT"</span>
                    <span class="rt-chip-text">{a.chip_text()}</span>
                </span>
            }
        })
    }
}

/// The speech-to-text chip.
#[component]
fn StageChip(rt: Realtime, which: &'static str) -> impl IntoView {
    let s = stage(rt, which);
    let pv = rt.pv;
    let served = Memo::new(move |_| {
        rt.served.with(|x| {
            if which == "asr" {
                x.asr.clone()
            } else {
                x.tts.clone()
            }
        })
    });
    let blocked = Memo::new(move |_| {
        let b = pv.block.get();
        s.with(|s| s.as_ref().is_some_and(|s| b.blocks(s)))
    });
    let remote = Memo::new(move |_| s.with(|s| s.as_ref().is_some_and(|s| s.local == Some(false))));
    let title = move || {
        let Some(st) = s.get() else {
            return "speech-to-text: not resolved yet".to_string();
        };
        let mut t = format!(
            "Speech-to-text: {}",
            st.alias.clone().unwrap_or_else(|| "none set".into())
        );
        if let Some(n) = under_block(&st, pv.block.get()).or_else(|| remote_note(&st)) {
            t.push_str(&format!(" — {n}"));
        }
        if let Some(by) = served.get() {
            t.push_str(&format!(" — the last turn was heard by {by}, its fallback"));
        }
        t
    };
    view! {
        <span
            class="chip rt-chip"
            class:warn=move || blocked.get() || served.with(Option::is_some)
            data-rt-chip=which
            title=title
            tabindex="0"
        >
            <span class="rt-chip-k">"STT"</span>
            <span class="rt-chip-text">{move || s.get().and_then(|s| s.alias).unwrap_or_else(|| "—".into())}</span>
            {move || remote.get().then(cloud)}
            {move || served.get().map(|b| view! { <span class="rt-served">{format!("→ {b}")}</span> })}
        </span>
    }
}

/// The text-to-speech and voice chip, and its picker.
#[component]
fn VoiceChip(rt: Realtime) -> impl IntoView {
    let s = stage(rt, "tts");
    let pv = rt.pv;
    let open = RwSignal::new(false);
    let anchor: NodeRef<html::Button> = NodeRef::new();
    let voice = Memo::new(move |_| {
        rt.parts.current.with(|c| {
            c.as_ref()
                .and_then(|t| t.voice_resolved.as_ref())
                .and_then(|r| r.voice.name.clone().or_else(|| r.voice.inherits.clone()))
        })
    });
    let served = Memo::new(move |_| rt.served.with(|x| x.tts.clone()));
    let blocked = Memo::new(move |_| {
        let b = pv.block.get();
        s.with(|s| s.as_ref().is_some_and(|s| b.blocks(s)))
    });
    let remote = Memo::new(move |_| s.with(|s| s.as_ref().is_some_and(|s| s.local == Some(false))));
    let title = move || {
        let Some(st) = s.get() else {
            return "text-to-speech: not resolved yet".to_string();
        };
        let mut t = format!(
            "Text-to-speech: {}",
            st.alias.clone().unwrap_or_else(|| "none set".into())
        );
        if let Some(v) = voice.get() {
            t.push_str(&format!(", voice {v}"));
        }
        if let Some(n) = under_block(&st, pv.block.get()).or_else(|| remote_note(&st)) {
            t.push_str(&format!(" — {n}"));
        }
        if let Some(by) = served.get() {
            t.push_str(&format!(
                " — the last reply was spoken by {by}, its fallback"
            ));
        }
        t.push_str(". Click to choose another for this conversation (from the next turn).");
        t
    };
    view! {
        <button
            type="button"
            node_ref=anchor
            class="chip rt-chip"
            class:warn=move || blocked.get() || served.with(Option::is_some)
            data-rt-chip="tts"
            title=title
            on:mousedown=|ev| ev.prevent_default()
            on:click=move |_| open.update(|o| *o = !*o)
        >
            <span class="rt-chip-k">"TTS"</span>
            <span class="rt-chip-text">
                {move || s.get().and_then(|s| s.alias).unwrap_or_else(|| "—".into())}
                {move || voice.get().map(|v| format!(" · {v}"))}
            </span>
            {move || remote.get().then(cloud)}
            {move || served.get().map(|b| view! { <span class="rt-served">{format!("→ {b}")}</span> })}
        </button>
        <Popover open=open anchor=anchor class="rt-pop" min_width=340>
            <VoicePicker rt=rt open=open/>
        </Popover>
    }
}

/// The thread's text-to-speech and voice, written as the thread's whole
/// `voice` (the seed left to the server, §2.2); the settings drawer's draft
/// is kept in step, so the write is no unsaved change there.
#[component]
fn VoicePicker(rt: Realtime, open: RwSignal<bool>) -> impl IntoView {
    let current = rt.parts.current;
    let own = current.get_untracked().map(|t| t.voice).unwrap_or_default();
    let tts = RwSignal::new(own.tts_alias.clone().unwrap_or_default());
    let name = RwSignal::new(own.voice.clone().unwrap_or_default());
    let busy = RwSignal::new(false);
    let tts_now = Signal::derive(move || {
        let t = tts.get();
        if !t.trim().is_empty() {
            return t;
        }
        current
            .with(|c| {
                c.as_ref()
                    .and_then(|t| t.voice_resolved.as_ref())
                    .and_then(|r| r.tts.inherited.clone())
            })
            .unwrap_or_default()
    });
    let voices = use_voices(tts_now);
    let save = move |_| {
        let Some(t) = current.get_untracked() else {
            return;
        };
        let pick = |s: String| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        let voice = ThreadVoice {
            tts_alias: pick(tts.get_untracked()),
            voice: pick(name.get_untracked()),
            seed: None,
            ..t.voice.clone()
        };
        let id = t.id;
        busy.set(true);
        let draft = rt.parts.draft;
        let status = rt.status;
        let refresh = rt.parts.refresh;
        leptos::task::spawn_local(async move {
            let res = crate::api::post::<Value, _>(
                format!("/chat/api/threads/{id}/settings"),
                &json!({ "voice": voice }),
            )
            .await;
            busy.try_set(false);
            match res {
                Ok(answer) => {
                    current.try_update(|c| {
                        if let Some(t) = c.as_mut().filter(|t| t.id == id) {
                            apply_answer(t, &answer);
                        }
                    });
                    draft
                        .tts
                        .try_set(voice.tts_alias.clone().unwrap_or_default());
                    draft.voice.try_set(voice.voice.clone().unwrap_or_default());
                    refresh.run(());
                    open.try_set(false);
                }
                Err(e) => status.set(Note::new(
                    "voice",
                    NoteKind::Error,
                    format!("the voice could not be saved: {e}"),
                )),
            }
        });
    };
    view! {
        <div class="vd-section rt-voice-pick">
            <div class="vd-head">"Text to speech of this conversation"</div>
            <ModelPicker
                value=tts
                tasks=lmgw_api_types::realtime::SPEECH_TASKS
                empty_label="inherit".to_string()
            />
            <div class="vd-head">"Voice"</div>
            <VoiceInput
                value=name
                on_input=Callback::new(move |v: String| name.set(v))
                list=voices
                list_id="rt-voice-list".to_string()
            />
            <div class="rt-pick-row">
                <span class="vd-note dim">"Applies from the next reply."</span>
                <button type="button" class="btn primary sm" disabled=move || busy.get() on:click=save>
                    "Save"
                </button>
            </div>
        </div>
    }
}

/// The window's echo mode, amber with §12.2's warning, and the four modes
/// in a popover (§9.4). A change reopens the microphone with the mode's
/// constraints and sets half duplex for `none` (§12.1).
#[component]
fn EchoPick(rt: Realtime) -> impl IntoView {
    let dev = rt.dev;
    let open = RwSignal::new(false);
    let anchor: NodeRef<html::Button> = NodeRef::new();
    let warning = Memo::new(move |_| dev.warning());
    let in_app = super::super::audio::shell::in_shell();
    view! {
        <button
            type="button"
            node_ref=anchor
            class="chip rt-chip"
            class:warn=move || warning.with(Option::is_some)
            data-rt-chip="echo"
            data-echo=move || dev.echo.get().key()
            title=move || {
                let mut t = format!(
                    "Echo: {} — {}",
                    dev.echo.get().label(in_app),
                    dev.echo.get().hint()
                );
                if let Some(w) = warning.get() {
                    t.push_str(&format!(" Warning: {w}."));
                }
                t
            }
            aria-haspopup="dialog"
            aria-expanded=move || open.get().to_string()
            on:mousedown=|ev| ev.prevent_default()
            on:click=move |_| open.update(|o| *o = !*o)
        >
            <span class="rt-chip-k">"ECHO"</span>
            <span class="rt-chip-text">{move || dev.echo.get().label(in_app)}</span>
        </button>
        <Popover open=open anchor=anchor class="rt-pop" min_width=340>
            <div class="vd-section">
                <div class="vd-head">"Echo, in this window"</div>
                <EchoChip dev=dev expanded=true/>
            </div>
        </Popover>
    }
}

/// The thread's tools, and lmgw's own admin tools when it carries them.
#[component]
fn ToolsChips(rt: Realtime) -> impl IntoView {
    let names = Memo::new(move |_| {
        rt.parts.current.with(|c| {
            c.as_ref()
                .map(|t| {
                    t.mcp_tools
                        .iter()
                        .map(|m| m.server_label.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    // The session's word once it spoke (`lmgw.chat.thread` refreshes it),
    // the thread's resolution before.
    let admin = Memo::new(move |_| {
        rt.admin_tools.get().unwrap_or_else(|| {
            rt.parts.current.with(|c| {
                c.as_ref()
                    .and_then(|t| t.voice_resolved.as_ref())
                    .is_some_and(|r| r.realtime.admin_tools)
            })
        })
    });
    view! {
        {move || {
            let n = names.get();
            (!n.is_empty()).then(|| view! {
                <span class="chip rt-chip" data-rt-chip="tools" tabindex="0"
                    title=format!("Tools this conversation's turns may call: {}", n.join(", "))>
                    <span class="rt-chip-k">"TOOLS"</span>
                    <span class="rt-chip-text">{n.join(", ")}</span>
                </span>
            })
        }}
        <Show when=move || admin.get()>
            <span class="chip rt-chip warn rt-admin" data-rt-chip="admin" tabindex="0"
                title="This conversation carries lmgw's own admin tools, and self-admin is on: a spoken turn can change lmgw's settings, models and containers. Voice mode informs, it does not block (the tools are your choice).">
                "admin tools: can change lmgw"
            </span>
        </Show>
    }
}
