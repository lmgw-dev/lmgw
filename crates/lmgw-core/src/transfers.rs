//! Device-to-device transfers (device transfers design): a paired device
//! offers a file to another paired device of the same app, relayed through
//! lmgw's disk. lmgw knows an app by the id it declared at pairing (§2),
//! never what the file means.
//!
//! - **The rows** are `store::transfers`: every state change one write
//!   transaction, conditional on the state it changes from, with its
//!   `transfer.updated` feed record (§6).
//! - **The files** (`content`): one per transfer under
//!   `<data_dir>/transfers/`, its length the upload's resume point (§3).
//! - **The upload slot** (`uploads`): one writer per transfer; a newer
//!   upload, and every end, stops the one that writes (I9).
//! - **The downloads** (`downloads`): the files the download bodies read,
//!   held by the registry every end closes (I26).
//! - **The ends** ([`end`]): each runs as a task of its own, so its commit,
//!   its unlink, its downloads' close and its feed record happen together
//!   even when the request that asked for it goes (I30).
//! - **Completion** (`complete`): a file that holds every byte is hashed
//!   and the transfer `ready` or `failed`, by whoever finds it full (I25).
//! - **The sweep** (`sweep`): the hourly tick's expiry, the ends a device
//!   that went or changed its app id leaves, the uploads left full, the
//!   ended rows' retention and the stray files (§7, §8).
//!
//! The routes are `web::transfers`; the feed's rendering is
//! `web::chat_feed::transfers`.

mod complete;
mod content;
mod downloads;
mod sweep;
mod uploads;

pub(crate) use complete::{complete_holding, Completed};
pub use content::{dir, is_id, new_id};
pub(crate) use content::{len, open_read, open_write, sync, synced_len, unlink, write_at};
pub use sweep::upkeep;
pub(crate) use uploads::{Held, Taker};

use chrono::Utc;
use lmgw_api_types::transfer::{Transfer, TransferParty, TransferState};

use crate::error::GatewayError;
use crate::state::{AppState, SharedState};
use crate::store::transfers::{self as table, Ending, EndsFrom, TransferRow};
use crate::store::DbResult;

/// What the gateway keeps about transfers while it runs: the upload slots,
/// the downloads' files, the offers' lock and, for a test, the free space
/// it stands in.
#[derive(Debug, Default)]
pub struct Transfers {
    pub(crate) uploads: uploads::Uploads,
    pub(crate) downloads: downloads::Downloads,
    /// Held across an offer's space check and its insert (§3): two offers
    /// never both take the same free space.
    pub(crate) offers: tokio::sync::Mutex<()>,
    /// A test's free space, in place of `statvfs`'s.
    free_override: std::sync::Mutex<Option<u64>>,
}

impl Transfers {
    /// How many upload slots are kept: one per transfer someone holds or
    /// waits for (I24), so a suite sees that none is left behind.
    #[doc(hidden)]
    pub fn upload_slots_for_tests(&self) -> usize {
        self.uploads.len()
    }

    /// How many files the downloads hold open: one per download in
    /// progress whose transfer did not end (I26).
    #[doc(hidden)]
    pub fn downloads_open_for_tests(&self) -> usize {
        self.downloads.len()
    }

    /// Stand `bytes` in for the data directory's free space (`None`: the
    /// filesystem's again), so a suite can fill the disk without filling
    /// it.
    #[doc(hidden)]
    pub fn set_free_bytes_for_tests(&self, bytes: Option<u64>) {
        *self.free_override.lock().unwrap_or_else(|e| e.into_inner()) = bytes;
    }

    /// The data directory's filesystem's free space for an unprivileged
    /// writer (`backends::paths::free_bytes`); `0`, with a warning, when no
    /// ancestor of it can be asked, so every offer is refused with that.
    pub(crate) fn free_bytes(&self, data_dir: &std::path::Path) -> u64 {
        if let Some(b) = *self.free_override.lock().unwrap_or_else(|e| e.into_inner()) {
            return b;
        }
        crate::backends::paths::free_bytes(data_dir).unwrap_or_else(|| {
            tracing::warn!(
                "transfers: the free space of {} cannot be read (statvfs, on Windows \
                 GetDiskFreeSpaceExW); offers are refused as if the disk were full",
                data_dir.display()
            );
            0
        })
    }
}

/// The largest offer lmgw takes now (§3): the free space less what every
/// upload in progress still needs (its size less its file's length).
pub(crate) async fn available(state: &AppState) -> DbResult<u64> {
    let free = state.transfers.free_bytes(&state.data_dir);
    let mut owed = 0u64;
    for t in table::active(&state.db).await? {
        if t.state == "uploading" {
            let have = len(&state.data_dir, &t.id).await;
            owed = owed.saturating_add(u64::try_from(t.size).unwrap_or(0).saturating_sub(have));
        }
    }
    Ok(free.saturating_sub(owed))
}

/// What the files of the transfers in progress hold on disk now.
pub(crate) async fn stored_bytes(state: &AppState) -> DbResult<u64> {
    let mut held = 0u64;
    for t in table::active(&state.db).await? {
        held = held.saturating_add(len(&state.data_dir, &t.id).await);
    }
    Ok(held)
}

