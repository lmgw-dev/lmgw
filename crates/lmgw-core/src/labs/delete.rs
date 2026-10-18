//! Deleting a group or a set run, or clearing a lab (labs-store design §1.4,
//! §1.5, §3.1): in one transaction the rows go (runs and artifact rows by
//! cascade, a set run's items with it) and the files no remaining row names
//! are found; after the commit those files are unlinked. Under the artifacts
//! lock, so an ingest cannot name a file in between.

use std::path::Path;

use lmgw_api_types::lab_store::LabKind;
use sqlx::SqlitePool;

use super::artifacts;
use crate::store;

/// Delete group `group` of `lab`, or every group and set run of `lab` when
/// `None`. How many groups went (0: there was none to delete).
pub async fn delete(
    pool: &SqlitePool,
    data_dir: &Path,
    lab: LabKind,
    group: Option<i64>,
) -> Result<u64, String> {
    let _held = artifacts::hold().await;
    let gone = {
        let mut tx = store::begin_write(pool).await.map_err(|e| e.to_string())?;
        let shas = store::lab_artifact_shas(&mut tx, lab, group)
            .await
            .map_err(|e| e.to_string())?;
        if group.is_none() {
            store::delete_lab_batches(&mut tx, lab, None)
                .await
                .map_err(|e| e.to_string())?;
        }
        let n = store::delete_lab_groups(&mut tx, lab, group)
            .await
            .map_err(|e| e.to_string())?;
        let unnamed = store::unnamed_lab_shas(&mut tx, &shas)
            .await
            .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        (n, unnamed)
    };
    artifacts::unlink(data_dir, &gone.1).await;
    Ok(gone.0)
}

/// Delete set run `batch` of `lab` with its items, their runs and artifact
/// rows, then the files no other row names. How many set runs went (0: there
/// was none).
pub async fn delete_batch(
    pool: &SqlitePool,
    data_dir: &Path,
    lab: LabKind,
    batch: i64,
) -> Result<u64, String> {
    let _held = artifacts::hold().await;
    let gone = {
        let mut tx = store::begin_write(pool).await.map_err(|e| e.to_string())?;
        let shas = store::lab_batch_artifact_shas(&mut tx, batch)
            .await
            .map_err(|e| e.to_string())?;
        let n = store::delete_lab_batches(&mut tx, lab, Some(batch))
            .await
            .map_err(|e| e.to_string())?;
        let unnamed = store::unnamed_lab_shas(&mut tx, &shas)
            .await
            .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        (n, unnamed)
    };
    artifacts::unlink(data_dir, &gone.1).await;
    Ok(gone.0)
}
