//! The candidate-alias editor (candidate-aliases design §4.1, §4.6, §6): a
//! second family of alias alongside the plain cloud alias
//! (`model_editors.rs`'s `AliasEditor`) — a client-facing name backed by a
//! primary local chat model plus alternates, used only when already loaded,
//! one fallback and five capability toggles. Its own file (not
//! `model_editors.rs`, already 2500+ lines, and not `models.rs`) per the
//! modularisation note; those two files get only the glue that wires this
//! in (a `mod` line, the "Candidate aliases" group, the "Add candidate
//! alias" button, the editor's mount point, the State-cell branch).
//!
//! **Unsaved-draft derivation.** `candidate_alias_set action=preview`
//! (`ops::candidate_alias::preview`, candidates design §6) runs the same
//! validation and derivation a real save would, over whatever is in this
//! form right now, without writing — so `common_facets`/`unsupported_by`/
//! `problems`/`fallback_usable` are always the server's own answer, never a
//! client-side reimplementation of the positive-support rule (which has a
//! genuine subtlety: `reasoning`'s "toggle/levels, or fixed+enabled" rule,
//! `candidates::facets::supports`'s own doc comment). Debounced a beat after
//! the last edit (`debounce`, below) so a pause in typing costs one request,
//! not one per keystroke — the same idea `local_edit.rs`'s `debounce_key`
//! uses for the ladder rung plan. Its `addable` field (every enabled local
//! chat model not yet a candidate, and which enabled facet each would drop)
//! is the "add candidate" list's filter, computed once server-side instead
//! of one request per row a client-only picker would otherwise cost.
//!
//! **Reordering.** Primary and alternates are two visually distinct slots
//! (the primary is a fixed single slot; "make primary" promotes an
//! alternate into it) — only the alternates reorder among themselves, by
//! native HTML5 drag-and-drop (a `dragstart`/`dragover`/`drop` cycle
//! tracking the dragged index in a plain signal, no `DataTransfer` payload
//! needed since source and target are the same page) with up/down buttons
//! as the always-present keyboard/no-drag path — nothing else in this
//! codebase reorders a list, so both are new here.

use std::collections::HashSet;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{CandidateAliasView, ModelsFull};
use serde_json::{json, Value};

use crate::catalog::CatalogEntry;
use crate::widgets::{use_toasts, Modal, ModalFooter, ModalSize, ModelPicker, Select};

use super::model_editors::{hold_body, hold_signals};

/// The five capability facets (candidates design §4.6, fact 13) — wire name
/// and editor label. Duplicated from `lmgw_core::candidates::facets::Facet`
/// on purpose: this WASM crate cannot depend on lmgw-core (that is the
/// server binary's crate), and this is the one place the UI needs the
/// vocabulary — same five, same order, as `/v1/models` and the MCP tool.
const FACETS: [(&str, &str); 5] = [
    ("vision", "Vision (image input)"),
    ("audio", "Audio input"),
    ("tool_calls", "Tool calls"),
    ("reasoning", "Reasoning"),
    ("structured_output", "Structured output"),
];

/// A blank candidate alias (id 0) opens the editor in "create" mode — the
/// defaults `ops::candidate_alias_set` applies to a create that leaves them
/// out (background off, fallback inherits the global hold fallback,
/// enabled). `capabilities_disabled` stays empty here on purpose: the save
/// omits it entirely until the owner touches a toggle, so a fresh alias with
/// nothing chosen yet still saves with every common facet on, the same
/// lenient default a bare MCP `create` gets (§12 entry 49).
pub(super) fn blank_candidate_alias() -> CandidateAliasView {
    CandidateAliasView {
        enabled: true,
        fallback_mode: "inherit".into(),
        ..Default::default()
    }
}

