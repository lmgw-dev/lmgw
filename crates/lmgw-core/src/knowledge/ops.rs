//! What the owner does to a knowledge base (chat-complete design §9.2, §9.5),
//! as functions the dashboard's `/api/knowledge/*` routes call — and anything
//! else later, so there is one implementation of each.
//!
//! Every refusal is a sentence the owner can act on: a model that does not
//! resolve, a chunk size its embedding model cannot take (refused, never
//! clamped), a file that is not knowledge-base material, a job that would
//! race another on the same base.

use std::collections::HashSet;

use bytes::Bytes;
use quickdoc_core::embed::Embedder;
use serde_json::Value;

use crate::jobs::{JobKind, JobView, Spawn};
use crate::quickdoc::{alias_for_identity, InProcessEmbedder, InProcessReranker};
use crate::state::SharedState;

use lmgw_api_types::knowledge::{self as api, UploadVerdict};

use super::store::{self, Kb, KbCounts, KbEdit, KbFile, NewKb, NewKbFile};
use super::{ingest, job_key, originals, reembed, sections};

/// `chunk_tokens` when a new base names none (§13).
pub const DEFAULT_CHUNK_TOKENS: i64 = 512;
/// `chunk_overlap` when a new base names none (§13).
pub const DEFAULT_CHUNK_OVERLAP: i64 = 64;

/// A base as the list, the detail page and `kb__list` show it; the wire form
/// is [`super::wire::base`].
#[derive(Debug, Clone)]
pub struct KbView {
    pub kb: Kb,
    /// `upstream/model (Nd)` — the pin, as a docs corpus shows its own.
    pub embed_identity: String,
    pub counts: KbCounts,
    /// What the base's vectors cost resident: embedded chunks × dims × 2.
    pub resident_bytes: i64,
    /// The embedding model's context in tokens, when this gateway knows it —
    /// what `chunk_tokens` is checked against.
    pub embed_context: Option<u64>,
    /// What one embedding input may hold — `min(context, ubatch)` for a local
    /// aux model — and where that number comes from. `chunk_tokens` (heading
    /// path included) is checked against it.
    pub embed_input_limit: Option<u64>,
    /// One sentence: where `embed_input_limit` comes from.
    pub embed_input_limit_source: String,
    /// What counts the tokens: `model` | `tiktoken` | `tiktoken_guess`.
    pub embed_tokenizer: &'static str,
    /// Whether a model on this gateway still resolves to the pin.
    pub embed_resolvable: bool,
    /// The live job on this base, or the last one that ran.
    pub job: Option<JobView>,
    /// One sentence each: what the owner should know about this base now.
    pub notes: Vec<String>,
}

/// `POST /api/knowledge/bases`'s body and `…/settings`'s: the wire types.
pub use lmgw_api_types::knowledge::{
    CreateKnowledgeBase as CreateKb, EditKnowledgeBase as EditKbRequest,
};

