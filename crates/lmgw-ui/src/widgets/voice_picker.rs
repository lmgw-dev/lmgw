//! A voice of a text-to-speech model: a free box (any name the model takes),
//! offered the voices of the model a draft names. Shared by Settings →
//! Realtime and Settings → Chat → Voice, and by a Chat thread's voice
//! settings (chat-voice design §2.1).
//!
//! A local model's list comes from `GET /v1/audio/voices`, which lmgw answers
//! from its own catalog without starting the model; a cloud model's is never
//! asked for — that would be a call to the provider.
//!
//! **A clip the model cannot clone** (no transcript, for a model that
//! needs one — `lmgw.needs_transcript`) is marked in the list and named
//! under the box, with where to transcribe it: speaking with it is refused
//! (`voice_needs_transcript`).
//!
//! **Read on demand** (WP1 review m6): the list is asked for when the box is
//! focused, not when it is drawn, and kept per alias for the page's life.
//! Under the GPU hold a local row with a provider as its fallback is
//! answered with the fallback's list — a provider call — so drawing a
//! thread's settings must not ask; the owner opening the box may.

use std::cell::RefCell;
use std::collections::HashMap;

use leptos::prelude::*;
use serde_json::Value;

use crate::catalog::use_model_catalog;
use crate::scope::{Latest, Scope};

/// audio.cpp reports voices as bare ids; a family that answers with objects is
/// read for the usual id keys rather than rendered as `[object Object]`.
pub fn voice_name(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    for key in ["id", "voice_id", "name"] {
        if let Some(s) = v[key].as_str() {
            return Some(s.to_string());
        }
    }
    None
}

/// Where the voice list of the drafted TTS model stands.
#[derive(Clone, Debug, PartialEq)]
pub enum Voices {
    /// No TTS model in the draft.
    NoModel,
    /// A model lmgw does not run here: its provider's voice names are not
    /// listed, and asking for them would be a call to the provider.
    Remote,
    /// A local model whose list is read when the box is focused.
    NotRead,
    Loading,
    Loaded {
        names: Vec<String>,
        default: Option<String>,
        /// Library clips without a transcript, for a model that cannot
        /// clone one without (`lmgw.needs_transcript`): refused until
        /// transcribed in the Audio lab.
        untranscribed: Vec<String>,
    },
    Failed(String),
}

impl Voices {
    /// The line under the box: what the list holds, or why there is none.
    pub fn status(&self) -> String {
        match self {
            Voices::NoModel => "pick a text-to-speech model first".to_string(),
            Voices::Remote => "a provider's model: type one of its voice names".to_string(),
            Voices::NotRead => "the model's voices are listed when you open this box".to_string(),
            Voices::Loading => "reading the model's voices…".to_string(),
            Voices::Loaded {
                names,
                default,
                untranscribed,
            } => {
                if names.is_empty() {
                    return "the model lists no voices — type a clip or voice name".into();
                }
                let shown: Vec<String> = names.iter().take(12).cloned().collect();
                let more = names.len().saturating_sub(shown.len());
                format!(
                    "{}{}{}{}",
                    shown.join(", "),
                    if more > 0 {
                        format!(" and {more} more")
                    } else {
                        String::new()
                    },
                    default
                        .as_ref()
                        .map(|d| format!(" · default {d}"))
                        .unwrap_or_default(),
                    if untranscribed.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " · this model cannot clone a clip without its transcript: {} — \
                             transcribe it in the Audio lab first",
                            untranscribed.join(", ")
                        )
                    }
                )
            }
            Voices::Failed(e) => format!("its voices could not be read: {e}"),
        }
    }

    /// The model's own default voice, once its list is read.
    pub fn default_voice(&self) -> Option<String> {
        match self {
            Voices::Loaded { default, .. } => default.clone(),
            _ => None,
        }
    }

    fn names(&self) -> Vec<String> {
        match self {
            Voices::Loaded { names, .. } => names.clone(),
            _ => Vec::new(),
        }
    }

    /// `name` is a clip the model cannot clone yet ([`Voices::Loaded`]).
    fn untranscribed(&self, name: &str) -> bool {
        matches!(self, Voices::Loaded { untranscribed, .. } if untranscribed.iter().any(|c| c == name))
    }
}

