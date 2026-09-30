//! The `kb_ingest` job (chat-complete design §9.2): every `pending` file of
//! one base → text → chunks → vectors, one file at a time, keyed `kb:<id>` so
//! a base has one live ingest.
//!
//! - **Progress** is in files; the detail names the file and its stage.
//! - **Cancel** lands between files and between embedding batches; the file
//!   in hand goes back to `pending`, with the reason on it.
//! - **GPU hold.** Unattended batch work is refused under a hold, never
//!   re-routed (gpu-hold design §2): a local embedding or vision model that is
//!   held stops the job with the hold's own message, and every file still
//!   waiting stays `pending` with that message as its reason. Resume — or the
//!   next upload — starts the job again.
//! - **A file that cannot be read** is `failed` with its reason, and the job
//!   goes on with the next one.
//! - **Re-ingest reuses vectors.** Chunk ids are content-derived, so a region
//!   that did not change keeps its id — and its stored vector, which is not
//!   paid for twice.
//!
//! The embedding model is the base's **pinned** one
//! ([`InProcessEmbedder::for_identity`]), refused when it no longer resolves,
//! exactly as a docs corpus's is.

use bytes::Bytes;
use quickdoc_core::embed::Embedder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::jobs::{JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress, Spawn};
use crate::quickdoc::embed::refuse_if_held;
use crate::quickdoc::query::TiktokenCounter;
use crate::quickdoc::InProcessEmbedder;
use crate::state::SharedState;

use quickdoc_core::embed::TokenCounter;

use std::collections::HashMap;

use super::chunk::{self, Sizes};
use super::ingest_split;
use super::limit;
use super::sections::{self, Stop};
use super::store::{self, Ingested, Kb, KbFile, NewKbChunk};
use super::{job_key, originals};

/// Request payload of a `kb_ingest` job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub kb_id: i64,
}

/// The job's progress detail. `done`/`total` are files.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Detail {
    pub kb_id: i64,
    pub kb: String,
    /// The file in hand, and what is happening to it.
    pub file: Option<String>,
    pub file_stage: String,
    pub ready: u64,
    pub failed: u64,
    pub chunks: u64,
    pub embedded: u64,
    /// Vectors reused from an earlier ingest of an unchanged region.
    pub reused: u64,
    /// What counted tokens while chunking the file in hand.
    #[serde(default)]
    pub tokenizer: String,
}

/// Start (or join) the ingest of `kb`'s pending files. The caller checks that
/// no re-embed runs on the base ([`super::ops::start_ingest`]).
pub(crate) async fn spawn(state: &SharedState, kb: &Kb) -> Result<Spawn, String> {
    crate::jobs::spawn(
        state,
        JobKind::KbIngest,
        Some(job_key(kb.id)),
        format!("ingest into {}", kb.name),
        json!({ "kb_id": kb.id }),
    )
    .await
}

pub struct KbIngestExecutor;

#[async_trait::async_trait]
impl JobExecutor for KbIngestExecutor {
    fn kind(&self) -> JobKind {
        JobKind::KbIngest
    }

    async fn run(&self, ctx: JobCtx, input: Value) -> Result<JobOutcome, String> {
        let input: Input =
            serde_json::from_value(input).map_err(|e| format!("kb_ingest input: {e}"))?;
        let outcome = run(&ctx, input.kb_id).await?;
        if matches!(outcome, JobOutcome::Done(_)) {
            hand_on(ctx.state.clone(), input.kb_id, ctx.id);
        }
        Ok(outcome)
    }
}

