//! "Fill from plan" in the aux editor (decision-models design §3.4, §3.9):
//! what the planner reads from the GGUF's header, filled into the form, with
//! each value's reason and the plan's warnings shown beside it, as
//! `lmgw__local_model_plan target=aux` answers them. Nothing is saved.
//! The Hugging Face wizard shows the same card ([`shown`], [`PlanCard`]) for
//! the aux row it creates from a download's plan.
//!
//! A saved row is refilled from the plan's own comparison with it
//! (`configured_as[].change`), not from the raw `params`: that comparison
//! keeps a causal decision row's own tuning (a context at or below the
//! plan's, a ubatch or batch at or above it), counts a projector the row's
//! args already name, and never touches placement.

use leptos::prelude::*;
use lmgw_api_types::PlanResult;
use serde_json::Value;

use super::aux_fields::AuxFields;
use crate::widgets::Toasts;

/// The fields a plan fills, in the order the editor shows them, with the
/// editor's label for each.
const FIELDS: [(&str, &str); 7] = [
    ("kind", "Served as"),
    ("pooling", "Pooling"),
    ("ctx_size", "Context size"),
    ("ubatch_size", "Ubatch size"),
    ("batch_size", "Batch size"),
    ("mmproj_path", "Projector"),
    ("backend", "Runs on"),
];

/// Which row a plan fills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::pages) enum Filling {
    /// A row being created: every planned value.
    New,
    /// The saved row `id`. On its own GGUF (`same_file`) it takes what the
    /// plan's comparison with it lists in `change`; pointed at another file
    /// in the form, the comparison is not about it, so it takes every planned
    /// value but where it runs.
    Saved { id: i64, same_file: bool },
}

/// Why a planned value was not filled in.
const KEPT_PLACEMENT: &str = "where a saved row runs stays your choice";
const KEPT_IN_LINE: &str = "the saved row is in line with the plan here";

/// One planned value as the plan card shows it.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::pages) struct PlannedField {
    pub key: &'static str,
    pub label: &'static str,
    /// As the form's input takes it.
    pub value: String,
    pub why: String,
    /// Why it was not filled in; `None` when it was.
    pub kept: Option<&'static str>,
}

/// A plan as the editor shows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(in crate::pages) struct ShownPlan {
    pub fields: Vec<PlannedField>,
    pub warnings: Vec<String>,
}

/// `plan` as it fills the row `filling` names.
pub(in crate::pages) fn shown(plan: &PlanResult, filling: Filling) -> ShownPlan {
    // The saved row's own comparison, when the plan made one for it (it
    // makes none for a file found outside the aux models dir).
    let changed: Option<Vec<&str>> = match filling {
        Filling::Saved {
            id,
            same_file: true,
        } => plan
            .configured_as
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.change.iter().map(|ch| ch.field.as_str()).collect()),
        _ => None,
    };
    let fields = FIELDS
        .iter()
        .filter_map(|&(key, label)| {
            let value = match plan.params.get(key)? {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            let kept = match (filling, &changed) {
                (Filling::New, _) => None,
                (Filling::Saved { .. }, _) if key == "backend" => Some(KEPT_PLACEMENT),
                (Filling::Saved { .. }, Some(changed)) if !changed.contains(&key) => {
                    Some(KEPT_IN_LINE)
                }
                (Filling::Saved { .. }, _) => None,
            };
            Some(PlannedField {
                key,
                label,
                value,
                why: plan
                    .rationale
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                kept,
            })
        })
        .collect();
    ShownPlan {
        fields,
        warnings: plan.warnings.clone(),
    }
}

/// The form's signals a plan fills.
#[derive(Clone, Copy)]
pub(super) struct PlanTargets {
    pub model_id: RwSignal<String>,
    pub kind: RwSignal<String>,
    pub pooling: RwSignal<String>,
    pub ctx_size: RwSignal<String>,
    pub fields: AuxFields,
}

impl PlanTargets {
    /// Every filled value of `shown` into its field, and the plan's model
    /// id into a blank one. A field kept, or one the plan proposes nothing
    /// for, holds what it holds.
    fn fill(&self, plan: &PlanResult, shown: &ShownPlan) {
        for f in shown.fields.iter().filter(|f| f.kept.is_none()) {
            let target = match f.key {
                "kind" => self.kind,
                "pooling" => self.pooling,
                "ctx_size" => self.ctx_size,
                "ubatch_size" => self.fields.ubatch_size,
                "batch_size" => self.fields.batch_size,
                "mmproj_path" => self.fields.mmproj_path,
                "backend" => self.fields.backend,
                _ => continue,
            };
            target.set(f.value.clone());
        }
        if self.model_id.with_untracked(|m| m.trim().is_empty()) {
            self.model_id.set(plan.model_id.clone());
        }
    }
}

