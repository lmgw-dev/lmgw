//! Async conversion jobs (document-conversion design §7.3, §7.6, §7.7,
//! D27): one job per request, covering all its files, run by the same
//! conversion as a sync request ([`batch::convert_each`]) on a task of its
//! own, as its caller's steps in the tool pool.
//!
//! - **State** is in SQLite (`store::convert_jobs`): the job's and each
//!   file's state and counts, written as each file gets its outcome.
//! - **Uploads** go to disk at accept time ([`results::write_input`]) and
//!   are read when their file is admitted (at most as many files at once as
//!   the pool has slots) and deleted when it is done, so a queued job holds
//!   no upload in memory. The queue and the bytes stored are shown under
//!   Settings › Retention.
//! - **Results** are the files' entries on disk ([`results`]):
//!   `<data dir>/convert/<id>/<index>/entry.json`, written as each file
//!   finishes, so `?index=` answers a file while the rest still run. They
//!   are document content: never in a row or a log line.
//! - **Changes** are pushed to whoever watches ([`Jobs::watch`]): the
//!   docling poll's `wait` and its status WebSocket read the same state.
//! - **Cancel** ([`Jobs::cancel`]) drops the conversion, which stops its
//!   running tools with their process groups and frees their pool slots;
//!   the job ends `cancelled` and its stored results are deleted.
//! - **Restart:** a job running when the gateway stopped is `interrupted`
//!   at the next start ([`sweep::boot`]); its finished files keep their
//!   results until the retention.
//! - **Retention** ([`sweep::upkeep`]): ended jobs go after
//!   `documents.job_retention_days` (0 keeps them); a docling client's
//!   fetched result goes [`RESULT_REMOVAL_DELAY`] after its first fetch.
//! - Jobs do not wait for the GPU hold (D27): a page that would need a held
//!   model gets its tier-0 text, as in a sync request.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use lmgw_api_types::documents::convert::{ConvertWarning, ConvertedDocument, DocStatus};
use lmgw_api_types::documents::jobs::JobFileState;
use tokio::sync::{mpsc, oneshot, watch};

use super::batch::{self, Each};
use super::budget::answers;
use super::budget::OutputBudget;
use super::docling_serve;
use super::file::Input;
use super::options::Plan;
use super::pool;
use crate::error::GatewayError;
use crate::state::SharedState;
use crate::store;

pub mod answer;
pub mod quota;
pub mod reads;
pub mod results;
pub mod secure;
pub mod sweep;

/// How long a docling client's result stays after its first fetch:
/// docling-serve's `DOCLING_SERVE_RESULT_REMOVAL_DELAY` default (its
/// single-use results, `DOCLING_SERVE_SINGLE_USE_RESULTS` on by default).
pub const RESULT_REMOVAL_DELAY: Duration = Duration::from_secs(300);

/// Who made a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `POST /v1/documents/convert` with `async: true`.
    Native,
    /// A docling-serve route.
    Docling,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Docling => "docling",
        }
    }
}

/// The jobs running in this process: how to cancel each, and its change
/// counter.
#[derive(Default)]
pub struct Jobs {
    live: Mutex<HashMap<String, Live>>,
    /// What the jobs store, and how many each key has ([`quota`]).
    pub ledger: quota::Ledger,
}

struct Live {
    cancel: Option<oneshot::Sender<()>>,
    changed: watch::Sender<u64>,
}

impl Jobs {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Live>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Job `id`'s changes while it runs; `None` once it has ended (its
    /// stored state is final).
    pub fn watch(&self, id: &str) -> Option<watch::Receiver<u64>> {
        self.lock().get(id).map(|l| l.changed.subscribe())
    }

    /// Cancel job `id`; `false` when it is not running here.
    pub fn cancel(&self, id: &str) -> bool {
        match self.lock().get_mut(id).and_then(|l| l.cancel.take()) {
            Some(tx) => tx.send(()).is_ok(),
            None => false,
        }
    }

    /// Whether job `id` runs here now.
    pub fn is_live(&self, id: &str) -> bool {
        self.lock().contains_key(id)
    }

    fn bump(&self, id: &str) {
        if let Some(l) = self.lock().get(id) {
            l.changed.send_modify(|n| *n += 1);
        }
    }

    fn end(&self, id: &str) {
        if let Some(l) = self.lock().remove(id) {
            l.changed.send_modify(|n| *n += 1);
        }
    }
}

