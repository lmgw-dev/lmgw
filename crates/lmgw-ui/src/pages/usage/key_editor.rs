//! A key's policy as preference rows in its detail: enabled and expiry,
//! the alias and tool scopes, the paired devices' tools it may use, a
//! device's URLs and hosted label, the budget and the rate limits, the
//! note. One draft per key, the save toast and Ctrl+S; the same `key_set`
//! body the policy dialog sent.
//!
//! Which half of its own policy a row owns:
//!
//! An **agent** token's scope and enabled flag are *derived*: lmgw rewrites
//! them from the manifest and the agent row on every save, run or import
//! (container-runtime §3.1), so a value typed here would be taken back
//! silently on the next agent write. An **internal** identity never reaches
//! the auth middleware, which is where the rate limits and expiry are
//! checked, and it cannot authenticate at all — so those are rows nothing
//! would ever read. Both are greyed out here *and* refused by `key_set`.
//!
//! An **owner** key is not a client at all (principals §3.12): it is not
//! scoped, budgeted or rate-limited, and `key_set` refuses every one of
//! those fields. They are not shown. `owner:dashboard` gives up one more,
//! its `enabled` flag: it is the door, and switching it off from behind
//! would lock the owner out of the surface that could switch it back.
//!
//! A **device**'s policy is a client key's (client-apps design §1.1), plus
//! its URLs and hosted label. Its enabled flag is not here: Disable is in
//! its detail's last group, which says what it ends before it ends it.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{KeyRow, KeySaved, ToolInventory};
use serde_json::json;

use super::device_tools::{hosted_reach, DeviceToolsEditor, DeviceToolsState, HostedLabel};
use super::devices::parse_urls;
use super::key_line::expiry_past;
use super::scope_editor::{alias_join, alias_split, AliasScopeEditor, ScopeState, ToolScopeEditor};
use crate::widgets::nav_split::SaveKey;
use crate::widgets::{
    use_toasts, ComboRow, EntryRow, PrefGroup, SaveToast, SwitchRow, TextRow, Toasts,
};

/// The form's fields as they were loaded (or last saved): what a change is
/// measured against.
#[derive(Clone, Default, PartialEq)]
struct Stored {
    enabled: bool,
    scope_mode: String,
    /// The alias list as the picker writes it back.
    aliases: String,
    tool_mode: String,
    /// The tool list as stored; compared through the picker's groups.
    tools: String,
    device_tools: Vec<String>,
    budget: String,
    period: String,
    rpm: String,
    tpm: String,
    conc: String,
    expiry: String,
    note: String,
    hosts: String,
    urls: String,
}

fn whole_or_blank(v: i64) -> String {
    if v > 0 {
        v.to_string()
    } else {
        String::new()
    }
}

fn grant_norm(labels: &[String]) -> Vec<String> {
    let mut v: Vec<String> = labels.iter().map(|l| l.trim().to_lowercase()).collect();
    v.sort();
    v.dedup();
    v
}

impl Stored {
    fn of(k: &KeyRow) -> Self {
        let (models, adv) = alias_split(&k.scope_patterns);
        Self {
            enabled: k.enabled,
            scope_mode: if k.scope_mode.is_empty() {
                "all".into()
            } else {
                k.scope_mode.clone()
            },
            aliases: alias_join(&models, &adv),
            tool_mode: if k.tool_scope_mode.is_empty() {
                "all".into()
            } else {
                k.tool_scope_mode.clone()
            },
            tools: k.tool_scope_patterns.clone(),
            device_tools: grant_norm(&k.device_tools),
            budget: if k.budget_micro > 0 {
                format!("{:.2}", k.budget_micro as f64 / 1e6)
            } else {
                String::new()
            },
            period: if k.budget_period.is_empty() {
                "month".into()
            } else {
                k.budget_period.clone()
            },
            rpm: whole_or_blank(k.rpm_limit),
            tpm: whole_or_blank(k.tpm_limit),
            conc: whole_or_blank(k.concurrency_limit),
            expiry: k.expires_at.clone().unwrap_or_default(),
            note: k.note.clone(),
            hosts: k.hosts_label.clone().unwrap_or_default(),
            urls: k
                .pairing
                .as_ref()
                .map(|p| p.urls.join("\n"))
                .unwrap_or_default(),
        }
    }
}

