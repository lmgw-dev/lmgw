//! A local chat model's detail: the whole local model editor (the
//! llama-server parameter surface) as preference groups, and the "exception
//! path" of a GGUF already on disk (`/models/local/new`, prefilled by
//! `?gguf=…&model_id=…` from Wiring's "wire up").
//!
//! The header has Start or Stop and Test, and in its menu Fill from plan,
//! the container's Restart, Logs and Apply, Duplicate, Benchmark and
//! Delete. The hero says where its container is; the problems the saved row
//! has and the last test's answer follow it. Changed fields carry the
//! accent dot, the save toast counts them, Ctrl+S saves, and leaving the
//! row with changes asks first.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{LlamaParams, LocalModelDetail, LocalModelSaved, PlanResult, SettingsFull};
use serde_json::{json, Value};

use super::super::route::Sel;
use super::super::split::SaveKey;
use super::super::{bump, Class, PageCtx, Row};
use super::{container_items, DetailHead, Hero, RunButton};
use crate::pages::fallback_field::Saves;
use crate::pages::local_test::{
    not_tested, run_load_test, test_key, LoadTestCard, Outcome, UnsavedTestModal,
};
use crate::scope::Scope;
use crate::widgets::{ActionRow, Ico, Icon, MenuItem, PrefGroup, SaveToast};

/// The command line, one option per line.
mod cmdline;
/// The form's signals, its baseline, its checks and its patch.
mod form;
/// The fields as preference groups.
mod groups;
/// The rung table and the GGUF browser.
mod ladder;
/// Every `LlamaParams` field and where it sits.
mod specs;

use form::LocalForm;

fn urlenc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

