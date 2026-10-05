//! Hugging Face — the intended way to get a model onto this box

use serde_json::{json, Value};

use crate::hf;
use crate::jobs::{self, JobKind};
use crate::state::SharedState;
use crate::store::{self};

/// What a file in a GGUF repo is *for*.
///
/// A repo like `unsloth/Muse-Glimmer-30B-GGUF` ships three GGUFs that look
/// alike and are configured completely differently: the weights, a vision
/// projector, and a speculative drafter. Naming them is the difference between
/// a caller seeing three opaque paths and seeing a model it can serve.
///
/// Classification is by filename because the hub's tree API gives us nothing
/// else without downloading; [`crate::modelinfo::model_inspect`] re-derives it
/// authoritatively from GGUF metadata once a file is local.
///
/// `target` decides *which* vocabulary of roles is used, and the image one is
/// a separate function ([`classify_image_file`]) rather than three more arms:
/// the chat kinds (`weights` / `mmproj` / `drafter`) describe one llama.cpp
/// repo, the image kinds (`diffusion` / `vae` / `text_encoder` / …) describe a
/// role inside a pipeline, and nothing sensible comes of running one set over
/// the other's files. A chat repo is never re-classified.
pub(crate) fn classify_repo_file(path: &str, target: &str) -> &'static str {
    if target == "image" {
        return classify_image_file(path, None);
    }
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    if !name.ends_with(".gguf") {
        return "other";
    }
    if name.starts_with("mmproj") || name.contains("mmproj") {
        return "mmproj";
    }
    // Only markers that cannot be part of a full model's name count anywhere
    // in it. `mtp` counts as a *prefix* only: a drafter is named for the head
    // it is (`mtp-gemma-4-…`), while `…-NEO-MTP-IQ4_XS` is a full model that
    // carries its own MTP layers. Plain `eagle` does not count at all — Eagle
    // is also a model family (RWKV's Eagle 7B); the drafter method is EAGLE-3.
    // Filing full models as drafters hid them from Wiring's "wire up" and had
    // `hf_add` pull every sibling quant along as a "companion".
    let prefix = ["mtp-", "mtp_"];
    let anywhere = ["dflash", "dspark", "draft", "eagle3", "eagle-3"];
    if prefix.iter().any(|m| name.starts_with(m)) || anywhere.iter().any(|m| name.contains(m)) {
        return "drafter";
    }
    "weights"
}