/// Read the plan for `gguf_path` (relative to the aux models dir) and fill
/// the form from it; `shown` gets what the plan card shows.
pub(super) fn fill_from_plan(
    gguf_path: String,
    filling: Filling,
    targets: PlanTargets,
    shown_plan: RwSignal<Option<ShownPlan>>,
    planning: RwSignal<bool>,
    toasts: Toasts,
) {
    let path = gguf_path.trim().to_string();
    if path.is_empty() {
        toasts.err("name the GGUF first: the plan reads its header");
        return;
    }
    if planning.get_untracked() {
        return;
    }
    planning.set(true);
    leptos::task::spawn_local(async move {
        let url = format!(
            "/api/local-model-plan?target=aux&path={}",
            String::from(js_sys::encode_uri_component(&path))
        );
        let res = crate::api::get::<PlanResult>(url).await;
        planning.set(false);
        match res {
            Ok(plan) => {
                let s = shown(&plan, filling);
                targets.fill(&plan, &s);
                let filled = s.fields.iter().filter(|f| f.kept.is_none()).count();
                shown_plan.set(Some(s));
                toasts.ok(match filled {
                    0 => "the saved row is in line with the plan — nothing to fill".to_string(),
                    n => format!("{n} field(s) filled from the plan — review and save"),
                });
            }
            Err(e) => toasts.err(e.to_string()),
        }
    });
}

