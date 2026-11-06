//! The open thread's settings drawer: the draft the page holds while the
//! drawer comes and goes, what a saved patch applies to the thread, and
//! the drawer itself.

use super::*;

/// The open thread's settings as they are being edited. Held by the page,
/// not by the panel that shows them: the panel unmounts whenever it folds —
/// a click back in the transcript, Esc, a narrower window — and a system
/// prompt typed into it went with it (review code:A2, par:PAR-1). Re-seeded
/// when another thread opens, as before.
#[derive(Clone, Copy)]
pub(crate) struct SettingsDraft {
    pub(crate) sys: RwSignal<String>,
    pub(crate) temp: RwSignal<String>,
    pub(crate) max_tok: RwSignal<String>,
    /// `""` (the model's default), `"on"` or `"off"`.
    pub(crate) think: RwSignal<String>,
    pub(crate) effort: RwSignal<String>,
    pub(crate) budget: RwSignal<String>,
    pub(crate) sampling: SamplingDraft,
    pub(crate) picked: RwSignal<Vec<ThreadMcp>>,
    pub(crate) kb: KbDraft,
    pub(crate) voice: VoiceDraft,
    /// The personality profile: `""` none, else its id.
    pub(crate) profile: RwSignal<String>,
    /// The image and audio tools' default models (chat-media-tools §3.8).
    pub(crate) media: crate::chat_media_defaults::MediaDraft,
}

impl SettingsDraft {
    pub(crate) fn new() -> Self {
        Self {
            sys: RwSignal::new(String::new()),
            temp: RwSignal::new(String::new()),
            max_tok: RwSignal::new(String::new()),
            think: RwSignal::new(String::new()),
            effort: RwSignal::new(String::new()),
            budget: RwSignal::new(String::new()),
            sampling: SamplingDraft::new(),
            picked: RwSignal::new(Vec::new()),
            kb: KbDraft::new(),
            voice: VoiceDraft::new(),
            profile: RwSignal::new(String::new()),
            media: crate::chat_media_defaults::MediaDraft::new(),
        }
    }

    pub(crate) fn seed(&self, t: &ChatThread) {
        let text = SettingsText::of(t);
        self.sys.set(text.sys);
        self.temp.set(text.temp);
        self.max_tok.set(text.max_tok);
        self.think.set(text.think);
        self.effort.set(text.effort);
        self.budget.set(text.budget);
        self.sampling.seed(text.sampling);
        self.profile.set(text.profile);
        self.picked.set(t.mcp_tools.clone());
        self.kb.seed(t);
        self.voice.load(&t.voice);
        self.media.load(&t.media_defaults);
    }

    /// The form as it stands (tracked).
    pub(crate) fn text(&self) -> SettingsText {
        SettingsText {
            sys: self.sys.get(),
            temp: self.temp.get(),
            max_tok: self.max_tok.get(),
            think: self.think.get(),
            effort: self.effort.get(),
            budget: self.budget.get(),
            sampling: self.sampling.text(),
            profile: self.profile.get(),
        }
    }

    /// Does the form differ from the thread as stored? (tracked)
    pub(crate) fn differs_from(&self, t: &ChatThread) -> bool {
        self.text().differs(&SettingsText::of(t))
            || self.picked.with(|p| *p != t.mcp_tools)
            || self.kb.differs_from(t)
            || self.voice.differs_from(&t.voice)
            || self.media.differs_from(&t.media_defaults)
    }
}

/// A settings patch the server took, applied to the thread as the page holds
/// it: only the keys the patch carries.
pub(crate) fn apply_settings(t: &mut ChatThread, body: &Value) {
    if let Some(v) = body.get("model_alias").and_then(Value::as_str) {
        t.model_alias = v.to_string();
    }
    if let Some(v) = body.get("system_prompt").and_then(Value::as_str) {
        t.system_prompt = v.to_string();
    }
    if let Some(v) = body.get("temperature") {
        t.temperature = v.as_f64();
    }
    if let Some(v) = body.get("max_tokens") {
        t.max_tokens = v.as_i64();
    }
    if let Some(v) = body.get("mcp_tools") {
        t.mcp_tools = serde_json::from_value(v.clone()).unwrap_or_default();
    }
    if let Some(v) = body.get("reasoning_enabled") {
        t.reasoning_enabled = v.as_bool();
    }
    if let Some(v) = body.get("reasoning_effort") {
        t.reasoning_effort = v.as_str().map(str::to_string);
    }
    if let Some(v) = body.get("reasoning_budget") {
        t.reasoning_budget = v.as_i64();
    }
    if let Some(v) = body.get("profile_id") {
        t.profile_id = v.as_i64();
    }
    if let Some(v) = body.get("media_defaults") {
        t.media_defaults = serde_json::from_value(v.clone()).unwrap_or_default();
    }
    chat_sampling::apply(t, body);
    apply_kb(t, body);
}

