//! Expressive speech (WP10, WP9b): the speech style a session's TTS gets
//! while the client sends none, and the hint in an audio response's prompt
//! about what square brackets do — the sounds the TTS makes, or the
//! delivery cues it takes — each told what the TTS the draft names does
//! with it, read from its `capabilities.speech` on `GET /v1/models/{alias}`,
//! which lmgw answers from its own catalog without calling a provider.

use leptos::prelude::*;
use lmgw_api_types::realtime::speech_hint_text;
use serde_json::Value;

use super::super::{Def, Page};
use crate::scope::{Latest, Scope};
use crate::widgets::Field;

/// What the drafted TTS model does with speech instructions and inline tags.
#[derive(Clone, Debug, PartialEq)]
enum Speech {
    /// No TTS model in the draft.
    NoModel,
    Loading,
    /// Its `capabilities.speech` words: `instructions`
    /// (`none`|`style`|`voice_design`|`passthrough`), `inline_tags`
    /// (`none`|`fixed`|`free`) and the tags a fixed one renders. A cloud
    /// alias the owner described no further has none: lmgw sends it the
    /// instructions as they are and strips every tag.
    Known {
        instructions: Option<String>,
        inline_tags: Option<String>,
        tags: Vec<String>,
    },
    Failed(String),
}

/// `/v1/models/{alias}`, each path segment encoded — an audio row's alias
/// is `audio/<id>`, and the route takes the rest of the path as the id.
fn model_url(alias: &str) -> String {
    let id = alias
        .split('/')
        .map(|s| String::from(js_sys::encode_uri_component(s)))
        .collect::<Vec<_>>()
        .join("/");
    format!("/v1/models/{id}")
}

/// The drafted TTS model's speech facts — the first of `tts_keys` the draft
/// sets — read once `d` is on screen and again whenever the draft names
/// another model.
fn drafted_speech(
    page: Page,
    d: &'static Def,
    tts_keys: &'static [&'static str],
) -> RwSignal<Speech> {
    let speech = RwSignal::new(Speech::NoModel);
    let latest = Latest::new();
    let scope = Scope::new();
    let tts = Memo::new(move |_| super::voice::drafted_tts(page, tts_keys));
    Effect::new(move |_| {
        let t = tts.get();
        if !page.shows(d) {
            return;
        }
        // A ticket per change: an answer for the model picked before never
        // lands over this one.
        let Some(ticket) = latest.next() else { return };
        if t.is_empty() {
            speech.set(Speech::NoModel);
            return;
        }
        speech.set(Speech::Loading);
        let url = model_url(&t);
        scope.spawn(async move {
            let res = crate::api::get::<Value>(url).await;
            if !latest.is(ticket) {
                return;
            }
            speech.set(match res {
                Ok(v) => {
                    let s = &v["capabilities"]["speech"];
                    let word = |k: &str| s[k].as_str().map(str::to_string);
                    Speech::Known {
                        instructions: word("instructions"),
                        inline_tags: word("inline_tags"),
                        tags: s["tags"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|t| t.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    }
                }
                Err(e) => Speech::Failed(e.to_string()),
            });
        });
    });
    speech
}

/// What the drafted TTS does with the style, in a line.
fn style_note(s: &Speech) -> String {
    match s {
        Speech::NoModel => "pick a text-to-speech model first".into(),
        Speech::Loading => "reading what the model does with it…".into(),
        Speech::Failed(e) => format!("what the model does with it could not be read: {e}"),
        Speech::Known { instructions, .. } => match instructions.as_deref() {
            Some("style") => "the model speaks in this style".into(),
            Some("voice_design") => "the model designs its voice from this description — \
                                     required, unless its row has a default description of its \
                                     own (which then wins over this)"
                .into(),
            Some("none") => "this model reads no instructions: it is not sent".into(),
            Some(_) => "sent with every clause; the model decides what it does with it".into(),
            None => "a provider's model: sent as its instructions with every clause".into(),
        },
    }
}

/// The paragraph the prompt would get for the drafted TTS while the hint is
/// on — the gateway's own text ([`speech_hint_text`]) — or why there is
/// none.
fn hint_preview(s: &Speech) -> Result<String, String> {
    match s {
        Speech::NoModel => Err("pick a text-to-speech model first".into()),
        Speech::Loading => Err("reading what the model does with square brackets…".into()),
        Speech::Failed(e) => Err(format!(
            "what the model does with square brackets could not be read: {e}"
        )),
        // A cloud alias the owner described no further publishes no
        // instructions word: it gets the style, but no cues.
        Speech::Known {
            instructions,
            inline_tags,
            tags,
        } => speech_hint_text(
            instructions.as_deref(),
            inline_tags.as_deref().unwrap_or("none"),
            tags,
        )
        .ok_or_else(|| {
            let why = "this model renders no tags and takes no delivery cues: the prompt says \
                       nothing about square brackets, and tags the model writes are stripped, \
                       never read out";
            if instructions.is_none() {
                format!(
                    "{why} (a provider's model takes cues only when its alias declares \
                     capabilities.speech.instructions \"style\")"
                )
            } else {
                why.to_string()
            }
        }),
    }
}

/// The speech style: prose, any length, empty for none (for the Chat's own:
/// realtime's). Told what the TTS the draft names in `tts_keys` does with it.
#[component]
pub(in crate::pages::settings) fn SpeechStyleField(
    d: &'static Def,
    page: Page,
    dirty: Signal<bool>,
    error: Signal<Option<String>>,
    id: String,
    hidden: Signal<bool>,
    tts_keys: &'static [&'static str],
) -> impl IntoView {
    let form = page.form;
    let k = d.key;
    let speech = drafted_speech(page, d, tts_keys);
    let unit = Signal::derive(move || match speech.get() {
        Speech::Known { instructions, .. } if instructions.as_deref() == Some("voice_design") => {
            "voice description"
        }
        _ if form.text(k).trim().is_empty() && k == "chat_speech_style" => "inherits Realtime",
        _ if form.text(k).trim().is_empty() => "none",
        _ => "speaking style",
    });
    view! {
        <Field
            label=d.label
            unit=unit
            hint=d.hint
            dirty=dirty
            error=error
            id=id
            hidden=hidden
        >
            <textarea
                class="input ta set-ta set-prompt"
                rows="3"
                placeholder=d.ph
                prop:value=move || form.text(k)
                on:input=move |ev| form.set_text(k, event_target_value(&ev))
            ></textarea>
            <div class="field-hint">{move || style_note(&speech.get())}</div>
        </Field>
    }
}

