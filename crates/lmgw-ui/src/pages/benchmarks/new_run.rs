//! The New benchmark modal (benchmark design §3.5, §8.2): a local chat row
//! (and its rung), an image (the row's, the class default, or any
//! llama-server image on this machine), the flat overrides with the row's own
//! value as each one's placeholder, the phases, repetitions and notes — and
//! beside them the plan `bench_plan` answers for exactly that, asked again a
//! moment after every change. Review lists what the start stops; Start does
//! it and opens the run.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::bench::Phase;
use lmgw_api_types::bench_ops::{BenchArgs, BenchPlan};
use lmgw_api_types::{LocalModel, ModelsFull, SettingsFull};

use super::plan_view::{Confirmation, PlanPreview};
use super::{phase_label, run_href, use_bn};
use crate::bench_api as api;
use crate::fmt::grouped;
use crate::scope::{Latest, Scope};
use crate::widgets::{Field, ImageClass, ImagePicker, Modal, ModalFooter, ModalSize, Select};

/// How long the form waits after a change before asking for the plan again.
const PLAN_DEBOUNCE: Duration = Duration::from_millis(300);

/// The phases the owner picks; load always runs.
const PICKABLE: [Phase; 5] = [
    Phase::Probes,
    Phase::Prefill,
    Phase::Decode,
    Phase::Concurrent,
    Phase::Mixed,
];

const CACHE_TYPES: [&str; 8] = [
    "f16", "bf16", "q8_0", "q5_1", "q5_0", "q4_1", "q4_0", "iq4_nl",
];

#[component]
pub fn NewRunModal() -> impl IntoView {
    let bn = use_bn();
    let open = RwSignal::new(false);
    let seed = RwSignal::new(None::<BenchArgs>);
    // `?new=1` opens it — from the page's button, or a link from Models or
    // Backends carrying `model` and `image`.
    Effect::new(move |_| {
        let want = bn.q_new.get() == "1";
        let is_open = open.get_untracked();
        if want && !is_open {
            let s = bn.seed.get_untracked().unwrap_or_else(|| BenchArgs {
                model_id: bn.q_model.get_untracked(),
                image: Some(bn.q_image.get_untracked()).filter(|i| !i.is_empty()),
                ..Default::default()
            });
            seed.set(Some(s));
            open.set(true);
        } else if !want && is_open {
            open.set(false);
        }
    });
    Effect::new(move |prev: Option<bool>| {
        let now = open.get();
        if prev == Some(true) && !now {
            seed.set(None);
            bn.seed.set(None);
            bn.q_new.set(String::new());
            bn.q_model.set(String::new());
            bn.q_image.set(String::new());
        }
        now
    });
    view! {
        <Modal open=open title="New benchmark" size=ModalSize::Wide>
            {move || seed.get().map(|s| view! { <NewRunBody seed=s open=open/> })}
        </Modal>
    }
}

/// A whole number or nothing; `Err` says what it has to be.
fn int(s: &str) -> Result<Option<i64>, String> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<i64>()
        .map(Some)
        .map_err(|_| "a whole number, or empty for the row's".to_string())
}

fn opt_text(s: &str) -> Option<String> {
    Some(s.trim().to_string()).filter(|s| !s.is_empty())
}

fn show_opt(v: Option<i64>) -> String {
    v.map(|v| format!("row: {}", grouped(v.max(0) as u64)))
        .unwrap_or_else(|| "row: server default".to_string())
}

/// The form, one signal per control.
#[derive(Clone, Copy)]
struct Draft {
    model: RwSignal<String>,
    rung: RwSignal<String>,
    image: RwSignal<String>,
    ctx: RwSignal<String>,
    parallel: RwSignal<String>,
    batch: RwSignal<String>,
    ubatch: RwSignal<String>,
    cache_k: RwSignal<String>,
    cache_v: RwSignal<String>,
    flash: RwSignal<String>,
    kv_unified: RwSignal<String>,
    ngl: RwSignal<String>,
    no_draft: RwSignal<bool>,
    phases: RwSignal<Vec<Phase>>,
    reps: RwSignal<String>,
    notes: RwSignal<String>,
}

