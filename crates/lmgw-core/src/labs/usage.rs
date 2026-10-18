//! What a lab keeps (labs-store design §1.5): the counts and sizes the
//! results head shows, and the artifacts directory's orphans. No retention
//! rule: nothing is deleted by age or count, and the size is in view.

use std::path::Path;

use lmgw_api_types::lab_store::{LabKind, LabUsage};
use sqlx::SqlitePool;

use super::artifacts;

/// `lab`'s usage, with the orphans of the whole artifacts directory (no row
/// of any lab names them, so they are no one lab's).
pub async fn usage(pool: &SqlitePool, data_dir: &Path, lab: LabKind) -> Result<LabUsage, String> {
    let counts = crate::store::lab_counts(pool, lab)
        .await
        .map_err(|e| e.to_string())?;
    let orphans = {
        let _held = artifacts::hold().await;
        artifacts::orphans(pool, data_dir).await?
    };
    Ok(LabUsage {
        lab,
        groups: counts.groups,
        batches: counts.batches,
        runs: counts.runs,
        artifacts: counts.artifacts,
        artifact_files: counts.artifact_files,
        text_bytes: counts.text_bytes,
        artifact_bytes: counts.artifact_bytes,
        orphan_files: orphans.files,
        orphan_bytes: orphans.bytes,
    })
}
