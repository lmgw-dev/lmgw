//! A client key's two scopes as pickers: which model aliases it may ask for
//! and which MCP tools it may see. Used by the key editor and, in a collapsed
//! section, by key creation.
//!
//! What is stored stays what it was — a mode and a newline-delimited list of
//! `*`-globs. The pickers are a way to write those lists:
//!
//! - **Aliases.** A line without a `*` is one model (a chip, added through
//!   the shared model picker); a line with one is a glob and lives in the
//!   *Advanced patterns* box. Nothing hand-written is lost.
//! - **Tools.** [`crate::widgets::tool_picker`] converts between the list and
//!   a source/tool selection; what it cannot represent stays in *Advanced
//!   patterns* too. Until the tool inventory has loaded (or if it cannot), the
//!   list is edited as plain text and saved exactly as it was loaded.

use leptos::prelude::*;
use lmgw_api_types::{McpServersResponse, ToolInventory};

use crate::widgets::tool_picker::{
    from_patterns, scope_groups, to_patterns, ScopeGroup, ScopeToolPicker, Selection, ThreadMcp,
};
use crate::widgets::{ModelPicker, Select};

/// The lines of an alias scope, split into models (no `*`) and globs.
pub(super) fn alias_split(patterns: &str) -> (Vec<String>, String) {
    let mut models: Vec<String> = Vec::new();
    let mut globs: Vec<&str> = Vec::new();
    for line in patterns.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line.contains('*') {
            globs.push(line);
        } else if !models.iter().any(|m| m == line) {
            models.push(line.to_string());
        }
    }
    (models, globs.join("\n"))
}