impl Draft {
    fn from(s: &BenchArgs) -> Self {
        let n = |v: Option<i64>| RwSignal::new(v.map(|v| v.to_string()).unwrap_or_default());
        let t = |v: &Option<String>| RwSignal::new(v.clone().unwrap_or_default());
        let phases: Vec<Phase> = if s.phases.is_empty() {
            PICKABLE.to_vec()
        } else {
            s.phases
                .iter()
                .copied()
                .filter(|p| *p != Phase::Load)
                .collect()
        };
        Self {
            model: RwSignal::new(s.model_id.clone()),
            rung: RwSignal::new(s.rung.to_string()),
            image: t(&s.image),
            ctx: n(s.ctx_size),
            parallel: n(s.parallel),
            batch: n(s.batch_size),
            ubatch: n(s.ubatch_size),
            cache_k: t(&s.cache_type_k),
            cache_v: t(&s.cache_type_v),
            flash: t(&s.flash_attn),
            kv_unified: RwSignal::new(match s.kv_unified {
                Some(true) => "on".into(),
                Some(false) => "off".into(),
                None => String::new(),
            }),
            ngl: n(s.n_gpu_layers),
            no_draft: RwSignal::new(s.no_draft),
            phases: RwSignal::new(phases),
            reps: RwSignal::new(s.repetitions.map(|r| r.to_string()).unwrap_or_default()),
            notes: RwSignal::new(s.notes.clone()),
        }
    }

    /// The request, notes left out (the plan ignores them) — or which field
    /// does not parse.
    fn args(&self) -> Result<BenchArgs, (&'static str, String)> {
        let f = |k: &'static str, s: RwSignal<String>| int(&s.get()).map_err(|e| (k, e));
        let reps = match self.reps.get().trim() {
            "" => None,
            t => Some(
                t.parse::<u32>()
                    .ok()
                    .filter(|r| *r >= 1)
                    .ok_or(("reps", "1 or more".to_string()))?,
            ),
        };
        let mut phases = vec![Phase::Load];
        phases.extend(self.phases.get());
        Ok(BenchArgs {
            model_id: self.model.get(),
            rung: self.rung.get().trim().parse().unwrap_or(0),
            image: opt_text(&self.image.get()),
            ctx_size: f("ctx", self.ctx)?,
            parallel: f("parallel", self.parallel)?,
            ubatch_size: f("ubatch", self.ubatch)?,
            batch_size: f("batch", self.batch)?,
            cache_type_k: opt_text(&self.cache_k.get()),
            cache_type_v: opt_text(&self.cache_v.get()),
            flash_attn: opt_text(&self.flash.get()),
            kv_unified: match self.kv_unified.get().as_str() {
                "on" => Some(true),
                "off" => Some(false),
                _ => None,
            },
            n_gpu_layers: f("ngl", self.ngl)?,
            no_draft: self.no_draft.get(),
            phases,
            repetitions: reps,
            notes: String::new(),
        })
    }
}

