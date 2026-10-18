//! The TTS panel's built-in voice list: the options of its picker and the
//! notes under it. The list is lmgw's catalog for the selected model, read
//! as soon as the model is picked (`Lab::load_voices`); library clips are not
//! in this picker, they have their own ("Reference clip").

use leptos::prelude::*;
use lmgw_api_types::audio_lab::{LabVoice, LabVoiceKind, LabVoices, LabVoicesSource};

use super::{Lab, VoiceList};

/// The picker's options: an empty value (the model's default voice), then
/// the model's own voices. The default one is marked, never preselected.
/// `current` is the form's value: a library clip it names (a preset or "Open
/// in form" can set one, and a clip's name is a valid `voice`) stays in the
/// list, so the field does not read "(not in list)".
pub(super) fn voice_options(v: &LabVoices, current: &str) -> Vec<(String, String)> {
    std::iter::once((String::new(), "(model default)".to_string()))
        .chain(
            v.voices
                .iter()
                .filter(|e| e.kind != LabVoiceKind::Library || e.id == current)
                .map(|e| (e.id.clone(), voice_label(e, v))),
        )
        .collect()
}

fn voice_label(e: &LabVoice, v: &LabVoices) -> String {
    let kind = match e.kind {
        LabVoiceKind::Preset => " · preset",
        LabVoiceKind::Embedding => " · embedding",
        LabVoiceKind::Library => " · library clip",
        LabVoiceKind::Engine => match v.source {
            LabVoicesSource::Upstream => " · from the provider",
            LabVoicesSource::Catalog => " · from the running model",
        },
        LabVoiceKind::Native => "",
    };
    let star = if v.default.as_deref() == Some(e.id.as_str()) {
        " (default)"
    } else {
        ""
    };
    format!("{}{kind}{star}", e.id)
}

/// Whether the model has no built-in voice of its own to pick.
pub(super) fn no_built_in(v: &LabVoices) -> bool {
    v.voices.iter().all(|e| e.kind == LabVoiceKind::Library)
}

/// The list's state under the picker: loading, or why it failed.
#[component]
pub(super) fn VoiceListNote(lab: Lab) -> impl IntoView {
    move || match lab.voice_list.get() {
        VoiceList::Loading => view! { <p class="dim mini-note">"Loading voices…"</p> }.into_any(),
        VoiceList::Failed(e) => view! {
            <div class="notice err" style="margin-top:8px">
                {format!("⚠ voice list: {e}")}
                <button class="btn ghost sm" on:click=move |_| lab.load_voices()>
                    "Retry"
                </button>
            </div>
        }
        .into_any(),
        VoiceList::Idle | VoiceList::Loaded => ().into_any(),
    }
}

/// The model cannot take the GPU now: the list is shown, but a request now is
/// refused or answered by the fallback. Without a fallback that is the hold
/// or a benchmark's lease; a fallback also stands in when another program
/// holds the card's memory, so that line does not name the hold.
#[component]
pub(super) fn HeldNote(lab: Lab) -> impl IntoView {
    move || {
        let (held, fallback) = lab.voices.with(|v| (v.held, v.fallback.clone()));
        held.then(|| {
            let note = match fallback {
                Some(fb) => format!(
                    "This model cannot take the GPU right now, so a request now is answered by {fb}."
                ),
                None => "The GPU hold (or a benchmark) is on, so a request now is refused.".to_string(),
            };
            view! { <p class="dim mini-note">{note}</p> }
        })
    }
}

/// Voices the model's package names but lacks on disk.
#[component]
pub(super) fn MissingVoices(lab: Lab) -> impl IntoView {
    move || {
        let missing = lab.voices.with(|v| v.missing.clone());
        (!missing.is_empty()).then(|| {
            view! {
                <p class="dim mini-note">
                    "Not on disk yet: " <span class="mono-sm">{missing.join(", ")}</span>
                    ". The Audio catalog's Download on this model's package fetches them."
                </p>
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice(id: &str, kind: LabVoiceKind) -> LabVoice {
        LabVoice {
            id: id.into(),
            kind,
            send: "voice".into(),
            transcript: None,
        }
    }

    #[test]
    fn the_picker_lists_own_voices_marks_the_default_and_leaves_clips_out() {
        let v = LabVoices {
            voices: vec![
                voice("Aria", LabVoiceKind::Native),
                voice("me", LabVoiceKind::Library),
                voice("narrator", LabVoiceKind::Preset),
                voice("extra", LabVoiceKind::Engine),
            ],
            default: Some("Aria".into()),
            ..LabVoices::default()
        };
        let opts = voice_options(&v, "");
        assert_eq!(opts[0].0, "", "empty means the model's default");
        let labels: Vec<&str> = opts.iter().skip(1).map(|o| o.1.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Aria (default)",
                "narrator · preset",
                "extra · from the running model"
            ]
        );
        let with_clip = voice_options(&v, "me");
        assert!(with_clip
            .iter()
            .any(|o| o.0 == "me" && o.1 == "me · library clip"));
        let remote = LabVoices {
            voices: vec![voice("x", LabVoiceKind::Engine)],
            source: LabVoicesSource::Upstream,
            ..LabVoices::default()
        };
        assert_eq!(voice_options(&remote, "")[1].1, "x · from the provider");
        assert!(!no_built_in(&v));
        let only_clips = LabVoices {
            voices: vec![voice("me", LabVoiceKind::Library)],
            ..LabVoices::default()
        };
        assert!(no_built_in(&only_clips));
        assert_eq!(voice_options(&only_clips, "").len(), 1);
    }
}