/// The tag hint: a checkbox, and the paragraph the prompt would get
/// ([`hint_preview`]).
#[component]
pub(in crate::pages::settings) fn TagHintField(
    d: &'static Def,
    page: Page,
    dirty: Signal<bool>,
    id: String,
    hidden: Signal<bool>,
) -> impl IntoView {
    let form = page.form;
    let k = d.key;
    let speech = drafted_speech(page, d, super::RT_TTS);
    let preview = move || -> Result<String, String> {
        if !form.flag(k) {
            return Err(
                "off: the prompt says nothing about square brackets (what the model writes in \
                 them still applies)"
                    .into(),
            );
        }
        hint_preview(&speech.get())
    };
    view! {
        <div class="set-tag-hint" id=id hidden=move || hidden.get()>
            <label class="check check-line" class:dirty=move || dirty.get()>
                <input
                    type="checkbox"
                    prop:checked=move || form.flag(k)
                    on:change=move |ev| form.set_flag(k, event_target_checked(&ev))
                />
                <span>{d.label}</span>
                {(!d.hint.is_empty()).then(|| view! { <span class="check-hint">{d.hint}</span> })}
            </label>
            {move || match preview() {
                Ok(text) => view! {
                    <blockquote class="set-hint-preview">{text}</blockquote>
                }
                .into_any(),
                Err(why) => view! { <div class="field-hint">{why}</div> }.into_any(),
            }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_note_says_what_the_model_does_with_the_style() {
        let known = |i: Option<&str>| Speech::Known {
            instructions: i.map(str::to_string),
            inline_tags: None,
            tags: vec![],
        };
        assert!(style_note(&known(Some("style"))).contains("speaks in this style"));
        assert!(style_note(&known(Some("voice_design"))).contains("required"));
        assert!(style_note(&known(Some("none"))).contains("not sent"));
        assert!(style_note(&known(Some("passthrough"))).contains("decides"));
        assert!(style_note(&known(None)).contains("provider"));
        assert!(style_note(&Speech::NoModel).contains("pick"));
    }

    #[test]
    fn the_preview_is_the_sounds_or_the_cues_or_neither() {
        let known = |i: Option<&str>, t: Option<&str>, tags: &[&str]| Speech::Known {
            instructions: i.map(str::to_string),
            inline_tags: t.map(str::to_string),
            tags: tags.iter().map(|t| t.to_string()).collect(),
        };
        let omni = hint_preview(&known(Some("passthrough"), Some("fixed"), &["laughter"]));
        assert!(omni.unwrap().contains("[laughter]"));
        let custom = hint_preview(&known(Some("style"), Some("none"), &[])).unwrap();
        assert!(custom.contains("such as [laughing]"), "{custom}");
        assert!(
            custom.contains("one or two lowercase English words"),
            "{custom}"
        );
        // A cloud alias described no further takes no cues, as in the
        // gateway; one declared a style takes them.
        let cloud = hint_preview(&known(None, None, &[])).unwrap_err();
        assert!(cloud.contains("instructions \"style\""), "{cloud}");
        assert_eq!(hint_preview(&known(Some("style"), None, &[])), Ok(custom));
        let design = hint_preview(&known(Some("voice_design"), Some("none"), &[]));
        assert!(design.unwrap_err().contains("takes no delivery cues"));
        assert!(hint_preview(&Speech::NoModel).unwrap_err().contains("pick"));
    }
}