/// `row` as the routes and the feed answer it, `received` its file's length
/// while it uploads (§3).
pub(crate) async fn wire(state: &AppState, row: &TransferRow) -> Transfer {
    let received = if row.state == "uploading" {
        len(&state.data_dir, &row.id).await
    } else {
        u64::try_from(row.received).unwrap_or(0)
    };
    wire_with(row, received)
}

/// `row` as answered, with `received` as given.
pub(crate) fn wire_with(row: &TransferRow, received: u64) -> Transfer {
    Transfer {
        id: row.id.clone(),
        app_id: row.app_id.clone(),
        kind: row.kind.clone(),
        title: row.title.clone(),
        filename: row.filename.clone(),
        size: u64::try_from(row.size).unwrap_or(0),
        sha256: row.sha256.clone(),
        from: TransferParty {
            device_id: row.from_key_id,
            name: row.from_name.clone(),
        },
        to: TransferParty {
            device_id: row.to_key_id,
            name: row.to_name.clone(),
        },
        state: state_of(row),
        received,
        created_at: row.created_at.clone(),
        expires_at: row.expires_at.clone(),
        ended_at: row.ended_at.clone(),
        end_reason: row.end_reason.clone(),
    }
}

/// A row's state as the wire says it.
pub(crate) fn state_of(row: &TransferRow) -> TransferState {
    TransferState::parse(&row.state).unwrap_or_else(|| TransferState::Unknown(row.state.clone()))
}

/// A transfer moved (§6, §11.1): the open feeds read the table, and the
/// owner's Devices card reads the transfers again.
pub(crate) fn said(state: &AppState, row: &TransferRow) {
    state.chat_feed.wake();
    state.telemetry.keys(row.from_key_id, "transfer");
}

/// End transfer `id` as `to`, for `reason`, from one of `from`'s states
/// (§1): its upload slot taken first, so nothing writes its file (I9) and
/// an upload it stops answers with the state it leaves (I23); its
/// downloads closed and its bytes deleted (I26); said. `None` when it was
/// in none of them. A task of its own (I30): a caller that goes while it
/// runs — a handler hyper drops when its client went — leaves no end
/// committed and not carried out.
pub(crate) async fn end(
    state: &SharedState,
    id: &str,
    from: EndsFrom,
    to: TransferState,
    reason: Option<&str>,
) -> DbResult<Option<TransferRow>> {
    spawn_end(state, id, from, to, reason)
        .await
        .map_err(|e| GatewayError::Internal(format!("ending the transfer stopped: {e}")))?
}

/// [`end`]'s task, for the caller that awaits it itself: a suite that
/// drops the caller and still waits for the end to be carried out.
pub(crate) fn spawn_end(
    state: &SharedState,
    id: &str,
    from: EndsFrom,
    to: TransferState,
    reason: Option<&str>,
) -> tokio::task::JoinHandle<DbResult<Option<TransferRow>>> {
    let (st, id, reason) = (state.clone(), id.to_string(), reason.map(str::to_string));
    tokio::spawn(async move {
        let held = st.transfers.uploads.take(&id, Taker::End).await;
        let ended = end_holding(&st, &id, from, to, reason.as_deref()).await;
        drop(held);
        ended
    })
}

/// [`end`] by the one that holds transfer `id`'s upload slot already (an
/// upload whose bytes did not match, one that found its transfer overdue),
/// which runs as a task of its own or is the sweep.
pub(crate) async fn end_holding(
    state: &AppState,
    id: &str,
    from: EndsFrom,
    to: TransferState,
    reason: Option<&str>,
) -> DbResult<Option<TransferRow>> {
    let received = len(&state.data_dir, id).await;
    let ended = table::end(
        &state.db,
        id,
        from,
        &Ending {
            state: to.as_str(),
            reason,
            received: (received > 0).then(|| i64::try_from(received).unwrap_or(i64::MAX)),
            now: Utc::now(),
        },
    )
    .await?;
    if let Some(row) = &ended {
        state.transfers.downloads.close(id);
        unlink(&state.data_dir, id).await;
        said(state, row);
    }
    Ok(ended)
}

/// Transfer `id` as a request reads it: one in progress past its
/// `expires_at` is expired first (I18), so a request an hour after it is
/// refused, not served.
pub(crate) async fn load(state: &SharedState, id: &str) -> DbResult<Option<TransferRow>> {
    let Some(row) = table::get(&state.db, id).await? else {
        return Ok(None);
    };
    if row.is_active() && row.expires_at <= crate::store::stamp(Utc::now()) {
        if let Some(ended) = expire(state, &row).await? {
            return Ok(Some(ended));
        }
        // Moved meanwhile: read what it is now.
        return table::get(&state.db, id).await;
    }
    Ok(Some(row))
}

