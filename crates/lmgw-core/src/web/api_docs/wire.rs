//! The corpus store's rows as the wire types of `lmgw-api-types` (`docs`):
//! what `/api/docs/*` answers is built here, field by field, with every source
//! struct destructured without `..` so a field added on either side fails the
//! build. A chunk is `knowledge::wire::chunk`.

use lmgw_api_types as dto;
use quickdoc_core::store::{
    Corpus, DocRequest, Document, EvalRun, GoldenCandidate, GoldenQuery, Source,
};

use crate::quickdoc::query::CorpusStatus;

/// One corpus as the Docs tab lists it: the row, its badges and its sources.
/// `unembedded` is the number of its chunks with no vector yet.
pub(crate) fn corpus(
    c: &Corpus,
    unembedded: i64,
    status: CorpusStatus,
    sources: Vec<Source>,
) -> dto::CorpusView {
    let Corpus {
        id,
        library,
        version,
        status: corpus_status,
        embed_upstream,
        embed_model,
        embed_dims,
        ingest_model,
        ingest_prompt_version,
        crawl_date,
        source_kind,
        eval_score,
        eval_best,
        eval_regression,
        eval_k,
        eval_at,
        chunk_count,
        created_at,
        updated_at,
    } = c;
    let CorpusStatus {
        embed_status,
        embed_model_resolvable: _,
        eval_status,
        flags,
        warnings,
    } = status;
    dto::CorpusView {
        id: *id,
        corpus_id: c.corpus_id(),
        library: library.clone(),
        version: version.clone(),
        status: corpus_status.clone(),
        chunk_count: *chunk_count,
        unembedded_chunks: unembedded,
        // What this corpus costs to serve once loaded (§5): dims × 2 bytes per
        // embedded chunk. The visible price of the exact-KNN scan.
        resident_bytes: (chunk_count - unembedded).max(0) * embed_dims * 2,
        embed_upstream: embed_upstream.clone(),
        embed_model: embed_model.clone(),
        embed_dims: *embed_dims,
        embed_identity: c.embed_identity().to_string(),
        ingest_model: ingest_model.clone(),
        ingest_prompt_version: ingest_prompt_version.clone(),
        crawl_date: crawl_date.clone(),
        source_kind: source_kind.clone(),
        eval_score: *eval_score,
        eval_best: *eval_best,
        // Never evaluated: the store holds 0 and "", which are not a depth and
        // not an instant.
        eval_k: (*eval_k > 0).then_some(*eval_k),
        eval_at: (!eval_at.is_empty()).then(|| eval_at.clone()),
        eval_regression: *eval_regression,
        embed_status: embed_status.into(),
        eval_status: eval_status.into(),
        flags,
        warnings,
        sources: sources.into_iter().map(source).collect(),
        created_at: created_at.clone(),
        updated_at: updated_at.clone(),
    }
}

pub(crate) fn source(s: Source) -> dto::CorpusSource {
    let Source {
        id,
        corpus_id,
        root,
        kind,
        fence,
        created_at,
    } = s;
    dto::CorpusSource {
        id,
        corpus_id,
        root,
        kind,
        fence,
        created_at,
    }
}

pub(crate) fn document(d: Document) -> dto::DocumentRow {
    let Document {
        id,
        source_id,
        url,
        content_hash,
        fetched_at,
    } = d;
    dto::DocumentRow {
        id,
        source_id,
        url,
        content_hash,
        fetched_at,
    }
}

pub(crate) fn golden_query(g: GoldenQuery) -> dto::GoldenQueryRow {
    let GoldenQuery {
        id,
        corpus_id,
        query,
        expected_chunk_ids,
        origin,
    } = g;
    dto::GoldenQueryRow {
        id,
        corpus_id,
        query,
        expected_chunk_ids,
        origin,
    }
}

pub(crate) fn golden_candidate(c: GoldenCandidate) -> dto::GoldenCandidateRow {
    let GoldenCandidate {
        id,
        corpus_id,
        query,
        expected_chunk_ids,
        rationale,
        model,
        status,
        golden_query_id,
        created_at,
        decided_at,
    } = c;
    dto::GoldenCandidateRow {
        id,
        corpus_id,
        query,
        expected_chunk_ids,
        rationale,
        model,
        status,
        golden_query_id,
        created_at,
        decided_at,
    }
}

pub(crate) fn eval_run(r: EvalRun) -> dto::EvalRunRow {
    let EvalRun {
        id,
        corpus_id,
        k,
        queries,
        hit_at_k,
        mrr,
        orphaned_queries,
        regression,
        params,
        report,
        created_at,
    } = r;
    dto::EvalRunRow {
        id,
        corpus_id,
        k,
        queries,
        hit_at_k,
        mrr,
        orphaned_queries,
        regression,
        params,
        report,
        created_at,
    }
}

pub(crate) fn doc_request(r: DocRequest) -> dto::DocRequestRow {
    let DocRequest {
        id,
        library,
        version,
        reason,
        client_name,
        count,
        first_requested_at,
        last_requested_at,
        status,
    } = r;
    dto::DocRequestRow {
        id,
        library,
        version,
        reason,
        client_name,
        count,
        first_requested_at,
        last_requested_at,
        status,
    }
}
