//! The core's knowledge views as the wire types of `lmgw-api-types`
//! (`knowledge`): what `/api/knowledge/*` answers is built here, field by
//! field, with every source struct destructured without `..` so a field added
//! on either side fails the build.

use lmgw_api_types::knowledge as api;
use lmgw_api_types::{
    FtsWeightsView, FusedHitView, SearchParamsView, SearchTraceView, StageHitView, TimingsView,
};
use quickdoc_core::retrieve::{
    FtsWeights, FusedHit, Hit, SearchParams, SearchTrace, StageHit, Timings,
};
use quickdoc_core::store::Chunk;

use super::ops::{EditOutcome, KbView, UploadItem, UploadOutcome};
use super::read::{ChunkSpan, Source};
use super::retrieve::{Excerpt, Retrieval};
use super::store::{Kb, KbCounts, KbFile};
use crate::jobs::JobView;

pub fn job(j: JobView) -> lmgw_api_types::JobRow {
    let JobView {
        id,
        kind,
        key,
        label,
        status,
        done,
        total,
        percent,
        stage,
        detail,
        error,
        created_at,
        started_at,
        finished_at,
    } = j;
    lmgw_api_types::JobRow {
        id,
        kind,
        key,
        label,
        status,
        done,
        total,
        percent,
        stage,
        detail,
        error,
        created_at,
        started_at,
        finished_at,
    }
}

pub fn counts(c: KbCounts) -> api::KnowledgeCounts {
    let KbCounts {
        files,
        ready,
        pending,
        ingesting,
        failed,
        chunks,
        embedded,
        bytes,
    } = c;
    api::KnowledgeCounts {
        files,
        ready,
        pending,
        ingesting,
        failed,
        chunks,
        embedded,
        bytes,
    }
}

pub fn file(f: KbFile) -> api::KnowledgeFile {
    let KbFile {
        id,
        kb_id,
        name,
        kind,
        sub,
        mime,
        size,
        sha256,
        status,
        error,
        pages,
        skipped_pages,
        notes,
        chunk_count,
        added_at,
        ingested_at,
    } = f;
    api::KnowledgeFile {
        id,
        kb_id,
        name,
        kind,
        sub,
        mime,
        size,
        sha256,
        status,
        error,
        pages,
        skipped_pages,
        notes,
        chunk_count,
        added_at,
        ingested_at,
    }
}

pub fn base(v: KbView) -> api::KnowledgeBase {
    let KbView {
        kb,
        embed_identity,
        counts: c,
        resident_bytes,
        embed_context,
        embed_input_limit,
        embed_input_limit_source,
        embed_tokenizer,
        embed_resolvable,
        job: j,
        notes,
    } = v;
    let Kb {
        id,
        name,
        description,
        embed_alias,
        embed_upstream,
        embed_model,
        embed_dims,
        rerank_alias,
        vision_alias,
        chunk_tokens,
        chunk_overlap,
        mcp_visible,
        status,
        vectors_rev,
        created_at,
        updated_at,
    } = kb;
    api::KnowledgeBase {
        id,
        name,
        description,
        embed_alias,
        embed_upstream,
        embed_model,
        embed_dims,
        rerank_alias,
        vision_alias,
        chunk_tokens,
        chunk_overlap,
        mcp_visible,
        status,
        vectors_rev,
        created_at,
        updated_at,
        embed_identity,
        counts: counts(c),
        resident_bytes,
        embed_context,
        embed_input_limit,
        embed_input_limit_source,
        embed_tokenizer: embed_tokenizer.to_string(),
        embed_resolvable,
        job: j.map(job),
        notes,
    }
}

pub fn edited(o: EditOutcome) -> api::KnowledgeBaseEdited {
    let EditOutcome {
        kb,
        reembed_job,
        ingest_job,
        rechunk_files,
    } = o;
    api::KnowledgeBaseEdited {
        kb: base(kb),
        reembed_job,
        ingest_job,
        rechunk_files,
    }
}

pub fn uploaded(o: UploadOutcome) -> api::UploadResult {
    let UploadOutcome { items, job } = o;
    api::UploadResult {
        items: items
            .into_iter()
            .map(|i| {
                let UploadItem {
                    name,
                    outcome,
                    file: f,
                    reason,
                } = i;
                api::UploadItem {
                    name,
                    outcome,
                    file: f.map(file),
                    reason,
                }
            })
            .collect(),
        job,
    }
}

pub fn span(c: ChunkSpan) -> api::ChunkSpan {
    let ChunkSpan {
        id,
        seq,
        page,
        heading_path,
        span_start,
        span_end,
        tokens,
    } = c;
    api::ChunkSpan {
        id,
        seq,
        page,
        heading_path,
        span_start,
        span_end,
        tokens,
    }
}

pub fn source(s: Source) -> api::KnowledgeSource {
    let Source {
        file: f,
        kb_name,
        text,
        chunks,
        highlight,
        notice,
    } = s;
    api::KnowledgeSource {
        file: file(f),
        kb_name,
        text,
        chunks: chunks.into_iter().map(span).collect(),
        highlight: highlight.map(span),
        notice,
    }
}

fn excerpt(e: Excerpt) -> api::KnowledgeExcerpt {
    let Excerpt {
        kb_id,
        kb,
        file_id,
        file,
        page,
        chunk_id,
        heading_path,
        text,
        score,
        rerank_skipped,
        tokens,
        span_start,
        span_end,
        file_sha,
    } = e;
    api::KnowledgeExcerpt {
        kb_id,
        kb,
        file_id,
        file,
        page,
        chunk_id,
        heading_path,
        text,
        score,
        rerank_skipped,
        tokens,
        span_start,
        span_end,
        file_sha,
    }
}

