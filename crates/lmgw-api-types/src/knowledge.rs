//! The Knowledge bases API (`/api/knowledge/*`): bases, their files, uploads,
//! the long-running ingest and re-embed that follow, the source viewer and the
//! search playground.
//!
//! The gateway parses and builds these types itself and the API document is
//! generated from them. A base's long-running work is an ordinary background
//! job ([`JobRow`], kinds `kb_ingest` and `kb_reembed`, key `kb:<base id>`):
//! the routes here answer with the job's id, and `GET /api/jobs` and the `jobs`
//! frames of `GET /api/events` carry its progress.

use serde::{Deserialize, Serialize};

use crate::JobRow;

/// A knowledge base as every route shows it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KnowledgeBase {
    pub id: i64,
    /// Unique across the bases.
    pub name: String,
    pub description: String,
    /// The embedding model alias the base was created with.
    pub embed_alias: String,
    /// The upstream the alias resolved to when the base was pinned.
    pub embed_upstream: String,
    /// The embedding model the base is pinned to.
    pub embed_model: String,
    /// The vector length of the pinned model.
    pub embed_dims: i64,
    /// Empty: no rerank stage.
    pub rerank_alias: String,
    /// Empty: text-less PDF pages are skipped and counted.
    pub vision_alias: String,
    pub chunk_tokens: i64,
    pub chunk_overlap: i64,
    /// Whether the `kb__*` tools on `/mcp` may search this base.
    pub mcp_visible: bool,
    /// `ready` or `re_embed_required` (the vectors no longer match the pinned
    /// model; Resume fills them in).
    #[cfg_attr(feature = "schema", schemars(extend("enum" = ["ready", "re_embed_required"])))]
    pub status: String,
    /// Moves on every chunk write.
    pub vectors_rev: i64,
    pub created_at: String,
    pub updated_at: String,
    /// `upstream/model (Nd)`: the pin as one line.
    pub embed_identity: String,
    pub counts: KnowledgeCounts,
    /// What the base's vectors cost resident in memory: embedded chunks x
    /// dimensions x 2.
    pub resident_bytes: i64,
    /// The embedding model's context in tokens, when the gateway knows it.
    pub embed_context: Option<u64>,
    /// What one embedding input may hold, `min(context, ubatch)` for a local
    /// model; `chunk_tokens` (heading path included) is checked against it.
    pub embed_input_limit: Option<u64>,
    /// One sentence: where `embed_input_limit` comes from.
    pub embed_input_limit_source: String,
    /// What counts the tokens: `model`, `tiktoken` or `tiktoken_guess`.
    pub embed_tokenizer: String,
    /// Whether a model on this gateway still resolves to the pin. When not,
    /// searches fall back to keywords and new files cannot be embedded.
    pub embed_resolvable: bool,
    /// The live job on the base, or the last one that ran.
    pub job: Option<JobRow>,
    /// One sentence each: what a person should know about the base now.
    pub notes: Vec<String>,
}

/// A base's file and chunk counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KnowledgeCounts {
    pub files: i64,
    pub ready: i64,
    pub pending: i64,
    pub ingesting: i64,
    pub failed: i64,
    pub chunks: i64,
    /// Chunks that have a vector; `chunks - embedded` are waiting for one.
    pub embedded: i64,
    /// The files' size in bytes.
    pub bytes: i64,
}

/// One file of a base.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KnowledgeFile {
    pub id: i64,
    pub kb_id: i64,
    /// The name it was uploaded under (a label, never a path).
    pub name: String,
    /// What the bytes were sniffed as.
    pub kind: String,
    pub sub: String,
    pub mime: String,
    pub size: i64,
    pub sha256: String,
    /// `pending`, `ingesting`, `ready` or `failed`.
    #[cfg_attr(
        feature = "schema",
        schemars(extend("enum" = ["pending", "ingesting", "ready", "failed"]))
    )]
    pub status: String,
    /// Why the last ingest failed.
    pub error: Option<String>,
    /// Pages, slides or sheets, when the file has them.
    pub pages: Option<i64>,
    /// Text-less PDF pages that were skipped (no vision model on the base).
    pub skipped_pages: i64,
    pub notes: Vec<String>,
    pub chunk_count: i64,
    pub added_at: String,
    pub ingested_at: Option<String>,
}

/// `GET /api/knowledge/bases`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KnowledgeBaseList {
    pub bases: Vec<KnowledgeBase>,
}

/// `GET /api/knowledge/bases/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KnowledgeBaseDetail {
    pub base: KnowledgeBase,
    pub files: Vec<KnowledgeFile>,
}

/// `GET /api/knowledge/bases/{id}/files`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KnowledgeFileList {
    pub files: Vec<KnowledgeFile>,
}

