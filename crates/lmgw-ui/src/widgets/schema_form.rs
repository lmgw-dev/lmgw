//! The agent config form (agent-catalog design §2.6): the JSON-schema subset
//! an agent's manifest may declare, rendered as controls and collected back
//! into a patch for `agent_config_set`.
//!
//! The server hands the schema over already flattened — [`AgentField`] per
//! property, in the author's document order — so nothing here parses JSON
//! Schema. It draws eight shapes and converts them back:
//!
//! | field | control | back out as |
//! |---|---|---|
//! | `string` | text input | string |
//! | `string` + `enum` | [`Select`] | string |
//! | `string` + `format: model_alias` | [`ModelPicker`] over the catalog | string, or null |
//! | `string` + `format: multiline` | textarea | string |
//! | `string` + `format: secret` | password input | string, or **omitted** |
//! | `string` + `format: directory`/`file` | path box + picker | string |
//! | `integer` / `number` | number input, `min`/`max` shown | number or null |
//! | `boolean` | checkbox | bool |
//! | `array` (of string) | comma-separated box that wraps and grows | array of strings |
//!
//! Three rules it does not bend:
//!
//! - **A secret is never pre-filled and an empty one is never sent.** The
//!   control shows only whether something is stored; leaving it blank keeps it
//!   (§2.6, the house convention for tokens). There is deliberately no "clear"
//!   affordance: `agent_config_set` reads an empty submission as "keep", so a
//!   clear button here would be a lie.
//! - **A mount field names a slot, never a host path** (mounts §5.1). The
//!   manifest declares the format and the access; the owner names the folder,
//!   here, and the relabel that binding performs is printed on the field
//!   itself rather than only in a log.
//! - **Bounds are shown, never enforced here.** `minimum`/`maximum` are
//!   rendered next to the label and validated server-side by
//!   `manifest::validate_values`, which names the field. One validator, one
//!   message. The only thing refused client-side is text that is not a number
//!   at all, because there is no value to send for it.

use leptos::prelude::*;
use leptos::wasm_bindgen::closure::Closure;
use leptos::wasm_bindgen::{JsCast, JsValue};
use lmgw_api_types::AgentField;
use serde_json::{Map, Value};

use super::{Explain, ModelPicker, Select};
use crate::catalog::use_model_catalog;

// ---------------------------------------------------------------------------
// The draft — one signal, not one per field
// ---------------------------------------------------------------------------

/// The form's live state: every control's raw text (a bool for a checkbox),
/// keyed by field name.
///
/// One signal rather than one per field, because [`load`](Draft::load) is
/// called from the page's `spawn_local` and signals created there would belong
/// to no owner. `generation` is bumped on every load and is part of each row's
/// key, so re-reading an agent rebuilds the controls instead of leaving a stale
/// `Select` behind.
#[derive(Clone, Copy)]
pub struct Draft {
    values: RwSignal<Map<String, Value>>,
    /// What the controls held when the config was last loaded or saved: what
    /// "unsaved" is measured against (ux:U-3).
    baseline: RwSignal<Map<String, Value>>,
    /// Fields the owner asked to **forget**, posted as `agent_config_set`'s
    /// `clear`. An empty control means "keep the stored secret", so wanting it
    /// gone needs its own gesture; it is a separate set rather than a sentinel
    /// value so nothing can mistake it for something to store.
    cleared: RwSignal<Vec<String>>,
    generation: RwSignal<u32>,
}

impl Default for Draft {
    fn default() -> Self {
        Self::new()
    }
}

impl Draft {
    pub fn new() -> Self {
        Self {
            values: RwSignal::new(Map::new()),
            baseline: RwSignal::new(Map::new()),
            cleared: RwSignal::new(Vec::new()),
            generation: RwSignal::new(0),
        }
    }

    /// Fill the controls from a fresh `GET /api/agents/{id}`.
    pub fn load(&self, fields: &[AgentField], config: &Value) {
        let fresh = initial_draft(fields, config);
        self.baseline.set(fresh.clone());
        self.values.set(fresh);
        self.cleared.set(Vec::new());
        self.generation.update(|g| *g += 1);
    }

    /// A re-read of the same agent: the controls follow it only while they
    /// hold nothing unsaved. With edits in them they stay, and only what they
    /// are compared against moves — a save elsewhere on the page (the
    /// manifest, the service) must not throw the form away.
    pub fn reload(&self, fields: &[AgentField], config: &Value) {
        if self.changed_untracked().is_empty() {
            self.load(fields, config);
        } else {
            self.baseline.set(initial_draft(fields, config));
        }
    }

    /// The controls as they stand, to hand to [`Self::mark_saved`] once a
    /// save of them has landed.
    pub fn snapshot(&self) -> (Map<String, Value>, Vec<String>) {
        (self.values.get_untracked(), self.cleared.get_untracked())
    }

    /// `sent` (a [`Self::snapshot`] taken at Save) is stored now: it is the
    /// new baseline. What was typed while the save was out stays unsaved.
    pub fn mark_saved(&self, (values, cleared): (Map<String, Value>, Vec<String>)) {
        self.baseline.set(values);
        self.cleared.update(|c| c.retain(|n| !cleared.contains(n)));
    }

    /// Back to what was loaded.
    pub fn discard(&self) {
        self.values.set(self.baseline.get_untracked());
        self.cleared.set(Vec::new());
        self.generation.update(|g| *g += 1);
    }