/// The upload race, closed. A file uploaded just as this job ends finds the
/// job still registered, so its own spawn answers `AlreadyRunning` — while
/// the job has already looked for pending files for the last time. Nothing
/// would ever pick that file up. So a job that finished cleanly hands on: once
/// it has left the registry, anything still `pending` gets a job of its own.
/// It does not matter which side of the job's last look an upload landed on.
/// A finishing re-embed hands on the same way (`own_job` is then its id).
///
/// A held or cancelled job does not hand on (its files wait for Resume on
/// purpose), and neither does a base whose *other* job is running (that job
/// ingests what is pending when it finishes).
///
/// The wait for the finished job to leave the registry backs off and is
/// bounded ([`HAND_ON_BACKOFF`]); giving up is logged and written on the
/// waiting files, so the base says why they wait and Resume starts them.
pub fn hand_on(state: SharedState, kb_id: i64, own_job: i64) {
    tokio::spawn(async move {
        if let Err(why) = hand_on_inner(&state, kb_id, own_job).await {
            tracing::error!("kb {kb_id}: {why}");
            let note = format!("{why} — Resume starts them");
            let _ = store::note_pending(&state.knowledge.pool, kb_id, &note).await;
        }
    });
}

/// Waits between looks at a finishing job still in the registry, or between
/// tries of a spawn that failed. The last one repeats; the count is the bound.
const HAND_ON_BACKOFF: [u64; 12] = [
    20, 40, 80, 160, 320, 640, 1280, 2000, 2000, 2000, 2000, 2000,
];

async fn hand_on_inner(state: &SharedState, kb_id: i64, own_job: i64) -> Result<(), String> {
    let mut last = String::new();
    for wait in HAND_ON_BACKOFF {
        let pending = store::files_in_status(&state.knowledge.pool, kb_id, "pending")
            .await
            .map_err(|e| {
                format!("could not look for files that arrived as the last job ended: {e}")
            })?;
        if pending.is_empty() {
            return Ok(());
        }
        if state
            .jobs
            .live_by_key(JobKind::KbReembed, &job_key(kb_id))
            .is_some_and(|j| j.id != own_job)
        {
            return Ok(());
        }
        let kb = match store::get_kb(&state.knowledge.pool, kb_id).await {
            Ok(Some(kb)) => kb,
            _ => return Ok(()),
        };
        match spawn(state, &kb).await {
            // Ours is still winding down: look again once it is gone.
            Ok(Spawn::AlreadyRunning(id)) if id == own_job => {
                last = format!("job {own_job} did not leave the queue");
            }
            // Someone else's ingest is on the base already: it looks again
            // for pending files before it ends, and hands on itself.
            Ok(_) => return Ok(()),
            Err(e) => last = e,
        }
        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
    }
    Err(format!(
        "could not start the ingest of files that arrived as the last job ended ({last})"
    ))
}

/// Why one file did not finish.
enum FileEnd {
    Held(String),
    Canceled,
    Failed(String),
}

impl From<Stop> for FileEnd {
    fn from(s: Stop) -> Self {
        match s {
            Stop::Held(m) => Self::Held(m),
            Stop::Canceled => Self::Canceled,
            Stop::Failed(m) => Self::Failed(m),
        }
    }
}

/// What finishing one file added up to.
#[derive(Default)]
struct FileDone {
    chunks: u64,
    embedded: u64,
    reused: u64,
}

