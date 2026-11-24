//! The editor's boxes: one signal each, and the views over them.

use leptos::prelude::*;
use lmgw_api_types::chat_profiles::{
    examples_from_text, examples_to_text, field, Example, Reasoning,
};

use super::model::{BlockMode, Form, StyleMode};

/// The fields the editor has changed against what is stored, by the wire's
/// names (`Form::changed`).
pub(super) type Changed = Memo<Vec<&'static str>>;

fn is_changed(changed: Changed, field: &'static str) -> Signal<bool> {
    Signal::derive(move || changed.with(|c| c.contains(&field)))
}
use crate::widgets::model_picker::ModelPicker;
use crate::widgets::voice_picker::{use_voices, VoiceInput};
use crate::widgets::{ActionRow, Ico, Icon, PrefGroup, StackRow, TextRow};

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

/// The Persona and Length rule rows.
#[component]
pub(super) fn TextGroup(st: State, changed: Changed) -> impl IntoView {
    view! {
        <PrefGroup title="Text">
            <TextRow
                title="Persona"
                sub="Who the model is and how it sounds. When set, it takes the place of the thread's own system prompt."
                value=st.persona
                dirty=is_changed(changed, field::PERSONA)
            />
            <TextRow
                title="Length rule"
                sub="A concrete rule, not \"short\": e.g. one to three sentences, details on request, no closing recap."
                value=st.length_rule
                dirty=is_changed(changed, field::LENGTH_RULE)
            />
        </PrefGroup>
    }
}

/// An icon button of an example row.
fn ex_btn(label: &'static str, ico: Ico, on: impl Fn() + 'static) -> impl IntoView {
    view! {
        <button type="button" class="icon-btn sm" title=label aria-label=label on:click=move |_| on()>
            <Icon ico=ico/>
        </button>
    }
}

#[component]
pub(super) fn ExamplesGroup(st: State, changed: Changed) -> impl IntoView {
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
    // Why the examples do not read: the text form's parse error, then one
    // sentence per half-filled row.
    let errors = Memo::new(move |_| {
        let mut out: Vec<String> = st.ex_err.get().into_iter().collect();
        out.extend(st.form().example_problems());
        out
    });
    view! {
        <PrefGroup
            title="Examples"
            desc="Three to five short exchanges teach the register best. They are static and go with every turn, so each one costs tokens (Count, below)."
        >
            <StackRow dirty=is_changed(changed, field::EXAMPLES)>
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
                                class="prow-area mono"
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
                                        class="prow-area"
                                        rows="2"
                                        placeholder="The user says…"
                                        prop:value=move || r.user.get()
                                        on:input=move |ev| r.user.set(event_target_value(&ev))
                                    ></textarea>
                                    <textarea
                                        class="prow-area"
                                        rows="2"
                                        placeholder="The model replies…"
                                        prop:value=move || r.reply.get()
                                        on:input=move |ev| r.reply.set(event_target_value(&ev))
                                    ></textarea>
                                </div>
                                <div class="pf-ex-acts">
                                    {ex_btn("Move up", Ico::Up, move || shift(r.key, -1))}
                                    {ex_btn("Move down", Ico::Down, move || shift(r.key, 1))}
                                    {ex_btn("Remove", Ico::Close, move || remove(r.key))}
                                </div>
                            </div>
                        </For>
                    </div>
                </Show>
                <For each=move || errors.get() key=|e| e.clone() let:e>
                    <div class="prow-err" role="alert">{e}</div>
                </For>
            </StackRow>
        </PrefGroup>
    }
}

