//! The cloning check: pick a saved sample and a cloning model, and have it
//! say a short line with the sample as its reference — to hear whether the
//! sample clones well before a realtime session speaks with it.
//!
//! It goes through the real speech route (`POST /voice-lab/api/clone-check`
//! hands it to `/v1/audio/speech`), so the answer is what any client gets:
//! the audio, the fallback that answered (`x-lmgw-fallback`), or the route's
//! error.

use gloo_net::http::Request;
use leptos::prelude::*;
use lmgw_api_types::voice_lab::{VoiceCloneCheck, VoiceModelFacts};

use super::page::{enc, Lab};
use crate::api;
use crate::widgets::{ModelPicker, Select};

/// The check's own state.
#[derive(Clone, Copy)]
pub(super) struct CloneCheck {
    /// The sample's voice name.
    pub sample: RwSignal<String>,
    pub alias: RwSignal<String>,
    pub text: RwSignal<String>,
    pub facts: RwSignal<Option<VoiceModelFacts>>,
    pub running: RwSignal<bool>,
    /// The answer's blob URL.
    pub audio: RwSignal<Option<String>>,
    /// Who answered it, in words.
    pub answered: RwSignal<String>,
    pub error: RwSignal<String>,
}

impl CloneCheck {
    pub fn new() -> Self {
        Self {
            sample: RwSignal::new(String::new()),
            alias: RwSignal::new(String::new()),
            text: RwSignal::new(String::new()),
            facts: RwSignal::new(None),
            running: RwSignal::new(false),
            audio: RwSignal::new(None),
            answered: RwSignal::new(String::new()),
            error: RwSignal::new(String::new()),
        }
    }
}

fn blob_url(bytes: &[u8], mime: &str) -> Option<String> {
    let arr = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::new();
    parts.push(&arr);
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type(mime);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).ok()?;
    web_sys::Url::create_object_url_with_blob(&blob).ok()
}

/// The message of an error answer: the OpenAI envelope's, else the text.
fn error_text(status: u16, body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("{status}: {}", body.trim()))
}

fn speak(lab: Lab) {
    let c = lab.clone;
    let body = VoiceCloneCheck {
        alias: c.alias.get_untracked(),
        sample: c.sample.get_untracked(),
        text: Some(c.text.get_untracked()).filter(|t| !t.trim().is_empty()),
        language: Some(lab.language.get_untracked()).filter(|l| !l.is_empty()),
    };
    c.running.set(true);
    c.error.set(String::new());
    c.answered.set(String::new());
    if let Some(old) = c.audio.get_untracked() {
        let _ = web_sys::Url::revoke_object_url(&old);
        c.audio.set(None);
    }
    lab.scope.spawn(async move {
        let sent = match Request::post("/voice-lab/api/clone-check").json(&body) {
            Ok(req) => req.send().await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        match sent {
            Ok(resp) if resp.ok() => {
                let fallback = resp.headers().get("x-lmgw-fallback");
                let mime = resp
                    .headers()
                    .get("content-type")
                    .unwrap_or_else(|| "audio/wav".into());
                match resp.binary().await {
                    Ok(bytes) => {
                        c.audio.set(blob_url(&bytes, &mime));
                        c.answered.set(match fallback {
                            Some(fb) => format!(
                                "{fb} answered in place of {} (its configured fallback) — \
                                 that is the voice you hear",
                                body.alias
                            ),
                            None => format!("{} spoke with {}", body.alias, body.sample),
                        });
                    }
                    Err(e) => c.error.set(e.to_string()),
                }
            }
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                c.error.set(error_text(status, &text));
            }
            Err(e) => c.error.set(format!("not sent: {e}")),
        }
        c.running.set(false);
    });
}