/// A job to start.
pub struct Start {
    pub key_id: Option<i64>,
    pub origin: Origin,
    /// The files to convert.
    pub files: Vec<Input>,
    /// Files whose outcome the request already decided (skipped by
    /// docling's `from_formats`), by their index.
    pub decided: Vec<ConvertedDocument>,
    pub plan: Plan,
    pub notes: Vec<ConvertWarning>,
    pub deadline: Option<Duration>,
    /// A docling request's answer settings, JSON.
    pub adapter: Option<String>,
    /// docling's `abort_on_error`: a file read with parts left out is failed
    /// as it is recorded, so the poll's counts and the result agree.
    pub abort_on_error: bool,
    /// Whose steps they are in the tool pool.
    pub caller: pool::Caller,
    /// The key's concurrency slot, handed over from the request's gate: the
    /// job holds it until it ends, so a key's `concurrency_limit` bounds its
    /// running jobs.
    pub slot: Option<crate::policy::ConcurrencyGuard>,
}

/// A new job id: `cj_` and 128 random bits in hex.
fn new_id() -> String {
    let bytes: [u8; 16] = rand::random();
    format!("cj_{}", hex::encode(bytes))
}

/// The file state an entry ends in.
pub fn file_state(d: &ConvertedDocument) -> JobFileState {
    match d.status {
        DocStatus::Converted => JobFileState::Converted,
        DocStatus::Partial => JobFileState::Partial,
        DocStatus::Failed => JobFileState::Failed,
        DocStatus::Skipped => JobFileState::Skipped,
    }
}

fn error_code(d: &ConvertedDocument) -> Option<String> {
    d.error.as_ref().and_then(|e| {
        serde_json::to_value(e.code)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
    })
}

fn input_len(i: &Input) -> i64 {
    i.bytes
        .as_ref()
        .map_or(0, |b| i64::try_from(b.len()).unwrap_or(i64::MAX))
}

/// Write job `s` and start it; its id.
pub async fn start(state: &SharedState, mut s: Start) -> Result<String, GatewayError> {
    let input_bytes: i64 = s.files.iter().map(input_len).sum();
    // The key's place and the uploads' room, or the named 429 / 507.
    let uploads = u64::try_from(input_bytes).unwrap_or(u64::MAX);
    let place = quota::admit(state, s.key_id, uploads).await?;
    let id = new_id();
    let mut rows: Vec<store::convert_jobs::NewJobFile> = s
        .files
        .iter()
        .map(|f| store::convert_jobs::NewJobFile {
            idx: i64::from(f.index),
            name: f.name.clone(),
            bytes: input_len(f),
            state: JobFileState::Pending.as_str(),
        })
        .collect();
    rows.extend(s.decided.iter().map(|d| store::convert_jobs::NewJobFile {
        idx: i64::from(d.index),
        name: d.name.clone(),
        bytes: 0,
        state: file_state(d).as_str(),
    }));
    rows.sort_by_key(|r| r.idx);
    let warnings = serde_json::to_string(&s.notes).unwrap_or_else(|_| "[]".into());
    store::convert_jobs::insert_job(
        &state.db,
        &store::convert_jobs::NewJob {
            id: &id,
            key_id: s.key_id,
            origin: s.origin.as_str(),
            created_at: chrono::Utc::now(),
            input_bytes,
            warnings: &warnings,
            adapter: s.adapter.as_deref(),
        },
        &rows,
    )
    .await
    .inspect_err(|_| state.convert_jobs.ledger.sub(uploads))?;
    // The uploads go to disk, so a queued job holds none in memory; the
    // request's files carry no bytes from here on. `admit` reserved them.
    let mut written = 0;
    if let Err(e) = spool(state, &id, &mut s, &mut written).await {
        abandon(state, &id).await;
        state
            .convert_jobs
            .ledger
            .sub(uploads.saturating_sub(written));
        return Err(GatewayError::Internal(format!(
            "storing a job's files: {e}"
        )));
    }
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let (changed, _) = watch::channel(0);
    state.convert_jobs.lock().insert(
        id.clone(),
        Live {
            cancel: Some(cancel_tx),
            changed,
        },
    );
    let st = state.clone();
    let job = id.clone();
    let caller = s.caller.clone();
    // Boxed: the conversion's future is large, and a task starts on a small
    // stack too.
    tokio::spawn(pool::as_caller(
        caller,
        Box::pin(run(st, job, s, cancel_rx, (place, uploads))),
    ));
    Ok(id)
}

/// Every upload to its place in the job's directory (and the results of the
/// files the request already decided); `written` counts the uploads'
/// bytes written.
async fn spool(
    state: &SharedState,
    id: &str,
    s: &mut Start,
    written: &mut u64,
) -> std::io::Result<()> {
    let ledger = &state.convert_jobs.ledger;
    for d in &s.decided {
        results::write_within(
            &state.data_dir,
            id,
            d.clone(),
            |n| {
                ledger.add(n);
                true
            },
            &results::Landed::default(),
        )
        .await?;
    }
    for f in &mut s.files {
        if let Ok(bytes) = &f.bytes {
            results::write_input(&state.data_dir, id, f.index, bytes).await?;
            *written += bytes.len() as u64;
            f.bytes = Ok(bytes::Bytes::new());
        }
    }
    Ok(())
}

