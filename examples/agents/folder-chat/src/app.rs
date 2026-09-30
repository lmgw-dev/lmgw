//! [`FolderChat`] — the whole agent behind one handle, which is what the
//! phase-2 server holds in its state.
//!
//! It owns the gateway client, the [`IndexDir`], and the current retriever.
//! The retriever loads every vector of the folder into memory (quickdoc's f16
//! matrix — the reason the manifest asks for more memory than the default),
//! so it is built once per sync and **swapped in whole** behind an
//! `Arc<RwLock<…>>`.
//!
//! **Only the vectors are a snapshot.** A question asked during a sync ranks
//! by KNN against the previous sync's vectors, but BM25 and the excerpt text
//! are read live from the index file, which the sync writes batch by batch.
//! So a mid-sync answer can mix the two states: a chunk stored since can
//! match by its words while its vector is not searched yet, and a chunk
//! deleted since drops out even if its old vector still ranks. Each excerpt
//! is still whole — a stored chunk is never half-written — and the next
//! question after the sync sees one state again.
//!
//! **The swap briefly holds two matrices.** The new retriever loads every
//! vector before it replaces the old one, and a question still running holds
//! the old one until it ends, so for that moment the vectors are resident
//! twice — the memory peak is double the `resident_bytes` the status shows.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use quickdoc_core::embed::{Embedder, TokenCounter};
use quickdoc_core::retrieve::{Retriever, SearchParams};
use quickdoc_core::store;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::chat::{self, Answer, AskError, ChatRequest, Citation, Depths, Searched};
use crate::config::{AgentConfig, ConfigError, GatewayEnv};
use crate::gateway::{
    Gateway, GatewayEmbedder, GatewayError, EMBED_FALLBACK_PREFIX, GPU_HOLD_MESSAGE,
};
use crate::index::{IndexDir, IndexError};
use crate::scan::FileKind;
use crate::source::{ReadError, SourceText};
use crate::sync::{
    self, AbortKind, Events, SyncError, SyncEvent, SyncOptions, SyncReport, CORPUS_LIBRARY,
    CORPUS_VERSION,
};
use crate::vision::{self, VisionOptions};

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Index(#[from] IndexError),
}

/// The `folder_chat_meta` key the end of the last sync is recorded under, so
/// a restarted container can still say when the index was last brought up to
/// date.
const META_LAST_SYNC_MS: &str = "last_sync_finished_ms";

/// What the UI's status line shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub folder: String,
    pub index_path: String,
    pub syncing: bool,
    /// A retriever is loaded: questions can be answered.
    pub ready: bool,
    /// `upstream/model (dims)` of the loaded retriever.
    pub embed_model: Option<String>,
    /// Files in the loaded index.
    pub documents: Option<usize>,
    pub chunks: Option<usize>,
    /// Bytes of vectors resident in memory.
    pub resident_bytes: Option<usize>,
    /// When the last sync ended (finished or stopped), milliseconds since the
    /// epoch — kept in the index, so it survives a restart.
    pub last_sync_finished_ms: Option<u64>,
    /// Set while the loaded index has not been checked against the model
    /// its embedding alias names now: it was loaded while that model was
    /// held ([`FolderChat::load_existing`]). Says so, for the UI; the first
    /// question or sync after the hold checks it.
    #[serde(default)]
    pub unverified: Option<String>,
    /// The last sync's report, from this process's lifetime.
    pub last_sync: Option<SyncReport>,
}

/// What [`FolderChat::search`] found: the hits (as [`Citation`]s, `n` being
/// the rank) and what shaped them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResults {
    pub hits: Vec<Citation>,
    pub rerank_model: Option<String>,
    /// Why reranking did not run, when it was asked for.
    pub rerank_skipped: Option<String>,
    /// The depths the search ran with — `k_fts` and `k_vec` raised to `k`
    /// when it is larger.
    pub retrieval: Depths,
    /// The same depths in words ([`chat::retrieval_note`]), and a rerank
    /// dropped because a hold fallback answered it.
    pub notes: Vec<String>,
}