/// `POST /api/knowledge/bases`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CreateKnowledgeBase {
    /// Unique across the bases.
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// The embedding model alias. The base is pinned to the model it
    /// resolves to; the alias is probed once to learn the vector length.
    pub embed_alias: String,
    /// A rerank model alias; none: no rerank stage.
    #[serde(default)]
    pub rerank_alias: Option<String>,
    /// A vision model alias that reads text-less PDF pages; none: they are
    /// skipped and counted.
    #[serde(default)]
    pub vision_alias: Option<String>,
    /// Chunk size in tokens; 512 when absent.
    #[serde(default)]
    pub chunk_tokens: Option<i64>,
    /// Overlap between chunks in tokens; 64 when absent.
    #[serde(default)]
    pub chunk_overlap: Option<i64>,
    /// Whether the `kb__*` tools on `/mcp` may search the base; true when
    /// absent.
    #[serde(default)]
    pub mcp_visible: Option<bool>,
}

/// `POST /api/knowledge/bases/{id}/settings`: every field optional; an empty
/// `rerank_alias` or `vision_alias` clears it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EditKnowledgeBase {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// A different model re-pins the base and re-embeds every chunk (a
    /// background job).
    #[serde(default)]
    pub embed_alias: Option<String>,
    #[serde(default)]
    pub rerank_alias: Option<String>,
    #[serde(default)]
    pub vision_alias: Option<String>,
    /// A change re-chunks every file (a background job).
    #[serde(default)]
    pub chunk_tokens: Option<i64>,
    #[serde(default)]
    pub chunk_overlap: Option<i64>,
    #[serde(default)]
    pub mcp_visible: Option<bool>,
}

/// The answer to a settings change: the base, and the work the change
/// started.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KnowledgeBaseEdited {
    pub kb: KnowledgeBase,
    /// The re-embed a model change started.
    pub reembed_job: Option<i64>,
    /// The re-ingest a chunk-size change started.
    pub ingest_job: Option<i64>,
    /// Files a chunk-size change sent back to `pending`. A model change
    /// decides inside its job which files to re-chunk; the job's stage and
    /// detail say which.
    pub rechunk_files: u64,
}

/// What became of one uploaded file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum UploadVerdict {
    /// Stored and queued for ingest.
    Added,
    /// A file of that name existed; its content was replaced and queued again.
    Replaced,
    /// The same name with the same bytes: nothing to do.
    Unchanged,
    /// The same bytes already in the base under another name.
    Duplicate,
    /// Not stored; `reason` says why (images and audio are refused, as is an
    /// empty or unreadable file).
    #[default]
    Refused,
}

impl UploadVerdict {
    /// The word on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Replaced => "replaced",
            Self::Unchanged => "unchanged",
            Self::Duplicate => "duplicate",
            Self::Refused => "refused",
        }
    }
}

/// One uploaded file's fate.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UploadItem {
    /// The name the part carried.
    pub name: String,
    pub outcome: UploadVerdict,
    /// The stored file; absent for a refused one.
    pub file: Option<KnowledgeFile>,
    /// Why it was not added, or what it replaced.
    pub reason: Option<String>,
}

/// `POST /api/knowledge/bases/{id}/files`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UploadResult {
    /// One per part, in order.
    pub items: Vec<UploadItem>,
    /// The ingest job the upload started or joined; `null` when nothing was
    /// queued.
    pub job: Option<i64>,
}

/// `POST /api/knowledge/bases/{id}/resume`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResumeResult {
    /// The job now running (or already running); `null` when the base has
    /// nothing waiting.
    pub job: Option<i64>,
    /// `kb_ingest` or `kb_reembed`; absent when no job was started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Present when Resume joined a running job (`true`) or started the
    /// plain ingest of the pending files (`false`); absent for a re-embed or
    /// re-chunk it started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub already_running: Option<bool>,
    /// Present when the files holding oversized chunks are re-chunked first:
    /// why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rechunk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `POST /api/knowledge/bases/{id}/cancel`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CancelResult {
    /// The job that was asked to stop.
    pub job: i64,
    pub message: String,
}

/// `POST /api/knowledge/bases/{id}/delete`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KnowledgeBaseDeleted {
    pub deleted: i64,
    pub name: String,
    /// Files the base held.
    pub files: i64,
    /// Chunks the base held.
    pub chunks: i64,
}

/// `POST /api/knowledge/files/{id}/delete`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KnowledgeFileDeleted {
    pub deleted: i64,
    pub name: String,
    /// Chunks the file held.
    pub chunks: i64,
}

/// `POST /api/knowledge/files/{id}/reingest`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KnowledgeFileRequeued {
    pub file: i64,
    /// The ingest job that will read it, started or joined.
    pub job: Option<i64>,
}