#[component]
pub(super) fn LocalDetail(
    page: PageCtx,
    /// The saved row; `None` makes a new one.
    id: Option<i64>,
    #[prop(into)] row: Signal<Option<Row>>,
    narrow: Memo<bool>,
    back: Callback<()>,
) -> impl IntoView {
    let create_mode = id.is_none();
    let toasts = page.toasts;
    let catalog = crate::catalog::use_model_catalog();
    let scope = Scope::new();
    let form = LocalForm::new();
    // What the saves said about the fallback, and a refusal of it, under
    // the field.
    let saves = Saves::new(id.unwrap_or_default());
    let loaded = RwSignal::new(create_mode);
    let saving = RwSignal::new(false);
    let save_error = RwSignal::new(None::<String>);
    let reload = RwSignal::new(0u32);
    let created = RwSignal::new(None::<i64>);
    // Test (local_test.rs): the load test on the row as saved, its answer
    // kept on the page; with unsaved edits it asks first, and "Save and
    // test" runs it once the save has gone through. Whether one is running
    // is app-wide, keyed by the saved id.
    let ops = page.ops;
    let test_result = RwSignal::new(None::<Outcome>);
    let test_ask = RwSignal::new(false);
    let test_after_save = RwSignal::new(false);
    // What the last save warned of (reasons it may not load) and advised
    // (it loads, but misbehaves under some input): in a toast, and kept
    // under the hero until the next save.
    let save_notes = RwSignal::new((Vec::<String>::new(), Vec::<String>::new()));

    if create_mode {
        form.rebase();
        // Prefilled from a link: not saved yet, so it counts as a change.
        let query = leptos_router::hooks::use_query_map();
        let prefill = |key: &str| query.read_untracked().get(key).unwrap_or_default();
        form.model_id.set(prefill("model_id"));
        form.gguf_path.set(prefill("gguf"));
    }
    let dirty_count = form.dirty_count();
    let invalid = form.invalid();
    let here = match id {
        Some(id) => Sel::Row(Class::Chat, id),
        None => Sel::NewChat,
    };
    page.nav.guard().watch_in(
        "this model",
        Signal::derive(move || dirty_count.get() > 0),
        Signal::stored(here.path()),
    );

    let detail = LocalResource::new(move || {
        reload.get();
        async move {
            let id = id?;
            crate::api::get::<LocalModelDetail>(format!("/api/local-model?id={id}"))
                .await
                .ok()
        }
    });
    let saved = Signal::derive(move || detail.get().flatten());
    // Chat class defaults, for the per-model image/extra_run_args overrides'
    // placeholders — an empty override inherits these (§3.1).
    let settings = LocalResource::new(|| crate::api::get::<SettingsFull>("/api/settings-full"));
    let class_settings = Signal::derive(move || settings.get().and_then(Result::ok));
    Effect::new(move |_| {
        let Some(d) = saved.get() else { return };
        // Only seed the form on first load / after save, never mid-edit.
        if loaded.get_untracked() && reload.get_untracked() == 0 {
            return;
        }
        // Edits typed while the save was in flight stay: the save rebased
        // to what it posted, and a load would overwrite what came after.
        if reload.get_untracked() > 0 && dirty_count.get_untracked() > 0 {
            return;
        }
        form.load(&d);
        loaded.set(true);
    });

    let on_save = Callback::new(move |()| {
        // Asked for by "Save and test" — for this save only.
        let then_test = test_after_save.get_untracked();
        test_after_save.set(false);
        if saving.get_untracked() || invalid.get_untracked() > 0 || dirty_count.get_untracked() == 0
        {
            return;
        }
        let sent = form.snapshot();
        let mut args = match form.collect() {
            Ok(a) => a,
            Err(e) => {
                toasts.err(format!("Not saved: {e}"));
                save_error.set(Some(e));
                return;
            }
        };
        // The id the row is saved under — the form's, which may be a rename.
        let saved_mid = args
            .get("model_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // Create, or update the row: the one this detail shows, or the one
        // its create made (a save after it, before the move to the row, is
        // an update of that one).
        let creating = saves.creating();
        let (action, target) = saves.target();
        args.insert("action".into(), json!(action));
        if !creating {
            args.insert("id".into(), target);
        }
        saving.set(true);
        save_error.set(None);
        save_notes.set((Vec::new(), Vec::new()));
        spawn_local(async move {
            let res = crate::api::post::<LocalModelSaved, _>(
                "/api/op/local_model_set",
                &Value::Object(args),
            )
            .await;
            saving.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v.message);
                    let warnings = v.warnings.clone().unwrap_or_default();
                    let advisories = v.advisories.clone().unwrap_or_default();
                    for note in warnings.iter().chain(&advisories).chain(&v.fallback_notes) {
                        toasts.warn(note.clone());
                    }
                    save_notes.try_set((warnings, advisories));
                    saves.answered(v.id, v.fallback_notes.clone());
                    // Pickers everywhere list what the gateway serves; this
                    // just changed it, and so did the list beside.
                    catalog.refresh();
                    bump(page.reload);
                    // Left while saving: the toast said how it went.
                    if !scope.alive() {
                        return;
                    }
                    form.rebase_to(sent);
                    if creating {
                        // The row's own detail shows what this save said, once
                        // the list has the row.
                        saves.carry();
                        created.set(Some(v.id));
                    } else if create_mode {
                        // An update of what this page created: it moves to
                        // the row once the list has it, and that row's
                        // detail reads it fresh.
                    } else {
                        reload.update(|n| *n += 1);
                        // A container still up on the previous configuration
                        // (mid-request when the save stopped it) is what a
                        // test now would load: say so instead of passing it.
                        let kept = v.container_kept_previous_config == Some(true);
                        if then_test && kept {
                            not_tested(
                                test_result,
                                format!(
                                    "Not tested: '{saved_mid}' is still serving a request on the \
                                     previous configuration, so a test now would load that one. \
                                     Test again once it is idle, or stop it first."
                                ),
                            );
                        } else if then_test {
                            run_load_test(ops, toasts, saved_mid, test_result);
                        }
                    }
                }
                Err(e) => {
                    let e = e.to_string();
                    if !saves.refused(&e) {
                        toasts.err(format!("Not saved: {e}"));
                        save_error.set(Some(e));
                    }
                }
            }
        });
    });
    // A create moves to the new row once the list has it (else the detail
    // would say for a moment that the row is not there), and only while the
    // form is clean: edits typed after the create stay here (a save of them
    // updates the row), and the move follows their save or Discard. A list
    // that fails to load keeps the form here; the list says why, and its
    // next load makes the move.
    Effect::new(move |_| {
        let Some(new_id) = created.get() else { return };
        let there = page.data.full.with(|f| {
            f.as_ref()
                .is_some_and(|f| f.local.iter().any(|v| v.model.id == new_id))
        });
        if there && dirty_count.get() == 0 && page.go(&Sel::Row(Class::Chat, new_id).path(), true) {
            // The move may have unmounted this detail already.
            created.try_set(None);
        }
    });
    let on_discard = Callback::new(move |()| {
        form.discard();
        save_error.set(None);
    });
    SaveKey::install(on_save);

    // Re-plan: overwrite the form's params with what the planner derives from
    // the GGUF right now. Never saves by itself.
    let planning = RwSignal::new(false);
    let replan = move || {
        let gp = form.gguf_path.get_untracked().trim().to_string();
        if gp.is_empty() {
            toasts.err("Fill from plan reads the weights: name the GGUF file first");
            return;
        }
        if planning.get_untracked() {
            return;
        }
        planning.set(true);
        scope.spawn(async move {
            let res = crate::api::get::<PlanResult>(format!(
                "/api/local-model-plan?path={}",
                urlenc(&gp)
            ))
            .await;
            planning.set(false);
            match res {
                Ok(plan) => {
                    match serde_json::from_value::<LlamaParams>(Value::Object(plan.params.clone()))
                    {
                        Ok(p) => {
                            form.fill(&p);
                            if form.model_id.get_untracked().trim().is_empty() {
                                form.model_id.set(plan.model_id.clone());
                            }
                            toasts.ok("form filled from the plan — review and save");
                        }
                        Err(e) => toasts.err(format!("plan did not parse: {e}")),
                    }
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    // The row as saved: what the test loads, whatever the form says now.
    let saved_row = move || saved.get().map(|d| (d.model_id, d.enabled));
    let saved_mid = Signal::derive(move || saved_row().map(|(m, _)| m).unwrap_or_default());
    let testing =
        Signal::derive(move || saved_row().is_some_and(|(mid, _)| ops.busy(&test_key(&mid))));
    let test_saved = Callback::new(move |()| {
        if let Some(mid) = saved.get_untracked().map(|d| d.model_id) {
            run_load_test(ops, toasts, mid, test_result);
        }
    });
    let save_and_test = Callback::new(move |()| {
        test_after_save.set(true);
        on_save.run(());
    });
    let on_test = move |_| {
        if dirty_count.get_untracked() > 0 {
            test_ask.set(true);
        } else {
            test_saved.run(());
        }
    };

    let live = crate::live::use_live();
    let more = Signal::derive(move || {
        let mut v = vec![MenuItem::new(
            if planning.get() {
                "Planning…"
            } else {
                "Fill from plan"
            },
            replan,
        )
        .disabled(planning.get())
        .title("Read the GGUF's metadata and fill the parameters from it; nothing is saved")];
        let Some(id) = id else { return v };
        let mid = saved_mid.get();
        v.extend(container_items(
            page,
            live,
            Class::Chat,
            mid.clone(),
            saved_row().is_some_and(|(_, e)| e),
        ));
        v.extend(super::copy_item(
            page,
            Class::Chat,
            saved.get().map(|d| d.gguf_path).unwrap_or_default(),
        ));
        v.push(
            MenuItem::new("Duplicate", move || {
                page.op(
                    "local_model_set",
                    json!({ "action": "duplicate", "id": id }),
                )
            })
            .title("Clone under a fresh id — same GGUF, params, args and flags"),
        );
        // Benchmark design §8.2: the New benchmark modal, this row picked.
        let bench = StoredValue::new(mid);
        v.push(
            MenuItem::new("Benchmark…", move || {
                let href = crate::pages::benchmarks::new_href(Some(&bench.get_value()), None);
                page.go(&href, false);
            })
            .title("Measure this row: load, probes, prefill, decode, concurrent and mixed load, tokens per joule — it takes the whole GPU while it runs"),
        );
        v.push(delete_item(page, id, row));
        v
    });

    let title = Signal::derive(move || {
        row.get()
            .map(|r| r.name)
            .or_else(|| Some(form.model_id.get()).filter(|m| !m.trim().is_empty()))
            .unwrap_or_else(|| "New local model".into())
    });
    let gguf_path = Signal::derive(move || {
        saved
            .get()
            .map(|d| d.gguf_path)
            .unwrap_or_else(|| form.gguf_path.get())
    });
    let sub_plain = Signal::derive(move || gguf_path.get().trim().is_empty());
    let sub = Signal::derive(move || {
        let p = gguf_path.get();
        if p.trim().is_empty() {
            "No GGUF file yet".to_string()
        } else {
            p
        }
    });
    let own_name = Signal::derive(move || {
        let prefix = class_settings
            .get()
            .map(|s| s.router.public_prefix)
            .unwrap_or_default();
        lmgw_api_types::public_names::prefixed(&prefix, form.model_id.get().trim())
    });
    let scrolled = RwSignal::new(false);

    view! {
        <DetailHead
            narrow=narrow
            back=back
            title=title
            scrolled=scrolled
            actions=move || {
                if create_mode {
                    return view! {
                        <button
                            type="button"
                            class="btn flat"
                            disabled=move || planning.get()
                            title="Read the GGUF's metadata and fill the parameters from it; nothing is saved"
                            on:click=move |_| replan()
                        >
                            <Icon ico=Ico::Wand/>
                            {move || if planning.get() { "Planning…" } else { "Fill from plan" }}
                        </button>
                    }
                    .into_any();
                }
                view! {
                    <RunButton
                        page=page
                        class=Class::Chat
                        model_id=saved_mid
                        enabled=Signal::derive(move || saved_row().is_some_and(|(_, e)| e))
                    />
                    <button
                        type="button"
                        class="btn flat"
                        disabled=move || {
                            testing.get() || saving.get() || !saved_row().is_some_and(|(_, e)| e)
                        }
                        title=move || {
                            if saved_row().is_some_and(|(_, e)| !e) {
                                "Enable it and save first: a disabled model is not served, so there is nothing to test"
                            } else {
                                "Load test on the saved model: starts its container if it is not running (which may evict others under the VRAM ledger), then generates one token"
                            }
                        }
                        on:click=on_test
                    >
                        <Icon ico=Ico::Check/>
                        {move || if testing.get() { "Testing…" } else { "Test" }}
                    </button>
                }
                .into_any()
            }
            more=more
        />
        <div
            class="detail-scroll mdetail-scroll"
            tabindex="-1"
            on:scroll=move |ev| super::track_scroll(&ev, scrolled)
        >
            <div class="detail-flow">
                <Hero class=Class::Chat name=title sub=sub row=row sub_plain=sub_plain/>
                <Show when=move || loaded.get() fallback=|| view! { <div class="mdetail-empty">"Loading…"</div> }>
                    {move || {
                        let problems = saved.get().map(|d| d.problems).unwrap_or_default();
                        (!problems.is_empty())
                            .then(|| {
                                view! {
                                    <PrefGroup title=format!("Problems · {}", problems.len())>
                                        {problems
                                            .into_iter()
                                            .map(|p| view! { <div class="prow problem-row">{p}</div> })
                                            .collect_view()}
                                    </PrefGroup>
                                }
                            })
                    }}
                    {move || {
                        let (warnings, advisories) = save_notes.get();
                        // The saved row's own advisories, until a save said its own.
                        let advisories = if warnings.is_empty() && advisories.is_empty() {
                            saved.get().map(|d| d.advisories).unwrap_or_default()
                        } else {
                            advisories
                        };
                        (!warnings.is_empty() || !advisories.is_empty())
                            .then(|| {
                                view! {
                                    <PrefGroup title="Notes">
                                        {warnings
                                            .into_iter()
                                            .map(|w| view! { <div class="prow problem-row">{w}</div> })
                                            .collect_view()}
                                        {advisories
                                            .into_iter()
                                            .map(|a| view! { <div class="prow notice-row warn">{a}</div> })
                                            .collect_view()}
                                    </PrefGroup>
                                }
                            })
                    }}
                    {(!create_mode).then(|| view! { <LoadTestCard testing=testing result=test_result/> })}
                    // The note of a context Get fitted to the card, and "Fit
                    // again" (find-a-model design D26). Its save is the row's:
                    // the fields it wrote take the saved values, and other
                    // unsaved edits stay as they are.
                    {(!create_mode)
                        .then(|| {
                            view! {
                                <div class="fit-note-wrap">
                                    <crate::pages::model_editors::fit_again::FitAgainNote
                                        target="chat"
                                        model_id=saved_mid
                                        note=Signal::derive(move || saved.get().and_then(|d| d.fit_note))
                                        on_saved=Callback::new(move |changes| form.fit_saved(changes))
                                    />
                                </div>
                            }
                        })}
                    <groups::MainGroups form=form create_mode=create_mode detail=saved/>
                    <groups::FallbackGroup form=form saves=saves own_name=own_name/>
                    <groups::AdvancedGroups
                        form=form
                        class_settings=class_settings
                        detail=saved
                        create_mode=create_mode
                    />
                    {id
                        .map(|id| {
                            view! {
                                <PrefGroup>
                                    <ActionRow
                                        title="Delete model…"
                                        sub="Removes it from the gateway config and stops its container; the files on disk stay"
                                        danger=true
                                        on_click=Callback::new(move |()| {
                                            let name = row.get_untracked().map(|r| r.name).unwrap_or_default();
                                            page.deleting.set(Some((Class::Chat, id, name)));
                                        })
                                    />
                                </PrefGroup>
                            }
                        })}
                </Show>
            </div>
        </div>
        <SaveToast
            dirty_count=dirty_count
            invalid=invalid
            saving=saving
            error=save_error
            on_save=on_save
            on_discard=on_discard
            save_label=move || if create_mode && created.get().is_none() { "Create model" } else { "Save" }
        />
        <UnsavedTestModal
            open=test_ask
            detail=form.dirty_detail()
            invalid=Signal::derive(move || invalid.get() > 0)
            saving=saving
            enabled_after=form.enabled
            on_save_and_test=save_and_test
            on_test_saved=test_saved
        />
    }
}

/// "Delete…" in a row's menu: the modal that says what goes and what stays.
pub(super) fn delete_item(page: PageCtx, id: i64, row: Signal<Option<Row>>) -> MenuItem {
    MenuItem::new("Delete…", move || {
        if let Some(r) = row.get_untracked() {
            page.deleting.set(Some((r.class, id, r.name)));
        }
    })
    .title("Remove it from the gateway config and stop its container; the files on disk stay")
    .destructive()
}