/// What a file in an image repo (or in the image models dir) is *for*:
/// `diffusion`, `checkpoint`, `vae`, `text_encoder`, `lora`, `upscaler`,
/// `other` — the `files` role it would most likely be filled into.
///
/// **These are heuristics and are labelled as such everywhere they surface**
/// (`role` on `lmgw__hf_repo`, `role_guess` on `lmgw__gguf_files`), because
/// nothing but the filename is available: a `.safetensors` has no header lmgw
/// reads, the hub's tree API returns names and sizes, and the same tensor set
/// ships under a dozen naming conventions. The editor lets the owner override
/// every one of them, and a wrong guess costs a pre-filled field, never a
/// stored value.
///
/// The ladder, strongest evidence first:
///
/// 1. **The directory** — `split_files/vae/`, `text_encoders/`,
///    `diffusion_models/`, `loras/`. Comfy-Org's layout says the role outright
///    and no filename rule beats it.
/// 2. **Unambiguous filename markers** — `clip_l`, `t5xxl`, `esrgan`, `_vae`.
/// 3. **The family name**, which is where `qwen` / `mistral` / `llm` have to
///    be resolved: they are the text-encoder markers §7 names, but
///    `Qwen_Image-Q4_K_M.gguf` is a *diffusion* model and
///    `Qwen3-4B-Instruct-2507-Q4_K_M.gguf` is the `--llm` encoder beside it.
///    A diffusion-family word in the name (`image`, `flux`, `diffusion`,
///    `dit`) wins over the vendor word; otherwise the vendor word means the
///    encoder. (Noted in the spec's §14 — §7's rule alone mis-files the whole
///    Qwen-Image family.)
/// 4. **A `-GGUF` repo or a `diffusion_models` directory** — §7's stated
///    fallback for the quantized DiTs.
/// 5. **A file at the repo root** (`sd_xl_base_1.0.safetensors`,
///    `v1-5-pruned-emaonly.ckpt`) — the all-in-one checkpoint shape, `-m`.
///
/// `repo` is passed when the caller knows it (`lmgw__hf_repo`, where paths are
/// repo-relative); when it is `None` the path is assumed to be the downloader's
/// own `<owner>/<repo>/<file>` layout and the repo is read back off it.
pub(crate) fn classify_image_file(path: &str, repo: Option<&str>) -> &'static str {
    if !hf::accepts_file("image", path) {
        return "other";
    }
    let (repo, rel) = match repo {
        Some(r) => (r.to_string(), path.to_string()),
        // `<owner>/<repo>/<file…>` — the layout `dest_rel_path` writes.
        None => match path.split('/').collect::<Vec<_>>() {
            parts if parts.len() >= 3 => (parts[..2].join("/"), parts[2..].join("/")),
            _ => (String::new(), path.to_string()),
        },
    };
    let name = rel.rsplit('/').next().unwrap_or(&rel).to_ascii_lowercase();
    let dir = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    // Where the file sits, as one lowercase haystack: the repo id plus the
    // directories inside it.
    let scope = format!("{repo}/{dir}/").to_ascii_lowercase();
    let in_dir = |needle: &str| scope.contains(&format!("/{needle}/"));

    // 1. directory evidence
    if in_dir("loras") || in_dir("lora") {
        return "lora";
    }
    if in_dir("vae") {
        return "vae";
    }
    if in_dir("text_encoders") || in_dir("text_encoder") || in_dir("clip") {
        return "text_encoder";
    }
    if in_dir("upscalers") || in_dir("upscale_models") {
        return "upscaler";
    }
    // 2. unambiguous filename markers
    if name.contains("lora") {
        return "lora";
    }
    if ["esrgan", "upscal"].iter().any(|m| name.contains(m)) {
        return "upscaler";
    }
    if ["ae.", "_vae", "vae_", "vae."]
        .iter()
        .any(|m| name.contains(m))
    {
        return "vae";
    }
    if ["clip_l", "clip_g", "t5xxl", "umt5", "clip-l", "clip-g"]
        .iter()
        .any(|m| name.contains(m))
    {
        return "text_encoder";
    }
    // 3. family name before vendor name
    let diffusion_family = ["image", "flux", "diffusion", "-dit", "_dit", "unet"]
        .iter()
        .any(|m| name.contains(m));
    let encoder_vendor = [
        "clip", "t5", "qwen", "mistral", "llm", "gemma", "llama", "mmproj",
    ]
    .iter()
    .any(|m| name.contains(m));
    if !diffusion_family && encoder_vendor {
        return "text_encoder";
    }
    if diffusion_family || in_dir("diffusion_models") || scope.contains("-gguf") {
        return "diffusion";
    }
    // 5. an all-in-one checkpoint sits at the repo root
    if dir.is_empty() {
        return "checkpoint";
    }
    "other"
}

/// The quantization label embedded in a GGUF filename (`…-UD-Q4_K_XL.gguf` →
/// `UD-Q4_K_XL`). Best-effort: repos are not consistent, so this is a hint for
/// choosing a file, never a fact about the file (the GGUF's own `general
/// .file_type` is the fact, and the two genuinely disagree in the wild).
pub(super) fn quant_label(path: &str) -> Option<String> {
    let stem = path.rsplit('/').next()?.strip_suffix(".gguf")?;
    let stem = hf::split_part_name(&format!("{stem}.gguf"))
        .map(|(p, _, _)| p.to_string())
        .unwrap_or_else(|| stem.to_string());
    let up = stem.to_ascii_uppercase();
    let idx = ["-UD-", "-Q", "-IQ", "-BF16", "-F16", "-F32"]
        .iter()
        .filter_map(|m| up.rfind(m))
        .min()?;
    Some(stem[idx + 1..].to_string())
}

