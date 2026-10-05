//! A spoken turn in the transcript (chat-voice design §3, §9.5): the mic
//! badge on a user turn that was dictated or said in voice mode, and on a
//! spoken reply the speaker badge, its timing line, the greyed rest that was
//! never heard behind a "not heard" marker, and its delivery cues as chips
//! (`cues.rs`).

use leptos::prelude::*;

use super::cues::cue_chips;
use super::spoken::{MsgVoice, VoiceTiming};
use crate::pages::chat::md_to_html;

const ICON_MIC: &str = "M8 1.8a2.2 2.2 0 0 1 2.2 2.2v4a2.2 2.2 0 0 1-4.4 0V4A2.2 2.2 0 0 1 8 1.8z M3.8 7.6a4.2 4.2 0 0 0 8.4 0 M8 11.8v2.4";
const ICON_SPEAKER: &str = "M2.5 6.2h2.4L8.3 3.4v9.2L4.9 9.8H2.5z M10.6 5.6a3.4 3.4 0 0 1 0 4.8";

/// A message's rendered text, its cues as chips when it is a reply spoken
/// in voice mode (`role` says which: §9.7.5).
pub(crate) fn spoken_html(html: String, voice: Option<&MsgVoice>, role: &str) -> String {
    if voice.is_some_and(|v| v.is_spoken_reply(role)) {
        cue_chips(&html)
    } else {
        html
    }
}

/// The mic badge under a spoken user turn — and, on a turn the chat model
/// heard that could not be transcribed, what its empty bubble stands for
/// (voice-audio-input design §5).
#[component]
pub(crate) fn MicBadge(voice: RwSignal<Option<MsgVoice>>) -> impl IntoView {
    move || {
        voice.get().filter(MsgVoice::is_spoken_user).map(|v| {
            let label = if v.via == super::spoken::VIA_DICTATION {
                "dictated"
            } else {
                "spoken"
            };
            let note = v.not_transcribed().then(|| {
                view! {
                    <span class="voice-note" data-not-transcribed="">
                        {super::spoken::NOT_TRANSCRIBED}
                    </span>
                }
            });
            view! {
                {note}
                <span class="voice-badge" title=v.mic_title() data-mic-badge=v.via.clone()>
                    <svg viewBox="0 0 16 16" aria-hidden="true"><path d=ICON_MIC></path></svg>
                    {label}
                </span>
            }
        })
    }
}

/// What a spoken reply carries under its text: the part never heard,
/// greyed behind its marker, then the speaker badge and the timing line,
/// which opens to every stage.
#[component]
pub(crate) fn SpokenReply(voice: RwSignal<Option<MsgVoice>>) -> impl IntoView {
    let unheard = Memo::new(move |_| {
        voice.with(|v| {
            v.as_ref()
                .and_then(|v| v.unheard.clone())
                .filter(|u| !u.trim().is_empty())
        })
    });
    let unheard_ref: NodeRef<leptos::html::Div> = NodeRef::new();
    Effect::new(move |_| {
        let Some(el) = unheard_ref.get() else { return };
        let html = unheard.get().map(|u| md_to_html(&u)).unwrap_or_default();
        el.set_inner_html(&cue_chips(&html));
    });
    view! {
        <Show when=move || unheard.with(Option::is_some)>
            <div class="msg-unheard" data-unheard="">
                <span class="unheard-mark" title="The voice was interrupted here: the rest was never heard, and the model does not see it">
                    "not heard"
                </span>
                <div class="md unheard-text" node_ref=unheard_ref></div>
            </div>
        </Show>
        {move || {
            voice
                .get()
                .filter(|v| v.is_spoken_reply("assistant"))
                .map(|v| {
                    let title = v.speaker_title();
                    let timing = v.timing.clone();
                    let note = v.note.clone();
                    view! {
                        <div class="voice-reply-line">
                            {note.map(|n| view! {
                                <span class="voice-note" data-voice-note="">{n}</span>
                            })}
                            <span class="voice-badge" title=title data-speaker-badge="">
                                <svg viewBox="0 0 16 16" aria-hidden="true"><path d=ICON_SPEAKER></path></svg>
                                "spoken"
                            </span>
                            {timing.map(|t| {
                                let details = t.details();
                                view! {
                                    <details class="voice-timing">
                                        <summary class="dim mono-sm">{timing_items(&t)}</summary>
                                        <ul class="dim mono-sm">
                                            {details.into_iter().map(|d| view! { <li>{d}</li> }).collect_view()}
                                        </ul>
                                    </details>
                                }
                            })}
                        </div>
                    }
                })
        }}
    }
}

/// A timing line's items (`VoiceTiming::line_items`): each one unbroken, a
/// name too long for the line ellipsized, the line wrapping only between
/// them.
pub(crate) fn timing_items(t: &VoiceTiming) -> impl IntoView {
    t.line_items()
        .into_iter()
        .map(|(sep, item)| {
            let title = item.clone();
            view! {
                {sep}
                <span class="ti" title=title>{item}</span>
            }
        })
        .collect_view()
}