/// A thread's settings as the form's text: prompt, temperature, max tokens,
/// and the three reasoning overrides.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SettingsText {
    pub(crate) sys: String,
    pub(crate) temp: String,
    pub(crate) max_tok: String,
    pub(crate) think: String,
    pub(crate) effort: String,
    pub(crate) budget: String,
    pub(crate) sampling: SamplingText,
    /// The profile's id, `""` for none.
    pub(crate) profile: String,
}

impl SettingsText {
    pub(crate) fn of(t: &ChatThread) -> Self {
        Self {
            sys: t.system_prompt.clone(),
            temp: t.temperature.map(|v| v.to_string()).unwrap_or_default(),
            max_tok: t.max_tokens.map(|v| v.to_string()).unwrap_or_default(),
            think: match t.reasoning_enabled {
                Some(true) => "on".into(),
                Some(false) => "off".into(),
                None => String::new(),
            },
            effort: t.reasoning_effort.clone().unwrap_or_default(),
            budget: t
                .reasoning_budget
                .map(|v| v.to_string())
                .unwrap_or_default(),
            sampling: SamplingText::of(t),
            profile: t.profile_id.map(|p| p.to_string()).unwrap_or_default(),
        }
    }

    /// Compared as a save would send them: surrounding blanks are not an edit.
    /// Numbers by value — `0.70` saved is `0.7` stored, and `0512` is `512` —
    /// or the marker would stay on after a save that took them.
    pub(crate) fn differs(&self, b: &Self) -> bool {
        self.sys != b.sys
            || !same_number::<f64>(&self.temp, &b.temp)
            || !same_number::<i64>(&self.max_tok, &b.max_tok)
            || !same_number::<i64>(&self.budget, &b.budget)
            || self.think.trim() != b.think.trim()
            || self.effort.trim() != b.effort.trim()
            || self.sampling.differs(&b.sampling)
            || self.profile != b.profile
    }
}

/// Two boxes hold the same number, or — when either is not one — the same
/// text.
pub(crate) fn same_number<T: std::str::FromStr + PartialEq>(a: &str, b: &str) -> bool {
    match (a.trim().parse::<T>(), b.trim().parse::<T>()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a.trim() == b.trim(),
    }
}

/// The self-admin toolset's label: a thread that carries it drives the
/// gateway's own configuration, and no paired device reaches it.
const SELF_ADMIN_LABEL: &str = "lmgw";