async fn run(ctx: &JobCtx, kb_id: i64) -> Result<JobOutcome, String> {
    let state = &ctx.state;
    let pool = &state.knowledge.pool;
    let kb = store::get_kb(pool, kb_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("knowledge base {kb_id} no longer exists"))?;
    // Residue of a crash, or of a job this one replaces: nothing is running
    // those files any more.
    store::reset_ingesting(pool, Some(kb.id))
        .await
        .map_err(|e| e.to_string())?;

    let mut detail = Detail {
        kb_id: kb.id,
        kb: kb.name.clone(),
        ..Default::default()
    };
    let mut done_files = 0u64;
    let report = |done: u64, pending: u64, stage: &str, d: &Detail| JobProgress {
        done,
        total: Some(done + pending),
        stage: stage.to_string(),
        detail: serde_json::to_value(d).unwrap_or(Value::Null),
    };

    // The pinned embedder, and the hold, before any file is touched.
    let embedder = match open_embedder(state, &kb).await {
        Ok(e) => e,
        Err(FileEnd::Held(why)) => return Ok(held(ctx, &kb, why, &detail).await),
        Err(FileEnd::Failed(why)) => {
            let _ = store::note_pending(pool, kb.id, &why).await;
            return Err(why);
        }
        Err(FileEnd::Canceled) => return Ok(JobOutcome::Canceled),
    };

    loop {
        // Re-read every round: a file uploaded while this job runs is picked
        // up by it rather than waiting for the next one.
        let pending = store::files_in_status(pool, kb.id, "pending")
            .await
            .map_err(|e| e.to_string())?;
        let Some(file) = pending.first().cloned() else {
            break;
        };
        let remaining = pending.len() as u64;
        if ctx.canceled() {
            let _ = store::note_pending(pool, kb.id, CANCELED).await;
            ctx.progress(report(done_files, remaining, "canceled", &detail))
                .await;
            return Ok(JobOutcome::CanceledWith(json!(detail)));
        }
        detail.file = Some(file.name.clone());
        detail.file_stage = "reading".into();
        ctx.progress(report(done_files, remaining, "ingesting", &detail))
            .await;
        // Every write about this file names the version (sha) the job took:
        // a file re-uploaded meanwhile is a new version that stays `pending`
        // for the next look, whatever becomes of this one.
        let took = store::set_file_status_for(pool, file.id, &file.sha256, "ingesting", None)
            .await
            .map_err(|e| e.to_string())?;
        if !took {
            continue;
        }

        let mut superseded = false;
        match ingest_file(ctx, &kb, &file, &embedder, &mut detail).await {
            Ok(Some(d)) => {
                detail.ready += 1;
                detail.chunks += d.chunks;
                detail.embedded += d.embedded;
                detail.reused += d.reused;
            }
            Ok(None) => superseded = true,
            Err(FileEnd::Failed(why)) => {
                detail.failed += 1;
                superseded =
                    !store::set_file_status_for(pool, file.id, &file.sha256, "failed", Some(&why))
                        .await
                        .map_err(|e| e.to_string())?;
            }
            Err(FileEnd::Held(why)) => {
                store::set_file_status_for(pool, file.id, &file.sha256, "pending", Some(&why))
                    .await
                    .map_err(|e| e.to_string())?;
                return Ok(held(ctx, &kb, why, &detail).await);
            }
            Err(FileEnd::Canceled) => {
                store::set_file_status_for(pool, file.id, &file.sha256, "pending", Some(CANCELED))
                    .await
                    .map_err(|e| e.to_string())?;
                let _ = store::note_pending(pool, kb.id, CANCELED).await;
                return Ok(JobOutcome::CanceledWith(json!(detail)));
            }
        }
        if superseded {
            // The old bytes were kept for this job to read; nothing names
            // them now.
            originals::remove_if_unused(pool, &state.data_dir, &file.sha256).await;
        }
        done_files += 1;
        detail.file_stage = "done".into();
        ctx.progress(report(done_files, remaining - 1, "ingesting", &detail))
            .await;
    }

    detail.file = None;
    detail.file_stage.clear();
    // A base whose model changed is `re_embed_required` until its last chunk
    // has a vector again — which an ingest that replaced every chunk has done.
    let now = store::get_kb(pool, kb.id)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(now) = now.filter(|k| k.status == "re_embed_required") {
        let missing = store::count_unembedded(pool, now.id, store::Unembedded::Live)
            .await
            .map_err(|e| e.to_string())?;
        if missing == 0 {
            store::set_kb_status(pool, now.id, "ready")
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    ctx.progress(report(done_files, 0, "done", &detail)).await;
    Ok(JobOutcome::Done(json!(detail)))
}

/// The reason a cancelled job leaves on the files it did not get to.
const CANCELED: &str = "ingestion was cancelled before this file — Resume continues it";

/// Stop for a GPU hold: the job ends `failed` with the hold's own message,
/// and every waiting file carries it as its reason. Nothing is lost — the
/// files stay `pending` for Resume.
async fn held(ctx: &JobCtx, kb: &Kb, why: String, detail: &Detail) -> JobOutcome {
    let reason = hold_reason(&ctx.state, &why);
    let _ = store::note_pending(&ctx.state.knowledge.pool, kb.id, &reason).await;
    JobOutcome::FailedWith {
        error: reason,
        value: json!(detail),
    }
}

/// "GPU hold is on: …" (or a benchmark's lease), the refusal itself, and
/// what happens next.
pub(crate) fn hold_reason(state: &SharedState, why: &str) -> String {
    let head = match state.snapshot().gpu_block() {
        Some(crate::bench::lease::GpuBlock::Benchmark(_)) => "a benchmark has the GPU",
        _ => "GPU hold is on",
    };
    format!("{head}: {why}. The files stay pending; Resume (or the next upload) continues.")
}

/// The base's pinned embedder, refused under a hold before anything is read.
async fn open_embedder(state: &SharedState, kb: &Kb) -> Result<InProcessEmbedder, FileEnd> {
    let e = InProcessEmbedder::for_identity(
        state.clone(),
        &kb.embed_identity(),
        &kb.label(),
        "change its embedding model (which re-embeds it)",
    )
    .await
    .map_err(|e| FileEnd::Failed(e.to_string()))?;
    check_hold(state, e.alias())?;
    Ok(e)
}

fn check_hold(state: &SharedState, alias: &str) -> Result<(), FileEnd> {
    refuse_if_held(state, alias, HOLD_WHY).map_err(|e| FileEnd::Held(e.to_string()))
}

/// The reason clause a held embedding refusal carries.
pub(crate) const HOLD_WHY: &str =
    "a knowledge base is pinned to the one model it was embedded with, so \
    it will not embed through a fallback either";

/// A chunk's stable id: sha256(kb ‖ file row ‖ file sha256 ‖ seq ‖ payload).
/// The file row is in it so two files never share an id: a replaced file whose
/// re-ingest failed still holds its old chunks, and the old bytes uploaded
/// again under another name must not collide with them.
pub fn chunk_id(kb_id: i64, file_id: i64, file_sha: &str, seq: usize, payload: &str) -> String {
    let mut h = Sha256::new();
    h.update(kb_id.to_le_bytes());
    h.update([0u8]);
    h.update(file_id.to_le_bytes());
    h.update([0u8]);
    h.update(file_sha.as_bytes());
    h.update([0u8]);
    h.update((seq as u64).to_le_bytes());
    h.update([0u8]);
    h.update(payload.as_bytes());
    hex::encode(h.finalize())
}

/// Ingest one file. `None` when the row was re-uploaded (or deleted) while
/// this ran: nothing was written, and the new version is still `pending`.
async fn ingest_file(
    ctx: &JobCtx,
    kb: &Kb,
    file: &KbFile,
    embedder: &InProcessEmbedder,
    detail: &mut Detail,
) -> Result<Option<FileDone>, FileEnd> {
    let state = &ctx.state;
    let pool = &state.knowledge.pool;
    let bytes = originals::read(&state.data_dir, &file.sha256)
        .await
        .map_err(FileEnd::Failed)?;
    let cancel = ctx.cancel_signal();
    let canceled = {
        let cancel = cancel.clone();
        move || cancel.load(std::sync::atomic::Ordering::Relaxed)
    };
    let text = sections::read(state, &kb.vision_alias, Bytes::from(bytes), &canceled).await?;

    detail.file_stage = "chunking".into();
    ctx.progress_detail(json!(detail)).await;
    let assembled = chunk::assemble(text.parts);
    let sizes = Sizes {
        chunk_tokens: kb.chunk_tokens.max(1) as usize,
        overlap: kb.chunk_overlap.max(0) as usize,
    };
    // Chunks are sized with the embedding model's own tokenizer where the
    // gateway can reach it, so `chunk_tokens` holds for what the model sees.
    let (counter, tokenizer) = limit::calibrate(state, embedder.alias(), &assembled.text).await;
    detail.tokenizer = tokenizer;
    // Chunking is CPU work that grows with the file: on a blocking thread, so
    // the runtime keeps serving, and it stops when the job is cancelled.
    let chunk::Assembled {
        text: assembled_text,
        sections: assembled_sections,
    } = assembled;
    let drafts = {
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            chunk::chunk_until(&assembled_sections, sizes, &*counter, &|| {
                cancel.load(std::sync::atomic::Ordering::Relaxed)
            })
        })
        .await
        .map_err(|e| FileEnd::Failed(format!("chunking failed: {e}")))?
        .ok_or(FileEnd::Canceled)?
    };
    let mut chunks: Vec<NewKbChunk> = drafts
        .iter()
        .enumerate()
        .map(|(seq, d)| NewKbChunk {
            id: chunk_id(kb.id, file.id, &file.sha256, seq, &d.payload),
            seq: seq as i64,
            page: d.page.map(i64::from),
            heading_path: d.heading_path.clone(),
            span_start: d.span.0 as i64,
            span_end: d.span.1 as i64,
            payload: d.payload.clone(),
            // Stored (and budgeted) in the gateway's one unit, tiktoken.
            tokens: TiktokenCounter.count(&chunk::embed_text(&d.heading_path, &d.payload)) as i64,
            embedding: None,
        })
        .collect();

    // Vectors of regions that did not change since the last ingest.
    let ids: Vec<String> = chunks.iter().map(|c| c.id.clone()).collect();
    let mut stored = store::stored_vectors(pool, kb.id, &ids)
        .await
        .map_err(|e| FileEnd::Failed(e.to_string()))?;
    let mut done = FileDone::default();
    for c in &mut chunks {
        if let Some(v) = stored.remove(&c.id).filter(|v| v.len() == kb.dims()) {
            c.embedding = Some(v);
            done.reused += 1;
        }
    }

    detail.file_stage = "embedding".into();
    ctx.progress_detail(json!(detail)).await;
    let batch = state.snapshot().settings.docs_embed_batch.max(1) as usize;
    let todo: Vec<usize> = (0..chunks.len())
        .filter(|i| chunks[*i].embedding.is_none())
        .collect();
    // A chunk the model refused as too large, replaced by its halves.
    let mut split_into: HashMap<usize, Vec<NewKbChunk>> = HashMap::new();
    let mut splits = 0usize;
    // Where halving stops: a fraction of the model's own limit (the base's
    // chunk size when the limit is unknown), never below the floor.
    let min_piece = ingest_split::min_piece_tokens(
        limit::input_limit(state, embedder.alias())
            .await
            .tokens
            .unwrap_or(kb.chunk_tokens.max(0) as u64),
    );
    for group in todo.chunks(batch) {
        if ctx.canceled() {
            return Err(FileEnd::Canceled);
        }
        check_hold(state, embedder.alias())?;
        let texts: Vec<String> = group
            .iter()
            .map(|i| chunk::embed_text(&chunks[*i].heading_path, &chunks[*i].payload))
            .collect();
        match embedder.embed(&texts).await {
            Ok(vectors) => {
                for (i, v) in group.iter().zip(vectors) {
                    chunks[*i].embedding = Some(v);
                    done.embedded += 1;
                }
            }
            // One input over the model's limit fails the whole request: go
            // chunk by chunk, and split the one(s) it refuses.
            Err(e) if ingest_split::is_oversize(&e.to_string()) => {
                for i in group {
                    let first = chunks[*i].clone();
                    let pieces = embed_splitting(
                        ctx,
                        embedder,
                        first,
                        &assembled_text,
                        min_piece,
                        &mut splits,
                        &mut done,
                    )
                    .await?;
                    if pieces.len() == 1 && pieces[0].id == chunks[*i].id {
                        chunks[*i].embedding = pieces.into_iter().next().and_then(|p| p.embedding);
                    } else {
                        split_into.insert(*i, pieces);
                    }
                }
            }
            Err(e) => return Err(embed_failure(state, embedder, &e.to_string())),
        }
    }
    if !split_into.is_empty() {
        chunks = chunks
            .into_iter()
            .enumerate()
            .flat_map(|(i, c)| split_into.remove(&i).unwrap_or_else(|| vec![c]))
            .collect();
        for (seq, c) in chunks.iter_mut().enumerate() {
            c.seq = seq as i64;
        }
    }
    done.chunks = chunks.len() as u64;

    let mut notes = text.notes;
    if splits > 0 {
        notes.push(format!(
            "{splits} chunk(s) were refused by '{}' as larger than one input and split in \
             halves — the model takes fewer tokens than chunk_tokens {} assumed; lower \
             chunk_tokens or raise the model's -ub/-b",
            embedder.alias(),
            kb.chunk_tokens
        ));
    }
    let ingested = Ingested {
        pages: text.pages,
        skipped_pages: text.skipped_pages,
        notes,
        text: assembled_text,
    };
    let finished = store::finish_file(
        pool,
        kb.id,
        file.id,
        &file.sha256,
        kb.dims(),
        &chunks,
        &ingested,
    )
    .await
    .map_err(FileEnd::Failed)?;
    Ok(finished.then_some(done))
}

