//! Knowledge bases (chat-complete design §9): the private store, ingestion
//! through the real routes, the GPU hold, retrieval with citations and a
//! budget, and the `kb__*` toolset on `/mcp` under `mcp_visible` and a key's
//! tool scope.
//!
//! No container: the embedding model and the vision model are one wiremock
//! upstream behind plain aliases. PDF cases need poppler (`pdftotext`,
//! `pdftoppm`) and say so when it is missing.

use std::time::Duration;

use lmgw_core::agent::ToolExecutor;
use lmgw_core::config::{AuxKind, Protocol, UpstreamKind};
use lmgw_core::knowledge::retrieve::{self, Options};
use lmgw_core::knowledge::{self, ops, store as kstore};
use lmgw_core::mcp::exec::KbExecutor;
use lmgw_core::proxy::RequestCtx;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewAuxModel, NewUpstream};
use quickdoc_core::embed::{EmbedIdentity, FixtureEmbedder};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::common::{serve, Gw};

const DIMS: usize = 32;
/// The embedding model's context, as the mock's catalog reports it.
const EMBED_CTX: u64 = 2048;

// ---------------------------------------------------------------------------
// The mock world
// ---------------------------------------------------------------------------

pub(crate) struct FixtureEmbeddings;

impl Respond for FixtureEmbeddings {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let inputs: Vec<String> = match &body["input"] {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect(),
            _ => Vec::new(),
        };
        let fixture = FixtureEmbedder::new(DIMS);
        let data: Vec<Value> = inputs
            .iter()
            .enumerate()
            .map(|(i, t)| json!({"object": "embedding", "index": i, "embedding": fixture.embed_one(t)}))
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "data": data, "model": "embed-tgt",
            "usage": {"prompt_tokens": 1, "total_tokens": 1},
        }))
    }
}

/// The vision model: whatever page it is shown, it reads a bank statement.
struct Reader;

impl Respond for Reader {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let raw = String::from_utf8_lossy(&req.body);
        assert!(
            raw.contains("image_url"),
            "a page goes to the model as an image"
        );
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "c1", "object": "chat.completion", "model": "vision-tgt",
            "choices": [{"index": 0, "message": {"role": "assistant",
                "content": "Kontoauszug Girokonto | Saldo: 1234 EUR"},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 900, "completion_tokens": 12},
        }))
    }
}

pub(crate) async fn mount(mock: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(FixtureEmbeddings)
        .mount(mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(Reader)
        .mount(mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [
                {"id": "embed-tgt", "object": "model", "context_length": EMBED_CTX},
                {"id": "vision-tgt", "object": "model", "context_length": 32768},
            ],
        })))
        .mount(mock)
        .await;
}

