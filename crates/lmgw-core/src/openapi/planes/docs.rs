//! `/api/docs/*`, the doc-corpora plane (api-docs design §4.6). WP4.
//!
//! Corpus CRUD, the debug search endpoint, golden queries and their
//! synthetic-candidate queue, eval runs, the doc-request queue and corpus
//! export/import — `web/api_docs.rs`'s own module doc. Ingest, re-embed, eval
//! and golden-generate all answer the same `DocsJobStarted` shape (the job now
//! running, or the one that already was); a handful of small mutations answer
//! only `{ok, ...}` with no DTO of their own, which this plane marks untyped
//! rather than inventing one lmgw does not have.

use lmgw_api_types as dto;

use super::super::registry::{Dialect, DocRoute, Req, Resp};
use crate::web::api_docs::{
    AcceptBody, CandidatesQuery, ChunksQuery, CorpusQuery, CreateCorpus, DetailQuery, EvalBody,
    ExportQuery, GenerateBody, GoldenBody, ImportQuery, ReEmbedBody, RequestStatusBody,
    RequestsQuery, SearchBody,
};

fn base(method: &'static str, path: &'static str, summary: &'static str) -> DocRoute {
    DocRoute {
        method,
        path,
        tag: "docs",
        summary,
        description: "",
        tool: None,
        query: None,
        path_ints: &[],
        request: Req::None,
        response: Resp::Untyped("BUG: docs.rs route builder did not override the response"),
        dialect: Dialect::Dashboard,
        endpoints: &[],
        model_task: None,
        confirm_note: None,
        writes: None,
        example: None,
    }
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        // -- Corpus CRUD -------------------------------------------------------
        DocRoute {
            description: "Every doc corpus with its badges and sources, plus the pending \
                doc-request count and the resolved rerank model.",
            response: Resp::Json(|g| g.root_schema_for::<dto::DocsOverview>()),
            ..base("GET", "/api/docs/corpora", "List doc corpora")
        },
        DocRoute {
            description: "Creates a corpus, resolving and pinning its embed and ingest \
                models before the row exists. start: true queues the ingest job \
                immediately.",
            request: Req::Json(|g| g.root_schema_for::<CreateCorpus>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::DocsJobStarted>()),
            ..base("POST", "/api/docs/corpora", "Create a doc corpus")
        },
        DocRoute {
            description: "One corpus's row, documents, golden queries and eval history \
                (limit caps the eval history; 0 or absent is the whole run).",
            path_ints: &["id"],
            query: Some(|g| g.root_schema_for::<DetailQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::CorpusDetail>()),
            ..base("GET", "/api/docs/corpora/{id}", "Get a corpus")
        },
        DocRoute {
            description: "Deletes a corpus and everything ingested into it.",
            path_ints: &["id"],
            response: Resp::Untyped("ad-hoc {ok, message}; no DTO"),
            confirm_note: Some("deletes every document, chunk and golden query in this corpus"),
            ..base("POST", "/api/docs/corpora/{id}/delete", "Delete a corpus")
        },
        DocRoute {
            description: "Queues (or reports already-running) the ingest job for this \
                corpus's configured sources.",
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<dto::DocsJobStarted>()),
            ..base(
                "POST",
                "/api/docs/corpora/{id}/ingest",
                "Start an ingest job",
            )
        },
        DocRoute {
            description: "Queues (or reports already-running) a re-embed job: fills in \
                missing vectors, or moves the pin onto a new embed_model.",
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<ReEmbedBody>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::DocsJobStarted>()),
            ..base(
                "POST",
                "/api/docs/corpora/{id}/re-embed",
                "Start a re-embed job",
            )
        },
        DocRoute {
            description: "Every document crawled into this corpus.",
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<dto::DocumentsResponse>()),
            ..base(
                "GET",
                "/api/docs/corpora/{id}/documents",
                "List a corpus's documents",
            )
        },
        DocRoute {
            description: "One document's chunks, verbatim payload included, for the corpus \
                browser's leaf view.",
            query: Some(|g| g.root_schema_for::<ChunksQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::ChunksResponse>()),
            ..base("GET", "/api/docs/chunks", "List a document's chunks")
        },
        // -- Debug search --------------------------------------------------------
        DocRoute {
            description: "Runs a retrieval search against one corpus with the same stage \
                pipeline docs__query uses, answering both the trace and the rendered \
                markdown a client would receive.",
            request: Req::Json(|g| g.root_schema_for::<SearchBody>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::DocsSearchResponse>()),
            // A POST only for its body: a search stores nothing (review R2 #8).
            writes: Some(false),
            ..base("POST", "/api/docs/search", "Search a corpus")
        },
        // -- Golden queries and eval ----------------------------------------------
        DocRoute {
            description: "One corpus's golden queries — the hand- or candidate-curated set \
                its eval runs are scored against.",
            query: Some(|g| g.root_schema_for::<CorpusQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::GoldenResponse>()),
            ..base("GET", "/api/docs/golden", "List golden queries")
        },
        DocRoute {
            description: "Creates or edits (id present) a golden query. Expected chunk ids \
                are validated against the corpus before they are stored.",
            request: Req::Json(|g| g.root_schema_for::<GoldenBody>()),
            response: Resp::Untyped("ad-hoc {ok, id}; no DTO"),
            ..base("POST", "/api/docs/golden", "Create or edit a golden query")
        },
        DocRoute {
            description: "Queues a run that samples chunks and asks the corpus's pinned \
                ingest model to write candidate golden queries from them — the queue is the \
                only path from a candidate to a real golden query.",
            request: Req::Json(|g| g.root_schema_for::<GenerateBody>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::DocsJobStarted>()),
            ..base(
                "POST",
                "/api/docs/golden/generate",
                "Generate golden-query candidates",
            )
        },
        DocRoute {
            description: "The synthetic golden-query curation queue, with the chunks each \
                candidate was written from alongside it.",
            query: Some(|g| g.root_schema_for::<CandidatesQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::GoldenCandidatesResponse>()),
            ..base(
                "GET",
                "/api/docs/golden/candidates",
                "List golden-query candidates",
            )
        },
        DocRoute {
            description: "Promotes a candidate to a real golden query, keeping origin: \
                synthetic. A decision is made once — a candidate not pending is refused.",
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<AcceptBody>()),
            response: Resp::Untyped("ad-hoc {ok, id}; no DTO"),
            ..base(
                "POST",
                "/api/docs/golden/candidates/{id}/accept",
                "Accept a golden-query candidate",
            )
        },
        DocRoute {
            description: "Discards a candidate; the row stays rejected rather than being \
                deleted, so a later generation run does not propose the same question again.",
            path_ints: &["id"],
            response: Resp::Untyped("ad-hoc {ok}; no DTO"),
            ..base(
                "POST",
                "/api/docs/golden/candidates/{id}/reject",
                "Reject a golden-query candidate",
            )
        },
        DocRoute {
            description: "Deletes one golden query.",
            path_ints: &["id"],
            response: Resp::Untyped("ad-hoc {ok}; no DTO"),
            ..base(
                "POST",
                "/api/docs/golden/{id}/delete",
                "Delete a golden query",
            )
        },
        DocRoute {
            description: "One corpus's eval-run history, most recent first (limit caps it; \
                0 or absent is the whole history — a score is read as a trend).",
            query: Some(|g| g.root_schema_for::<CorpusQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::EvalHistory>()),
            ..base("GET", "/api/docs/eval", "List eval runs")
        },
        DocRoute {
            description: "Queues (or reports already-running) an eval run over the corpus's \
                golden queries at the given hit@k depth.",
            request: Req::Json(|g| g.root_schema_for::<EvalBody>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::DocsJobStarted>()),
            ..base("POST", "/api/docs/eval", "Start an eval run")
        },
        // -- The doc-request queue -------------------------------------------------
        DocRoute {
            description: "The doc-request queue: libraries an agent asked for that this \
                gateway has no corpus for yet.",
            query: Some(|g| g.root_schema_for::<RequestsQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::DocRequestsResponse>()),
            ..base("GET", "/api/docs/requests", "List doc requests")
        },
        DocRoute {
            description: "Dismisses a doc request, or puts a dismissed one back to pending. \
                fulfilled is set only by a matching ingest completing, never by hand.",
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<RequestStatusBody>()),
            response: Resp::Untyped("ad-hoc {ok}; no DTO"),
            ..base(
                "POST",
                "/api/docs/requests/{id}/status",
                "Set a doc request's status",
            )
        },
        // -- Export / import ---------------------------------------------------------
        DocRoute {
            description: "The corpus store as a plain SQLite file — one corpus with \
                ?corpus_id=, or the whole file. The file can be copied to another machine as is.",
            query: Some(|g| g.root_schema_for::<ExportQuery>()),
            response: Resp::Binary(&["application/vnd.sqlite3"]),
            ..base("GET", "/api/docs/export", "Export corpora as a SQLite file")
        },
        DocRoute {
            description: "What a GET /api/docs/export download would contain, without \
                transferring the file — the half a UI shows before committing, and the half \
                an import re-derives to check against.",
            query: Some(|g| g.root_schema_for::<ExportQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::ExportManifest>()),
            ..base(
                "GET",
                "/api/docs/export/manifest",
                "Preview a corpus export's manifest",
            )
        },
        DocRoute {
            description: "Imports a corpus SQLite file (from GET /api/docs/export). \
                validate_only runs every check and writes nothing; replace overwrites an \
                existing library@version. Both checks happen before anything is written.",
            request: Req::Raw("application/vnd.sqlite3"),
            response: Resp::Json(|g| g.root_schema_for::<dto::ImportReport>()),
            confirm_note: Some("replace overwrites a library@version that already exists here"),
            query: Some(|g| g.root_schema_for::<ImportQuery>()),
            ..base(
                "POST",
                "/api/docs/import",
                "Import corpora from a SQLite file",
            )
        },
    ]
}