/// A job that could not start: its row and its directory go.
async fn abandon(state: &SharedState, id: &str) {
    if let Err(e) = store::convert_jobs::delete_job(&state.db, id).await {
        tracing::warn!("deleting conversion job {id} that did not start: {e}");
    }
    let gone = results::remove(&state.data_dir, id).await;
    state.convert_jobs.ledger.sub(gone);
}

/// The job's task: convert, record each file as it finishes, end the job.
/// `place` is its key's count ([`quota`]), given back as the task ends;
/// `uploads` the bytes of its files, which it reads back from disk.
async fn run(
    state: SharedState,
    id: String,
    s: Start,
    cancel: oneshot::Receiver<()>,
    (_place, uploads): (quota::Place, u64),
) {
    let who = s.caller.share.clone();
    let facts = state.doc_tools.current().await;
    let (tx, mut rx) = mpsc::unbounded_channel::<ConvertedDocument>();
    let Start {
        files,
        decided,
        plan,
        notes,
        deadline,
        slot: _slot,
        abort_on_error,
        ..
    } = s;
    let work = async {
        let each = Each {
            done: Some(tx),
            deadline,
            spool: Some(results::Spool {
                data_dir: state.data_dir.clone(),
                job: id.clone(),
                stored: state.convert_jobs.ledger.stored_count(),
            }),
        };
        let r =
            batch::convert_each(files, &plan, &facts, notes, OutputBudget::request(), &each).await;
        drop(each);
        r
    };
    // The files done by the time the last batch was recorded go in one
    // batch, at most `batch_files()` of them: their entries side by side,
    // their rows in one transaction (§14 performance round: a transaction a
    // file was 5.5 ms of a 200-file job's 6 ms a file).
    let record = async {
        while let Some(doc) = rx.recv().await {
            let batch = batch_of(doc, &mut rx, batch_files());
            record_files(&state, &id, batch, abort_on_error).await;
        }
    };
    let both = async {
        // The job's share of the answers in flight while it converts and
        // records, counted against the key that sent it: its uploads read
        // back and its files' entries live in memory until they are written
        // (`budget::answers`). The request that accepted it held at least as
        // much, so the bound admits it; it is waited for here, before the
        // first file (a cancel ends the wait), and the wait is no file's time.
        let a = answers::global();
        let _share = a
            .hold(&who, uploads.saturating_add(a.output_budget()))
            .await
            .ok();
        tokio::join!(Box::pin(work), record).0
    };
    tokio::select! {
        (_, answer) = both => {
            let mut all = answer.documents;
            if abort_on_error {
                docling_serve::abort_partial(&mut all);
            }
            all.extend(decided);
            let read = all
                .iter()
                .any(|d| matches!(d.status, DocStatus::Converted | DocStatus::Partial));
            let tools = serde_json::to_string(&answer.tools).unwrap_or_else(|_| "[]".into());
            let (end, error) = if read {
                ("succeeded", None)
            } else {
                ("failed", Some("no file was converted; each file's entry says why"))
            };
            if let Err(e) =
                store::convert_jobs::finish_job(&state.db, &id, end, "interrupted", &tools, error).await
            {
                tracing::warn!("ending conversion job {id}: {e}");
            }
            let gone = results::remove_inputs(&state.data_dir, &id).await;
            state.convert_jobs.ledger.sub(gone);
        }
        _ = cancel => {
            // The conversion is dropped here: its tools are stopped and their
            // slots given back. Then its results go.
            match store::convert_jobs::finish_job(
                &state.db,
                &id,
                "cancelled",
                "cancelled",
                "[]",
                Some("cancelled by the caller; its results were deleted"),
            )
            .await
            {
                Ok(_) => {
                    let gone = results::remove(&state.data_dir, &id).await;
                    state.convert_jobs.ledger.sub(gone);
                }
                Err(e) => tracing::warn!("cancelling conversion job {id}: {e}"),
            }
            tracing::info!("conversion job {id} cancelled");
        }
    }
    state.convert_jobs.end(&id);
}

/// One file's outcome: its entry on disk, then its row.
/// A result that could not be written is the file's own failure, with the
/// reason, not a file the gateway "stopped" on.
///
/// With `abort_on_error` a file read with parts left out is failed here, so
/// the job's states, its counts and its stored entry agree.
pub async fn record_file(
    state: &SharedState,
    id: &str,
    doc: ConvertedDocument,
    abort_on_error: bool,
) {
    record_files(state, id, vec![doc], abort_on_error).await;
}

