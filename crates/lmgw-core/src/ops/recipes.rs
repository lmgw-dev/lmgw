//! Image recipes — "add from recipe" for a class whose pipeline spans repos

use lmgw_api_types as dto;
use serde_json::Value;

use crate::hf;
use crate::image_recipes;
use crate::state::SharedState;
use crate::store::{self};

/// The shipped recipe list, joined against this box: which components are on
/// disk, which are downloading, which rows already serve one
/// (image-generation design §7.2).
///
/// Read-only and offline — the list is compiled in, so this never touches the
/// hub. What it *does* touch is the filesystem (one `is_file` per component)
/// and the tracked-download table, because "add from recipe" has to be able to
/// say what is left to fetch.
pub async fn image_recipes(state: &SharedState) -> Result<Value, String> {
    let snap = state.snapshot();
    let models_dir = snap.settings.image.models_dir.clone();
    let tracked = store::list_hf_models_by_target(&state.db, "image")
        .await
        .unwrap_or_default();

    let recipes: Vec<dto::ImageRecipe> = image_recipes::all()
        .iter()
        .map(|r| {
            let components: Vec<dto::ImageRecipeComponent> = r
                .components
                .iter()
                .map(|c| {
                    let dest = c.dest_rel_path();
                    let row = tracked
                        .iter()
                        .find(|t| t.repo == c.repo && t.file == c.file);
                    let present = component_on_disk(&models_dir, &dest);
                    dto::ImageRecipeComponent {
                        role: c.role.to_string(),
                        repo: c.repo.to_string(),
                        file: c.file.to_string(),
                        dest_path: dest,
                        size_bytes: c.size_bytes,
                        size: hf::fmt_bytes(c.size_bytes),
                        gated: c.gated,
                        note: c.note.to_string(),
                        present,
                        downloading: row
                            .is_some_and(|t| matches!(t.status.as_str(), "queued" | "downloading")),
                        done: row.is_some_and(|t| t.status == "done"),
                        download_id: row.map(|t| t.id),
                        shared_with: image_recipes::shared_with(r.key, c.repo, c.file)
                            .into_iter()
                            .map(str::to_string)
                            .collect(),
                        alternatives: c
                            .alternatives
                            .iter()
                            .map(|a| dto::ImageRecipeAlternative {
                                file: a.file.to_string(),
                                size_bytes: a.size_bytes,
                                size: hf::fmt_bytes(a.size_bytes),
                                label: a.label.to_string(),
                                present: component_on_disk(
                                    &models_dir,
                                    &hf::dest_rel_path(c.repo, a.file).unwrap_or_default(),
                                ),
                            })
                            .collect(),
                    }
                })
                .collect();
            let installed = components.iter().all(|c| c.present);
            // The primary component is what a row points at, so that is what
            // "already served" asks about — a shared VAE proves nothing.
            let primary = r.primary().map(|c| c.dest_rel_path()).unwrap_or_default();
            let served = snap.image_models.iter().any(|m| {
                m.files.values().any(|v| {
                    v.as_str().is_some_and(|v| {
                        v.trim_start_matches("/models/").trim_start_matches('/') == primary
                    })
                })
            });
            dto::ImageRecipe {
                key: r.key.to_string(),
                display_name: r.display_name.to_string(),
                description: r.description.to_string(),
                args: r.args_json(),
                modes: r.modes.iter().map(|m| (*m).to_string()).collect(),
                edit: r.edit,
                vram_note: r.vram_note.to_string(),
                total_bytes: r.total_bytes(),
                total_size: hf::fmt_bytes(r.total_bytes()),
                partial: !installed && components.iter().any(|c| c.present || c.downloading),
                installed,
                served,
                suggested_model_id: r.suggested_model_id(),
                components,
            }
        })
        .collect();

    let view = dto::ImageRecipes {
        recipes,
        models_dir_missing: models_dir.trim().is_empty(),
        models_dir,
        hf_token_set: !snap.settings.hf_token.trim().is_empty(),
        next_step: "image_recipe_add key=<key> queues every component that is not already on \
                    disk and returns a prefilled row; poll hf_downloads until each queued id \
                    says 'done', then post that row at image_model_set action=create and \
                    finish with local_model_test target=image."
            .into(),
    };
    serde_json::to_value(view).map_err(|e| e.to_string())
}

/// Is this component's file already under the image models dir?
///
/// Separate from an `is_file` call only so the unconfigured-dir case cannot
/// be answered by accident: joining a relative path onto `""` resolves against
/// the process's cwd, which would report a file that has nothing to do with
/// this gateway.
fn component_on_disk(models_dir: &str, dest: &str) -> bool {
    if models_dir.trim().is_empty() || dest.is_empty() {
        return false;
    }
    std::path::Path::new(models_dir).join(dest).is_file()
}