/// A whole number, 0 or empty for none: why `raw` is not one.
pub(super) fn limit_err(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.parse::<i64>() {
        Ok(v) if v >= 0 => None,
        _ => Some("A whole number, 0 or empty for none".into()),
    }
}

/// A budget: an amount like 10 or 10.50, a comma read as the point; empty
/// is none. Reading text that will not parse as `0` turned "10,50x" into
/// *no budget at all*, saved with a green toast.
pub(super) fn budget_micro(raw: &str) -> Result<i64, String> {
    let raw = raw.trim().replace(',', ".");
    if raw.is_empty() {
        return Ok(0);
    }
    match raw.parse::<f64>() {
        Ok(v) if v >= 0.0 && v.is_finite() => Ok((v * 1e6).round() as i64),
        _ => Err("An amount like 10 or 10.50; empty means none".into()),
    }
}

fn limit_value(raw: &str) -> i64 {
    raw.trim().parse::<i64>().unwrap_or(0).max(0)
}

/// One key's draft.
#[derive(Clone, Copy)]
pub(super) struct PolicyForm {
    row: Memo<Option<KeyRow>>,
    pub scope: ScopeState,
    pub device_tools: DeviceToolsState,
    enabled: RwSignal<bool>,
    budget: RwSignal<String>,
    period: RwSignal<String>,
    rpm: RwSignal<String>,
    tpm: RwSignal<String>,
    conc: RwSignal<String>,
    expiry: RwSignal<String>,
    note: RwSignal<String>,
    hosts: RwSignal<String>,
    urls: RwSignal<String>,
    /// A past expiry ends the key's connections the moment it is saved
    /// (client-apps design §1.6): said on the first Save, done on the
    /// second (review W2-20). Any edit of the field asks again
    /// ([`Self::expiry_edited`]).
    past_said: RwSignal<bool>,
    stored: RwSignal<Option<Stored>>,
    saving: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    toasts: Toasts,
    /// Read the keys again after a save.
    reload: Callback<()>,
}

impl PolicyForm {
    pub fn new(
        row: Memo<Option<KeyRow>>,
        hosted: Signal<Vec<HostedLabel>>,
        reload: Callback<()>,
    ) -> Self {
        let f = Self::bare(row, use_toasts(), ScopeState::new(), reload);
        // The row as the list reads it, again as spend lands: loaded when it
        // first arrives, and again whenever its policy changed (a save, an
        // agent write, a Disable) while nothing is being edited. Edits in
        // progress are never replaced by a refetch.
        Effect::new(move |_| {
            let Some(k) = row.get() else { return };
            let next = Stored::of(&k);
            if f.takes(&next) {
                f.load(&k, next, &hosted.get_untracked());
            }
        });
        // Another moment typed after the warning is asked about again.
        Effect::new(move |prev: Option<()>| {
            f.expiry.track();
            if prev.is_some() {
                f.expiry_edited();
            }
        });
        f
    }

    /// The draft's signals, without the effects that follow the row and the
    /// expiry field.
    fn bare(
        row: Memo<Option<KeyRow>>,
        toasts: Toasts,
        scope: ScopeState,
        reload: Callback<()>,
    ) -> Self {
        Self {
            toasts,
            reload,
            row,
            scope,
            device_tools: DeviceToolsState::new(),
            enabled: RwSignal::new(true),
            budget: RwSignal::new(String::new()),
            period: RwSignal::new("month".into()),
            rpm: RwSignal::new(String::new()),
            tpm: RwSignal::new(String::new()),
            conc: RwSignal::new(String::new()),
            expiry: RwSignal::new(String::new()),
            note: RwSignal::new(String::new()),
            hosts: RwSignal::new(String::new()),
            urls: RwSignal::new(String::new()),
            past_said: RwSignal::new(false),
            stored: RwSignal::new(None),
            saving: RwSignal::new(false),
            error: RwSignal::new(None),
        }
    }