#[component]
pub(crate) fn ThreadSettings(
    thread: RwSignal<Option<ChatThread>>,
    /// The page's: this panel comes and goes, the draft stays.
    draft: SettingsDraft,
    unsaved: Memo<bool>,
    on_save: impl Fn(Value) + Copy + Send + Sync + 'static,
    is_admin: bool,
    /// The ongoing folder this thread is the current thread of, by name.
    #[prop(into)]
    current_of: Signal<Option<String>>,
) -> impl IntoView {
    let toasts = use_toasts();
    // Attaching the self-admin toolset to an ongoing conversation's current
    // thread takes it out of every paired device's reach, and the
    // conversation moves on for every client (client-apps design L3, review
    // W5-3): said before Save.
    let rolls_over = move || {
        let attaching = draft
            .picked
            .with(|p| p.iter().any(|m| m.server_label.trim() == SELF_ADMIN_LABEL))
            && !thread.with(|t| {
                t.as_ref().is_some_and(|t| {
                    t.mcp_tools
                        .iter()
                        .any(|m| m.server_label.trim() == SELF_ADMIN_LABEL)
                })
            });
        current_of.get().filter(|_| attaching && !is_admin)
    };
    // Why the overrides as typed cannot be saved; shown under them and
    // blocking Save, as the server would refuse them too.
    let errors = DraftErrors::of(draft);
    let save = move |_| {
        // The page applies it to the thread, and says so, once the server
        // has taken it.
        match draft_patch(&draft) {
            Ok(body) => on_save(body),
            Err(e) => toasts.err(e),
        }
    };
    view! {
        <div class="chat-settings density-dense">
            <SettingsFields
                draft=draft
                errors=errors
                media_query=Signal::derive(move || {
                    thread
                        .with(|t| t.as_ref().map(|t| format!("?thread={}", t.id)))
                        .unwrap_or_default()
                })
                media_again=Signal::derive(move || {
                    thread.with(|t| t.as_ref().map(|t| t.updated_at.clone()).unwrap_or_default())
                })
                voice_resolved=Signal::derive(move || {
                    thread.with(|t| t.as_ref().and_then(|t| t.voice_resolved.clone()))
                })
                thread_id=Signal::derive(move || thread.with(|t| t.as_ref().map(|t| t.id)))
                model=Signal::derive(move || {
                    thread.with(|t| t.as_ref().map(|t| t.model_alias.clone()).unwrap_or_default())
                })
                prompt_label=if is_admin {
                    "System prompt — appended to the built-in admin prompt"
                } else {
                    "System prompt"
                }
            />
            {move || {
                rolls_over()
                    .map(|folder| {
                        view! {
                            <div class="notice warn">
                                {format!(
                                    "This is the current thread of the ongoing conversation \
                                     '{folder}'. With the self-admin tools attached, paired \
                                     devices can no longer reach it: the conversation \
                                     continues in a new thread for every client the next \
                                     time one of them asks for it."
                                )}
                            </div>
                        }
                    })
            }}
            <div class="chat-settings-save">
                <Show when=move || unsaved.get()>
                    <span class="count attn">"unsaved"</span>
                    <button
                        class="link-btn"
                        title="Put this thread's saved settings back"
                        on:click=move |_| {
                            if let Some(t) = thread.get_untracked() {
                                draft.seed(&t);
                            }
                        }
                    >
                        "Discard"
                    </button>
                </Show>
                <button
                    class="btn primary"
                    disabled=move || errors.any()
                    on:click=save
                >
                    "Save"
                </button>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the server took is what the page holds after: only the patch's
    /// keys, and a null clears a number back to "model default".
    #[test]
    fn a_saved_patch_is_applied_to_the_thread_key_by_key() {
        let mut t = ChatThread {
            model_alias: "a".into(),
            system_prompt: "old".into(),
            temperature: Some(0.2),
            max_tokens: Some(100),
            ..Default::default()
        };
        apply_settings(&mut t, &json!({ "model_alias": "b" }));
        assert_eq!(
            (
                t.model_alias.as_str(),
                t.system_prompt.as_str(),
                t.temperature
            ),
            ("b", "old", Some(0.2))
        );
        apply_settings(
            &mut t,
            &json!({ "system_prompt": "new", "temperature": null, "max_tokens": 5, "mcp_tools": [] }),
        );
        assert_eq!(
            (t.system_prompt.as_str(), t.temperature, t.max_tokens),
            ("new", None, Some(5))
        );
        apply_settings(
            &mut t,
            &json!({ "reasoning_enabled": false, "reasoning_effort": null, "reasoning_budget": 0 }),
        );
        assert_eq!(
            (
                t.reasoning_enabled,
                t.reasoning_effort.as_deref(),
                t.reasoning_budget
            ),
            (Some(false), None, Some(0))
        );
    }

    #[test]
    fn thread_settings_are_unsaved_only_when_a_save_would_change_them() {
        let t = ChatThread {
            system_prompt: "Be kind".into(),
            temperature: Some(0.7),
            reasoning_enabled: Some(true),
            reasoning_budget: Some(512),
            ..Default::default()
        };
        let stored = SettingsText::of(&t);
        assert_eq!(
            (
                stored.temp.as_str(),
                stored.think.as_str(),
                stored.budget.as_str()
            ),
            ("0.7", "on", "512")
        );
        assert!(!stored.differs(&stored));
        let padded = SettingsText {
            temp: " 0.7 ".into(),
            max_tok: " ".into(),
            budget: "512 ".into(),
            ..stored.clone()
        };
        assert!(!padded.differs(&stored));
        let typed = SettingsText {
            sys: "Be terse".into(),
            ..stored.clone()
        };
        assert!(typed.differs(&stored));
        let other_effort = SettingsText {
            effort: "high".into(),
            ..stored.clone()
        };
        assert!(other_effort.differs(&stored));
        // Saved as typed, stored as the number it is.
        let spelled = SettingsText {
            temp: "0.70".into(),
            budget: "0512".into(),
            ..stored.clone()
        };
        assert!(!spelled.differs(&stored));
    }

    #[test]
    fn the_profile_is_a_setting_that_saves_and_applies_like_the_rest() {
        let mut t = ChatThread {
            profile_id: Some(3),
            ..Default::default()
        };
        let stored = SettingsText::of(&t);
        assert_eq!(stored.profile, "3");
        let none = SettingsText {
            profile: String::new(),
            ..stored.clone()
        };
        assert!(none.differs(&stored));
        apply_settings(&mut t, &json!({ "profile_id": null }));
        assert_eq!(t.profile_id, None);
        apply_settings(&mut t, &json!({ "profile_id": 5 }));
        assert_eq!(t.profile_id, Some(5));
        // A patch without the key leaves it alone.
        apply_settings(&mut t, &json!({ "max_tokens": 5 }));
        assert_eq!(t.profile_id, Some(5));
    }
}