/// An embedding error as the end of a file: a hold switched on mid-file is
/// still a hold, not a broken file.
fn embed_failure(state: &SharedState, embedder: &InProcessEmbedder, e: &str) -> FileEnd {
    if let Err(FileEnd::Held(h)) = check_hold(state, embedder.alias()) {
        FileEnd::Held(h)
    } else {
        FileEnd::Failed(format!("embedding with {}: {e}", embedder.alias()))
    }
}

/// Embed one chunk on its own; when the model refuses it as too large, split
/// it in halves and go on with each, recursively, until the pieces are taken.
/// A piece of at most `min_piece` tokens ([`ingest_split::min_piece_tokens`]),
/// or one that cannot be cut any more, fails the file with the model's own
/// refusal: a refusal of something that small is not about its size, and
/// halving on would cost ~2 embed calls per byte. Returns the pieces in
/// reading order, each with its vector.
async fn embed_splitting(
    ctx: &JobCtx,
    embedder: &InProcessEmbedder,
    first: NewKbChunk,
    text: &str,
    min_piece: u64,
    splits: &mut usize,
    done: &mut FileDone,
) -> Result<Vec<NewKbChunk>, FileEnd> {
    let state = &ctx.state;
    let mut out = Vec::new();
    let mut stack = vec![first];
    while let Some(mut c) = stack.pop() {
        if ctx.canceled() {
            return Err(FileEnd::Canceled);
        }
        check_hold(state, embedder.alias())?;
        let input = [chunk::embed_text(&c.heading_path, &c.payload)];
        match embedder.embed(&input).await {
            Ok(mut v) => {
                c.embedding = v.pop();
                done.embedded += 1;
                out.push(c);
            }
            Err(e) if ingest_split::is_oversize(&e.to_string()) => {
                let halved = if c.tokens as u64 <= min_piece {
                    None
                } else {
                    ingest_split::halves(&c, text)
                };
                match halved {
                    Some((a, b)) => {
                        *splits += 1;
                        stack.push(b);
                        stack.push(a);
                    }
                    None => {
                        return Err(FileEnd::Failed(format!(
                            "embedding with {}: a piece of {} bytes ({} tokens) is still \
                             refused as too large and is not cut further (pieces of at most \
                             {min_piece} tokens are not split): {e}",
                            embedder.alias(),
                            c.payload.len(),
                            c.tokens
                        )))
                    }
                }
            }
            Err(e) => return Err(embed_failure(state, embedder, &e.to_string())),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_ids_depend_on_base_file_position_and_text() {
        let a = chunk_id(1, 7, "f", 0, "x");
        assert_eq!(a, chunk_id(1, 7, "f", 0, "x"));
        assert_ne!(a, chunk_id(2, 7, "f", 0, "x"));
        assert_ne!(a, chunk_id(1, 8, "f", 0, "x"), "another file row");
        assert_ne!(a, chunk_id(1, 7, "g", 0, "x"));
        assert_ne!(a, chunk_id(1, 7, "f", 1, "x"));
        assert_ne!(a, chunk_id(1, 7, "f", 0, "y"));
        assert_eq!(a.len(), 64);
    }
}