/// List a Hugging Face repo's files, classified by role and grouped by quant.
///
/// `target` decides both which file kinds are listed and which role vocabulary
/// names them (§7.1): GGUF-only with weights/mmproj/drafter for the three
/// llama.cpp-shaped classes, and the whole stable-diffusion.cpp set with the
/// pipeline roles for `image`.
pub async fn hf_repo(
    state: &SharedState,
    repo: &str,
    search: Option<&str>,
    target: &str,
) -> Result<Value, String> {
    let target = hf::normalize_target(target)?;
    let snap = state.snapshot();
    let files = hf::list_repo_files(&state.http, &snap.settings.hf_token, repo).await?;
    let needle = search.map(str::to_ascii_lowercase);

    let mut out: Vec<Value> = Vec::new();
    let mut seen_parts: Vec<String> = Vec::new();
    for f in &files {
        if !hf::accepts_file(target, &f.path) {
            continue;
        }
        if let Some(n) = &needle {
            if !f.path.to_ascii_lowercase().contains(n.as_str()) {
                continue;
            }
        }
        // A split GGUF is one logical file; list the first part and say how
        // many there are, since selecting any part downloads all of them.
        let parts = hf::expand_parts(&f.path, &files);
        if parts.len() > 1 {
            if seen_parts.contains(&parts[0]) {
                continue;
            }
            seen_parts.push(parts[0].clone());
        }
        let total: u64 = if parts.len() > 1 {
            files
                .iter()
                .filter(|x| parts.contains(&x.path))
                .filter_map(|x| x.size)
                .sum()
        } else {
            f.size.unwrap_or(0)
        };
        out.push(json!({
            "file": if parts.len() > 1 { parts[0].clone() } else { f.path.clone() },
            "role": match target {
                "image" => classify_image_file(&f.path, Some(repo)),
                _ => classify_repo_file(&f.path, target),
            },
            "quant": quant_label(&f.path),
            "parts": parts.len(),
            "size_bytes": total,
            "size": hf::fmt_bytes(total),
        }));
    }
    if out.is_empty() {
        return Err(format!(
            "no {} files in {repo} matching that filter",
            hf::accepted_extensions_phrase(target)
        ));
    }
    Ok(json!({
        "repo": repo,
        "target": target,
        "files": out,
        "note": match target {
            "image" => "Roles are filename heuristics, not facts read out of the files: \
                        'diffusion' / 'checkpoint' is what loads the pipeline, 'vae', \
                        'text_encoder', 'lora' and 'upscaler' are its components. One image \
                        pipeline spans several repos, so prefer lmgw__image_recipes — it \
                        names every component of a known family with its repo, file and \
                        size — and use lmgw__hf_add target=image for a file a recipe does \
                        not cover.",
            _ => "Pass one 'weights' file to lmgw__hf_add; its mmproj and \
                  drafter companions are fetched with it by default.",
        },
    }))
}