/// The Models page's State cell for a candidate alias (`models.rs`'s
/// `ModelRow`, the branch for a class with no container of its own): there
/// is no runtime to show, so this shows what its own derivation last found
/// instead — a problem count, and `gpu_deferred` deferrals over the last
/// 24 h (§6, counted from the request log).
#[component]
pub(super) fn CandidateStateCell(problems: usize, deferrals_24h: Option<u64>) -> impl IntoView {
    let cls = if problems > 0 { "chip warn" } else { "chip ok" };
    let label = if problems > 0 {
        format!("{problems} problem{}", if problems == 1 { "" } else { "s" })
    } else {
        "ok".to_string()
    };
    view! {
        <span class=cls>
            <span class="dot"></span>
            {label}
        </span>
        {deferrals_24h
            .filter(|n| *n > 0)
            .map(|n| {
                view! {
                    <span
                        class="type-badge"
                        style="margin-left:4px"
                        title="gpu_deferred refusals in the last 24 hours: the alias's own fallback answered instead of waiting for the GPU"
                    >
                        {format!("{n} deferred/24h")}
                    </span>
                }
            })}
    }
}

/// Settle a fast-changing value a beat after it stops changing — the same
/// one-request-per-pause idea `local_edit.rs`'s `debounce_key` uses for the
/// ladder rung plan, generalised past `Option<String>` since the preview's
/// draft key is a whole JSON body.
fn debounce<T: Clone + PartialEq + Send + Sync + 'static>(key: Memo<T>) -> Signal<T> {
    let out = RwSignal::new(key.get_untracked());
    let gen = StoredValue::new(0u64);
    Effect::new(move |prev: Option<()>| {
        let k = key.get();
        if prev.is_none() {
            out.set(k);
            return;
        }
        gen.update_value(|g| *g += 1);
        let mine = gen.get_value();
        set_timeout(
            move || {
                if gen.try_get_value() == Some(mine) {
                    out.set(k);
                }
            },
            Duration::from_millis(250),
        );
    });
    out.into()
}

#[component]
pub fn CandidateAliasEditor(
    editing: RwSignal<Option<CandidateAliasView>>,
    /// `/api/models/full`, already held by the Models page — the local chat
    /// rows the "add candidate" and fallback pickers offer, and every other
    /// candidate alias's name (the fallback's "must not be another candidate
    /// alias" rule, checked here only for the picker's grey-out; the save
    /// itself is refused server-side regardless).
    full: RwSignal<Option<ModelsFull>>,
    on_saved: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = editing.get().is_some();
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && editing.get_untracked().is_some() {
            editing.set(None);
        }
    });
    view! {
        <Modal open=open title="Candidate alias" size=ModalSize::Wide guard=true>
            {move || {
                editing
                    .get()
                    .map(|c| {
                        view! { <CandidateAliasForm c=c full=full open=open on_saved=on_saved/> }
                            .into_any()
                    })
            }}
        </Modal>
    }
}