    /// Whether the row as read now (`next`) replaces the form: when nothing
    /// is loaded yet, or when it changed and nothing is being edited.
    fn takes(self, next: &Stored) -> bool {
        if self.stored.with_untracked(|s| s.as_ref() == Some(next)) {
            return false;
        }
        self.stored.with_untracked(Option::is_none) || untrack(|| self.dirty_count()) == 0
    }

    /// The expiry field changed: a warning about the moment it held is
    /// spent, and the next Save of a past moment asks again.
    fn expiry_edited(self) {
        if self.past_said.get_untracked() {
            self.past_said.set(false);
            self.error.set(None);
        }
    }

    fn load(self, k: &KeyRow, s: Stored, hosted: &[HostedLabel]) {
        self.scope.load(
            &k.scope_mode,
            &k.scope_patterns,
            &k.tool_scope_mode,
            &k.tool_scope_patterns,
        );
        self.device_tools.load(&k.device_tools, hosted);
        self.enabled.set(s.enabled);
        self.budget.set(s.budget.clone());
        self.period.set(s.period.clone());
        self.rpm.set(s.rpm.clone());
        self.tpm.set(s.tpm.clone());
        self.conc.set(s.conc.clone());
        self.expiry.set(s.expiry.clone());
        self.note.set(s.note.clone());
        self.hosts.set(s.hosts.clone());
        self.urls.set(s.urls.clone());
        self.past_said.set(false);
        self.error.set(None);
        self.stored.set(Some(s));
    }

    /// Put back what was loaded.
    fn discard(self, hosted: &[HostedLabel]) {
        if let Some(k) = self.row.get_untracked() {
            let s = Stored::of(&k);
            self.load(&k, s, hosted);
        }
    }

    fn kind(self) -> String {
        self.row
            .with(|r| r.as_ref().map(|r| r.kind.clone()).unwrap_or_default())
    }

    /// Each field's change against what is stored (tracked), in the order
    /// the rows show them.
    fn changes(self) -> Vec<bool> {
        let Some(s) = self.stored.get() else {
            return Vec::new();
        };
        let kind = self.kind();
        let owner = kind == "owner";
        let mut v = vec![
            self.enabled.get() != s.enabled,
            self.note.with(|n| n != &s.note),
        ];
        if owner {
            return v;
        }
        v.extend([
            self.scope.scope_mode.with(|m| m != &s.scope_mode),
            self.scope.alias_patterns() != s.aliases,
            self.scope.tool_mode.with(|m| m != &s.tool_mode),
            self.scope.tool_patterns() != self.scope.tool_patterns_of(&s.tools),
            self.budget.with(|b| b.trim() != s.budget),
            self.period.with(|p| p != &s.period),
            self.rpm.with(|x| x.trim() != s.rpm),
            self.tpm.with(|x| x.trim() != s.tpm),
            self.conc.with(|x| x.trim() != s.conc),
            self.expiry.with(|x| x.trim() != s.expiry.trim()),
        ]);
        if matches!(kind.as_str(), "key" | "device") {
            v.push(grant_norm(&self.device_tools.labels()) != s.device_tools);
        }
        if kind == "device" {
            v.push(self.hosts.with(|h| h.trim() != s.hosts.trim()));
            v.push(self.urls.with(|u| u.trim() != s.urls.trim()));
        }
        v
    }

    pub fn dirty_count(self) -> usize {
        self.changes().into_iter().filter(|c| *c).count()
    }

    fn field_dirty(self, f: fn(&Self, &Stored) -> bool) -> Signal<bool> {
        Signal::derive(move || {
            self.stored
                .with(|s| s.as_ref().is_some_and(|s| f(&self, s)))
        })
    }

    /// Fields that do not parse: Save waits for them.
    fn invalid(self) -> usize {
        let mut n = 0;
        if self.kind() == "owner" {
            return 0;
        }
        if self.budget.with(|b| budget_micro(b).is_err()) {
            n += 1;
        }
        for s in [self.rpm, self.tpm, self.conc] {
            if s.with(|v| limit_err(v).is_some()) {
                n += 1;
            }
        }
        n
    }