/// Queue every component of a recipe that is not already on disk, and hand
/// back the row the editor is about to be filled with (§7.2).
///
/// Deliberately **not** a create: the files are still transferring, and
/// [`image_model_set`](crate::ops::image_model_set) refuses a row whose `files` do not resolve — which is
/// the check that keeps a broken row out of the table, so this must not go
/// around it. The caller polls `hf_downloads` and then posts `row` at
/// `image_model_set action=create`.
pub async fn image_recipe_add(
    state: &SharedState,
    key: &str,
    diffusion_file: Option<&str>,
) -> Result<Value, String> {
    let recipe = image_recipes::find(key).ok_or_else(|| {
        format!(
            "unknown recipe '{key}' — this build ships {}",
            image_recipes::keys().join(", ")
        )
    })?;
    let snap = state.snapshot();
    let models_dir = hf::models_dir_to_write(state, "image")?;
    let chosen = recipe.choose_files(diffusion_file)?;

    // A gated component with no token queues a download that fails at the
    // first byte, so refuse before anything is written — naming the component,
    // because "set a token" is useless without knowing which file needs one.
    if let Some(refusal) =
        recipe.gated_refusal(diffusion_file, !snap.settings.hf_token.trim().is_empty())
    {
        return Err(refusal);
    }

    let mut queued_rows: Vec<dto::QueuedDownload> = Vec::new();
    let mut already: Vec<String> = Vec::new();
    let mut in_flight: Vec<String> = Vec::new();
    let tracked_before = store::list_hf_models(&state.db).await.unwrap_or_default();
    for (c, file) in &chosen {
        let dest = hf::dest_rel_path(c.repo, file)?;
        if component_on_disk(&models_dir, &dest) {
            already.push(format!("{} ({dest})", c.role));
            continue;
        }
        // Asking for a component that is already on its way would `upsert` the
        // row back to `queued` *under* the job downloading it — the Downloads
        // page would then show a file as queued while it is being written, and
        // the second `start` is refused as already running anyway.
        if let Some(row) = tracked_before.iter().find(|t| {
            t.repo == c.repo
                && t.file == **file
                && t.target == "image"
                && matches!(t.status.as_str(), "queued" | "downloading")
        }) {
            in_flight.push(format!("{} ({dest}, {})", c.role, row.status));
            continue;
        }
        // One job per component through the path `hf_add` uses, so progress
        // shows up where every other download does.
        crate::web::hf::queue_files(state, c.repo, &[(*file).to_string()], "image").await?;
        let tracked = store::list_hf_models(&state.db).await.unwrap_or_default();
        if let Some(row) = tracked
            .iter()
            .find(|t| t.repo == c.repo && t.file == *file && t.target == "image")
        {
            queued_rows.push(dto::QueuedDownload {
                id: row.id,
                file: row.file.clone(),
                dest_path: row.dest_path.clone(),
                status: row.status.clone(),
            });
        }
    }

    let files = recipe.files_json(diffusion_file)?;
    // `local_model_test` draws a still, so it is only the last step for a row
    // that claims `img_gen`. A `vid_gen`-only pipeline (Wan) refuses that
    // request by design, and telling the caller to end there would hand them
    // an error as the happy path.
    let finish = match recipe.modes.contains(&"img_gen") {
        true => "and finish with local_model_test target=image",
        false => {
            "then start it with container model=<id> action=apply — local_model_test draws a \
             still, which this vid_gen-only row does not serve"
        }
    };
    let queued = queued_rows.len() as u32;
    let ids: Vec<String> = queued_rows.iter().map(|r| r.id.to_string()).collect();
    let result = dto::ImageRecipeAddResult {
        ok: true,
        key: recipe.key.to_string(),
        display_name: recipe.display_name.to_string(),
        files_queued: queued,
        downloads: queued_rows,
        already_present: already,
        already_queued: in_flight.clone(),
        row: dto::ImageRecipeRow {
            action: "create".into(),
            model_id: recipe.suggested_model_id(),
            files,
            args: recipe.args_json(),
            modes: recipe.modes.iter().map(|m| (*m).to_string()).collect(),
            edit: recipe.edit,
        },
        message: if queued == 0 && !in_flight.is_empty() {
            format!(
                "nothing new was queued for {}: {} already downloading or queued — poll \
                 lmgw__hf_downloads until every one says 'done', then post 'row' at \
                 image_model_set action=create",
                recipe.display_name,
                in_flight.join(", ")
            )
        } else if queued == 0 {
            format!(
                "every component of {} is already in the image models dir — post 'row' at \
                 image_model_set action=create, {finish}",
                recipe.display_name
            )
        } else {
            format!(
                "queued {queued} download(s) for {} (ids {}); poll lmgw__hf_downloads until \
                 every one says 'done', then post 'row' at image_model_set action=create \
                 {finish}. The row is not created here — image_model_set refuses files that \
                 are not on disk yet, and that check is what keeps a broken row out of the \
                 table.",
                recipe.display_name,
                ids.join(", ")
            )
        },
    };
    serde_json::to_value(result).map_err(|e| e.to_string())
}
