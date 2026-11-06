//! The transfers' step of the hourly maintenance tick (device transfers
//! design §3, §4, §7, §8), beside the Chat's (`server::chat_upkeep`). The
//! tick's first pass runs as lmgw starts.
//!
//! 1. every transfer in progress past its `expires_at` expires, its bytes
//!    deleted (a request that touches one expires it at once too, I18);
//! 2. every transfer in progress whose device row is gone or disabled is
//!    canceled — what a crash between a key's write and its transfers' end
//!    left open; and every one offered to a device that declares another
//!    app id now (I28);
//! 3. every upload whose file holds every byte is completed (I25): one
//!    whose last write was not followed by its completion (lmgw stopped,
//!    the hash failed);
//! 4. ended rows older than `chat_feed_retention_days` (by `ended_at`) are
//!    deleted, `0` keeping them (I15);
//! 5. a file whose name is a transfer id and whose transfer is not in
//!    progress is deleted (a crash between an end's commit and its unlink);
//!    no other name in the directory is touched.

use chrono::Utc;
use lmgw_api_types::transfer::TransferState;

use super::{Completed, Taker};
use crate::state::SharedState;
use crate::store::transfers::{self as table, EndsFrom};

/// One pass. Public for the suites that run the tick's step themselves.
pub async fn upkeep(state: &SharedState) {
    match table::overdue(&state.db, Utc::now()).await {
        Ok(ids) => {
            for id in ids {
                let row = match table::get(&state.db, &id).await {
                    Ok(Some(row)) => row,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!("transfer {id}: reading it failed ({e})");
                        continue;
                    }
                };
                match super::expire(state, &row).await {
                    Ok(Some(_)) => tracing::info!(
                        "transfer {id} from '{}' to '{}' expired undelivered; its bytes are \
                         deleted",
                        row.from_name,
                        row.to_name
                    ),
                    Ok(None) => {}
                    Err(e) => tracing::warn!("transfer {id}: expiring it failed ({e})"),
                }
            }
        }
        Err(e) => tracing::warn!("transfers: reading the overdue ones failed ({e})"),
    }
    match table::orphaned(&state.db).await {
        Ok(left) => {
            for (id, key_id, deleted) in left {
                let name = match table::get(&state.db, &id).await {
                    Ok(Some(r)) if r.from_key_id == key_id => r.from_name,
                    Ok(Some(r)) => r.to_name,
                    _ => continue,
                };
                let why = super::ended_reason(&name, deleted);
                if let Err(e) = super::end(
                    state,
                    &id,
                    EndsFrom::Active,
                    TransferState::Canceled,
                    Some(&why),
                )
                .await
                {
                    tracing::warn!("transfer {id}: ending it failed ({e})");
                }
            }
        }
        Err(e) => tracing::warn!("transfers: reading the orphaned ones failed ({e})"),
    }
    match table::mismatched(&state.db, None).await {
        Ok(left) => {
            for (id, app_id) in left {
                let name = match table::get(&state.db, &id).await {
                    Ok(Some(r)) => r.to_name,
                    _ => continue,
                };
                let why = super::app_id_reason(&name, app_id.as_deref());
                if let Err(e) = super::end(
                    state,
                    &id,
                    EndsFrom::Active,
                    TransferState::Canceled,
                    Some(&why),
                )
                .await
                {
                    tracing::warn!("transfer {id}: ending it failed ({e})");
                }
            }
        }
        Err(e) => tracing::warn!("transfers: reading the ones of another app id failed ({e})"),
    }
    full_uploads(state).await;
    let days = state.snapshot().settings.chat_feed_retention_days;
    match table::prune(&state.db, days, Utc::now()).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            "pruned {n} ended transfers older than {days} days (Settings → Chat → Change feed → \
             retention)"
        ),
        Err(e) => tracing::warn!("transfers: pruning the ended ones failed ({e})"),
    }
    stray_files(state).await;
}

/// Step 3: the uploads whose file holds every byte, completed under their
/// slot when nobody holds it or waits for it: an upload that holds it
/// completes the file itself, and an end ends it (I25). Nothing that runs
/// is stopped for this (2026-10-22): what is left is the start's pass and a
/// full file nobody touches, which the next tick finds again.
async fn full_uploads(state: &SharedState) {
    let active = match table::active(&state.db).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("transfers: reading the ones in progress failed ({e})");
            return;
        }
    };
    for t in active.into_iter().filter(|t| t.state == "uploading") {
        let size = u64::try_from(t.size).unwrap_or(u64::MAX);
        if super::len(&state.data_dir, &t.id).await < size {
            continue;
        }
        let Some(held) = state.transfers.uploads.try_take(&t.id) else {
            continue;
        };
        let row = match table::get(&state.db, &t.id).await {
            Ok(Some(r)) if r.state == "uploading" => r,
            _ => continue,
        };
        if super::len(&state.data_dir, &row.id).await < size {
            continue;
        }
        match super::complete_holding(state, &row).await {
            Ok(Completed::Ready) => tracing::info!(
                "transfer {} from '{}' to '{}' held every byte without being completed; it is \
                 ready now",
                row.id,
                row.from_name,
                row.to_name
            ),
            Ok(Completed::Failed { sha }) => tracing::info!(
                "transfer {} held every byte without being completed; its sha256 {sha} is not \
                 the offer's, so it failed and its bytes are deleted",
                row.id
            ),
            Ok(Completed::Moved(_)) => {}
            Err(e) => tracing::warn!("transfer {}: completing it failed ({e})", row.id),
        }
        drop(held);
    }
}

/// Step 5: the files no transfer in progress names.
async fn stray_files(state: &SharedState) {
    let on_disk = super::content::files(&state.data_dir).await;
    if on_disk.is_empty() {
        return;
    }
    let active = match table::active(&state.db).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("transfers: reading the ones in progress failed ({e})");
            return;
        }
    };
    for id in on_disk {
        if active.iter().any(|t| t.id == id) {
            continue;
        }
        // Its slot first: an upload of a transfer created after the list
        // above was read writes nothing while this holds it, and one that
        // ended meanwhile is gone from it.
        let held = state.transfers.uploads.take(&id, Taker::End).await;
        let still = matches!(table::get(&state.db, &id).await, Ok(Some(r)) if r.is_active());
        if still {
            continue;
        }
        tracing::info!("transfers: deleting the file of {id}, which no transfer in progress names");
        state.transfers.downloads.close(&id);
        super::unlink(&state.data_dir, &id).await;
        drop(held);
    }
}