    /// Save, then measure against what was saved; the refetch that follows
    /// loads whatever the server made of it.
    pub fn save(self) {
        let toasts = self.toasts;
        let Some(k) = self.row.get_untracked() else {
            return;
        };
        if self.saving.get_untracked() || untrack(|| self.dirty_count()) == 0 {
            return;
        }
        if untrack(|| self.invalid()) > 0 {
            return;
        }
        self.error.set(None);
        // What this save sends, taken as the body is: what the form is
        // measured against once it is stored. Typing while the save is out
        // stays a change.
        let sent = self.snapshot();
        // An owner row owns two fields. The rest are not sent to be refused —
        // they are not sent.
        let body = if k.kind == "owner" {
            let mut body = json!({ "id": k.id, "note": self.note.get_untracked() });
            if k.name != "owner:dashboard" {
                body["enabled"] = json!(self.enabled.get_untracked());
            }
            body
        } else {
            let typed = self.expiry.get_untracked();
            let stored_expiry = self
                .stored
                .with_untracked(|s| s.as_ref().map(|s| s.expiry.clone()))
                .unwrap_or_default();
            if typed.trim() != stored_expiry.trim()
                && expiry_past(&typed)
                && !self.past_said.get_untracked()
            {
                self.past_said.set(true);
                self.error.set(Some(
                    "the expiry has passed: Save again to expire the key now".into(),
                ));
                return;
            }
            let Ok(budget) = budget_micro(&self.budget.get_untracked()) else {
                return;
            };
            let scope = self.scope;
            let mut body = json!({
                "id": k.id,
                "enabled": self.enabled.get_untracked(),
                "scope_mode": scope.scope_mode.get_untracked(),
                "scope_patterns": untrack(|| scope.alias_patterns()),
                "tool_scope_mode": scope.tool_mode.get_untracked(),
                "tool_scope_patterns": untrack(|| scope.tool_patterns()),
                "budget_micro": budget,
                "budget_period": self.period.get_untracked(),
                "rpm_limit": limit_value(&self.rpm.get_untracked()),
                "tpm_limit": limit_value(&self.tpm.get_untracked()),
                "concurrency_limit": limit_value(&self.conc.get_untracked()),
                "expires_at": typed.trim(),
                "note": self.note.get_untracked(),
            });
            // A client key's and a device's: an agent's tools are its
            // manifest's, and an internal identity presents no credential.
            if matches!(k.kind.as_str(), "key" | "device") {
                body["device_tools"] = json!(untrack(|| self.device_tools.labels()));
            }
            if k.kind == "device" {
                // Disable lives in its own row, behind its confirmation.
                if let Some(o) = body.as_object_mut() {
                    o.remove("enabled");
                }
                body["hosts_label"] = json!(self.hosts.get_untracked().trim());
                // The URLs the device dials, in order; a row with none yet
                // (not paired since TLS) keeps none until its New token
                // proposes.
                if !self.urls.get_untracked().trim().is_empty() {
                    match parse_urls(&self.urls.get_untracked()) {
                        Ok(list) => body["urls"] = json!(list),
                        Err(e) => {
                            self.error.set(Some(e));
                            return;
                        }
                    }
                }
            }
            body
        };
        self.saving.set(true);
        spawn_local(async move {
            let res = crate::api::post::<KeySaved, _>("/api/op/key_set", &body).await;
            // A key left while the save was out: its draft is gone with its
            // page, and the toast says what happened.
            if self.saving.try_set(false).is_some() {
                // A disposed signal hands the value back.
                match res {
                    Ok(v) => toasts.ok(v.message),
                    Err(e) => toasts.err(e.to_string()),
                }
                return;
            }
            match res {
                Ok(v) => {
                    toasts.ok(v.message);
                    self.mark_saved(sent);
                    self.reload.run(());
                }
                Err(e) => self.error.set(Some(e.to_string())),
            }
        });
    }

