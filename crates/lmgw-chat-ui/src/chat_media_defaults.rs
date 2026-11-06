//! The image and audio tools' default models (chat-media-tools design §3.8)
//! as the pickers show them: Settings → Chat's (`settings::chat_media`), a
//! folder's and a thread's (the settings drawer and the folder form, through
//! [`MediaSection`]).
//!
//! Every list is the gateway's (`GET /chat/api/media/defaults`): the same
//! rows and aliases each tool's `model` enum offers, with a label. A stored
//! name the gateway no longer resolves stays picked and is marked
//! unavailable with the gateway's reason — never replaced by another model.
//! In a folder or a thread the empty choice is "Inherit", which names what
//! it inherits, and "None" is a choice of its own (no default whatever the
//! levels above say).

use leptos::prelude::*;
use lmgw_api_types::chat_media::{MediaDefaults, MediaDefaultsView, MediaField};

use lmgw_ui_kit::widgets::Select;

/// A folder's or thread's "no default" in the draft (stored as `""`); the
/// draft's `""` is "inherit" (stored as absent).
pub(crate) const NONE: &str = "-";

/// The section's five boxes, as the drawer and the folder form hold them.
#[derive(Clone, Copy)]
pub(crate) struct MediaDraft {
    fields: [RwSignal<String>; 5],
}

fn index(f: MediaField) -> usize {
    MediaField::ALL.iter().position(|x| *x == f).unwrap_or(0)
}

impl MediaDraft {
    pub(crate) fn new() -> Self {
        Self {
            fields: std::array::from_fn(|_| RwSignal::new(String::new())),
        }
    }

    pub(crate) fn of(&self, f: MediaField) -> RwSignal<String> {
        self.fields[index(f)]
    }

    /// A level's stored defaults into the boxes.
    pub(crate) fn load(&self, m: &MediaDefaults) {
        for f in MediaField::ALL {
            self.of(f).set(match m.get(f) {
                None => String::new(),
                Some("") => NONE.to_string(),
                Some(v) => v.to_string(),
            });
        }
    }

    /// The boxes as a level's defaults (untracked).
    pub(crate) fn patch(&self) -> MediaDefaults {
        let mut m = MediaDefaults::default();
        for f in MediaField::ALL {
            *m.slot(f) = match self.of(f).get_untracked().trim() {
                "" => None,
                NONE => Some(String::new()),
                v => Some(v.to_string()),
            };
        }
        m
    }

    /// Whether the boxes differ from `m` (tracked).
    pub(crate) fn differs_from(&self, m: &MediaDefaults) -> bool {
        MediaField::ALL.iter().any(|f| {
            let stored = match m.get(*f) {
                None => "",
                Some("") => NONE,
                Some(v) => v,
            };
            self.of(*f).with(|v| v.trim() != stored)
        })
    }
}

/// The gateway's view for `query` (`""`, `?folder=N`, `?thread=N`), read
/// again whenever `again` changes.
pub fn use_view(query: Signal<String>, again: Signal<String>) -> Memo<Option<MediaDefaultsView>> {
    let http = lmgw_ui_kit::http::use_http();
    let res = LocalResource::new(move || {
        let q = query.get();
        again.track();
        async move {
            http.get::<MediaDefaultsView>(format!("/chat/api/media/defaults{q}"))
                .await
                .ok()
        }
    });
    Memo::new(move |_| res.get().flatten())
}

/// A name as a person reads it: its picker label, or the bare name with
/// why it is unavailable.
fn named(v: &MediaDefaultsView, f: MediaField, name: &str) -> String {
    if let Some(c) = v.choices.of(f).iter().find(|c| c.name == name) {
        return c.label.clone();
    }
    match v.why_unavailable(f, name) {
        Some(_) => format!("{name} — unavailable"),
        None => name.to_string(),
    }
}

/// What "Inherit" takes, in words.
fn inherited_said(v: &MediaDefaultsView, f: MediaField) -> String {
    match v.inherited.get(f) {
        None | Some("") if f == MediaField::Voice => "the model's default voice".to_string(),
        None | Some("") => "none, the model chooses".to_string(),
        Some(n) => named(v, f, n),
    }
}

