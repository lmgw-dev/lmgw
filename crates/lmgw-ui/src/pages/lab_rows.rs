//! Small pieces the labs' preference rows share (modern design, WP5): a
//! file picker as a row's button, and the prompt as a rounded field under
//! its heading.

use leptos::prelude::*;

use crate::widgets::{Ico, Icon};

/// A button that opens the file dialog: a label around a hidden file input,
/// so the row shows a button, not the browser's own "Browse… No file" box.
/// `on_file` gets the input's change event, as a bare `<input type=file>`
/// would.
#[component]
pub(super) fn FilePick(
    /// The dialog's filter (`image/*`, `audio/*`).
    accept: &'static str,
    /// The button's words.
    #[prop(default = "Choose…")]
    label: &'static str,
    /// The tooltip.
    #[prop(optional)]
    title: &'static str,
    /// Several files at once.
    #[prop(optional)]
    multiple: bool,
    #[prop(into, default = Signal::derive(|| false))] disabled: Signal<bool>,
    on_file: Callback<web_sys::Event>,
) -> impl IntoView {
    view! {
        <label class="btn sm file-pick" class:disabled=move || disabled.get() title=title>
            <Icon ico=Ico::Upload/>
            {label}
            <input
                class="file-hidden"
                type="file"
                accept=accept
                multiple=multiple
                disabled=move || disabled.get()
                on:change=move |ev| on_file.run(ev)
            />
        </label>
    }
}

/// The prompt: a heading and a rounded text field on the pane, not a row in
/// a box, so it reads as the lab's main input (the Chat's composer).
#[component]
pub(super) fn PromptField(
    #[prop(into)] title: TextProp,
    value: RwSignal<String>,
    #[prop(into)] placeholder: TextProp,
    /// Extra classes on the textarea (`vl-text`).
    #[prop(optional)]
    class: &'static str,
    /// On the heading's line, at its end.
    #[prop(optional, into)]
    suffix: Option<ViewFn>,
) -> impl IntoView {
    let label = title.clone();
    view! {
        <section class="pref-group lab-prompt">
            <div class="pref-head">
                <h2>{move || title.get()}</h2>
                {suffix.map(|s| view! { <span class="pref-suffix">{s.run()}</span> })}
            </div>
            <textarea
                class=format!("input ta lab-prompt-field {class}")
                aria-label=move || label.get().to_string()
                placeholder=move || placeholder.get().to_string()
                prop:value=move || value.get()
                on:input=move |ev| value.set(event_target_value(&ev))
            ></textarea>
        </section>
    }
}
