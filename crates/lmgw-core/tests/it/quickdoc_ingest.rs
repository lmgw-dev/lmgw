//! quickdoc ingestion end to end (quickdoc design §8), with no network and no
//! container: the documentation host, the ingest model and the embedding model
//! are all one wiremock upstream.
//!
//! The run this exercises is the whole contract in one pass — fetch, sniff,
//! window, extract, **validate**, chunk, embed, query — and the scripted model
//! deliberately corrupts one code sample on its first turn, exactly the way
//! context7's pipeline was observed to. That span must be rejected, told to the
//! model, corrected on the next turn, and never appear in the corpus.

use std::sync::Arc;
use std::time::Duration;

use lmgw_core::config::{AuxKind, Protocol, UpstreamKind, AUX_UPSTREAM_NAME};
use lmgw_core::jobs::{self, JobKind};
use lmgw_core::quickdoc::{ingest, reembed, InProcessEmbedder};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewAuxModel, NewLocalModel, NewUpstream};
use quickdoc_core::embed::{EmbedIdentity, Embedder, FixtureEmbedder};
use quickdoc_core::ingest::prompt;
use quickdoc_core::retrieve::{Retriever, SearchParams};
use quickdoc_core::store::{self as qstore, NewCorpus};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// The page under ingestion. Line numbers matter — the scripted model quotes
/// them — so they are spelled out next to the text.
///
/// ```text
///  1  # axum routing
///  2
///  3  Handlers are async functions ...
///  4
///  5  ```rust
///  6  let app = Router::new().route("/", get(root));
///  7  ```
///  8
///  9  ## Extractors
/// 10
/// 11  An extractor pulls data out of the request.
/// ```
const DOC: &str = r#"# axum routing

Handlers are async functions that return something implementing IntoResponse.

```rust
let app = Router::new().route("/", get(root));
```

## Extractors

An extractor pulls data out of the request.
"#;

const DIMS: usize = 32;

// ---------------------------------------------------------------------------
// The mock gateway-side world
// ---------------------------------------------------------------------------

/// `/v1/embeddings` answered with quickdoc's own deterministic fixture vectors,
/// so the KNN stage is exercised for real without a model anywhere.
struct FixtureEmbeddings;

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

fn emit_call(id: &str, sections: Value) -> Value {
    json!({
        "id": "chatcmpl-1", "object": "chat.completion", "model": "ingest-tgt",
        "choices": [{"index": 0, "message": {
            "role": "assistant", "content": null,
            "tool_calls": [{"id": id, "type": "function", "function": {
                "name": "emit_extraction",
                "arguments": json!({"sections": sections}).to_string(),
            }}],
        }, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 40},
    })
}

fn text_reply(text: &str) -> Value {
    json!({
        "id": "chatcmpl-2", "object": "chat.completion", "model": "ingest-tgt",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 120, "completion_tokens": 8},
    })
}

fn section(start: usize, end: usize, first: &str, last: &str, title: &str) -> Value {
    json!({
        "start_line": start, "end_line": end,
        "first_line": first, "last_line": last,
        "heading_path": title, "derived_title": title,
        "derived_summary": format!("A section about {title}."),
        "boilerplate": false,
    })
}

/// The three turns of one extraction run, in order.
async fn mount_model(mock: &MockServer) {
    let turns = vec![
        // Turn 1: one good section, and one whose quoted code has been
        // *rewritten* — `Router::new({` for `Router::new().route(...)`.
        emit_call(
            "c1",
            json!([
                section(
                    9,
                    11,
                    "## Extractors",
                    "An extractor pulls data out of the request.",
                    "Extractors"
                ),
                section(5, 6, "```rust", "let app = Router::new({", "Routing"),
            ]),
        ),
        // Turn 2: the corrected span, after being told what the line really says.
        emit_call(
            "c2",
            json!([section(1, 7, "# axum routing", "```", "Routing")]),
        ),
        text_reply("Extracted two sections."),
    ];
    for (i, reply) in turns.into_iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .up_to_n_times(1)
            .with_priority((i + 1) as u8)
            .expect(..=1)
            .mount(mock)
            .await;
    }
}