/// Download a repo file and, by default, its companions.
///
/// This is the intended entry point for adding a model. The file-path route
/// (`lmgw__local_model_set` with a `gguf_path`) still exists for files lmgw
/// did not download, but it cannot fetch anything and gives no companion
/// discovery.
#[allow(clippy::too_many_arguments)]
pub async fn hf_add(
    state: &SharedState,
    repo: &str,
    file: Option<&str>,
    quant: Option<&str>,
    target: &str,
    companions: bool,
) -> Result<Value, String> {
    hf::validate_repo(repo)?;
    let target = hf::normalize_target(target)?;
    // Refused before the hub is asked, like an unset models dir.
    hf::models_dir_to_write(state, target)?;
    let snap = state.snapshot();
    let available = hf::list_repo_files(&state.http, &snap.settings.hf_token, repo).await?;
    let ggufs: Vec<&hf::HfFile> = available
        .iter()
        .filter(|f| hf::accepts_file(target, &f.path))
        .collect();
    // What "the model file" means per class: the chat classes have one
    // `weights` GGUF, an image repo has the diffusion model or the all-in-one
    // checkpoint that `-m` / `--diffusion-model` takes.
    let is_primary = |path: &str| match target {
        "image" => matches!(
            classify_image_file(path, Some(repo)),
            "diffusion" | "checkpoint"
        ),
        _ => classify_repo_file(path, target) == "weights",
    };

    // Resolve which weights file to fetch: explicit name, else by quant label.
    let chosen: String = match (file.map(str::trim).filter(|s| !s.is_empty()), quant) {
        (Some(f), _) => {
            if !available.iter().any(|a| a.path == f) {
                return Err(format!(
                    "{f} not found in {repo} — call lmgw__hf_repo to list it"
                ));
            }
            // The file is in the repo, but not every file in a repo is one
            // this class can load: a `.safetensors` written into the chat
            // models dir is a file llama-server will never open, discovered
            // as a missing GGUF at the first start.
            if !hf::accepts_file(target, f) {
                return Err(format!(
                    "{f} is not a file the {target} class loads — target={target} accepts {}. \
                     A stable-diffusion.cpp pipeline's files go to target=image.",
                    hf::accepted_extensions_phrase(target)
                ));
            }
            f.to_string()
        }
        (None, Some(q)) => {
            let q = q.to_ascii_uppercase();
            let hit = ggufs
                .iter()
                .filter(|f| is_primary(&f.path))
                .find(|f| f.path.to_ascii_uppercase().contains(&q))
                .ok_or_else(|| {
                    // The same per-target kind refusal the explicit `file=`
                    // branch gives: when this repo holds nothing the target
                    // can load, the quant is not what went wrong, and
                    // "available: (none)" sends the caller hunting for a
                    // label that does not exist.
                    if ggufs.is_empty() {
                        return format!(
                            "{repo} has no file the {target} class loads — target={target} \
                             accepts {}. A stable-diffusion.cpp pipeline's files go to \
                             target=image.",
                            hf::accepted_extensions_phrase(target)
                        );
                    }
                    let have: Vec<String> = ggufs
                        .iter()
                        .filter(|f| is_primary(&f.path))
                        .filter_map(|f| quant_label(&f.path))
                        .collect();
                    format!(
                        "no weights file matching quant '{q}'; available: {}",
                        have.join(", ")
                    )
                })?;
            hit.path.clone()
        }
        (None, None) => {
            let weights: Vec<String> = ggufs
                .iter()
                .filter(|f| is_primary(&f.path))
                .filter_map(|f| quant_label(&f.path))
                .collect();
            return Err(format!(
                "pass file or quant — this repo has these quants: {}",
                if weights.is_empty() {
                    "(none found)".into()
                } else {
                    weights.join(", ")
                }
            ));
        }
    };

    // Companions live beside the chosen file; a repo with several quants ships
    // exactly one projector/drafter shared by all of them.
    let dir = chosen.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let mut wanted = vec![chosen.clone()];
    let mut extras: Vec<Value> = Vec::new();
    // Companions are a llama.cpp repo's shape — a projector and a drafter
    // beside the weights. An image pipeline's components live in *other*
    // repos (§2.7), so there is nothing here to pull along and the recipe
    // verbs are what fetch a whole pipeline.
    if companions && target != "image" {
        for f in &ggufs {
            let same_dir = f.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("") == dir;
            let role = classify_repo_file(&f.path, target);
            if same_dir && matches!(role, "mmproj" | "drafter") && !wanted.contains(&f.path) {
                wanted.push(f.path.clone());
                extras.push(json!({ "file": f.path, "role": role }));
            }
        }
    }

    let queued = crate::web::hf::queue_files(state, repo, &wanted, target).await?;
    // Hand back the row ids and destination paths: without them a caller can
    // only re-list every download ever tracked and diff it to find its own.
    let tracked = store::list_hf_models(&state.db).await.unwrap_or_default();
    let queued_rows: Vec<Value> = tracked
        .iter()
        .filter(|r| r.repo == repo && wanted.contains(&r.file))
        .map(|r| {
            json!({
                "id": r.id, "file": r.file, "dest_path": r.dest_path, "status": r.status,
            })
        })
        .collect();
    // A sharded `.safetensors` is not a split GGUF: sd-server opens one file
    // and has no loader that reassembles shards, and lmgw will not guess the
    // sibling names for a format whose index it does not read. What was asked
    // for is queued, and the response says what that is worth.
    let mut warnings: Vec<String> = Vec::new();
    if let Some((n, total)) = hf::safetensors_shard(&chosen) {
        warnings.push(format!(
            "'{chosen}' is shard {n} of {total} of a sharded .safetensors — only this shard \
             was queued. stable-diffusion.cpp loads a single file per component and cannot \
             reassemble shards, so look for a single-file or GGUF conversion of this \
             component (lmgw__image_recipes names one for every family it ships)"
        ));
    }
    Ok(json!({
        "ok": true,
        "repo": repo,
        "weights": chosen,
        "companions": extras,
        "files_queued": queued,
        "downloads": queued_rows,
        "warnings": warnings,
        "gguf_path": hf::dest_rel_path(repo, &chosen).unwrap_or_default(),
        "dest_prefix": format!("{repo}/"),
        "target": target,
        "message": match target {
            "aux" => format!(
                "queued {queued} file(s) into the aux (embedding / rerank) models dir; poll \
                 lmgw__hf_downloads until status is 'done', then call lmgw__local_model_plan \
                 with gguf_path '{repo}/{chosen}' and target=aux — it returns the parameters \
                 lmgw__aux_model_set action=create takes"
            ),
            "audio" => format!(
                "queued {queued} file(s) into the audio models dir; poll lmgw__hf_downloads \
                 until status is 'done', then configure the model on the dashboard's Audio \
                 page (audio.cpp models need a family and task)"
            ),
            "image" => format!(
                "queued {queued} file(s) into the image models dir as '{repo}/{chosen}'; poll \
                 lmgw__hf_downloads until status is 'done'. An image pipeline needs more than \
                 this one file — its VAE and text encoders live in other repos — so either \
                 run lmgw__image_recipe_add for a known family (it queues every component and \
                 hands back a prefilled row) or call lmgw__local_model_plan target=image on \
                 this path to see which components a matching recipe still wants"
            ),
            _ => format!(
                "queued {queued} file(s); poll lmgw__hf_downloads until status is \
                 'done', then call lmgw__local_model_plan with gguf_path \
                 '{repo}/{chosen}' to get ready-to-apply parameters"
            ),
        },
    }))
}