/// The most files a job records in one batch, and writes at once: the tool
/// pool's size (`pool::global`), as many as convert side by side
/// (performance round review S7). The files' content is bounded besides by
/// the request's output (`MAX_UNCOMPRESSED_BYTES` for all of them).
fn batch_files() -> usize {
    usize::try_from(super::pool::global().facts().slots)
        .unwrap_or(1)
        .max(1)
}

/// `first` and what else `rx` holds now, at most `most` together.
fn batch_of<T>(first: T, rx: &mut mpsc::UnboundedReceiver<T>, most: usize) -> Vec<T> {
    let mut batch = vec![first];
    while batch.len() < most {
        match rx.try_recv() {
            Ok(more) => batch.push(more),
            Err(_) => break,
        }
    }
    batch
}

/// [`record_file`] for several files: every entry written, at most
/// [`batch_files`] at once, then every row in one transaction, so each
/// file's entry is on disk before its row says it is done.
pub async fn record_files(
    state: &SharedState,
    id: &str,
    docs: Vec<ConvertedDocument>,
    abort_on_error: bool,
) {
    use futures::stream::{self, StreamExt};
    let writes = stream::iter(docs.into_iter().map(|mut doc| async move {
        if abort_on_error {
            docling_serve::abort_partial(std::slice::from_mut(&mut doc));
        }
        let index = doc.index;
        let stored = (file_state(&doc), i64::from(doc.parts), error_code(&doc));
        // What is reserved and not on disk goes back when this ends, written,
        // failed or dropped by a cancel (review N1, verification N-a).
        let reservation = quota::Reservation::new(state);
        let wrote = results::write_within(
            &state.data_dir,
            id,
            doc,
            |n| reservation.room(n),
            &reservation.landed,
        )
        .await;
        drop(reservation);
        let (state_now, parts, code) = match wrote {
            Ok(_) => stored,
            Err(e) if results::is_no_room(&e) => {
                tracing::warn!(
                    "file {index} of conversion job {id} was not stored: the jobs' storage \
                     bound is reached ({})",
                    quota::STORAGE_CODE
                );
                (
                    JobFileState::Failed,
                    0,
                    Some(format!("{NOT_STORED}{}", quota::NO_ROOM)),
                )
            }
            Err(e) => {
                tracing::warn!("storing file {index} of conversion job {id}: {e}");
                (
                    JobFileState::Failed,
                    0,
                    Some(format!("{NOT_STORED}{}", e.kind())),
                )
            }
        };
        store::convert_jobs::FileDone {
            idx: i64::from(index),
            state: state_now.as_str().to_string(),
            parts,
            error: code,
        }
    }));
    let stored: Vec<_> = writes.buffer_unordered(batch_files()).collect().await;
    match store::convert_jobs::files_done(&state.db, id, &stored).await {
        Ok(true) => state.convert_jobs.bump(id),
        Ok(false) => {}
        Err(e) => tracing::warn!(
            "recording {} file(s) of conversion job {id}: {e}",
            stored.len()
        ),
    }
}

/// The start of the error a file carries when its converted text could not
/// be stored; the I/O error's kind follows.
pub const NOT_STORED: &str = "result_not_stored: ";

/// Cancel job `id` and wait until it has ended; `false` when it was not
/// running.
pub async fn cancel(state: &SharedState, id: &str) -> bool {
    let Some(mut rx) = state.convert_jobs.watch(id) else {
        return false;
    };
    if !state.convert_jobs.cancel(id) {
        return false;
    }
    // The task removes the job from the live map last; the sender goes with
    // it.
    while rx.changed().await.is_ok() {
        if state.convert_jobs.watch(id).is_none() {
            break;
        }
    }
    true
}

/// What the jobs hold now (`doc_jobs_stored`).
pub async fn stored(state: &SharedState) -> lmgw_api_types::documents::jobs::JobsStored {
    let (jobs, bytes) = results::usage(&state.data_dir).await;
    let (running, queued_files) = store::convert_jobs::queue(&state.db)
        .await
        .unwrap_or((0, 0));
    lmgw_api_types::documents::jobs::JobsStored {
        jobs,
        bytes,
        running,
        queued_files,
    }
}

#[cfg(test)]
mod tests_batch {
    use super::*;

    #[test]
    fn a_batch_holds_what_waits_up_to_its_bound() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        for i in 1..10 {
            tx.send(i).unwrap();
        }
        assert_eq!(batch_of(0, &mut rx, 4), vec![0, 1, 2, 3]);
        // The rest waits for the next batch.
        assert_eq!(batch_of(100, &mut rx, 100), vec![100, 4, 5, 6, 7, 8, 9]);
        assert!(batch_files() >= 1);
    }
}