/// The retriever questions are answered from.
#[derive(Clone)]
struct Loaded {
    retriever: Arc<Retriever>,
    /// Its embedding model was checked against the index's fingerprint
    /// (`sync::probe_matches`). `false` only for an index loaded while the
    /// model was held.
    probed: bool,
}

/// What [`Status::unverified`] says while [`Loaded::probed`] is `false`.
const UNVERIFIED: &str = "loaded while the embedding model was held, so the model its alias \
                          names now is not checked yet; the first question or sync after the \
                          hold checks it";

pub struct FolderChat {
    config: AgentConfig,
    gateway: Gateway,
    index: IndexDir,
    retriever: Arc<RwLock<Option<Loaded>>>,
    last_sync: RwLock<Option<SyncReport>>,
    last_sync_finished_ms: RwLock<Option<u64>>,
    tokens: Arc<dyn TokenCounter>,
}

impl FolderChat {
    /// Open the index in `config.folder` (creating the hidden directory on the
    /// first run). Makes no gateway call.
    pub async fn open(config: AgentConfig, gateway: Gateway) -> Result<Self, OpenError> {
        let index = IndexDir::open(&config.folder).await?;
        let last_ms = index
            .meta(META_LAST_SYNC_MS)
            .await?
            .and_then(|v| v.parse().ok());
        Ok(Self {
            config,
            gateway,
            index,
            retriever: Arc::new(RwLock::new(None)),
            last_sync: RwLock::new(None),
            last_sync_finished_ms: RwLock::new(last_ms),
            tokens: crate::chunk::estimator(),
        })
    }

