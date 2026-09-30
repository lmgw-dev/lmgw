//! The fields a thread's settings are edited with — prompt, temperature,
//! max tokens, sampling, reasoning, tool servers — and the patch they make.
//!
//! One set of fields for two forms: the open thread's settings drawer and a
//! folder's defaults ([`super::chat_folders`]), which are the same settings a
//! new thread starts with. A setting added to the thread joins both by
//! joining [`SettingsFields`] and [`draft_patch`].

use leptos::prelude::*;
use serde_json::{json, Value};

use super::chat::SettingsDraft;
use super::chat_knowledge::KbSection;
use super::chat_reasoning::{reasoning_patch, ReasoningFields};
use super::chat_sampling::{self, SamplingFields};
use crate::widgets::tool_picker::McpPicker;

/// Why the reasoning and sampling boxes as typed cannot be saved. Shown under
/// them, and blocking Save: the server makes the same checks.
#[derive(Clone, Copy)]
pub(super) struct DraftErrors {
    pub reasoning: Memo<Option<String>>,
    pub sampling: Memo<Option<String>>,
}

impl DraftErrors {
    pub(super) fn of(d: SettingsDraft) -> Self {
        Self {
            reasoning: Memo::new(move |_| {
                reasoning_patch(&d.think.get(), &d.effort.get(), &d.budget.get()).err()
            }),
            sampling: Memo::new(move |_| chat_sampling::sampling_patch(&d.sampling.text()).err()),
        }
    }

    /// Is anything unsavable? (tracked) `false` once the form is gone: a
    /// button in a modal's foot outlives the form by a beat.
    pub(super) fn any(&self) -> bool {
        self.reasoning.try_with(Option::is_some).unwrap_or(false)
            || self.sampling.try_with(Option::is_some).unwrap_or(false)
    }
}

/// The draft as a settings body — every key, `null` for a blank box — or the
/// first thing wrong with it, worded for a toast.
pub(super) fn draft_patch(d: &SettingsDraft) -> Result<Value, String> {
    let number = |what: &str, s: String| -> Result<Option<f64>, String> {
        match s.trim() {
            "" => Ok(None),
            s => s
                .parse()
                .map(Some)
                .map_err(|_| format!("{what} must be a number")),
        }
    };
    let temperature = number("temperature", d.temp.get_untracked())?;
    let max_tokens = match d.max_tok.get_untracked().trim() {
        "" => None,
        s => Some(
            s.parse::<i64>()
                .map_err(|_| "max tokens must be a number".to_string())?,
        ),
    };
    let reasoning = reasoning_patch(
        &d.think.get_untracked(),
        &d.effort.get_untracked(),
        &d.budget.get_untracked(),
    )?;
    let sampling = chat_sampling::sampling_patch(&d.sampling.text_untracked())?;
    let mut body = json!({
        "system_prompt": d.sys.get_untracked(),
        "temperature": temperature,
        "max_tokens": max_tokens,
        "mcp_tools": d.picked.get_untracked(),
    });
    if let (Some(b), Some(r)) = (body.as_object_mut(), reasoning.as_object()) {
        b.extend(r.clone());
    }
    if let (Some(b), Some(s)) = (body.as_object_mut(), sampling.as_object()) {
        b.extend(s.clone());
    }
    if let (Some(b), Some(k)) = (body.as_object_mut(), d.kb.patch()?.as_object()) {
        b.extend(k.clone());
    }
    Ok(body)
}

/// The settings boxes. `model` is the model the reasoning fields describe.
#[component]
pub(super) fn SettingsFields(
    draft: SettingsDraft,
    errors: DraftErrors,
    #[prop(into)] model: Signal<String>,
    prompt_label: &'static str,
) -> impl IntoView {
    let SettingsDraft {
        sys,
        temp,
        max_tok,
        think,
        effort,
        budget,
        sampling,
        picked,
        kb,
    } = draft;
    view! {
        <div class="field">
            <label>{prompt_label}</label>
            <textarea
                class="input ta"
                rows="6"
                prop:value=move || sys.get()
                on:input=move |ev| sys.set(event_target_value(&ev))
            ></textarea>
        </div>
        <div class="field-grid" style="--field-min:100px">
            <div class="field">
                <label>"Temperature"</label>
                <input
                    class="input mono"
                    placeholder="model default"
                    prop:value=move || temp.get()
                    on:input=move |ev| temp.set(event_target_value(&ev))
                />
            </div>
            <div class="field">
                <label>"Max tokens"</label>
                <input
                    class="input mono"
                    placeholder="model default"
                    prop:value=move || max_tok.get()
                    on:input=move |ev| max_tok.set(event_target_value(&ev))
                />
            </div>
        </div>
        <SamplingFields draft=sampling error=errors.sampling/>
        <ReasoningFields
            model=model
            think=think
            effort=effort
            budget=budget
            error=errors.reasoning
        />
        <McpPicker picked=picked/>
        <KbSection kb=kb/>
    }
}
