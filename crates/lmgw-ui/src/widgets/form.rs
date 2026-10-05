//! Forms that know what changed (UX plan §2 #13): one draft per page instead
//! of a Save button per card, a bar that always says how much is unsaved,
//! inline validation that blocks Save, and explanations folded to their
//! first sentence.
//!
//! [`FormState`] works on flat dotted keys (`"vram.headroom_mb"`) and turns
//! the changed ones back into the nested patch the settings ops take:
//!
//! ```ignore
//! let form = FormState::new(flatten(&json!({"vram": {"headroom_mb": 512}})));
//! view! {
//!     <Field label="Headroom" unit="MiB" dirty=form.dirty_signal("vram.headroom_mb")
//!            error=form.error_signal("vram.headroom_mb")>
//!         <input class="input" prop:value=move || form.text("vram.headroom_mb")
//!                on:input=move |ev| form.set_text("vram.headroom_mb", event_target_value(&ev))/>
//!     </Field>
//! }
//! // on save: form.patch() == Ok({"vram": {"headroom_mb": 768}})
//! ```

// The area phases adopt these page by page (Settings first, Phase 3).
#![allow(dead_code)]

use std::collections::BTreeMap;

use leptos::html;
use leptos::prelude::*;
use leptos::wasm_bindgen::closure::Closure;
use serde_json::{Map, Number, Value};
use wasm_bindgen::JsCast;

use super::modal::counts_as_edit;

/// How a field's text becomes JSON. Inferred from the baseline value
/// ([`Kind::infer`]); declare it with [`FormState::with_kind`] where the
/// baseline cannot say — a number that is `null` today is still a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A string, trimmed.
    Text,
    /// A string, trimmed; empty is `null`.
    OptText,
    /// A whole number; empty is an error.
    Int,
    /// A whole number; empty is `null`.
    OptInt,
    /// A number; empty is an error.
    Float,
    /// A number; empty is `null`.
    OptFloat,
    /// A checkbox.
    Flag,
    /// Any JSON, set whole with [`FormState::set_value`] (a list, an object).
    Raw,
}

impl Kind {
    pub fn infer(v: &Value) -> Kind {
        match v {
            Value::Bool(_) => Kind::Flag,
            Value::Number(n) if n.is_f64() => Kind::Float,
            Value::Number(_) => Kind::Int,
            Value::String(_) => Kind::Text,
            Value::Null => Kind::OptText,
            Value::Array(_) | Value::Object(_) => Kind::Raw,
        }
    }
}

