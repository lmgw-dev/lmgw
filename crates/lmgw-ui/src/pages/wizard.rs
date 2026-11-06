//! The "Add from Hugging Face" wizard — the primary way models enter lmgw.
//! Repo → pick weights (+companions) → download with live progress →
//! auto-plan + auto-create (chat and aux). Apply stays a separate,
//! deliberate step.
//!
//! An aux download is planned like a chat one (`local_model_plan
//! target=aux`): the header decides the kind (embed, rerank or decision) and
//! the five fields (decision-models design §3.3, §3.4), the row is created
//! from the plan's params, and the plan's card — each value with its reason,
//! and its warnings — is shown as the aux editor shows it.
//!
//! Downloads run server-side; if the modal (or the whole window) closes
//! mid-download, the Downloads page can finish the job with its
//! "Plan & create" action.
//!
//! **Marks and the recommended file** (find-a-model design §3.3, WP3). For
//! the chat and aux classes the list is asked again with
//! `fit=unless_metered`, which marks every weights file with its fit on the
//! graphics card ([`marks`]) and names the recommended one; that file is
//! selected, and nothing downloads before the button. A file chosen whose
//! mark was estimated from another file's header, or is by its size, is read
//! exactly (`local_model_plan` with `repo` + `file`) before the button is
//! enabled; a file chosen before the marks are in waits for them, so
//! nothing is read without a card to measure against (D9). On a metered
//! connection nothing is read by itself (D123): the marks are by size, and
//! "Check exactly" asks for the headers.
//!
//! **Pick-only mode** (D34). With `on_pick`, the button reads "Use this
//! file" and hands the chosen file back to the host instead of downloading
//! it: the finder's "Choose the file myself". It offers only the chat and
//! aux classes, the two Get it sets up, and no companions box: Get it fetches
//! the projector and the drafter itself.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::model_find::{FitMark, PlanFit};
use lmgw_api_types::{
    AuxModelSaved, DownloadsView, HfAddResult, LocalModelSaved, PlanResult, RepoFile, RepoFiles,
};
use serde_json::{json, Value};

pub(crate) mod marks;

use super::model_editors::aux_plan::{self, Filling, PlanCard, ShownPlan};
use crate::scope::Scope;
use crate::widgets::{use_toasts, ModalFooter};

fn urlenc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

/// What finishing a chat or aux download does with the file's plan — here
/// and on the Downloads page, whose "Plan & create" finishes a download the
/// wizard was closed on.
#[derive(Debug, PartialEq)]
pub(super) enum Planned {
    /// A row of the class already serves this GGUF: nothing is created.
    AlreadyServed,
    /// The create body: every planned parameter (for aux: the kind, the
    /// pooling, the context and the five fields), the plan's model id, and
    /// the downloaded file.
    Create(Value),
}

pub(super) fn planned(plan: &PlanResult, gguf: &str) -> Planned {
    if !plan.configured_as.is_empty() {
        return Planned::AlreadyServed;
    }
    let mut args = plan.params.clone();
    args.insert("action".into(), json!("create"));
    args.insert("model_id".into(), json!(plan.model_id));
    args.insert("gguf_path".into(), json!(gguf));
    Planned::Create(Value::Object(args))
}

/// The plan route for a downloaded GGUF of `target` (chat or aux), with the
/// file's path already URI-encoded: an aux file is planned with
/// `target=aux`, which reads the kind and the five fields from its header.
/// It reads the header only: a download never triggers a model load, and
/// whether the image can load the file is the row's Test button's job.
pub(super) fn plan_url(target: &str, gguf_encoded: &str) -> String {
    let aux = if target == "aux" { "target=aux&" } else { "" };
    format!("/api/local-model-plan?{aux}path={gguf_encoded}")
}

/// How a row created from `plan` for `target` is named: an aux row with its
/// kind.
pub(super) fn created_name(plan: &PlanResult, target: &str) -> String {
    match plan.params.get("kind").and_then(Value::as_str) {
        Some(kind) if target == "aux" => format!("{kind} model '{}'", plan.model_id),
        _ => format!("'{}'", plan.model_id),
    }
}

/// Post a plan's create body ([`Planned::Create`]) to `target`'s set op,
/// and the created row's id.
pub(super) async fn post_create(target: &str, body: &Value) -> crate::api::Result<i64> {
    if target == "aux" {
        crate::api::post::<AuxModelSaved, _>("/api/op/aux_model_set", body)
            .await
            .map(|v| v.id)
    } else {
        crate::api::post::<LocalModelSaved, _>("/api/op/local_model_set", body)
            .await
            .map(|v| v.id)
    }
}

/// The companions an `hf_add` for `target` asks for, with the box ticked:
/// chat takes a projector and a drafter, aux a projector (an aux row has no
/// drafter); audio and image take none.
fn wants_companions(target: &str, ticked: bool) -> bool {
    ticked && matches!(target, "chat" | "aux")
}

#[derive(Clone, PartialEq)]
enum Step {
    Pick,
    Downloading,
    /// Every file is down; the plan is being read from the header.
    Planning,
    Done {
        summary: String,
        edit_id: Option<i64>,
    },
    Failed {
        error: String,
    },
}

/// The file the pick-only mode hands back (D34).
#[derive(Debug, Clone, PartialEq)]
pub struct PickedFile {
    pub repo: String,
    /// The weights file's path in `repo`.
    pub file: String,
    /// The class it was listed for: `chat`, `aux`, `audio` or `image`.
    pub target: String,
    /// Whether the companions box was ticked.
    pub companions: bool,
    /// Its fit on the card, by its own header when it was read; `None` when
    /// no fit was measured.
    pub fit: Option<PlanFit>,
    /// Its bytes as the listing gives them (every part of a split file).
    pub size_bytes: u64,
    /// Its quant label, from its name.
    pub quant: Option<String>,
}