/// The sentence under a box whose name does not resolve (`inherited`: the
/// name is the level above's).
fn unavailable_line(
    v: &MediaDefaultsView,
    f: MediaField,
    draft: &str,
    inherit: bool,
) -> Option<(String, String)> {
    let (name, whose) = match draft {
        NONE => return None,
        "" if inherit => (v.inherited.get(f)?.to_string(), "The inherited "),
        "" => return None,
        n => (n.to_string(), ""),
    };
    if name.is_empty() {
        return None;
    }
    let in_list = f == MediaField::Voice || v.choices.of(f).iter().any(|c| c.name == name);
    let reason = match v.why_unavailable(f, &name) {
        Some(r) => r.to_string(),
        None if in_list => return None,
        None => "not offered for this tool now".to_string(),
    };
    let then = if f == MediaField::Voice {
        "the model's default voice is used"
    } else {
        "a call must name its model until you pick another or this one comes back"
    };
    Some((
        name.clone(),
        format!("{whose}{name} is unavailable: {reason}. Until then {then}."),
    ))
}

/// One model box: the tool's choices, the empty choice (`inherit`: "Inherit
/// — what", else "None — the model chooses"), a stored name that no longer
/// resolves kept and marked.
#[component]
pub fn MediaModelBox(
    value: RwSignal<String>,
    field: MediaField,
    view: Memo<Option<MediaDefaultsView>>,
    inherit: bool,
    /// Without its own label: Settings' `Field` carries it.
    #[prop(optional)]
    bare: bool,
) -> impl IntoView {
    let options = Signal::derive(move || {
        let mut out: Vec<(String, String)> = Vec::new();
        let v = view.get();
        if inherit {
            let said = v
                .as_ref()
                .map(|v| inherited_said(v, field))
                .unwrap_or_else(|| "…".to_string());
            out.push((String::new(), format!("Inherit — {said}")));
            out.push((NONE.to_string(), "None — the model chooses".to_string()));
        } else {
            out.push((String::new(), "None — the model chooses".to_string()));
        }
        let Some(v) = v else {
            return out;
        };
        out.extend(
            v.choices
                .of(field)
                .iter()
                .map(|c| (c.name.clone(), c.label.clone())),
        );
        let cur = value.get();
        let cur = cur.trim();
        if !cur.is_empty() && cur != NONE && !out.iter().any(|(n, _)| n == cur) {
            out.push((cur.to_string(), format!("{cur} — unavailable")));
        }
        out
    });
    let warn = move || {
        view.with(|v| {
            v.as_ref()
                .and_then(|v| unavailable_line(v, field, value.get().trim(), inherit))
        })
    };
    let inner = move || {
        view! {
            <Select value=value options=options/>
            {move || {
                warn()
                    .map(|(_, line)| {
                        view! {
                            <div class="field-hint warn" data-media-unavailable=field.as_str()>
                                {line}
                            </div>
                        }
                    })
            }}
        }
    };
    if bare {
        return view! { <div class="media-box" data-media-field=field.as_str()>{inner()}</div> }
            .into_any();
    }
    view! {
        <div class="field" data-media-field=field.as_str()>
            <label>{field.label()}</label>
            {inner()}
        </div>
    }
    .into_any()
}

/// The typed voice box's "Type a voice…" choice (never stored).
const TYPED: &str = "+";

/// What the typed voice box's choice shows for draft value `v`: Inherit
/// (`""`), the model's default voice ([`NONE`]), else a typed name.
fn typed_choice(v: &str) -> String {
    match v.trim() {
        "" => String::new(),
        NONE => NONE.to_string(),
        _ => TYPED.to_string(),
    }
}

