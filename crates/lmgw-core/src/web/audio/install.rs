//! A package's files against what is on this machine (audio-class gap 4).
//!
//! A spec's file list grows: audio.cpp added Pocket TTS's English
//! `embeddings/alba.safetensors` a week after the owner's package was
//! downloaded, and nothing said the package now lacked its built-in voice.
//! The catalog view names the files a package lacks — no `done` download
//! row, or one whose file is no longer on disk — and a download queues only
//! those, so "complete install" fetches one file instead of the package
//! again.

use std::collections::HashSet;
use std::path::Path;

use crate::state::SharedState;
use crate::store::HfModelRow;

/// A row whose file was downloaded: `done`, or `update_available` (on
/// disk, with a newer one upstream).
fn downloaded(row: &HfModelRow) -> bool {
    matches!(row.status.as_str(), "done" | "update_available")
}

/// The tracked, downloaded rows whose file is not under `models_dir` (the
/// audio class's) any more. Blocking: one `stat` per row.
pub(super) fn absent_on_disk(models_dir: &str, rows: &[HfModelRow]) -> HashSet<i64> {
    if models_dir.trim().is_empty() {
        return HashSet::new();
    }
    let dir = Path::new(models_dir);
    rows.iter()
        .filter(|r| downloaded(r) && !dir.join(&r.dest_path).is_file())
        .map(|r| r.id)
        .collect()
}

/// The files of a package (`files`, the spec's list) that are not installed,
/// given the package's tracked `rows`: no downloaded row, or one in
/// `absent`.
pub(super) fn missing_files(
    files: &[String],
    rows: &[&HfModelRow],
    absent: &HashSet<i64>,
) -> Vec<String> {
    files
        .iter()
        .filter(|f| {
            !rows
                .iter()
                .any(|r| &r.file == *f && downloaded(r) && !absent.contains(&r.id))
        })
        .cloned()
        .collect()
}

/// The package's installed files (downloaded, still on disk) whose row was
/// fetched at another revision than `revision` — the pin a download takes
/// now. `NULL` reads as `main` ([`HfModelRow::revision`]). A row still on
/// its way is left to its transfer.
pub(super) fn at_another_revision(
    rows: &[&HfModelRow],
    absent: &HashSet<i64>,
    revision: &str,
) -> Vec<String> {
    rows.iter()
        .filter(|r| downloaded(r) && !absent.contains(&r.id) && r.revision() != revision)
        .map(|r| r.file.clone())
        .collect()
}

/// Of `stale` — the package's installed files from another revision than
/// the pin ([`at_another_revision`]) — the ones whose bytes the pin
/// changed, and the ones it did not. A pin moves for the package as a
/// whole, and most of its files are often the same at both commits: a file
/// whose ETag at the pin (one HEAD) is the one its row recorded is the
/// pin's file already, so its row records the pin ([`record_at_pin`]) and
/// the bytes are not fetched again. A file the hub names no ETag for, or
/// does not answer for, is fetched: "cannot tell" is not "the same", and
/// the transfer then says what the hub said. `(changed, recorded)`.
///
/// The HEADs go out together, one per file: the click waits on them before
/// anything is queued, and a hub that does not answer then costs one
/// timeout rather than one per file.
pub(super) async fn changed_at_pin(
    state: &SharedState,
    rows: &[&HfModelRow],
    stale: Vec<String>,
    revision: &str,
) -> (Vec<String>, Vec<String>) {
    let token = state.snapshot().settings.hf_token.clone();
    let token = token.as_str();
    // Each file with its row when the pin has the row's bytes.
    let asked = futures::future::join_all(stale.into_iter().map(|file| async move {
        let row = rows
            .iter()
            .copied()
            .find(|r| r.file == file && downloaded(r));
        let same = match row {
            Some(row) => {
                let remote =
                    crate::hf::remote_etag(&state.http, token, &row.repo, revision, &row.file)
                        .await;
                matches!((&row.etag, &remote), (Some(l), Ok(Some(r))) if l == r)
            }
            None => false,
        };
        (file, row.filter(|_| same))
    }))
    .await;
    let (mut changed, mut recorded) = (Vec::new(), Vec::new());
    for (file, same) in asked {
        if let Some(row) = same {
            match record_at_pin(state, row, revision).await {
                Ok(()) => {
                    recorded.push(file);
                    continue;
                }
                Err(e) => tracing::warn!(
                    "recording {}/{} at {revision}, where its bytes are unchanged: {e} — it is \
                     fetched again instead",
                    row.repo,
                    row.file
                ),
            }
        }
        changed.push(file);
    }
    (changed, recorded)
}

/// Records the pin on a row whose file has the same bytes there
/// ([`crate::hf::record_matched`]), and takes back an `update_available`
/// flag, as the update check does on a match: the flag came from a check
/// against another revision (one under `latest`), and Update would fetch
/// the same bytes again.
async fn record_at_pin(
    state: &SharedState,
    row: &HfModelRow,
    revision: &str,
) -> Result<(), String> {
    crate::hf::record_matched(state, row, revision).await?;
    if row.status == "update_available" {
        crate::store::set_hf_status(&state.db, row.id, "done", None)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// A row still on its way (queued or downloading).
pub(super) fn in_flight(row: &HfModelRow) -> bool {
    matches!(row.status.as_str(), "queued" | "downloading")
}
