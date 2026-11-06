//! The one **Fallback** control of every model and alias editor (openai
//! egress design §14.12): the model or alias that answers whenever this one
//! cannot. A searchable picker over the gateway catalog narrowed to the
//! owner's own model type (decision 29: a fallback is the same type of
//! model), "None" first and a typed passthrough id allowed; under it, where
//! the chosen fallback leads ("→ cloud-mini → none"), a save's refusal of
//! the fallback, and what the last save said about it.
//!
//! [`Saves`] is what an editor keeps across its saves: the row's id once
//! the first save created it (a second save is an update, never a second
//! create), the notes the saves said with the fallback they speak of (on
//! screen while the field holds that fallback, again when it is picked
//! back), the refusal under the field, and whether the form may close. A
//! create that opens the saved row's own page hands its notes to that
//! page's editor ([`Saves::carry`]).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use lmgw_api_types::ModelsFull;
use serde_json::{json, Value};

use crate::catalog::CatalogEntry;
use crate::widgets::ModelPicker;

/// Every model type a fallback can have, each its own: the tasks
/// `/v1/models` publishes (`capabilities::KNOWN_TASKS`), the image tasks one
/// type (an image row that edits also generates).
const TYPES: [&[&str]; 21] = [
    &["chat"],
    &["realtime"],
    &["embedding"],
    &["rerank"],
    &["decision"],
    &["image_generation", "image_edit"],
    &["video_generation"],
    &["tts"],
    &["asr"],
    &["vdes"],
    &["align"],
    &["gen"],
    &["clon"],
    &["vc"],
    &["svc"],
    &["s2s"],
    &["sep"],
    &["vad"],
    &["diar"],
    &["spk"],
    &["midi"],
];

/// The catalog tasks a fallback may have for an owner of `task`: its own
/// type. A task the dashboard does not know (none, or one newer than this
/// build) offers every model, and the save checks it.
pub(super) fn same_type_tasks(task: &str) -> &'static [&'static str] {
    let task = match task.trim() {
        "embed" => "embedding",
        "image" => "image_generation",
        t => t,
    };
    TYPES
        .iter()
        .find(|t| t.contains(&task))
        .copied()
        .unwrap_or(&[])
}

/// A plain alias's type, for its picker (review UI3): what its capability
/// override states (`{"capabilities": {"task": …}}`, the owner's word),
/// else its upstream catalog's entry for the model, else what the gateway
/// publishes for the alias as saved. `""` when none says.
pub(super) fn alias_task(
    override_json: &str,
    upstream: Option<String>,
    gateway: Option<String>,
) -> String {
    let stated = serde_json::from_str::<Value>(override_json.trim())
        .ok()
        .and_then(|v| {
            v.pointer("/capabilities/task")
                .and_then(Value::as_str)
                .map(String::from)
        });
    stated.or(upstream).or(gateway).unwrap_or_default()
}

/// A stored fallback as the control's value: `""` is none.
pub(super) fn fallback_value(stored: &Option<String>) -> RwSignal<String> {
    RwSignal::new(stored.clone().unwrap_or_default())
}

/// The save body's `fallback`: the name when one is picked, else `null` —
/// and `"fallback"` pushed onto `clear`, whatever the row had when the form
/// opened: a fallback set by an earlier save of the same open form is
/// removed by "None" as well (review UI1). Clearing none is no change.
pub(super) fn fallback_arg(clear: &mut Vec<&'static str>, value: RwSignal<String>) -> Value {
    let v = value.get_untracked().trim().to_string();
    if !v.is_empty() {
        return Value::from(v);
    }
    clear.push("fallback");
    Value::Null
}

/// The notes a save said, with the fallback they speak of (trimmed).
type Noted = (String, Vec<String>);

thread_local! {
    /// Notes a create handed to the editor of the row it saved
    /// ([`Saves::carry`]): that row's id and its notes. Taken once.
    static CARRIED: RefCell<Option<(i64, Noted)>> = const { RefCell::new(None) };
}

/// What an editor keeps across its saves (module doc).
#[derive(Clone, Copy)]
pub(super) struct Saves {
    /// The row's id: 0 until a create saved it.
    id: RwSignal<i64>,
    /// What the last save that said anything said about the fallback, and
    /// which fallback that was: under the field while it holds that one.
    notes: RwSignal<Option<Noted>>,
    /// A save's refusal of the fallback, under the field.
    error: RwSignal<Option<String>>,
    /// The field's value, once [`FallbackField`] is drawn: which fallback
    /// a save's notes speak of.
    field: StoredValue<Option<RwSignal<String>>>,
    modal: Option<lmgw_ui_kit::widgets::modal::ModalState>,
}