/// The voice box: the voices of the speech model it goes with (`speak`, the
/// draft's box, or what it inherits); for a cloud model, whose voices are
/// the provider's own, the same choices with "Type a voice…" and a box for
/// the name.
#[component]
pub fn MediaVoiceBox(
    value: RwSignal<String>,
    speak: Signal<String>,
    view: Memo<Option<MediaDefaultsView>>,
    inherit: bool,
    /// Without its own label: Settings' `Field` carries it.
    #[prop(optional)]
    bare: bool,
) -> impl IntoView {
    let f = MediaField::Voice;
    // The speech model the voice goes with.
    let model = Signal::derive(move || {
        let s = speak.get();
        match s.trim() {
            NONE => String::new(),
            "" if inherit => view
                .with(|v| v.as_ref().and_then(|v| v.inherited.speak.clone()))
                .unwrap_or_default(),
            m => m.to_string(),
        }
    });
    let voices = Signal::derive(move || {
        let m = model.get();
        view.with(|v| v.as_ref().and_then(|v| v.choices.voices.get(&m).cloned()))
    });
    let options = Signal::derive(move || {
        let mut out: Vec<(String, String)> = Vec::new();
        if inherit {
            let said = view
                .with(|v| v.as_ref().map(|v| inherited_said(v, f)))
                .unwrap_or_else(|| "…".to_string());
            out.push((String::new(), format!("Inherit — {said}")));
            out.push((NONE.to_string(), "The model's default voice".to_string()));
        } else {
            out.push((String::new(), "The model's default voice".to_string()));
        }
        out.extend(
            voices
                .get()
                .unwrap_or_default()
                .into_iter()
                .map(|v| (v.clone(), v)),
        );
        let cur = value.get();
        let cur = cur.trim();
        if !cur.is_empty() && cur != NONE && !out.iter().any(|(n, _)| n == cur) {
            out.push((cur.to_string(), format!("{cur} — unavailable")));
        }
        out
    });
    let warn = move || {
        view.with(|v| {
            v.as_ref()
                .and_then(|v| unavailable_line(v, f, value.get().trim(), inherit))
        })
    };
    // A model is picked whose voices lmgw does not list: a cloud alias.
    let typed = Signal::derive(move || !model.get().is_empty() && voices.get().is_none());
    // Its voice: the same Inherit and "the model's default voice" as a
    // row's, and "Type a voice…", which opens a box for the provider's
    // voice name.
    let choice = RwSignal::new(typed_choice(&value.get_untracked()));
    // "Type a voice…" picked and the box still empty (the draft's `""`):
    // the choice stays, rather than reading as Inherit.
    let typing = RwSignal::new(choice.get_untracked() == TYPED);
    Effect::watch(
        move || value.get(),
        move |v, _, _| {
            let c = typed_choice(v);
            let empty_box = typing.get_untracked() && v.trim().is_empty();
            if !empty_box && choice.get_untracked() != c {
                choice.set(c);
            }
        },
        false,
    );
    Effect::watch(
        move || choice.get(),
        move |c, _, _| {
            if c == TYPED {
                typing.set(true);
                if value.get_untracked() == NONE {
                    value.set(String::new());
                }
            } else {
                typing.set(false);
                if value.get_untracked() != *c {
                    value.set(c.clone());
                }
            }
        },
        false,
    );
    let typed_options = Signal::derive(move || {
        let mut out: Vec<(String, String)> = Vec::new();
        if inherit {
            let said = view
                .with(|v| v.as_ref().map(|v| inherited_said(v, f)))
                .unwrap_or_else(|| "…".to_string());
            out.push((String::new(), format!("Inherit — {said}")));
            out.push((NONE.to_string(), "The model's default voice".to_string()));
        } else {
            out.push((String::new(), "The model's default voice".to_string()));
        }
        out.push((TYPED.to_string(), "Type a voice…".to_string()));
        out
    });
    let inner = move || {
        view! {
            <Show
                when=move || typed.get()
                fallback=move || view! { <Select value=value options=options/> }
            >
                <Select value=choice options=typed_options/>
                <Show when=move || choice.get() == TYPED>
                    <input
                        class="input mono media-voice-typed"
                        placeholder="The provider's voice name"
                        aria-label="Voice name"
                        spellcheck="false"
                        prop:value=move || if value.get() == NONE { String::new() } else { value.get() }
                        on:input=move |ev| value.set(event_target_value(&ev))
                    />
                </Show>
            </Show>
            {move || {
                (model.get().is_empty() && !bare)
                    .then(|| {
                        view! {
                            <div class="field-hint">
                                "Goes with the default speech model: pick one to choose its voice."
                            </div>
                        }
                    })
            }}
            {move || {
                warn()
                    .map(|(_, line)| {
                        view! {
                            <div class="field-hint warn" data-media-unavailable="voice">
                                {line}
                            </div>
                        }
                    })
            }}
        }
    };
    if bare {
        return view! { <div class="media-box" data-media-field="voice">{inner()}</div> }
            .into_any();
    }
    view! {
        <div class="field" data-media-field="voice">
            <label>{f.label()}</label>
            {inner()}
        </div>
    }
    .into_any()
}