/// The classes the "Serve as" switch offers: in the pick-only mode the two
/// Get it sets up (`model_get` takes chat and aux, find-a-model design
/// D29), else every class.
fn classes_for(pick_only: bool) -> &'static [&'static str] {
    if pick_only {
        &["chat", "aux"]
    } else {
        &["chat", "aux", "audio", "image"]
    }
}

/// Whether the list of `target` gets marks: the classes a hub plan reads a
/// header for (find-a-model design D51).
fn marks_on(target: &str) -> bool {
    matches!(target, "chat" | "aux")
}

/// Whether `fit` is a header's, not an estimate or a size: what a chosen
/// file needs before the button (find-a-model design §3.3). A list without
/// a card to measure against has nothing to check.
fn exact_fit(fit: &PlanFit, estimated_from: Option<&str>) -> bool {
    estimated_from.is_none()
        && !matches!(
            fit.beside.mark,
            FitMark::FitsBySize | FitMark::DoesNotFitBySize | FitMark::Unknown
        )
}

#[component]
pub fn HfWizard(
    /// A repo to open with, listed at once.
    #[prop(optional, into)]
    initial_repo: Option<String>,
    /// The file to select in it, in place of the recommended one.
    #[prop(optional, into)]
    initial_file: Option<String>,
    /// The class to list for; `chat` when absent.
    #[prop(optional, into)]
    initial_target: Option<String>,
    /// The pick-only mode (find-a-model design D34): the button reads "Use
    /// this file" and hands the file here; nothing is downloaded.
    #[prop(optional, into)]
    on_pick: Option<Callback<PickedFile>>,
) -> impl IntoView {
    let toasts = use_toasts();
    // A host other than the Models page (the finder's pick-only mode) has
    // no list to reload.
    let reload = use_context::<super::models::ModelsReload>();

    let step = RwSignal::new(Step::Pick);
    let repo = RwSignal::new(initial_repo.clone().unwrap_or_default());
    let target = RwSignal::new(initial_target.unwrap_or_else(|| "chat".to_string()));
    // The marks (module doc): the list asked again with `fit`, the files
    // read exactly since, and the read under way.
    let fit_answer = RwSignal::new(None::<RepoFiles>);
    let fit_busy = RwSignal::new(false);
    let fit_err = RwSignal::new(None::<String>);
    let exact = RwSignal::new(Vec::<(String, PlanFit)>::new());
    let reading = RwSignal::new(None::<String>);
    let read_err = RwSignal::new(Vec::<(String, String)>::new());
    // Answers for a list that was replaced since end there.
    let generation = StoredValue::new(0u32);
    let wanted_file = StoredValue::new(initial_file);
    // The aux plan an aux row was created from, as the aux editor's plan
    // card shows it. The kind is the header's, not a question: an embedder,
    // a reranker and a decision model all come down the same HF path.
    let aux_shown = RwSignal::new(None::<ShownPlan>);
    let fetching = RwSignal::new(false);
    let files = RwSignal::new(None::<Vec<RepoFile>>);
    // The server's note on the listing: what it says when nothing matched.
    let files_note = RwSignal::new(String::new());
    let fetch_err = RwSignal::new(None::<String>);
    let selected = RwSignal::new(None::<String>);
    // What the downloads list already has from this repo, by file: `done`
    // is on disk, anything else is on its way (or was cut off).
    let tracked = RwSignal::new(Vec::<(String, String)>::new());
    let companions = RwSignal::new(true);
    let queued_ids = RwSignal::new(Vec::<i64>::new());
    let gguf_path = RwSignal::new(String::new());
    let progress = RwSignal::new(Vec::<(String, Option<u64>, String)>::new());
    // What the model's files run in, when it was not on this machine: the
    // download fetched it too (machine-check design §5, D16), its own line.
    let engine = RwSignal::new(None::<lmgw_api_types::EngineDownload>);
    let starting = RwSignal::new(false);
    // Reads that answer after the page was left end there (review code:C4).
    let scope = Scope::new();
    // The progress poll, cleared with the page. `poll` starts it from a
    // task's continuation, where there is no owner to hang a cleanup on — so
    // it used to outlive the page and read its disposed signals every 2 s.
    let poller = StoredValue::new(None::<IntervalHandle>);
    let stop_polling = move || {
        if let Some(h) = poller.try_update_value(Option::take).flatten() {
            h.clear();
        }
    };
    on_cleanup(stop_polling);

    // A file's fit as the list shows it: its exact read's, else the list's
    // with the list's card where the file's is left out (D63), with the file
    // it was estimated from.
    let shown_fit = move |file: &str| -> Option<(PlanFit, Option<String>)> {
        if let Some(f) = exact.with(|v| v.iter().find(|(n, _)| n == file).map(|(_, f)| f.clone())) {
            return Some((f, None));
        }
        fit_answer.with(|a| {
            let a = a.as_ref()?;
            a.files
                .iter()
                .find(|f| f.file == file)
                .and_then(|f| f.fit.clone().map(|fit| (fit, f.estimated_from.clone())))
                .map(|(mut fit, from)| {
                    if fit.card.is_none() {
                        fit.card = a.card.clone();
                    }
                    (fit, from)
                })
        })
    };
    // Reads `file` exactly when its mark isn't a header's (module doc):
    // never before the marks are in, without a card, or on a metered
    // connection. Never downloads.
    let check = move |file: String| {
        let tgt = target.get_untracked();
        if !marks_on(&tgt) {
            return;
        }
        // The marks' answer decides; a list whose marks failed reads what is
        // chosen, as a plan of it would.
        let gate = fit_answer
            .with_untracked(|a| a.as_ref().map(|a| a.card.is_some() && a.metered.is_none()));
        match gate {
            None if fit_busy.get_untracked() || fit_err.with_untracked(Option::is_none) => return,
            Some(false) => return,
            _ => {}
        }
        let known = shown_fit(&file).is_some_and(|(f, from)| exact_fit(&f, from.as_deref()));
        if known {
            return;
        }
        let r = repo.get_untracked().trim().to_string();
        let gen = generation.get_value();
        reading.set(Some(file.clone()));
        scope.spawn(async move {
            let res = crate::api::get::<PlanResult>(format!(
                "/api/local-model-plan?repo={}&file={}&target={}",
                urlenc(&r),
                urlenc(&file),
                urlenc(&tgt),
            ))
            .await;
            if generation.get_value() != gen {
                return;
            }
            match res {
                Ok(p) => match p.fit {
                    Some(fit) => exact.update(|v| v.push((file.clone(), fit))),
                    None => read_err.update(|v| {
                        v.push((
                            file.clone(),
                            "the plan measured no fit for this file".into(),
                        ))
                    }),
                },
                Err(e) => read_err.update(|v| v.push((file.clone(), e.to_string()))),
            }
            if reading.get_untracked().as_deref() == Some(file.as_str()) {
                reading.set(None);
            }
        });
    };
    let select = move |file: String| {
        selected.set(Some(file.clone()));
        check(file);
    };
    // Asks for the marks with `fit` as `mode` (`unless_metered` by itself,
    // `true` for "Check exactly"); the answer selects the recommended file
    // when none is chosen yet, and decides whether a chosen one is read.
    let ask_marks = move |r: String, tgt: String, gen: u32, mode: &'static str| {
        fit_busy.set(true);
        fit_err.set(None);
        scope.spawn(async move {
            let res = crate::api::get::<RepoFiles>(format!(
                "/api/hf/repo?repo={}&target={}&fit={mode}",
                urlenc(&r),
                urlenc(&tgt),
            ))
            .await;
            if generation.get_value() != gen {
                return;
            }
            fit_busy.set(false);
            match res {
                Ok(a) => {
                    let pick = a.recommended.clone();
                    fit_answer.set(Some(a));
                    match (selected.get_untracked(), pick) {
                        (Some(f), _) => check(f),
                        (None, Some(p)) => select(p),
                        (None, None) => {}
                    }
                }
                Err(e) => {
                    fit_err.set(Some(e.to_string()));
                    if let Some(f) = selected.get_untracked() {
                        check(f);
                    }
                }
            }
        });
    };
    let check_exactly = move |_| {
        let r = repo.get_untracked().trim().to_string();
        let tgt = target.get_untracked();
        fit_answer.set(None);
        ask_marks(r, tgt, generation.get_value(), "true");
    };

    let fetch = move |_| {
        let r = repo.get_untracked().trim().to_string();
        if r.is_empty() || fetching.get_untracked() {
            return;
        }
        fetching.set(true);
        fetch_err.set(None);
        // Nothing of the list before stays under the new one, whatever this
        // listing answers (review 7).
        files.set(None);
        selected.set(None);
        tracked.set(Vec::new());
        generation.update_value(|g| *g += 1);
        let gen = generation.get_value();
        fit_answer.set(None);
        fit_busy.set(false);
        fit_err.set(None);
        exact.set(Vec::new());
        reading.set(None);
        read_err.set(Vec::new());
        let tgt = target.get_untracked();
        scope.spawn(async move {
            let res = crate::api::get::<RepoFiles>(format!(
                "/api/hf/repo?repo={}&target={}",
                urlenc(&r),
                urlenc(&tgt),
            ))
            .await;
            fetching.set(false);
            if generation.get_value() != gen {
                return;
            }
            match res {
                Ok(rf) => {
                    // The target's own primary role first, then a ladder by
                    // quant width and size. The vocabulary is per target
                    // (image-generation §7.1): `weights` for chat/aux/audio,
                    // `diffusion`/`checkpoint` for image. An earlier pass
                    // picked nothing here, because nothing could tell which
                    // file fits: the alphabetically first file of a repo is as
                    // often a 17 GB BF16 as the quant anyone wants. The marks
                    // can, so the recommended file is selected once they are
                    // in (below), and a file the host asked for before that.
                    files_note.set(rf.note);
                    let mut fs = rf.files;
                    fs.sort_by(|a, b| {
                        let key = |f: &RepoFile| {
                            (
                                !is_primary_role(&tgt, file_role(&f.file, &f.role)),
                                f.quant.as_deref().map_or(u32::MAX, quant_bits),
                                f.size_bytes,
                            )
                        };
                        key(a).cmp(&key(b)).then(a.file.cmp(&b.file))
                    });
                    let wanted = wanted_file
                        .get_value()
                        .filter(|w| fs.iter().any(|f| &f.file == w));
                    files.set(Some(fs));
                    // Asked for before the selection, which then waits for
                    // the marks' answer instead of reading (D9, D65).
                    if marks_on(&tgt) {
                        ask_marks(r.clone(), tgt.clone(), gen, "unless_metered");
                    }
                    if let Some(w) = wanted {
                        select(w);
                    }
                    // The "on disk" marks; the list works without them.
                    if let Ok(dv) = crate::api::get::<DownloadsView>("/api/hf/downloads").await {
                        tracked.set(
                            dv.downloads
                                .into_iter()
                                .filter(|d| d.repo == r)
                                .map(|d| (d.file, d.status))
                                .collect(),
                        );
                    }
                }
                Err(e) => fetch_err.set(Some(e.to_string())),
            }
        });
    };

    // Called when every queued row reports done: plan + create without
    // further clicks. Never applies.
    let finish = move || {
        let tgt = target.get_untracked();
        let gguf = gguf_path.get_untracked();
        scope.spawn(async move {
            if tgt == "chat" || tgt == "aux" {
                let aux = tgt == "aux";
                step.set(Step::Planning);
                let plan = match crate::api::get::<PlanResult>(plan_url(&tgt, &urlenc(&gguf))).await
                {
                    Ok(p) => p,
                    Err(e) => {
                        step.set(Step::Failed {
                            error: format!("download finished but planning failed: {e}"),
                        });
                        return;
                    }
                };
                let body = match planned(&plan, &gguf) {
                    Planned::AlreadyServed => {
                        step.set(Step::Done {
                            summary: format!(
                                "'{}' downloaded — a model already serves this GGUF, nothing \
                                 created",
                                plan.model_id
                            ),
                            edit_id: None,
                        });
                        return;
                    }
                    Planned::Create(body) => body,
                };
                let reload_list = move || {
                    if let Some(r) = reload {
                        super::models::bump(r);
                    }
                };
                // Shown whether the create lands or not: its warnings are
                // what explain a refusal.
                if aux {
                    aux_shown.set(Some(aux_plan::shown(&plan, Filling::New)));
                }
                match post_create(&tgt, &body).await {
                    Ok(id) => {
                        reload_list();
                        // The warnings' own words: the reason a warning
                        // gives is what the owner needs.
                        let warn = match (plan.warnings.len(), aux) {
                            (0, _) => String::new(),
                            (n, false) => {
                                format!(" · {n} planner warning(s): {}", plan.warnings.join("; "))
                            }
                            (n, true) => format!(" · {n} planner warning(s), below"),
                        };
                        let what = created_name(&plan, &tgt);
                        step.set(Step::Done {
                            summary: format!(
                                "{what} created from the planned parameters{warn}. Start it \
                                 from Models when ready."
                            ),
                            // The aux editor is a modal on the Models page,
                            // not a page of its own to link to.
                            edit_id: (!aux).then_some(id),
                        });
                    }
                    Err(e) => step.set(Step::Failed {
                        error: format!("download finished but create failed: {e}"),
                    }),
                }
            } else if tgt == "image" {
                // No auto-create for this class: a pipeline is several files
                // across several repos, and one of them alone names no model
                // (image-generation §7.2). "Add from recipe" on Models is the
                // path that fills a row; this one just puts the file on disk.
                step.set(Step::Done {
                    summary: "image file downloaded into the image models dir — name it in an \
                              image model's files (Models → Local · image), or use Add from \
                              recipe for a whole pipeline"
                        .into(),
                    edit_id: None,
                });
            } else {
                step.set(Step::Done {
                    summary: "audio files downloaded — configure the model on the Audio page \
                              (audio model setup needs family/task details)"
                        .into(),
                    edit_id: None,
                });
            }
        });
    };

    // 2 s progress poll while downloading; stops itself on completion.
    let poll = move || {
        stop_polling();
        let handle = set_interval_with_handle(
            move || {
                if step.try_get_untracked() != Some(Step::Downloading) {
                    return;
                }
                scope.spawn(async move {
                    let Ok(dv) = crate::api::get::<DownloadsView>("/api/hf/downloads").await else {
                        return;
                    };
                    let ids = queued_ids.get_untracked();
                    let mine: Vec<_> = dv
                        .downloads
                        .into_iter()
                        .filter(|d| ids.contains(&d.id))
                        .collect();
                    if let Some(bad) = mine.iter().find(|d| d.status == "failed") {
                        step.set(Step::Failed {
                            error: bad
                                .error
                                .clone()
                                .unwrap_or_else(|| format!("download of {} failed", bad.file)),
                        });
                        return;
                    }
                    progress.set(
                        mine.iter()
                            .map(|d| (d.file.clone(), d.percent, d.status.clone()))
                            .collect(),
                    );
                    if !mine.is_empty() && mine.iter().all(|d| d.status == "done") {
                        step.set(Step::Done {
                            summary: "finalizing…".into(),
                            edit_id: None,
                        });
                        finish();
                    }
                });
            },
            std::time::Duration::from_secs(2),
        );
        if let Ok(h) = handle {
            poller.set_value(Some(h));
        }
    };

    let start = move |_| {
        let Some(file) = selected.get_untracked() else {
            return;
        };
        if starting.get_untracked() {
            return;
        }
        if let Some(pick) = on_pick {
            let (size_bytes, quant) = files.with_untracked(|fs| {
                fs.as_ref()
                    .and_then(|fs| fs.iter().find(|f| f.file == file))
                    .map_or((0, None), |f| (f.size_bytes, f.quant.clone()))
            });
            pick.run(PickedFile {
                repo: repo.get_untracked().trim().to_string(),
                file: file.clone(),
                target: target.get_untracked(),
                companions: companions.get_untracked(),
                fit: shown_fit(&file).map(|(f, _)| f),
                size_bytes,
                quant,
            });
            return;
        }
        starting.set(true);
        let r = repo.get_untracked().trim().to_string();
        let tgt = target.get_untracked();
        let comp = wants_companions(&tgt, companions.get_untracked());
        spawn_local(async move {
            let res = crate::api::post::<HfAddResult, _>(
                "/api/op/hf_add",
                &json!({
                    "repo": r, "file": file, "target": tgt, "companions": comp,
                }),
            )
            .await;
            starting.set(false);
            match res {
                Ok(add) => {
                    engine.set(add.engine.clone());
                    queued_ids.set(add.downloads.iter().map(|d| d.id).collect());
                    gguf_path.set(add.gguf_path.clone());
                    progress.set(
                        add.downloads
                            .iter()
                            .map(|d| (d.file.clone(), None, d.status.clone()))
                            .collect(),
                    );
                    step.set(Step::Downloading);
                    if scope.alive() {
                        poll();
                    }
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let reset = move |_| {
        step.set(Step::Pick);
        files.set(None);
        selected.set(None);
        queued_ids.set(Vec::new());
        progress.set(Vec::new());
        aux_shown.set(None);
        engine.set(None);
    };
    // The line of the download for the graphics card, while the wizard
    // shows this model's download and after it.
    let engine_line = move || {
        engine.get().map(|e| {
            view! {
                <div class="dl-line wiz-gpu-line">
                    <crate::gpu_download::PullLine engine=e/>
                </div>
            }
        })
    };

    if initial_repo.is_some_and(|r| !r.trim().is_empty()) {
        fetch(());
    }

    let start_label = move || {
        if on_pick.is_some() {
            "Use this file"
        } else if starting.get() {
            "Queueing…"
        } else if creates_a_row(&target.get()) {
            "Download & create"
        } else {
            // Audio needs family/task details and an image row needs several
            // files, so for those two the wizard only downloads.
            "Download"
        }
    };

    view! {
        {move || match step.get() {
            Step::Pick => view! {
                <div class="wiz">
                    <div class="row" style="flex-wrap:nowrap">
                        <input
                            class="input mono"
                            style="flex:1"
                            placeholder="org/repo, e.g. unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF"
                            prop:value=move || repo.get()
                            on:input=move |ev| repo.set(event_target_value(&ev))
                            on:keydown=move |ev| {
                                if ev.key() == "Enter" {
                                    fetch(());
                                }
                            }
                        />
                        <button
                            class="btn"
                            disabled=move || fetching.get()
                            on:click=move |_| fetch(())
                        >
                            {move || if fetching.get() { "Fetching…" } else { "Fetch" }}
                        </button>
                    </div>
                    <div class="row" style="margin-top:10px">
                        <span class="dim">"Serve as:"</span>
                        <div class="seg">
                            {classes_for(on_pick.is_some())
                                .iter()
                                .copied()
                                .map(|t| {
                                    view! {
                                        <button
                                            class=move || {
                                                if target.get() == t { "seg-btn active" } else { "seg-btn" }
                                            }
                                            on:click=move |_| {
                                                if target.get_untracked() != t {
                                                    target.set(t.to_string());
                                                    files.set(None);
                                                    selected.set(None);
                                                    generation.update_value(|g| *g += 1);
                                                    fit_answer.set(None);
                                                    fit_busy.set(false);
                                                }
                                            }
                                        >
                                            {t}
                                        </button>
                                    }
                                })
                                .collect_view()}
                        </div>
                        <Show when=move || on_pick.is_none() && wants_companions(&target.get(), true)>
                            <label
                                class="row dim"
                                style="gap:5px"
                                title=move || {
                                    if target.get() == "aux" {
                                        "The kind (embed, rerank or decision) and its settings are read from the GGUF's header once it is down"
                                    } else {
                                        ""
                                    }
                                }
                            >
                                <input
                                    type="checkbox"
                                    prop:checked=move || companions.get()
                                    on:change=move |ev| companions.set(event_target_checked(&ev))
                                />
                                {move || {
                                    if target.get() == "aux" {
                                        "fetch the projector companion"
                                    } else {
                                        "fetch projector / drafter companions"
                                    }
                                }}
                            </label>
                        </Show>
                    </div>
                    {move || fetch_err.get().map(|e| view! { <div class="wiz-err">{e}</div> })}
                    {move || {
                        files
                            .get()
                            .map(|fs| {
                                let tgt = target.get_untracked();
                                let picks = fs
                                    .iter()
                                    .filter(|f| is_primary_role(&tgt, file_role(&f.file, &f.role)))
                                    .count();
                                let others = fs.len() - picks;
                                if fs.is_empty() {
                                    return view! {
                                        <div class="wiz-count dim">
                                            {files_note.get()}
                                        </div>
                                    }
                                    .into_any();
                                }
                                view! {
                                    <div class="wiz-count dim">
                                        {format!(
                                            "{} files · {picks} to pick from{}",
                                            fs.len(),
                                            if others > 0 {
                                                format!(" · {others} companions and other files, listed after them")
                                            } else {
                                                String::new()
                                            },
                                        )}
                                        {move || {
                                            fit_answer
                                                .with(|a| {
                                                    a.as_ref()
                                                        .filter(|a| a.header_bytes_read > 0)
                                                        .map(|a| {
                                                            format!(
                                                                " · lmgw read {} of file headers to check which fit",
                                                                crate::fmt::fmt_bytes(a.header_bytes_read),
                                                            )
                                                        })
                                                })
                                        }}
                                    </div>
                                    {move || fit_line(fit_busy.get(), fit_err.get(), fit_answer.get())}
                                    <Show when=move || {
                                        !fit_busy.get()
                                            && fit_answer.with(|a| a.as_ref().is_some_and(|a| a.metered.is_some()))
                                    }>
                                        <div class="row wiz-fit">
                                            <button
                                                class="btn"
                                                title="Reads the start of each file it needs, a few MiB each, mostly the model's vocabulary"
                                                on:click=check_exactly
                                            >
                                                "Check exactly"
                                            </button>
                                            <span class="dim">"reads the start of the files it needs, a few MiB each"</span>
                                        </div>
                                    </Show>
                                    <div class="wiz-files fill-pane">
                                        <For each=move || fs.clone() key=|f| f.file.clone() let:f>
                                            {
                                                let file = f.file.clone();
                                                let file2 = f.file.clone();
                                                let file3 = f.file.clone();
                                                let is_weights = f.role == "weights";
                                                let file4 = f.file.clone();
                                                let mark = Signal::derive(move || {
                                                    // The whole card's pick, recommended because
                                                    // nothing qualifies beside what runs now, leads
                                                    // with the whole card as the line does (D120).
                                                    let lead = fit_answer
                                                        .with(|a| {
                                                            a.as_ref().is_some_and(|a| {
                                                                a.recommended.as_deref() == Some(file4.as_str())
                                                                    && a.picks.as_ref().is_some_and(|p| {
                                                                        p.beside.file.as_deref() != Some(file4.as_str())
                                                                    })
                                                            })
                                                        });
                                                    let lead = if lead { marks::Lead::WholeCard } else { marks::Lead::Beside };
                                                    shown_fit(&file)
                                                        .and_then(|(fit, from)| marks::mark(&fit, from.as_deref(), lead))
                                                });
                                                let recommended = Signal::derive(move || {
                                                    fit_answer
                                                        .with(|a| {
                                                            a.as_ref()
                                                                .and_then(|a| a.recommended.as_deref())
                                                                == Some(file2.as_str())
                                                        })
                                                });
                                                let checking = Signal::derive(move || {
                                                    reading.with(|r| r.as_deref() == Some(file3.as_str()))
                                                        || (is_weights && fit_busy.get()
                                                            && fit_answer.with(Option::is_none))
                                                });
                                                view! {
                                                    <FileRow
                                                        f=f
                                                        target=target.get_untracked()
                                                        selected=selected
                                                        tracked=tracked
                                                        mark=mark
                                                        recommended=recommended
                                                        checking=checking
                                                        on_select=Callback::new(move |f: String| select(f))
                                                    />
                                                }
                                            }
                                        </For>
                                    </div>
                                }
                                    .into_any()
                            })
                    }}
                    <ModalFooter>
                        <span class="wiz-pick dim">
                            {move || match selected.get() {
                                Some(f) => {
                                    let err = read_err
                                        .with(|v| v.iter().find(|(n, _)| *n == f).map(|(_, e)| e.clone()));
                                    match (reading.get().as_deref() == Some(f.as_str()), err) {
                                        (true, _) => format!("{f} · checking whether it fits…"),
                                        (false, Some(e)) => format!("{f} · couldn't check it exactly: {e}"),
                                        (false, None) => f,
                                    }
                                }
                                None if files.with(Option::is_some) => "pick a file".to_string(),
                                None => String::new(),
                            }}
                        </span>
                        <button
                            class="btn primary"
                            disabled=move || {
                                selected.get().is_none() || files.get().is_none()
                                    || starting.get()
                                    || (reading.get().is_some() && reading.get() == selected.get())
                            }
                            on:click=start
                        >
                            {start_label}
                        </button>
                    </ModalFooter>
                </div>
            }
                .into_any(),
            Step::Planning => view! {
                <div class="wiz">
                    <div class="mini-head">
                        "Planning…"
                    </div>
                </div>
            }
                .into_any(),
            Step::Downloading => view! {
                <div class="wiz">
                    <div class="mini-head">"Downloading — safe to close, it finishes on its own"</div>
                    <div class="fill-pane">
                        <For
                            each=move || progress.get()
                            key=|(f, p, s)| (f.clone(), *p, s.clone())
                            let:item
                        >
                            {
                                let (file, pct, status) = item;
                                view! {
                                    <div class="dl-line">
                                        <span class="mono-sm">{file}</span>
                                        <span class="dim mono-sm">
                                            {match pct {
                                                Some(p) => format!("{p}%"),
                                                None => status,
                                            }}
                                        </span>
                                        <div class="progress">
                                            <i style=format!(
                                                "width:{}%",
                                                pct.unwrap_or(2),
                                            )></i>
                                        </div>
                                    </div>
                                }
                            }
                        </For>
                        {engine_line}
                    </div>
                </div>
            }
                .into_any(),
            Step::Done { summary, edit_id } => view! {
                <div class="wiz">
                    <div class="wiz-ok">{summary}</div>
                    {engine_line}
                    <Show when=move || aux_shown.with(Option::is_some)>
                        <div class="fill-pane">
                            <PlanCard plan=aux_shown/>
                        </div>
                    </Show>
                    <ModalFooter>
                        <button class="btn ghost" on:click=reset>
                            "Add another"
                        </button>
                        {edit_id
                            .map(|id| {
                                let href = format!("/models/local/{id}");
                                view! {
                                    <a class="btn primary" href=href>
                                        "Open in editor"
                                    </a>
                                }
                            })}
                    </ModalFooter>
                </div>
            }
                .into_any(),
            Step::Failed { error } => view! {
                <div class="wiz">
                    <div class="wiz-err">{error}</div>
                    <Show when=move || aux_shown.with(Option::is_some)>
                        <div class="fill-pane">
                            <PlanCard plan=aux_shown/>
                        </div>
                    </Show>
                    <ModalFooter>
                        <span class="dim">"Recover from the Downloads page, or"</span>
                        <button class="btn ghost" on:click=reset>
                            "start over"
                        </button>
                    </ModalFooter>
                </div>
            }
                .into_any(),
        }}
    }
}

/// Whether finishing a download of this target also creates a model row.
/// Chat and aux plan one from the file's header and create it; audio needs
/// family/task details the files do not carry, and an image row needs several
/// files across several repos (image-generation §7.2).
fn creates_a_row(target: &str) -> bool {
    matches!(target, "chat" | "aux")
}

/// The role a target downloads *on its own*, as opposed to one fetched as a
/// companion of it. `hf_add` resolves companions for chat; for image every
/// file is picked deliberately, because a pipeline spans repos and the VAE in
/// one of them is nobody's companion (image-generation §7.1).
fn is_primary_role(target: &str, role: &str) -> bool {
    match target {
        "image" => true,
        _ => role == "weights",
    }
}

/// What a GGUF is for: the server's filename guess (`weights`, `mmproj`,
/// `drafter`, …), except for the one kind it files under weights — an
/// importance matrix (`imatrix.gguf`) is quantization input, not a model
/// anyone can serve. Shared with the Downloads page, which offers
/// "Plan & create" on weights only.
pub(super) fn file_role<'a>(path: &str, guess: &'a str) -> &'a str {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    if name.contains("imatrix") {
        "imatrix"
    } else {
        guess
    }
}

/// The weight width a quant label names, for ordering a repo's files as a
/// ladder (Q2 … Q8, then 16- and 32-bit): `UD-Q4_K_XL` and `IQ4_XS` are 4,
/// `BF16` is 16. Unknown labels sort last.
fn quant_bits(quant: &str) -> u32 {
    let q = quant.to_ascii_uppercase();
    let q = q.trim_start_matches("UD-");
    if q.starts_with("BF16") || q.starts_with("F16") {
        return 16;
    }
    if q.starts_with("F32") {
        return 32;
    }
    let digits_after = |marker: &str| {
        q.find(marker).and_then(|i| {
            let rest = &q[i + marker.len()..];
            let n: String = rest.chars().take_while(char::is_ascii_digit).collect();
            n.parse::<u32>().ok()
        })
    };
    digits_after("Q")
        .or_else(|| digits_after("FP"))
        .unwrap_or(u32::MAX)
}

/// The line above the list while its marks come in, and what holds for the
/// whole list once they are ([`marks::list_lines`]).
fn fit_line(busy: bool, err: Option<String>, answer: Option<RepoFiles>) -> Option<AnyView> {
    if busy {
        return Some(
            view! {
                <div class="wiz-fit dim">"Checking which files fit your graphics card…"</div>
            }
            .into_any(),
        );
    }
    if let Some(e) = err {
        return Some(
            view! {
                <div class="wiz-fit dim">{format!("Couldn't check which files fit: {e}")}</div>
            }
            .into_any(),
        );
    }
    let lines = marks::list_lines(&answer?, short);
    (!lines.is_empty()).then(|| {
        view! {
            <div class="wiz-fit">
                {lines.into_iter().map(|l| view! { <div>{l}</div> }).collect_view()}
            </div>
        }
        .into_any()
    })
}

/// A file's name without its folders.
fn short(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

#[allow(clippy::too_many_arguments)]
#[component]
fn FileRow(
    f: RepoFile,
    target: String,
    selected: RwSignal<Option<String>>,
    tracked: RwSignal<Vec<(String, String)>>,
    /// Its mark on the card, when it has one (find-a-model design §3.3).
    mark: Signal<Option<marks::Mark>>,
    /// The list's recommended file (§3.2).
    recommended: Signal<bool>,
    /// Its fit is being read.
    checking: Signal<bool>,
    on_select: Callback<String>,
) -> impl IntoView {
    let file = f.file.clone();
    let pick = f.file.clone();
    let role = file_role(&f.file, &f.role).to_string();
    let is_weights = is_primary_role(&target, &role);
    let is_sel = {
        let file = file.clone();
        move || selected.get().as_deref() == Some(file.as_str())
    };
    let why_not = match role.as_str() {
        "imatrix" => "importance matrix — quantization input, not a model",
        _ => "companion — fetched automatically with the weights",
    };
    let on_disk = {
        let file = file.clone();
        move || {
            tracked.with(|t| {
                t.iter()
                    .find(|(f, _)| *f == file)
                    .map(|(_, status)| status.clone())
            })
        }
    };
    view! {
        <button
            class=move || {
                let mut c = String::from("wiz-file");
                if is_sel() {
                    c.push_str(" sel");
                }
                if !is_weights {
                    c.push_str(" companion");
                }
                c
            }
            disabled=!is_weights
            title=if is_weights { "" } else { why_not }
            on:click=move |_| on_select.run(pick.clone())
        >
            <span class="mono-sm wiz-name">{f.file.clone()}</span>
            {f.quant.clone().map(|q| view! { <span class="type-badge">{q}</span> })}
            {(!is_weights || role != "weights")
                .then(|| view! { <span class="type-badge">{role.clone()}</span> })}
            {(f.parts > 1).then(|| view! { <span class="dim">{format!("{} parts", f.parts)}</span> })}
            {move || {
                on_disk()
                    .map(|status| {
                        if status == "done" {
                            view! {
                                <span class="chip ok" title="Already downloaded into the models dir">
                                    <span class="dot"></span>
                                    "on disk"
                                </span>
                            }
                                .into_any()
                        } else {
                            view! {
                                <span class="chip live" title="Tracked on the Downloads page">
                                    <span class="dot"></span>
                                    {status}
                                </span>
                            }
                                .into_any()
                        }
                    })
            }}
            <span class="spacer" style="flex:1"></span>
            {move || {
                recommended
                    .get()
                    .then(|| view! { <span class="chip info wiz-mark">"recommended"</span> })
            }}
            {move || {
                if checking.get() {
                    return Some(
                        view! { <span class="chip live wiz-mark"><span class="dot"></span>"checking…"</span> }
                            .into_any(),
                    );
                }
                mark.get()
                    .map(|m| {
                        view! {
                            <span class=format!("chip {} wiz-mark", m.tone) title=m.tip>
                                {m.text}
                            </span>
                        }
                            .into_any()
                    })
            }}
            <span class="dim mono-sm wiz-size">{f.size.clone()}</span>
        </button>
    }
}

#[cfg(test)]
mod tests {
    use super::{
        created_name, file_role, plan_url, planned, quant_bits, wants_companions, Planned,
    };
    use lmgw_api_types::PlanResult;
    use serde_json::{json, Value};

    /// A Clef-shaped aux plan: a decision row with its five fields' plan and
    /// a sibling projector, as `local_model_plan target=aux` answers it.
    fn clef(configured: bool) -> PlanResult {
        let configured_as = if configured {
            json!([{"model_id": "clef", "id": 4, "enabled": true, "in_sync": true, "change": []}])
        } else {
            json!([])
        };
        serde_json::from_value(json!({
            "model_id": "clef",
            "params": {
                "gguf_path": "org/Clef-GGUF/Clef-Q4_K_M.gguf",
                "kind": "decision",
                "ctx_size": 262144,
                "ubatch_size": 4096,
                "batch_size": 4096,
                "mmproj_path": "org/Clef-GGUF/mmproj-Clef-F16.gguf",
            },
            "rationale": {"kind": "the header declares decision.type 'clef'"},
            "warnings": [],
            "configured_as": configured_as,
            "next_step": "",
        }))
        .unwrap()
    }

    #[test]
    fn an_aux_download_is_created_from_its_plan_s_kind_and_fields() {
        let gguf = "org/Clef-GGUF/Clef-Q4_K_M.gguf";
        let Planned::Create(body) = planned(&clef(false), gguf) else {
            panic!("an unconfigured file is created");
        };
        assert_eq!(body["action"], "create");
        assert_eq!(body["model_id"], "clef");
        assert_eq!(body["gguf_path"], gguf);
        assert_eq!(body["kind"], "decision", "the header's kind, not a guess");
        assert_eq!(body["ubatch_size"], 4096);
        assert_eq!(body["batch_size"], 4096);
        assert_eq!(body["mmproj_path"], "org/Clef-GGUF/mmproj-Clef-F16.gguf");
        assert_eq!(body.get("backend"), None::<&Value>, "placed as planned");
        assert_eq!(planned(&clef(true), gguf), Planned::AlreadyServed);
    }

    #[test]
    fn the_pick_only_mode_offers_the_classes_get_it_sets_up() {
        assert_eq!(super::classes_for(true), &["chat", "aux"]);
        assert_eq!(super::classes_for(false).len(), 4);
    }

    #[test]
    fn chat_and_aux_fetch_companions_when_ticked() {
        assert!(wants_companions("chat", true));
        assert!(wants_companions("aux", true));
        assert!(!wants_companions("aux", false));
        assert!(!wants_companions("audio", true));
        assert!(!wants_companions("image", true));
    }

    /// The wizard and the Downloads page plan and name a row the same way.
    #[test]
    fn an_aux_file_is_planned_as_aux_and_named_by_its_kind() {
        assert_eq!(
            plan_url("aux", "org%2Fm.gguf"),
            "/api/local-model-plan?target=aux&path=org%2Fm.gguf"
        );
        assert_eq!(
            plan_url("chat", "org%2Fm.gguf"),
            "/api/local-model-plan?path=org%2Fm.gguf"
        );
        assert_eq!(created_name(&clef(false), "aux"), "decision model 'clef'");
        assert_eq!(created_name(&clef(false), "chat"), "'clef'");
    }

    #[test]
    fn quants_read_as_a_width_ladder() {
        assert_eq!(quant_bits("UD-Q4_K_XL"), 4);
        assert_eq!(quant_bits("IQ4_XS"), 4);
        assert_eq!(quant_bits("Q8_0"), 8);
        assert_eq!(quant_bits("IQ3_XXS"), 3);
        assert_eq!(quant_bits("BF16"), 16);
        assert_eq!(quant_bits("f16"), 16);
        assert_eq!(quant_bits("MXFP4_MOE"), 4);
        assert_eq!(quant_bits("kquant"), u32::MAX);
    }

    #[test]
    fn an_importance_matrix_is_not_weights() {
        assert_eq!(
            file_role("noctrex/Shieldstral-GGUF/imatrix.gguf", "weights"),
            "imatrix"
        );
        assert_eq!(file_role("x/Model-Q4_K_M.gguf", "weights"), "weights");
        assert_eq!(file_role("x/mmproj-F16.gguf", "mmproj"), "mmproj");
    }
}