/// The plan card: its warnings, then each value with its reason.
#[component]
pub(in crate::pages) fn PlanCard(plan: RwSignal<Option<ShownPlan>>) -> impl IntoView {
    move || {
        plan.get().map(|p| {
            let warnings = (!p.warnings.is_empty()).then(|| {
                view! {
                    <div class="notice warn">
                        <b>"Plan warnings"</b>
                        {p.warnings
                            .iter()
                            .map(|w| view! { <span class="detail">{w.clone()}</span> })
                            .collect_view()}
                    </div>
                }
            });
            let fields = p
                .fields
                .iter()
                .map(|f| {
                    let value = match (f.key, f.value.as_str()) {
                        ("backend", "cpu") => "CPU".to_string(),
                        (_, "") => "none".to_string(),
                        (_, v) => v.to_string(),
                    };
                    let kept = f
                        .kept
                        .map(|why| view! { <span class="dim">{format!(" · kept: {why}")}</span> });
                    view! {
                        <div class="field">
                            <label>{f.label} " " <span class="mono-sm">{value}</span> {kept}</label>
                            <p class="dim mini-note">{f.why.clone()}</p>
                        </div>
                    }
                })
                .collect_view();
            view! {
                <section class="card edit-section">
                    <h3>"Plan " <span class="dim cmd-note">"read from the GGUF's header"</span></h3>
                    {warnings}
                    {fields}
                </section>
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn laya() -> PlanResult {
        serde_json::from_value(json!({
            "model_id": "laya-q8_0",
            "params": {
                "gguf_path": "Laya-Q8_0.gguf",
                "kind": "decision",
                "ctx_size": 8192,
                "ubatch_size": 8192,
                "batch_size": 8192,
                "backend": "cpu",
            },
            "rationale": {
                "kind": "the header declares decision.type 'laya'",
                "ctx_size": "the model's trained context (8192)",
                "ubatch_size": "= ctx_size",
                "batch_size": "= ubatch_size",
                "backend": "the CPU",
            },
            "warnings": ["no systemone template"],
            "configured_as": [],
            "next_step": "",
        }))
        .unwrap()
    }

    /// A Kev plan as the gateway answers it for a configured row: 2048 and
    /// the trained context proposed, and the row's comparison listing only
    /// `changed` (decision-models design §3.4, `decision::within_tuning`).
    fn kev(changed: &[&str]) -> PlanResult {
        let change: Vec<Value> = changed
            .iter()
            .map(|f| json!({"field": f, "current": 1024, "planned": 2048, "why": "the 2048"}))
            .collect();
        serde_json::from_value(json!({
            "model_id": "kev-4b",
            "params": {
                "gguf_path": "Kev-4B.gguf",
                "kind": "decision",
                "ctx_size": 262144,
                "ubatch_size": 2048,
                "batch_size": 2048,
                "mmproj_path": "mmproj-Kev-4B.gguf",
            },
            "rationale": {},
            "warnings": [],
            "configured_as": [{
                "model_id": "kev", "id": 3, "enabled": true,
                "in_sync": change.is_empty(), "change": change,
            }],
            "next_step": "",
        }))
        .unwrap()
    }

    fn targets(ctx: &str, ubatch: &str, args_projector: bool) -> PlanTargets {
        let m: lmgw_api_types::AuxModel = serde_json::from_value(json!({
            "id": 3, "model_id": "kev", "gguf_path": "Kev-4B.gguf", "kind": "decision",
            "ctx_size": ctx.parse::<i64>().ok(), "ubatch_size": ubatch.parse::<i64>().ok(),
            "batch_size": ubatch.parse::<i64>().ok(), "idle_seconds": 300, "enabled": true,
            "args": if args_projector { json!(["--mmproj", "/models/mmproj-Kev-4B.gguf"]) } else { json!([]) },
        }))
        .unwrap();
        PlanTargets {
            model_id: RwSignal::new(m.model_id.clone()),
            kind: RwSignal::new(m.kind.clone()),
            pooling: RwSignal::new(String::new()),
            ctx_size: RwSignal::new(ctx.into()),
            fields: AuxFields::new(&m),
        }
    }

    #[test]
    fn a_plan_shows_its_fields_in_the_editor_s_order_with_their_reasons() {
        let p = shown(&laya(), Filling::New);
        assert_eq!(
            p.fields.iter().map(|f| f.key).collect::<Vec<_>>(),
            ["kind", "ctx_size", "ubatch_size", "batch_size", "backend"],
            "gguf_path is the field the plan was read from, not one it fills"
        );
        assert_eq!(p.fields[1].value, "8192");
        assert_eq!(p.fields[1].why, "the model's trained context (8192)");
        assert_eq!(p.fields[4].value, "cpu");
        assert!(p.fields.iter().all(|f| f.kept.is_none()));
        assert_eq!(p.warnings, ["no systemone template"]);
    }

    /// The review's case: a causal decision row tuned to ctx 4096 and
    /// ub/b 4096 is in line with the plan (its comparison lists nothing), so
    /// a fill keeps all three, and its projector named in args is not filled
    /// into the field over the flag.
    #[test]
    fn a_tuned_causal_row_keeps_its_tuning_and_its_args_projector() {
        let owner = Owner::new();
        owner.with(|| {
            let t = targets("4096", "4096", true);
            let plan = kev(&[]);
            let s = shown(
                &plan,
                Filling::Saved {
                    id: 3,
                    same_file: true,
                },
            );
            assert!(
                s.fields.iter().all(|f| f.kept == Some(KEPT_IN_LINE)),
                "{s:?}"
            );
            t.fill(&plan, &s);
            assert_eq!(t.ctx_size.get_untracked(), "4096");
            assert_eq!(t.fields.ubatch_size.get_untracked(), "4096");
            assert_eq!(t.fields.batch_size.get_untracked(), "4096");
            assert_eq!(t.fields.mmproj_path.get_untracked(), "");
        });
    }

    #[test]
    fn a_saved_row_takes_what_its_comparison_lists_and_keeps_where_it_runs() {
        let owner = Owner::new();
        owner.with(|| {
            let t = targets("65536", "1024", false);
            let plan = kev(&["batch_size", "ubatch_size"]);
            let s = shown(
                &plan,
                Filling::Saved {
                    id: 3,
                    same_file: true,
                },
            );
            t.fill(&plan, &s);
            assert_eq!(t.fields.ubatch_size.get_untracked(), "2048");
            assert_eq!(t.fields.batch_size.get_untracked(), "2048");
            assert_eq!(
                t.ctx_size.get_untracked(),
                "65536",
                "a smaller context is kept"
            );

            let laya_plan = laya();
            let s = shown(
                &laya_plan,
                Filling::Saved {
                    id: 3,
                    same_file: false,
                },
            );
            let backend = s.fields.iter().find(|f| f.key == "backend").unwrap();
            assert_eq!(backend.kept, Some(KEPT_PLACEMENT));
            assert!(s
                .fields
                .iter()
                .filter(|f| f.key != "backend")
                .all(|f| f.kept.is_none()));
            t.fill(&laya_plan, &s);
            assert_eq!(
                t.ctx_size.get_untracked(),
                "8192",
                "another file: its plan, whole"
            );
            assert_eq!(
                t.fields.backend.get_untracked(),
                "",
                "a saved row stays on the GPU"
            );
        });
    }

    #[test]
    fn a_new_row_takes_the_whole_plan_and_its_model_id() {
        let owner = Owner::new();
        owner.with(|| {
            let t = targets("", "", false);
            t.model_id.set(String::new());
            let plan = laya();
            t.fill(&plan, &shown(&plan, Filling::New));
            assert_eq!(t.kind.get_untracked(), "decision");
            assert_eq!(t.fields.backend.get_untracked(), "cpu");
            assert_eq!(t.fields.ubatch_size.get_untracked(), "8192");
            assert_eq!(t.model_id.get_untracked(), "laya-q8_0");
            t.model_id.set("laya".into());
            t.fill(&plan, &shown(&plan, Filling::New));
            assert_eq!(
                t.model_id.get_untracked(),
                "laya",
                "a named row keeps its name"
            );
        });
    }
}
