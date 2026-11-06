//! A Hugging Face file's fit, and the pick among a repo's files, before any
//! download (find-a-model design §3, D7, D8, D120): what the file list's
//! marks (WP3), the finder's card (WP4) and Get it (WP5) ask.
//!
//! A file's fit is its hub plan's (`local_model_plan` with `repo` + `file`):
//! the same plan and the same fit the file gets once it is on disk. A file
//! whose header can't be read, or that can't be planned (a target the hub
//! plan doesn't read headers for), is marked by its listed size alone (D8),
//! with the reason in its notes, and is never the pick; a file whose size
//! the listing doesn't give, or a split file with a part missing from it,
//! is `Unknown`, never a fit. Which files' headers are
//! read is the caller's: each read costs 1 to 16 MiB (§2.2), and the file
//! list reads only the candidates near the card's edge ([`marks`], §3.3).

use lmgw_api_types::model_find::{FilePick, FitMeasure, PlanFit};

use crate::hf::{self, HfFile};
use crate::runtime::Class;
use crate::state::SharedState;

use super::fit::{self, CardNow};
use super::pick::{self, Candidate};
use super::plan_files::PlanFiles;

pub mod marks;

/// One file's hub plan and its fit by both measures.
#[derive(Debug, Clone)]
pub struct FileFits {
    /// The file's path in the repo.
    pub file: String,
    /// Every part's bytes, from the listing; `None` when the listing lacks
    /// a part or a size, and then the mark is `Unknown`.
    pub bytes: Option<u64>,
    /// The plan's input, when the header was read.
    pub files: Option<PlanFiles>,
    pub fit: PlanFit,
    /// The header bytes this file's plan received from the hub, a read that
    /// failed after it began included; a header the process had read before
    /// adds nothing (D123).
    pub read_bytes: u64,
    /// Why the hub refused, when it refused as it would every read of the
    /// repo (a gated repo, its rate limit, no connection; D66): a caller
    /// reading several files stops there.
    pub hub_failure: Option<String>,
}

/// The fit of `file` in `repo` (module doc), `listing` being the repo's tree
/// at `main`; `target` as `local_model_plan` takes it.
pub async fn file_fit(
    state: &SharedState,
    repo: &str,
    file: &str,
    listing: &[HfFile],
    target: Option<&str>,
) -> FileFits {
    let snap = state.snapshot();
    let card = CardNow::of(state, &snap).await;
    file_fit_planned(state, repo, file, listing, target, card.as_ref())
        .await
        .0
}

/// [`file_fit`] on `card`, a reading of the card the caller took, with the
/// plan's parameters when the header was read: what the file list sizes the
/// repo's other files with ([`marks`]), every file of one answer on one
/// reading (D66).
pub(crate) async fn file_fit_planned(
    state: &SharedState,
    repo: &str,
    file: &str,
    listing: &[HfFile],
    target: Option<&str>,
    card: Option<&CardNow>,
) -> (FileFits, Option<serde_json::Map<String, serde_json::Value>>) {
    let listed = super::plan_files::hub::listed_bytes(listing, file);
    let snap = state.snapshot();
    // The target is checked before any read: a class the hub plan doesn't
    // read headers for costs nothing to refuse.
    let read = match super::hub_class(target) {
        Err(e) => Err(hf::ReadError::file(e)),
        Ok(class) => {
            match super::plan_files::hub::from_hub(state, repo, file, listing, class).await {
                Ok(files) => {
                    super::plan_from_on(state, &snap, files.found_in, &files, card.cloned())
                        .await
                        .map(|plan| (files, plan))
                        .map_err(hf::ReadError::file)
                }
                Err(e) => Err(e),
            }
        }
    };
    match read {
        Ok((files, plan)) => (
            FileFits {
                file: file.to_string(),
                bytes: listed.bytes,
                fit: serde_json::from_value(plan["fit"].clone()).unwrap_or_default(),
                read_bytes: files.source_dto().header_bytes_read,
                files: Some(files),
                hub_failure: None,
            },
            plan["params"].as_object().cloned(),
        ),
        Err(e) => {
            // Where `hf_add` puts it, so a loaded row on the file isn't
            // counted beside it; the header's class isn't known here.
            let class = super::hub_class(target)
                .ok()
                .flatten()
                .unwrap_or(Class::Chat);
            let card = match hf::dest_rel_path(repo, file) {
                Ok(rel) => card.cloned().map(|c| c.beside_itself(&snap, class, &rel)),
                Err(_) => card.cloned(),
            };
            let why = format!("marked by its size alone: {}", e.message);
            let by = |m| {
                let mut f = fit::by_size(listed.bytes, card.as_ref(), m, why.clone());
                f.notes.extend(listed.missing.clone());
                f
            };
            (
                FileFits {
                    file: file.to_string(),
                    bytes: listed.bytes,
                    files: None,
                    fit: PlanFit {
                        card: card.as_ref().map(CardNow::dto),
                        whole_card: by(FitMeasure::WholeCard),
                        beside: by(FitMeasure::Beside),
                    },
                    read_bytes: e.received,
                    hub_failure: e.hub.then(|| e.message.clone()),
                },
                None,
            )
        }
    }
}

/// The pick among `fits` by both measures (D7, D120): the whole card's, and
/// beside what runs now. The whole card's capacity is the first fit's card.
pub fn pick_both(fits: &[FileFits]) -> (FilePick, FilePick) {
    let card_bytes = fits
        .iter()
        .find_map(|f| f.fit.card.as_ref().map(|c| c.capacity_bytes))
        .unwrap_or(0);
    let cands = |m: FitMeasure| -> Vec<Candidate> {
        fits.iter()
            .map(|f| Candidate {
                file: f.file.clone(),
                quant: hf::quant_label(&f.file),
                bytes: f.bytes.unwrap_or(0),
                fit: match m {
                    FitMeasure::WholeCard => f.fit.whole_card.clone(),
                    FitMeasure::Beside => f.fit.beside.clone(),
                },
            })
            .collect()
    };
    (
        pick::pick(
            &cands(FitMeasure::WholeCard),
            FitMeasure::WholeCard,
            card_bytes,
        ),
        pick::pick(&cands(FitMeasure::Beside), FitMeasure::Beside, card_bytes),
    )
}