impl Saves {
    /// For a row of `id` (0: a create), with the notes a create handed it
    /// ([`Self::carry`]). Inside a modal, when there is one.
    pub(super) fn new(id: i64) -> Self {
        let carried = CARRIED.with_borrow_mut(|c| {
            c.take_if(|(row, _)| id > 0 && *row == id)
                .map(|(_, noted)| noted)
        });
        Self {
            id: RwSignal::new(id),
            notes: RwSignal::new(carried),
            error: RwSignal::new(None),
            field: StoredValue::new(None),
            modal: crate::widgets::use_modal(),
        }
    }

    /// The fallback the field holds now, trimmed.
    fn value(&self) -> String {
        self.field
            .get_value()
            .map(|v| v.get_untracked().trim().to_string())
            .unwrap_or_default()
    }

    /// The notes for the fallback `value` holds, if the saves said any.
    fn notes_for(&self, value: &str) -> Vec<String> {
        self.notes.with(|n| match n {
            Some((of, notes)) if of == value.trim() => notes.clone(),
            _ => Vec::new(),
        })
    }

    /// Hand the notes on screen to the editor that opens the saved row next
    /// (a create that navigates to its row's page): it shows them as this
    /// one did, instead of losing them with this form.
    pub(super) fn carry(&self) {
        let id = self.id.get_untracked();
        if let Some(noted) = self.notes.get_untracked().filter(|_| id > 0) {
            CARRIED.with_borrow_mut(|c| *c = Some((id, noted)));
        }
    }

    /// Whether the next save creates the row.
    pub(super) fn creating(&self) -> bool {
        self.id.get_untracked() == 0
    }

    /// The save body's `action` and `id`.
    pub(super) fn target(&self) -> (&'static str, Value) {
        match self.id.get_untracked() {
            0 => ("create", Value::Null),
            id => ("update", json!(id)),
        }
    }

    /// The row's id, once saved.
    pub(super) fn id(&self) -> i64 {
        self.id.get_untracked()
    }

    /// A save went through as row `id`, saying `notes`: the next save is an
    /// update; its notes show for the fallback it saved, and the ones kept
    /// stay when it said none (the server says them when the fallback
    /// changes, and they hold for as long as that fallback is the one —
    /// picked away and back without a save between, too). Whether the form
    /// may close: nothing left to read for the fallback it saved. When it
    /// stays open the modal is told the form is saved, so closing it does
    /// not ask about changes.
    pub(super) fn answered(&self, id: i64, notes: Vec<String>) -> bool {
        if id > 0 {
            self.id.set(id);
        }
        self.error.set(None);
        let value = self.value();
        if !notes.is_empty() {
            self.notes.set(Some((value.clone(), notes)));
        }
        let close = untrack(|| self.notes_for(&value)).is_empty();
        if !close {
            if let Some(m) = self.modal {
                m.saved();
            }
        }
        close
    }

    /// A save was refused with `e`: a refusal of the fallback (the save
    /// checks' words start "fallback") shows under the field and is taken
    /// here (`true`); any other is the caller's to show.
    pub(super) fn refused(&self, e: &str) -> bool {
        let of_fallback = e.starts_with("fallback '") || e.starts_with("fallback:");
        if of_fallback {
            self.error.set(Some(e.to_string()));
        }
        of_fallback
    }
}

/// Every model's and alias's configured fallback by the name a client
/// reaches it by, for the chain line.
fn fallbacks_of(full: &ModelsFull) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut put = |name: &str, f: &Option<String>| {
        if let Some(f) = f.as_ref().filter(|f| !f.trim().is_empty()) {
            out.insert(name.to_string(), f.clone());
        }
    };
    for a in &full.aliases {
        put(&a.alias, &a.fallback);
    }
    for c in &full.candidate_aliases {
        put(&c.alias, &c.fallback);
    }
    for m in &full.local {
        put(&m.public_name, &m.model.fallback);
    }
    for m in &full.aux {
        put(&m.public_name, &m.model.fallback);
    }
    for m in &full.audio {
        put(&m.public_name, &m.model.fallback);
    }
    for m in &full.image {
        put(&m.public_name, &m.model.fallback);
    }
    out
}

/// The candidate aliases' names, lowercased (they are looked up so): none
/// may be a fallback yet.
fn candidates_of(full: &ModelsFull) -> HashSet<String> {
    full.candidate_aliases
        .iter()
        .map(|c| c.alias.to_lowercase())
        .collect()
}

