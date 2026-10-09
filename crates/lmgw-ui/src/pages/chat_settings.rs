//! The fields a thread's settings are edited with — prompt, temperature,
//! max tokens, sampling, reasoning, tool servers, knowledge, voice — and the
//! patch they make.
//!
//! One set of fields for two forms: the open thread's settings drawer and a
//! folder's defaults ([`super::chat_folders`]), which are the same settings a
//! new thread starts with. A setting added to the thread joins both by
//! joining [`SettingsFields`] and [`draft_patch`].

use leptos::prelude::*;
use serde_json::{json, Value};

use super::chat::SettingsDraft;
use super::chat_knowledge::KbSection;
use super::chat_profiles::{use_profile_dir, ProfileField};
use super::chat_reasoning::{reasoning_patch, ReasoningFields};
use super::chat_sampling::{self, SamplingFields};
use super::chat_voice::{VoiceResolved, VoiceSection};
use crate::widgets::tool_picker::McpPicker;

/// Why the reasoning and sampling boxes as typed cannot be saved. Shown under
/// them, and blocking Save: the server makes the same checks.
#[derive(Clone, Copy)]
pub(super) struct DraftErrors {
    pub reasoning: Memo<Option<String>>,
    pub sampling: Memo<Option<String>>,
    pub voice: Memo<Option<String>>,
}

impl DraftErrors {
    pub(super) fn of(d: SettingsDraft) -> Self {
        Self {
            reasoning: Memo::new(move |_| {
                reasoning_patch(&d.think.get(), &d.effort.get(), &d.budget.get()).err()
            }),
            sampling: Memo::new(move |_| chat_sampling::sampling_patch(&d.sampling.text()).err()),
            voice: Memo::new(move |_| d.voice.error()),
        }
    }

    /// Is anything unsavable? (tracked) `false` once the form is gone: a
    /// button in a modal's foot outlives the form by a beat.
    pub(super) fn any(&self) -> bool {
        self.reasoning.try_with(Option::is_some).unwrap_or(false)
            || self.sampling.try_with(Option::is_some).unwrap_or(false)
            || self.voice.try_with(Option::is_some).unwrap_or(false)
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
    body["voice"] = d.voice.patch()?;
    body["profile_id"] = match d.profile.get_untracked().trim().parse::<i64>() {
        Ok(id) => json!(id),
        Err(_) => Value::Null,
    };
    Ok(body)
}

/// The settings boxes. `model` is the model the reasoning fields describe;
/// `voice_resolved` is the thread's voice resolution (none for a folder's
/// defaults).
#[component]
pub(super) fn SettingsFields(
    draft: SettingsDraft,
    errors: DraftErrors,
    #[prop(into)] model: Signal<String>,
    prompt_label: &'static str,
    #[prop(optional, into)] voice_resolved: Option<Signal<Option<VoiceResolved>>>,
    /// The open thread's id, for the profile editor's link (none for a folder).
    #[prop(optional, into)]
    thread_id: Option<Signal<Option<i64>>>,
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
        voice,
        profile,
    } = draft;
    let dir = use_profile_dir();
    // The profile as the form picks it (not yet as saved): its persona takes
    // the system prompt's place, its reasoning stands in for a blank one.
    let prof = Memo::new(move |_| dir.and_then(|d| profile.with(|v| d.of_value(v))));
    let replaced = Memo::new(move |_| {
        prof.with(|p| {
            p.as_ref()
                .filter(|p| !p.persona.trim().is_empty())
                .map(|p| p.name.clone())
        })
    });
    let voice_section = match voice_resolved {
        Some(r) => view! { <VoiceSection draft=voice resolved=r/> }.into_any(),
        None => view! { <VoiceSection draft=voice/> }.into_any(),
    };
    view! {
        <ProfileField
            value=profile
            thread=thread_id.unwrap_or(Signal::stored(None))
            folder=thread_id.is_none()
        />
        <div class="field">
            <label>{prompt_label}</label>
            <textarea
                class="input ta"
                rows="6"
                disabled=move || replaced.with(Option::is_some)
                prop:value=move || sys.get()
                on:input=move |ev| sys.set(event_target_value(&ev))
            ></textarea>
            {move || {
                replaced
                    .get()
                    .map(|n| {
                        view! {
                            <div class="field-hint" data-prompt-replaced="">
                                {format!("Replaced by the profile '{n}' while it is picked")}
                            </div>
                        }
                    })
            }}
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
        {move || {
            prof
                .get()
                .and_then(|p| p.reasoning.map(|r| (p.name, r)))
                .filter(|_| think.with(|t| t.trim().is_empty()))
                .map(|(n, r)| {
                    view! {
                        <div class="field-hint" data-reasoning-source="profile">
                            {format!(
                                "Thinking left blank: the profile '{n}' sets it {} (source: profile)",
                                r.as_str()
                            )}
                        </div>
                    }
                })
        }}
        <McpPicker picked=picked/>
        <KbSection kb=kb/>
        {voice_section}
    }
}