#[component]
fn NewRunBody(seed: BenchArgs, open: RwSignal<bool>) -> impl IntoView {
    let bn = use_bn();
    let scope = Scope::new();
    let d = Draft::from(&seed);
    let step = RwSignal::new(0u8);

    // The rows and the class default image, once per opening.
    let rows = RwSignal::new(None::<Result<Vec<LocalModel>, String>>);
    let class_image = RwSignal::new(String::new());
    scope.spawn(async move {
        let res = crate::api::get::<ModelsFull>("/api/models/full").await;
        rows.set(Some(
            res.map(|m| m.local.into_iter().map(|v| v.model).collect())
                .map_err(|e| e.to_string()),
        ));
    });
    scope.spawn(async move {
        if let Ok(s) = crate::api::get::<SettingsFull>("/api/settings-full").await {
            class_image.set(s.router.image);
        }
    });
    let row = Memo::new(move |_| {
        let id = d.model.get();
        rows.with(|r| {
            r.as_ref()
                .and_then(|r| r.as_ref().ok())
                .and_then(|r| r.iter().find(|m| m.model_id == id).cloned())
        })
    });
    // No row picked yet: the first enabled one.
    Effect::new(move |_| {
        if !d.model.with_untracked(String::is_empty) {
            return;
        }
        if let Some(first) = rows.with(|r| {
            r.as_ref()
                .and_then(|r| r.as_ref().ok())
                .and_then(|r| r.iter().find(|m| m.enabled).map(|m| m.model_id.clone()))
        }) {
            d.model.set(first);
        }
    });
    // Another row: a rung past its ladder and a drafter it has not got go.
    Effect::new(move |_| {
        let Some(r) = row.get() else { return };
        let top = r.ladder.len();
        if d.rung
            .with_untracked(|v| v.parse::<usize>().unwrap_or(0) > top)
        {
            d.rung.set("0".into());
        }
        let has_draft = r.params.draft_gguf_path.is_some() || r.params.spec_type.is_some();
        if !has_draft && d.no_draft.get_untracked() {
            d.no_draft.set(false);
        }
    });

    let parsed = Memo::new(move |_| d.args());
    let plan_args = Memo::new(move |_| parsed.get().ok().filter(|a| !a.model_id.is_empty()));
    let plan = RwSignal::new(None::<Result<BenchPlan, String>>);
    let plan_for = RwSignal::new(None::<BenchArgs>);
    let read = Latest::new();
    let timer = StoredValue::new(None::<TimeoutHandle>);
    Effect::new(move |_| {
        let a = plan_args.get();
        if let Some(h) = timer.get_value() {
            h.clear();
        }
        let Some(a) = a else { return };
        let h = set_timeout_with_handle(
            move || {
                let Some(ticket) = read.next() else { return };
                scope.spawn(async move {
                    let res = api::plan(&a).await.map_err(|e| e.to_string());
                    if read.is(ticket) {
                        plan.set(Some(res));
                        plan_for.set(Some(a));
                    }
                });
            },
            PLAN_DEBOUNCE,
        )
        .ok();
        timer.set_value(h);
    });
    on_cleanup(move || {
        if let Some(Some(h)) = timer.try_get_value() {
            h.clear();
        }
    });
    // The plan shown is for what the form says now.
    let current = Memo::new(move |_| plan_args.with(|a| plan_for.with(|p| a.is_some() && a == p)));
    let ready =
        move || current.get() && plan.with(|p| matches!(p, Some(Ok(p)) if p.blocked.is_none()));

    let starting = RwSignal::new(false);
    let start = move |_| {
        let Some(mut a) = plan_args.get_untracked() else {
            return;
        };
        a.notes = d.notes.get_untracked().trim().to_string();
        starting.set(true);
        let toasts = bn.toasts;
        spawn_local(async move {
            match api::start(&a).await {
                Ok(s) => {
                    toasts.ok(s.message);
                    if !bn.scope.alive() {
                        return;
                    }
                    bn.go(run_href(s.run_id));
                    open.try_set(false);
                }
                Err(e) => {
                    toasts.err(format!("not started: {e}"));
                    starting.try_set(false);
                    // What changed (a run started elsewhere, the hold) shows
                    // in a fresh plan.
                    if let Some(a) = plan_args.try_get_untracked().flatten() {
                        scope.spawn(async move {
                            if let Ok(p) = api::plan(&a).await {
                                plan.set(Some(Ok(p)));
                            }
                        });
                    }
                }
            }
        });
    };

    let field_err = move |k: &'static str| {
        Signal::derive(move || {
            parsed.with(|p| {
                p.as_ref()
                    .err()
                    .filter(|(f, _)| *f == k)
                    .map(|(_, e)| e.clone())
            })
        })
    };

    view! {
        <Show
            when=move || step.get() == 0
            fallback=move || {
                view! {
                    {move || match plan.get() {
                        Some(Ok(p)) => view! { <Confirmation plan=p/> }.into_any(),
                        _ => view! { <div class="empty">"No plan to confirm."</div> }.into_any(),
                    }}
                }
            }
        >
            <div class="bn-new">
                <div class="bn-new-form">
                    <RowSection d=d rows=rows row=row class_image=class_image/>
                    <OverrideSection d=d row=row field_err=field_err/>
                    <PhaseSection d=d reps_err=field_err("reps")/>
                </div>
                <div class="bn-new-plan">
                    {move || match plan.get() {
                        None => view! { <div class="empty">"Working out the plan…"</div> }.into_any(),
                        Some(Err(e)) => view! { <div class="notice err"><b>"No plan"</b>{e}</div> }.into_any(),
                        Some(Ok(p)) => {
                            let phases = p.params.phases.clone();
                            view! { <PlanPreview plan=p phases=phases stale=Signal::derive(move || !current.get())/> }.into_any()
                        }
                    }}
                </div>
            </div>
        </Show>
        <ModalFooter>
            <Show
                when=move || step.get() == 0
                fallback=move || {
                    view! {
                        <button class="btn ghost" on:click=move |_| step.set(0)>"Back"</button>
                        <button class="btn primary" disabled=move || starting.get() || !ready() on:click=start>
                            {move || if starting.get() { "Starting…" } else { "Start benchmark" }}
                        </button>
                    }
                }
            >
                <button class="btn ghost" on:click=move |_| open.set(false)>"Cancel"</button>
                <button
                    class="btn primary"
                    disabled=move || !ready()
                    title=move || {
                        if !current.get() {
                            "Waiting for the plan of what the form says now".to_string()
                        } else {
                            plan.with(|p| match p {
                                Some(Ok(p)) => p.blocked.as_ref().map(|b| b.message.clone()).unwrap_or_else(|| "See what it stops, then start".into()),
                                Some(Err(e)) => e.clone(),
                                None => "Waiting for the plan".into(),
                            })
                        }
                    }
                    on:click=move |_| step.set(1)
                >
                    "Review and start…"
                </button>
            </Show>
        </ModalFooter>
    }
}