#[component]
pub(super) fn VoiceBlockGroup(
    st: State,
    changed: Changed,
    /// The generic block as the last Show assembled it, as the greyed
    /// reference while "generic" is picked.
    reference: Signal<Option<String>>,
) -> impl IntoView {
    let is = move |m: BlockMode| Signal::derive(move || st.block_mode.get() == m);
    let problem = Memo::new(move |_| st.form().block_problem());
    view! {
        <PrefGroup
            title="Voice block"
            desc="The paragraph added to a spoken turn that tells the model to answer for the ear."
        >
            <StackRow dirty=is_changed(changed, field::VOICE_BLOCK)>
                <div class="seg" role="group" aria-label="Voice block">
                    {seg_btn("Generic", is(BlockMode::Generic), move || st.block_mode.set(BlockMode::Generic))}
                    {seg_btn("None", is(BlockMode::None), move || st.block_mode.set(BlockMode::None))}
                    {seg_btn("Own", is(BlockMode::Own), move || st.block_mode.set(BlockMode::Own))}
                </div>
                {move || match st.block_mode.get() {
                    BlockMode::Own => {
                        view! {
                            <textarea
                                class="prow-area"
                                rows="4"
                                aria-label="Own voice block"
                                prop:value=move || st.block_text.get()
                                on:input=move |ev| st.block_text.set(event_target_value(&ev))
                            ></textarea>
                        }
                            .into_any()
                    }
                    BlockMode::None => {
                        view! { <div class="pf-note">"Spoken turns get no voice block."</div> }
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
                                    <div class="pf-note">
                                        "The Chat's generic block. Its text shows here once the static part below is shown."
                                    </div>
                                }
                                    .into_any()
                            }
                        }
                    }
                }}
                {move || problem.get().map(|e| view! { <div class="prow-err" role="alert">{e}</div> })}
            </StackRow>
        </PrefGroup>
    }
}

#[component]
pub(super) fn BehaviourGroup(
    st: State,
    changed: Changed,
    /// The stored form: the voice rows mark what differs from it.
    base: RwSignal<Form>,
    list_id: String,
) -> impl IntoView {
    let is = move |r: Option<Reasoning>| Signal::derive(move || st.reasoning.get() == r);
    let style_is = move |m: StyleMode| Signal::derive(move || st.style_mode.get() == m);
    let list = use_voices(Signal::derive(move || st.tts_alias.get()));
    let tts_dirty =
        Signal::derive(move || base.with(|b| st.tts_alias.get().trim() != b.tts_alias.trim()));
    let voice_dirty =
        Signal::derive(move || base.with(|b| st.voice.get().trim() != b.voice.trim()));
    let style_dirty = Signal::derive(move || {
        base.with(|b| {
            let f = st.form();
            f.wire_voice().speech_style != b.wire_voice().speech_style
        })
    });
    let style_problem = Memo::new(move |_| st.form().style_problem());
    view! {
        <PrefGroup title="Behaviour">
            <ActionRow
                title="Reasoning"
                sub="Thinking is the costliest voice delay. Inherit leaves it to the thread, then the route's default."
                dirty=is_changed(changed, field::REASONING)
            >
                <div class="seg" role="group" aria-label="Reasoning">
                    {seg_btn("Inherit", is(None), move || st.reasoning.set(None))}
                    {seg_btn("On", is(Some(Reasoning::On)), move || st.reasoning.set(Some(Reasoning::On)))}
                    {seg_btn("Off", is(Some(Reasoning::Off)), move || st.reasoning.set(Some(Reasoning::Off)))}
                </div>
            </ActionRow>
            <StackRow title="Text-to-speech model" sub="Empty: the Chat's own." dirty=tts_dirty>
                <ModelPicker
                    value=st.tts_alias
                    tasks=&["tts", "vdes"]
                    empty_label="inherit".to_string()
                />
            </StackRow>
            <StackRow title="Voice" dirty=voice_dirty>
                <VoiceInput
                    value=Signal::derive(move || st.voice.get())
                    on_input=Callback::new(move |v: String| st.voice.set(v))
                    list=list
                    list_id=list_id
                    placeholder=Signal::derive(|| "inherit".to_string())
                />
            </StackRow>
            <StackRow
                title="Speech style"
                sub="Given to the text-to-speech model as it speaks. Inherit takes the thread's, then the Chat's; none sends no style at all."
                dirty=style_dirty
            >
                <div class="seg" role="group" aria-label="Speech style">
                    {seg_btn("Inherit", style_is(StyleMode::Inherit), move || st.style_mode.set(StyleMode::Inherit))}
                    {seg_btn("None", style_is(StyleMode::None), move || st.style_mode.set(StyleMode::None))}
                    {seg_btn("Own", style_is(StyleMode::Own), move || st.style_mode.set(StyleMode::Own))}
                </div>
                <Show when=move || st.style_mode.get() == StyleMode::Own>
                    <input
                        class="prow-entry"
                        spellcheck="false"
                        aria-label="Own speech style"
                        prop:value=move || st.speech_style.get()
                        on:input=move |ev| st.speech_style.set(event_target_value(&ev))
                    />
                </Show>
                {move || style_problem.get().map(|e| view! { <div class="prow-err" role="alert">{e}</div> })}
            </StackRow>
        </PrefGroup>
    }
}