/// One corpus chunk as the wire type, field by field.
pub fn chunk(c: &Chunk) -> lmgw_api_types::ChunkRow {
    let Chunk {
        id,
        document_id,
        corpus_id,
        heading_path,
        span_start,
        span_end,
        payload,
        derived_title,
        derived_summary,
    } = c;
    lmgw_api_types::ChunkRow {
        id: id.clone(),
        document_id: *document_id,
        corpus_id: *corpus_id,
        heading_path: heading_path.clone(),
        span_start: *span_start,
        span_end: *span_end,
        payload: payload.clone(),
        derived_title: derived_title.clone(),
        derived_summary: derived_summary.clone(),
    }
}

/// One search hit of the docs playground as the wire type, field by field
/// (`rerank_skipped` is written only when true, as the raw hit wrote it).
pub fn hit(h: &Hit) -> lmgw_api_types::SearchHit {
    let Hit {
        chunk: c,
        score,
        rrf_score,
        fts_rank,
        knn_rank,
        knn_score,
        rerank_score,
        rerank_skipped,
        tokens,
    } = h;
    lmgw_api_types::SearchHit {
        chunk: chunk(c),
        score: *score,
        rrf_score: *rrf_score,
        fts_rank: fts_rank.map(|r| r as u64),
        knn_rank: knn_rank.map(|r| r as u64),
        knn_score: *knn_score,
        rerank_score: *rerank_score,
        rerank_skipped: *rerank_skipped,
        tokens: *tokens as u64,
    }
}

/// `quickdoc-core`'s trace as the wire type, field by field: the knowledge
/// search and the docs playground both answer through this one conversion.
/// A NaN score stays a NaN and is written as `null`, as the raw trace was.
pub fn trace(t: &SearchTrace) -> SearchTraceView {
    let SearchTrace {
        corpus_id,
        embed_model,
        params,
        fts_query,
        fts,
        knn_skipped,
        knn,
        fused,
        rerank_model,
        rerank_skipped,
        rerank,
        token_counter,
        budget_used_tokens,
        budget_dropped,
        resident_vectors,
        resident_bytes,
        knn_kernel,
        timings,
    } = t;
    let SearchParams {
        k_fts,
        k_vec,
        rrf_k,
        fts_weights,
        rerank: params_rerank,
        k_rerank,
        limit,
        budget_tokens,
    } = params;
    let FtsWeights {
        payload,
        heading_path,
        derived_title,
        derived_summary,
    } = fts_weights;
    let Timings {
        embed_ms,
        fts_ms,
        knn_ms,
        fuse_ms,
        fetch_ms,
        rerank_ms,
        total_ms,
    } = timings;
    SearchTraceView {
        corpus_id: corpus_id.clone(),
        embed_model: embed_model.clone(),
        params: SearchParamsView {
            k_fts: *k_fts as u64,
            k_vec: *k_vec as u64,
            rrf_k: *rrf_k,
            fts_weights: FtsWeightsView {
                payload: *payload,
                heading_path: *heading_path,
                derived_title: *derived_title,
                derived_summary: *derived_summary,
            },
            rerank: *params_rerank,
            k_rerank: *k_rerank as u64,
            limit: *limit as u64,
            budget_tokens: budget_tokens.map(|b| b as u64),
        },
        fts_query: fts_query.clone(),
        fts: fts.iter().map(stage_hit).collect(),
        knn_skipped: knn_skipped.clone(),
        knn: knn.iter().map(stage_hit).collect(),
        fused: fused
            .iter()
            .map(|h| {
                let FusedHit {
                    chunk_id,
                    rrf_score,
                    fts_rank,
                    knn_rank,
                } = h;
                FusedHitView {
                    chunk_id: chunk_id.clone(),
                    rrf_score: *rrf_score,
                    fts_rank: fts_rank.map(|r| r as u64),
                    knn_rank: knn_rank.map(|r| r as u64),
                }
            })
            .collect(),
        rerank_model: rerank_model.clone(),
        rerank_skipped: rerank_skipped.clone(),
        rerank: rerank.iter().map(stage_hit).collect(),
        token_counter: token_counter.clone(),
        budget_used_tokens: *budget_used_tokens as u64,
        budget_dropped: budget_dropped.clone(),
        resident_vectors: *resident_vectors as u64,
        resident_bytes: *resident_bytes as u64,
        knn_kernel: (*knn_kernel).to_string(),
        timings: TimingsView {
            embed_ms: *embed_ms,
            fts_ms: *fts_ms,
            knn_ms: *knn_ms,
            fuse_ms: *fuse_ms,
            fetch_ms: *fetch_ms,
            rerank_ms: *rerank_ms,
            total_ms: *total_ms,
        },
    }
}

fn stage_hit(h: &StageHit) -> StageHitView {
    let StageHit {
        chunk_id,
        rank,
        score,
        skipped,
    } = h;
    StageHitView {
        chunk_id: chunk_id.clone(),
        rank: *rank as u64,
        score: *score,
        skipped: *skipped,
    }
}

pub fn search(r: Retrieval) -> api::KnowledgeSearchResult {
    let Retrieval {
        excerpts,
        tokens,
        dropped,
        notes,
        ms,
        searched,
        traces,
    } = r;
    api::KnowledgeSearchResult {
        excerpts: excerpts.into_iter().map(excerpt).collect(),
        tokens,
        dropped,
        notes,
        ms,
        searched,
        traces: traces.iter().map(trace).collect(),
    }
}