#[component]
pub(super) fn CloneCard(lab: Lab) -> impl IntoView {
    let c = lab.clone;
    // The language's check line is the prefill — put in only while the box
    // still holds the last prefill (or nothing): a line the owner typed
    // stays when the sentences are read again (a sentence saved) or the
    // language changes.
    let prefilled = StoredValue::new(String::new());
    Effect::new(move |_| {
        let lang = lab.language.get();
        let line = lab
            .sentences
            .with(|v| {
                v.iter()
                    .find(|s| s.language == lang)
                    .map(|s| s.check_line.clone())
            })
            .unwrap_or_default();
        let now = c.text.get_untracked();
        if now.trim().is_empty() || now == prefilled.get_value() {
            c.text.set(line.clone());
        }
        prefilled.set_value(line);
    });
    // What the picked model does with a clip.
    Effect::new(move |_| {
        let alias = c.alias.get();
        c.facts.set(None);
        if alias.trim().is_empty() {
            return;
        }
        lab.scope.spawn(async move {
            let url = format!("/voice-lab/api/model?alias={}", enc(&alias));
            if let Ok(f) = api::get::<VoiceModelFacts>(url).await {
                if c.alias.get_untracked() == alias {
                    c.facts.set(Some(f));
                }
            }
        });
    });
    on_cleanup(move || {
        if let Some(u) = c.audio.try_get_untracked().flatten() {
            let _ = web_sys::Url::revoke_object_url(&u);
        }
    });
    let samples = Signal::derive(move || {
        lab.samples.with(|v| {
            v.iter()
                .map(|s| {
                    let made = if s.provenance.is_some() {
                        ""
                    } else {
                        " · uploaded"
                    };
                    (s.clip.voice.clone(), format!("{}{made}", s.clip.voice))
                })
                .collect::<Vec<_>>()
        })
    });
    let ready =
        move || !c.sample.get().is_empty() && !c.alias.get().trim().is_empty() && !c.running.get();
    view! {
        <div class="card vl-clone" id="vl-clone">
            <h3 class="lab-h3">"Cloning check"</h3>
            <p class="dim mini-note">
                "Have a cloning model say a short line with a saved sample as its reference, the "
                "way a realtime session would speak with it — to hear whether the sample clones well."
            </p>
            <div class="field-grid" style="--field-min:180px">
                <div class="field">
                    <label>"Sample"</label>
                    <Select value=c.sample options=samples placeholder="pick a saved sample"/>
                </div>
                <div class="field">
                    <label>"Cloning model"</label>
                    <ModelPicker
                        value=c.alias
                        tasks=lmgw_api_types::realtime::SPEECH_TASKS
                        recent_key="voicelab.clone"
                    />
                </div>
            </div>
            {move || {
                c.facts.get().and_then(|f| f.clone_note.map(|n| (f.clones, n))).map(|(clones, n)| {
                    view! { <div class="field-hint" class:vl-warn=!clones>{n}</div> }
                })
            }}
            <div class="field" style="margin-top:10px">
                <label>"Line"</label>
                <input
                    class="input"
                    placeholder="what the model says"
                    prop:value=move || c.text.get()
                    on:input=move |ev| c.text.set(event_target_value(&ev))
                />
            </div>
            <div class="row" style="margin-top:10px">
                <button class="btn primary sm" disabled=move || !ready() on:click=move |_| speak(lab)>
                    "Speak"
                </button>
                <Show when=move || c.running.get()>
                    <span class="chip live">
                        <i class="dot"></i>
                        "speaking…"
                    </span>
                </Show>
            </div>
            {move || c.audio.get().map(|u| view! { <audio controls autoplay src=u></audio> })}
            {move || {
                let a = c.answered.get();
                (!a.is_empty()).then(|| view! { <div class="dim mini-note">{a}</div> })
            }}
            {move || {
                let e = c.error.get();
                (!e.is_empty()).then(|| view! { <div class="notice err">{format!("⚠ {e}")}</div> })
            }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_answer_says_its_message() {
        assert_eq!(
            error_text(
                400,
                r#"{"error":{"message":"no sample 'x'","code":"not_found"}}"#
            ),
            "no sample 'x'"
        );
        assert_eq!(error_text(502, "bad gateway"), "502: bad gateway");
    }
}
