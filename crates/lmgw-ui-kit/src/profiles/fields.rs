//! The editor's boxes: one signal each, and the views over them.

use leptos::prelude::*;
use lmgw_api_types::chat_profiles::{examples_from_text, examples_to_text, Example, Reasoning};

use super::model::{BlockMode, Form, StyleMode};
use crate::widgets::form::Field;
use crate::widgets::model_picker::ModelPicker;
use crate::widgets::voice_picker::{use_voices, VoiceInput};

/// One example row: the key keeps its boxes when others move.
#[derive(Clone, Copy)]
pub(super) struct Row {
    pub key: u64,
    pub user: RwSignal<String>,
    pub reply: RwSignal<String>,
}

#[derive(Clone, Copy)]
pub(super) struct State {
    pub name: RwSignal<String>,
    pub persona: RwSignal<String>,
    pub length_rule: RwSignal<String>,
    pub rows: RwSignal<Vec<Row>>,
    next_key: StoredValue<u64>,
    /// `Some` while the examples are edited in their text form.
    pub ex_text: RwSignal<Option<String>>,
    /// Why the text form does not read, while it does not.
    pub ex_err: RwSignal<Option<String>>,
    pub block_mode: RwSignal<BlockMode>,
    pub block_text: RwSignal<String>,
    pub reasoning: RwSignal<Option<Reasoning>>,
    pub tts_alias: RwSignal<String>,
    pub voice: RwSignal<String>,
    pub style_mode: RwSignal<StyleMode>,
    pub speech_style: RwSignal<String>,
}

impl State {
    pub fn new(form: &Form) -> Self {
        let s = Self {
            name: RwSignal::new(String::new()),
            persona: RwSignal::new(String::new()),
            length_rule: RwSignal::new(String::new()),
            rows: RwSignal::new(Vec::new()),
            next_key: StoredValue::new(0),
            ex_text: RwSignal::new(None),
            ex_err: RwSignal::new(None),
            block_mode: RwSignal::new(BlockMode::Generic),
            block_text: RwSignal::new(String::new()),
            reasoning: RwSignal::new(None),
            tts_alias: RwSignal::new(String::new()),
            voice: RwSignal::new(String::new()),
            style_mode: RwSignal::new(StyleMode::Inherit),
            speech_style: RwSignal::new(String::new()),
        };
        s.load(form);
        s
    }

    fn row(&self, e: &Example) -> Row {
        let key = self.next_key.get_value();
        self.next_key.set_value(key + 1);
        Row {
            key,
            user: RwSignal::new(e.user.clone()),
            reply: RwSignal::new(e.reply.clone()),
        }
    }

    fn set_rows(&self, examples: &[Example]) {
        let rows = examples.iter().map(|e| self.row(e)).collect();
        self.rows.set(rows);
    }

    /// Put a form into the boxes (a saved profile, a reset).
    pub fn load(&self, f: &Form) {
        self.name.set(f.name.clone());
        self.persona.set(f.persona.clone());
        self.length_rule.set(f.length_rule.clone());
        self.set_rows(&f.examples);
        self.ex_text.set(None);
        self.ex_err.set(None);
        self.block_mode
            .set(f.block_mode.unwrap_or(BlockMode::Generic));
        self.block_text.set(f.block_text.clone());
        self.reasoning.set(f.reasoning);
        self.tts_alias.set(f.tts_alias.clone());
        self.voice.set(f.voice.clone());
        self.style_mode.set(f.style_mode);
        self.speech_style.set(f.speech_style.clone());
    }

    /// The boxes as a form; reading it tracks every box.
    pub fn form(&self) -> Form {
        Form {
            name: self.name.get(),
            persona: self.persona.get(),
            length_rule: self.length_rule.get(),
            examples: self
                .rows
                .get()
                .iter()
                .map(|r| Example {
                    user: r.user.get(),
                    reply: r.reply.get(),
                })
                .collect(),
            block_mode: Some(self.block_mode.get()),
            block_text: self.block_text.get(),
            reasoning: self.reasoning.get(),
            tts_alias: self.tts_alias.get(),
            voice: self.voice.get(),
            style_mode: self.style_mode.get(),
            speech_style: self.speech_style.get(),
        }
    }
}

/// One segment of a `.seg` choice.
pub(super) fn seg_btn(
    label: &'static str,
    active: Signal<bool>,
    pick: impl Fn() + 'static,
) -> impl IntoView {
    view! {
        <button
            type="button"
            class="seg-btn"
            class:active=move || active.get()
            aria-pressed=move || active.get().to_string()
            on:click=move |_| pick()
        >
            {label}
        </button>
    }
}

