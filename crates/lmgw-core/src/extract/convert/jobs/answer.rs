//! What a job answers (document-conversion design §7.3): its state
//! ([`ConvertJob`]), and its result, the same body a sync request gets,
//! assembled from the stored entries in request order. A file without an
//! entry (the gateway stopped before it finished) is a failed entry with
//! `interrupted`, so the answer always has one entry per file.

use lmgw_api_types::documents::convert::{
    ConvertAnswer, ConvertError, ConvertStats, ConvertWarning, ConvertedDocument, DocStatus,
    ErrorCode, ToolUsed,
};
use lmgw_api_types::documents::jobs::{ConvertJob, JobFile, JobFileState, JobPool, JobState};

use super::results;
use crate::principal::Principal;
use crate::state::SharedState;
use crate::store;
use crate::store::convert_jobs::{JobFileRow, JobRow};

/// A job as read for an answer.
pub struct Loaded {
    pub row: JobRow,
    pub files: Vec<JobFileRow>,
}

impl Loaded {
    pub fn state(&self) -> JobState {
        JobState::parse(&self.row.state)
    }

    pub fn warnings(&self) -> Vec<ConvertWarning> {
        serde_json::from_str(&self.row.warnings).unwrap_or_default()
    }

    fn tools(&self) -> Vec<ToolUsed> {
        serde_json::from_str(&self.row.tools).unwrap_or_default()
    }
}

/// Job `id`, when `who` may see it: the key that made it (a request without
/// a key sees the jobs made without one), or the owner. Anything else reads
/// as no job, so a key cannot learn another's job ids.
pub async fn load(state: &SharedState, who: &Principal, id: &str) -> Option<Loaded> {
    if !results::is_job_id(id) {
        return None;
    }
    let row = store::convert_jobs::get_job(&state.db, id)
        .await
        .ok()
        .flatten()?;
    if !(who.is_owner() || who.key_id() == row.key_id) {
        return None;
    }
    let files = store::convert_jobs::job_files(&state.db, id).await.ok()?;
    Some(Loaded { row, files })
}

fn n(v: i64) -> u32 {
    u32::try_from(v.max(0)).unwrap_or(u32::MAX)
}

/// The job's state, with the tool pool's while it runs.
pub fn status(l: &Loaded) -> ConvertJob {
    let state = l.state();
    let pool = (state == JobState::Running).then(|| {
        let f = super::pool::global().facts();
        JobPool {
            slots: f.slots,
            running: f.running,
            waiting: f.waiting,
            answers: crate::extract::convert::budget::answers::global().facts(),
        }
    });
    ConvertJob {
        job: l.row.id.clone(),
        state,
        created_at: l.row.created_at.clone(),
        finished_at: l.row.finished_at.clone(),
        files: n(l.row.files),
        input_bytes: u64::try_from(l.row.input_bytes.max(0)).unwrap_or(0),
        parts: n(l.row.parts),
        parts_done: n(l.row.parts_done),
        error: l.row.error.clone(),
        warnings: l.warnings(),
        documents: l
            .files
            .iter()
            .map(|f| JobFile {
                index: n(f.idx),
                name: f.name.clone(),
                bytes: u64::try_from(f.bytes.max(0)).unwrap_or(0),
                state: JobFileState::parse(&f.state),
                parts: n(f.parts),
                parts_done: n(f.parts_done),
                error: f.error.clone(),
            })
            .collect(),
        pool,
        fetched_at: l.row.fetched_at.clone(),
    }
}

/// The entry of a file the job has no stored entry for.
fn unfinished(st: &SharedState, f: &JobFileRow) -> ConvertedDocument {
    let state = JobFileState::parse(&f.state);
    let cancelled = state == JobFileState::Cancelled;
    // A file that was converted but whose text could not be stored.
    let unstored = (state == JobFileState::Failed)
        .then_some(f.error.as_deref())
        .flatten()
        .and_then(|e| e.strip_prefix(super::NOT_STORED));
    ConvertedDocument {
        index: n(f.idx),
        name: f.name.clone(),
        status: DocStatus::Failed,
        format: None,
        parts: 0,
        content: None,
        structure: None,
        pictures: Vec::new(),
        text: None,
        warnings: Vec::new(),
        stats: ConvertStats::default(),
        error: Some(ConvertError {
            code: if unstored.is_some() {
                ErrorCode::Unreadable
            } else {
                ErrorCode::Interrupted
            },
            message: if unstored == Some(super::quota::NO_ROOM) {
                super::quota::no_room_message(st)
            } else if let Some(kind) = unstored {
                format!(
                    "the converted text could not be stored ({kind}); send the file again once \
                     the data dir's disk has room"
                )
            } else if cancelled {
                "the job was cancelled before this file was converted".to_string()
            } else {
                "the gateway stopped before this file was converted; send it again".to_string()
            },
            tool: None,
        }),
    }
}

/// File `f`'s entry: stored, or the one an unfinished file gets.
pub async fn entry(state: &SharedState, l: &Loaded, f: &JobFileRow) -> ConvertedDocument {
    let stored = if JobFileState::parse(&f.state).has_result() {
        results::read(&state.data_dir, &l.row.id, n(f.idx)).await
    } else {
        None
    };
    stored.unwrap_or_else(|| unfinished(state, f))
}

/// The whole answer of an ended job, as the sync request would have had it.
pub async fn full(state: &SharedState, l: &Loaded) -> (u16, ConvertAnswer) {
    let mut documents = Vec::with_capacity(l.files.len());
    for f in &l.files {
        documents.push(entry(state, l, f).await);
    }
    let summary = super::batch::summary(&documents);
    let status = super::batch::status(&documents);
    (
        status,
        ConvertAnswer {
            documents,
            summary,
            tools: l.tools(),
            warnings: l.warnings(),
        },
    )
}
