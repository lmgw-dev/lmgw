//! The voice menu beside the composer's microphone (chat-voice design §6.5,
//! §2.4, WP7 review M3): one button whose popover holds "read replies aloud"
//! for the open thread and the window's audio devices.
//!
//! The button shows what matters without opening it: the speaker in the
//! accent colour while the thread reads its replies aloud, amber when
//! something is off — lmgw's playback or output failed, the chosen output is
//! gone, §12.2's echo warning, or the replies read aloud would go to a
//! fallback now (the hold, a benchmark run). Its title says which.

use leptos::html;
use leptos::prelude::*;
use serde_json::{json, Value};

use super::super::devices::{use_voice_devices, DevicesPanel};
use super::super::page::{use_page_voice, PageVoice};
use super::super::state::{blocker, problem_link, Note, NoteKind};
use super::super::{source_label, ThreadVoice, VoiceDraft};
use super::ICON_SPEAKER;
use crate::widgets::Popover;

/// The menu's button and popover.
#[component]
pub(super) fn VoiceMenu(draft: VoiceDraft, on_saved: Callback<()>) -> impl IntoView {
    let Some(pv) = use_page_voice() else {
        return ().into_any();
    };
    let dev = use_voice_devices();
    let open = RwSignal::new(false);
    let anchor: NodeRef<html::Button> = NodeRef::new();
    // A chosen output is what the warnings need the list for (gone, or not
    // the default): read it once, quietly, so the button can show them
    // before the popover was ever opened. With none chosen there is nothing
    // to read (in the app each read is a pw-dump).
    if dev.output.get_untracked().is_some() {
        dev.refresh();
    }
    let alert = Memo::new(move |_| dev.alert());
    let reading = Memo::new(move |_| {
        pv.current.with(|c| {
            c.as_ref()
                .and_then(|t| t.voice_resolved.as_ref())
                .is_some_and(|r| r.read_aloud.value)
        })
    });
    let (tts_note, tts_blocked) = (pv.tts_note, pv.tts_blocked);
    // Replies read aloud now go to a fallback: worth the amber too.
    let read_warn = Memo::new(move |_| {
        (reading.get() && tts_blocked.get())
            .then(|| tts_note.get())
            .flatten()
            .map(|n| format!("read replies aloud: {n}"))
    });
    let title = move || {
        let mut t = format!(
            "Voice: read replies aloud ({}), audio devices",
            if reading.get() { "on" } else { "off" }
        );
        for w in [alert.get(), read_warn.get()].into_iter().flatten() {
            t.push_str(&format!(" — {w}"));
        }
        t
    };
    let warning = move || {
        [alert.get(), read_warn.get()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("; ")
    };
    view! {
        <button
            type="button"
            node_ref=anchor
            class="btn ghost composer-attach composer-voice-menu"
            class:on=move || reading.get()
            class:warn=move || alert.with(Option::is_some) || read_warn.with(Option::is_some)
            title=title
            aria-label="Voice: read replies aloud, audio devices"
            aria-description=warning
            aria-haspopup="dialog"
            aria-expanded=move || open.get().to_string()
            data-voice-devices-btn=""
            data-voice-alert=move || alert.get().unwrap_or_default()
            data-read-aloud=move || if reading.get() { "on" } else { "off" }
            on:mousedown=|ev| ev.prevent_default()
            on:click=move |_| open.update(|o| *o = !*o)
        >
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d=ICON_SPEAKER></path></svg>
            <svg class="vm-caret" viewBox="0 0 8 8" aria-hidden="true"><path d="M1.5 5.2L4 2.8l2.5 2.4"></path></svg>
        </button>
        <Popover open=open anchor=anchor class="voice-dev-pop" min_width=380>
            <ReadAloudRow pv=pv draft=draft on_saved=on_saved/>
            <DevicesPanel dev=dev/>
        </Popover>
    }
    .into_any()
}

/// "Read replies aloud" for the open thread (§6.5): shows the resolved
/// value and where it comes from, and writes `voice.read_aloud` on the
/// thread. `draft` is the settings drawer's, kept in step so the drawer
/// does not show the switch's write as an unsaved change.
#[component]
fn ReadAloudRow(pv: PageVoice, draft: VoiceDraft, on_saved: Callback<()>) -> impl IntoView {
    let current = pv.current;
    let (note, warn) = (pv.tts_note, pv.tts_blocked);
    let busy = RwSignal::new(false);
    let resolved = Memo::new(move |_| {
        current.with(|c| {
            c.as_ref().and_then(|t| t.voice_resolved.as_ref()).map(|r| {
                (
                    r.read_aloud.value,
                    r.read_aloud.source.clone(),
                    r.tts.alias.clone(),
                )
            })
        })
    });
    let on = Memo::new(move |_| resolved.with(|r| r.as_ref().is_some_and(|r| r.0)));
    let facts = move || {
        let Some((_, source, tts)) = resolved.get() else {
            return String::new();
        };
        let mut t = format!("from {}", source_label(source.as_deref()));
        if let Some(a) = tts {
            t.push_str(&format!(" · text-to-speech: {a}"));
        }
        t
    };
    let toggle = move |_| {
        let Some(t) = current.get_untracked() else {
            return;
        };
        if busy.get_untracked() {
            return;
        }
        let want = !on.get_untracked();
        if want {
            if let Some(why) = blocker(t.voice_resolved.as_ref(), "tts") {
                pv.status.set(
                    Note::new("read-aloud", NoteKind::Error, format!("read-aloud: {why}"))
                        .linked(problem_link(t.voice_resolved.as_ref(), &["tts"])),
                );
            }
        } else {
            // Turning it off also ends what reads now.
            pv.read_aloud.stop();
        }
        // The whole object, as stored, with the seed left to the server.
        let voice = ThreadVoice {
            read_aloud: Some(want),
            seed: None,
            ..t.voice.clone()
        };
        let id = t.id;
        busy.set(true);
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
                            super::super::apply_answer(t, &answer);
                        }
                    });
                    draft
                        .read_aloud
                        .try_set(if want { "on" } else { "off" }.to_string());
                    on_saved.run(());
                }
                Err(e) => pv.status.set(Note::new(
                    "read-aloud",
                    NoteKind::Error,
                    format!("read replies aloud could not be saved: {e}"),
                )),
            }
        });
    };
    view! {
        <div class="vd-section vm-read">
            <div class="vd-head">"Replies"</div>
            <button
                type="button"
                class="vm-switch"
                role="switch"
                aria-checked=move || on.get().to_string()
                disabled=move || busy.get()
                title=move || {
                    let (value, source) = resolved
                        .with(|r| r.as_ref().map(|r| (r.0, r.1.clone())))
                        .unwrap_or_default();
                    format!(
                        "Read replies aloud: {} ({}). Click to turn it {} for this conversation.",
                        if value { "on" } else { "off" },
                        source_label(source.as_deref()),
                        if value { "off" } else { "on" },
                    )
                }
                data-read-aloud-toggle=move || if on.get() { "on" } else { "off" }
                on:click=toggle
            >
                <span class="vm-track" aria-hidden="true"><i></i></span>
                "Read replies aloud"
            </button>
            <div class="vd-note dim">{facts}</div>
            {move || note.get().map(|n| view! {
                <div class="vd-note" class:warn=move || warn.get()>{n}</div>
            })}
        </div>
    }
}