/// The value as the draft holds it: text for the text-like kinds (what the
/// input shows), the value itself otherwise.
fn render(kind: Kind, v: &Value) -> Value {
    match kind {
        Kind::Flag | Kind::Raw => v.clone(),
        _ => Value::String(match v {
            Value::Null => String::new(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }),
    }
}

/// A draft value as the JSON a patch carries, or the message that says why
/// it cannot be one.
pub fn normalize(kind: Kind, v: &Value) -> Result<Value, String> {
    let text = || match v {
        Value::String(s) => s.trim().to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    match kind {
        Kind::Flag => Ok(Value::Bool(v.as_bool().unwrap_or(false))),
        Kind::Raw => Ok(v.clone()),
        Kind::Text => Ok(Value::String(text())),
        Kind::OptText => {
            let t = text();
            Ok(if t.is_empty() {
                Value::Null
            } else {
                Value::String(t)
            })
        }
        Kind::Int | Kind::OptInt => {
            let t = text();
            if t.is_empty() {
                return if kind == Kind::OptInt {
                    Ok(Value::Null)
                } else {
                    Err("a whole number is required".to_string())
                };
            }
            if let Ok(n) = t.parse::<i64>() {
                Ok(Value::from(n))
            } else if let Ok(n) = t.parse::<u64>() {
                Ok(Value::from(n))
            } else {
                Err(format!("\u{201c}{t}\u{201d} is not a whole number"))
            }
        }
        Kind::Float | Kind::OptFloat => {
            let t = text();
            if t.is_empty() {
                return if kind == Kind::OptFloat {
                    Ok(Value::Null)
                } else {
                    Err("a number is required".to_string())
                };
            }
            t.parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .and_then(Number::from_f64)
                .map(Value::Number)
                .ok_or_else(|| format!("\u{201c}{t}\u{201d} is not a number"))
        }
    }
}

/// Equal as values: numbers by magnitude (`8` and `8.0` are the same
/// number), everything else structurally.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// Nested JSON → dotted keys. Objects are walked; everything else (arrays
/// included) is a leaf.
pub fn flatten(v: &Value) -> Map<String, Value> {
    fn walk(prefix: &str, v: &Value, out: &mut Map<String, Value>) {
        match v {
            Value::Object(m) if !m.is_empty() => {
                for (k, v) in m {
                    let key = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    walk(&key, v, out);
                }
            }
            leaf => {
                out.insert(prefix.to_string(), leaf.clone());
            }
        }
    }
    let mut out = Map::new();
    walk("", v, &mut out);
    out
}

/// Dotted keys → nested JSON: `{"vram.headroom_mb": 5}` → `{"vram":
/// {"headroom_mb": 5}}`.
pub fn nest(flat: impl IntoIterator<Item = (String, Value)>) -> Value {
    let mut root = Map::new();
    for (key, v) in flat {
        let mut parts: Vec<&str> = key.split('.').collect();
        let last = parts.pop().unwrap_or_default();
        let mut node = &mut root;
        for p in parts {
            let slot = node
                .entry(p.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if !slot.is_object() {
                *slot = Value::Object(Map::new());
            }
            node = slot.as_object_mut().expect("just made an object");
        }
        node.insert(last.to_string(), v);
    }
    Value::Object(root)
}

/// The pure core of [`FormState::patch`]: every changed key, nested, or
/// every changed key that does not parse with its message.
pub fn build_patch(
    baseline: &Map<String, Value>,
    draft: &Map<String, Value>,
    kinds: &BTreeMap<String, Kind>,
) -> Result<Value, BTreeMap<String, String>> {
    let mut changed = Vec::new();
    let mut errors = BTreeMap::new();
    for (k, d) in draft {
        let kind = kind_of(kinds, baseline, k);
        match normalize(kind, d) {
            Ok(v) => {
                if !is_baseline(kind, baseline.get(k), &v) {
                    changed.push((k.clone(), v));
                }
            }
            Err(e) => {
                errors.insert(k.clone(), e);
            }
        }
    }
    if errors.is_empty() {
        Ok(nest(changed))
    } else {
        Err(errors)
    }
}

fn kind_of(kinds: &BTreeMap<String, Kind>, baseline: &Map<String, Value>, k: &str) -> Kind {
    kinds
        .get(k)
        .copied()
        .unwrap_or_else(|| baseline.get(k).map_or(Kind::OptText, Kind::infer))
}

/// Is `v` (normalized) what the baseline already holds? The baseline is
/// normalized the same way, so a stored `"abc "` is not "changed" to `"abc"`.
fn is_baseline(kind: Kind, base: Option<&Value>, v: &Value) -> bool {
    let base = base.cloned().unwrap_or(Value::Null);
    match normalize(kind, &render(kind, &base)) {
        Ok(b) => same(&b, v),
        Err(_) => false,
    }
}

fn is_dirty_in(
    baseline: &Map<String, Value>,
    draft: &Map<String, Value>,
    kinds: &BTreeMap<String, Kind>,
    k: &str,
) -> bool {
    let Some(d) = draft.get(k) else { return false };
    let kind = kind_of(kinds, baseline, k);
    match normalize(kind, d) {
        Ok(v) => !is_baseline(kind, baseline.get(k), &v),
        // Text that does not parse is never what was saved.
        Err(_) => true,
    }
}

/// A page's (or a modal's) draft over flat dotted keys. `Copy`: pass it
/// around like a signal.
#[derive(Clone, Copy)]
pub struct FormState {
    baseline: RwSignal<Map<String, Value>>,
    pub draft: RwSignal<Map<String, Value>>,
    /// Per-key messages: a value that does not parse, or what the server
    /// said about a field ([`Self::set_error`]).
    pub errors: RwSignal<BTreeMap<String, String>>,
    kinds: RwSignal<BTreeMap<String, Kind>>,
}

impl FormState {
    pub fn new(flat: Map<String, Value>) -> Self {
        let form = Self {
            baseline: RwSignal::new(Map::new()),
            draft: RwSignal::new(Map::new()),
            errors: RwSignal::new(BTreeMap::new()),
            kinds: RwSignal::new(BTreeMap::new()),
        };
        form.rebase(flat);
        form
    }

    /// Declare a field's kind where the baseline cannot say (a `null` that
    /// is a number). Before the first edit.
    ///
    /// Like every untracked read in here, it does nothing on a form whose
    /// page is gone: a load that answers after the page was left must not
    /// trap the module on a disposed signal (review code:C4).
    pub fn with_kind(self, key: &str, kind: Kind) -> Self {
        self.kinds.update(|k| {
            k.insert(key.to_string(), kind);
        });
        let Some(base) = self
            .baseline
            .try_with_untracked(|b| b.get(key).cloned().unwrap_or(Value::Null))
        else {
            return self;
        };
        self.draft.update(|d| {
            d.insert(key.to_string(), render(kind, &base));
        });
        self
    }

    fn kind(&self, k: &str) -> Kind {
        self.kinds
            .try_with_untracked(|kinds| self.baseline.try_with_untracked(|b| kind_of(kinds, b, k)))
            .flatten()
            .unwrap_or(Kind::OptText)
    }

    /// A new baseline — what the server holds now, after a save or a
    /// refetch. Every draft resets to it and every message clears.
    pub fn rebase(&self, flat: Map<String, Value>) {
        let Some(kinds) = self.kinds.try_get_untracked() else {
            return;
        };
        let draft = flat
            .iter()
            .map(|(k, v)| {
                let kind = kinds.get(k).copied().unwrap_or_else(|| Kind::infer(v));
                (k.clone(), render(kind, v))
            })
            .collect();
        self.baseline.set(flat);
        self.draft.set(draft);
        self.errors.set(BTreeMap::new());
    }

    /// The draft as it is now — what a save sends from. Hand it back to
    /// [`Self::saved`] once the save is through.
    pub fn snapshot(&self) -> Map<String, Value> {
        self.draft.try_get_untracked().unwrap_or_default()
    }

    /// A save of `sent` (a [`Self::snapshot`]) went through and `now` is what
    /// the server holds since. Like [`Self::rebase`], except that a key typed
    /// into while the save was out keeps its newer text, and stays dirty
    /// against the new baseline: the fields are not locked during a save, so
    /// what was typed then must not vanish (review code:S1).
    pub fn saved(&self, sent: &Map<String, Value>, now: Map<String, Value>) {
        let Some(current) = self.draft.try_get_untracked() else {
            return;
        };
        let later: Vec<(String, Value)> = current
            .into_iter()
            .filter(|(k, v)| sent.get(k) != Some(v))
            .collect();
        self.rebase(now);
        for (k, v) in later {
            match v {
                Value::String(s) => self.set_text(&k, s),
                other => self.set_value(&k, other),
            }
        }
    }

    /// What the server holds after a save of `sent` when it could not be read
    /// back: the baseline with every changed key of `sent` in its normalized
    /// form (review code:S4). Fields the server rewrites on the way in
    /// (trimming, a prefix that loses its slashes) show as sent until the
    /// next load.
    pub fn baseline_after(&self, sent: &Map<String, Value>) -> Map<String, Value> {
        let (Some(mut base), Some(kinds)) = (
            self.baseline.try_get_untracked(),
            self.kinds.try_get_untracked(),
        ) else {
            return Map::new();
        };
        for (k, d) in sent {
            let kind = kind_of(&kinds, &base, k);
            if let Ok(v) = normalize(kind, d) {
                if !is_baseline(kind, base.get(k), &v) {
                    base.insert(k.clone(), v);
                }
            }
        }
        base
    }

    /// The text an input shows (tracked).
    pub fn text(&self, k: &str) -> String {
        self.draft.with(|d| match d.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        })
    }

    /// Typed text; a value that does not parse gets its message right away.
    pub fn set_text(&self, k: &str, text: impl Into<String>) {
        let v = Value::String(text.into());
        let msg = normalize(self.kind(k), &v).err();
        self.draft.update(|d| {
            d.insert(k.to_string(), v);
        });
        self.errors.update(|e| match msg {
            Some(m) => {
                e.insert(k.to_string(), m);
            }
            None => {
                e.remove(k);
            }
        });
    }

    pub fn flag(&self, k: &str) -> bool {
        self.draft
            .with(|d| d.get(k).and_then(Value::as_bool).unwrap_or(false))
    }

    pub fn set_flag(&self, k: &str, on: bool) {
        self.set_value(k, Value::Bool(on));
    }

    /// A whole value (a `Raw` list, a flag).
    pub fn set_value(&self, k: &str, v: Value) {
        self.draft.update(|d| {
            d.insert(k.to_string(), v);
        });
        self.errors.update(|e| {
            e.remove(k);
        });
    }

    /// The draft value (tracked), for a `Raw` field.
    pub fn value(&self, k: &str) -> Value {
        self.draft
            .with(|d| d.get(k).cloned().unwrap_or(Value::Null))
    }

    pub fn is_dirty(&self, k: &str) -> bool {
        let kinds = self.kinds.get();
        self.baseline
            .with(|b| self.draft.with(|d| is_dirty_in(b, d, &kinds, k)))
    }

    pub fn dirty_keys(&self) -> Vec<String> {
        let kinds = self.kinds.get();
        self.baseline.with(|b| {
            self.draft.with(|d| {
                d.keys()
                    .filter(|k| is_dirty_in(b, d, &kinds, k))
                    .cloned()
                    .collect()
            })
        })
    }

    pub fn dirty_count(&self) -> usize {
        self.dirty_keys().len()
    }

    /// Fields whose text does not parse — what blocks Save. A server's
    /// message on a field is shown but does not block: the next save is how
    /// it is answered.
    pub fn invalid_count(&self) -> usize {
        let kinds = self.kinds.get();
        self.baseline.with(|b| {
            self.draft.with(|d| {
                d.iter()
                    .filter(|(k, v)| normalize(kind_of(&kinds, b, k), v).is_err())
                    .count()
            })
        })
    }

    pub fn error(&self, k: &str) -> Option<String> {
        self.errors.with(|e| e.get(k).cloned())
    }

    /// A message on one field — typically what a refused save said about it.
    /// It stays until the field is edited or the form rebased.
    pub fn set_error(&self, k: &str, msg: impl Into<String>) {
        self.errors.update(|e| {
            e.insert(k.to_string(), msg.into());
        });
    }

    /// The changed keys as the nested patch a settings op takes; nothing
    /// that is unchanged is sent. On `Err` every field that does not parse
    /// has its message set, and the map says which.
    pub fn patch(&self) -> Result<Value, BTreeMap<String, String>> {
        let res = self
            .baseline
            .try_with_untracked(|b| {
                self.draft
                    .try_with_untracked(|d| self.kinds.try_with_untracked(|k| build_patch(b, d, k)))
            })
            .flatten()
            .flatten()
            // A form that is gone has nothing left to send.
            .unwrap_or_else(|| Ok(Value::Object(Map::new())));
        if let Err(errs) = &res {
            self.errors.update(|e| e.extend(errs.clone()));
        }
        res
    }

    /// Back to the baseline.
    pub fn discard(&self) {
        if let Some(b) = self.baseline.try_get_untracked() {
            self.rebase(b);
        }
    }

    /// `is_dirty(k)` as a signal, for [`Field`]'s `dirty`.
    pub fn dirty_signal(&self, k: &str) -> Signal<bool> {
        let (form, k) = (*self, k.to_string());
        Signal::derive(move || form.is_dirty(&k))
    }

    /// `error(k)` as a signal, for [`Field`]'s `error`.
    pub fn error_signal(&self, k: &str) -> Signal<Option<String>> {
        let (form, k) = (*self, k.to_string());
        Signal::derive(move || form.error(&k))
    }

    /// The SaveBar's detail: changed keys counted per group, by key prefix
    /// — `[("vram.", "GPU"), ("runtimes.", "Runtimes")]` → "GPU (2) ·
    /// Runtimes (1)". Keys under no prefix are not listed.
    pub fn dirty_by(&self, groups: &'static [(&'static str, &'static str)]) -> Signal<String> {
        let form = *self;
        Signal::derive(move || {
            let keys = form.dirty_keys();
            groups
                .iter()
                .filter_map(|(prefix, label)| {
                    let n = keys.iter().filter(|k| k.starts_with(prefix)).count();
                    (n > 0).then(|| format!("{label} ({n})"))
                })
                .collect::<Vec<_>>()
                .join(" · ")
        })
    }
}

/// A labelled control: `.field` with its label (and unit), the control,
/// then an inline error and a hint. `dirty` draws the amber rule the dirty
/// state is known by; `error` marks the control red and says why; `warn`
/// marks it amber and says what is off without blocking anything (Settings:
/// a stored value no save is judging).
#[component]
pub fn Field(
    #[prop(into)] label: TextProp,
    #[prop(optional, into)] hint: Option<TextProp>,
    /// "MiB", "s", "0 = unlimited": after the label, dimmed.
    #[prop(optional, into)]
    unit: Option<TextProp>,
    /// Spans the whole field grid row.
    #[prop(optional)]
    wide: bool,
    #[prop(optional, into)] dirty: Option<Signal<bool>>,
    #[prop(optional, into)] error: Option<Signal<Option<String>>>,
    #[prop(optional, into)] warn: Option<Signal<Option<String>>>,
    /// An anchor to link to ("Settings → GPU → Hold").
    #[prop(optional, into)]
    id: Option<String>,
    /// Out of the layout while true — a search that does not match it. The
    /// field stays mounted, so what was typed into it stays too.
    #[prop(optional, into)]
    hidden: Option<Signal<bool>>,
    /// Rendered after the hint's text, inside it — a link to where the
    /// inherited value is set ("Settings → Runtimes → Chat").
    #[prop(optional, into)]
    hint_extra: Option<ViewFn>,
    children: Children,
) -> impl IntoView {
    let is_dirty = move || dirty.is_some_and(|d| d.get());
    let msg = move || error.and_then(|e| e.get());
    let note = move || warn.and_then(|w| w.get());
    let gone = move || hidden.is_some_and(|h| h.get());
    view! {
        <div
            class="field"
            class:wide=wide
            class:dirty=is_dirty
            class:invalid=move || msg().is_some()
            class:warned=move || msg().is_none() && note().is_some()
            id=id
            hidden=gone
        >
            <label>
                {move || label.get()}
                // An empty unit or hint is no unit or hint: a table-driven
                // form passes every row's, filled or not.
                {unit
                    .map(|u| {
                        move || {
                            let t = u.get();
                            (!t.is_empty())
                                .then(|| view! { <span class="field-unit">{t.to_string()}</span> })
                        }
                    })}
            </label>
            {children()}
            {move || msg().map(|m| view! { <div class="field-err" role="alert">{m}</div> })}
            {move || note().map(|m| view! { <div class="field-warn" role="note">{m}</div> })}
            {match hint_extra {
                Some(extra) => {
                    view! {
                        // The space is written out: `view!` drops the one
                        // between two blocks, and the hint ran into its link.
                        <div class="field-hint">
                            {hint.map(|h| move || h.get().to_string())} " " {extra.run()}
                        </div>
                    }
                        .into_any()
                }
                None => {
                    hint.map(|h| {
                            move || {
                                let t = h.get();
                                (!t.is_empty())
                                    .then(|| view! { <div class="field-hint">{t.to_string()}</div> })
                            }
                        })
                        .into_any()
                }
            }}
        </div>
    }
}

/// The page foot of a form page: always rendered, so where Save lives never
/// moves. Clean it says so, quietly, with both buttons off; dirty it counts
/// ("3 unsaved · GPU (2) · Runtimes (1)"); a field that does not parse
/// blocks Save and says how many; a refused save's message stays until the
/// next save.
#[component]
pub fn SaveBar(
    #[prop(into)] dirty_count: Signal<usize>,
    /// Where the changes are, e.g. [`FormState::dirty_by`].
    #[prop(optional, into)]
    detail: Option<Signal<String>>,
    /// Fields that do not parse ([`FormState::invalid_count`]).
    #[prop(optional, into)]
    invalid: Option<Signal<usize>>,
    #[prop(into)] saving: Signal<bool>,
    #[prop(into)] error: Signal<Option<String>>,
    on_save: Callback<()>,
    on_discard: Callback<()>,
    /// The Save button's label; "Save changes" when unset.
    #[prop(optional, into)]
    save_label: Option<TextProp>,
) -> impl IntoView {
    let n = move || dirty_count.get();
    let bad = move || invalid.map_or(0, |i| i.get());
    let clean = move || n() == 0;
    // Named: a `>` inside a `view!` attribute closure ends the tag.
    let blocked = move || clean() || saving.get() || bad() != 0;
    view! {
        <div class="save-bar" class:dirty=move || !clean() role="status">
            <span class="save-state">
                {move || {
                    if clean() {
                        view! { <span class="dim">"No unsaved changes"</span> }.into_any()
                    } else {
                        let d = detail.map(|d| d.get()).filter(|d| !d.is_empty());
                        view! {
                            <b>{format!("{} unsaved", n())}</b>
                            {d.map(|d| format!(" · {d}"))}
                        }
                            .into_any()
                    }
                }}
            </span>
            {move || {
                let b = bad();
                (b > 0)
                    .then(|| {
                        let s = if b == 1 { "field" } else { "fields" };
                        view! { <span class="save-invalid">{format!("{b} {s} invalid")}</span> }
                    })
            }}
            {move || {
                error
                    .get()
                    .map(|e| {
                        let tip = e.clone();
                        view! {
                            <span class="save-err" title=tip>
                                "Not saved: "
                                {e}
                            </span>
                        }
                    })
            }}
            <span class="spacer"></span>
            <button
                type="button"
                class="btn ghost"
                disabled=move || clean() || saving.get()
                on:click=move |_| on_discard.run(())
            >
                "Discard"
            </button>
            <button
                type="button"
                class="btn primary"
                disabled=blocked
                on:click=move |_| on_save.run(())
            >
                {move || {
                    if saving.get() {
                        "Saving…".to_string()
                    } else {
                        save_label
                            .as_ref()
                            .map(|l| l.get().to_string())
                            .unwrap_or_else(|| "Save changes".to_string())
                    }
                }}
            </button>
        </div>
    }
}

/// An explanation folded to its first sentence (`<details class="explain">`),
/// remembering whether it was opened as `lmgw.ui.open.<persist>` — a wall of
/// notes read once should not greet the owner on every visit.
#[component]
pub fn Explain(
    /// The first sentence; the rest is the children.
    #[prop(into)]
    summary: TextProp,
    /// e.g. `settings.explain.gpu`.
    persist: &'static str,
    children: Children,
) -> impl IntoView {
    let open = crate::prefs::persisted_bool(&format!("open.{persist}"), false);
    view! {
        <details
            class="explain"
            prop:open=move || open.get()
            on:toggle=move |ev| {
                let now = ev
                    .target()
                    .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                    .is_some_and(|el| el.has_attribute("open"));
                if open.get_untracked() != now {
                    open.set(now);
                }
            }
        >
            <summary>{move || summary.get()}</summary>
            <div class="explain-body">{children()}</div>
        </details>
    }
}

/// Has anything inside `node` been typed into or changed? A delegated
/// `input`/`change` listener, so custom controls count once they fire
/// `change` (the Select and ModelPicker do). Filter boxes inside pops and
/// inputs marked `data-untracked` do not count.
pub fn use_touched(node: NodeRef<html::Div>) -> RwSignal<bool> {
    let touched = RwSignal::new(false);
    Effect::new(move |_| {
        let Some(el) = node.get() else { return };
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |ev: web_sys::Event| {
            if !touched.get_untracked() && counts_as_edit(&ev) {
                touched.set(true);
            }
        });
        for t in ["input", "change"] {
            let _ = el.add_event_listener_with_callback(t, cb.as_ref().unchecked_ref());
        }
        let held = StoredValue::new_local((el, cb));
        on_cleanup(move || {
            held.with_value(|(el, cb)| {
                for t in ["input", "change"] {
                    let _ = el.remove_event_listener_with_callback(t, cb.as_ref().unchecked_ref());
                }
            });
        });
    });
    touched
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> Map<String, Value> {
        flatten(&json!({
            "bind_addr": "127.0.0.1:8001",
            "max_body_mb": 64,
            "vram": {"headroom_mb": 512, "safety": 0.9},
            "hold": {"fallback_alias": "", "active": false},
            "budget": null,
        }))
    }

    fn draft_with(pairs: &[(&str, Value)]) -> Map<String, Value> {
        let b = base();
        let mut d: Map<String, Value> = b
            .iter()
            .map(|(k, v)| (k.clone(), render(Kind::infer(v), v)))
            .collect();
        for (k, v) in pairs {
            d.insert(k.to_string(), v.clone());
        }
        d
    }

    #[test]
    fn flatten_and_nest_are_inverse() {
        let b = base();
        assert_eq!(b.get("vram.headroom_mb"), Some(&json!(512)));
        assert_eq!(b.get("hold.active"), Some(&json!(false)));
        assert_eq!(
            nest(b.clone()),
            json!({
                "bind_addr": "127.0.0.1:8001",
                "max_body_mb": 64,
                "vram": {"headroom_mb": 512, "safety": 0.9},
                "hold": {"fallback_alias": "", "active": false},
                "budget": null,
            })
        );
    }

    #[test]
    fn an_untouched_draft_patches_nothing() {
        let p = build_patch(&base(), &draft_with(&[]), &BTreeMap::new()).unwrap();
        assert_eq!(p, json!({}));
    }

    #[test]
    fn dotted_keys_nest_and_only_dirty_keys_are_sent() {
        let d = draft_with(&[
            ("vram.headroom_mb", json!(" 768 ")),
            ("hold.active", json!(true)),
            // typed back to what it was: not a change
            ("max_body_mb", json!("64")),
            ("vram.safety", json!("0.90")),
        ]);
        let p = build_patch(&base(), &d, &BTreeMap::new()).unwrap();
        assert_eq!(
            p,
            json!({"vram": {"headroom_mb": 768}, "hold": {"active": true}})
        );
    }

    #[test]
    fn text_is_trimmed_and_whitespace_is_not_a_change() {
        let d = draft_with(&[("bind_addr", json!("127.0.0.1:8001  "))]);
        assert_eq!(
            build_patch(&base(), &d, &BTreeMap::new()).unwrap(),
            json!({})
        );
        let d = draft_with(&[("bind_addr", json!(" 0.0.0.0:8001"))]);
        assert_eq!(
            build_patch(&base(), &d, &BTreeMap::new()).unwrap(),
            json!({"bind_addr": "0.0.0.0:8001"})
        );
    }

    #[test]
    fn numbers_that_do_not_parse_are_errors_per_key() {
        let d = draft_with(&[
            ("vram.headroom_mb", json!("5l2")),
            ("vram.safety", json!("abc")),
            ("max_body_mb", json!("")),
            ("bind_addr", json!("fine")),
        ]);
        let errs = build_patch(&base(), &d, &BTreeMap::new()).unwrap_err();
        assert_eq!(
            errs.keys().collect::<Vec<_>>(),
            ["max_body_mb", "vram.headroom_mb", "vram.safety"]
        );
        assert!(errs["vram.headroom_mb"].contains("not a whole number"));
        assert!(errs["vram.safety"].contains("not a number"));
        assert!(errs["max_body_mb"].contains("required"));
        // a fraction is not a whole number either
        let d = draft_with(&[("max_body_mb", json!("1.5"))]);
        assert!(build_patch(&base(), &d, &BTreeMap::new()).is_err());
    }

    #[test]
    fn a_declared_optional_number_clears_to_null() {
        let kinds = BTreeMap::from([("budget".to_string(), Kind::OptFloat)]);
        let d = draft_with(&[("budget", json!("12.5"))]);
        assert_eq!(
            build_patch(&base(), &d, &kinds).unwrap(),
            json!({"budget": 12.5})
        );
        let d = draft_with(&[("budget", json!(""))]);
        assert_eq!(build_patch(&base(), &d, &kinds).unwrap(), json!({}));
        let d = draft_with(&[("budget", json!("x"))]);
        assert!(build_patch(&base(), &d, &kinds).is_err());
    }

    #[test]
    fn form_state_tracks_dirty_keys_and_errors() {
        let form = FormState::new(base()).with_kind("budget", Kind::OptFloat);
        assert_eq!(form.dirty_count(), 0);
        form.set_text("vram.headroom_mb", "1024");
        form.set_flag("hold.active", true);
        form.set_text("budget", "oops");
        assert_eq!(
            form.dirty_keys(),
            ["budget", "hold.active", "vram.headroom_mb"]
        );
        assert_eq!(form.invalid_count(), 1);
        assert!(form.error("budget").is_some());
        assert!(form.patch().is_err());

        form.set_text("budget", "");
        assert_eq!(form.error("budget"), None);
        assert_eq!(
            form.patch().unwrap(),
            json!({"vram": {"headroom_mb": 1024}, "hold": {"active": true}})
        );

        form.discard();
        assert_eq!(form.dirty_count(), 0);
        assert_eq!(form.text("vram.headroom_mb"), "512");
    }

    #[test]
    fn a_form_whose_page_is_gone_ignores_a_late_load() {
        let owner = Owner::new();
        let form = owner.with(|| FormState::new(base()));
        owner.cleanup();
        // What a settings load that answered after the page was left does:
        // nothing, rather than a read of a disposed signal.
        let form = form.with_kind("budget", Kind::OptFloat);
        form.rebase(base());
        form.discard();
        assert_eq!(form.patch().unwrap(), json!({}));
    }

    #[test]
    fn typing_while_a_save_is_out_survives_the_read_back() {
        let form = FormState::new(base());
        form.set_text("max_body_mb", "128");
        let sent = form.snapshot();
        // Typed after Save was pressed, before the answer came back.
        form.set_text("bind_addr", "0.0.0.0:8001");
        form.set_text("max_body_mb", "256");
        let mut stored = base();
        stored.insert("max_body_mb".into(), json!(128));
        form.saved(&sent, stored);
        assert_eq!(form.dirty_keys(), ["bind_addr", "max_body_mb"]);
        assert_eq!(form.text("bind_addr"), "0.0.0.0:8001");
        assert_eq!(form.text("max_body_mb"), "256");
        assert_eq!(
            form.patch().unwrap(),
            json!({"bind_addr": "0.0.0.0:8001", "max_body_mb": 256})
        );
    }

    #[test]
    fn a_save_that_could_not_be_read_back_rebases_on_what_was_sent() {
        let form = FormState::new(base()).with_kind("budget", Kind::OptFloat);
        form.set_text("max_body_mb", " 128 ");
        form.set_flag("hold.active", true);
        form.set_text("budget", "");
        let sent = form.snapshot();
        let now = form.baseline_after(&sent);
        assert_eq!(now.get("max_body_mb"), Some(&json!(128)));
        assert_eq!(now.get("hold.active"), Some(&json!(true)));
        assert_eq!(now.get("budget"), Some(&Value::Null));
        form.saved(&sent, now);
        assert_eq!(form.dirty_count(), 0);
        assert_eq!(form.text("max_body_mb"), "128");
    }

    #[test]
    fn rebase_takes_the_saved_values_as_the_new_baseline() {
        let form = FormState::new(base());
        form.set_text("max_body_mb", "128");
        let mut saved = base();
        saved.insert("max_body_mb".into(), json!(128));
        form.rebase(saved);
        assert_eq!(form.dirty_count(), 0);
        assert_eq!(form.text("max_body_mb"), "128");
    }
}
