//! What async conversion jobs may store and queue (security round A, M2;
//! this revises WP5's decision "no cap on queued jobs"):
//!
//! - **Storage for conversion jobs** (`documents.job_storage_mib`; 0 is
//!   derived): the bytes the jobs store together, their uploads while they
//!   run and their results after. Derived at every check (review S3): half
//!   of what the jobs store now and what is free now on the data directory's
//!   file system together, so the other half of the free space stays for
//!   the database, models and everything else on that disk, also after a
//!   model download filled some of it. A size set by hand is used as it is.
//!   A request whose uploads do not fit beside what is stored is refused
//!   with 507 [`STORAGE_CODE`]; a file whose result does not fit is not
//!   stored, its entry naming the setting. Where the free space cannot be
//!   read and nothing is set by hand, storage is not bounded.
//! - **Jobs per key** (`documents.jobs_per_key`; 0 is derived): the jobs
//!   one key (every keyless request together) has running or queued.
//!   Derived: the tool pool's size, as many as can each have a step running
//!   at once. One more is refused with 429 [`PER_KEY_CODE`].
//!
//! What is stored is counted here ([`Ledger`]): read from the disk at the
//! first check after start and again at the hourly upkeep, every upload and
//! result reserved when it is checked (so requests checked at once never
//! pass the bound together, review N1) and given back when its write fails,
//! and what a file's end (its upload), a job's end, cancel or deletion
//! removes taken off.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use lmgw_api_types::documents::bounds::JobBounds;
use lmgw_api_types::units::fmt_bytes;

use super::results;
use crate::error::GatewayError;
use crate::state::SharedState;

/// The 507's code.
pub const STORAGE_CODE: &str = "job_storage_full";
/// The 429's code.
pub const PER_KEY_CODE: &str = "too_many_jobs";
/// The error a file's entry carries when its result found no room (after
/// `super::NOT_STORED`).
pub const NO_ROOM: &str = "job_storage";

/// The bytes the jobs store, as counted; shared with the jobs' spools,
/// which take off each upload as its file is done.
#[derive(Debug, Clone, Default)]
pub struct Stored(Arc<AtomicU64>);

impl Stored {
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    pub fn add(&self, bytes: u64) {
        self.0.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn sub(&self, bytes: u64) {
        let _ = self
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(bytes))
            });
    }

    /// Count `bytes` when they fit under `limit` (`None`: not bounded),
    /// in one step.
    fn reserve(&self, bytes: u64, limit: Option<u64>) -> Result<(), u64> {
        self.0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                let to = n.saturating_add(bytes);
                limit.is_none_or(|l| to <= l).then_some(to)
            })
            .map(|_| ())
    }
}

/// What the jobs store and who has how many.
#[derive(Debug, Default)]
pub struct Ledger {
    stored: Stored,
    synced: tokio::sync::OnceCell<()>,
    per_key: Arc<Mutex<HashMap<Option<i64>, u32>>>,
}

/// One admitted job's place in its key's count, given back when the job
/// ends (dropped with its task).
#[must_use = "the job's place is given back when this is dropped"]
pub struct Place {
    per_key: Arc<Mutex<HashMap<Option<i64>, u32>>>,
    key: Option<i64>,
}

impl Drop for Place {
    fn drop(&mut self) {
        let mut g = self.per_key.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = g.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                g.remove(&self.key);
            }
        }
    }
}

impl Ledger {
    /// Bytes stored now, as counted.
    pub fn stored(&self) -> u64 {
        self.stored.get()
    }

    /// The count itself, for a job's spool.
    pub fn stored_count(&self) -> Stored {
        self.stored.clone()
    }

    /// `bytes` more were written.
    pub fn add(&self, bytes: u64) {
        self.stored.add(bytes);
    }

    /// `bytes` were deleted.
    pub fn sub(&self, bytes: u64) {
        self.stored.sub(bytes);
    }

    /// The count read from the disk once, before the first check.
    async fn ready(&self, state: &SharedState) {
        self.synced.get_or_init(|| resync(state)).await;
    }
}

/// What the disk holds now becomes the count (at the hourly upkeep).
pub async fn resync(state: &SharedState) {
    let (_, bytes) = results::usage(&state.data_dir).await;
    state
        .convert_jobs
        .ledger
        .stored
        .0
        .store(bytes, Ordering::Relaxed);
}

/// The storage bound in force now.
#[derive(Debug, Clone, Copy)]
pub struct Storage {
    /// `None`: not bounded.
    pub limit: Option<u64>,
    pub by_hand: bool,
    /// Free on the data directory's file system now (`None`: not read).
    pub free: Option<u64>,
    /// What the jobs stored when it was read.
    pub stored: u64,
}

/// The storage bound in force now (module doc).
pub fn storage_limit(state: &SharedState) -> Storage {
    let stored = state.convert_jobs.ledger.stored();
    let mib = state.snapshot().settings.documents.job_storage_mib;
    let free = crate::backends::paths::free_bytes(&state.data_dir);
    let limit = if mib > 0 {
        Some(mib.saturating_mul(1 << 20))
    } else {
        free.map(|f| f.saturating_add(stored) / 2)
    };
    Storage {
        limit,
        by_hand: mib > 0,
        free,
        stored,
    }
}

/// The jobs per key in force, and whether it is set by hand.
pub fn per_key_limit(state: &SharedState) -> (u32, bool) {
    let n = state.snapshot().settings.documents.jobs_per_key;
    if n > 0 {
        return (n, true);
    }
    (super::super::pool::global().facts().slots.max(1), false)
}

