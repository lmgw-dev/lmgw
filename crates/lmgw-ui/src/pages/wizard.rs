//! The "Add from Hugging Face" wizard — the primary way models enter lmgw.
//! Repo → pick weights (+companions) → download with live progress →
//! auto-plan + auto-create (chat) / auto-create (aux). Apply stays a
//! separate, deliberate step.
//!
//! Downloads run server-side; if the modal (or the whole window) closes
//! mid-download, the Downloads page can finish the job with its
//! "Plan & create" action.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{DownloadsView, HfAddResult, PlanResult, RepoFile, RepoFiles};
use serde_json::{json, Value};

use crate::scope::Scope;
use crate::widgets::{use_toasts, ModalFooter};

fn urlenc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

/// Suggested aux model id from a weights filename (chat models get theirs
/// from the server-side plan).
fn aux_model_id(file: &str) -> String {
    let stem = file.rsplit('/').next().unwrap_or(file);
    let stem = stem.strip_suffix(".gguf").unwrap_or(stem);
    stem.to_lowercase()
}

#[derive(Clone, PartialEq)]
enum Step {
    Pick,
    Downloading,
    Done {
        summary: String,
        edit_id: Option<i64>,
    },
    Failed {
        error: String,
    },
}

#[component]
pub fn HfWizard() -> impl IntoView {
    let toasts = use_toasts();
    let reload = expect_context::<super::models::ModelsReload>();

    let step = RwSignal::new(Step::Pick);
    let repo = RwSignal::new(String::new());
    let target = RwSignal::new("chat".to_string());
    // Which class an aux download becomes. Rerankers come down the same HF
    // path as everything else (all three classes are llama.cpp GGUFs), so the
    // only thing the wizard has to ask is which flag the preset section gets.
    let aux_kind = RwSignal::new("embed".to_string());
    let fetching = RwSignal::new(false);
    let files = RwSignal::new(None::<Vec<RepoFile>>);
    let fetch_err = RwSignal::new(None::<String>);
    let selected = RwSignal::new(None::<String>);
    // What the downloads list already has from this repo, by file: `done`
    // is on disk, anything else is on its way (or was cut off).
    let tracked = RwSignal::new(Vec::<(String, String)>::new());
    let companions = RwSignal::new(true);
    let queued_ids = RwSignal::new(Vec::<i64>::new());
    let gguf_path = RwSignal::new(String::new());
    let progress = RwSignal::new(Vec::<(String, Option<u64>, String)>::new());
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

    let fetch = move |_| {
        let r = repo.get_untracked().trim().to_string();
        if r.is_empty() || fetching.get_untracked() {
            return;
        }
        fetching.set(true);
        fetch_err.set(None);
        selected.set(None);
        tracked.set(Vec::new());
        let tgt = target.get_untracked();
        scope.spawn(async move {
            let res = crate::api::get::<RepoFiles>(format!(
                "/api/hf/repo?repo={}&target={}",
                urlenc(&r),
                urlenc(&tgt),
            ))
            .await;
            fetching.set(false);
            match res {
                Ok(rf) => {
                    // The target's own primary role first, then a ladder by
                    // quant width and size. Nothing is picked for the user:
                    // the alphabetically first file of a repo is as often a
                    // 17 GB BF16 as the quant anyone wants. The vocabulary is
                    // per target (image-generation §7.1): `weights` for
                    // chat/aux/audio, `diffusion`/`checkpoint` for image.
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
                    files.set(Some(fs));
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
            if tgt == "chat" {
                let plan = match crate::api::get::<PlanResult>(format!(
                    "/api/local-model-plan?path={}",
                    urlenc(&gguf)
                ))
                .await
                {
                    Ok(p) => p,
                    Err(e) => {
                        step.set(Step::Failed {
                            error: format!("download finished but planning failed: {e}"),
                        });
                        return;
                    }
                };
                if !plan.configured_as.is_empty() {
                    step.set(Step::Done {
                        summary: format!(
                            "'{}' downloaded — a model already serves this GGUF, nothing created",
                            plan.model_id
                        ),
                        edit_id: None,
                    });
                    return;
                }
                let mut args = plan.params.clone();
                args.insert("action".into(), json!("create"));
                args.insert("model_id".into(), json!(plan.model_id));
                args.insert("gguf_path".into(), json!(gguf));
                match crate::api::post::<Value, _>("/api/op/local_model_set", &Value::Object(args))
                    .await
                {
                    Ok(v) => {
                        super::models::bump(reload);
                        let id = v.get("id").and_then(Value::as_i64);
                        let warn = if plan.warnings.is_empty() {
                            String::new()
                        } else {
                            format!(
                                " · {} planner warning(s) — see the editor",
                                plan.warnings.len()
                            )
                        };
                        step.set(Step::Done {
                            summary: format!(
                                "'{}' created from the planned parameters{warn}. Start it from \
                                 Models when ready.",
                                plan.model_id
                            ),
                            edit_id: id,
                        });
                    }
                    Err(e) => step.set(Step::Failed {
                        error: format!("download finished but create failed: {e}"),
                    }),
                }
            } else if tgt == "aux" {
                let model_id = aux_model_id(&gguf);
                let kind = aux_kind.get_untracked();
                match crate::api::post::<Value, _>(
                    "/api/op/aux_model_set",
                    &json!({
                        "action": "create", "model_id": model_id,
                        "gguf_path": gguf, "kind": kind,
                    }),
                )
                .await
                {
                    Ok(_) => {
                        super::models::bump(reload);
                        step.set(Step::Done {
                            summary: format!(
                                "{kind} model '{model_id}' created. Start it from Models when ready."
                            ),
                            edit_id: None,
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
        starting.set(true);
        let r = repo.get_untracked().trim().to_string();
        let tgt = target.get_untracked();
        let comp = companions.get_untracked() && tgt == "chat";
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
    };

    let start_label = move || {
        if starting.get() {
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
                            {["chat", "aux", "audio", "image"]
                                .into_iter()
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
                                                }
                                            }
                                        >
                                            {t}
                                        </button>
                                    }
                                })
                                .collect_view()}
                        </div>
                        <Show when=move || target.get() == "chat">
                            <label class="row dim" style="gap:5px">
                                <input
                                    type="checkbox"
                                    prop:checked=move || companions.get()
                                    on:change=move |ev| companions.set(event_target_checked(&ev))
                                />
                                "fetch projector / drafter companions"
                            </label>
                        </Show>
                        <Show when=move || target.get() == "aux">
                            <div class="seg">
                                {["embed", "rerank"]
                                    .into_iter()
                                    .map(|k| {
                                        view! {
                                            <button
                                                class=move || {
                                                    if aux_kind.get() == k {
                                                        "seg-btn active"
                                                    } else {
                                                        "seg-btn"
                                                    }
                                                }
                                                on:click=move |_| aux_kind.set(k.to_string())
                                            >
                                                {k}
                                            </button>
                                        }
                                    })
                                    .collect_view()}
                            </div>
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
                                    </div>
                                    <div class="wiz-files fill-pane">
                                        <For each=move || fs.clone() key=|f| f.file.clone() let:f>
                                            <FileRow
                                                f=f
                                                target=target.get_untracked()
                                                selected=selected
                                                tracked=tracked
                                            />
                                        </For>
                                    </div>
                                }
                            })
                    }}
                    <ModalFooter>
                        <span class="wiz-pick dim">
                            {move || match selected.get() {
                                Some(f) => f,
                                None if files.with(Option::is_some) => "pick a file".to_string(),
                                None => String::new(),
                            }}
                        </span>
                        <button
                            class="btn primary"
                            disabled=move || {
                                selected.get().is_none() || files.get().is_none()
                                    || starting.get()
                            }
                            on:click=start
                        >
                            {start_label}
                        </button>
                    </ModalFooter>
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
                    </div>
                </div>
            }
                .into_any(),
            Step::Done { summary, edit_id } => view! {
                <div class="wiz">
                    <div class="wiz-ok">{summary}</div>
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
/// Chat plans and creates one, aux creates one from the filename; audio needs
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

#[component]
fn FileRow(
    f: RepoFile,
    target: String,
    selected: RwSignal<Option<String>>,
    tracked: RwSignal<Vec<(String, String)>>,
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
            on:click=move |_| selected.set(Some(pick.clone()))
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
            <span class="dim mono-sm">{f.size.clone()}</span>
        </button>
    }
}

#[cfg(test)]
mod tests {
    use super::{file_role, quant_bits};

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
