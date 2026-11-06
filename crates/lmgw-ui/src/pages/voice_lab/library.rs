//! The reference panel: the voice library (the Audio lab's), each sample
//! with its transcript and — when the Voices lab made it — how: the model
//! asked and the one that answered, the description, voice, seed, language,
//! dates and the licence its package states (information only). Regenerate
//! puts that request back into the form; Check cloning picks the sample for
//! the cloning check; ✕ deletes the clip after a confirm.

use leptos::prelude::*;
use lmgw_api_types::audio_lab::ClipsAck;
use lmgw_api_types::voice_lab::{VoiceProvenance, VoiceSample};

use super::page::{enc, Lab};
use super::rounds::secs;
use crate::api;
use crate::fmt::fmt_bytes;
use crate::widgets::ConfirmButton;

/// The record as labelled lines, in reading order.
pub(super) fn provenance_rows(p: &VoiceProvenance) -> Vec<(&'static str, String)> {
    let mut rows = vec![("Asked", p.alias.clone())];
    if p.answered_by != p.alias {
        let why = p
            .fallback_reason
            .as_deref()
            .map(|r| format!(" (fallback: {})", crate::pages::traffic::fallback_words(r)))
            .unwrap_or_default();
        rows.push(("Answered by", format!("{}{why}", p.answered_by)));
    }
    rows.push((
        "Model",
        match &p.row {
            Some(r) => match &r.variant {
                Some(v) => format!("{} · {} ({v})", r.model_id, r.family),
                None => format!("{} · {}", r.model_id, r.family),
            },
            None => format!("{} on {}", p.upstream_model, p.upstream),
        },
    ));
    if let Some(d) = &p.description {
        rows.push(("Description", d.clone()));
    }
    if let Some(v) = &p.voice {
        rows.push(("Voice", v.clone()));
    }
    rows.push((
        "Seed",
        p.seed
            .map(|s| s.to_string())
            .unwrap_or_else(|| "none reached the model".into()),
    ));
    if let Some(l) = &p.language {
        rows.push(("Language", l.clone()));
    }
    if let Some(asked) = &p.asked_text {
        // The transcript is what the engine was sent; the round asked this.
        rows.push(("Asked to say", asked.clone()));
    }
    rows.push(("Made", p.generated_at.replace('T', " ")));
    rows.push(("Saved", p.saved_at.replace('T', " ")));
    rows.push((
        "Licence",
        p.licence
            .as_ref()
            .and_then(|l| l.label().map(str::to_string))
            .unwrap_or_else(|| "not stated".into()),
    ));
    let t = &p.trim;
    rows.push((
        "Trimmed",
        match &t.note {
            Some(n) => n.clone(),
            None => format!(
                "{} ms before, {} ms after; {} kept",
                t.lead_ms,
                t.tail_ms,
                secs(Some(t.kept_ms))
            ),
        },
    ));
    rows
}

#[component]
pub(super) fn Library(lab: Lab) -> impl IntoView {
    let preview = RwSignal::new(None::<String>);
    view! {
        <div class="lab-help vl-library">
            {move || {
                (!lab.library_active.get())
                    .then(|| {
                        view! {
                            <div class="notice warn">
                                "The audio class's voice directory is not this library, so a "
                                "sample is reachable as a voice_ref path only. Set it to "
                                <span class="mono-sm">"/models/voices"</span>
                                " under "
                                <a href=super::super::settings::href("audio.voice_dir")>
                                    "Settings → Runtimes → Audio"
                                </a>
                                " to speak with a sample by its name."
                            </div>
                        }
                    })
            }}
            {move || {
                let e = lab.samples_err.get();
                (!e.is_empty()).then(|| view! { <div class="notice err">{format!("⚠ {e}")}</div> })
            }}
            {move || {
                let e = lab.records_err.get();
                (!e.is_empty()).then(|| view! {
                    <div class="notice err">{format!("⚠ How the samples were made is not shown: {e}")}</div>
                })
            }}
            <Show
                when=move || !lab.samples.with(Vec::is_empty)
                fallback=|| {
                    view! {
                        <div class="empty">
                            "No samples yet. Save a candidate from a round: it lands here with "
                            "its transcript and how it was made."
                        </div>
                    }
                }
            >
                <For
                    each=move || lab.samples.get()
                    key=|s| (s.clip.name.clone(), s.provenance.as_ref().map(|p| p.sha256.clone()), s.provenance_current)
                    let:s
                >
                    <SampleRow lab=lab s=s preview=preview/>
                </For>
            </Show>
            {move || {
                preview
                    .get()
                    .map(|name| {
                        view! {
                            <audio
                                controls
                                autoplay
                                src=format!("/audio-lab/api/refs/{}", enc(&name))
                            ></audio>
                        }
                    })
            }}
            <p class="dim mini-note">
                "The Audio lab's voice library: a clip's name is the voice a cloning model "
                "speaks with, its transcript the reference text."
            </p>
        </div>
    }
}

