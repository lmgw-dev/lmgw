//! What Settings › Documents says of the layout model, and its download.
//!
//! [`rows`] is the page's row: the same [`files::locate`] and the same
//! [`super::choice`] a conversion reads, so the page and a conversion
//! cannot disagree about whether the model is used. [`add`] is
//! `doc_model_add`: the pinned file queued through the download queue
//! every other model file takes (it shows on the Downloads page, with its
//! progress; the queue checks the bytes against the hub's SHA-256, and the
//! gateway checks them against the pinned one before the first load). A
//! download loads nothing and switches nothing on.

use lmgw_api_types::documents::models::{
    DocModelAddResult, DocModelLoaded, DocModelRow, DocModelState,
};

use super::files::{self, Located, ModelFile, HERON};
use super::session::{self, Now};
use super::{LAYOUT_IDLE_UNLOAD, LAYOUT_MEMORY_BYTES, LAYOUT_THREADS};
use crate::hf;
use crate::jobs::hf_download;
use crate::state::SharedState;
use crate::store::{self, HfModelRow};

/// The download target whose folder the file goes into (design §3).
const TARGET: &str = "aux";

/// The tracked download of `model`, when it was queued here.
fn tracked<'a>(model: &ModelFile, rows: &'a [HfModelRow]) -> Option<&'a HfModelRow> {
    let dest = model.dest_rel_path();
    rows.iter()
        .find(|t| t.repo == model.repo && t.file == model.file && t.dest_path == dest)
}

fn on_its_way(row: &HfModelRow) -> bool {
    matches!(row.status.as_str(), "queued" | "downloading")
}

/// The row of `model` from what is at its place and its tracked download.
pub(super) fn row(
    model: &ModelFile,
    located: Located,
    download: Option<&HfModelRow>,
    on: bool,
) -> DocModelRow {
    let downloading = download.is_some_and(on_its_way);
    let (state, path, message) = match located {
        Located::Found(p) => (DocModelState::Found, Some(p), None),
        Located::NoModelsDir => (
            DocModelState::NoFolder,
            None,
            Some(format!(
                "Choose where model files are stored first: {}",
                hf::models_dir_location(TARGET)
            )),
        ),
        // A download writes beside the file until it is whole.
        Located::Missing(p) | Located::Wrong(p, _) if downloading => {
            (DocModelState::Downloading, Some(p), None)
        }
        Located::Missing(p) => (
            DocModelState::Missing,
            Some(p),
            download
                .filter(|d| d.status == "failed")
                .and_then(|d| d.error.clone())
                .map(|e| format!("Its last download failed: {e}")),
        ),
        Located::Wrong(p, why) => (
            DocModelState::WrongFile,
            Some(p),
            Some(format!("The file at its place is not the model: {why}")),
        ),
    };
    let found = state == DocModelState::Found;
    let loaded = match session::now() {
        Now::NotLoaded => None,
        Now::Busy { threads } => Some(DocModelLoaded {
            threads: u32::try_from(threads).unwrap_or(u32::MAX),
            idle_secs: None,
        }),
        Now::Loaded { threads, idle } => Some(DocModelLoaded {
            threads: u32::try_from(threads).unwrap_or(u32::MAX),
            idle_secs: Some(idle.as_secs()),
        }),
    };
    DocModelRow {
        id: model.id.to_string(),
        label: model.label.to_string(),
        adds: model.adds.to_string(),
        used_for: vec!["PDF".to_string()],
        state,
        chosen: on,
        in_use: on && found,
        message,
        size_bytes: model.size_bytes,
        licence: model.licence.to_string(),
        repo: model.repo.to_string(),
        revision: model.revision.to_string(),
        file: model.file.to_string(),
        sha256: model.sha256.to_string(),
        url: model.url(),
        path: path.map(|p| p.display().to_string()),
        download_id: download.map(|d| d.id),
        memory_bytes: LAYOUT_MEMORY_BYTES,
        threads: LAYOUT_THREADS as u32,
        idle_unload_secs: LAYOUT_IDLE_UNLOAD.as_secs(),
        loaded,
    }
}