    /// The fields that differ from what was loaded, secrets ticked for
    /// clearing included (tracked).
    pub fn changed(&self) -> Vec<String> {
        self.baseline.with(|b| {
            self.values
                .with(|v| self.cleared.with(|c| changed_fields(b, v, c)))
        })
    }

    fn changed_untracked(&self) -> Vec<String> {
        self.baseline.with_untracked(|b| {
            self.values
                .with_untracked(|v| self.cleared.with_untracked(|c| changed_fields(b, v, c)))
        })
    }

    /// Is this field marked to be forgotten on the next save?
    pub fn is_cleared(&self, name: &str) -> bool {
        self.cleared.with(|c| c.iter().any(|n| n == name))
    }

    pub fn set_cleared(&self, name: &str, on: bool) {
        let key = name.to_string();
        self.cleared.update(|c| {
            c.retain(|n| n != &key);
            if on {
                c.push(key);
            }
        });
    }

    /// The `clear` list to POST alongside the patch, narrowed to fields this
    /// form actually drew — a schema edit between load and save must not send
    /// a name the server would refuse.
    pub fn clear_list(&self, fields: &[AgentField]) -> Vec<String> {
        self.cleared.with_untracked(|c| {
            c.iter()
                .filter(|n| fields.iter().any(|f| &&f.name == n))
                .cloned()
                .collect()
        })
    }

    /// Does the form hold something for this field **right now**?
    ///
    /// Tracked, and the live counterpart to asking the same of the stored
    /// config: a model picked a second ago is in here and not yet in there.
    /// A secret is never drawn with its value, so a stored one counts unless
    /// the owner has ticked it for clearing; a checkbox always has an answer.
    pub fn has_value(&self, f: &AgentField) -> bool {
        has_value(f, self.is_cleared(&f.name), &self.text(&f.name))
    }

    fn generation(&self) -> u32 {
        self.generation.get()
    }