#[component]
pub(super) fn TextFields(st: State) -> impl IntoView {
    view! {
        <Field
            label="Persona"
            hint="Who the model is and how it sounds. When set, it takes the place of the thread's own system prompt."
            wide=true
        >
            <textarea
                class="input ta"
                rows="5"
                prop:value=move || st.persona.get()
                on:input=move |ev| st.persona.set(event_target_value(&ev))
            ></textarea>
        </Field>
        <Field
            label="Length rule"
            hint="A concrete rule, not \"short\": e.g. one to three sentences, details on request, no closing recap."
            wide=true
        >
            <textarea
                class="input ta"
                rows="3"
                prop:value=move || st.length_rule.get()
                on:input=move |ev| st.length_rule.set(event_target_value(&ev))
            ></textarea>
        </Field>
    }
}

#[component]
pub(super) fn ExamplesField(st: State) -> impl IntoView {
    let in_text = move || st.ex_text.get().is_some();
    let toggle = move |_| {
        if in_text() {
            st.ex_text.set(None);
            st.ex_err.set(None);
        } else {
            let wire = st.form().wire_examples();
            st.ex_text.set(Some(examples_to_text(&wire)));
        }
    };
    let add = move |_| {
        let r = st.row(&Example::default());
        st.rows.update(|v| v.push(r));
    };
    let on_text = move |ev| {
        let t = event_target_value(&ev);
        st.ex_text.set(Some(t.clone()));
        match examples_from_text(&t) {
            Ok(v) => {
                st.set_rows(&v);
                st.ex_err.set(None);
            }
            Err(e) => st.ex_err.set(Some(e)),
        }
    };
    let remove = move |key: u64| st.rows.update(|v| v.retain(|r| r.key != key));
    let shift = move |key: u64, by: isize| {
        st.rows.update(|v| {
            if let Some(i) = v.iter().position(|r| r.key == key) {
                let j = i as isize + by;
                if (0..v.len() as isize).contains(&j) {
                    v.swap(i, j as usize);
                }
            }
        })
    };
    view! {
        <Field
            label="Examples"
            hint="Three to five short exchanges teach the register best. They are static and go with every turn, so each one costs tokens (Count, below)."
            wide=true
            error=Signal::derive(move || st.ex_err.get())
        >
            <div class="pf-ex-bar">
                <button type="button" class="btn sm" on:click=add disabled=in_text>
                    "Add example"
                </button>
                <button type="button" class="btn ghost sm" on:click=toggle>
                    {move || if in_text() { "Edit as rows" } else { "Edit as text" }}
                </button>
            </div>
            <Show
                when=move || !in_text()
                fallback=move || {
                    view! {
                        <textarea
                            class="input ta mono"
                            rows="10"
                            spellcheck="false"
                            placeholder="User: …\nReply: …"
                            prop:value=move || st.ex_text.get().unwrap_or_default()
                            on:input=on_text
                        ></textarea>
                    }
                }
            >
                <div class="pf-ex-list">
                    <For each=move || st.rows.get() key=|r| r.key let:r>
                        <div class="pf-ex">
                            <div class="pf-ex-n">
                                {move || {
                                    st.rows
                                        .get()
                                        .iter()
                                        .position(|x| x.key == r.key)
                                        .map(|i| (i + 1).to_string())
                                }}
                            </div>
                            <div class="pf-ex-boxes">
                                <textarea
                                    class="input ta"
                                    rows="2"
                                    placeholder="The user says…"
                                    prop:value=move || r.user.get()
                                    on:input=move |ev| r.user.set(event_target_value(&ev))
                                ></textarea>
                                <textarea
                                    class="input ta"
                                    rows="2"
                                    placeholder="The model replies…"
                                    prop:value=move || r.reply.get()
                                    on:input=move |ev| r.reply.set(event_target_value(&ev))
                                ></textarea>
                            </div>
                            <div class="pf-ex-acts">
                                <button
                                    type="button"
                                    class="btn ghost sm"
                                    title="Move up"
                                    aria-label="Move up"
                                    on:click=move |_| shift(r.key, -1)
                                >
                                    "↑"
                                </button>
                                <button
                                    type="button"
                                    class="btn ghost sm"
                                    title="Move down"
                                    aria-label="Move down"
                                    on:click=move |_| shift(r.key, 1)
                                >
                                    "↓"
                                </button>
                                <button
                                    type="button"
                                    class="btn ghost sm"
                                    title="Remove"
                                    aria-label="Remove"
                                    on:click=move |_| remove(r.key)
                                >
                                    "✕"
                                </button>
                            </div>
                        </div>
                    </For>
                </div>
            </Show>
        </Field>
    }
}