#[component]
fn RowSection(
    d: Draft,
    rows: RwSignal<Option<Result<Vec<LocalModel>, String>>>,
    row: Memo<Option<LocalModel>>,
    class_image: RwSignal<String>,
) -> impl IntoView {
    let options = Signal::derive(move || {
        rows.with(|r| {
            r.as_ref()
                .and_then(|r| r.as_ref().ok())
                .map(|r| {
                    r.iter()
                        .map(|m| {
                            let label = if m.enabled {
                                m.model_id.clone()
                            } else {
                                format!("{} (disabled)", m.model_id)
                            };
                            (m.model_id.clone(), label)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    let rungs = Signal::derive(move || {
        row.with(|r| {
            r.as_ref()
                .map(|r| {
                    let file = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
                    let mut v = vec![(
                        "0".to_string(),
                        format!(
                            "base · {} · {} ctx",
                            file(&r.gguf_path),
                            r.params
                                .ctx_size
                                .map(|c| grouped(c.max(0) as u64))
                                .unwrap_or_else(|| "default".into())
                        ),
                    )];
                    v.extend(r.ladder.iter().enumerate().map(|(i, g)| {
                        (
                            (i + 1).to_string(),
                            format!(
                                "rung {} · {} · {} ctx",
                                i + 1,
                                file(&g.gguf_path),
                                grouped(g.ctx_size.max(0) as u64)
                            ),
                        )
                    }));
                    v
                })
                .unwrap_or_default()
        })
    });
    let placeholder = Signal::derive(move || {
        row.with(|r| r.as_ref().and_then(|r| r.image.clone()))
            .unwrap_or_else(|| class_image.get())
    });
    let load_err = move || rows.with(|r| r.as_ref().and_then(|r| r.as_ref().err().cloned()));
    view! {
        <div class="bn-new-sec">
            <h3>"Row and build"</h3>
            {move || load_err().map(|e| view! { <div class="notice err">{format!("Reading the rows failed: {e}")}</div> })}
            <div class="field-grid">
                <Field label="Local chat row" hint="The row is rendered as it would run; the overrides below change the bench container only.">
                    <Select value=d.model options=options placeholder="pick a row"/>
                </Field>
                <Show when=move || rungs.with(|r| r.len() > 1)>
                    <Field label="Rung" hint="A ladder row's rung: its own weights and context.">
                        <Select value=d.rung options=rungs/>
                    </Field>
                </Show>
                <Field
                    label="Image"
                    wide=true
                    hint="Empty runs the row's image (or the chat class default, shown greyed). Any llama-server image on this machine; comparing two builds is two runs of one row with two images."
                >
                    <ImagePicker value=d.image class=ImageClass::Chat clearable=true placeholder=placeholder/>
                </Field>
            </div>
        </div>
    }
}

#[component]
fn OverrideSection(
    d: Draft,
    row: Memo<Option<LocalModel>>,
    field_err: impl Fn(&'static str) -> Signal<Option<String>> + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let p = move |f: fn(&LocalModel) -> Option<i64>| {
        Signal::derive(move || show_opt(row.with(|r| r.as_ref().and_then(f))))
    };
    let ctx_ph = Signal::derive(move || {
        let rung = d.rung.get().parse::<usize>().unwrap_or(0);
        show_opt(row.with(|r| {
            r.as_ref().and_then(|r| {
                if rung == 0 {
                    r.params.ctx_size
                } else {
                    r.ladder.get(rung - 1).map(|g| g.ctx_size)
                }
            })
        }))
    });
    let row_text = move |f: fn(&LocalModel) -> Option<String>| {
        Signal::derive(move || {
            format!(
                "row: {}",
                row.with(|r| r.as_ref().and_then(f))
                    .unwrap_or_else(|| "unset".into())
            )
        })
    };
    let cache_opts = move |f: fn(&LocalModel) -> Option<String>| {
        let as_row = row_text(f);
        Signal::derive(move || {
            let mut v = vec![(String::new(), as_row.get())];
            v.extend(CACHE_TYPES.iter().map(|c| (c.to_string(), c.to_string())));
            v
        })
    };
    let flash_opts = {
        let as_row = row_text(|r| r.params.flash_attn.clone());
        Signal::derive(move || {
            vec![
                (String::new(), as_row.get()),
                ("auto".into(), "auto".into()),
                ("on".into(), "on".into()),
                ("off".into(), "off".into()),
            ]
        })
    };
    let kv_opts = {
        let as_row = row_text(|r| {
            r.params
                .kv_unified
                .map(|b| if b { "on".into() } else { "off".into() })
        });
        Signal::derive(move || {
            vec![
                (String::new(), as_row.get()),
                ("on".into(), "on".into()),
                ("off".into(), "off".into()),
            ]
        })
    };
    let has_draft = move || {
        row.with(|r| {
            r.as_ref()
                .is_some_and(|r| r.params.draft_gguf_path.is_some() || r.params.spec_type.is_some())
        })
    };
    let num =
        move |label: &'static str, key: &'static str, sig: RwSignal<String>, ph: Signal<String>| {
            view! {
                <Field label=label error=field_err(key)>
                    <input
                        class="input"
                        inputmode="numeric"
                        placeholder=move || ph.get()
                        prop:value=move || sig.get()
                        on:input=move |ev| sig.set(event_target_value(&ev))
                    />
                </Field>
            }
        };
    view! {
        <div class="bn-new-sec">
            <h3>"Overrides"</h3>
            <p class="dim bn-plan-note">"Empty is the row's value (the greyed text). An override changes the bench container only, never the row."</p>
            <div class="field-grid bn-over-grid">
                {num("Context", "ctx", d.ctx, ctx_ph)}
                {num("Slots", "parallel", d.parallel, p(|r| r.params.parallel))}
                {num("Batch", "batch", d.batch, p(|r| r.params.batch_size))}
                {num("µbatch", "ubatch", d.ubatch, p(|r| r.params.ubatch_size))}
                {num("GPU layers", "ngl", d.ngl, p(|r| r.params.n_gpu_layers))}
                <Field label="Cache K">
                    <Select value=d.cache_k options=cache_opts(|r| r.params.cache_type_k.clone()) filter=false/>
                </Field>
                <Field label="Cache V">
                    <Select value=d.cache_v options=cache_opts(|r| r.params.cache_type_v.clone()) filter=false/>
                </Field>
                <Field label="Flash attention">
                    <Select value=d.flash options=flash_opts/>
                </Field>
                <Field label="Unified KV">
                    <Select value=d.kv_unified options=kv_opts/>
                </Field>
            </div>
            <Show when=has_draft>
                <label class="check bn-nodraft">
                    <input
                        type="checkbox"
                        prop:checked=move || d.no_draft.get()
                        on:change=move |ev| d.no_draft.set(event_target_checked(&ev))
                    />
                    "Without the row's speculative drafter"
                </label>
            </Show>
        </div>
    }
}

#[component]
fn PhaseSection(d: Draft, reps_err: Signal<Option<String>>) -> impl IntoView {
    let toggle = move |p: Phase, on: bool| {
        d.phases.update(|v| {
            v.retain(|x| *x != p);
            if on {
                v.push(p);
                v.sort();
            }
        })
    };
    view! {
        <div class="bn-new-sec">
            <h3>"Phases"</h3>
            <div class="row check-row bn-phases">
                <label class="check" title="Always runs: the load time and the VRAM the load took are measured on every run">
                    <input type="checkbox" checked disabled/>
                    "load"
                </label>
                {PICKABLE
                    .iter()
                    .map(|p| {
                        let p = *p;
                        view! {
                            <label class="check">
                                <input
                                    type="checkbox"
                                    prop:checked=move || d.phases.with(|v| v.contains(&p))
                                    on:change=move |ev| toggle(p, event_target_checked(&ev))
                                />
                                {phase_label(p)}
                            </label>
                        }
                    })
                    .collect_view()}
            </div>
            <div class="field-grid bn-reps">
                <Field label="Repetitions" hint="Of every point; each keeps its median, min and max." error=reps_err>
                    <input
                        class="input"
                        inputmode="numeric"
                        placeholder="3"
                        prop:value=move || d.reps.get()
                        on:input=move |ev| d.reps.set(event_target_value(&ev))
                    />
                </Field>
                <Field label="Notes" wide=true>
                    <textarea
                        class="input ta bn-notes-ta"
                        placeholder="What this run is for — stored with it, editable later"
                        prop:value=move || d.notes.get()
                        on:input=move |ev| d.notes.set(event_target_value(&ev))
                    ></textarea>
                </Field>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_override_is_the_rows_and_junk_is_refused() {
        assert_eq!(int(""), Ok(None));
        assert_eq!(int(" 8192 "), Ok(Some(8192)));
        assert!(int("8k").is_err());
        assert_eq!(opt_text("  "), None);
        assert_eq!(opt_text(" q8_0 "), Some("q8_0".into()));
        assert_eq!(show_opt(Some(262_144)), "row: 262\u{202F}144");
        assert_eq!(show_opt(None), "row: server default");
    }
}
