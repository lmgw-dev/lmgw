//! `/api/knowledge/*`, the Knowledge bases API: named collections of a
//! person's own documents, their files, uploads, the background jobs that
//! chunk and embed them, the source viewer and a search playground. Every
//! shape is `lmgw-api-types`' `knowledge` module, the types the handlers
//! parse and build. Admin routes (`Cap::Admin`), so they appear in the admin
//! document only.

use lmgw_api_types::knowledge as kb;
use schemars::generate::SchemaGenerator;
use schemars::Schema;

use super::super::registry::{Dialect, DocRoute, Req, Resp};
use super::super::schemas;

fn route(
    method: &'static str,
    path: &'static str,
    summary: &'static str,
    description: &'static str,
) -> DocRoute {
    DocRoute {
        method,
        path,
        tag: "knowledge",
        summary,
        description,
        tool: None,
        query: None,
        path_ints: &[],
        request: Req::None,
        response: Resp::NoContent,
        dialect: Dialect::Dashboard,
        endpoints: &[],
        model_task: None,
        confirm_note: None,
        writes: None,
        example: None,
    }
}

fn upload_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "KnowledgeUploadRequest",
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "file": {
                    "type": "string",
                    "format": "binary",
                    "description": "One file; send as many parts as there are files. The \
                        part's field name does not matter, its file name names the file."
                }
            },
            "description": "multipart/form-data with one part per file."
        }),
    )
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<kb::KnowledgeBaseList>()),
            ..route(
                "GET",
                "/api/knowledge/bases",
                "List knowledge bases",
                "Every base with its counts, its pinned embedding model, whether that model \
                 still resolves on this gateway, the live job (or the last one that ran) and \
                 `notes`: one sentence each on what needs attention (files waiting with no job \
                 running, chunks without a vector, a chunk size above what the embedding model \
                 takes).",
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<kb::CreateKnowledgeBase>()),
            response: Resp::Json(|g| g.root_schema_for::<kb::KnowledgeBase>()),
            ..route(
                "POST",
                "/api/knowledge/bases",
                "Create a knowledge base",
                "Creates an empty base and pins it to the embedding model `embed_alias` \
                 resolves to; the alias is probed once to learn the vector length, so the model \
                 must be reachable. The pin outlives the alias: a different model is a settings \
                 change that re-embeds. Chunk size defaults to 512 tokens with 64 overlap. \
                 400 op_failed names the reason: an empty name, a name already used, an alias \
                 that is not an embedding (or rerank, or vision) model, a chunk size above what \
                 the embedding model takes per input, or an unreachable model.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<kb::KnowledgeBaseDetail>()),
            ..route(
                "GET",
                "/api/knowledge/bases/{id}",
                "Get a knowledge base",
                "One base, as in the list, and its files. 400 op_failed for a base that does \
                 not exist.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<kb::EditKnowledgeBase>()),
            response: Resp::Json(|g| g.root_schema_for::<kb::KnowledgeBaseEdited>()),
            ..route(
                "POST",
                "/api/knowledge/bases/{id}/settings",
                "Change a base's settings",
                "Every field is optional; an empty `rerank_alias` or `vision_alias` clears it. \
                 A different `embed_alias` re-pins the base and starts a re-embed job \
                 (`reembed_job`); a changed `chunk_tokens` or `chunk_overlap` sends every file \
                 back to pending (`rechunk_files`) and starts an ingest job (`ingest_job`). \
                 Both are background jobs: search keeps serving the old chunks until the new \
                 ones are in. A model or chunk-size change while a job runs on the base is refused \
                 (400 op_failed naming the job): wait for it or cancel it first. Other \
                 refusals as for create.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<kb::KnowledgeBaseDeleted>()),
            confirm_note: Some("deletes the base, its files and every chunk and vector"),
            ..route(
                "POST",
                "/api/knowledge/bases/{id}/delete",
                "Delete a knowledge base",
                "Deletes the base with its files, chunks and vectors, and the stored originals \
                 nothing else uses. A job running on the base is cancelled first. Threads that \
                 referenced the base keep their stored citations; they lose the base from \
                 later retrievals.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<kb::ResumeResult>()),
            ..route(
                "POST",
                "/api/knowledge/bases/{id}/resume",
                "Resume a base's pending work",
                "Starts the one job the base needs, or joins the one already running: a \
                 re-embed (`kb_reembed`) when chunks lack a vector or the base is marked \
                 `re_embed_required`, otherwise an ingest (`kb_ingest`) of the pending files. \
                 The route only starts the job and answers at once with its id in `job`; \
                 measuring the stored chunks against the model, re-chunking what does not \
                 fit and embedding all run in the job. When the last re-embed was refused as \
                 larger than the model's input, the files holding the oversized chunks are \
                 marked pending and an ingest (`kb_ingest`) re-chunks them instead: `kind` \
                 is `kb_ingest`, and `rechunk` says why. A base with nothing waiting answers \
                 `job: null` and a message. Observe progress with the job: `GET /api/jobs` \
                 lists it (key `kb:{id}`), the `jobs` frames of `GET /api/events` stream it, \
                 and the base's own `job` field keeps the last outcome. A file that failed is \
                 not retried here; re-ingest it.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<kb::CancelResult>()),
            ..route(
                "POST",
                "/api/knowledge/bases/{id}/cancel",
                "Cancel a base's running job",
                "Asks the base's running ingest or re-embed to stop; the job finishes its \
                 current step and ends as canceled. Files not yet read stay pending, vectors \
                 already written stay, and Resume continues from there. 400 op_failed when no \
                 job is running on the base.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<kb::KnowledgeFileList>()),
            ..route(
                "GET",
                "/api/knowledge/bases/{id}/files",
                "List a base's files",
                "The files of the base, with their ingest status (`pending`, `ingesting`, \
                 `ready`, `failed`), chunk counts and, for a failed one, the reason.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::Multipart(upload_request),
            response: Resp::Json(|g| g.root_schema_for::<kb::UploadResult>()),
            ..route(
                "POST",
                "/api/knowledge/bases/{id}/files",
                "Upload files into a base",
                "multipart/form-data, one part per file (the part's file name is the stored \
                 name, reduced to its last path component). The whole request is bounded by \
                 the max_body_mb setting (0 is unbounded): 413 body_limit naming it, for a \
                 declared or a streamed body. Each file's kind is sniffed from its bytes, \
                 never its name: images and audio are refused, as is an empty file. The \
                 answer has one item per part: `added`, `replaced` (the name existed with \
                 other bytes, or its ingest had failed; the file is queued again), `unchanged` \
                 (same name, same bytes), `duplicate` (same bytes under another name) or \
                 `refused` with the `reason`. A refused file does not fail the request. Added \
                 and replaced files are queued and the ingest job is started or joined: `job` \
                 is its id (`null` when nothing was queued). Ingestion — reading, chunking, \
                 embedding, a vision model for text-less PDF pages — runs in that job; \
                 observe it as for Resume. 400 no_files for a request without any part, \
                 400 bad_request for a malformed multipart body.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<kb::KnowledgeFileDeleted>()),
            confirm_note: Some("deletes the file and its chunks"),
            ..route(
                "POST",
                "/api/knowledge/files/{id}/delete",
                "Delete a file",
                "Deletes the file with its chunks and vectors, and its stored original unless \
                 another file shares the bytes. 400 op_failed for a file being ingested right \
                 now (cancel the base's job first) or one that does not exist.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<kb::KnowledgeFileRequeued>()),
            ..route(
                "POST",
                "/api/knowledge/files/{id}/reingest",
                "Ingest a file again",
                "Marks the file pending and starts (or joins) the base's ingest job, whose id \
                 is `job`. Use it after the base gained a vision model, or for a failed file \
                 whose cause is fixed. The file's earlier chunks stay searchable until the new \
                 ones are in. 400 op_failed for a file being ingested right now.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            query: Some(|g| g.root_schema_for::<kb::SourceQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<kb::KnowledgeSource>()),
            ..route(
                "GET",
                "/api/knowledge/files/{id}/text",
                "Read a file's extracted text",
                "The source viewer: the file's extracted text (`null` until it has been \
                 ingested once), every chunk's byte range in that text, and the chunk named by \
                 `chunk` as `highlight`. A citation whose chunk id a later re-ingest renamed \
                 is found again by the `span_start`, `span_end` and `sha` the citation stored, \
                 and `notice` says so (or that the file changed since). 400 op_failed for a \
                 file that does not exist.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Binary(&["*/*"]),
            ..route(
                "GET",
                "/api/knowledge/files/{id}/original",
                "Download a file's original",
                "The uploaded bytes, with the media type they were sniffed as and \
                 Content-Disposition: attachment under the file's name (an ASCII fallback \
                 plus the RFC 5987 filename*). 404 not_found for a file that does not exist \
                 or whose original is gone from disk.",
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<kb::KnowledgeSearchRequest>()),
            response: Resp::Json(|g| g.root_schema_for::<kb::KnowledgeSearchResult>()),
            writes: Some(false),
            ..route(
                "POST",
                "/api/knowledge/search",
                "Search knowledge bases",
                "The search playground: hybrid keyword and vector search with fusion and, \
                 where the base names a reranker, reranking, over the bases in `kb_ids` (none \
                 given: every base, whether or not it is visible to the `kb__*` tools). The \
                 answer has the excerpts with their scores and positions, `notes` for what was \
                 not searched or not fully (a base still ingesting, chunks without a vector, a \
                 model that no longer resolves), and `traces`, one per embedding and rerank \
                 model pair, with what each stage saw and how long it took. `budget_tokens` \
                 bounds the excerpts' token total; `params` overrides the search stages. \
                 Searching embeds the query, which costs a model call. A search that cannot use a \
                 model says so in `notes` instead of failing. 400 op_failed for \
                 an unknown `params` key.",
            )
        },
    ]
}