    /// A control's text, tracked — so a reload shows through immediately.
    pub fn text(&self, name: &str) -> String {
        self.values.with(|v| {
            v.get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
    }

    fn text_now(&self, name: &str) -> String {
        self.values.with_untracked(|v| {
            v.get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
    }

    pub fn set_text(&self, name: &str, text: String) {
        let key = name.to_string();
        self.values.update(|v| {
            v.insert(key, Value::String(text));
        });
    }

    pub fn flag(&self, name: &str) -> bool {
        self.values
            .with(|v| v.get(name).and_then(Value::as_bool).unwrap_or(false))
    }

    pub fn set_flag(&self, name: &str, on: bool) {
        let key = name.to_string();
        self.values.update(|v| {
            v.insert(key, Value::Bool(on));
        });
    }

    /// What to POST to `agent_config_set`, or the first problems to show.
    pub fn patch(&self, fields: &[AgentField]) -> Result<Map<String, Value>, String> {
        self.values.with_untracked(|v| build_patch(fields, v))
    }
}

// ---------------------------------------------------------------------------
// The two pure halves, so the conversion rules are testable without a DOM
// ---------------------------------------------------------------------------

/// The stored config (secrets already masked as `{ "has_value": … }`) as
/// control text. A field the config does not mention falls back to the
/// schema's `default`, which is what the run would use anyway — showing an
/// empty box for a field that has an effective value would be a lie.
pub fn initial_draft(fields: &[AgentField], config: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    for f in fields {
        let stored = config.get(&f.name);
        if f.ty == "boolean" {
            let on = stored
                .and_then(Value::as_bool)
                .or_else(|| f.default.as_ref().and_then(Value::as_bool))
                .unwrap_or(false);
            out.insert(f.name.clone(), Value::Bool(on));
            continue;
        }
        // A secret is never pre-filled: the API returns `{ has_value }` and
        // never the value, and an empty box is exactly what "keep it" looks
        // like when submitted.
        let text = if f.format == "secret" {
            String::new()
        } else {
            let v = stored
                .filter(|v| !v.is_null())
                .or(f.default.as_ref())
                .cloned()
                .unwrap_or(Value::Null);
            value_to_text(&v)
        };
        out.insert(f.name.clone(), Value::String(text));
    }
    out
}

/// The fields whose control differs from the baseline — text compared
/// trimmed, as a save would send it — plus the secrets ticked for clearing.
/// A secret typed into is a change; its baseline is always empty.
pub fn changed_fields(
    baseline: &Map<String, Value>,
    values: &Map<String, Value>,
    cleared: &[String],
) -> Vec<String> {
    let norm = |v: Option<&Value>| match v {
        Some(Value::String(s)) => Value::String(s.trim().to_string()),
        Some(Value::Null) | None => Value::String(String::new()),
        Some(other) => other.clone(),
    };
    let mut out: Vec<String> = values
        .keys()
        .chain(baseline.keys())
        .filter(|k| norm(values.get(*k)) != norm(baseline.get(*k)))
        .cloned()
        .collect();
    out.extend(cleared.iter().cloned());
    out.sort();
    out.dedup();
    out
}

/// One stored value as the control shows it. Arrays are comma-joined (§2.6);
/// anything else prints as itself, never as JSON quoting.
fn value_to_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(value_to_text)
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

/// Controls back to config values.
///
/// Sparse on purpose: a secret left blank is **omitted**, so the server keeps
/// the stored one. Everything else is sent, including an empty string and an
/// empty array — those are edits, not absences.
pub fn build_patch(
    fields: &[AgentField],
    draft: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    let mut out = Map::new();
    let mut errors: Vec<String> = Vec::new();
    for f in fields {
        let raw = draft.get(&f.name);
        match f.ty.as_str() {
            "boolean" => {
                out.insert(
                    f.name.clone(),
                    Value::Bool(raw.and_then(Value::as_bool).unwrap_or(false)),
                );
            }
            "integer" | "number" => {
                let text = raw.and_then(Value::as_str).unwrap_or("").trim().to_string();
                if text.is_empty() {
                    // Cleared. `null` is how the API spells "unset"; a required
                    // field then fails server-side, naming itself.
                    out.insert(f.name.clone(), Value::Null);
                    continue;
                }
                match parse_number(&f.ty, &text) {
                    Some(v) => {
                        out.insert(f.name.clone(), v);
                    }
                    None => errors.push(format!(
                        "'{}': '{text}' is not {}",
                        label_of(f),
                        if f.ty == "integer" {
                            "a whole number"
                        } else {
                            "a number"
                        }
                    )),
                }
            }
            "array" => {
                let text = raw.and_then(Value::as_str).unwrap_or("");
                // A comma or a line break ends an item: the box wraps, and
                // Enter in it is a natural way to start the next one.
                let items: Vec<Value> = text
                    .split([',', '\n'])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| Value::String(s.to_string()))
                    .collect();
                out.insert(f.name.clone(), Value::Array(items));
            }
            // string, and anything a newer server grows that this build draws
            // as a text box.
            _ => {
                let text = raw.and_then(Value::as_str).unwrap_or("").to_string();
                if f.format == "secret" && text.is_empty() {
                    continue;
                }
                // The picker's "manifest default" choice. `null` is how the
                // API spells "unset", and an unset field runs on the schema's
                // default — an empty string would be a value of its own, an
                // alias nothing serves.
                if f.format == "model_alias" && text.trim().is_empty() {
                    out.insert(f.name.clone(), Value::Null);
                    continue;
                }
                out.insert(f.name.clone(), Value::String(text));
            }
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors.join("; "))
    }
}

/// [`Draft::has_value`] without the signals, so the one subtle case has a test:
/// a secret's box is blank even when a secret is stored, so the box cannot be
/// what answers for it — `has_value` on the field is, right up until the owner
/// ticks clear.
fn has_value(f: &AgentField, cleared: bool, text: &str) -> bool {
    if cleared {
        return false;
    }
    if f.ty == "boolean" {
        return true;
    }
    if !text.trim().is_empty() {
        return true;
    }
    f.format == "secret" && f.has_value
}

/// What binding a folder does to it, verbatim from mounts §5.5 — on the field
/// where the owner decides, and repeated under the Definition tab's *Mounts*
/// heading, because it is not undone when the agent stops.
///
/// The second sentence is what `rw` plus `--userns=keep-id` actually grants
/// (§5.5): the container process is the owner's own uid inside, so "writable"
/// is not "may add files" — it is every power the owner has over that tree.
/// Nothing else on the page says it, and the moment to read it is before the
/// folder is picked.
pub const RELABEL_NOTE: &str = "binding relabels this folder and everything under it for \
                                containers (`container_file_t`); nothing undoes it when the \
                                agent stops. With rw the container runs as your user and can \
                                delete or change anything under it.";

/// [`RELABEL_NOTE`] folded to one line: both halves of it — the relabel that
/// stays, and what rw hands the container — because the fold must not be the
/// thing that hides the second one.
pub const RELABEL_SUMMARY: &str =
    "Binding relabels the folder for containers for good; with rw the agent can change anything in it.";

/// Is this field a host mount — one of the two formats whose value is a path
/// on the machine lmgw runs on (mounts §5.1)?
pub fn is_mount(f: &AgentField) -> bool {
    f.ty == "string" && (f.format == "directory" || f.format == "file")
}

/// The access mode a mount runs under: what the **manifest** declared, and `ro`
/// when it declared nothing — the loader's own default, printed rather than
/// left blank. Never something this form can change: whether an agent writes is
/// a property of the agent (§5.1).
pub fn access_of(f: &AgentField) -> &str {
    let declared = f.access.trim();
    if declared.is_empty() {
        "ro"
    } else {
        declared
    }
}

/// The chip beside a mount control, and in the Definition tab's list:
/// `directory · rw`, `file · ro` (§5.8).
pub fn mount_chip(f: &AgentField) -> String {
    format!("{} · {}", f.format, access_of(f))
}

fn parse_number(ty: &str, text: &str) -> Option<Value> {
    if ty == "integer" {
        return text.parse::<i64>().ok().map(Value::from);
    }
    text.parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
}

/// What the form calls a field: its `title` when the author gave one, its key
/// otherwise.
pub fn label_of(f: &AgentField) -> String {
    if f.title.trim().is_empty() {
        f.name.clone()
    } else {
        f.title.clone()
    }
}

/// The small grey note next to a label: the type, the format, and the bounds
/// the server will enforce — visible, never silently applied.
fn hint_of(f: &AgentField) -> String {
    let mut parts: Vec<String> = Vec::new();
    if is_mount(f) {
        // The chip beside the control already reads `directory · rw`; saying it
        // again three words to the left is the same fact twice on one row.
    } else if !f.format.is_empty() {
        parts.push(f.format.clone());
    } else if f.ty == "array" {
        parts.push("comma-separated".to_string());
    } else if f.ty != "string" {
        parts.push(f.ty.clone());
    }
    match (f.minimum, f.maximum) {
        (Some(lo), Some(hi)) => parts.push(format!("{lo}–{hi}")),
        (Some(lo), None) => parts.push(format!("min {lo}")),
        (None, Some(hi)) => parts.push(format!("max {hi}")),
        (None, None) => {}
    }
    if f.required {
        parts.push("required".to_string());
    }
    parts.join(" · ")
}

/// A field whose control takes the control *and* help columns, with its help
/// on the line below: the control wants the width (a comma-separated list, a
/// multiline prompt, a path with two buttons and a chip). A description alone
/// does not make a field wide — it has a column of its own.
fn is_wide(f: &AgentField) -> bool {
    f.ty == "array" || f.format == "multiline" || is_mount(f)
}

/// The task a picker opens scoped to, from the model the field holds: an
/// embedding field offers embedders, a chat field chat models. Everything else
/// stays one "+N other tasks" chip away; an unknown task scopes nothing.
pub fn task_scope(task: Option<&str>) -> &'static [&'static str] {
    match task {
        Some("chat") => &["chat"],
        Some("embedding") => &["embedding"],
        Some("rerank") => &["rerank"],
        Some("tts") => &["tts"],
        Some("asr") => &["asr"],
        Some("image_generation") => &["image_generation"],
        Some("image_edit") => &["image_edit"],
        _ => &[],
    }
}

/// What the picker's empty choice says: the schema default it falls back to,
/// or that there is none.
pub fn default_label(f: &AgentField) -> String {
    match f.default.as_ref().map(value_to_text) {
        Some(d) if !d.trim().is_empty() => format!("manifest default · {d}"),
        _ => "not set · the manifest has no default".to_string(),
    }
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

/// Render one agent's config form as rows of label | control | help (the
/// help drops under its control when the form is narrow). Every `model_alias`
/// field picks from the shared model catalog.
#[component]
pub fn SchemaForm(#[prop(into)] fields: Signal<Vec<AgentField>>, draft: Draft) -> impl IntoView {
    let rows = move || {
        let generation = draft.generation();
        fields
            .get()
            .into_iter()
            .map(move |f| (generation, f))
            .collect::<Vec<_>>()
    };
    view! {
        <Show when=move || !fields.get().is_empty() fallback=|| view! {
            <div class="dim">"This agent has no config form — nothing to fill in."</div>
        }>
            <div class="form-rows">
                <div class="fr-grid">
                    <For
                        each=rows
                        key=|(generation, f)| format!("{generation}:{}", f.name)
                        let:row
                    >
                        {
                            let (_, f) = row;
                            view! { <FieldControl field=f draft=draft/> }
                        }
                    </For>
                </div>
            </div>
        </Show>
    }
}

#[component]
fn FieldControl(field: AgentField, draft: Draft) -> impl IntoView {
    let desc = field.description.clone();
    let hint = hint_of(&field);
    let secret = field.format == "secret";
    let stored = field.has_value;
    // A mount answers from the **form**, not from the server: the folder just
    // picked is in the draft and not yet stored, and `has_value` is the same
    // question the Start buttons ask of it (§5.8).
    let mount = is_mount(&field).then(|| StoredValue::new(field.clone()));
    let control = control_for(&field, draft);
    view! {
        <div class="fr" class:wide=is_wide(&field)>
            <label class="fr-label">
                {label_of(&field)}
                {(!hint.is_empty()).then(|| view! { " " <span class="fname">{hint}</span> })}
                // Whether a secret is stored belongs on the label, not beside
                // the box: the box is where you type the new one, and a chip
                // next to it wraps in a narrow grid cell.
                {secret
                    .then(|| {
                        view! {
                            " "
                            <span class=if stored { "chip ok" } else { "chip off" }>
                                {if stored { "set" } else { "not set" }}
                            </span>
                        }
                    })}
                {mount
                    .map(|f| {
                        view! {
                            " "
                            <span class=move || {
                                if draft.has_value(&f.get_value()) { "chip ok" } else { "chip off" }
                            }>
                                {move || {
                                    if draft.has_value(&f.get_value()) { "set" } else { "not set" }
                                }}
                            </span>
                        }
                    })}
            </label>
            <div class="fr-ctl">{control}</div>
            <div class="fr-help">
                {(!desc.trim().is_empty()).then(|| view! { <div>{desc}</div> })}
                // On the field, because this is where the owner decides — the
                // run log saying it afterwards would be saying it too late
                // (§5.5). Folded to its one line; both halves are in it.
                {mount
                    .is_some()
                    .then(|| {
                        view! {
                            <Explain summary=RELABEL_SUMMARY persist="agents.relabel">
                                {RELABEL_NOTE}
                            </Explain>
                        }
                    })}
            </div>
        </div>
    }
}

fn control_for(f: &AgentField, draft: Draft) -> AnyView {
    let name = StoredValue::new(f.name.clone());
    match (f.ty.as_str(), f.format.as_str()) {
        ("boolean", _) => view! {
            <label class="check">
                <input
                    type="checkbox"
                    prop:checked=move || draft.flag(&name.get_value())
                    on:change=move |ev| draft.set_flag(&name.get_value(), event_target_checked(&ev))
                />
                {move || if draft.flag(&name.get_value()) { "on" } else { "off" }}
            </label>
        }
        .into_any(),
        ("string", "secret") => {
            let stored = f.has_value;
            view! {
                <div class="row secret-row">
                    <input
                        class="input mono"
                        type="password"
                        style="flex:1"
                        disabled=move || draft.is_cleared(&name.get_value())
                        placeholder=if stored {
                            "stored — leave empty to keep it"
                        } else {
                            "not set yet"
                        }
                        prop:value=move || draft.text(&name.get_value())
                        on:input=move |ev| draft.set_text(&name.get_value(), event_target_value(&ev))
                    />
                    // Only for a field that has something to forget: an empty
                    // submission keeps the stored secret, so this is the one
                    // way to say "revoked, drop it" — and it is deliberately a
                    // pending state the Save confirms, not a second endpoint
                    // that fires on click.
                    {stored
                        .then(|| {
                            view! {
                                <label
                                    class="check"
                                    title="remove the stored value when you save"
                                >
                                    <input
                                        type="checkbox"
                                        prop:checked=move || draft.is_cleared(&name.get_value())
                                        on:change=move |ev| {
                                            let on = event_target_checked(&ev);
                                            draft.set_cleared(&name.get_value(), on);
                                            if on {
                                                draft.set_text(&name.get_value(), String::new());
                                            }
                                        }
                                    />
                                    "clear"
                                </label>
                            }
                        })}
                </div>
            }
            .into_any()
        }
        // A host mount (§5.1): the owner names a folder or a file on the machine
        // lmgw runs on, and the container sees it at `/lmgw/mounts/<name>`. The
        // secret's shape, because the row answers the same two questions — is
        // something bound, and how do I change it.
        ("string", "directory" | "file") => {
            let chip = mount_chip(f);
            let directory = f.format == "directory";
            view! {
                <MountControl
                    name=f.name.clone()
                    chip=chip
                    directory=directory
                    title=label_of(f)
                    draft=draft
                />
            }
            .into_any()
        }
        ("string", "multiline") => view! {
            <textarea
                class="input ta mono"
                prop:value=move || draft.text(&name.get_value())
                on:input=move |ev| draft.set_text(&name.get_value(), event_target_value(&ev))
            ></textarea>
        }
        .into_any(),
        ("string", "model_alias") => view! {
            <DraftModel
                name=name.get_value()
                draft=draft
                empty=default_label(f)
                fallback=f.default.as_ref().map(value_to_text).unwrap_or_default()
            />
        }
        .into_any(),
        ("array", _) => view! { <ListControl name=name.get_value() draft=draft/> }.into_any(),
        _ if !f.enum_values.is_empty() => {
            let values = f.enum_values.clone();
            let options =
                Signal::derive(move || values.iter().map(|v| (v.clone(), v.clone())).collect());
            view! { <DraftSelect name=name.get_value() draft=draft options=options/> }.into_any()
        }
        _ => {
            let numeric = f.ty == "integer" || f.ty == "number";
            view! {
                <input
                    class=if numeric { "input mono w-num" } else { "input mono" }
                    inputmode=if numeric { "numeric" } else { "text" }
                    prop:value=move || draft.text(&name.get_value())
                    on:input=move |ev| draft.set_text(&name.get_value(), event_target_value(&ev))
                />
            }
            .into_any()
        }
    }
}

/// A `directory` or `file` field (mounts §5.4, §5.8).
///
/// Inside the desktop shell the box is read-only and **Choose…** opens the
/// system picker; in a browser there is no bridge to a picker, so the same box
/// is editable and says what belongs in it. Either way the value travels as an
/// ordinary string, and every rule about it — absolute, canonical, the kind the
/// format asks for, not nested in another `rw` mount — is the server's, applied
/// when it is stored and again when it is used, and named in its refusal.
#[component]
fn MountControl(
    name: String,
    /// `directory · rw` / `file · ro`, from [`mount_chip`].
    chip: String,
    /// Which of the two formats this is — and which picker to open.
    directory: bool,
    /// What the dialog is titled: what the form calls this field.
    title: String,
    draft: Draft,
) -> impl IntoView {
    let key = StoredValue::new(name);
    let title = StoredValue::new(title);
    // Asked once, and the only thing that decides between the two shapes: no
    // bridge, no picker to open (§5.4).
    let shell = crate::ui_scale::tauri_invoke().is_some();
    let bound = move || !draft.text(&key.get_value()).trim().is_empty();
    view! {
        <div class="row secret-row">
            <input
                class="input mono"
                style="flex:1"
                readonly=shell
                placeholder=if shell {
                    "not set"
                } else {
                    "absolute path on the gateway machine"
                }
                prop:value=move || draft.text(&key.get_value())
                on:input=move |ev| draft.set_text(&key.get_value(), event_target_value(&ev))
            />
            {shell
                .then(|| {
                    view! {
                        <button
                            class="btn ghost"
                            title="pick it on the machine the gateway runs on"
                            on:click=move |_| pick_path(directory, title.get_value(), draft, key)
                        >
                            "Choose…"
                        </button>
                    }
                })}
            // A mount has nothing like the secret's "leave it blank to keep it",
            // so emptying the box is the whole of clearing it: an unbound slot
            // is simply a value the start does not mount.
            <button
                class="btn ghost"
                title="unbind the slot — the agent starts without this mount"
                disabled=move || !bound()
                on:click=move |_| draft.set_text(&key.get_value(), String::new())
            >
                "Clear"
            </button>
            <span class="chip off" title="the manifest declares the mode, never this form">
                {chip}
            </span>
        </div>
    }
}

/// Open the shell's own file picker and put what it returns into the draft,
/// like any typed value (mounts §5.4).
///
/// The same `window.__TAURI_INTERNALS__.invoke` bridge the zoom drives
/// ([`crate::ui_scale::tauri_invoke`]), with the dialog plugin's own command
/// and its own argument shape — its options travel under one `options` key, the
/// way its JS binding sends them. Nothing in lmgw-core knows a dialog exists.
/// `defaultPath` is left out when nothing is bound yet rather than guessed at:
/// the picker's own idea of where to start beats one invented here.
///
/// Fire and forget, like the zoom call. A cancelled dialog resolves with
/// `null` and changes nothing — clearing a bound path is the Clear button's
/// job, not a mis-click's. The promise rejects only when the
/// `dialog:allow-open` capability is missing, and an unhandled rejection in the
/// console is exactly the signal wanted then.
fn pick_path(directory: bool, title: String, draft: Draft, name: StoredValue<String>) {
    let Some((internals, invoke)) = crate::ui_scale::tauri_invoke() else {
        return;
    };
    let options = js_sys::Object::new();
    let set = |k: &str, v: &JsValue| {
        let _ = js_sys::Reflect::set(&options, &JsValue::from_str(k), v);
    };
    set("directory", &JsValue::from_bool(directory));
    // One path: a mount field holds one value, and a list is not something this
    // control could store.
    set("multiple", &JsValue::from_bool(false));
    set("title", &JsValue::from_str(&title));
    let current = draft.text_now(&name.get_value());
    if !current.trim().is_empty() {
        set("defaultPath", &JsValue::from_str(current.trim()));
    }
    let args = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&args, &JsValue::from_str("options"), &options);
    let Ok(pending) = invoke.call2(&internals, &JsValue::from_str("plugin:dialog|open"), &args)
    else {
        return;
    };
    let Ok(promise) = pending.dyn_into::<js_sys::Promise>() else {
        return;
    };
    // `once`, because the dialog answers exactly once: what the callback closed
    // over is freed when it fires, rather than held for the life of the page.
    let done = Closure::once(move |picked: JsValue| {
        if let Some(path) = picked.as_string() {
            draft.set_text(&name.get_value(), path);
        }
    });
    let _ = promise.then(&done);
    done.forget();
}

/// [`ModelPicker`] bound to one draft key, the way [`DraftSelect`] binds a
/// `Select`.
///
/// It opens scoped to the task of the model the form opened on — the value
/// the manifest's author (or the owner) already chose says what kind of model
/// the field wants, and nothing else here does. An empty field ("manifest
/// default") asks the same of the default it falls back to (code:A6). The
/// scope is fixed once the catalog knows that model, so picking across tasks
/// through "+N other tasks" does not re-scope the picker under the pointer. An
/// alias the gateway no longer serves is pinned "(not served)" by the picker
/// itself.
#[component]
fn DraftModel(
    name: String,
    draft: Draft,
    empty: String,
    /// The schema default, which an empty value runs on.
    fallback: String,
) -> impl IntoView {
    let key = StoredValue::new(name);
    let value = RwSignal::new(draft.text_now(&key.get_value()));
    Effect::new(move |_| {
        let picked = value.get();
        draft.set_text(&key.get_value(), picked);
    });
    let catalog = use_model_catalog();
    let seed = Some(value.get_untracked())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(fallback);
    let tasks = Memo::new(move |prev: Option<&&'static [&'static str]>| {
        if let Some(t) = prev.filter(|t| !t.is_empty()) {
            return *t;
        }
        catalog.entries.with(|es| {
            task_scope(
                es.iter()
                    .find(|e| e.id == seed)
                    .and_then(|e| e.task.as_deref()),
            )
        })
    });
    let empty = StoredValue::new(empty);
    move || {
        view! {
            <ModelPicker
                value=value
                tasks=tasks.get()
                empty_label=empty.get_value()
                recent_key="agents"
            />
        }
    }
}

/// An `array` of strings as one comma-separated box that wraps and grows with
/// its text (ux:U-20): a single-line input showed mail-labeler's taxonomy as
/// "…Finance, Receipts, Invoi" at 1440 wide, so the list being edited was
/// never all on screen. Re-measured when the text changes and when the box
/// changes width, which is what changes where it wraps.
#[component]
fn ListControl(name: String, draft: Draft) -> impl IntoView {
    let key = StoredValue::new(name);
    let boxed: NodeRef<leptos::html::Div> = NodeRef::new();
    let area: NodeRef<leptos::html::Textarea> = NodeRef::new();
    let size = crate::charts::use_element_size(boxed);
    Effect::new(move |_| {
        size.track();
        draft.text(&key.get_value());
        let Some(el) = area.get() else { return };
        let style = web_sys::HtmlElement::style(&el);
        let _ = style.set_property("height", "auto");
        // The borders on top of the content height: `.input` is border-box.
        let _ = style.set_property("height", &format!("{}px", el.scroll_height() + 2));
    });
    view! {
        <div class="list-box" node_ref=boxed>
            <textarea
                class="input ta list-ta"
                rows="1"
                node_ref=area
                prop:value=move || draft.text(&key.get_value())
                on:input=move |ev| draft.set_text(&key.get_value(), event_target_value(&ev))
            ></textarea>
        </div>
    }
}

/// [`Select`] bound to one draft key. The widget owns an `RwSignal<String>`, so
/// the row keeps one and mirrors it into the draft; the row is rebuilt whenever
/// the draft is reloaded (see [`Draft::generation`]), which is what keeps the
/// two from drifting.
#[component]
fn DraftSelect(
    name: String,
    draft: Draft,
    #[prop(into)] options: Signal<Vec<(String, String)>>,
) -> impl IntoView {
    let key = StoredValue::new(name);
    let value = RwSignal::new(draft.text_now(&key.get_value()));
    Effect::new(move |_| {
        let picked = value.get();
        draft.set_text(&key.get_value(), picked);
    });
    view! { <Select value=value options=options placeholder="pick one"/> }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unsaved_is_what_differs_from_the_loaded_form() {
        let base = initial_draft(
            &[
                field("prefix", "string", ""),
                field("dry_run", "boolean", ""),
                field("token", "string", "secret"),
            ],
            &json!({"prefix": "AI/", "dry_run": false, "token": {"has_value": true}}),
        );
        let mut v = base.clone();
        assert!(changed_fields(&base, &v, &[]).is_empty());
        // trailing space: not what a save would send differently
        v.insert("prefix".into(), json!("AI/ "));
        assert!(changed_fields(&base, &v, &[]).is_empty());
        v.insert("prefix".into(), json!("Mail/"));
        v.insert("dry_run".into(), json!(true));
        v.insert("token".into(), json!("s3cret"));
        assert_eq!(
            changed_fields(&base, &v, &[]),
            ["dry_run", "prefix", "token"]
        );
        // a secret ticked for forgetting is an edit too
        assert_eq!(changed_fields(&base, &base, &["token".into()]), ["token"]);
    }

    fn field(name: &str, ty: &str, format: &str) -> AgentField {
        AgentField {
            name: name.to_string(),
            ty: ty.to_string(),
            format: format.to_string(),
            ..Default::default()
        }
    }

    /// The form opens on what the run would actually use: the stored value,
    /// else the schema default.
    #[test]
    fn the_draft_falls_back_to_the_schema_default() {
        let mut limit = field("limit", "integer", "");
        limit.default = Some(json!(50));
        let mut prefix = field("label_prefix", "string", "");
        prefix.default = Some(json!("lmgw"));
        let fields = vec![limit, prefix];

        let d = initial_draft(&fields, &json!({ "label_prefix": "mine" }));
        assert_eq!(d["limit"], json!("50"));
        assert_eq!(d["label_prefix"], json!("mine"));
    }

    /// An array field is one comma-separated line both ways, and round-trips
    /// through whitespace the way a person types it.
    #[test]
    fn an_array_field_is_comma_separated_both_ways() {
        let fields = vec![field("categories", "array", "")];
        let d = initial_draft(&fields, &json!({ "categories": ["Work", "Finance"] }));
        assert_eq!(d["categories"], json!("Work, Finance"));

        let mut draft = Map::new();
        draft.insert("categories".into(), json!(" Work , Finance ,, Travel "));
        let patch = build_patch(&fields, &draft).unwrap();
        assert_eq!(patch["categories"], json!(["Work", "Finance", "Travel"]));

        // The box wraps and takes Enter: a line break ends an item too.
        draft.insert("categories".into(), json!("Work,\nFinance\nTravel,"));
        let patch = build_patch(&fields, &draft).unwrap();
        assert_eq!(patch["categories"], json!(["Work", "Finance", "Travel"]));

        // An emptied box is an empty list, not an absent field: it is an edit.
        draft.insert("categories".into(), json!("  "));
        let patch = build_patch(&fields, &draft).unwrap();
        assert_eq!(patch["categories"], json!([]));
    }

    /// A secret is never shown and an untouched one is never sent, so the
    /// stored value survives every other edit on the form (§2.6).
    #[test]
    fn a_secret_is_never_prefilled_and_an_empty_one_is_not_sent() {
        let mut secret = field("token", "string", "secret");
        secret.has_value = true;
        let fields = vec![secret];

        let d = initial_draft(&fields, &json!({ "token": { "has_value": true } }));
        assert_eq!(d["token"], json!(""));

        let patch = build_patch(&fields, &d).unwrap();
        assert!(
            !patch.contains_key("token"),
            "an empty secret must be omitted so the server keeps it: {patch:?}"
        );

        let mut typed = Map::new();
        typed.insert("token".into(), json!("s3cr3t"));
        assert_eq!(
            build_patch(&fields, &typed).unwrap()["token"],
            json!("s3cr3t")
        );
    }

    /// What the Run tab's buttons read to tell "not filled in" from "filled in
    /// and not saved".
    #[test]
    fn the_form_answers_for_a_stored_secret_it_never_shows() {
        let model = field("model", "string", "model_alias");
        assert!(!has_value(&model, false, ""));
        assert!(!has_value(&model, false, "   "));
        assert!(has_value(&model, false, "qwen3.8"));
        // Ticked for clearing: gone on save, so gone here too.
        assert!(!has_value(&model, true, "qwen3.8"));

        let mut secret = field("token", "string", "secret");
        secret.has_value = true;
        // The box is blank by design and the stored value is what counts.
        assert!(has_value(&secret, false, ""));
        assert!(!has_value(&secret, true, ""));
        let unfilled = field("token", "string", "secret");
        assert!(!has_value(&unfilled, false, ""));

        // A checkbox always has an answer, so it is never the thing missing.
        assert!(has_value(&field("dry_run", "boolean", ""), false, ""));
    }

    /// Numbers go out as numbers, a cleared one as `null`, and text that is not
    /// a number at all is refused here — naming the field — because there is no
    /// value to send for it.
    #[test]
    fn numbers_are_typed_and_junk_is_refused_by_name() {
        let mut limit = field("limit", "integer", "");
        limit.title = "Max messages".into();
        let temp = field("temperature", "number", "");
        let fields = vec![limit, temp];

        let mut draft = Map::new();
        draft.insert("limit".into(), json!(" 50 "));
        draft.insert("temperature".into(), json!("0.25"));
        let patch = build_patch(&fields, &draft).unwrap();
        assert_eq!(patch["limit"], json!(50));
        assert_eq!(patch["temperature"], json!(0.25));

        draft.insert("limit".into(), json!(""));
        assert_eq!(build_patch(&fields, &draft).unwrap()["limit"], Value::Null);

        draft.insert("limit".into(), json!("fifty"));
        let err = build_patch(&fields, &draft).unwrap_err();
        assert!(
            err.contains("Max messages") && err.contains("whole number"),
            "the error must name the field as the form labels it: {err}"
        );
    }

    /// Booleans are always sent, so unticking a box is an edit rather than a
    /// field the server never hears about.
    #[test]
    fn a_checkbox_always_submits() {
        let fields = vec![field("dry_run", "boolean", "")];
        let mut draft = Map::new();
        draft.insert("dry_run".into(), json!(false));
        assert_eq!(
            build_patch(&fields, &draft).unwrap()["dry_run"],
            json!(false)
        );
    }

    /// A field's control spans the help column only when the control needs
    /// the width. A description has a column of its own now, so it never
    /// widens its field — at any length.
    #[test]
    fn only_a_control_that_needs_the_width_is_wide() {
        let plain = field("label_prefix", "string", "");
        assert!(!is_wide(&plain));
        assert!(is_wide(&field("categories", "array", "")));
        assert!(is_wide(&field("system", "string", "multiline")));

        let mut long = plain;
        long.description = "How many unread messages one run looks at. The list call is \
                            cheap; the classify calls are not."
            .into();
        assert!(!is_wide(&long));
    }

    /// An emptied model picker is "use the manifest's default", which the API
    /// spells `null`; an empty string would be an alias of its own.
    #[test]
    fn an_empty_model_field_goes_out_as_null() {
        let mut model = field("model", "string", "model_alias");
        model.default = Some(json!("gemma4-12b"));
        let fields = vec![model.clone()];
        let mut draft = Map::new();
        draft.insert("model".into(), json!("  "));
        assert_eq!(build_patch(&fields, &draft).unwrap()["model"], Value::Null);
        draft.insert("model".into(), json!("qwen3.8"));
        assert_eq!(
            build_patch(&fields, &draft).unwrap()["model"],
            json!("qwen3.8")
        );
        assert_eq!(default_label(&model), "manifest default · gemma4-12b");
        assert_eq!(
            default_label(&field("rerank", "string", "model_alias")),
            "not set · the manifest has no default"
        );
    }

    /// The picker opens on the task of the model the field holds; a task this
    /// build has no scope for, or no model at all, scopes nothing.
    #[test]
    fn the_picker_scope_follows_the_held_models_task() {
        assert_eq!(task_scope(Some("embedding")), &["embedding"]);
        assert_eq!(task_scope(Some("chat")), &["chat"]);
        assert!(task_scope(Some("video")).is_empty());
        assert!(task_scope(None).is_empty());
    }

    /// A mount's chip is the manifest's answer, and an absent `access` is `ro`
    /// — the loader's own default, printed rather than left as half a chip.
    #[test]
    fn a_mount_chip_names_the_format_and_the_declared_access() {
        let mut notes = field("notes", "string", "directory");
        assert!(is_mount(&notes));
        assert_eq!(mount_chip(&notes), "directory · ro");
        notes.access = "rw".into();
        assert_eq!(mount_chip(&notes), "directory · rw");
        assert_eq!(mount_chip(&field("digest", "string", "file")), "file · ro");

        assert!(!is_mount(&field("token", "string", "secret")));
        assert!(!is_mount(&field("system", "string", "multiline")));
    }

    /// A mount takes the whole row — a path, two buttons and a chip, with the
    /// relabel sentence under them — and the grey note on the label leaves the
    /// format to the chip instead of saying it twice on one line.
    #[test]
    fn a_mount_field_owns_its_row_and_leaves_the_format_to_the_chip() {
        let mut notes = field("notes", "string", "directory");
        notes.access = "rw".into();
        notes.required = true;
        assert!(is_wide(&notes));
        assert_eq!(hint_of(&notes), "required");
        // Every other format still says what it is there.
        assert_eq!(
            hint_of(&field("system", "string", "multiline")),
            "multiline"
        );
    }

    /// A bound path is an ordinary string both ways, and an emptied box is an
    /// edit the server hears: a mount has no "leave it blank to keep it".
    #[test]
    fn a_mount_value_travels_like_any_other_string() {
        let fields = vec![field("notes", "string", "directory")];
        let d = initial_draft(&fields, &json!({ "notes": "/home/alice/Notes" }));
        assert_eq!(d["notes"], json!("/home/alice/Notes"));
        assert_eq!(
            build_patch(&fields, &d).unwrap()["notes"],
            json!("/home/alice/Notes")
        );

        // Clear empties the box; the slot is then simply unbound.
        let mut cleared = Map::new();
        cleared.insert("notes".into(), json!(""));
        assert_eq!(build_patch(&fields, &cleared).unwrap()["notes"], json!(""));

        // And the set/not-set chip follows the box, not a server flag.
        assert!(has_value(&fields[0], false, "/home/alice/Notes"));
        assert!(!has_value(&fields[0], false, "  "));
    }

    /// The label the form shows, and the note beside it: bounds are visible,
    /// never a silent clamp.
    #[test]
    fn the_hint_shows_the_bounds_the_server_will_enforce() {
        let mut f = field("concurrency", "integer", "");
        f.minimum = Some(1.0);
        f.required = true;
        assert_eq!(label_of(&f), "concurrency");
        f.title = "Parallel calls".into();
        assert_eq!(label_of(&f), "Parallel calls");
        assert_eq!(hint_of(&f), "integer · min 1 · required");
    }
}