/// The clips of a `GET /v1/audio/voices` answer the model cannot clone:
/// none unless `lmgw.needs_transcript`, then the library entries whose
/// transcript is not recorded.
fn untranscribed_of(v: &Value) -> Vec<String> {
    if v["lmgw"]["needs_transcript"].as_bool() != Some(true) {
        return Vec::new();
    }
    v["lmgw"]["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["kind"] == "library" && e["transcript"] == false)
        .filter_map(|e| e["id"].as_str().map(str::to_string))
        .collect()
}

thread_local! {
    /// The lists read so far, by alias, for the page's life (module doc).
    /// A failure is not kept: the next focus asks again.
    static READ: RefCell<HashMap<String, Voices>> = RefCell::new(HashMap::new());
}

fn read_before(alias: &str) -> Option<Voices> {
    READ.with(|m| m.borrow().get(alias).cloned())
}

fn remember(alias: &str, v: &Voices) {
    if matches!(v, Voices::Loaded { .. }) {
        READ.with(|m| m.borrow_mut().insert(alias.to_string(), v.clone()));
    }
}

/// The voice list of one box, and whether its owner asked for it.
#[derive(Clone, Copy)]
pub struct VoiceList {
    pub voices: RwSignal<Voices>,
    /// Set by the box's first focus: from then on the list of the model the
    /// draft names is read, and read again when it names another.
    wanted: RwSignal<bool>,
}

impl VoiceList {
    /// The box was focused: read the list (once per alias, module doc), or
    /// again after a read that failed.
    pub fn want(&self) {
        let failed = matches!(self.voices.get_untracked(), Voices::Failed(_));
        if !self.wanted.get_untracked() || failed {
            // `update` notifies even when the flag is already set.
            self.wanted.update(|w| *w = true);
        }
    }
}

/// The voice list of the TTS model `tts` names, read once the box asks for
/// it ([`VoiceList::want`]) and again whenever `tts` names another model.
/// A list read before for that alias is shown at once, asking nothing.
/// Each read takes a ticket, so a list still in flight for the model
/// picked before never lands over this one.
pub fn use_voices(tts: Signal<String>) -> VoiceList {
    let catalog = use_model_catalog();
    let voices = RwSignal::new(Voices::NoModel);
    let wanted = RwSignal::new(false);
    let latest = Latest::new();
    let scope = Scope::new();
    let tts = Memo::new(move |_| tts.get().trim().to_string());
    let local = Memo::new(move |_| {
        let t = tts.get();
        catalog
            .entries
            .with(|e| e.iter().find(|e| e.id == t).map(|e| e.local))
    });
    Effect::new(move |_| {
        let t = tts.get();
        let want = wanted.get();
        let Some(ticket) = latest.next() else { return };
        match local.get() {
            _ if t.is_empty() => voices.set(Voices::NoModel),
            Some(false) => voices.set(Voices::Remote),
            // Not in the model list (yet): nothing is asked of a name the
            // gateway may route to a provider.
            None => voices.set(Voices::Remote),
            Some(true) => {
                if let Some(v) = read_before(&t) {
                    voices.set(v);
                    return;
                }
                if !want {
                    voices.set(Voices::NotRead);
                    return;
                }
                voices.set(Voices::Loading);
                let url = format!(
                    "/v1/audio/voices?model={}",
                    String::from(js_sys::encode_uri_component(&t))
                );
                scope.spawn(async move {
                    let res = crate::api::get::<Value>(url).await;
                    let v = match res {
                        Ok(v) => Voices::Loaded {
                            names: v["voices"]
                                .as_array()
                                .map(|a| a.iter().filter_map(voice_name).collect())
                                .unwrap_or_default(),
                            default: v["lmgw"]["default"].as_str().map(str::to_string),
                            untranscribed: untranscribed_of(&v),
                        },
                        Err(e) => Voices::Failed(e.to_string()),
                    };
                    remember(&t, &v);
                    if latest.is(ticket) {
                        voices.set(v);
                    }
                });
            }
        }
    });
    VoiceList { voices, wanted }
}

/// The box: free text offered `voices` as a datalist, with the list's
/// status under it. Focusing it asks for the list. `placeholder` is what an
/// empty box means (an inherited voice); unset, it is the model's own
/// default once read.
#[component]
pub fn VoiceInput(
    #[prop(into)] value: Signal<String>,
    on_input: Callback<String>,
    list: VoiceList,
    /// Unique on the page: the datalist's id.
    list_id: String,
    #[prop(optional, into)] placeholder: Option<Signal<String>>,
) -> impl IntoView {
    let voices = list.voices;
    let options = move || {
        let v = voices.get();
        v.names()
            .into_iter()
            .map(|n| {
                let label = v
                    .untranscribed(&n)
                    .then(|| format!("{n} — needs a transcript (Audio lab)"));
                view! { <option value=n.clone() label=label></option> }
            })
            .collect_view()
    };
    let placeholder = move || match placeholder {
        Some(p) => p.get(),
        None => voices.get().default_voice().unwrap_or_default(),
    };
    view! {
        <input
            class="input"
            list=list_id.clone()
            spellcheck="false"
            autocomplete="off"
            placeholder=placeholder
            prop:value=move || value.get()
            on:focus=move |_| list.want()
            on:input=move |ev| on_input.run(event_target_value(&ev))
        />
        <datalist id=list_id>{options}</datalist>
        <div class="field-hint">{move || voices.get().status()}</div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_line_names_the_voices_and_the_default() {
        let loaded = Voices::Loaded {
            names: (0..14).map(|i| format!("v{i}")).collect(),
            default: Some("v0".into()),
            untranscribed: Vec::new(),
        };
        let s = loaded.status();
        assert!(s.starts_with("v0, v1"), "{s}");
        assert!(s.contains("and 2 more"), "{s}");
        assert!(s.ends_with("· default v0"), "{s}");
        assert_eq!(loaded.default_voice().as_deref(), Some("v0"));
        assert!(Voices::NoModel.status().contains("pick"));
        assert!(Voices::Remote.status().contains("provider"));
        assert!(Voices::NotRead.status().contains("when you open"));
        let empty = Voices::Loaded {
            names: vec![],
            default: None,
            untranscribed: Vec::new(),
        };
        assert!(empty.status().contains("lists no voices"));
    }

    #[test]
    fn a_clip_the_model_cannot_clone_is_named_with_where_to_fix_it() {
        let answer = serde_json::json!({
            "voices": ["anna", "bert", "ryan"],
            "lmgw": {"needs_transcript": true, "entries": [
                {"id": "anna", "kind": "library", "send": "voice", "transcript": false},
                {"id": "bert", "kind": "library", "send": "voice", "transcript": true},
                {"id": "ryan", "kind": "native", "send": "voice"}
            ]}
        });
        assert_eq!(untranscribed_of(&answer), ["anna"]);
        let mut other = answer.clone();
        other["lmgw"]["needs_transcript"] = false.into();
        assert!(
            untranscribed_of(&other).is_empty(),
            "a model that clones without"
        );
        let loaded = Voices::Loaded {
            names: vec!["anna".into(), "bert".into()],
            default: None,
            untranscribed: untranscribed_of(&answer),
        };
        assert!(loaded.untranscribed("anna") && !loaded.untranscribed("bert"));
        let s = loaded.status();
        assert!(
            s.ends_with("without its transcript: anna — transcribe it in the Audio lab first"),
            "{s}"
        );
    }

    #[test]
    fn a_read_list_is_kept_per_alias_and_a_failure_is_not() {
        let loaded = Voices::Loaded {
            names: vec!["alba".into()],
            default: None,
            untranscribed: Vec::new(),
        };
        remember("audio/kept", &loaded);
        remember("audio/failed", &Voices::Failed("down".into()));
        assert_eq!(read_before("audio/kept"), Some(loaded));
        assert_eq!(read_before("audio/failed"), None);
        assert_eq!(read_before("audio/other"), None);
    }
}