#[component]
fn SampleRow(lab: Lab, s: VoiceSample, preview: RwSignal<Option<String>>) -> impl IntoView {
    let name = s.clip.name.clone();
    let voice = s.clip.voice.clone();
    let (n1, n2) = (name.clone(), name.clone());
    let delete = Callback::new(move |()| {
        let name = n2.clone();
        lab.scope.spawn(async move {
            let url = format!("/audio-lab/api/refs/{}/delete", enc(&name));
            match api::post::<ClipsAck, _>(url, &()).await {
                Ok(_) => {
                    lab.toasts.ok(format!("deleted {name}"));
                    lab.load_samples();
                }
                Err(e) => lab.toasts.err(e.to_string()),
            }
        });
    });
    let p = s.provenance.clone();
    let p2 = p.clone();
    let check = move |_| {
        lab.clone.sample.set(voice.clone());
        if let Some(el) = leptos::prelude::document().get_element_by_id("vl-clone") {
            el.scroll_into_view();
        }
    };
    let transcript = s.clip.transcript.clone();
    view! {
        <div class="vl-sample">
            <div class="clip-item" title=s.clip.server_path.clone()>
                <span class="n">{s.clip.voice.clone()}</span>
                <span class="dim mono-sm">{fmt_bytes(s.clip.size)}</span>
                <button class="btn ghost" title="Play" on:click=move |_| preview.set(Some(n1.clone()))>
                    "▸"
                </button>
                <ConfirmButton
                    label="✕"
                    confirm="Delete this sample from the library?"
                    class="btn ghost sm del"
                    title="Delete this clip and its transcript from the voice library"
                    on_confirm=delete
                />
            </div>
            <div class="vl-transcript dim">
                {if transcript.trim().is_empty() {
                    "no transcript — a cloning model that needs one refuses it".to_string()
                } else {
                    transcript
                }}
            </div>
            <div class="row vl-sample-actions">
                {p2.map(|p| {
                    view! {
                        <button
                            class="btn ghost sm"
                            data-dock-pick=""
                            title="Fill the form with the request this sample was made from. GPU output is not bit-identical per seed: it makes new candidates."
                            on:click=move |_| lab.regenerate(&p)
                        >
                            "Regenerate"
                        </button>
                    }
                })}
                <button
                    class="btn ghost sm"
                    data-dock-pick=""
                    title="Pick this sample for the cloning check"
                    on:click=check
                >
                    "Check cloning"
                </button>
            </div>
            {p.map(|p| {
                let stale = !s.provenance_current;
                view! {
                    <details class="vl-prov">
                        <summary>
                            {format!("Made with {} · {}", p.alias, p.saved_at.get(..10).unwrap_or(""))}
                        </summary>
                        {stale.then(|| view! {
                            <div class="mini-note vl-warn">
                                "The clip was replaced since: this describes the sample the Voices lab saved, not the file now."
                            </div>
                        })}
                        <dl class="vl-prov-rows">
                            {provenance_rows(&p)
                                .into_iter()
                                .map(|(k, v)| view! { <dt>{k}</dt> <dd>{v}</dd> })
                                .collect_view()}
                        </dl>
                    </details>
                }
            })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::voice_lab::{VoiceRow, VoiceTrim};

    #[test]
    fn a_record_reads_as_labelled_lines() {
        let p = VoiceProvenance {
            alias: "audio/design".into(),
            answered_by: "cloud-tts".into(),
            fallback_reason: Some("hold".into()),
            row: Some(VoiceRow {
                model_id: "design".into(),
                family: "qwen3_tts".into(),
                variant: Some("voice_design".into()),
            }),
            description: Some("warm".into()),
            seed: Some(7),
            language: Some("en".into()),
            generated_at: "2026-10-19T14:02:33Z".into(),
            saved_at: "2026-10-19T14:05:00Z".into(),
            trim: VoiceTrim {
                lead_ms: 460,
                tail_ms: 180,
                kept_ms: 1160,
                note: None,
            },
            ..VoiceProvenance::default()
        };
        let rows = provenance_rows(&p);
        let get = |k: &str| rows.iter().find(|(l, _)| *l == k).map(|(_, v)| v.as_str());
        assert_eq!(get("Answered by"), Some("cloud-tts (fallback: hold)"));
        assert_eq!(get("Model"), Some("design · qwen3_tts (voice_design)"));
        assert_eq!(get("Seed"), Some("7"));
        assert_eq!(get("Licence"), Some("not stated"));
        assert_eq!(
            get("Trimmed"),
            Some("460 ms before, 180 ms after; 1.2 s kept")
        );
        assert_eq!(get("Voice"), None, "no voice was asked");
    }
}