#[component]
fn CandidateAliasForm(
    c: CandidateAliasView,
    full: RwSignal<Option<ModelsFull>>,
    open: RwSignal<bool>,
    on_saved: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let toasts = use_toasts();
    let create = c.id == 0;
    let id = c.id;
    let alias = RwSignal::new(c.alias.clone());
    let candidates = RwSignal::new(c.candidates.clone());
    let background = RwSignal::new(c.background);
    let (fallback_mode, fallback_alias) = hold_signals(&c.fallback_mode, &c.fallback);
    // `None` only for a brand-new draft that has never had a stored opinion
    // — the save then omits `capabilities_disabled` entirely, same
    // leniency a bare `create` gets over MCP (§12 entry 49's `Option`
    // split). Editing an existing row always starts `Some`, carrying its
    // stored value forward, exactly like `update`'s own convention.
    let disabled_facets: RwSignal<Option<HashSet<String>>> = RwSignal::new(if create {
        None
    } else {
        Some(c.capabilities_disabled.iter().cloned().collect())
    });
    let notes = RwSignal::new(c.notes.clone());
    let saving = RwSignal::new(false);
    let save_error = RwSignal::new(None::<String>);
    let dragging = RwSignal::new(None::<usize>);

    let local = Signal::derive(move || {
        full.with(|f| f.as_ref().map(|f| f.local.clone()).unwrap_or_default())
    });
    let candidate_names = Signal::derive(move || {
        full.with(|f| {
            f.as_ref()
                .map(|f| {
                    f.candidate_aliases
                        .iter()
                        .map(|c| c.alias.to_lowercase())
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default()
        })
    });
    let name_of = move |id: &str| {
        local.with(|rows| {
            rows.iter()
                .find(|r| r.model.model_id == id)
                .map(|r| r.public_name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| id.to_string())
        })
    };

    // --- Live preview (§6): re-derived a beat after every edit ----------
    let draft = Memo::new(move |_| {
        json!({
            "action": "preview",
            "id": if create { Value::Null } else { json!(id) },
            "alias": alias.get().trim(),
            "candidates": candidates.get().join(","),
            "background": background.get(),
            "fallback_mode": fallback_mode.get(),
            "fallback": fallback_alias.get().trim(),
            "capabilities_disabled": disabled_facets.get().map(|s| {
                let mut v: Vec<String> = s.into_iter().collect();
                v.sort();
                v.join(",")
            }),
        })
    });
    let draft = debounce(draft);
    let preview = LocalResource::new(move || {
        let body = draft.get();
        async move {
            crate::api::post::<Value, _>("/api/op/candidate_alias_set", &body)
                .await
                .ok()
        }
    });
    let field = move |name: &'static str| -> Value {
        preview
            .get()
            .flatten()
            .and_then(|v| v.get(name).cloned())
            .unwrap_or(Value::Null)
    };
    let str_list = move |name: &'static str| -> Vec<String> {
        field(name)
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let common_facets = move || str_list("common_facets");
    let problems = move || str_list("problems");
    let advisories = move || str_list("advisories");
    let preview_error = move || field("error").as_str().map(str::to_string);
    let fallback_usable = move || field("fallback_usable").as_bool().unwrap_or(false);
    let unsupported_by = move |facet: &str| -> Vec<String> {
        field("unsupported_by")
            .get(facet)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    // Every enabled local chat model not already a candidate, and which of
    // it supports every facet the draft currently enables — "listing only
    // enabled local chat models that support every enabled facet" (§6),
    // computed once server-side (`ops::candidate_alias::preview`'s
    // `addable`) instead of one request per row a client-only picker would
    // otherwise cost.
    let addable = move || -> Vec<String> {
        field("addable")
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|e| {
                        e.get("missing")
                            .and_then(Value::as_array)
                            .is_none_or(|m| m.is_empty())
                    })
                    .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let excluded_count = move || {
        field("addable")
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|e| {
                        e.get("missing")
                            .and_then(Value::as_array)
                            .is_some_and(|m| !m.is_empty())
                    })
                    .count()
            })
            .unwrap_or(0)
    };

    // --- Candidate list edits --------------------------------------------
    let add_pick = RwSignal::new(String::new());
    Effect::new(move |_| {
        let picked = add_pick.get();
        if picked.is_empty() {
            return;
        }
        candidates.update(|v| v.push(picked));
        add_pick.set(String::new());
    });
    let add_options = Signal::derive(move || {
        addable()
            .into_iter()
            .map(|id| {
                let label = name_of(&id);
                (id, label)
            })
            .collect::<Vec<_>>()
    });

    let toggle_facet = move |name: &'static str| {
        let now_common = common_facets().iter().any(|f| f == name);
        if !now_common {
            return;
        }
        disabled_facets.update(|opt| {
            // First manual touch materialises `Some` as an *empty* set, not
            // today's non-common facets — the patch must carry only the
            // owner's own switches (never a greyed, unsupported facet the
            // owner never touched), so the server can tell a real switch
            // from a facet that is merely uncommon right now (§4.6 rule 2).
            let set = opt.get_or_insert_with(HashSet::new);
            if set.contains(name) {
                set.remove(name);
            } else {
                set.insert(name.to_string());
            }
        });
    };
    let is_checked = move |name: &str| {
        common_facets().iter().any(|f| f == name)
            && !disabled_facets.with(|d| d.as_ref().is_some_and(|d| d.contains(name)))
    };

    // Alternates are `candidates[1..]`; the primary is `candidates[0]`.
    // Every mutator below takes an alternate's own 0-based index among the
    // alternates, and adds 1 for the absolute position.
    let move_alt = move |i: usize, delta: isize| {
        candidates.update(|v| {
            let ai = i + 1;
            let target = ai as isize + delta;
            if target < 1 || (target as usize) >= v.len() {
                return;
            }
            v.swap(ai, target as usize);
        });
    };
    let make_primary = move |i: usize| {
        candidates.update(|v| {
            let ai = i + 1;
            if ai < v.len() {
                v.swap(0, ai);
            }
        });
    };
    let remove_at = move |ai: usize| {
        candidates.update(|v| {
            if ai < v.len() {
                v.remove(ai);
            }
        });
    };
    let reorder_alt = move |from: usize, to: usize| {
        candidates.update(|v| {
            let (fi, ti) = (from + 1, to + 1);
            if fi >= v.len() || ti >= v.len() || fi == ti {
                return;
            }
            let item = v.remove(fi);
            v.insert(ti, item);
        });
    };

    let fallback_mode_options: Signal<Vec<(String, String)>> = Signal::derive(|| {
        vec![
            (
                "inherit".to_string(),
                "Inherit — use the global hold fallback".to_string(),
            ),
            ("none".to_string(), "None — refuse with a 503".to_string()),
            ("alias".to_string(), "Route to another alias".to_string()),
        ]
    });
    let fallback_disallow = Callback::new(move |e: CatalogEntry| {
        e.local || candidate_names.get().contains(&e.id.to_lowercase())
    });

    // A plain binding rather than an inline closure in the `<For each=…>`
    // attribute below: a turbofish (`::<Vec<_>>`) inside a `view!` tag's
    // attribute value trips up its tag parser (the `<`/`>` read as markup),
    // so the collection is built here where it is an ordinary expression.
    let alt_items = move || -> Vec<(usize, String)> {
        candidates.get().into_iter().skip(1).enumerate().collect()
    };

    let save = move |_| {
        if saving.get_untracked() {
            return;
        }
        save_error.set(None);
        let cands = candidates.get_untracked();
        if cands.is_empty() {
            save_error.set(Some(
                "pick a primary model first — the first candidate is the primary".to_string(),
            ));
            return;
        }
        let (fb_mode, fb_alias) = match hold_body(fallback_mode, fallback_alias) {
            Ok(v) => v,
            Err(e) => {
                save_error.set(Some(e));
                return;
            }
        };
        let notes_v = notes.get_untracked();
        let mut clear: Vec<&str> = Vec::new();
        if notes_v.trim().is_empty() {
            clear.push("notes");
        }
        let args = json!({
            "action": if create { "create" } else { "update" },
            "id": if create { Value::Null } else { json!(id) },
            "alias": alias.get_untracked().trim(),
            "candidates": cands.join(","),
            "background": background.get_untracked(),
            "fallback_mode": fb_mode,
            "fallback": fb_alias,
            "capabilities_disabled": disabled_facets.get_untracked().map(|s| {
                let mut v: Vec<String> = s.into_iter().collect();
                v.sort();
                v.join(",")
            }),
            "notes": notes_v,
            "clear": clear.join(","),
        });
        saving.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>("/api/op/candidate_alias_set", &args).await;
            saving.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("candidate alias saved")
                        .to_string());
                    open.set(false);
                    on_saved();
                }
                Err(e) => save_error.set(Some(e.to_string())),
            }
        });
    };

    view! {
        <div class="card-flow editor-flow">
            <section class="card edit-section">
                <h3>"Route"</h3>
                <div class="field-grid">
                    <div class="field">
                        <label>"Alias" <span class="field-unit">"the name clients request"</span></label>
                        <input
                            class="input mono"
                            prop:value=move || alias.get()
                            on:input=move |ev| alias.set(event_target_value(&ev))
                        />
                    </div>
                    <label class="row" style="margin-top:10px">
                        <input
                            type="checkbox"
                            prop:checked=move || background.get()
                            on:change=move |ev| background.set(event_target_checked(&ev))
                        />
                        "background"
                    </label>
                </div>
                <p class="field-hint">
                    "A background job is a guest on the GPU: it never evicts or interrupts your \
                     own models, and never waits — it uses the primary only when it is already \
                     up or can start without disturbing you, else a loaded alternate, else the \
                     fallback."
                </p>

                <h3 style="margin-top:14px">"Primary"</h3>
                {move || match candidates.get().first().cloned() {
                    Some(p) => {
                        view! {
                            <div class="candidate-row candidate-primary">
                                <span class="type-badge">"primary"</span>
                                <span class="mono-sm">{name_of(&p)}</span>
                                <button
                                    type="button"
                                    class="btn ghost sm"
                                    disabled=move || candidates.with(Vec::len) < 2
                                    title=if candidates.get().len() < 2 {
                                        "add an alternate first, or delete the alias"
                                    } else {
                                        "remove the primary — the first alternate becomes primary"
                                    }
                                    on:click=move |_| remove_at(0)
                                >
                                    "Remove"
                                </button>
                            </div>
                        }
                            .into_any()
                    }
                    None => {
                        view! {
                            <p class="dim mini-note">
                                "None yet — pick a local chat model below; it becomes the primary."
                            </p>
                        }
                            .into_any()
                    }
                }}

                <h3 style="margin-top:14px">
                    "Alternates"
                    <span class="field-unit">"used only when already loaded"</span>
                </h3>
                <ul class="candidate-alt-list">
                    <For
                        each=alt_items
                        key=|(i, id)| (*i, id.clone())
                        let:item
                    >
                        {
                            let (i, cid) = item;
                            let n_alts = candidates.with(Vec::len).saturating_sub(1);
                            let is_first = i == 0;
                            let is_last = i + 1 >= n_alts;
                            view! {
                                <li
                                    class="candidate-alt-row"
                                    draggable="true"
                                    on:dragstart=move |_| dragging.set(Some(i))
                                    on:dragover=move |ev| ev.prevent_default()
                                    on:drop=move |ev| {
                                        ev.prevent_default();
                                        if let Some(from) = dragging.get_untracked() {
                                            reorder_alt(from, i);
                                        }
                                        dragging.set(None);
                                    }
                                    on:dragend=move |_| dragging.set(None)
                                >
                                    <span class="drag-handle" title="Drag to reorder">"⠿"</span>
                                    <span class="mono-sm">{name_of(&cid)}</span>
                                    <span class="candidate-alt-actions">
                                        <button
                                            type="button"
                                            class="btn ghost sm"
                                            disabled=is_first
                                            title="Move up"
                                            on:click=move |_| move_alt(i, -1)
                                        >
                                            "↑"
                                        </button>
                                        <button
                                            type="button"
                                            class="btn ghost sm"
                                            disabled=is_last
                                            title="Move down"
                                            on:click=move |_| move_alt(i, 1)
                                        >
                                            "↓"
                                        </button>
                                        <button
                                            type="button"
                                            class="btn ghost sm"
                                            on:click=move |_| make_primary(i)
                                        >
                                            "Make primary"
                                        </button>
                                        <button
                                            type="button"
                                            class="btn ghost sm"
                                            on:click=move |_| remove_at(i + 1)
                                        >
                                            "Remove"
                                        </button>
                                    </span>
                                </li>
                            }
                        }
                    </For>
                </ul>

                <div class="field wide" style="margin-top:6px">
                    <label>"Add candidate"</label>
                    <Select
                        value=add_pick
                        options=add_options
                        placeholder="pick a local chat model…"
                    />
                    <p class="field-hint">
                        {move || {
                            let n = excluded_count();
                            if n == 0 {
                                "Only enabled local chat models that support every facet this alias \
                                 enables are offered.".to_string()
                            } else {
                                format!(
                                    "Only enabled local chat models that support every facet this \
                                     alias enables are offered — {n} more do not and are left out.",
                                )
                            }
                        }}
                    </p>
                </div>
            </section>

            <section class="card edit-section">
                <h3>"Fallback"</h3>
                <div class="hold-pick">
                    <Select value=fallback_mode options=fallback_mode_options/>
                    <Show when=move || fallback_mode.get() == "alias">
                        <ModelPicker
                            value=fallback_alias
                            tasks=&["chat"]
                            recent_key="candidate-fallback"
                            disallow=(
                                fallback_disallow,
                                "must not need this GPU, and must not be another candidate alias",
                            )
                        />
                    </Show>
                </div>
                <p class="field-hint">
                    "Answers every request this alias cannot route locally right now: under the \
                     GPU hold, when VRAM this gateway cannot free is short, or — for a background \
                     job — whenever the primary cannot start without disturbing your own work."
                </p>
                {move || {
                    (fallback_mode.get() == "alias" && !fallback_alias.get().trim().is_empty()
                        && !fallback_usable())
                        .then(|| {
                            view! {
                                <p class="field-err" role="alert">
                                    "This fallback does not support every facet this alias enables \
                                     yet — see the problems below."
                                </p>
                            }
                        })
                }}
            </section>

            <section class="card edit-section">
                <h3>"Capabilities"</h3>
                <div class="field-grid">
                    <For each=move || FACETS.to_vec() key=|(n, _)| n.to_string() let:facet>
                        {
                            let (name, label) = facet;
                            let common = move || common_facets().iter().any(|f| f == name);
                            view! {
                                <label class="row" class:dim=move || !common()>
                                    <input
                                        type="checkbox"
                                        prop:checked=move || is_checked(name)
                                        disabled=move || !common()
                                        on:change=move |_| toggle_facet(name)
                                    />
                                    {label}
                                    {move || {
                                        let who = unsupported_by(name);
                                        (!common() && !who.is_empty())
                                            .then(|| {
                                                view! {
                                                    <span class="dim cell-note">
                                                        {format!("not supported by: {}", who.join(", "))}
                                                    </span>
                                                }
                                            })
                                    }}
                                </label>
                            }
                        }
                    </For>
                </div>
            </section>

            <section class="card edit-section">
                <h3>"Notes"</h3>
                <textarea
                    class="input ta"
                    prop:value=move || notes.get()
                    on:input=move |ev| notes.set(event_target_value(&ev))
                ></textarea>
            </section>

            {move || {
                let probs = problems();
                let advs = advisories();
                let err = preview_error();
                (err.is_some() || !probs.is_empty() || !advs.is_empty())
                    .then(|| {
                        view! {
                            <section class="card edit-section">
                                {err.map(|e| view! { <p class="field-err" role="alert">{e}</p> })}
                                {(!probs.is_empty())
                                    .then(|| {
                                        view! {
                                            <ul class="mini-note">
                                                <For each=move || problems() key=|p| p.clone() let:p>
                                                    <li class="field-warn">{p}</li>
                                                </For>
                                            </ul>
                                        }
                                    })}
                                {(!advs.is_empty())
                                    .then(|| {
                                        view! {
                                            <ul class="mini-note dim">
                                                <For each=move || advisories() key=|a| a.clone() let:a>
                                                    <li>{a}</li>
                                                </For>
                                            </ul>
                                        }
                                    })}
                            </section>
                        }
                    })
            }}
        </div>
        <ModalFooter>
            {move || {
                save_error
                    .get()
                    .map(|e| view! { <p class="field-err" role="alert">{e}</p> })
            }}
            <button class="btn ghost" on:click=move |_| open.set(false)>
                "Cancel"
            </button>
            <button class="btn primary" disabled=move || saving.get() on:click=save>
                {move || if saving.get() { "Saving…" } else { "Save candidate alias" }}
            </button>
        </ModalFooter>
    }
}