async fn kb_or_err(state: &SharedState, id: i64) -> Result<Kb, String> {
    store::get_kb(&state.knowledge.pool, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no knowledge base {id}"))
}

async fn file_or_err(state: &SharedState, id: i64) -> Result<KbFile, String> {
    store::get_file(&state.knowledge.pool, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no knowledge-base file {id}"))
}

/// The embedding model's context length, when this gateway knows it: a
/// local aux model's planned context, or what the upstream's catalog says.
pub async fn embed_context(state: &SharedState, alias: &str) -> Option<u64> {
    crate::capabilities::exposed::exposed_entry(state, alias)
        .await
        .and_then(|e| e.context_length)
}

/// Check chunk sizes against each other and against the embedding model's
/// real context. Larger is refused, never clamped; an unknown context is said
/// and accepted.
async fn check_sizes(
    state: &SharedState,
    embed_alias: &str,
    chunk_tokens: i64,
    overlap: i64,
) -> Result<Option<String>, String> {
    if chunk_tokens < 1 {
        return Err(format!(
            "chunk_tokens must be at least 1 (got {chunk_tokens})"
        ));
    }
    if overlap < 0 || overlap >= chunk_tokens {
        return Err(format!(
            "chunk_overlap must be at least 0 and smaller than chunk_tokens ({chunk_tokens}); \
             got {overlap}"
        ));
    }
    let limit = super::limit::input_limit(state, embed_alias).await;
    if let Some(why) = super::limit::refusal(&limit, embed_alias, chunk_tokens) {
        return Err(why);
    }
    Ok(super::limit::notes(&limit, embed_alias).into_iter().next())
}

fn opt_alias(v: &Option<String>) -> String {
    v.as_deref().map(str::trim).unwrap_or_default().to_string()
}

/// Refuse a rerank alias that does not resolve or cannot rank.
fn check_rerank(state: &SharedState, alias: &str) -> Result<(), String> {
    if alias.is_empty() {
        return Ok(());
    }
    InProcessReranker::for_alias(state.clone(), alias)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Refuse a vision alias that does not resolve, or that says it cannot see.
/// A model whose capabilities are unknown is taken at its word.
async fn check_vision(state: &SharedState, alias: &str) -> Result<(), String> {
    if alias.is_empty() {
        return Ok(());
    }
    state
        .snapshot()
        .resolve(alias)
        .map_err(|e| format!("the vision model '{alias}' does not resolve: {e}"))?;
    let vision = crate::capabilities::exposed::exposed_entry(state, alias)
        .await
        .and_then(|e| e.capabilities)
        .and_then(|c| c.vision);
    if vision == Some(false) {
        return Err(format!(
            "'{alias}' does not take image input, so it cannot read a page without text — \
             pick a model with vision, or leave the vision model empty to skip such pages"
        ));
    }
    Ok(())
}

pub async fn create(state: &SharedState, req: CreateKb) -> Result<KbView, String> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err("a knowledge base needs a name".into());
    }
    let embed_alias = req.embed_alias.trim().to_string();
    if embed_alias.is_empty() {
        return Err("a knowledge base needs an embedding model (embed_alias)".into());
    }
    let chunk_tokens = req.chunk_tokens.unwrap_or(DEFAULT_CHUNK_TOKENS);
    let chunk_overlap = req.chunk_overlap.unwrap_or(DEFAULT_CHUNK_OVERLAP);
    let rerank_alias = opt_alias(&req.rerank_alias);
    let vision_alias = opt_alias(&req.vision_alias);
    check_rerank(state, &rerank_alias)?;
    check_vision(state, &vision_alias).await?;
    check_sizes(state, &embed_alias, chunk_tokens, chunk_overlap).await?;
    if store::get_kb_by_name(&state.knowledge.pool, &name)
        .await
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Err(format!("a knowledge base named '{name}' already exists"));
    }
    // The identity this base will be pinned to, learnt by embedding a probe.
    let embedder = InProcessEmbedder::probe(state.clone(), &embed_alias)
        .await
        .map_err(|e| e.to_string())?;
    let id = store::insert_kb(
        &state.knowledge.pool,
        &NewKb {
            name,
            description: req.description.unwrap_or_default().trim().to_string(),
            embed_alias,
            embed: embedder.identity(),
            rerank_alias,
            vision_alias,
            chunk_tokens,
            chunk_overlap,
            mcp_visible: req.mcp_visible.unwrap_or(true),
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    view(state, &kb_or_err(state, id).await?).await
}

/// What an edit changed besides the row.
#[derive(Debug, Clone)]
pub struct EditOutcome {
    pub kb: KbView,
    /// The re-embed a model change started.
    pub reembed_job: Option<i64>,
    /// The re-ingest a chunk-size change started.
    pub ingest_job: Option<i64>,
    /// Files a chunk-size change sent back to `pending`. A model change
    /// decides in its job ([`reembed`]'s first stage) which files to
    /// re-chunk; the job's stage and detail say which.
    pub rechunk_files: u64,
}

pub async fn edit(state: &SharedState, id: i64, req: EditKbRequest) -> Result<EditOutcome, String> {
    let kb = kb_or_err(state, id).await?;
    let pool = &state.knowledge.pool;

    let name = req.name.as_deref().map(str::trim).map(str::to_string);
    if name.as_deref() == Some("") {
        return Err("a knowledge base needs a name".into());
    }
    if let Some(n) = &name {
        if let Some(other) = store::get_kb_by_name(pool, n)
            .await
            .map_err(|e| e.to_string())?
        {
            if other.id != kb.id {
                return Err(format!("a knowledge base named '{n}' already exists"));
            }
        }
    }
    let rerank_alias = req.rerank_alias.as_ref().map(|a| a.trim().to_string());
    if let Some(a) = &rerank_alias {
        check_rerank(state, a)?;
    }
    let vision_alias = req.vision_alias.as_ref().map(|a| a.trim().to_string());
    if let Some(a) = &vision_alias {
        check_vision(state, a).await?;
    }

    let new_embed = req
        .embed_alias
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty() && *a != kb.embed_alias)
        .map(str::to_string);
    let chunk_tokens = req.chunk_tokens.unwrap_or(kb.chunk_tokens);
    let chunk_overlap = req.chunk_overlap.unwrap_or(kb.chunk_overlap);
    let sizes_changed = chunk_tokens != kb.chunk_tokens || chunk_overlap != kb.chunk_overlap;
    // The busy check comes first: nothing below — not even the probe, which
    // loads the new model — runs against a base that has a job.
    if new_embed.is_some() || sizes_changed {
        refuse_if_busy(state, &kb, None)?;
    }
    let embed_for_check = new_embed.clone().unwrap_or_else(|| kb.embed_alias.clone());
    if sizes_changed || new_embed.is_some() {
        check_sizes(state, &embed_for_check, chunk_tokens, chunk_overlap).await?;
    }

    // A model change probes the new model first: a refusal (a hold, a
    // reranker picked by mistake) leaves the base exactly as it was. Whether
    // the stored chunks fit the new model is not decided here — measuring
    // them is work that grows with the base, so the re-embed job does it as
    // its first stage ([`reembed`]) and re-chunks only the files that overrun.
    let mut repin: Option<(String, quickdoc_core::embed::EmbedIdentity)> = None;
    let mut rename: Option<(String, quickdoc_core::embed::EmbedIdentity)> = None;
    if let Some(alias) = &new_embed {
        let probe = InProcessEmbedder::probe(state.clone(), alias)
            .await
            .map_err(|e| e.to_string())?;
        if probe.identity() == kb.embed_identity() {
            // Another name for the same model: nothing to re-embed.
            rename = Some((alias.clone(), probe.identity()));
        } else {
            repin = Some((alias.clone(), probe.identity()));
        }
    }

    store::update_kb(
        pool,
        kb.id,
        &KbEdit {
            name,
            description: req.description.map(|d| d.trim().to_string()),
            rerank_alias,
            vision_alias,
            chunk_tokens: req.chunk_tokens,
            chunk_overlap: req.chunk_overlap,
            mcp_visible: req.mcp_visible,
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    if let Some((alias, identity)) = &rename {
        store::set_kb_embed(pool, kb.id, alias, identity)
            .await
            .map_err(|e| e.to_string())?;
    }
    let kb = kb_or_err(state, kb.id).await?;

    // A model change that also changes the chunk sizes is one ingest job: the
    // pin moves and the old vectors go now, and the ingest embeds the new
    // chunks with the new model — re-embedding the old chunks first would
    // embed everything twice.
    let rechunk_all = sizes_changed;
    let mut rechunk_files = 0;
    let mut reembed_alias = None;
    let mut repinned_now = false;
    if let Some((alias, identity)) = repin {
        if rechunk_all {
            repinned_now = true;
            store::set_kb_embed(pool, kb.id, &alias, &identity)
                .await
                .map_err(|e| e.to_string())?;
            store::clear_embeddings(pool, kb.id)
                .await
                .map_err(|e| e.to_string())?;
            store::set_kb_status(pool, kb.id, "re_embed_required")
                .await
                .map_err(|e| e.to_string())?;
        } else {
            reembed_alias = Some(alias);
        }
    }
    let kb = kb_or_err(state, kb.id).await?;
    if rechunk_all {
        rechunk_files = store::mark_all_pending(pool, kb.id)
            .await
            .map_err(|e| e.to_string())?;
        if rechunk_files == 0 && repinned_now {
            // No file to re-chunk: a pin that moved has no chunks to fill.
            store::set_kb_status(pool, kb.id, "ready")
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    let mut reembed_job = None;
    let mut ingest_job = None;
    if let Some(alias) = reembed_alias {
        // The re-embed hands on to an ingest of whatever is pending when it
        // finishes, so a model change with new uploads needs one job.
        reembed_job = Some(reembed::spawn(state, &kb, Some(alias), true).await?.id());
    } else if rechunk_files > 0 {
        ingest_job = Some(ingest::spawn(state, &kb).await?.id());
    }
    Ok(EditOutcome {
        kb: view(state, &kb).await?,
        reembed_job,
        ingest_job,
        rechunk_files,
    })
}

/// The live job on a base, of either kind.
fn live_job(state: &SharedState, kb_id: i64) -> Option<JobView> {
    let key = job_key(kb_id);
    state
        .jobs
        .live_by_key(JobKind::KbIngest, &key)
        .or_else(|| state.jobs.live_by_key(JobKind::KbReembed, &key))
}

/// Refuse to start work that would race a job already on the base. `allow`
/// is the kind the caller is about to join rather than race.
fn refuse_if_busy(state: &SharedState, kb: &Kb, allow: Option<JobKind>) -> Result<(), String> {
    match live_job(state, kb.id) {
        Some(j) if Some(j.kind.as_str()) != allow.map(|k| k.as_str()) => Err(format!(
            "{} has a {} job running (job {}) — wait for it or cancel it first",
            kb.label(),
            j.kind,
            j.id
        )),
        _ => Ok(()),
    }
}

/// Start (or join) the ingest of a base's pending files, unless a re-embed
/// is running — which hands on to an ingest itself when it finishes.
pub async fn start_ingest(state: &SharedState, kb: &Kb) -> Result<Option<i64>, String> {
    if let Some(j) = live_job(state, kb.id) {
        return Ok(Some(j.id));
    }
    Ok(Some(ingest::spawn(state, kb).await?.id()))
}

/// Resume: fill in missing vectors if the base needs it (the re-embed then
/// ingests what is pending), else ingest the pending files.
///
/// The handler only records the intent and starts one job: measuring whether
/// the stored chunks fit the pinned model, and deciding which files to
/// re-chunk rather than re-embed, is the re-embed job's first stage
/// ([`reembed`]) — work that grows with the base does not run in a request.
/// The one decision it makes itself costs a single job row: the last re-embed
/// was refused by the model as larger than one input, whatever the
/// measurement said (a tokenizer's error). Then the files holding chunks
/// without a vector — and only those — are re-chunked.
pub async fn resume(state: &SharedState, kb_id: i64) -> Result<api::ResumeResult, String> {
    let kb = kb_or_err(state, kb_id).await?;
    if let Some(j) = live_job(state, kb.id) {
        return Ok(api::ResumeResult {
            job: Some(j.id),
            kind: Some(j.kind),
            already_running: Some(true),
            ..Default::default()
        });
    }
    let pool = &state.knowledge.pool;
    let unembedded = store::count_unembedded(pool, kb.id, store::Unembedded::Live)
        .await
        .map_err(|e| e.to_string())?;
    let pending = store::files_in_status(pool, kb.id, "pending")
        .await
        .map_err(|e| e.to_string())?
        .len();
    if kb.status == "re_embed_required" || unembedded > 0 {
        let refused = match last_job(state, kb.id).await {
            Some(j) if j.kind == JobKind::KbReembed.as_str() && j.status == "failed" => j
                .error
                .filter(|e| super::ingest_split::is_oversize(e))
                .map(|_| {
                    format!(
                        "the last re-embed was refused by '{}' as larger than one input",
                        kb.embed_alias
                    )
                }),
            _ => None,
        };
        if let Some(why) = refused {
            let ids: Vec<i64> = store::unembedded_files(pool, kb.id)
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|f| f.status == "ready")
                .map(|f| f.id)
                .collect();
            let n = store::mark_files_pending(pool, &ids, &format!("re-chunking: {why}"))
                .await
                .map_err(|e| e.to_string())?;
            if n > 0 {
                let s = ingest::spawn(state, &kb).await?;
                return Ok(api::ResumeResult {
                    job: Some(s.id()),
                    kind: Some(JobKind::KbIngest.as_str().to_string()),
                    message: Some(format!(
                        "re-chunking {n} file(s) for {}: {why}",
                        kb.embed_alias
                    )),
                    rechunk: Some(why),
                    ..Default::default()
                });
            }
        }
        let s = reembed::spawn(state, &kb, None, unembedded > 0).await?;
        return Ok(api::ResumeResult {
            job: Some(s.id()),
            kind: Some(JobKind::KbReembed.as_str().to_string()),
            message: Some(
                if unembedded > 0 {
                    "measuring the stored chunks against the model first — the files that do \
                     not fit are re-chunked, the rest re-embedded"
                } else {
                    "clearing the re-embed mark"
                }
                .to_string(),
            ),
            ..Default::default()
        });
    }
    if pending == 0 {
        return Ok(api::ResumeResult {
            message: Some(format!(
                "{} has nothing waiting — no job started",
                kb.label()
            )),
            ..Default::default()
        });
    }
    let s = ingest::spawn(state, &kb).await?;
    Ok(api::ResumeResult {
        job: Some(s.id()),
        kind: Some(JobKind::KbIngest.as_str().to_string()),
        already_running: Some(matches!(s, Spawn::AlreadyRunning(_))),
        ..Default::default()
    })
}

pub async fn cancel(state: &SharedState, kb_id: i64) -> Result<api::CancelResult, String> {
    let kb = kb_or_err(state, kb_id).await?;
    let Some(j) = live_job(state, kb.id) else {
        return Err(format!("{} has no job running", kb.label()));
    };
    let message = crate::jobs::cancel(state, j.id).await?;
    Ok(api::CancelResult { job: j.id, message })
}

/// Delete a base: its files, chunks and — where nothing else uses them —
/// originals.
pub async fn delete(state: &SharedState, kb_id: i64) -> Result<api::KnowledgeBaseDeleted, String> {
    let kb = kb_or_err(state, kb_id).await?;
    if let Some(j) = live_job(state, kb.id) {
        let _ = crate::jobs::cancel(state, j.id).await;
    }
    let counts = store::kb_counts_for(&state.knowledge.pool, &[kb.id])
        .await
        .map_err(|e| e.to_string())?
        .remove(&kb.id)
        .unwrap_or_default();
    let shas = store::delete_kb(&state.knowledge.pool, kb.id)
        .await
        .map_err(|e| e.to_string())?;
    state.knowledge.forget(kb.id);
    let unique: HashSet<String> = shas.into_iter().collect();
    for sha in &unique {
        originals::remove_if_unused(&state.knowledge.pool, &state.data_dir, sha).await;
    }
    Ok(api::KnowledgeBaseDeleted {
        deleted: kb.id,
        name: kb.name,
        files: counts.files,
        chunks: counts.chunks,
    })
}

/// One uploaded file's fate.
#[derive(Debug, Clone)]
pub struct UploadItem {
    pub name: String,
    pub outcome: UploadVerdict,
    pub file: Option<KbFile>,
    /// Why it was not added, or what it replaced.
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UploadOutcome {
    pub items: Vec<UploadItem>,
    /// The ingest job the upload started or joined.
    pub job: Option<i64>,
}

/// Add files to a base. Each is sniffed from its bytes (never its name):
/// images and audio are refused, the rest stored under its sha256 and queued
/// `pending`. The same bytes already in the base are skipped (`unchanged`
/// under the same name, `duplicate` under another); a known name with new
/// bytes replaces that file's content. Then the ingest starts — or joins the
/// one running.
pub async fn upload(
    state: &SharedState,
    kb_id: i64,
    files: Vec<(String, Bytes)>,
) -> Result<UploadOutcome, String> {
    let kb = kb_or_err(state, kb_id).await?;
    let pool = &state.knowledge.pool;
    let mut items = Vec::with_capacity(files.len());
    let mut queued = false;
    for (raw_name, bytes) in files {
        let name = clean_name(&raw_name);
        let refuse = |reason: String| UploadItem {
            name: name.clone(),
            outcome: UploadVerdict::Refused,
            file: None,
            reason: Some(reason),
        };
        if bytes.is_empty() {
            items.push(refuse("the file is empty".into()));
            continue;
        }
        let sniffed = match crate::extract::sniff_async(bytes.clone()).await {
            Ok(s) => s,
            Err(e) => {
                items.push(refuse(e.to_string()));
                continue;
            }
        };
        if let Some(why) = sections::accepts(&sniffed) {
            items.push(refuse(why));
            continue;
        }
        let sha = originals::sha256_hex(&bytes);
        if let Some(existing) = store::find_file_by_sha(pool, kb.id, &sha)
            .await
            .map_err(|e| e.to_string())?
        {
            let same_name = existing.name == name;
            // Only bytes that were *ingested* are unchanged. The same bytes
            // whose ingest failed are queued again (the owner uploading them
            // again is asking for exactly that); ones already queued are
            // said to be.
            if same_name && existing.status == "failed" {
                let why = existing.error.clone().unwrap_or_default();
                // The upload carries the bytes, so the original is stored
                // again (a no-op when it is there): a file that failed
                // because its original went missing ("could not be read —
                // upload it again") is repaired by exactly that.
                let held = originals::hold().await;
                originals::store(&state.data_dir, &bytes).await?;
                store::set_file_status(pool, existing.id, "pending", None)
                    .await
                    .map_err(|e| e.to_string())?;
                drop(held);
                queued = true;
                items.push(UploadItem {
                    name: name.clone(),
                    outcome: UploadVerdict::Replaced,
                    reason: Some(format!(
                        "the earlier attempt to ingest these bytes failed ({why}) — queued again"
                    )),
                    file: store::get_file(pool, existing.id)
                        .await
                        .map_err(|e| e.to_string())?,
                });
                continue;
            }
            items.push(UploadItem {
                name: name.clone(),
                outcome: if same_name {
                    UploadVerdict::Unchanged
                } else {
                    UploadVerdict::Duplicate
                },
                reason: Some(if same_name && existing.status != "ready" {
                    "the same file is already queued for ingestion — skipped".to_string()
                } else if same_name {
                    "the same file is already in this knowledge base — skipped".to_string()
                } else {
                    format!(
                        "the same bytes are already in this knowledge base as '{}' — skipped",
                        existing.name
                    )
                }),
                file: Some(existing),
            });
            continue;
        }
        let same_name = store::find_file_by_name(pool, kb.id, &name)
            .await
            .map_err(|e| e.to_string())?;
        // A file being ingested right now may be replaced: the job writes
        // only the version it took (`store::finish_file` checks the digest),
        // and the new one stays queued for the next look.
        let new = NewKbFile {
            kb_id: kb.id,
            name: name.clone(),
            kind: sniffed.kind.as_str().to_string(),
            sub: sniffed.sub.to_string(),
            mime: sniffed.mime.to_string(),
            size: bytes.len() as i64,
            sha256: sha,
        };
        // The original is stored and its row recorded under one lock, so a
        // delete of the same bytes elsewhere cannot remove the file between.
        let held = originals::hold().await;
        originals::store(&state.data_dir, &bytes).await?;
        let (outcome, id, reason) = match same_name {
            Some(old) => {
                store::replace_file(pool, old.id, &new)
                    .await
                    .map_err(|e| e.to_string())?;
                drop(held);
                originals::remove_if_unused(pool, &state.data_dir, &old.sha256).await;
                (
                    UploadVerdict::Replaced,
                    old.id,
                    Some(format!(
                        "replaced the earlier '{name}' ({} bytes); its chunks stay searchable \
                         until the new content is ingested{}",
                        old.size,
                        if old.status == "ingesting" {
                            " (it was being ingested — the new version follows right after)"
                        } else {
                            ""
                        }
                    )),
                )
            }
            None => {
                let id = store::insert_file(pool, &new)
                    .await
                    .map_err(|e| e.to_string())?;
                drop(held);
                (UploadVerdict::Added, id, None)
            }
        };
        queued = true;
        items.push(UploadItem {
            name,
            outcome,
            file: store::get_file(pool, id).await.map_err(|e| e.to_string())?,
            reason,
        });
    }
    let job = if queued {
        start_ingest(state, &kb).await?
    } else {
        None
    };
    Ok(UploadOutcome { items, job })
}

/// A file name as stored: its last path component, without control
/// characters. The name is a label; nothing on disk is ever named after it.
fn clean_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    match cleaned.trim() {
        "" => "unnamed".to_string(),
        n => n.to_string(),
    }
}

pub async fn delete_file(
    state: &SharedState,
    file_id: i64,
) -> Result<api::KnowledgeFileDeleted, String> {
    let file = file_or_err(state, file_id).await?;
    if file.status == "ingesting" {
        return Err(format!(
            "'{}' is being ingested right now — cancel the job first",
            file.name
        ));
    }
    store::delete_file(&state.knowledge.pool, file.id)
        .await
        .map_err(|e| e.to_string())?;
    originals::remove_if_unused(&state.knowledge.pool, &state.data_dir, &file.sha256).await;
    Ok(api::KnowledgeFileDeleted {
        deleted: file.id,
        name: file.name,
        chunks: file.chunk_count,
    })
}

/// Queue one file again — after its base gained a vision model, say, or
/// after it failed.
pub async fn reingest_file(
    state: &SharedState,
    file_id: i64,
) -> Result<api::KnowledgeFileRequeued, String> {
    let file = file_or_err(state, file_id).await?;
    if file.status == "ingesting" {
        return Err(format!("'{}' is being ingested right now", file.name));
    }
    let kb = kb_or_err(state, file.kb_id).await?;
    store::set_file_status(&state.knowledge.pool, file.id, "pending", None)
        .await
        .map_err(|e| e.to_string())?;
    let job = start_ingest(state, &kb).await?;
    Ok(api::KnowledgeFileRequeued { file: file.id, job })
}

pub async fn get(state: &SharedState, kb_id: i64) -> Result<KbView, String> {
    view(state, &kb_or_err(state, kb_id).await?).await
}

pub async fn files(state: &SharedState, kb_id: i64) -> Result<Vec<KbFile>, String> {
    let kb = kb_or_err(state, kb_id).await?;
    store::list_files(&state.knowledge.pool, kb.id)
        .await
        .map_err(|e| e.to_string())
}

pub async fn list(state: &SharedState) -> Result<Vec<KbView>, String> {
    let kbs = store::list_kbs(&state.knowledge.pool)
        .await
        .map_err(|e| e.to_string())?;
    // One count for all of them.
    let mut counts = store::kb_counts(&state.knowledge.pool)
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(kbs.len());
    for kb in &kbs {
        out.push(view_with(state, kb, counts.remove(&kb.id).unwrap_or_default()).await?);
    }
    Ok(out)
}

/// The last finished job on a base, of either kind.
async fn last_job(state: &SharedState, kb_id: i64) -> Option<JobView> {
    let key = job_key(kb_id);
    let mut best: Option<crate::store::JobRow> = None;
    for kind in [JobKind::KbIngest, JobKind::KbReembed] {
        if let Ok(mut rows) =
            crate::store::list_jobs_by_key(&state.db, kind.as_str(), &key, 1).await
        {
            if let Some(r) = rows.pop() {
                if best.as_ref().is_none_or(|b| r.id > b.id) {
                    best = Some(r);
                }
            }
        }
    }
    best.map(|r| JobView::from_row(&r))
}

/// The last job of one kind on a base.
async fn last_job_of(state: &SharedState, kb_id: i64, kind: JobKind) -> Option<JobView> {
    crate::store::list_jobs_by_key(&state.db, kind.as_str(), &job_key(kb_id), 1)
        .await
        .ok()?
        .pop()
        .map(|r| JobView::from_row(&r))
}

pub async fn view(state: &SharedState, kb: &Kb) -> Result<KbView, String> {
    let counts = store::kb_counts_for(&state.knowledge.pool, &[kb.id])
        .await
        .map_err(|e| e.to_string())?
        .remove(&kb.id)
        .unwrap_or_default();
    view_with(state, kb, counts).await
}

/// [`view`] with the base's counts already in hand.
pub async fn view_with(state: &SharedState, kb: &Kb, counts: KbCounts) -> Result<KbView, String> {
    let snap = state.snapshot();
    let resolvable = alias_for_identity(&snap, &kb.embed_identity()).is_some();
    let embed_context = embed_context(state, &kb.embed_alias).await;
    let limit = super::limit::input_limit(state, &kb.embed_alias).await;
    let live = live_job(state, kb.id);
    let job = match &live {
        Some(j) => Some(j.clone()),
        None => last_job(state, kb.id).await,
    };

    let mut notes = Vec::new();
    if !resolvable {
        notes.push(format!(
            "no model on this gateway resolves to {} any more — searches fall back to \
             keywords and new files cannot be embedded until the model is back or the base \
             is moved to another one",
            kb.embed_identity()
        ));
    }
    notes.extend(super::limit::notes(&limit, &kb.embed_alias));
    if let Some(max) = limit.tokens.filter(|m| kb.chunk_tokens as u64 > *m) {
        notes.push(format!(
            "chunk_tokens {} is above what '{}' takes per input ({max} tokens: {}) — files \
             fail to embed until the limit is raised or chunk_tokens lowered",
            kb.chunk_tokens, kb.embed_alias, limit.source
        ));
    }
    if live.is_none() && counts.pending > 0 {
        notes.push(format!(
            "{} file(s) waiting and no job running — Resume ingests them",
            counts.pending
        ));
    }
    // Chunks without a vector, split by whose they are: a failed file's stale
    // chunks are named as that file's problem (Resume cannot fill them and
    // does not try), the rest are what Resume re-embeds.
    let unembedded = if live.is_none() && counts.embedded < counts.chunks {
        store::unembedded_files(&state.knowledge.pool, kb.id)
            .await
            .map_err(|e| e.to_string())?
    } else {
        Vec::new()
    };
    let (failed_files, waiting_files): (Vec<_>, Vec<_>) =
        unembedded.iter().partition(|f| f.status == "failed");
    let waiting: i64 = waiting_files.iter().map(|f| f.chunks).sum();
    for f in &failed_files {
        notes.push(format!(
            "'{}' failed ({}) and its {} earlier chunk(s) have no vector — they are found by \
             keyword search only. Re-ingest the file once the cause is fixed, or delete it; \
             Resume does not retry it",
            f.name,
            f.error.as_deref().unwrap_or("no reason recorded"),
            f.chunks
        ));
    }
    if waiting > 0 {
        notes.push(format!(
            "{waiting} of {} chunks have no vector — Resume re-embeds them, first re-chunking \
             the files whose chunks are larger than '{}' takes",
            counts.chunks, kb.embed_alias
        ));
    } else if live.is_none() && kb.status == "re_embed_required" {
        notes.push(
            "the base is marked as needing a re-embed but no chunk that can be embedded \
             lacks a vector — Resume clears the mark"
                .to_string(),
        );
    }
    // A re-embed that failed says why, and the way out, right on the base.
    if live.is_none() {
        if let Some(j) = job
            .as_ref()
            .filter(|j| j.kind == JobKind::KbReembed.as_str() && j.status == "failed")
        {
            if let Some(e) = &j.error {
                notes.push(format!("the last re-embed failed: {e}"));
            }
        }
    }
    // The re-embed's first stage decided which files to re-chunk; while they
    // wait for their ingest, the base says what was decided and how.
    if counts.pending > 0 {
        let reembed = match &live {
            Some(j) if j.kind == JobKind::KbReembed.as_str() => Some(j.clone()),
            Some(_) => None,
            None => last_job_of(state, kb.id, JobKind::KbReembed).await,
        };
        if let Some(j) = reembed {
            let files: Vec<&str> = j.detail["rechunk_files"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if !files.is_empty() {
                notes.push(format!(
                    "{} file(s) are re-chunked for '{}' because {} ({}): {}",
                    files.len(),
                    kb.embed_alias,
                    j.detail["rechunk_why"]
                        .as_str()
                        .unwrap_or("their chunks overrun it"),
                    j.detail["measure"].as_str().unwrap_or("measured"),
                    files.join(", ")
                ));
            }
        }
    }
    notes.extend(rerank_note(state, kb).await);
    Ok(KbView {
        embed_identity: kb.embed_identity().to_string(),
        resident_bytes: counts.embedded * kb.embed_dims * 2,
        embed_context,
        embed_input_limit: limit.tokens,
        embed_input_limit_source: limit.source,
        embed_tokenizer: limit.tokenizer,
        embed_resolvable: resolvable,
        job,
        notes,
        counts,
        kb: kb.clone(),
    })
}

/// What a reranker's per-input limit means for this base: a pair is the query
/// and a chunk together, so a chunk near the limit leaves the query no room.
async fn rerank_note(state: &SharedState, kb: &Kb) -> Option<String> {
    let alias = kb.rerank_alias.trim();
    if alias.is_empty() {
        return None;
    }
    let limit = super::limit::input_limit(state, alias).await;
    let max = limit.tokens?;
    let need = kb.chunk_tokens as u64 + super::limit::PAIR_SPECIAL_TOKENS as u64;
    (need >= max).then(|| {
        format!(
            "the reranker '{alias}' takes {max} tokens per query + chunk pair ({}), and \
             chunks of {} tokens leave the query no room — such pairs are not reranked \
             (they keep their search order behind the reranked ones)",
            limit.source, kb.chunk_tokens
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_name_is_a_label_not_a_path() {
        assert_eq!(clean_name("../../etc/passwd"), "passwd");
        assert_eq!(
            clean_name("C:\\Users\\x\\Steuer 2025.pdf"),
            "Steuer 2025.pdf"
        );
        assert_eq!(clean_name("a\u{0}b.txt"), "ab.txt");
        assert_eq!(clean_name("  "), "unnamed");
    }
}