    /// Inside the container: `LMGW_*` and `/lmgw/input.json`.
    pub async fn from_env() -> Result<Self, OpenError> {
        let config = AgentConfig::from_env()?;
        let gateway = Gateway::new(GatewayEnv::from_env()?);
        Self::open(config, gateway).await
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    pub fn gateway(&self) -> &Gateway {
        &self.gateway
    }

    pub fn index(&self) -> &IndexDir {
        &self.index
    }

    pub async fn status(&self) -> Status {
        let loaded = self.retriever.read().await.clone();
        let unverified = loaded
            .as_ref()
            .filter(|l| !l.probed)
            .map(|_| UNVERIFIED.to_string());
        let r = loaded.map(|l| l.retriever);
        let documents = match &r {
            Some(r) => self.index.document_count(r.corpus().id).await.ok(),
            None => None,
        };
        Status {
            folder: self.index.folder().display().to_string(),
            index_path: self.index.db_path().display().to_string(),
            syncing: self.index.is_syncing(),
            ready: r.is_some(),
            embed_model: r.as_ref().map(|r| r.corpus().embed_identity().to_string()),
            documents,
            chunks: r.as_ref().map(|r| r.resident_vectors()),
            resident_bytes: r.as_ref().map(|r| r.resident_bytes()),
            last_sync_finished_ms: *self.last_sync_finished_ms.read().await,
            last_sync: self.last_sync.read().await.clone(),
            unverified,
        }
    }

    /// At start-up: load the index that is already on disk, if it was built
    /// with the configured embedding model, without syncing. `Ok(false)` when
    /// there is nothing usable yet (no index, or one for another model — the
    /// next sync rebuilds it).
    ///
    /// Connecting embeds one probe, which tells the model behind the alias
    /// apart from another of the same width (`sync::probe_matches`). **Under
    /// a hold** the probe cannot run, and the index is loaded as its corpus
    /// row pins it (alias and width, [`GatewayEmbedder::unprobed`]) rather
    /// than hidden as "no index yet": the owner sees the index, and a
    /// question's own embedding reports the hold while it lasts. The check is
    /// only put off — the first question or sync after the hold probes, and
    /// an index built by another model is then unloaded
    /// ([`AskError::IndexModelChanged`]). Any other gateway failure is an
    /// error here.
    pub async fn load_existing(&self) -> Result<bool, GatewayError> {
        let key = format!("{CORPUS_LIBRARY}@{CORPUS_VERSION}");
        let Ok(Some(corpus)) = store::get_corpus_by_id(self.index.pool(), &key).await else {
            return Ok(false);
        };
        let alias = &self.config.embed_model;
        if &corpus.embed_model != alias {
            return Ok(false);
        }
        match self.gateway.connect_embedder(alias).await {
            Ok(embedder) => {
                // The alias may name another model than the index was built
                // with (same width, other vectors): then the next sync
                // rebuilds it.
                if matches!(self.probe_matches(&embedder).await, Ok(false)) {
                    return Ok(false);
                }
                Ok(self.install_retriever(embedder, true).await.is_ok())
            }
            Err(e) if e.is_gpu_hold() => {
                let embedder = GatewayEmbedder::unprobed(&self.gateway, alias, corpus.dims());
                let loaded = self.install_retriever(Arc::new(embedder), false).await;
                if loaded.is_ok() {
                    tracing::info!(
                        "{}: the index on disk is loaded unchecked ({UNVERIFIED})",
                        e.hold_reason()
                    );
                }
                Ok(loaded.is_ok())
            }
            Err(e) => Err(e),
        }
    }

    /// Whether `embedder`'s probe is the model the index was built with.
    async fn probe_matches(&self, embedder: &GatewayEmbedder) -> Result<bool, IndexError> {
        match embedder.probe() {
            Some(p) => sync::probe_matches(&self.index, p).await,
            None => Ok(false),
        }
    }

    /// The retriever a question runs on. One loaded under a hold is checked
    /// first: the model its alias names now is probed and compared with the
    /// index's fingerprint. Still held — the question goes ahead, and its own
    /// embedding reports the hold. The same model — it is marked checked and
    /// answers. Another model — it is unloaded, and the question is
    /// [`AskError::IndexModelChanged`] (the next sync rebuilds the index).
    async fn retriever_for_question(&self) -> Result<Arc<Retriever>, AskError> {
        let loaded = self
            .retriever
            .read()
            .await
            .clone()
            .ok_or(AskError::NoIndex)?;
        if loaded.probed {
            return Ok(loaded.retriever);
        }
        let alias = &self.config.embed_model;
        let embedder = match self.gateway.connect_embedder(alias).await {
            Ok(e) => e,
            Err(e) if e.is_gpu_hold() => return Ok(loaded.retriever),
            Err(e) => return Err(AskError::Gateway(e)),
        };
        let same = self
            .probe_matches(&embedder)
            .await
            .map_err(|e| AskError::Retrieval(e.to_string()))?;
        // Only the slot's own retriever changes state: a sync may have
        // swapped in a checked one meanwhile.
        let mut slot = self.retriever.write().await;
        let current = slot
            .as_ref()
            .is_some_and(|l| Arc::ptr_eq(&l.retriever, &loaded.retriever));
        if same {
            if current {
                if let Some(l) = slot.as_mut() {
                    l.probed = true;
                }
            }
            return Ok(loaded.retriever);
        }
        if current {
            *slot = None;
        }
        Err(AskError::IndexModelChanged {
            alias: alias.clone(),
            detail: format!(
                "the probe text no longer embeds within PROBE_SAME_MODEL_MIN_COSINE ({}) of \
                 what it did",
                sync::PROBE_SAME_MODEL_MIN_COSINE
            ),
        })
    }

    /// Run one sync against the gateway, streaming progress into `events`,
    /// then swap in a retriever over the result.
    ///
    /// Everything that can stop a sync before it starts — the embedding
    /// model's context is smaller than `chunk_tokens`, the GPU is held, the
    /// gateway is unreachable — is reported the same way as a stop midway: an
    /// [`SyncEvent::Aborted`] on the channel and [`SyncError::Aborted`] back.
    pub async fn sync(&self, events: Events) -> Result<SyncReport, SyncError> {
        self.sync_with(events, false).await
    }

    /// [`FolderChat::sync`], and with `retry_failed` the owner's Retry failed
    /// pages first: every cached page failure is forgotten under the sync's
    /// own permit (`SyncOptions::retry_failed`), so those pages are read
    /// again.
    pub async fn sync_with(
        &self,
        events: Events,
        retry_failed: bool,
    ) -> Result<SyncReport, SyncError> {
        if self.index.is_syncing() {
            return Err(SyncError::AlreadyRunning);
        }
        let abort = |kind: AbortKind, reason: String| {
            let _ = events.send(SyncEvent::Aborted {
                kind,
                reason: reason.clone(),
            });
            SyncError::Aborted {
                kind,
                reason,
                partial: Box::default(),
            }
        };
        let alias = self.config.embed_model.clone();
        let info = match self.gateway.model_info(&alias).await {
            Ok(i) => i,
            Err(e) => {
                return Err(abort(
                    kind_of(&e),
                    format!("the embedding model {alias} could not be looked up: {e}"),
                ))
            }
        };
        let embedder = match self.gateway.connect_embedder(&alias).await {
            Ok(e) => e,
            Err(e) if e.is_gpu_hold() => {
                return Err(abort(
                    AbortKind::GpuHold,
                    format!("{}; nothing was synced", e.hold_reason()),
                ))
            }
            Err(e) => {
                return Err(abort(
                    AbortKind::Embedder,
                    format!("the embedding model {alias} could not be reached: {e}"),
                ))
            }
        };
        let opts = SyncOptions {
            probe: embedder.probe().map(<[f32]>::to_vec),
            vision: self.config.vision_model.clone().map(|model| VisionOptions {
                model,
                every_page: self.config.vision_every_page,
                gateway: self.gateway.clone(),
            }),
            retry_failed,
            ..SyncOptions::new(self.config.chunk_tokens, info.context_length)
        };
        let mut outcome = sync::run(&self.index, embedder.clone(), &opts, &events).await;
        // Nothing to load and nothing to record: another sync is running, or
        // this one stopped because the index directory is no longer the
        // agent's alone — that stop touches the index not at all, not even
        // for reading (SQLite keeps read marks in its `-shm` file).
        let untouched = matches!(
            outcome,
            Err(SyncError::AlreadyRunning)
                | Err(SyncError::Aborted {
                    kind: AbortKind::IndexContainment,
                    ..
                })
        );
        // Swap in a retriever over whatever is now committed — after a
        // partial sync too: every file marked current is whole, and a stale
        // retriever from before an index reset could not open at all. If it
        // cannot be built, the previous one stays and the report says why.
        if !untouched {
            if let Err(e) = self.install_retriever(embedder, true).await {
                let note = format!(
                    "the index was updated but could not be loaded for answering: {e}; \
                     questions are answered from the previous index"
                );
                match &mut outcome {
                    Ok(r) => r.notes.push(note),
                    Err(SyncError::Aborted { partial, .. }) => partial.notes.push(note),
                    Err(SyncError::AlreadyRunning) => {}
                }
            }
        }
        let report = match &outcome {
            Ok(r) => Some(r.clone()),
            Err(SyncError::Aborted { partial, .. }) => Some((**partial).clone()),
            Err(SyncError::AlreadyRunning) => None,
        };
        if let Some(r) = report {
            *self.last_sync.write().await = Some(r);
        }
        if !untouched {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            *self.last_sync_finished_ms.write().await = Some(now);
            // Best effort: a clock line the next start shows, never a reason
            // for the sync itself to fail.
            let _ = self
                .index
                .set_meta(META_LAST_SYNC_MS, &now.to_string())
                .await;
        }
        outcome
    }

    /// Load a retriever over the index as it is now and swap it in; `probed`
    /// says whether `embedder` was checked against the index's fingerprint.
    async fn install_retriever(
        &self,
        embedder: Arc<dyn Embedder>,
        probed: bool,
    ) -> Result<(), quickdoc_core::QuickdocError> {
        let key = format!("{CORPUS_LIBRARY}@{CORPUS_VERSION}");
        let mut r = Retriever::load_by_id(self.index.pool().clone(), &key, embedder)
            .await?
            .with_token_counter(self.tokens.clone());
        r = match &self.config.rerank_model {
            Some(m) => r.with_reranker(Arc::new(self.gateway.reranker(m))),
            None => r.with_rerank_unavailable("rerank_model is empty, so reranking is off"),
        };
        *self.retriever.write().await = Some(Loaded {
            retriever: Arc::new(r),
            probed,
        });
        Ok(())
    }

    /// The search parameters a question runs with unless the caller passes
    /// its own to [`FolderChat::ask_with`].
    pub fn search_params(&self) -> SearchParams {
        chat::default_search_params(self.config.rerank_model.is_some())
    }

    /// Answer one question: budget, retrieve, and start streaming.
    pub async fn ask(&self, req: &ChatRequest) -> Result<Answer, AskError> {
        self.ask_with(req, self.search_params()).await
    }

    pub async fn ask_with(
        &self,
        req: &ChatRequest,
        params: SearchParams,
    ) -> Result<Answer, AskError> {
        // Clone the Arc and let the lock go: a sync finishing mid-answer swaps
        // the slot without waiting for this question.
        let retriever = self.retriever_for_question().await?;
        let info = self.gateway.model_info(&self.config.chat_model).await?;
        let prepared = chat::prepare(
            &retriever,
            &info,
            req,
            self.tokens.as_ref(),
            params,
            self.config.chunk_tokens,
        )
        .await
        .map_err(hold_is_hold)?;
        let mut meta = prepared.meta;
        // Lines as the sync stored them: no file is read per question.
        self.index
            .fill_stored_lines(&mut meta.citations)
            .await
            .map_err(|e| AskError::Retrieval(e.to_string()))?;
        let stream = self
            .gateway
            .chat_stream(&self.config.chat_model, &prepared.messages)
            .await?;
        meta.served_by_fallback = stream.fallback;
        Ok(Answer {
            meta,
            chunks: stream.chunks,
        })
    }

    /// One file of the folder as the index sees it — the MCP `read` tool.
    /// A PDF's text is `pdftotext`'s output followed by exactly the page
    /// readings its indexed text was built with ([`IndexDir::pdf_readings`],
    /// appended as the sync appends them, [`vision::append_readings`]), so the
    /// lines a citation of a reading names are the lines here — also while a
    /// pass over it that stopped part-way is pending. A PDF changed since its
    /// last sync gets no readings: they were of other bytes.
    pub async fn read_file(&self, rel: &str) -> Result<SourceText, ReadError> {
        let mut text = crate::source::read_text(self.index.folder(), rel).await?;
        if let (FileKind::Pdf, Some(hash)) = (text.kind, text.content_hash.as_deref()) {
            let readings = self
                .index
                .pdf_readings(&text.path, hash)
                .await
                .map_err(|e| ReadError::Io {
                    path: rel.to_string(),
                    message: format!("its page readings could not be read from the index: {e}"),
                })?;
            vision::append_readings(&mut text.text, &readings);
        }
        Ok(text)
    }

    /// The retrieval half of a question, without a model: the `k` best
    /// excerpts for `query`, with their stored line numbers — the MCP
    /// `search` tool.
    ///
    /// No token budget applies (`budget_tokens: None`): the caller asked for
    /// `k`, and gets `k` whenever the folder has that many chunks. The
    /// candidate pools of both stages (`k_fts`, `k_vec`) are raised to `k`
    /// when `k` is larger than quickdoc's defaults, so a large `k` is not cut
    /// short by a depth nobody asked for. Reranking runs as it does for the
    /// chat (on when a rerank model is configured), over quickdoc's
    /// `k_rerank` window; the rest keeps its fused order.
    pub async fn search(&self, query: &str, k: usize) -> Result<SearchResults, AskError> {
        let retriever = self.retriever_for_question().await?;
        let query = query.trim();
        if query.is_empty() {
            return Err(AskError::EmptyQuestion);
        }
        let d = SearchParams::default();
        let params = SearchParams {
            limit: k,
            k_fts: d.k_fts.max(k),
            k_vec: d.k_vec.max(k),
            rerank: self.config.rerank_model.is_some(),
            budget_tokens: None,
            ..d
        };
        let searched = chat::search(&retriever, query, &params)
            .await
            .map_err(hold_is_hold)?;
        let rerank_skipped = searched.rerank_skipped();
        let Searched {
            result,
            rerank_dropped,
        } = searched;
        let mut notes = vec![chat::retrieval_note(
            &params,
            result.trace.rerank_model.as_deref(),
        )];
        notes.extend(rerank_dropped.map(|d| d.note));
        let urls = store::document_urls(
            retriever.pool(),
            &result
                .hits
                .iter()
                .map(|h| h.chunk.id.clone())
                .collect::<Vec<_>>(),
        )
        .await
        .map_err(|e| AskError::Retrieval(e.to_string()))?;
        let mut hits: Vec<Citation> = result
            .hits
            .iter()
            .enumerate()
            .map(|(i, h)| Citation {
                n: i + 1,
                path: urls
                    .get(&h.chunk.id)
                    .cloned()
                    .unwrap_or_else(|| h.chunk.derived_title.clone()),
                heading_path: h.chunk.heading_path.clone(),
                page: chat::page_of(&h.chunk.heading_path),
                span_start: h.chunk.span_start,
                span_end: h.chunk.span_end,
                start_line: None,
                end_line: None,
                chunk_id: h.chunk.id.clone(),
                score: h.score,
                text: h.chunk.payload.clone(),
            })
            .collect();
        self.index
            .fill_stored_lines(&mut hits)
            .await
            .map_err(|e| AskError::Retrieval(e.to_string()))?;
        Ok(SearchResults {
            hits,
            rerank_model: result.trace.rerank_model.clone(),
            rerank_skipped,
            retrieval: Depths::from(&params),
            notes,
        })
    }
}

/// A question embedding (or rerank) refused by a hold is the hold, not a
/// retrieval bug.
fn hold_is_hold(e: AskError) -> AskError {
    match e {
        AskError::Retrieval(msg)
            if msg.contains("gpu_hold")
                || msg.contains(GPU_HOLD_MESSAGE)
                || msg.contains(EMBED_FALLBACK_PREFIX) =>
        {
            AskError::Gateway(GatewayError::GpuHold {
                message: "the question could not be embedded or reranked".into(),
            })
        }
        other => other,
    }
}

fn kind_of(e: &GatewayError) -> AbortKind {
    if e.is_gpu_hold() {
        AbortKind::GpuHold
    } else {
        AbortKind::Embedder
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_send<T: Send>(_: &T) {}

    /// Phase 2 holds one `Arc<FolderChat>` in its server state and spawns the
    /// sync onto its own task, so the handle and both entry futures must be
    /// `Send + Sync`. A compile-time check; nothing runs.
    #[allow(dead_code, unreachable_code, clippy::diverging_sub_expression)]
    fn the_agent_can_live_in_server_state(app: &FolderChat) {
        fn is_send_sync<T: Send + Sync>() {}
        is_send_sync::<FolderChat>();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        is_send(&app.sync(tx));
        is_send(&app.ask(&ChatRequest::default()));
    }
}