fn storage_words(s: &Storage, limit: u64) -> String {
    if s.by_hand {
        format!(
            "Settings › Documents › Storage for conversion jobs ({}, documents_job_storage_mib)",
            fmt_bytes(limit)
        )
    } else {
        format!(
            "Settings › Documents › Storage for conversion jobs ({}: half of the {} the jobs \
             store and the {} free on the data directory's disk together; set \
             documents_job_storage_mib to change it)",
            fmt_bytes(limit),
            fmt_bytes(s.stored),
            fmt_bytes(s.free.unwrap_or_default())
        )
    }
}

/// Admit a job of `key` with `input_bytes` of uploads: its place in the
/// key's count, or the named refusal.
pub async fn admit(
    state: &SharedState,
    key: Option<i64>,
    input_bytes: u64,
) -> Result<Place, GatewayError> {
    let ledger = &state.convert_jobs.ledger;
    ledger.ready(state).await;
    let storage = storage_limit(state);
    // Reserved now (review N1); `super::spool` writes the uploads into it.
    if let Err(stored) = ledger.stored.reserve(input_bytes, storage.limit) {
        let limit = storage.limit.unwrap_or_default();
        return Err(GatewayError::Refused {
            status: 507,
            code: STORAGE_CODE,
            message: format!(
                "the conversion jobs store {} and this job's files are {}, more than {} allows \
                 ({STORAGE_CODE}); fetch or delete finished jobs, or wait for their retention",
                fmt_bytes(stored),
                fmt_bytes(input_bytes),
                storage_words(&storage, limit)
            ),
        });
    }
    let (most, by_hand) = per_key_limit(state);
    let mut g = ledger.per_key.lock().unwrap_or_else(|e| e.into_inner());
    let n = g.entry(key).or_default();
    if *n >= most {
        ledger.sub(input_bytes);
        let setting = if by_hand {
            format!("Settings › Documents › Jobs per key ({most}, documents_jobs_per_key)")
        } else {
            format!(
                "Settings › Documents › Jobs per key ({most}: the tool pool's size; set \
                 documents_jobs_per_key to change it)"
            )
        };
        return Err(GatewayError::Refused {
            status: 429,
            code: PER_KEY_CODE,
            message: format!(
                "this key has {n} conversion jobs running or queued, the most {setting} allows \
                 ({PER_KEY_CODE}); send this one when one of them has ended"
            ),
        });
    }
    *n += 1;
    Ok(Place {
        per_key: ledger.per_key.clone(),
        key,
    })
}

/// Whether a result of `bytes` fits beside what is stored; reserved in one
/// step when it does (review N1; [`unwritten`] gives it back when its write
/// fails).
pub fn room_for(state: &SharedState, bytes: u64) -> bool {
    let limit = storage_limit(state).limit;
    state
        .convert_jobs
        .ledger
        .stored
        .reserve(bytes, limit)
        .is_ok()
}

/// A result of `bytes` reserved by [`room_for`] was not written.
pub fn unwritten(state: &SharedState, bytes: u64) {
    state.convert_jobs.ledger.sub(bytes);
}

/// One result's reservation ([`room_for`]) for as long as its write runs:
/// dropped, it gives back what was reserved and did not reach the disk
/// ([`results::Landed`]), whether the write failed or was dropped by a
/// cancel midway (security round A verification, N-a; before, a cancel
/// left the reservation counted until the hourly resync).
pub struct Reservation<'a> {
    state: &'a SharedState,
    reserved: AtomicU64,
    /// What the write has put on disk.
    pub landed: results::Landed,
}

impl<'a> Reservation<'a> {
    pub fn new(state: &'a SharedState) -> Self {
        Self {
            state,
            reserved: AtomicU64::new(0),
            landed: results::Landed::default(),
        }
    }

    /// [`room_for`] `bytes`, kept by this reservation when they fit.
    pub fn room(&self, bytes: u64) -> bool {
        let ok = room_for(self.state, bytes);
        if ok {
            self.reserved.store(bytes, Ordering::Relaxed);
        }
        ok
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let left = self
            .reserved
            .load(Ordering::Relaxed)
            .saturating_sub(self.landed.bytes());
        if left > 0 {
            unwritten(self.state, left);
        }
    }
}

/// The message a file's entry carries when its result found no room.
pub fn no_room_message(state: &SharedState) -> String {
    let s = storage_limit(state);
    match s.limit {
        Some(limit) => format!(
            "the converted text was not stored: the conversion jobs' storage is full, as {} \
             allows ({STORAGE_CODE}); fetch or delete finished jobs and send the file again",
            storage_words(&s, limit)
        ),
        None => "the converted text was not stored: the conversion jobs' storage is full \
                 (job_storage_full)"
            .to_string(),
    }
}

/// Settings › Documents' facts.
pub async fn facts(state: &SharedState) -> JobBounds {
    state.convert_jobs.ledger.ready(state).await;
    let s = storage_limit(state);
    let (per_key, per_key_by_hand) = per_key_limit(state);
    JobBounds {
        storage_bytes: s.limit.unwrap_or(0),
        storage_by_hand: s.by_hand,
        free_bytes: s.free,
        stored_bytes: s.stored,
        per_key,
        per_key_by_hand,
        pool_slots: super::super::pool::global().facts().slots,
    }
}

#[cfg(test)]
mod tests;