/// The page's model rows.
pub async fn rows(state: &SharedState) -> Vec<DocModelRow> {
    let choice = super::choice();
    let downloads = store::list_hf_models_by_target(&state.db, TARGET)
        .await
        .unwrap_or_default();
    let located = files::locate(&HERON, &choice.models_dir);
    vec![row(&HERON, located, tracked(&HERON, &downloads), choice.on)]
}

/// Whether "Use the layout model" can be switched on: its file is found.
/// The refusal says what to do first.
pub fn can_switch_on(models_dir: &str) -> Result<(), String> {
    match files::locate(&HERON, models_dir) {
        Located::Found(_) => Ok(()),
        Located::NoModelsDir => Err(format!(
            "the layout model cannot be switched on: no folder for model files is set ({}), so \
             its file cannot be here",
            hf::models_dir_location(TARGET)
        )),
        Located::Missing(_) => Err(format!(
            "the layout model cannot be switched on: its file is not here yet. Download it \
             first (Settings → Documents, or doc_model_add id={}; {}, {})",
            HERON.id,
            hf::fmt_bytes(HERON.size_bytes),
            HERON.licence
        )),
        Located::Wrong(_, why) => Err(format!(
            "the layout model cannot be switched on: the file at its place is not the model \
             ({why})"
        )),
    }
}

/// `doc_model_add {id}` (module doc).
pub async fn add(state: &SharedState, id: &str) -> Result<DocModelAddResult, String> {
    let model = &HERON;
    if id.trim() != model.id {
        return Err(format!(
            "unknown model '{id}' — the model files lmgw can use for converting: {}",
            model.id
        ));
    }
    let models_dir = hf::models_dir_to_write(state, TARGET)?;
    let answer = |download_id, present, queued, message: String| DocModelAddResult {
        ok: true,
        id: model.id.to_string(),
        download_id,
        already_present: present,
        already_queued: queued,
        message,
    };
    let dest = model.dest_rel_path();
    if let Located::Found(path) = files::locate(model, &models_dir) {
        return Ok(answer(
            None,
            true,
            false,
            format!(
                "{} is already here ({}); switch it on under Settings → Documents",
                model.label,
                path.display()
            ),
        ));
    }
    let downloads = store::list_hf_models_by_target(&state.db, TARGET)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(d) = tracked(model, &downloads).filter(|d| on_its_way(d)) {
        return Ok(answer(
            Some(d.id),
            false,
            true,
            format!(
                "{} is already {} (download {}): poll hf_downloads with ids [{}] until it says \
                 'done'",
                model.label, d.status, d.id, d.id
            ),
        ));
    }
    // The file must be in the repository at the pinned commit as the pin
    // names it: the hub's own listing, as every queued download is checked.
    let token = state.snapshot().settings.hf_token.clone();
    let listed = hf::list_tree(
        &state.hub_http,
        &token,
        &hf::hf_base(),
        model.repo,
        model.revision,
    )
    .await
    .map_err(|f| hf::listing_refusal(f, None))?;
    let Some(file) = listed.iter().find(|f| f.path == model.file) else {
        return Err(format!(
            "{} not found in {} at {}",
            model.file, model.repo, model.revision
        ));
    };
    if file.oid().is_some_and(|oid| oid != model.sha256) {
        return Err(format!(
            "{} in {} at {} is not the file lmgw pins (the hub names another SHA-256); nothing \
             was downloaded",
            model.file, model.repo, model.revision
        ));
    }
    let row_id = store::upsert_hf_model_at(
        &state.db,
        model.repo,
        model.file,
        &dest,
        TARGET,
        model.revision,
    )
    .await
    .map_err(|e| e.to_string())?;
    let row = store::get_hf_model(&state.db, row_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("download entry {row_id} for {} vanished", model.file))?;
    hf_download::start(state, &row).await?;
    Ok(answer(
        Some(row_id),
        false,
        false,
        format!(
            "queued {} ({}, {}) into {dest}; poll hf_downloads with ids [{row_id}] until it \
             says 'done', then switch it on under Settings → Documents (settings_set \
             documents_onnx_layout=true). The download loads nothing.",
            model.label,
            hf::fmt_bytes(model.size_bytes),
            model.licence
        ),
    ))
}