async fn mount_world(mock: &MockServer) {
    // The documentation host.
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("User-agent: *\nDisallow: /private\n"),
        )
        .mount(mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/docs/axum.md"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/markdown")
                .set_body_string(DOC),
        )
        .mount(mock)
        .await;
    // The upstream catalog — where the ingest model's real context length comes
    // from, and therefore where the extraction window is sized from.
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [
                {"id": "ingest-tgt", "n_ctx": 32768},
                {"id": "embed-tgt", "n_ctx": 8192},
            ],
        })))
        .mount(mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(FixtureEmbeddings)
        .mount(mock)
        .await;
}

async fn setup(mock: &MockServer) -> SharedState {
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
    for (alias, target) in [("ingest-model", "ingest-tgt"), ("embed-model", "embed-tgt")] {
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
    let mut settings = state.snapshot().settings.clone();
    // Politeness is a visible setting; a mock host needs none of it.
    settings.docs_fetch_delay_ms = 0;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    state
}

async fn seed_corpus(state: &SharedState, mock: &MockServer, embed: EmbedIdentity) -> i64 {
    let id = qstore::insert_corpus(
        &state.corpus,
        &NewCorpus {
            library: "axum".into(),
            version: "0.8".into(),
            status: "ingesting".into(),
            embed,
            ingest_model: "ingest-model".into(),
            ingest_prompt_version: prompt::CURRENT.into(),
            crawl_date: String::new(),
            source_kind: "markdown".into(),
        },
    )
    .await
    .unwrap();
    qstore::insert_source(
        &state.corpus,
        id,
        &format!("{}/docs/axum.md", mock.uri()),
        "markdown",
        &[],
    )
    .await
    .unwrap();
    id
}

async fn wait_status(state: &SharedState, id: i64, want: &str) -> store::JobRow {
    for _ in 0..600 {
        let row = store::get_job(&state.db, id).await.unwrap().unwrap();
        if row.status == want {
            return row;
        }
        if matches!(row.status.as_str(), "done" | "failed" | "canceled") {
            panic!("job {id} ended as '{}': {:?}", row.status, row.error);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {id} never reached '{want}'");
}

// ---------------------------------------------------------------------------
// The end-to-end run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_extract_validate_embed_and_then_find_it() {
    let mock = MockServer::start().await;
    mount_world(&mock).await;
    mount_model(&mock).await;
    let state = setup(&mock).await;

    // The pin is measured, not declared: probing the alias resolves it to a
    // concrete upstream + model and learns the width from a real vector.
    let probe = InProcessEmbedder::probe(state.clone(), "embed-model")
        .await
        .unwrap();
    let identity = probe.identity();
    assert_eq!(identity, EmbedIdentity::new("test-up", "embed-tgt", DIMS));

    // An agent asked for this library before it existed (§7). Finishing the
    // ingest is what closes that request — nobody has to remember to.
    qstore::file_doc_request(
        &state.corpus,
        "axum",
        "",
        Some("no corpus"),
        Some("claude-code"),
    )
    .await
    .unwrap();

    let corpus_id = seed_corpus(&state, &mock, identity.clone()).await;
    let corpus = qstore::get_corpus(&state.corpus, corpus_id)
        .await
        .unwrap()
        .unwrap();
    let job = ingest::start(&state, &corpus).await.unwrap().id();
    let row = wait_status(&state, job, "done").await;

    // ---- the verbatim contract ----
    let result: Value = serde_json::from_str(row.result.as_deref().unwrap()).unwrap();
    assert_eq!(result["documents"], 1);
    assert_eq!(result["extracted"], 1);
    assert_eq!(result["chunks"], 2);
    assert_eq!(
        result["rejected_spans"], 1,
        "the rewritten code sample must have been rejected"
    );

    // Typed progress, flushed past the publish throttle: documents as the
    // universal done/total, the ingest's own counters in `detail`.
    let progress: Value = serde_json::from_str(&row.progress).unwrap();
    assert_eq!(progress["done"], 1);
    assert_eq!(progress["total"], 1);
    assert_eq!(progress["detail"]["corpus"], "axum@0.8");
    assert_eq!(progress["detail"]["fetched"], 1);
    assert_eq!(progress["detail"]["extracted"], 1);
    assert_eq!(progress["detail"]["embedded"], 2);
    assert_eq!(progress["detail"]["rejected_spans"], 1);

    let corpus = qstore::get_corpus(&state.corpus, corpus_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(corpus.status, "ready");
    assert_eq!(corpus.chunk_count, 2);
    assert!(!corpus.crawl_date.is_empty());

    // Auto-fulfilment (§10): the version-less request for `axum` is answered by
    // `axum@0.8` finishing, and leaves the owner's queue empty.
    assert!(
        qstore::list_doc_requests(&state.corpus, "pending")
            .await
            .unwrap()
            .is_empty(),
        "a finished ingest must close the requests it answers"
    );
    let fulfilled = qstore::list_doc_requests(&state.corpus, "fulfilled")
        .await
        .unwrap();
    assert_eq!(fulfilled.len(), 1);
    assert_eq!(fulfilled[0].client_name.as_deref(), Some("claude-code"));

    let documents = qstore::list_documents(&state.corpus, corpus_id)
        .await
        .unwrap();
    assert_eq!(documents.len(), 1);
    let chunks = qstore::list_chunks(&state.corpus, documents[0].id)
        .await
        .unwrap();
    assert_eq!(chunks.len(), 2);
    for c in &chunks {
        let span = (c.span_start as usize, c.span_end as usize);
        assert!(
            quickdoc_core::ingest::is_verbatim(DOC, span, &c.payload),
            "payload of {} is not the source text at {span:?}",
            c.id
        );
        assert!(
            !c.payload.contains("Router::new({"),
            "the rewritten sample reached the corpus: {}",
            c.payload
        );
        // Derived text is stored beside the payload, never inside it.
        assert!(!c.derived_title.is_empty());
        assert!(!c.payload.contains(&c.derived_summary));
    }
    assert!(
        chunks.iter().any(|c| c
            .payload
            .contains(r#"let app = Router::new().route("/", get(root));"#)),
        "the corrected span carries the real code"
    );

    // ---- and the corpus answers questions ----
    let embedder: Arc<dyn Embedder> = InProcessEmbedder::for_corpus(state.clone(), &corpus)
        .await
        .unwrap()
        .into_arc();
    let retriever = Retriever::load(state.corpus.clone(), corpus_id, embedder)
        .await
        .unwrap();
    let found = retriever
        .search(
            "extractor pulls data out of the request",
            &SearchParams::default(),
        )
        .await
        .unwrap();
    assert!(
        found.hits[0]
            .chunk
            .payload
            .contains("An extractor pulls data"),
        "top hit was {:?}",
        found.hits[0].chunk.payload
    );
    assert!(!found.trace.knn.is_empty(), "the KNN stage really ran");

    let found = retriever
        .search("router route handler", &SearchParams::default())
        .await
        .unwrap();
    assert!(found
        .hits
        .iter()
        .any(|h| h.chunk.payload.contains("Router::new()")));

    // ---- re-ingest: the hash gate keeps the model out of it ----
    // Every scripted turn is spent; a second extraction call would 404 and fail
    // the job, so this passing *is* the assertion that the model never ran.
    let again = ingest::start(&state, &corpus).await.unwrap().id();
    let row = wait_status(&state, again, "done").await;
    let result: Value = serde_json::from_str(row.result.as_deref().unwrap()).unwrap();
    assert_eq!(result["unchanged"], 1);
    assert_eq!(result["extracted"], 0);
    assert_eq!(result["chunks"], 2, "the chunks are still there");
}

/// A re-ingest must not launder away a `re_embed_required` badge that is still
/// true.
///
/// The trap is the interaction of two features built in different steps: a
/// re-embed that dies half way leaves chunks with `embedding IS NULL`, and the
/// `content_hash` gate then skips every one of those documents on the next
/// ingest without ever embedding them. Flipping to `ready` on chunk count alone
/// would report a healthy corpus whose KNN half covers a fraction of it — and
/// `docs__resolve` would tell the asking agent the same lie.
#[tokio::test]
async fn a_re_ingest_does_not_clear_a_re_embed_badge_it_did_not_fix() {
    let mock = MockServer::start().await;
    mount_world(&mock).await;
    mount_model(&mock).await;
    let state = setup(&mock).await;

    let identity = InProcessEmbedder::probe(state.clone(), "embed-model")
        .await
        .unwrap()
        .identity();
    qstore::file_doc_request(&state.corpus, "axum", "", None, Some("claude-code"))
        .await
        .unwrap();
    let corpus_id = seed_corpus(&state, &mock, identity).await;
    let corpus = qstore::get_corpus(&state.corpus, corpus_id)
        .await
        .unwrap()
        .unwrap();
    let job = ingest::start(&state, &corpus).await.unwrap().id();
    wait_status(&state, job, "done").await;
    assert_eq!(
        qstore::get_corpus(&state.corpus, corpus_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "ready"
    );

    // A re-embed that was cancelled or failed after dropping the old vectors.
    assert_eq!(
        qstore::clear_corpus_embeddings(&state.corpus, corpus_id)
            .await
            .unwrap(),
        2
    );
    qstore::set_corpus_status(&state.corpus, corpus_id, "re_embed_required")
        .await
        .unwrap();

    // Re-ingest. Every scripted model turn is already spent, so the job
    // completing at all proves the hash gate kept the model out of it — and
    // therefore that nothing re-embedded anything.
    let again = ingest::start(&state, &corpus).await.unwrap().id();
    let row = wait_status(&state, again, "done").await;
    let result: Value = serde_json::from_str(row.result.as_deref().unwrap()).unwrap();
    assert_eq!(result["unchanged"], 1);
    assert_eq!(result["chunks"], 2);

    assert_eq!(
        qstore::count_unembedded(&state.corpus, corpus_id)
            .await
            .unwrap(),
        2,
        "the re-ingest really did leave the vectors missing"
    );
    let corpus = qstore::get_corpus(&state.corpus, corpus_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        corpus.status, "re_embed_required",
        "an ingest that embedded nothing must not call the corpus ready"
    );

    // The request is still closed: fulfilment follows "there is a corpus now",
    // and the badge is what tells the agent what kind of corpus it got.
    assert!(qstore::list_doc_requests(&state.corpus, "pending")
        .await
        .unwrap()
        .is_empty());
}

/// The `re_embed` kind rides the same embedder. Moving a corpus onto another
/// model re-pins it first and drops every old vector — half a corpus in each
/// space is worse than none.
#[tokio::test]
async fn re_embed_repins_the_corpus_and_refills_every_vector() {
    let mock = MockServer::start().await;
    mount_world(&mock).await;
    mount_model(&mock).await;
    let state = setup(&mock).await;

    // A second alias onto the same model: a different *name*, the same resolved
    // identity, so re-embedding through it must be recognised as a no-op re-pin.
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "embed-other".into(),
            upstream_id: state.snapshot().aliases["embed-model"].upstream_id,
            upstream_model_id: "embed-tgt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let identity = InProcessEmbedder::probe(state.clone(), "embed-model")
        .await
        .unwrap()
        .identity();
    let corpus_id = seed_corpus(&state, &mock, identity).await;
    let corpus = qstore::get_corpus(&state.corpus, corpus_id)
        .await
        .unwrap()
        .unwrap();
    let job = ingest::start(&state, &corpus).await.unwrap().id();
    wait_status(&state, job, "done").await;
    assert_eq!(
        qstore::count_unembedded(&state.corpus, corpus_id)
            .await
            .unwrap(),
        0
    );

    let corpus = qstore::get_corpus(&state.corpus, corpus_id)
        .await
        .unwrap()
        .unwrap();
    let documents = qstore::list_documents(&state.corpus, corpus_id)
        .await
        .unwrap();
    let before: Vec<String> = qstore::list_chunks(&state.corpus, documents[0].id)
        .await
        .unwrap()
        .iter()
        .map(|c| c.id.clone())
        .collect();

    let job = reembed::start(&state, &corpus, Some("embed-other".into()))
        .await
        .unwrap()
        .id();
    let row = wait_status(&state, job, "done").await;
    let result: Value = serde_json::from_str(row.result.as_deref().unwrap()).unwrap();
    assert_eq!(
        result["repinned"], false,
        "the same resolved model is not a re-pin, whatever it is called"
    );
    // Nothing was unembedded, so nothing needed re-embedding.
    assert_eq!(result["embedded"], 0);
    assert_eq!(
        qstore::get_corpus(&state.corpus, corpus_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "ready"
    );
    // Payloads are never touched, so chunk ids — and with them citations and
    // golden queries — survive a model change.
    let after: Vec<String> = qstore::list_chunks(&state.corpus, documents[0].id)
        .await
        .unwrap()
        .iter()
        .map(|c| c.id.clone())
        .collect();
    assert_eq!(before, after);
}

/// The other half of the same job: a re-embed onto a genuinely different model.
/// The pin moves *before* the vectors are dropped, so a query landing mid-run
/// sees a corpus honestly empty for its declared model rather than one full of
/// vectors from a space it no longer claims — and every chunk is refilled.
#[tokio::test]
async fn re_embed_onto_another_model_moves_the_pin_and_refills_everything() {
    let mock = MockServer::start().await;
    mount_world(&mock).await;
    mount_model(&mock).await;
    let state = setup(&mock).await;

    // A second alias onto a *different* upstream model: a different resolved
    // identity, which is what makes this a re-pin rather than a no-op.
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "embed-two".into(),
            upstream_id: state.snapshot().aliases["embed-model"].upstream_id,
            upstream_model_id: "embed-tgt-2".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let identity = InProcessEmbedder::probe(state.clone(), "embed-model")
        .await
        .unwrap()
        .identity();
    let corpus_id = seed_corpus(&state, &mock, identity).await;
    let corpus = qstore::get_corpus(&state.corpus, corpus_id)
        .await
        .unwrap()
        .unwrap();
    let job = ingest::start(&state, &corpus).await.unwrap().id();
    wait_status(&state, job, "done").await;

    let documents = qstore::list_documents(&state.corpus, corpus_id)
        .await
        .unwrap();
    let before: Vec<String> = qstore::list_chunks(&state.corpus, documents[0].id)
        .await
        .unwrap()
        .iter()
        .map(|c| c.id.clone())
        .collect();
    assert!(!before.is_empty());

    let corpus = qstore::get_corpus(&state.corpus, corpus_id)
        .await
        .unwrap()
        .unwrap();
    let job = reembed::start(&state, &corpus, Some("embed-two".into()))
        .await
        .unwrap()
        .id();
    let row = wait_status(&state, job, "done").await;
    let result: Value = serde_json::from_str(row.result.as_deref().unwrap()).unwrap();
    assert_eq!(result["repinned"], true, "{result}");
    assert_eq!(
        result["embedded"].as_u64().unwrap(),
        before.len() as u64,
        "every chunk was re-embedded, not just the ones that had no vector"
    );

    let corpus = qstore::get_corpus(&state.corpus, corpus_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        corpus.embed_identity().model,
        "embed-tgt-2",
        "the corpus is pinned to what it was actually embedded with"
    );
    assert_eq!(corpus.status, "ready");
    assert_eq!(
        qstore::count_unembedded(&state.corpus, corpus_id)
            .await
            .unwrap(),
        0,
        "no chunk is left in the old space"
    );
    // Payloads are never touched, so citations and golden queries survive.
    let after: Vec<String> = qstore::list_chunks(&state.corpus, documents[0].id)
        .await
        .unwrap()
        .iter()
        .map(|c| c.id.clone())
        .collect();
    assert_eq!(before, after);

    // And the new pin is the one a later plain re-embed binds to — the alias
    // that was typed is not what a corpus remembers.
    let job = reembed::start(&state, &corpus, None).await.unwrap().id();
    let row = wait_status(&state, job, "done").await;
    let result: Value = serde_json::from_str(row.result.as_deref().unwrap()).unwrap();
    assert!(
        result["embed_model"]
            .as_str()
            .unwrap()
            .contains("embed-tgt-2"),
        "{result}"
    );
}

/// §9a's live-verified footgun on the ingestion path: llama-server answers
/// `/v1/embeddings` against a reranker section with 200 and an all-zero vector.
/// The in-process embedder refuses the model by kind, before any call.
#[tokio::test]
async fn a_reranker_can_never_become_a_corpus_embedder() {
    let state = AppState::init_for_tests().await.unwrap();
    store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: AUX_UPSTREAM_NAME.into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::LlamaServer,
            // Nothing listens here: if the gate stopped firing, this would fail
            // with a transport error instead of quietly passing.
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 2_000,
            enabled: true,
            expose_all: true,
            expose_prefix: "embed".into(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_aux_model(
        &state.db,
        &NewAuxModel {
            model_id: "bge-reranker-v2-m3".into(),
            gguf_path: "r.gguf".into(),
            kind: AuxKind::Rerank,
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

    let err = InProcessEmbedder::probe(state.clone(), "embed/bge-reranker-v2-m3")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("rerank"), "{err}");
    assert!(err.contains("all-zero vector"), "{err}");

    // And a corpus that somehow pinned one refuses to open on the same grounds.
    let corpus_id = qstore::insert_corpus(
        &state.corpus,
        &NewCorpus::new(
            "axum",
            "0.8",
            EmbedIdentity::new(AUX_UPSTREAM_NAME, "bge-reranker-v2-m3", 1024),
        ),
    )
    .await
    .unwrap();
    let corpus = qstore::get_corpus(&state.corpus, corpus_id)
        .await
        .unwrap()
        .unwrap();
    let err = InProcessEmbedder::for_corpus(state.clone(), &corpus)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("rerank"), "{err}");
}

/// A vector of the wrong width is a hard, named error — never truncated, never
/// padded. A corpus whose vectors are the wrong shape is unsearchable, and the
/// only honest answer is to refuse before anything is written.
#[tokio::test]
async fn a_wrong_width_vector_stops_the_job_instead_of_being_reshaped() {
    let mock = MockServer::start().await;
    mount_world(&mock).await;
    mount_model(&mock).await;
    let state = setup(&mock).await;

    // Pinned to a width the (fixture) embedder does not produce.
    let corpus_id = seed_corpus(
        &state,
        &mock,
        EmbedIdentity::new("test-up", "embed-tgt", DIMS + 1),
    )
    .await;
    let corpus = qstore::get_corpus(&state.corpus, corpus_id)
        .await
        .unwrap()
        .unwrap();
    let job = ingest::start(&state, &corpus).await.unwrap().id();

    let mut row = store::get_job(&state.db, job).await.unwrap().unwrap();
    for _ in 0..600 {
        row = store::get_job(&state.db, job).await.unwrap().unwrap();
        if row.status == "done" || row.status == "failed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The document itself fails, so the run ends with nothing ingested and says
    // so rather than reporting a healthy corpus.
    assert_eq!(row.status, "failed", "{row:?}");
    let err = row.error.unwrap_or_default();
    assert!(err.contains("nothing was ingested"), "{err}");
    assert_eq!(
        qstore::get_corpus(&state.corpus, corpus_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "failed"
    );
    assert_eq!(
        qstore::count_unembedded(&state.corpus, corpus_id)
            .await
            .unwrap(),
        0,
        "no half-embedded rows were left behind"
    );

    // The document was marked stale, so a retry re-runs it instead of being
    // skipped forever by the content-hash gate.
    let documents = qstore::list_documents(&state.corpus, corpus_id)
        .await
        .unwrap();
    assert_eq!(documents[0].content_hash, "");
}

/// Every quickdoc job kind has an executor now that `golden_gen`'s has landed.
#[tokio::test]
async fn the_quickdoc_kinds_are_all_registered() {
    let state = AppState::init_for_tests().await.unwrap();
    let kinds = state.jobs.registered_kinds();
    assert!(kinds.contains(&JobKind::Ingest));
    assert!(kinds.contains(&JobKind::ReEmbed));
    assert!(kinds.contains(&JobKind::EvalRun));
    assert!(kinds.contains(&JobKind::GoldenGen));

    // An ingest of a corpus that does not exist fails with that, not with
    // "no executor".
    let id = jobs::spawn(
        &state,
        JobKind::Ingest,
        None,
        "ghost".into(),
        json!({"corpus_id": 999}),
    )
    .await
    .unwrap()
    .id();
    for _ in 0..200 {
        let row = store::get_job(&state.db, id).await.unwrap().unwrap();
        if row.status == "failed" {
            assert!(row.error.unwrap_or_default().contains("no longer exists"));
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the ingest job never finished");
}

// ---------------------------------------------------------------------------
// GPU hold (gpu-hold design §2/§7.13): unattended batch jobs are refused,
// never re-routed.
// ---------------------------------------------------------------------------

/// `ExtractionPlan::build` keeps plain `resolve` and checks the hold itself,
/// before anything else runs — before the token counter that *would* fall
/// back (`count_tokens_inner`), before the document fetch, before the cloud
/// mock is ever touched. A held ingest against a genuinely local model
/// therefore fails on the first tick with the named `GpuHold` error, and a
/// global fallback being configured makes no difference at all: nobody is
/// waiting on an ingest, and it can be re-run once the hold is released.
#[tokio::test]
async fn ingest_is_refused_under_hold_and_never_reaches_the_cloud_mock() {
    let mock = MockServer::start().await;
    let state = setup(&mock).await;

    // A genuinely local ingest model: its route classifies as local, which is
    // what makes the hold check in `ExtractionPlan::build` actually trip —
    // `setup`'s own "ingest-model" is a cloud-shaped alias and would sail
    // through untouched.
    store::insert_local_model(
        &state.db,
        &NewLocalModel {
            model_id: "local-ingest".into(),
            gguf_path: "local-ingest.gguf".into(),
            params: Default::default(),
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            ladder: vec![],
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    // Set on purpose: the point is that a batch job ignores an available
    // fallback entirely, not merely that none exists to try.
    let mut s = state.snapshot().settings.clone();
    s.hold.fallback_alias = Some("ingest-model".into());
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    lmgw_core::ops::hold_set(&state, true).await.unwrap();

    let corpus_id = qstore::insert_corpus(
        &state.corpus,
        &NewCorpus {
            library: "axum".into(),
            version: "0.8".into(),
            status: "ingesting".into(),
            embed: EmbedIdentity::new("test-up", "embed-tgt", DIMS),
            ingest_model: "local-ingest".into(),
            ingest_prompt_version: prompt::CURRENT.into(),
            crawl_date: String::new(),
            source_kind: "markdown".into(),
        },
    )
    .await
    .unwrap();
    qstore::insert_source(
        &state.corpus,
        corpus_id,
        &format!("{}/docs/axum.md", mock.uri()),
        "markdown",
        &[],
    )
    .await
    .unwrap();
    let corpus = qstore::get_corpus(&state.corpus, corpus_id)
        .await
        .unwrap()
        .unwrap();

    let job = ingest::start(&state, &corpus).await.unwrap().id();
    let row = wait_status(&state, job, "failed").await;

    let err = row.error.unwrap_or_default();
    assert!(
        err.contains("holding the GPU") || err.contains("gpu_hold"),
        "the job's own error must name the hold: {err}"
    );
    assert!(
        mock.received_requests().await.unwrap().is_empty(),
        "not one byte of this run may reach the cloud mock — no fetch, no token count, \
         no fallback"
    );
}