    /// The form as it stands, the way [`Stored`] holds it.
    fn snapshot(self) -> Stored {
        let scope = self.scope;
        Stored {
            enabled: self.enabled.get_untracked(),
            scope_mode: scope.scope_mode.get_untracked(),
            aliases: untrack(|| scope.alias_patterns()),
            tool_mode: scope.tool_mode.get_untracked(),
            tools: untrack(|| scope.tool_patterns()),
            device_tools: grant_norm(&untrack(|| self.device_tools.labels())),
            budget: self.budget.get_untracked().trim().to_string(),
            period: self.period.get_untracked(),
            rpm: self.rpm.get_untracked().trim().to_string(),
            tpm: self.tpm.get_untracked().trim().to_string(),
            conc: self.conc.get_untracked().trim().to_string(),
            expiry: self.expiry.get_untracked().trim().to_string(),
            note: self.note.get_untracked(),
            hosts: self.hosts.get_untracked().trim().to_string(),
            urls: self.urls.get_untracked().trim().to_string(),
        }
    }

    /// What a save sent (`sent`, its [`Self::snapshot`]) is now what is
    /// stored; whatever was typed since stays a change against it.
    fn mark_saved(self, sent: Stored) {
        self.past_said.set(false);
        self.stored.set(Some(sent));
    }
}

