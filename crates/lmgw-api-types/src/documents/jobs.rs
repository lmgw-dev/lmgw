//! Async conversion jobs (document-conversion design §7.3, §7.6, §7.7): a
//! request with `async: true` answers 202 with [`JobAccepted`]; the job's
//! state is `GET /v1/documents/jobs/{id}` ([`ConvertJob`]), its result `GET
//! /v1/documents/jobs/{id}/result` (the sync answer, or one file's entry with
//! `?index=`), and `DELETE /v1/documents/jobs/{id}` cancels it.
//!
//! A job belongs to the key that made it: another key gets a 404, the owner
//! sees every job. Its results are kept for `documents_job_retention_days`
//! (Settings › Retention). A job that was running when the gateway stopped is
//! `interrupted` after the restart: files that had finished keep their
//! results, the others say so.

use serde::{Deserialize, Serialize};

/// The 202 of an async request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct JobAccepted {
    /// The job's id (`cj_…`): the path of its state and result.
    pub job: String,
}

/// A job's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum JobState {
    /// Accepted; its files wait for tool slots or are being read.
    Running,
    /// Every file has an outcome and at least one was converted.
    Succeeded,
    /// Every file has an outcome and none was converted.
    Failed,
    /// Cancelled (`DELETE`): its running tools were stopped and its results
    /// deleted.
    Cancelled,
    /// The gateway stopped while it ran; files that had finished keep their
    /// results.
    Interrupted,
}

impl JobState {
    /// The column's spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    /// The state a column holds; an unknown spelling reads as interrupted.
    pub fn parse(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Interrupted,
        }
    }

    /// Whether it has ended.
    pub fn is_done(self) -> bool {
        self != Self::Running
    }
}

/// One file's state in a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum JobFileState {
    /// Waiting for a tool slot, or being read.
    Pending,
    Converted,
    Partial,
    Failed,
    Skipped,
    /// The job was cancelled before this file had an outcome.
    Cancelled,
    /// The gateway stopped before this file had an outcome.
    Interrupted,
}

impl JobFileState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Converted => "converted",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "converted" => Self::Converted,
            "partial" => Self::Partial,
            "failed" => Self::Failed,
            "skipped" => Self::Skipped,
            "cancelled" => Self::Cancelled,
            _ => Self::Interrupted,
        }
    }

    /// Whether the file has a stored entry (`?index=` answers it).
    pub fn has_result(self) -> bool {
        matches!(
            self,
            Self::Converted | Self::Partial | Self::Failed | Self::Skipped
        )
    }
}

/// `GET /v1/documents/jobs/{id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConvertJob {
    pub job: String,
    pub state: JobState,
    /// When it was accepted, and when it ended: RFC 3339 UTC.
    pub created_at: String,
    pub finished_at: Option<String>,
    /// The files and their total size in bytes, as the request carried them.
    pub files: u32,
    pub input_bytes: u64,
    /// Parts (pages, slides, sheets) of the files that have an outcome, and
    /// how many of them are done. A file's parts are known once it is read,
    /// so `parts` grows as files finish; progress inside one file is not
    /// reported yet.
    pub parts: u32,
    pub parts_done: u32,
    /// Why the job ended without a result (cancelled, interrupted, failed).
    pub error: Option<String>,
    /// The request's own notes (an option whose feature is not built yet).
    pub warnings: Vec<super::convert::ConvertWarning>,
    /// Per file, in request order.
    pub documents: Vec<JobFile>,
    /// The tool pool now, while the job runs: how many steps of all callers
    /// run and wait. Steps take free slots round robin per caller, so this is
    /// the queue a waiting file stands in, not a position. `None` once the
    /// job has ended.
    pub pool: Option<JobPool>,
    /// When a docling-serve client fetched its result: it is deleted
    /// `RESULT_REMOVAL_DELAY` (300 s) later, as docling-serve's single-use
    /// results are. `None`: kept for the retention setting.
    pub fetched_at: Option<String>,
}

/// One file of a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct JobFile {
    pub index: u32,
    pub name: String,
    pub bytes: u64,
    pub state: JobFileState,
    /// Its parts, once it has an outcome.
    pub parts: u32,
    pub parts_done: u32,
    /// Its error's code, when it failed.
    pub error: Option<String>,
}

/// The tool pool, as a running job sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct JobPool {
    pub slots: u32,
    pub running: u32,
    pub waiting: u32,
    /// The answers in flight now (security round A): a job holds a share of
    /// their memory while it converts, and waits for one before its first
    /// file when they are all held.
    #[serde(default)]
    pub answers: super::bounds::AnswerFacts,
}

/// `GET /v1/documents/jobs/{id}/result`'s query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct JobResultQuery {
    /// One file's entry, by its 0-based position, as soon as that file has
    /// an outcome; absent: the whole answer once the job has ended.
    pub index: Option<u32>,
}

/// What the conversion jobs hold on this computer now (`doc_jobs_stored`,
/// beside Settings › Retention's "Conversion jobs"): the jobs with files on
/// disk (uploads while they run, converted text after) and what those take,
/// and the work still queued.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct JobsStored {
    /// Jobs with a directory in the data dir.
    pub jobs: u64,
    /// Bytes of those directories together.
    pub bytes: u64,
    /// Jobs running now.
    pub running: u64,
    /// Files of running jobs that have no outcome yet (waiting or being
    /// converted).
    pub queued_files: u64,
}