/// Tracked downloads with live byte progress.
///
/// The rows are the durable `hf_models` records; the live columns come from the
/// `hf_download` jobs currently running (§9c). The shape is unchanged from when
/// downloads had their own registry — `lmgw__hf_downloads` is a documented
/// agent workflow ("poll until status is 'done'") and it keeps working
/// verbatim. `job_id` is additive, for cancelling.
pub async fn hf_downloads(state: &SharedState) -> Result<Value, String> {
    let rows = store::list_hf_models(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let active = state.jobs.live_of_kind(JobKind::HfDownload);
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            let key = jobs::hf_download::job_key(r.id);
            let live = active
                .iter()
                .find(|j| j.key.as_deref() == Some(key.as_str()));
            json!({
                "id": r.id,
                "repo": r.repo,
                "file": r.file,
                "dest_path": r.dest_path,
                "target": r.target,
                "status": r.status,
                "error": r.error,
                "size": r.size_bytes.map(|b| hf::fmt_bytes(b.max(0) as u64)),
                "downloaded_at": r.downloaded_at,
                "received_bytes": live.map(|j| j.done),
                "percent": live.and_then(|j| j.percent),
                "job_id": live.map(|j| j.id),
                // What the download asked for and what it got; null on a row
                // from before lmgw recorded them (unknown, not guessed).
                "requested_revision": r.requested_revision,
                "resolved_commit": r.resolved_commit,
            })
        })
        .collect();
    Ok(json!({ "downloads": items, "active": active.len() }))
}

/// Background jobs (§9c) — the poll half of the live feed. `limit <= 0` returns
/// everything retention has kept.
pub async fn jobs_list(
    state: &SharedState,
    kind: Option<&str>,
    active_only: bool,
    limit: i64,
) -> Result<Value, String> {
    let kind = match kind.map(str::trim).filter(|k| !k.is_empty()) {
        Some(k) => Some(JobKind::parse(k).ok_or_else(|| {
            format!(
                "unknown job kind '{k}' (expected {})",
                JobKind::ALL
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?),
        None => None,
    };
    let items = jobs::list(state, kind, active_only, limit).await?;
    Ok(json!({ "jobs": items, "active": state.jobs.live().len() }))
}

/// Ask a running job to stop (§9c). Cooperative — the executor finishes on its
/// own terms, so this reports the request, not the completion.
pub async fn job_cancel(state: &SharedState, id: i64) -> Result<Value, String> {
    let message = jobs::cancel(state, id).await?;
    Ok(json!({ "ok": true, "message": message }))
}

/// Re-fetch, untrack or update-check a tracked download.
pub async fn hf_set(
    state: &SharedState,
    action: &str,
    id: Option<i64>,
    target: &str,
) -> Result<Value, String> {
    let need_id = || id.ok_or_else(|| format!("'{action}' requires id (see lmgw__hf_downloads)"));
    match action {
        "redownload" => {
            let file = crate::web::hf::redownload(state, need_id()?).await?;
            Ok(json!({ "ok": true, "message": format!("re-downloading {file}") }))
        }
        "delete" => {
            crate::web::hf::delete_tracked(state, need_id()?).await?;
            Ok(json!({ "ok": true, "message": "entry untracked and file removed" }))
        }
        "cancel" => {
            let message = crate::web::hf::cancel_download(state, need_id()?).await?;
            Ok(json!({ "ok": true, "message": message }))
        }
        "check_updates" => {
            let (n, errors) =
                crate::web::hf::check_updates(state, hf::normalize_target(target)?).await;
            Ok(json!({ "ok": errors.is_empty(), "updates_available": n, "errors": errors }))
        }
        other => Err(format!(
            "unknown action '{other}' (redownload|cancel|delete|check_updates)"
        )),
    }
}