/// The policy's groups for one key, and its save toast.
#[component]
pub(super) fn PolicyGroups(
    form: PolicyForm,
    /// The labels the paired devices host now, for the device-tools grant.
    hosted: Signal<Vec<HostedLabel>>,
) -> impl IntoView {
    let row = form.row;
    let kind = Memo::new(move |_| form.kind());
    let id = Memo::new(move |_| row.with(|r| r.as_ref().map_or(0, |r| r.id)));
    let device = Memo::new(move |_| kind.get() == "device");
    let client = Memo::new(move |_| matches!(kind.get().as_str(), "key" | "device"));
    let derived = Memo::new(move |_| kind.get() == "agent");
    let inert = Memo::new(move |_| kind.get() == "internal");
    let owner = Memo::new(move |_| kind.get() == "owner");
    let door =
        Memo::new(move |_| row.with(|r| r.as_ref().is_some_and(|k| k.name == "owner:dashboard")));
    let scope = form.scope;

    // What the gateway serves on `/mcp`, for the tool scope and its preview
    // — read only for a client key, since listing connects every enabled
    // server that is not up yet (the MCP page does the same).
    let inventory = LocalResource::new(move || {
        let wanted = client.get();
        async move {
            if !wanted {
                return None;
            }
            Some(
                crate::api::get::<ToolInventory>("/api/tools")
                    .await
                    .map_err(|e| e.to_string()),
            )
        }
    });
    let inv_state: Signal<Option<Result<ToolInventory, String>>> =
        Signal::derive(move || inventory.get().flatten());
    // The preview runs the same matcher the gate does, from the shared crate,
    // so what it lists is what the key will be served.
    let preview = Memo::new(move |_| {
        let inv = inv_state.get()?.ok()?;
        let offered = inv
            .tools
            .iter()
            .filter(|t| t.plane == "/mcp" && t.available && !t.stale)
            .count();
        let (mode, pats) = (scope.tool_mode.get(), scope.tool_patterns());
        // A paired device's tools: its own whole, another's only through the
        // grant, then as the scope says.
        let (hosted, grant) = (hosted.get(), form.device_tools.labels());
        let own = device.get().then(|| id.get());
        let seen: Vec<String> = inv
            .tools
            .iter()
            .filter(|t| t.plane == "/mcp" && t.available && !t.stale)
            .filter(|t| {
                hosted_reach(&t.source_label, &hosted, own, &grant)
                    .unwrap_or_else(|| lmgw_api_types::scope::admits(&mode, &pats, &t.name))
            })
            .map(|t| t.name.clone())
            .collect();
        Some((seen, offered))
    });

    let unsaved = Memo::new(move |_| form.dirty_count());
    let invalid = Memo::new(move |_| form.invalid());
    SaveKey::install(Callback::new(move |()| form.save()));
    let dirty = move |f: fn(&PolicyForm, &Stored) -> bool| form.field_dirty(f);
    let budget_err = Signal::derive(move || form.budget.with(|b| budget_micro(b).err()));
    let rpm_err = Signal::derive(move || form.rpm.with(|v| limit_err(v)));
    let tpm_err = Signal::derive(move || form.tpm.with(|v| limit_err(v)));
    let conc_err = Signal::derive(move || form.conc.with(|v| limit_err(v)));
    let enabled_off = Signal::derive(move || derived.get() || inert.get() || door.get());
    let period_opts = Signal::derive(|| {
        vec![
            ("day".to_string(), "Per day".to_string()),
            ("month".into(), "Per month".into()),
            ("total".into(), "In total".into()),
        ]
    });
    let expiry_sub = Signal::derive(move || {
        if inert.get() {
            "An internal identity presents no credential, so an expiry binds nothing".to_string()
        } else {
            "A date (YYYY-MM-DD, the whole day) or an RFC 3339 time; an expired key is refused with 401".to_string()
        }
    });
    let limits_desc = move || {
        if inert.get() {
            "A budget is enforced for it, and is the lever that stops an unattended ingest. Rate limits bind a credential it never presents, so they are greyed out."
        } else {
            "A rate limit answers 429 with a real Retry-After; a budget answers 403, because a monthly budget will not clear inside a retry window. Empty or 0 is none."
        }
    };

    view! {
        <PrefGroup title="Access">
            <Show when=move || !device.get()>
                <SwitchRow
                    title="Enabled"
                    sub=Signal::derive(move || {
                        if door.get() {
                            "The dashboard key is the door: rotate it instead".to_string()
                        } else if derived.get() {
                            "Follows the agent: Disable on its page is the kill switch".to_string()
                        } else if inert.get() {
                            "An internal identity is lmgw's own work".to_string()
                        } else {
                            "A disabled key is refused, its policy and history kept".to_string()
                        }
                    })
                    value=form.enabled
                    disabled=enabled_off
                    dirty=dirty(|f, s| f.enabled.get() != s.enabled)
                />
            </Show>
            <Show when=move || !owner.get()>
                <EntryRow
                    title="Expires"
                    sub=expiry_sub
                    value=form.expiry
                    placeholder="never"
                    mono=true
                    disabled=Signal::derive(move || inert.get())
                    dirty=dirty(|f, s| f.expiry.with(|x| x.trim() != s.expiry.trim()))
                />
                <Show when=move || form.past_said.get()>
                    <div class="prow notice-row warn">
                        "That moment has passed: saving expires the key now — its Chat feed, voice "
                        "sessions and tool link close, and every request it makes is refused. Save "
                        "again to do it."
                    </div>
                </Show>
            </Show>
            <EntryRow
                title="Note"
                value=form.note
                placeholder="what it is for"
                dirty=dirty(|f, s| f.note.with(|n| n != &s.note))
            />
        </PrefGroup>
        <Show when=move || !owner.get()>
            <PrefGroup
                title="Scope"
                desc=Signal::derive(move || {
                    if derived.get() {
                        "Derived from the agent's manifest and rewritten on its next save, run or import: shown here, set on the agent's page.".to_string()
                    } else {
                        String::new()
                    }
                })
            >
                <AliasScopeEditor
                    state=scope
                    disabled=Signal::derive(move || derived.get())
                    dirty=Signal::derive(move || {
                        form.stored.with(|s| {
                            s.as_ref().is_some_and(|s| {
                                scope.scope_mode.with(|m| m != &s.scope_mode)
                                    || scope.alias_patterns() != s.aliases
                            })
                        })
                    })
                />
                <ToolScopeEditor
                    state=scope
                    inventory=inv_state
                    disabled=Signal::derive(move || derived.get() || inert.get())
                    dirty=Signal::derive(move || {
                        form.stored.with(|s| {
                            s.as_ref().is_some_and(|s| {
                                scope.tool_mode.with(|m| m != &s.tool_mode)
                                    || scope.tool_patterns() != scope.tool_patterns_of(&s.tools)
                            })
                        })
                    })
                />
                <Show when=move || client.get()>
                    <div class="prow notice-row scope-preview">
                        {move || match preview.get() {
                            None => "Which MCP tools this key sees on /mcp and may attach in a /v1/responses run.".to_string(),
                            Some((seen, of)) if seen.len() == of => format!("Sees all {of} tools on /mcp."),
                            Some((seen, of)) if seen.is_empty() => format!("Sees none of the {of} tools on /mcp."),
                            Some((seen, of)) => {
                                format!("Sees {} of {of} tools on /mcp: {}.", seen.len(), seen.join(", "))
                            }
                        }}
                        " The lmgw__* self-admin tools need an owner key and are never in reach of a client key. A caller with no key sees everything until Require API key is on."
                    </div>
                    <DeviceToolsEditor
                        state=form.device_tools
                        hosted=hosted
                        own=Signal::derive(move || device.get().then(|| id.get()))
                        disabled=Signal::derive(|| false)
                        scope=Signal::derive(move || (scope.tool_mode.get(), scope.tool_patterns()))
                        dirty=dirty(|f, s| grant_norm(&f.device_tools.labels()) != s.device_tools)
                    />
                </Show>
                <Show when=move || derived.get()>
                    {move || {
                        let agent = row
                            .with(|r| r.as_ref().map(|r| r.name.trim_start_matches("agent:").to_string()))
                            .unwrap_or_default();
                        view! {
                            <a class="prow action" href=format!("/agents/{agent}")>
                                <div class="prow-text">
                                    <span class="prow-title">"The agent's page"</span>
                                    <span class="prow-sub">"Its manifest's tools[] and its Enabled switch"</span>
                                </div>
                                <crate::widgets::Icon ico=crate::widgets::Ico::Right class="prow-chevron"/>
                            </a>
                        }
                    }}
                </Show>
            </PrefGroup>
            <Show when=move || device.get()>
                <PrefGroup title="Pairing">
                    <TextRow
                        mono=true
                        title="URLs"
                        sub="One https URL a line, in the order the device tries them. The device learns a change at its next connection; a name or address new to the list goes into the server certificates."
                        value=form.urls
                        placeholder="https://…"
                        dirty=dirty(|f, s| f.urls.with(|u| u.trim() != s.urls.trim()))
                    />
                    <EntryRow
                        title="Hosts tools as"
                        sub="The prefix its own MCP tools are named under (label__…); empty takes the grant away"
                        value=form.hosts
                        placeholder="no grant"
                        mono=true
                        dirty=dirty(|f, s| f.hosts.with(|h| h.trim() != s.hosts.trim()))
                    />
                </PrefGroup>
            </Show>
            <PrefGroup title="Budget and rate limits" desc=Signal::derive(move || limits_desc().to_string())>
                <EntryRow
                    title="Budget"
                    value=form.budget
                    placeholder="none"
                    mono=true
                    error=budget_err
                    dirty=dirty(|f, s| f.budget.with(|b| b.trim() != s.budget))
                />
                <ComboRow
                    title="Budget period"
                    value=form.period
                    options=period_opts
                    dirty=dirty(|f, s| f.period.with(|p| p != &s.period))
                />
                <EntryRow
                    title="Requests per minute"
                    sub="Counts model calls: each call of a Chat turn's tool loop, and a knowledge base's search, is one"
                    value=form.rpm
                    placeholder="none"
                    mono=true
                    error=rpm_err
                    disabled=Signal::derive(move || inert.get())
                    dirty=dirty(|f, s| f.rpm.with(|x| x.trim() != s.rpm))
                />
                <EntryRow
                    title="Tokens per minute"
                    value=form.tpm
                    placeholder="none"
                    mono=true
                    error=tpm_err
                    disabled=Signal::derive(move || inert.get())
                    dirty=dirty(|f, s| f.tpm.with(|x| x.trim() != s.tpm))
                />
                <EntryRow
                    title="Requests at once"
                    value=form.conc
                    placeholder="none"
                    mono=true
                    error=conc_err
                    disabled=Signal::derive(move || inert.get())
                    dirty=dirty(|f, s| f.conc.with(|x| x.trim() != s.conc))
                />
            </PrefGroup>
        </Show>
        <SaveToast
            dirty_count=Signal::derive(move || unsaved.get())
            invalid=Signal::derive(move || invalid.get())
            saving=form.saving
            error=form.error
            on_save=Callback::new(move |()| form.save())
            on_discard=Callback::new(move |()| form.discard(&hosted.get_untracked()))
        />
    }
}