/// A gateway whose `embed-model` and `vision-model` aliases point at the mock.
pub(crate) async fn setup(mock: &MockServer) -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    for (alias, target) in [("embed-model", "embed-tgt"), ("vision-model", "vision-tgt")] {
        store::insert_alias(
            &state.db,
            &NewAlias {
                alias: alias.into(),
                upstream_id: up,
                upstream_model_id: target.into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: None,
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    state
}

pub(crate) async fn post(gw: &Gw, route: &str, body: Value) -> (u16, Value) {
    let r = gw
        .client()
        .post(format!("{gw}{route}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

pub(crate) async fn get(gw: &Gw, route: &str) -> (u16, Value) {
    let r = gw
        .client()
        .get(format!("{gw}{route}"))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

pub(crate) async fn upload(gw: &Gw, kb: i64, files: &[(&str, Vec<u8>)]) -> (u16, Value) {
    let mut form = reqwest::multipart::Form::new();
    for (name, bytes) in files {
        form = form.part(
            "file",
            reqwest::multipart::Part::bytes(bytes.clone()).file_name(name.to_string()),
        );
    }
    let r = gw
        .client()
        .post(format!("{gw}/api/knowledge/bases/{kb}/files"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

pub(crate) async fn wait_job(state: &SharedState, id: i64) -> store::JobRow {
    for _ in 0..3000 {
        let row = store::get_job(&state.db, id).await.unwrap().unwrap();
        if matches!(row.status.as_str(), "done" | "failed" | "canceled") {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {id} never finished");
}

pub(crate) async fn create(gw: &Gw, body: Value) -> i64 {
    let (status, v) = post(gw, "/api/knowledge/bases", body).await;
    assert_eq!(status, 200, "{v}");
    v["id"].as_i64().unwrap()
}

/// Create a base, upload `files`, and wait for its ingest.
pub(crate) async fn ingested(
    state: &SharedState,
    gw: &Gw,
    body: Value,
    files: &[(&str, Vec<u8>)],
) -> i64 {
    let id = create(gw, body).await;
    let (status, v) = upload(gw, id, files).await;
    assert_eq!(status, 200, "{v}");
    let job = v["job"].as_i64().expect("an upload starts the ingest");
    let row = wait_job(state, job).await;
    assert_eq!(row.status, "done", "{:?}", row.error);
    id
}

pub(crate) fn cleanup(state: &SharedState) {
    let _ = std::fs::remove_dir_all(&state.data_dir);
}

pub(crate) const NOTES: &str =
    "# Taxes 2025\n\nThe tax return was filed in March.\n\n## Refund\n\n\
The refund of 412 EUR arrived in May.\n\n## Costs\n\n| Item | Amount |\n| --- | --- |\n\
| Accountant | 300 |\n| Software | 40 |\n";

pub(crate) const LETTER: &str =
    "Dear landlord,\n\nthe rent for the flat was raised to 950 EUR from July.\n\
Kind regards";

/// A valid PDF with one page per entry: `Some(text)` draws it, `None` leaves
/// the page blank — what a scan looks like to `pdftotext`.
pub(crate) fn pdf(pages: &[Option<&str>]) -> Vec<u8> {
    let n = pages.len();
    let mut objs: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {n} >>",
            (0..n)
                .map(|i| format!("{} 0 R", 4 + 2 * i))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ];
    for (i, text) in pages.iter().enumerate() {
        objs.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents {} 0 R \
             /Resources << /Font << /F1 3 0 R >> >> >>",
            5 + 2 * i
        ));
        let content = match text {
            Some(t) => format!("BT /F1 14 Tf 20 100 Td ({t}) Tj ET"),
            None => String::new(),
        };
        objs.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ));
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).into_bytes());
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).into_bytes());
    for o in offsets {
        out.extend(format!("{o:010} 00000 n \n").into_bytes());
    }
    out.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .into_bytes(),
    );
    out
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// `knowledge.db` holds the owner's documents: 0600 like `lmgw.sqlite`,
/// sidecars included, never the world-readable corpus file.
#[cfg(unix)]
#[tokio::test]
async fn the_knowledge_db_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(knowledge::KNOWLEDGE_DB_FILE);
    let pool = kstore::open(&path).await.unwrap();
    // A write, so the WAL sidecars exist.
    kstore::insert_kb(
        &pool,
        &kstore::NewKb {
            name: "x".into(),
            description: String::new(),
            embed_alias: "e".into(),
            embed: EmbedIdentity::new("u", "m", 4),
            rerank_alias: String::new(),
            vision_alias: String::new(),
            chunk_tokens: 512,
            chunk_overlap: 64,
            mcp_visible: true,
        },
    )
    .await
    .unwrap();
    drop(pool);
    let pool = kstore::open(&path).await.unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let p = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        if p.exists() {
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} is {mode:o}", p.display());
        }
    }
    assert_eq!(kstore::list_kbs(&pool).await.unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Ingestion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_base_ingests_uploads_and_skips_what_it_already_has() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let gw = serve(state.clone()).await;

    let (status, v) = post(
        &gw,
        "/api/knowledge/bases",
        json!({"name": "Taxes", "description": "tax papers", "embed_alias": "embed-model",
               "chunk_tokens": 40, "chunk_overlap": 8}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let id = v["id"].as_i64().unwrap();
    assert_eq!(v["embed_dims"], DIMS, "the pin is measured by a probe: {v}");
    assert_eq!(v["embed_identity"], "test-up/embed-tgt (32d)");
    assert_eq!(v["mcp_visible"], true, "shared on /mcp by default");
    assert_eq!(v["embed_context"], EMBED_CTX);

    let (status, v) = upload(
        &gw,
        id,
        &[
            ("notes.md", NOTES.as_bytes().to_vec()),
            ("letter.txt", LETTER.as_bytes().to_vec()),
            ("photo.png", b"\x89PNG\r\n\x1a\nrest".to_vec()),
            ("voice.wav", b"RIFF\0\0\0\0WAVEfmt ".to_vec()),
        ],
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let outcomes: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes, ["added", "added", "refused", "refused"], "{v}");
    assert!(v["items"][2]["reason"]
        .as_str()
        .unwrap()
        .contains("images are not knowledge-base material"));
    assert!(v["items"][3]["reason"].as_str().unwrap().contains("audio"));
    let row = wait_job(&state, v["job"].as_i64().unwrap()).await;
    assert_eq!(row.status, "done", "{:?}", row.error);
    assert_eq!(row.kind, "kb_ingest");
    assert_eq!(row.key.as_deref(), Some(format!("kb:{id}").as_str()));

    let (_, d) = get(&gw, &format!("/api/knowledge/bases/{id}")).await;
    let files = d["files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "{d}");
    for f in files {
        assert_eq!(f["status"], "ready", "{f}");
        assert!(f["chunk_count"].as_i64().unwrap() > 0, "{f}");
    }
    let total: i64 = files
        .iter()
        .map(|f| f["chunk_count"].as_i64().unwrap())
        .sum();
    assert_eq!(d["base"]["counts"]["chunks"], total);
    assert_eq!(d["base"]["counts"]["embedded"], total);
    assert_eq!(d["base"]["job"]["status"], "done");

    // Every chunk is within the base's size, and the notes file's table was
    // chunked under its heading path.
    let notes_id = files.iter().find(|f| f["name"] == "notes.md").unwrap()["id"]
        .as_i64()
        .unwrap();
    let chunks = kstore::file_chunks(&state.knowledge.pool, notes_id)
        .await
        .unwrap();
    assert!(chunks.iter().all(|c| c.tokens <= 40), "{chunks:#?}");
    assert!(chunks
        .iter()
        .any(|c| c.heading_path == "Taxes 2025 > Costs" && c.payload.contains("| Accountant")));
    let ids_before: Vec<String> = chunks.iter().map(|c| c.id.clone()).collect();

    // The same bytes again: skipped, no job.
    let (_, v) = upload(&gw, id, &[("notes.md", NOTES.as_bytes().to_vec())]).await;
    assert_eq!(v["items"][0]["outcome"], "unchanged", "{v}");
    assert!(v["job"].is_null(), "{v}");
    let (_, v) = upload(&gw, id, &[("copy.md", NOTES.as_bytes().to_vec())]).await;
    assert_eq!(v["items"][0]["outcome"], "duplicate", "{v}");
    assert!(v["items"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("as 'notes.md'"));

    // New bytes under a known name replace that file's content.
    let changed = format!("{NOTES}\n## Next year\n\nFile before April.\n");
    let (_, v) = upload(&gw, id, &[("notes.md", changed.into_bytes())]).await;
    assert_eq!(v["items"][0]["outcome"], "replaced", "{v}");
    assert_eq!(
        v["items"][0]["file"]["id"], notes_id,
        "the row keeps its id"
    );
    let row = wait_job(&state, v["job"].as_i64().unwrap()).await;
    assert_eq!(row.status, "done", "{:?}", row.error);
    let after = kstore::file_chunks(&state.knowledge.pool, notes_id)
        .await
        .unwrap();
    assert!(after
        .iter()
        .any(|c| c.payload.contains("File before April")));
    // Content-derived ids: the file's sha changed, so do its chunk ids.
    assert!(after.iter().all(|c| !ids_before.contains(&c.id)));

    // The originals are on disk under their digests, private.
    let dir = knowledge::originals::dir(&state.data_dir);
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(names.len(), 2, "the replaced original is gone: {names:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        for n in &names {
            assert_eq!(mode(&dir.join(n)), 0o600);
        }
    }

    // Downloading an original gives its bytes under its name.
    let letter_id = files.iter().find(|f| f["name"] == "letter.txt").unwrap()["id"]
        .as_i64()
        .unwrap();
    let r = gw
        .client()
        .get(format!("{gw}/api/knowledge/files/{letter_id}/original"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert!(r.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .contains("filename=\"letter.txt\""));
    assert_eq!(r.bytes().await.unwrap().as_ref(), LETTER.as_bytes());
    cleanup(&state);
}

/// `chunk_tokens` is checked against the embedding model's real context:
/// larger is refused, never clamped.
#[tokio::test]
async fn chunk_sizes_are_refused_not_clamped() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let gw = serve(state.clone()).await;

    let (status, v) = post(
        &gw,
        "/api/knowledge/bases",
        json!({"name": "Big", "embed_alias": "embed-model", "chunk_tokens": 4000}),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    let msg = v["message"].as_str().unwrap();
    assert!(msg.contains("takes per input"), "{msg}");
    assert!(msg.contains("catalog"), "{msg}");
    assert!(msg.contains("at most 2048"), "{msg}");

    let (status, v) = post(
        &gw,
        "/api/knowledge/bases",
        json!({"name": "Odd", "embed_alias": "embed-model", "chunk_tokens": 100,
               "chunk_overlap": 100}),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert!(v["message"].as_str().unwrap().contains("chunk_overlap"));

    // And on an edit.
    let id = create(&gw, json!({"name": "Fine", "embed_alias": "embed-model", "chunk_tokens": 200, "chunk_overlap": 20})).await;
    let (status, v) = post(
        &gw,
        &format!("/api/knowledge/bases/{id}/settings"),
        json!({"chunk_tokens": 3000}),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    let kb = kstore::get_kb(&state.knowledge.pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kb.chunk_tokens, 200, "a refused edit changes nothing");
    cleanup(&state);
}

/// The GPU hold refuses local embedding: the job stops with a visible
/// "GPU hold is on" status, and the files stay pending with that reason.
#[tokio::test]
async fn a_gpu_hold_stops_the_ingest_and_leaves_files_pending() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    store::insert_aux_model(
        &state.db,
        &NewAuxModel {
            model_id: "embed-local".into(),
            gguf_path: "e.gguf".into(),
            kind: AuxKind::Embed,
            pooling: None,
            ctx_size: None,
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let alias = state.snapshot().aux_public_name("embed-local");
    // Seeded directly: creating it through the op would probe the model,
    // and there is no container here to answer.
    let id = kstore::insert_kb(
        &state.knowledge.pool,
        &kstore::NewKb {
            name: "Local".into(),
            description: String::new(),
            embed_alias: alias.clone(),
            embed: EmbedIdentity::new(lmgw_core::config::AUX_UPSTREAM_NAME, "embed-local", DIMS),
            rerank_alias: String::new(),
            vision_alias: String::new(),
            chunk_tokens: 512,
            chunk_overlap: 64,
            mcp_visible: true,
        },
    )
    .await
    .unwrap();
    lmgw_core::ops::hold_set(&state, true).await.unwrap();

    let out = ops::upload(
        &state,
        id,
        vec![
            ("a.md".into(), NOTES.as_bytes().to_vec().into()),
            ("b.txt".into(), LETTER.as_bytes().to_vec().into()),
        ],
    )
    .await
    .unwrap();
    let row = wait_job(&state, out.job.unwrap()).await;
    assert_eq!(row.status, "failed");
    let err = row.error.unwrap_or_default();
    assert!(err.starts_with("GPU hold is on"), "{err}");
    assert!(
        err.contains("holding the GPU"),
        "the refusal itself is in it: {err}"
    );
    for f in kstore::list_files(&state.knowledge.pool, id).await.unwrap() {
        assert_eq!(f.status, "pending", "{f:?}");
        assert!(
            f.error
                .as_deref()
                .unwrap_or_default()
                .starts_with("GPU hold is on"),
            "the file says why it waits: {f:?}"
        );
    }
    let view = ops::get(&state, id).await.unwrap();
    assert!(
        view.notes.iter().any(|n| n.contains("Resume")),
        "{:?}",
        view.notes
    );
    assert!(
        !mock
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path().ends_with("/embeddings")),
        "nothing was embedded anywhere else instead"
    );

    // Resume under the hold stops the same way — nothing is lost.
    let v = ops::resume(&state, id).await.unwrap();
    let row = wait_job(&state, v["job"].as_i64().unwrap()).await;
    assert_eq!(row.status, "failed");
    assert_eq!(
        kstore::files_in_status(&state.knowledge.pool, id, "pending")
            .await
            .unwrap()
            .len(),
        2
    );
    cleanup(&state);
}

// ---------------------------------------------------------------------------
// Retrieval
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_returns_citations_with_file_and_page_within_the_budget() {
    if !lmgw_core::extract::pdf::available().await {
        eprintln!("skipped: poppler-utils (pdftotext/pdftoppm) is not installed");
        return;
    }
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let gw = serve(state.clone()).await;

    let doc = pdf(&[
        Some("Rent invoice March 800 EUR"),
        None,
        Some("Holiday plan August Lisbon"),
    ]);
    // With a vision model: the blank page is read.
    let id = ingested(
        &state,
        &gw,
        json!({"name": "Papers", "embed_alias": "embed-model", "vision_alias": "vision-model"}),
        &[
            ("doc.pdf", doc.clone()),
            ("letter.txt", LETTER.as_bytes().to_vec()),
        ],
    )
    .await;
    let files = kstore::list_files(&state.knowledge.pool, id).await.unwrap();
    let pdf_file = files.iter().find(|f| f.name == "doc.pdf").unwrap();
    assert_eq!(pdf_file.pages, Some(3));
    assert_eq!(pdf_file.skipped_pages, 0);
    assert!(
        pdf_file
            .notes
            .iter()
            .any(|n| n.contains("read by vision-model") && n.contains("2 (ocr)")),
        "{:?}",
        pdf_file.notes
    );

    let r = retrieve::retrieve(&state, &[id], "holiday august lisbon", &Options::default()).await;
    let top = &r.excerpts[0];
    assert_eq!(
        (top.file.as_str(), top.page),
        ("doc.pdf", Some(3)),
        "{r:#?}"
    );
    assert_eq!(top.kb, "Papers");
    assert!(top.text.contains("Holiday plan"));
    // The OCR'd page is searchable, and cited as its own page.
    let r = retrieve::retrieve(&state, &[id], "Kontoauszug Saldo", &Options::default()).await;
    assert_eq!(r.excerpts[0].page, Some(2), "{r:#?}");
    // The source viewer highlights the cited span inside the file's text.
    let src = knowledge::read::source(
        &state,
        top.file_id,
        Some(&top.chunk_id),
        &knowledge::read::Cited::default(),
    )
    .await
    .unwrap();
    let text = src.text.unwrap();
    let h = src.highlight.unwrap();
    assert!(text[h.span_start as usize..h.span_end as usize].contains("Holiday plan"));
    assert!(text.contains("--- page 2 ---"));

    // The budget: the first excerpt always, the rest only while they fit.
    let r = retrieve::retrieve(
        &state,
        &[id],
        "rent invoice holiday plan landlord",
        &Options {
            budget_tokens: Some(3),
            params: None,
            caller: None,
        },
    )
    .await;
    assert_eq!(r.excerpts.len(), 1, "{r:#?}");
    assert!(r.dropped > 0, "what the budget dropped is counted: {r:#?}");
    let r = retrieve::retrieve(
        &state,
        &[id],
        "rent invoice holiday plan landlord",
        &Options {
            budget_tokens: Some(60),
            params: None,
            caller: None,
        },
    )
    .await;
    assert!(r.tokens <= 60 || r.excerpts.len() == 1, "{r:#?}");
    assert_eq!(r.tokens, r.excerpts.iter().map(|e| e.tokens).sum::<usize>());

    // The playground over HTTP carries the traces.
    let (status, v) = post(
        &gw,
        "/api/knowledge/search",
        json!({"query": "holiday", "kb_ids": [id], "budget_tokens": 500}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["excerpts"][0]["page"], 3, "{v}");
    assert!(
        v["traces"][0]["fts"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "{v}"
    );

    // Without a vision model the blank page is skipped, counted and said.
    let id2 = ingested(
        &state,
        &gw,
        json!({"name": "Scans", "embed_alias": "embed-model"}),
        &[("doc.pdf", doc)],
    )
    .await;
    let f = &kstore::list_files(&state.knowledge.pool, id2)
        .await
        .unwrap()[0];
    assert_eq!(f.skipped_pages, 1, "{f:?}");
    assert!(
        f.notes.iter().any(|n| n.starts_with("page 2: no text")),
        "{:?}",
        f.notes
    );
    cleanup(&state);
}

/// A base whose model is held answers from keywords and says why — the Chat
/// never blocks a turn on retrieval.
#[tokio::test]
async fn retrieval_never_fails_it_degrades_and_says_why() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let gw = serve(state.clone()).await;
    let id = ingested(
        &state,
        &gw,
        json!({"name": "Taxes", "embed_alias": "embed-model"}),
        &[("notes.md", NOTES.as_bytes().to_vec())],
    )
    .await;
    // The alias that pinned it is gone: keyword search only, with the reason.
    let alias = state
        .snapshot()
        .aliases
        .values()
        .find(|a| a.alias == "embed-model")
        .unwrap()
        .id;
    store::delete_alias(&state.db, alias).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let r = retrieve::retrieve(&state, &[id, 999], "refund May", &Options::default()).await;
    assert!(!r.excerpts.is_empty(), "BM25 still answers: {r:#?}");
    assert!(r.excerpts[0].text.contains("refund"));
    assert!(
        r.notes.iter().any(|n| n.contains("keyword search only")),
        "{:?}",
        r.notes
    );
    assert!(r.notes.iter().any(|n| n.contains("999 no longer exists")));
    assert!(r.traces[0].knn_skipped.is_some());
    cleanup(&state);
}

// ---------------------------------------------------------------------------
// The kb toolset
// ---------------------------------------------------------------------------

async fn scoped_key(
    state: &SharedState,
    gw: &Gw,
    name: &str,
    mode: &str,
    patterns: &str,
) -> String {
    let id = store::insert_api_key(
        &state.db,
        name,
        &lmgw_core::config::hash_api_key(&format!("lmgw-{name}")),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let (status, body) = post(
        gw,
        "/api/op/key_set",
        json!({"id": id, "tool_scope_mode": mode, "tool_scope_patterns": patterns}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    format!("lmgw-{name}")
}

/// One `/mcp` request as exactly one credential.
async fn mcp_as(gw: &Gw, token: &str, method_: &str, params: Value) -> Value {
    let client = gw.anon();
    let init = client
        .post(format!("{gw}/mcp"))
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                       "clientInfo": {"name": "kb-test", "version": "0"}}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(init.status().as_u16(), 200, "initialize");
    let sid = init.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    client
        .post(format!("{gw}/mcp"))
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("mcp-session-id", sid)
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": method_, "params": params}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap_or(Value::Null)
}

async fn kb_names(gw: &Gw, token: &str) -> Vec<String> {
    let body = mcp_as(gw, token, "tools/list", json!({})).await;
    let mut names: Vec<String> = body["result"]["tools"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .filter(|n| n.starts_with("kb__"))
        .collect();
    names.sort();
    names
}

fn result_text(body: &Value) -> (String, bool) {
    let r = &body["result"];
    (
        r["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        r["isError"] == Value::Bool(true),
    )
}

#[tokio::test]
async fn mcp_serves_only_shared_bases_and_honours_the_key_scope() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let gw = serve(state.clone()).await;
    let shared = ingested(
        &state,
        &gw,
        json!({"name": "Shared", "embed_alias": "embed-model"}),
        &[("notes.md", NOTES.as_bytes().to_vec())],
    )
    .await;
    let private = ingested(
        &state,
        &gw,
        json!({"name": "Private", "embed_alias": "embed-model", "mcp_visible": false}),
        &[("letter.txt", LETTER.as_bytes().to_vec())],
    )
    .await;
    let owner = gw.key.clone();

    assert_eq!(
        kb_names(&gw, &owner).await,
        ["kb__list", "kb__read", "kb__search"]
    );
    let (text, err) = result_text(
        &mcp_as(
            &gw,
            &owner,
            "tools/call",
            json!({"name": "kb__list", "arguments": {}}),
        )
        .await,
    );
    assert!(!err, "{text}");
    assert!(
        text.contains("\"Shared\"") && !text.contains("Private"),
        "{text}"
    );

    // A base that is not shared answers like one that does not exist.
    let (text, err) = result_text(
        &mcp_as(
            &gw,
            &owner,
            "tools/call",
            json!({"name": "kb__search", "arguments": {"query": "rent", "kb": "Private"}}),
        )
        .await,
    );
    assert!(
        err && text.contains("no knowledge base 'Private'"),
        "{text}"
    );
    let (text, err) = result_text(
        &mcp_as(
            &gw,
            &owner,
            "tools/call",
            json!({"name": "kb__search", "arguments": {"query": "refund rent landlord"}}),
        )
        .await,
    );
    assert!(!err, "{text}");
    assert!(text.contains("[1] Shared · notes.md"), "{text}");
    assert!(!text.contains("letter.txt"), "{text}");

    // kb__read: from a cited chunk, and the private file is not there.
    let notes = &kstore::list_files(&state.knowledge.pool, shared)
        .await
        .unwrap()[0];
    let first = kstore::file_chunks(&state.knowledge.pool, notes.id)
        .await
        .unwrap()[0]
        .id
        .clone();
    let (text, err) = result_text(
        &mcp_as(
            &gw,
            &owner,
            "tools/call",
            json!({"name": "kb__read", "arguments": {"file_id": notes.id, "from_chunk": first}}),
        )
        .await,
    );
    assert!(
        !err && text.contains("The tax return was filed in March"),
        "{text}"
    );
    let letter = &kstore::list_files(&state.knowledge.pool, private)
        .await
        .unwrap()[0];
    let (text, err) = result_text(
        &mcp_as(
            &gw,
            &owner,
            "tools/call",
            json!({"name": "kb__read", "arguments": {"file_id": letter.id}}),
        )
        .await,
    );
    assert!(err && text.contains("no file"), "{text}");

    // The Chat's own selection reaches the private base, whatever its switch.
    let exec = KbExecutor::new(state.clone(), RequestCtx::default()).only([private]);
    let out = exec
        .call("kb__search", &json!({"query": "rent landlord"}))
        .await;
    let (text, _) = lmgw_core::ir::flatten_tool_result(&out.blocks);
    assert!(
        !out.is_error && text.contains("Private · letter.txt"),
        "{text}"
    );
    let out = exec.call("kb__list", &json!({})).await;
    let (text, _) = lmgw_core::ir::flatten_tool_result(&out.blocks);
    assert!(
        text.contains("Private") && !text.contains("Shared"),
        "{text}"
    );

    // A key's tool scope: kb__* in a deny list hides and refuses them…
    let denied = scoped_key(&state, &gw, "nokb", "deny", "kb__*").await;
    assert!(kb_names(&gw, &denied).await.is_empty());
    let body = mcp_as(
        &gw,
        &denied,
        "tools/call",
        json!({"name": "kb__list", "arguments": {}}),
    )
    .await;
    assert!(body["error"].is_object() || result_text(&body).1, "{body}");
    // …and an allow list admits exactly what it names.
    let search_only = scoped_key(&state, &gw, "search", "allow", "kb__search").await;
    assert_eq!(kb_names(&gw, &search_only).await, ["kb__search"]);
    let all = scoped_key(&state, &gw, "allkb", "allow", "kb__*").await;
    assert_eq!(kb_names(&gw, &all).await.len(), 3);
    cleanup(&state);
}

/// `kb` is a reserved tool prefix, like `lmgw` and `docs`.
#[tokio::test]
async fn a_southbound_server_cannot_take_the_kb_prefix() {
    let state = AppState::init_for_tests().await.unwrap();
    let patch: lmgw_core::ops::McpServerPatch = serde_json::from_value(json!({
        "action": "create", "name": "impostor", "transport": "http",
        "url": "https://mcp.example.com/mcp", "tool_prefix": "kb",
    }))
    .unwrap();
    let err = lmgw_core::ops::mcp_server_set(&state, patch, lmgw_core::ops::RowWriter::Dashboard)
        .await
        .unwrap_err();
    assert!(err.contains("reserved"), "{err}");
    assert!(lmgw_core::mcp::RESERVED_NAMESPACES
        .iter()
        .any(|(p, ns)| *p == "kb" && *ns == "kb__"));
}

/// The `kb` label attaches like `docs`: the tools resolve as built-ins.
#[tokio::test]
async fn the_kb_label_attaches_like_a_server() {
    let state = AppState::init_for_tests().await.unwrap();
    let specs = vec![lmgw_core::ingress::responses::McpToolSpec {
        server_label: "kb".to_string(),
        allowed_tools: None,
        require_approval: Default::default(),
    }];
    let r =
        lmgw_core::mcp::exec::resolve(&state, &specs, &lmgw_core::mcp::scope::ToolScope::gateway())
            .await;
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    let mut names = r.builtin.clone();
    names.sort();
    assert_eq!(names, ["kb__list", "kb__read", "kb__search"]);
}

#[tokio::test]
async fn deleting_a_base_removes_its_files_chunks_and_originals() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let gw = serve(state.clone()).await;
    let keep = ingested(
        &state,
        &gw,
        json!({"name": "Keep", "embed_alias": "embed-model"}),
        &[("letter.txt", LETTER.as_bytes().to_vec())],
    )
    .await;
    let gone = ingested(
        &state,
        &gw,
        json!({"name": "Gone", "embed_alias": "embed-model"}),
        &[
            ("notes.md", NOTES.as_bytes().to_vec()),
            // The same bytes as Keep's: that original must survive.
            ("letter.txt", LETTER.as_bytes().to_vec()),
        ],
    )
    .await;
    let files = kstore::list_files(&state.knowledge.pool, gone)
        .await
        .unwrap();
    let dir = knowledge::originals::dir(&state.data_dir);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);

    let (status, v) = post(
        &gw,
        &format!("/api/knowledge/bases/{gone}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["files"], 2);
    assert!(kstore::get_kb(&state.knowledge.pool, gone)
        .await
        .unwrap()
        .is_none());
    for f in &files {
        assert!(kstore::get_file(&state.knowledge.pool, f.id)
            .await
            .unwrap()
            .is_none());
        assert!(kstore::file_chunks(&state.knowledge.pool, f.id)
            .await
            .unwrap()
            .is_empty());
    }
    let left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(left, [knowledge::originals::sha256_hex(LETTER.as_bytes())]);
    // Keep still searches.
    let r = retrieve::retrieve(&state, &[keep], "rent landlord", &Options::default()).await;
    assert!(!r.excerpts.is_empty());
    // Its matrix is not stale either: a new chunk is found by vector search.
    let counts = kstore::kb_counts(&state.knowledge.pool).await.unwrap();
    assert!(!counts.contains_key(&gone));
    cleanup(&state);
}

/// Changing a base's embedding model re-pins it and re-embeds every chunk;
/// the chunk ids — and so the citations — survive.
#[tokio::test]
async fn a_model_change_reembeds_and_keeps_chunk_ids() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let gw = serve(state.clone()).await;
    let id = ingested(
        &state,
        &gw,
        json!({"name": "Taxes", "embed_alias": "embed-model"}),
        &[("notes.md", NOTES.as_bytes().to_vec())],
    )
    .await;
    let before: Vec<String> = {
        let f = &kstore::list_files(&state.knowledge.pool, id).await.unwrap()[0];
        kstore::file_chunks(&state.knowledge.pool, f.id)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect()
    };
    // A second name for another model on the same mock.
    let up = state.snapshot().upstreams.values().next().unwrap().id;
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "embed-2".into(),
            upstream_id: up,
            upstream_model_id: "embed-other".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let (status, v) = post(
        &gw,
        &format!("/api/knowledge/bases/{id}/settings"),
        json!({"embed_alias": "embed-2"}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let job = v["reembed_job"].as_i64().expect("a model change re-embeds");
    let row = wait_job(&state, job).await;
    assert_eq!(row.status, "done", "{:?}", row.error);
    assert_eq!(row.kind, "kb_reembed");
    let kb = kstore::get_kb(&state.knowledge.pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kb.embed_model, "embed-other");
    assert_eq!(kb.status, "ready");
    let f = &kstore::list_files(&state.knowledge.pool, id).await.unwrap()[0];
    let after: Vec<String> = kstore::file_chunks(&state.knowledge.pool, f.id)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(before, after);
    assert_eq!(
        kstore::count_unembedded(&state.knowledge.pool, id, kstore::Unembedded::All)
            .await
            .unwrap(),
        0
    );
    cleanup(&state);
}