/// `GET /api/knowledge/files/{id}/text`'s query.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SourceQuery {
    /// The chunk to highlight (a citation's chunk id).
    #[serde(default)]
    pub chunk: Option<String>,
    /// What a citation stored besides the chunk id, for one whose chunk id a
    /// re-ingest renamed: the passage's start (byte offset into the text) ...
    #[serde(default)]
    pub span_start: Option<i64>,
    /// ... its end ...
    #[serde(default)]
    pub span_end: Option<i64>,
    /// ... and the file's sha256 when it was cited.
    #[serde(default)]
    pub sha: Option<String>,
}

/// Where a chunk sits in its file, without its text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChunkSpan {
    pub id: String,
    /// The chunk's position in the file.
    pub seq: i64,
    /// 1-based page, slide or sheet, when the file has them.
    pub page: Option<i64>,
    pub heading_path: String,
    /// Byte range in the file's extracted text.
    pub span_start: i64,
    pub span_end: i64,
    pub tokens: i64,
}

/// `GET /api/knowledge/files/{id}/text`: the source viewer's answer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KnowledgeSource {
    pub file: KnowledgeFile,
    pub kb_name: String,
    /// The extracted text; `null` until the file has been ingested once.
    pub text: Option<String>,
    /// Every chunk's span, in file order.
    pub chunks: Vec<ChunkSpan>,
    /// The cited chunk (or the one found by the citation's position).
    pub highlight: Option<ChunkSpan>,
    /// Said when the highlight is not the cited chunk itself: it was found by
    /// the citation's stored position because the chunk id no longer exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// `POST /api/knowledge/search`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KnowledgeSearchRequest {
    pub query: String,
    /// The bases to search; omitted or empty searches every base.
    #[serde(default)]
    pub kb_ids: Vec<i64>,
    /// Token budget for the excerpts; `0` or absent: none (the search's own
    /// `limit` decides).
    #[serde(default)]
    pub budget_tokens: Option<usize>,
    /// Stage overrides on top of the configured search defaults. Any subset of
    /// the keys below; an unknown key is a 400 op_failed naming the valid ones.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(schema_with = "search_params_schema"))]
    pub params: Option<serde_json::Value>,
}

#[cfg(feature = "schema")]
fn search_params_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": ["object", "null"],
        "description": "Stage overrides on top of the configured search defaults; \
            any subset of these keys.",
        "additionalProperties": false,
        "properties": {
            "k_fts": {"type": "integer", "description": "Candidates from keyword search."},
            "k_vec": {"type": "integer", "description": "Candidates from vector search."},
            "rrf_k": {"type": "number", "description": "Reciprocal-rank-fusion constant."},
            "fts_weights": {"type": "object", "description": "BM25 column weights: payload, heading_path, derived_title, derived_summary."},
            "rerank": {"type": "boolean", "description": "Whether the rerank stage runs."},
            "k_rerank": {"type": "integer", "description": "How many fused candidates the reranker scores."},
            "limit": {"type": "integer", "description": "Excerpts returned at most."},
            "budget_tokens": {"type": ["integer", "null"], "description": "Token budget."}
        }
    })
}

/// One retrieved excerpt.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KnowledgeExcerpt {
    pub kb_id: i64,
    /// The base's name.
    pub kb: String,
    pub file_id: i64,
    /// The file's name.
    pub file: String,
    /// 1-based PDF page; `null` for other files.
    pub page: Option<i64>,
    pub chunk_id: String,
    pub heading_path: String,
    /// The chunk's text, verbatim.
    pub text: String,
    /// The final order's score: the reranker's when it ran, the fused
    /// (reciprocal-rank) score otherwise.
    pub score: f32,
    /// Present (and true) when the rerank stage ran but did not score this
    /// excerpt (its query + chunk pair was over the reranker's input limit):
    /// `score` is its fused score and it ranks behind the reranked ones.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub rerank_skipped: bool,
    /// Counted over heading path and text.
    pub tokens: usize,
    /// Byte range in the file's extracted text.
    pub span_start: i64,
    pub span_end: i64,
    /// The sha256 of the file version the chunk was cut from.
    pub file_sha: String,
}

/// `POST /api/knowledge/search`: what a search found and what it could not do.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KnowledgeSearchResult {
    pub excerpts: Vec<KnowledgeExcerpt>,
    /// Tokens the excerpts use.
    pub tokens: usize,
    /// Excerpts that ranked but did not fit the budget.
    pub dropped: usize,
    /// Why something was not searched, or not fully; one sentence each.
    pub notes: Vec<String>,
    /// How long the search took.
    pub ms: f64,
    /// The bases that were searched, by name.
    pub searched: Vec<String>,
    /// One per search (per embedding and rerank model pair): what each stage
    /// saw.
    pub traces: Vec<crate::SearchTraceView>,
}
