//! Settings › Documents' "Transcribe audio with" (document-conversion
//! design §8, D5, WP7): the speech-to-text model audio files, and the sound
//! of videos, are transcribed with in a conversion, off until one is chosen,
//! in Model help below "Describe pictures with". Beside the choice the row
//! says, as "Read pages with a model" does ([`super::model`]),
//! whether the chosen model runs on this computer or the audio is sent to a
//! named outside service, and which model transcribes whenever the chosen
//! one cannot run (the GPU hold for a local model) and where that one is.
//!
//! Saved at once through the dashboard's `settings_set`
//! (`documents_asr_alias`). The admin tools cannot set it: choosing where
//! callers' audio is sent is the owner's own act.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::documents::model_help::PageReader;
use serde_json::json;

use super::model::outside_said;
use super::use_docs;
use crate::widgets::{use_toasts, ModelPicker};

/// Where the chosen model runs: on this computer, or which outside service
/// the audio is sent to. `None` while none is chosen.
pub(super) fn where_said(m: &PageReader) -> Option<String> {
    if m.alias.is_empty() {
        return None;
    }
    Some(match (&m.outside, m.local) {
        (Some(o), _) => format!(
            "{} is not a model lmgw runs on this computer: audio and the sound of videos are \
             sent to {}.",
            m.alias,
            outside_said(o)
        ),
        (None, true) => format!(
            "{} runs on this computer: the audio it transcribes stays here.",
            m.alias
        ),
        (None, false) => format!(
            "lmgw cannot tell where {} runs (its name does not resolve to a model now).",
            m.alias
        ),
    })
}

/// Who transcribes whenever the chosen model cannot run, and where that is.
/// `None` while none is chosen.
pub(super) fn fallback_said(m: &PageReader) -> Option<String> {
    if m.alias.is_empty() {
        return None;
    }
    let when = if m.local {
        "cannot run (the GPU hold is on, the graphics card is full or in use by a benchmark)"
    } else {
        "cannot answer (its service is out of quota or cannot be reached)"
    };
    let Some(f) = &m.fallback else {
        return Some(format!(
            "Whenever {} {when}, audio and videos are not transcribed: the conversion says so \
             ({} has no fallback).",
            m.alias, m.alias
        ));
    };
    let to = match &f.outside {
        Some(o) => format!(
            "{} is not a model lmgw runs on this computer, so then the audio is sent to {}",
            f.alias,
            outside_said(o)
        ),
        None if m.local => format!("{} runs on this computer too", f.alias),
        None => format!("{} runs on this computer", f.alias),
    };
    Some(format!(
        "Whenever {} {when}, its fallback {} transcribes instead: {to}.",
        m.alias, f.alias
    ))
}

/// What the row says under its title.
pub(super) fn sub_said(m: &PageReader) -> String {
    if m.alias.is_empty() {
        "Off. Audio files, and the sound of videos, can be transcribed by a speech-to-text \
         model; nothing is sent to a model until you choose one."
            .to_string()
    } else {
        format!(
            "Audio files, and the sound of videos, are transcribed by {} for the key that sends \
             them.",
            m.alias
        )
    }
}

#[component]
pub(super) fn SpeechRow(model: PageReader) -> impl IntoView {
    let d = use_docs();
    let toasts = use_toasts();
    let saved = StoredValue::new(model.alias.clone());
    let value = RwSignal::new(model.alias.clone());
    let saving = RwSignal::new(false);
    Effect::new(move |_| {
        let picked = value.get();
        if picked == saved.get_value() {
            return;
        }
        saved.set_value(picked.clone());
        let done = if picked.is_empty() {
            "Audio and videos are not transcribed".to_string()
        } else {
            format!("Audio and videos are transcribed with {picked}")
        };
        saving.set(true);
        spawn_local(async move {
            let body = json!({ "documents_asr_alias": picked });
            match crate::api::post::<serde_json::Value, _>("/api/op/settings_set", &body).await {
                Ok(_) => toasts.ok(done),
                Err(e) => toasts.err(e.to_string()),
            }
            saving.try_set(false);
            // Drawn again from what is stored now, a refused choice included.
            d.load(false);
        });
    });
    let goes = where_said(&model);
    let leaves = model.outside.is_some();
    let hold = fallback_said(&model);
    let problems = model.problems.clone();
    view! {
        <div class="prow doc-row stack" data-speech-model="">
            <div class="prow-text">
                <span class="prow-title">"Transcribe audio with"</span>
                <span class="prow-sub">{sub_said(&model)}</span>
                {goes.map(|g| {
                    view! { <span class="prow-sub" class:doc-warn=leaves data-speech-where="">{g}</span> }
                })}
                {hold.map(|h| view! { <span class="prow-sub" data-speech-fallback="">{h}</span> })}
                {problems
                    .into_iter()
                    .map(|p| view! { <span class="prow-sub doc-warn">{p}</span> })
                    .collect_view()}
            </div>
            <div class="prow-end">
                <ModelPicker
                    value=value
                    tasks=&["asr"]
                    empty_label="Off"
                    recent_key="documents.asr_alias"
                    disabled=saving
                />
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::documents::model_help::{Outside, PageFallback};

    fn local(alias: &str) -> PageReader {
        PageReader {
            alias: alias.into(),
            local: true,
            ..Default::default()
        }
    }

    #[test]
    fn the_row_says_whether_audio_stays_here_and_who_answers_in_its_place() {
        assert_eq!(where_said(&PageReader::default()), None);
        assert_eq!(fallback_said(&PageReader::default()), None);
        assert!(sub_said(&PageReader::default()).starts_with("Off."));
        let mut m = local("example-stt");
        assert!(where_said(&m).unwrap().contains("stays here"));
        let none = fallback_said(&m).unwrap();
        assert!(
            none.contains("the GPU hold is on") && none.contains("not transcribed"),
            "{none}"
        );
        m.fallback = Some(PageFallback {
            alias: "example-cloud/whisper".into(),
            local: false,
            outside: Some(Outside {
                service: "example-cloud".into(),
                host: "api.example".into(),
            }),
            sees: None,
        });
        let away = fallback_said(&m).unwrap();
        assert!(
            away.contains("its fallback example-cloud/whisper transcribes instead")
                && away.contains("sent to the service \"example-cloud\" (api.example)"),
            "{away}"
        );
        let cloud = PageReader {
            alias: "example-cloud/whisper".into(),
            outside: Some(Outside {
                service: "example-cloud".into(),
                host: String::new(),
            }),
            ..Default::default()
        };
        let w = where_said(&cloud).unwrap();
        assert!(w.contains("sent to the service \"example-cloud\""), "{w}");
    }
}