/// The inverse of [`alias_split`].
pub(super) fn alias_join(models: &[String], advanced: &str) -> String {
    models
        .iter()
        .map(String::as_str)
        .chain(advanced.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Everything the two editors hold, in signals the caller reads when it saves.
#[derive(Clone, Copy)]
pub(super) struct ScopeState {
    pub scope_mode: RwSignal<String>,
    models: RwSignal<Vec<String>>,
    alias_adv: RwSignal<String>,
    pub tool_mode: RwSignal<String>,
    picked: RwSignal<Vec<ThreadMcp>>,
    tool_adv: RwSignal<String>,
    /// The tool list as loaded; edited directly while there are no groups.
    tool_raw: RwSignal<String>,
    /// What the picker offers, once the inventory has loaded.
    pub groups: RwSignal<Option<Vec<ScopeGroup>>>,
    /// Why the servers' tool prefixes are unknown, if they are.
    servers_err: RwSignal<Option<String>>,
    /// `tool_raw` has been loaded but not yet split over groups. Only then may
    /// a (re)arrival of the groups re-split it: after that the picks are the
    /// truth, and every refetch of the inventory would throw them away (a
    /// create form has an empty `tool_raw`, so it dropped the ticks).
    split_pending: RwSignal<bool>,
    /// A key was loaded and the inventory has not arrived since: `groups` may
    /// still hold the *previous* open's groups (the state outlives the
    /// editor), which the load split over. The next arrival re-splits once,
    /// if the picks are still what that split produced.
    refresh_pending: RwSignal<bool>,
    /// What the last split produced, to tell whether the user has edited since.
    last_split: RwSignal<Option<(Vec<ThreadMcp>, String)>>,
}

impl ScopeState {
    pub fn new() -> Self {
        let s = Self::bare();
        // The inventory arrives after the row: split the loaded text again
        // once there is something to split it over.
        Effect::new(move |_| {
            s.groups.track();
            s.on_groups();
        });
        s
    }

    /// The signals alone, without the effect that follows `groups`.
    fn bare() -> Self {
        Self {
            scope_mode: RwSignal::new("all".into()),
            models: RwSignal::new(Vec::new()),
            alias_adv: RwSignal::new(String::new()),
            tool_mode: RwSignal::new("all".into()),
            picked: RwSignal::new(Vec::new()),
            tool_adv: RwSignal::new(String::new()),
            tool_raw: RwSignal::new(String::new()),
            groups: RwSignal::new(None),
            servers_err: RwSignal::new(None),
            split_pending: RwSignal::new(false),
            refresh_pending: RwSignal::new(false),
            last_split: RwSignal::new(None),
        }
    }

    /// Load one key's stored scope.
    pub fn load(
        &self,
        scope_mode: &str,
        scope_patterns: &str,
        tool_mode: &str,
        tool_patterns: &str,
    ) {
        self.scope_mode.set(
            if scope_mode.is_empty() {
                "all"
            } else {
                scope_mode
            }
            .to_string(),
        );
        let (models, adv) = alias_split(scope_patterns);
        self.models.set(models);
        self.alias_adv.set(adv);
        self.tool_mode.set(
            if tool_mode.is_empty() {
                "all"
            } else {
                tool_mode
            }
            .to_string(),
        );
        self.tool_raw.set(tool_patterns.to_string());
        self.split_pending.set(true);
        self.refresh_pending.set(true);
        self.resplit();
    }

    /// The groups were (re)set: split the loaded text if that is still owed.
    fn on_groups(&self) {
        if self.split_pending.get_untracked() {
            self.refresh_pending.set(false);
            self.resplit();
        } else if self.refresh_pending.get_untracked() {
            self.refresh_pending.set(false);
            let now = (self.picked.get_untracked(), self.tool_adv.get_untracked());
            if self.last_split.get_untracked().as_ref() == Some(&now) {
                self.resplit();
            }
        }
    }

    fn resplit(&self) {
        if let Some(groups) = self.groups.get_untracked() {
            let (picked, adv) = from_patterns(&self.tool_raw.get_untracked(), &groups);
            self.last_split.set(Some((picked.clone(), adv.clone())));
            self.picked.set(picked);
            self.tool_adv.set(adv);
            self.split_pending.set(false);
        }
    }

    /// The alias scope as `key_set` / `key_create` take it.
    pub fn alias_patterns(&self) -> String {
        alias_join(&self.models.get(), &self.alias_adv.get())
    }

    /// The tool scope as `key_set` / `key_create` take it.
    pub fn tool_patterns(&self) -> String {
        match self.groups.get() {
            Some(groups) => to_patterns(&self.picked.get(), &groups, &self.tool_adv.get()),
            None => self.tool_raw.get(),
        }
    }

    /// Both scopes at their defaults — nothing to send at creation.
    pub fn is_default(&self) -> bool {
        untrack(|| {
            self.scope_mode.get() == "all"
                && self.alias_patterns().is_empty()
                && self.tool_mode.get() == "all"
                && self.tool_patterns().is_empty()
        })
    }
}

fn mode_options() -> Signal<Vec<(String, String)>> {
    Signal::derive(|| {
        vec![
            ("all".to_string(), "all".to_string()),
            ("allow".into(), "allow only…".into()),
            ("deny".into(), "deny…".into()),
        ]
    })
}

/// The advanced-patterns box: collapsed, with the count in its summary.
#[component]
fn Advanced(
    text: RwSignal<String>,
    placeholder: &'static str,
    disabled: Signal<bool>,
) -> impl IntoView {
    let count = move || text.with(|t| t.lines().filter(|l| !l.trim().is_empty()).count());
    view! {
        <details class="lab-details">
            <summary>
                "Advanced patterns"
                {move || (count() > 0).then(|| format!(" ({})", count()))}
            </summary>
            <textarea
                class="input ta mono"
                placeholder=placeholder
                aria-label="Advanced patterns"
                disabled=move || disabled.get()
                prop:value=move || text.get()
                on:input=move |ev| text.set(event_target_value(&ev))
            ></textarea>
        </details>
    }
}

/// Which model aliases the key may ask for.
#[component]
pub(super) fn AliasScopeEditor(state: ScopeState, disabled: Signal<bool>) -> impl IntoView {
    let ScopeState {
        scope_mode,
        models,
        alias_adv,
        ..
    } = state;
    let add = RwSignal::new(String::new());
    Effect::new(move |_| {
        let v = add.get();
        if v.is_empty() {
            return;
        }
        models.update(|m| {
            if !m.contains(&v) {
                m.push(v.clone());
            }
        });
        add.set(String::new());
    });
    view! {
        <div class="row" style="margin-top:10px">
            <span class="lbl">"aliases"</span>
            <Select value=scope_mode disabled=disabled options=mode_options()/>
        </div>
        <Show when=move || scope_mode.get() != "all">
            <div class="scope-chips">
                <For each=move || models.get() key=|m| m.clone() let:m>
                    <span class="chip filter-chip mono">
                        {m.clone()}
                        <button
                            class="btn ghost"
                            aria-label=format!("Remove {m}")
                            disabled=move || disabled.get()
                            on:click={
                                let m = m.clone();
                                move |_| models.update(|l| l.retain(|x| x != &m))
                            }
                        >
                            "×"
                        </button>
                    </span>
                </For>
                <Show when=move || models.with(|m| m.is_empty())>
                    <span class="dim mini-note">
                        {move || {
                            if scope_mode.get() == "allow" {
                                "No model picked: the key may ask for none."
                            } else {
                                "No model picked: nothing is denied."
                            }
                        }}
                    </span>
                </Show>
            </div>
            <ModelPicker value=add empty_label="Add a model…" allow_custom=true disabled=disabled/>
            <Advanced text=alias_adv placeholder="one glob per line — claude-*, qwen*" disabled=disabled/>
        </Show>
    }
}

/// Which MCP tools the key may see. `inventory` is `/api/tools`, loaded by the
/// caller (its preview reads the same one): `None` while it loads.
#[component]
pub(super) fn ToolScopeEditor(
    state: ScopeState,
    inventory: Signal<Option<Result<ToolInventory, String>>>,
    disabled: Signal<bool>,
) -> impl IntoView {
    let ScopeState {
        tool_mode,
        picked,
        tool_adv,
        tool_raw,
        groups,
        servers_err,
        ..
    } = state;
    // The servers' registered prefixes, which the inventory does not carry.
    let servers = LocalResource::new(move || {
        let ready = matches!(inventory.get(), Some(Ok(_)));
        async move {
            if !ready {
                return None;
            }
            Some(crate::api::get::<McpServersResponse>("/api/mcp-servers").await)
        }
    });
    Effect::new(move |_| {
        let (Some(Ok(inv)), Some(res)) = (inventory.get(), servers.get().flatten()) else {
            return;
        };
        match res {
            Ok(s) => {
                servers_err.set(None);
                groups.set(Some(scope_groups(&inv, &s.mcp_servers)));
            }
            Err(e) => {
                // Without prefixes no server has a glob, so every tick
                // enumerates: safe, and said out loud.
                servers_err.set(Some(e.to_string()));
                groups.set(Some(scope_groups(&inv, &[])));
            }
        }
    });
    let selection = Selection {
        list: picked.into(),
        set: Callback::new(move |v| picked.set(v)),
    };
    let group_sig: Signal<Vec<ScopeGroup>> =
        Signal::derive(move || groups.get().unwrap_or_default());
    view! {
        <div class="row" style="margin-top:10px">
            <span class="lbl">"tools"</span>
            <Select value=tool_mode disabled=disabled options=mode_options()/>
        </div>
        <Show when=move || tool_mode.get() != "all">
            {move || match groups.get() {
                Some(_) => {
                    view! {
                        <div class="mini-note dim">
                            {move || {
                                if tool_mode.get() == "deny" {
                                    "Ticked tools are hidden from the key."
                                } else {
                                    "Only ticked tools are visible to the key."
                                }
                            }}
                        </div>
                        {move || {
                            servers_err
                                .get()
                                .map(|e| {
                                    view! {
                                        <div class="mini-note dim">
                                            "Server tool prefixes unavailable (" {e}
                                            ") — ticking a whole server lists its tools."
                                        </div>
                                    }
                                })
                        }}
                        <ScopeToolPicker groups=group_sig selection=selection disabled=disabled/>
                        <Advanced
                            text=tool_adv
                            placeholder="one glob per line — github__*, docs__query"
                            disabled=disabled
                        />
                    }
                        .into_any()
                }
                None if matches!(inventory.get(), Some(Err(_))) => {
                    view! {
                        <div class="mini-note dim">
                            "Tool list unavailable ("
                            {move || match inventory.get() {
                                Some(Err(e)) => e,
                                _ => String::new(),
                            }}
                            ") — edit the patterns as text."
                        </div>
                        <textarea
                            class="input ta mono"
                            placeholder="one glob per line — github__*, docs__query"
                            disabled=move || disabled.get()
                            prop:value=move || tool_raw.get()
                            on:input=move |ev| tool_raw.set(event_target_value(&ev))
                        ></textarea>
                    }
                        .into_any()
                }
                None => view! { <div class="mini-note dim">"Loading tools…"</div> }.into_any(),
            }}
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_and_globs_split_by_the_star() {
        let (m, adv) = alias_split("kilo/x\n claude-*\n\nkilo/x\nqwen3");
        assert_eq!(m, vec!["kilo/x", "qwen3"]);
        assert_eq!(adv, "claude-*");
        assert_eq!(alias_join(&m, &adv), "kilo/x\nqwen3\nclaude-*");
    }

    fn docs_group() -> ScopeGroup {
        ScopeGroup {
            label: "docs".into(),
            prefix: Some("docs".into()),
            tools: vec![crate::widgets::tool_picker::ToolItem {
                name: "docs__query".into(),
                description: None,
            }],
            reason: None,
        }
    }

    #[test]
    fn ticks_survive_the_inventory_arriving_again() {
        let owner = Owner::new();
        owner.set();
        let s = ScopeState::bare();
        // Create form: nothing stored, groups not there yet.
        s.load("all", "", "deny", "");
        s.groups.set(Some(vec![docs_group()]));
        s.on_groups();
        // The user ticks docs.
        s.picked.set(vec![ThreadMcp {
            server_label: "docs".into(),
            allowed_tools: None,
            require_approval: None,
        }]);
        assert_eq!(s.tool_patterns(), "docs__*");
        // Collapse/expand refetches the inventory and sets the groups again.
        s.groups.set(Some(vec![docs_group()]));
        s.on_groups();
        assert_eq!(s.tool_patterns(), "docs__*");
        // Loading another key's scope still splits that text.
        s.load("all", "", "deny", "docs__query");
        assert_eq!(s.picked.get_untracked().len(), 1);
    }

    #[test]
    fn a_scope_loaded_before_the_groups_is_split_when_they_arrive() {
        let owner = Owner::new();
        owner.set();
        let s = ScopeState::bare();
        s.load("all", "", "allow", "docs__*");
        assert!(s.picked.get_untracked().is_empty());
        s.groups.set(Some(vec![docs_group()]));
        s.on_groups();
        assert_eq!(s.picked.get_untracked().len(), 1);
        assert_eq!(s.tool_patterns(), "docs__*");
    }

    #[test]
    fn reopening_a_key_re_splits_over_the_fresh_inventory_unless_edited() {
        let owner = Owner::new();
        owner.set();
        let s = ScopeState::bare();
        // A first open, with the tools known.
        s.groups.set(Some(vec![]));
        s.load("all", "", "allow", "docs__*");
        s.on_groups();
        assert!(s.picked.get_untracked().is_empty(), "no docs group yet");
        // Reopen: the stale groups split the text (nothing), then the fresh
        // inventory arrives and the same text now matches the docs group.
        s.load("all", "", "allow", "docs__*");
        assert!(s.picked.get_untracked().is_empty());
        s.groups.set(Some(vec![docs_group()]));
        s.on_groups();
        assert_eq!(s.picked.get_untracked().len(), 1);
        assert_eq!(s.tool_patterns(), "docs__*");
        // Reopen again, but the user edits before the inventory lands.
        s.groups.set(Some(vec![]));
        s.load("all", "", "allow", "docs__*");
        s.picked.set(vec![ThreadMcp {
            server_label: "other".into(),
            allowed_tools: None,
            require_approval: None,
        }]);
        s.groups.set(Some(vec![docs_group()]));
        s.on_groups();
        assert_eq!(s.picked.get_untracked()[0].server_label, "other");
    }

    #[test]
    fn an_empty_scope_joins_to_nothing() {
        assert_eq!(alias_split(""), (vec![], String::new()));
        assert_eq!(alias_join(&[], "  \n"), "");
    }
}