/// The policy's change count for the page's dirty guard.
pub(super) fn unsaved(form: PolicyForm) -> Signal<bool> {
    Signal::derive(move || form.dirty_count() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_reads_a_comma_and_refuses_text() {
        assert_eq!(budget_micro(""), Ok(0));
        assert_eq!(budget_micro("10"), Ok(10_000_000));
        assert_eq!(budget_micro("10,50"), Ok(10_500_000));
        assert!(budget_micro("10,50x").is_err());
        assert!(budget_micro("-1").is_err());
    }

    #[test]
    fn a_limit_is_whole_or_blank() {
        assert_eq!(limit_err(""), None);
        assert_eq!(limit_err("60"), None);
        assert!(limit_err("1.5").is_some());
        assert!(limit_err("-2").is_some());
        assert_eq!(limit_value(" 60 "), 60);
        assert_eq!(limit_value(""), 0);
    }

    fn client_key(note: &str, expiry: Option<&str>) -> KeyRow {
        KeyRow {
            id: 4,
            kind: "key".into(),
            name: "ci".into(),
            enabled: true,
            note: note.into(),
            expires_at: expiry.map(Into::into),
            ..Default::default()
        }
    }

    fn form_of(row: RwSignal<Option<KeyRow>>) -> PolicyForm {
        let row = Memo::new(move |_| row.get());
        let f = PolicyForm::bare(
            row,
            Toasts::new(),
            ScopeState::bare(),
            Callback::new(|()| {}),
        );
        let k = row.get_untracked().unwrap();
        f.load(&k, Stored::of(&k), &[]);
        f
    }

    /// The baseline a save leaves is what it sent: typing while the POST
    /// was out stays a change, and the re-read that follows (the row as
    /// sent) does not put the stored value back over it.
    #[test]
    fn a_save_rebases_to_what_it_sent() {
        let owner = Owner::new();
        owner.with(|| {
            let row = RwSignal::new(Some(client_key("old", None)));
            let f = form_of(row);
            f.note.set("sent".into());
            let sent = f.snapshot();
            // Typed while the save is out.
            f.note.set("typed later".into());
            f.mark_saved(sent);
            assert_eq!(
                untrack(|| f.dirty_count()),
                1,
                "the later typing is a change"
            );
            assert_eq!(
                f.stored
                    .with_untracked(|s| s.as_ref().map(|s| s.note.clone())),
                Some("sent".into())
            );
            // The re-read after the save: the row as the server stored it.
            let after = client_key("sent", None);
            assert!(
                !f.takes(&Stored::of(&after)),
                "the edit in progress is kept"
            );
            assert_eq!(f.note.get_untracked(), "typed later");
            // Someone else's change while nothing is edited is taken.
            f.note.set("sent".into());
            assert!(f.takes(&Stored::of(&client_key("theirs", None))));
        });
    }

    /// The past-expiry warning holds for the moment it was given about:
    /// another moment typed after it asks again.
    #[test]
    fn another_past_moment_asks_again() {
        let owner = Owner::new();
        owner.with(|| {
            let row = RwSignal::new(Some(client_key("", None)));
            let f = form_of(row);
            f.past_said.set(true);
            f.error.set(Some("the expiry has passed".into()));
            f.expiry_edited();
            assert!(!f.past_said.get_untracked());
            assert_eq!(f.error.get_untracked(), None);
            // An error of another field is not the warning's to clear.
            f.error.set(Some("a URL".into()));
            f.expiry_edited();
            assert_eq!(f.error.get_untracked(), Some("a URL".into()));
        });
    }

    #[test]
    fn what_is_stored_is_read_the_way_the_form_shows_it() {
        let k = KeyRow {
            enabled: true,
            scope_patterns: "gemma4\n  \nclaude-*\ngemma4".into(),
            device_tools: vec!["Kai".into(), "kai".into()],
            budget_micro: 10_500_000,
            rpm_limit: 0,
            tpm_limit: 1000,
            ..Default::default()
        };
        let s = Stored::of(&k);
        assert_eq!(s.scope_mode, "all");
        assert_eq!(s.aliases, "gemma4\nclaude-*");
        assert_eq!(s.device_tools, vec!["kai".to_string()]);
        assert_eq!(s.budget, "10.50");
        assert_eq!(s.period, "month");
        assert_eq!(s.rpm, "");
        assert_eq!(s.tpm, "1000");
    }
}