/// Where `first` leads, as the line under the picker says it: each hop's
/// own fallback in turn, ending at "none" or at the hop that comes back
/// ("loops"). Static: which hop answers a given request is the gate's.
fn chain_line(map: &HashMap<String, String>, own: &str, first: &str) -> String {
    let mut seen = vec![own.to_string(), first.to_string()];
    let mut line = format!("→ {first}");
    let mut cur = first.to_string();
    loop {
        match map.get(&cur) {
            None => {
                line.push_str(" → none");
                return line;
            }
            Some(next) if seen.contains(next) => {
                line.push_str(&format!(" → {next} (a loop: the save refuses it)"));
                return line;
            }
            Some(next) => {
                line.push_str(&format!(" → {next}"));
                seen.push(next.clone());
                cur = next.clone();
            }
        }
    }
}

/// The Fallback field. `tasks` is the owner's type ([`same_type_tasks`]);
/// `own_name` is the owner's public name, which is not offered; `saves` the
/// editor's save state ([`Saves`]), whose notes and refusal show here;
/// `hint` names the reasons that can apply to this kind of owner. A change
/// of the value takes the notes and the refusal off: they spoke of the
/// fallback that was there.
#[component]
pub(super) fn FallbackField(
    value: RwSignal<String>,
    #[prop(into)] tasks: Signal<&'static [&'static str]>,
    #[prop(into)] own_name: Signal<String>,
    saves: Saves,
    hint: &'static str,
    #[prop(optional, into)] dirty: Option<Signal<bool>>,
) -> impl IntoView {
    let full = LocalResource::new(|| crate::api::get::<ModelsFull>("/api/models/full"));
    let candidates = Memo::new(move |_| {
        full.get()
            .and_then(|r| r.ok())
            .map(|f| candidates_of(&f))
            .unwrap_or_default()
    });
    saves.field.set_value(Some(value));
    // A refusal spoke of the fallback that was there; the notes are kept
    // with the fallback they speak of, and show again when it is picked
    // back.
    Effect::new(move |prev: Option<String>| {
        let now = value.get();
        if prev.is_some_and(|p| p.trim() != now.trim()) {
            saves.error.set(None);
        }
        now
    });
    let chain = move || {
        let v = value.get();
        let v = v.trim();
        if v.is_empty() {
            return "No fallback: this model answers, or the request gets the error it gives."
                .to_string();
        }
        match full.get().and_then(|r| r.ok()) {
            Some(full) => chain_line(&fallbacks_of(&full), &own_name.get(), v),
            None => format!("→ {v}"),
        }
    };
    let is_dirty = move || dirty.is_some_and(|d| d.get());
    view! {
        <div class="field wide fallback-field" class:dirty=is_dirty>
            <label>"Fallback"</label>
            {move || {
                // Rebuilt when the type changes, not on every keystroke in
                // the owner's name: the name is read when an entry is judged.
                let tasks = tasks.get();
                view! {
                    <ModelPicker
                        value=value
                        tasks=tasks
                        empty_label="None"
                        allow_custom=true
                        recent_key="fallback"
                        disallow=(
                            Callback::new(move |e: CatalogEntry| {
                                e.id == own_name.get_untracked().as_str()
                                    || candidates.with_untracked(|c| c.contains(&e.id.to_lowercase()))
                                    || (!tasks.is_empty()
                                        && e.task.as_deref().is_some_and(|t| !tasks.contains(&t)))
                            }),
                            "a fallback is a model of the same type, not this one, and not a \
                             candidate alias (which picks a model per request)",
                        )
                    />
                }
            }}
            {move || {
                saves
                    .error
                    .get()
                    .map(|e| view! { <div class="field-err" role="alert">{e}</div> })
            }}
            <div class="fallback-chain mono-sm">{chain}</div>
            {move || {
                saves
                    .notes_for(&value.get())
                    .into_iter()
                    .map(|n| view! { <div class="field-warn" role="note">{n}</div> })
                    .collect_view()
            }}
            <div class="field-hint">{hint}</div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chain_line_follows_each_hops_own_fallback_to_none_or_a_loop() {
        let map: HashMap<String, String> = [("b", "c"), ("c", "b")]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        assert_eq!(chain_line(&map, "a", "x"), "→ x → none");
        assert_eq!(
            chain_line(&map, "a", "b"),
            "→ b → c → b (a loop: the save refuses it)"
        );
        let back: HashMap<String, String> = [("b".to_string(), "a".to_string())].into();
        assert!(chain_line(&back, "a", "b").ends_with("→ a (a loop: the save refuses it)"));
    }

    /// Every task the gateway publishes is a type of its own (review UI3):
    /// an audio row of any task, a realtime model, narrow the picker.
    #[test]
    fn a_type_offers_its_own_tasks() {
        assert_eq!(same_type_tasks("embed"), &["embedding"]);
        assert_eq!(same_type_tasks("asr"), &["asr"]);
        assert_eq!(
            same_type_tasks("image_edit"),
            same_type_tasks("image_generation")
        );
        for task in [
            "gen", "clon", "vc", "svc", "s2s", "sep", "vad", "diar", "spk", "midi", "realtime",
        ] {
            assert_eq!(same_type_tasks(task), &[task], "{task}");
        }
        assert!(same_type_tasks("").is_empty());
        assert!(same_type_tasks("a-task-from-a-newer-build").is_empty());
    }

    /// An alias whose upstream catalog cannot be read still narrows the
    /// picker, by its own override or by what the gateway publishes for it
    /// (review UI3).
    #[test]
    fn an_alias_is_typed_by_its_override_its_catalog_or_the_gateway() {
        let over = r#"{"capabilities": {"task": "embedding"}}"#;
        assert_eq!(alias_task(over, Some("chat".into()), None), "embedding");
        assert_eq!(
            alias_task("", Some("tts".into()), Some("chat".into())),
            "tts"
        );
        assert_eq!(alias_task("not json", None, Some("chat".into())), "chat");
        assert_eq!(alias_task("", None, None), "");
    }

    /// "None" always names the clear, so a fallback an earlier save of the
    /// same open form set is removed too (review UI1).
    #[test]
    fn none_always_names_the_clear() {
        let owner = Owner::new();
        owner.with(|| {
            let mut clear = Vec::new();
            assert_eq!(
                fallback_arg(&mut clear, RwSignal::new(" ".into())),
                Value::Null
            );
            assert_eq!(clear, ["fallback"]);
            let mut clear = Vec::new();
            assert_eq!(
                fallback_arg(&mut clear, RwSignal::new(" cloud ".into())),
                Value::from("cloud")
            );
            assert!(clear.is_empty());
        });
    }

    /// A second save is an update of the row the first one created, the
    /// notes stay while the fallback does, and a refusal of the fallback is
    /// the field's (review C-a, UI6).
    #[test]
    fn saves_remember_the_row_and_keep_the_notes() {
        let owner = Owner::new();
        owner.with(|| {
            let s = Saves::new(0);
            s.field.set_value(Some(RwSignal::new("cloud".to_string())));
            assert_eq!(s.target(), ("create", Value::Null));
            assert!(s.answered(7, Vec::new()), "nothing to read: it closes");
            assert_eq!(s.target(), ("update", json!(7)));
            assert!(!s.answered(7, vec!["shares the upstream".into()]));
            assert!(!s.answered(7, Vec::new()), "the note still holds");
            assert_eq!(untrack(|| s.notes_for("cloud")), ["shares the upstream"]);
            assert!(s.refused("fallback 'x' does not resolve: …"));
            assert!(!s.refused("upstream_model must not be empty"));
        });
    }

    /// A fallback picked away and back without a save between shows its
    /// notes again, and a save that leaves it unchanged — the server says
    /// nothing then — keeps them (review N5).
    #[test]
    fn notes_come_back_with_the_fallback_they_speak_of() {
        let owner = Owner::new();
        owner.with(|| {
            let s = Saves::new(3);
            let field = RwSignal::new("x".to_string());
            s.field.set_value(Some(field));
            assert!(!s.answered(3, vec!["x shares the upstream".into()]));
            field.set("y".into());
            assert!(untrack(|| s.notes_for("y")).is_empty(), "not y's");
            field.set("x".into());
            assert!(!s.answered(3, Vec::new()), "x's notes hold: it stays open");
            assert_eq!(untrack(|| s.notes_for(" x ")), ["x shares the upstream"]);
        });
    }

    /// A create that opens its row's page hands its notes to that page's
    /// editor, once (review N4).
    #[test]
    fn a_create_hands_its_notes_to_the_rows_page() {
        let owner = Owner::new();
        owner.with(|| {
            let s = Saves::new(0);
            s.field.set_value(Some(RwSignal::new("x".to_string())));
            assert!(!s.answered(9, vec!["x is disabled".into()]));
            s.carry();
            assert!(
                Saves::new(8).notes.get_untracked().is_none(),
                "another row's page"
            );
            let page = Saves::new(9);
            assert_eq!(untrack(|| page.notes_for("x")), ["x is disabled"]);
            assert!(Saves::new(9).notes.get_untracked().is_none(), "taken once");
        });
    }
}