#[component]
pub(super) fn VoiceBlockField(
    st: State,
    /// The generic block as the last Show assembled it, as the greyed
    /// reference while "generic" is picked.
    reference: Signal<Option<String>>,
) -> impl IntoView {
    let is = move |m: BlockMode| Signal::derive(move || st.block_mode.get() == m);
    view! {
        <Field
            label="Voice block"
            hint="The paragraph added to a spoken turn that tells the model to answer for the ear."
            wide=true
        >
            <div class="seg" role="group" aria-label="Voice block">
                {seg_btn("Generic", is(BlockMode::Generic), move || st.block_mode.set(BlockMode::Generic))}
                {seg_btn("None", is(BlockMode::None), move || st.block_mode.set(BlockMode::None))}
                {seg_btn("Own", is(BlockMode::Own), move || st.block_mode.set(BlockMode::Own))}
            </div>
            {move || match st.block_mode.get() {
                BlockMode::Own => {
                    view! {
                        <textarea
                            class="input ta"
                            rows="4"
                            prop:value=move || st.block_text.get()
                            on:input=move |ev| st.block_text.set(event_target_value(&ev))
                        ></textarea>
                    }
                        .into_any()
                }
                BlockMode::None => {
                    view! { <div class="field-hint">"Spoken turns get no voice block."</div> }
                        .into_any()
                }
                BlockMode::Generic => {
                    match reference.get() {
                        Some(t) => {
                            view! {
                                <pre class="pf-ref" title="The voice turn as it assembles now">
                                    {t}
                                </pre>
                            }
                                .into_any()
                        }
                        None => {
                            view! {
                                <div class="field-hint">
                                    "The Chat's generic block. Its text shows here once the static part below is shown."
                                </div>
                            }
                                .into_any()
                        }
                    }
                }
            }}
        </Field>
    }
}

#[component]
pub(super) fn BehaviourFields(st: State, list_id: String) -> impl IntoView {
    let is = move |r: Option<Reasoning>| Signal::derive(move || st.reasoning.get() == r);
    let style_is = move |m: StyleMode| Signal::derive(move || st.style_mode.get() == m);
    let list = use_voices(Signal::derive(move || st.tts_alias.get()));
    view! {
        <Field
            label="Reasoning"
            hint="Thinking is the costliest voice delay. Inherit leaves it to the thread, then the route's default."
        >
            <div class="seg" role="group" aria-label="Reasoning">
                {seg_btn("Inherit", is(None), move || st.reasoning.set(None))}
                {seg_btn("On", is(Some(Reasoning::On)), move || st.reasoning.set(Some(Reasoning::On)))}
                {seg_btn("Off", is(Some(Reasoning::Off)), move || st.reasoning.set(Some(Reasoning::Off)))}
            </div>
        </Field>
        <div class="field-grid pf-voice" style="--field-min:220px">
            <Field
                label="Text-to-speech model"
                hint="Empty: the Chat's own."
            >
                <ModelPicker
                    value=st.tts_alias
                    tasks=&["tts", "vdes"]
                    empty_label="inherit".to_string()
                />
            </Field>
            <Field label="Voice">
                <VoiceInput
                    value=Signal::derive(move || st.voice.get())
                    on_input=Callback::new(move |v: String| st.voice.set(v))
                    list=list
                    list_id=list_id
                    placeholder=Signal::derive(|| "inherit".to_string())
                />
            </Field>
            <Field
                label="Speech style"
                hint="Given to the text-to-speech model as it speaks. Inherit takes the thread's, then the Chat's; none sends no style at all."
                wide=true
            >
                <div class="seg" role="group" aria-label="Speech style">
                    {seg_btn("Inherit", style_is(StyleMode::Inherit), move || st.style_mode.set(StyleMode::Inherit))}
                    {seg_btn("None", style_is(StyleMode::None), move || st.style_mode.set(StyleMode::None))}
                    {seg_btn("Own", style_is(StyleMode::Own), move || st.style_mode.set(StyleMode::Own))}
                </div>
                <Show when=move || st.style_mode.get() == StyleMode::Own>
                    <input
                        class="input"
                        spellcheck="false"
                        prop:value=move || st.speech_style.get()
                        on:input=move |ev| st.speech_style.set(event_target_value(&ev))
                    />
                </Show>
            </Field>
        </div>
    }
}
