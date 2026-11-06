//! Completing an upload (device transfers design §4, I10, I25): once a
//! transfer's file holds every byte, it is synced and hashed, and the
//! transfer is `ready` (offered to the receiver) or `failed` (its bytes
//! deleted). Whoever holds the upload slot and finds the file full does it:
//! the upload that wrote its last byte, any later upload of it whatever its
//! range, and the sweep (at start and hourly) for one nobody finished.

use lmgw_api_types::transfer::TransferState;

use super::{end_holding, said};
use crate::state::AppState;
use crate::store::transfers::{self as table, EndsFrom, TransferRow};

/// What completing an upload came to.
#[derive(Debug)]
pub(crate) enum Completed {
    /// The sha256 matched: `ready`, offered to the receiver.
    Ready,
    /// It did not, the sha256 lmgw read: `failed`, the bytes deleted.
    Failed { sha: String },
    /// It was not uploading any more: what it is now, when there is a row.
    Moved(Option<Box<TransferRow>>),
}

/// The sentence a failed transfer's `end_reason` keeps (I17); the refusal
/// names both hashes.
pub(crate) const MISMATCH_REASON: &str =
    "the uploaded bytes did not match the sha256 the offer named";

/// Complete `row`, whose file holds every byte, by the one that holds its
/// upload slot. `Err` with what failed (the sync or the hash: the transfer
/// stays `uploading`, and the next upload or the sweep tries again).
pub(crate) async fn complete_holding(
    state: &AppState,
    row: &TransferRow,
) -> Result<Completed, String> {
    let sha = super::content::hash_file(&state.data_dir, &row.id)
        .await
        .map_err(|e| format!("hashing the transfer's file failed: {e}"))?;
    if sha == row.sha256 {
        return match table::mark_ready(&state.db, &row.id, chrono::Utc::now()).await {
            Ok(Some(ready)) => {
                said(state, &ready);
                Ok(Completed::Ready)
            }
            Ok(None) => table::get(&state.db, &row.id)
                .await
                .map(|now| Completed::Moved(now.map(Box::new)))
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
    }
    match end_holding(
        state,
        &row.id,
        EndsFrom::Uploading,
        TransferState::Failed,
        Some(MISMATCH_REASON),
    )
    .await
    {
        Ok(Some(_)) => Ok(Completed::Failed { sha }),
        Ok(None) => table::get(&state.db, &row.id)
            .await
            .map(|now| Completed::Moved(now.map(Box::new)))
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}
