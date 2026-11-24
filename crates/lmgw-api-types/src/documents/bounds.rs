//! What bounds the conversions as a whole (document-conversion design §14,
//! security round A): the memory answers in flight may hold, and what the
//! async jobs may store and queue. Shown on Settings › Documents.

use serde::{Deserialize, Serialize};

/// The answers in flight, gateway-wide: every conversion holds a share of
/// this memory from before its files are read until its answer has been
/// written to the client (or the client went away).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AnswerFacts {
    /// What all shares together may hold: the memory lmgw may use, as the
    /// tool pool reads it (`PoolInputs::memory_bytes`). `None` where the
    /// memory is not read on this system yet: not bounded then.
    pub limit_bytes: Option<u64>,
    /// What one request's files may produce together: `MAX_UNCOMPRESSED_BYTES`,
    /// or half of `limit_bytes` where that is less. A share is the request's
    /// upload plus this (the most upload `max_body_mb` allows when its length
    /// is not declared), then its body's real size.
    pub output_bytes: u64,
    /// Held now, by how many conversions.
    pub held_bytes: u64,
    pub holding: u32,
    /// Conversions waiting for a share now (before their files are read).
    pub waiting: u32,
    /// An answer's write deadline: `write_grace_s` plus one second for
    /// every `min_rate_bytes_per_s` of its body. A client still reading
    /// past it has its connection ended, and the share goes back.
    pub write_grace_s: u64,
    pub min_rate_bytes_per_s: u64,
    /// A request body's read deadline, the same shape (408 past it).
    pub read_grace_s: u64,
    pub read_min_rate_bytes_per_s: u64,
    /// Whether one caller's shares apply now.
    pub callers: CallerFacts,
}

/// Whether one caller's shares (of the tool pool, of the answers) apply:
/// only when a second caller can exist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CallerFacts {
    /// "Require API key" is on.
    pub sign_in: bool,
    /// Callers that may call the conversion routes: enabled keys that hold
    /// `/v1`, one account's owner keys counted once.
    pub callers: u32,
    /// The shares apply: `sign_in` and more than one caller.
    pub shared: bool,
}

/// The async jobs' bounds: the bytes they may store, and how many one key
/// may have running or queued. Each is a setting; 0 is derived.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct JobBounds {
    /// "Storage for conversion jobs" in force, in bytes.
    pub storage_bytes: u64,
    /// Set by hand (`documents.job_storage_mib`), not derived.
    pub storage_by_hand: bool,
    /// Free now on the data directory's file system (`None`: not read;
    /// the bound is then off unless set by hand). The derived bound is half
    /// of this and `stored_bytes` together.
    pub free_bytes: Option<u64>,
    /// What the jobs store now (their uploads while they run, their
    /// results after), as counted since start.
    pub stored_bytes: u64,
    /// "Jobs per key" in force: running and queued together.
    pub per_key: u32,
    /// Set by hand (`documents.jobs_per_key`), not derived.
    pub per_key_by_hand: bool,
    /// What the derived figure comes from: the tool pool's size.
    pub pool_slots: u32,
}