/// [`load`] by the one that holds transfer `id`'s upload slot already (an
/// upload): an overdue one is expired under that slot.
pub(crate) async fn load_holding(state: &AppState, id: &str) -> DbResult<Option<TransferRow>> {
    let Some(row) = table::get(&state.db, id).await? else {
        return Ok(None);
    };
    if row.is_active() && row.expires_at <= crate::store::stamp(Utc::now()) {
        let why = expired_reason(&row);
        if let Some(ended) = end_holding(
            state,
            id,
            EndsFrom::Active,
            TransferState::Expired,
            Some(&why),
        )
        .await?
        {
            return Ok(Some(ended));
        }
        return table::get(&state.db, id).await;
    }
    Ok(Some(row))
}

/// Expire `row` (§7).
pub(crate) async fn expire(
    state: &SharedState,
    row: &TransferRow,
) -> DbResult<Option<TransferRow>> {
    let why = expired_reason(row);
    end(
        state,
        &row.id,
        EndsFrom::Active,
        TransferState::Expired,
        Some(&why),
    )
    .await
}

/// Why `row` expired, as a sentence for people (I17): how long its offer
/// lasted, read from its own two times, never a setting's key or a raw
/// time (the card formats `expires_at` and `ended_at` itself).
fn expired_reason(row: &TransferRow) -> String {
    let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    let days = match (at(&row.created_at), at(&row.expires_at)) {
        (Some(from), Some(to)) => ((to - from).num_hours() + 12).div_euclid(24),
        _ => 0,
    };
    let within = match days {
        d if d >= 2 => format!("within {d} days of the offer"),
        1 => "within a day of the offer".to_string(),
        _ => "before the offer ran out".to_string(),
    };
    if row.state == "uploading" {
        format!("its upload did not finish {within}")
    } else {
        format!("not delivered {within}")
    }
}

/// Device `key_id`, named `name` (`device:<name>`), was disabled or deleted
/// (`deleted`): every transfer it sends or receives that is in progress is
/// canceled, its bytes deleted, the other side told (§8). A task of its
/// own, every end with it (I30). A failure is logged; the hourly sweep
/// ends what is left (`sweep`).
pub async fn device_ended(state: &SharedState, key_id: i64, name: &str, deleted: bool) {
    let (st, name) = (state.clone(), name.to_string());
    if let Err(e) = tokio::spawn(async move { end_device(&st, key_id, &name, deleted).await }).await
    {
        tracing::warn!("transfers of device {key_id}: ending them stopped ({e})");
    }
}

/// [`device_ended`]'s ends.
async fn end_device(state: &SharedState, key_id: i64, name: &str, deleted: bool) {
    let why = ended_reason(name, deleted);
    let ids = match table::of_device(&state.db, key_id).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(
                "transfers of {}: reading them failed ({e}); the hourly sweep ends them",
                crate::devices::short_name(name)
            );
            return;
        }
    };
    for id in ids {
        if let Err(e) = end(
            state,
            &id,
            EndsFrom::Active,
            TransferState::Canceled,
            Some(&why),
        )
        .await
        {
            tracing::warn!("transfer {id}: ending it failed ({e}); the hourly sweep ends it");
        }
    }
}

/// Why a transfer of device `name` ended when the device did.
fn ended_reason(name: &str, deleted: bool) -> String {
    format!(
        "device '{}' was {}",
        crate::devices::short_name(name),
        if deleted { "deleted" } else { "disabled" }
    )
}

/// Device `key_id`, named `name`, enrolled or was confirmed again, and its
/// app id may have changed (§2, I28): every transfer offered to it that is
/// in progress and names another app id than the one it declares now is
/// canceled, its bytes deleted, the sender told. A task of its own, every
/// end with it (I30). A failure is logged; the hourly sweep ends what is
/// left (`sweep`).
pub async fn app_id_settled(state: &SharedState, key_id: i64, name: &str) {
    let (st, name) = (state.clone(), name.to_string());
    if let Err(e) = tokio::spawn(async move { end_other_app(&st, key_id, &name).await }).await {
        tracing::warn!("transfers to device {key_id}: ending them stopped ({e})");
    }
}

/// [`app_id_settled`]'s ends.
async fn end_other_app(state: &SharedState, key_id: i64, name: &str) {
    let left = match table::mismatched(&state.db, Some(key_id)).await {
        Ok(left) => left,
        Err(e) => {
            tracing::warn!(
                "transfers to {}: reading them failed ({e}); the hourly sweep ends them",
                crate::devices::short_name(name)
            );
            return;
        }
    };
    for (id, app_id) in left {
        let why = app_id_reason(name, app_id.as_deref());
        match end(
            state,
            &id,
            EndsFrom::Active,
            TransferState::Canceled,
            Some(&why),
        )
        .await
        {
            Ok(Some(_)) => tracing::info!("transfer {id} canceled: {why}"),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("transfer {id}: ending it failed ({e}); the hourly sweep ends it")
            }
        }
    }
}

/// Why a transfer to device `name` ended when it paired again as `app_id`
/// (none: without one).
fn app_id_reason(name: &str, app_id: Option<&str>) -> String {
    let bare = crate::devices::short_name(name);
    match app_id {
        Some(app) => format!("device '{bare}' paired again as another app ({app})"),
        None => format!("device '{bare}' paired again without an app id"),
    }
}

#[cfg(test)]
mod tests;