/// The section of a thread's settings and a folder's form: its own boxes,
/// each "Inherit" naming what the level above holds. `query` names the
/// level (`?thread=N`, `?folder=N`).
#[component]
pub(crate) fn MediaSection(
    draft: MediaDraft,
    #[prop(into)] query: Signal<String>,
    /// Read the gateway's view again when this changes (a save).
    #[prop(into)]
    again: Signal<String>,
    /// A folder's form: every thread in the folder reads these at its turn.
    folder: bool,
) -> impl IntoView {
    let view = use_view(query, again);
    let speak = draft.of(MediaField::Speak);
    let hint = if folder {
        "The model each tool uses when the chat model names none, for every chat in this folder \
         (read at each turn, not copied); a chat can pick its own. Inherit takes Settings → Chat's."
    } else {
        "The model each tool uses when the chat model names none. Inherit takes the folder's, \
         then Settings → Chat's."
    };
    view! {
        <div class="field media-defaults" data-media-defaults="">
            <label>"Picture and sound tools"</label>
            <div class="field-hint">{hint}</div>
            <div class="field-grid" style="--field-min:220px">
                {MediaField::MODELS
                    .into_iter()
                    .map(|f| {
                        view! { <MediaModelBox value=draft.of(f) field=f view=view inherit=true/> }
                    })
                    .collect_view()}
                <MediaVoiceBox
                    value=draft.of(MediaField::Voice)
                    speak=Signal::derive(move || speak.get())
                    view=view
                    inherit=true
                />
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::chat_media::{MediaChoice, MediaUnavailable};

    fn view_with(inherited: MediaDefaults) -> MediaDefaultsView {
        MediaDefaultsView {
            choices: lmgw_api_types::chat_media::MediaChoices {
                generate: vec![MediaChoice {
                    name: "image/z".into(),
                    label: "image/z · on this machine".into(),
                    local: true,
                }],
                ..Default::default()
            },
            inherited,
            unavailable: vec![MediaUnavailable {
                field: "generate".into(),
                name: "image/y".into(),
                reason: "the image model is switched off".into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn inherit_names_what_it_takes_and_a_stale_name_says_why() {
        let v = view_with(MediaDefaults {
            generate: Some("image/y".into()),
            ..Default::default()
        });
        assert_eq!(
            inherited_said(&v, MediaField::Generate),
            "image/y — unavailable"
        );
        assert_eq!(
            inherited_said(&v, MediaField::Edit),
            "none, the model chooses"
        );
        let (_, line) = unavailable_line(&v, MediaField::Generate, "", true).unwrap();
        assert!(
            line.starts_with(
                "The inherited image/y is unavailable: the image model is switched off."
            ),
            "{line}"
        );
        assert!(unavailable_line(&v, MediaField::Generate, "image/z", true).is_none());
        assert!(unavailable_line(&v, MediaField::Generate, NONE, true).is_none());
        let (_, line) = unavailable_line(&v, MediaField::Generate, "image/y", false).unwrap();
        assert!(line.starts_with("image/y is unavailable"), "{line}");
    }

    #[test]
    fn a_typed_voice_keeps_inherit_and_the_default_voice_apart() {
        assert_eq!(typed_choice(""), "");
        assert_eq!(typed_choice(NONE), NONE);
        assert_eq!(typed_choice(" alloy "), TYPED);
    }
}
