//! `image_upscaler_add {key}`: queue a shipped upscaler into
//! `upscalers/<file>` of the image models dir, through the download queue
//! every other file takes (it shows on the Downloads page, a retry or an
//! update re-fetches it to the same place).

use lmgw_api_types as dto;

use crate::hf;
use crate::jobs::hf_download;
use crate::state::SharedState;
use crate::store;

/// Queue the shipped upscaler `key` (module doc). A file already there, or
/// already on its way, is not queued again. Its download is a row of its
/// own (`upscalers/<file>`), beside an `hf_add` of the same repo file at
/// `<repo>/<file>` (migration 0086): neither moves the other.
pub async fn image_upscaler_add(
    state: &SharedState,
    key: &str,
) -> Result<dto::ImageUpscalerAddResult, String> {
    let u = super::find(key).ok_or_else(|| {
        format!(
            "unknown upscaler '{key}' — this build ships {}",
            super::SHIPPED
                .iter()
                .map(|u| u.key)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    let models_dir = hf::models_dir_to_write(state, "image")?;
    let snap = state.snapshot();
    let token_set = !snap.settings.hf_token.trim().is_empty();
    if u.gated && !token_set {
        return Err(format!("{}: {}", u.display_name, hf::gated_message(u.repo)));
    }
    let dest = u.dest_rel_path();
    let answer = |download, present, queued, message: String| dto::ImageUpscalerAddResult {
        ok: true,
        key: u.key.to_string(),
        download,
        already_present: present,
        already_queued: queued,
        message,
    };
    if super::present(&models_dir, u) {
        return Ok(answer(
            None,
            true,
            false,
            format!(
                "{} is already in the image models dir ({dest}): image__upscale and the \
                 card's Upscale offer it as '{}'",
                u.display_name,
                u.stem()
            ),
        ));
    }
    let tracked = store::list_hf_models_by_target(&state.db, "image")
        .await
        .map_err(|e| e.to_string())?;
    if let Some(row) = tracked.iter().find(|t| {
        t.repo == u.repo
            && t.file == u.file
            && t.dest_path == dest
            && matches!(t.status.as_str(), "queued" | "downloading")
    }) {
        return Ok(answer(
            None,
            false,
            true,
            format!(
                "{} is already {} (download {}): poll hf_downloads with ids [{}] until it says \
                 'done'",
                u.display_name, row.status, row.id, row.id
            ),
        ));
    }
    // The file must be in the repo as the entry names it: the hub's own
    // listing, as every queued download is checked.
    let available = hf::list_tree(
        &state.hub_http,
        &snap.settings.hf_token,
        &hf::hf_base(),
        u.repo,
        "main",
    )
    .await
    .map_err(|f| hf::listing_refusal(f, None))?;
    if !available.iter().any(|f| f.path == u.file) {
        return Err(format!("{} not found in {}", u.file, u.repo));
    }
    let id = store::upsert_hf_model_at(&state.db, u.repo, u.file, &dest, "image", "main")
        .await
        .map_err(|e| e.to_string())?;
    let row = store::get_hf_model(&state.db, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("download entry {id} for {} vanished", u.file))?;
    hf_download::start(state, &row).await?;
    let row = store::get_hf_model(&state.db, id)
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or(row);
    Ok(answer(
        Some(dto::QueuedDownload {
            id,
            file: row.file,
            dest_path: row.dest_path,
            status: row.status,
        }),
        false,
        false,
        format!(
            "queued {} ({}, {}) into {dest}; poll hf_downloads with ids [{id}] until it says \
             'done'. Then image__upscale and the card's Upscale offer it as '{}': an image \
             server already running finds it at its next upscale.",
            u.display_name,
            hf::fmt_bytes(u.size_bytes),
            u.licence,
            u.stem()
        ),
    ))
}
