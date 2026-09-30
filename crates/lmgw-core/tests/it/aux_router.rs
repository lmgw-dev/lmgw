//! The aux class: the rerank model kind, the zero-vector gate, and the
//! `embed` → `aux` settings rename (quickdoc design §9a).
//!
//! The preset-apply flow this file also used to cover went with router mode
//! (per-model-containers §7): there is no shared aux container to reload or
//! restart, and `tests/it/ops_container.rs` covers the per-model verbs that
//! replaced it.

use std::sync::{Arc, Mutex};

use lmgw_core::config::{AuxKind, Settings};
use lmgw_core::runtime::registry::Registry;
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewAuxModel};
use serde_json::{json, Value};

use crate::common;
use common::{serve, Gw};

// ---------------------------------------------------------------------------
// The zero-vector gate
// ---------------------------------------------------------------------------

/// llama-server answers `/v1/embeddings` against a reranker section with HTTP
/// 200 and an all-zero vector — reranking sets the embedding flag internally,
/// so nothing in the response says anything is wrong. Forwarding that would
/// fill a corpus with vectors that look valid and rank at random, so lmgw
/// refuses on the kind it recorded itself, before any upstream call.
#[tokio::test]
async fn embeddings_against_a_rerank_model_are_refused_not_forwarded() {
    let state = AppState::init_for_tests().await.unwrap();

    // No `upstreams` row is seeded: since per-model containers §5 an enabled
    // aux model is routable on the strength of its own row, onto the synthetic
    // aux upstream. That upstream's base URL is unconnectable until a hold
    // overwrites it, so if the kind gate below ever stopped firing the request
    // would fail loudly instead of quietly passing.
    for (model_id, kind) in [
        ("bge-m3", AuxKind::Embed),
        ("bge-reranker-v2-m3", AuxKind::Rerank),
    ] {
        store::insert_aux_model(
            &state.db,
            &NewAuxModel {
                model_id: model_id.into(),
                gguf_path: format!("{model_id}.gguf"),
                kind,
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
    }
    state.reload_snapshot().await.unwrap();

    let base = serve(state.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({"model": "embed/bge-reranker-v2-m3", "input": "hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "a reranker must not answer embeddings");
    let body: Value = resp.json().await.unwrap();
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("bge-reranker-v2-m3") && msg.contains("rerank"),
        "the refusal has to name the model and why: {body}"
    );

    // The failure is visible in the request log, not just to the caller.
    let logs = store::query_logs(
        &state.db,
        &store::LogFilter {
            alias: None,
            upstream_name: None,
            errors_only: true,
            limit: 10,
            before_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        logs.iter()
            .any(|r| r.requested_alias == "embed/bge-reranker-v2-m3" && r.status == 400),
        "the refused request should be logged: {logs:?}"
    );

    // The embedding model beside it is untouched by the gate — it gets as far
    // as the (dead) upstream, which is a transport error, not a 400.
    let status = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({"model": "embed/bge-m3", "input": "hello"}))
        .send()
        .await
        .unwrap()
        .status();
    assert_ne!(status, 400, "the gate must not catch embedding models");
}

// ---------------------------------------------------------------------------
// /v1/rerank — the mirror-image gate, and the Jina shape on the wire
// ---------------------------------------------------------------------------

/// A `podman` that agrees to everything and starts nothing. The container
/// these tests forward to is the wiremock upstream itself; all this has to do
/// is not shell the real binary.
struct FakePodman;

#[async_trait::async_trait]
impl lmgw_core::runtime::registry::CommandRunner for FakePodman {
    async fn run(
        &self,
        _program: &str,
        _args: &[String],
    ) -> std::io::Result<lmgw_core::runtime::registry::CmdOutput> {
        Ok(lmgw_core::runtime::registry::CmdOutput {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

/// Register both aux kinds and serve the gateway.
/// Returns `(state, gateway base url)`.
///
/// No `upstreams` row is created: since §5 an aux model resolves out of the
/// `aux_models` table under `settings.aux_router.public_prefix` onto the
/// synthetic aux upstream, so the two rows below are the entire wiring.
///
/// An aux route is a *local* route (per-model-containers §3.2), so reaching the
/// upstream means going through `acquire` first: the gateway starts the
/// model's container and forwards to the port it came up on (§5). The fake
/// podman below never starts anything, and the port allocator is pointed
/// straight at `upstream_base` — so the container "is" that upstream, and the
/// wire assertions below are about the same server they always were. A caller
/// that passes a dead port gets a failed start after `load_timeout_seconds`,
/// which is why that setting is seconds rather than the ten-minute default.
async fn aux_world(upstream_base: &str) -> (lmgw_core::state::SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let port: u16 = upstream_base
        .trim_end_matches("/v1")
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("aux_world wants an http://host:port/v1 upstream");
    state.set_runtime_for_tests(std::sync::Arc::new(Registry::with_ports(
        std::sync::Arc::new(FakePodman),
        reqwest::Client::new(),
        std::sync::Arc::new(move || Ok(port)),
    )));
    let mut s = Settings::default();
    s.vram.load_timeout_seconds = 2;
    // The start sequence refuses a class with no models dir before it renders
    // any argv; nothing is read from it here (the fake podman mounts nothing).
    s.aux_router.models_dir = std::env::temp_dir().display().to_string();
    store::save_settings(&state.db, &s).await.unwrap();
    for (model_id, kind) in [
        ("bge-m3", AuxKind::Embed),
        ("bge-reranker-v2-m3", AuxKind::Rerank),
    ] {
        store::insert_aux_model(
            &state.db,
            &NewAuxModel {
                model_id: model_id.into(),
                gguf_path: format!("{model_id}.gguf"),
                kind,
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
    }
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

/// The gate in the other direction (quickdoc §9a). An embedding section has no
/// cross-encoder head: llama-server answers a rerank request against one with
/// its pooling output, a plausible float that is not a relevance score. lmgw
/// refuses on the kind it recorded, before any upstream call — the upstream
/// here is a dead port, so a forwarded request would fail loudly instead.
#[tokio::test]
async fn rerank_against_an_embedding_model_is_refused_not_forwarded() {
    let (state, base) = aux_world("http://127.0.0.1:1/v1").await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/rerank"))
        .json(&json!({
            "model": "embed/bge-m3",
            "query": "what is a panda",
            "documents": ["a bear", "a fruit"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "an embedder must not answer rerank");
    let body: Value = resp.json().await.unwrap();
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("bge-m3") && msg.contains("/v1/embeddings"),
        "the refusal has to name the model and where it should have gone: {body}"
    );

    let logs = store::query_logs(
        &state.db,
        &store::LogFilter {
            alias: None,
            upstream_name: None,
            errors_only: true,
            limit: 10,
            before_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        logs.iter()
            .any(|r| r.requested_alias == "embed/bge-m3" && r.status == 400),
        "the refused request should be logged: {logs:?}"
    );

    // The reranker beside it is untouched by the gate — it gets as far as the
    // (dead) upstream, which is a transport error, not a 400.
    let status = reqwest::Client::new()
        .post(format!("{base}/v1/rerank"))
        .json(&json!({
            "model": "embed/bge-reranker-v2-m3", "query": "q", "documents": ["a"],
        }))
        .send()
        .await
        .unwrap()
        .status();
    assert_ne!(status, 400, "the gate must not catch rerank models");
}

/// The wire contract: lmgw sends llama.cpp's documented Jina shape (`query` +
/// `documents`, model rewritten to the section id) and answers in the same
/// shape. A TEI-spelled request (`texts`) is accepted on the way in, because a
/// client that speaks one of the two should not have to learn the other.
#[tokio::test]
async fn rerank_speaks_the_jina_shape_and_accepts_the_tei_spelling() {
    let upstream = wiremock::MockServer::start().await;
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let recorder = seen.clone();
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/rerank"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            recorder.lock().unwrap().push(body.clone());
            let n = body["documents"].as_array().map(Vec::len).unwrap_or(0);
            // Deliberately out of order and reversed: the caller must map by
            // `index`, not by position.
            let results: Vec<Value> = (0..n)
                .rev()
                .map(|i| json!({"index": i, "relevance_score": i as f64}))
                .collect();
            wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "model": "bge-reranker-v2-m3", "object": "list", "results": results,
                "usage": {"prompt_tokens": 7, "total_tokens": 7},
            }))
        })
        .mount(&upstream)
        .await;
    // The readiness probe the start sequence polls before it forwards (§3.6).
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/health"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
        .mount(&upstream)
        .await;

    let (state, base) = aux_world(&format!("{}/v1", upstream.uri())).await;
    let client = reqwest::Client::new();

    let body: Value = client
        .post(format!("{base}/v1/rerank"))
        .json(&json!({
            "model": "embed/bge-reranker-v2-m3",
            "query": "what is a panda",
            "documents": ["hi", "it is a bear"],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["results"].as_array().unwrap().len(), 2);
    assert_eq!(body["results"][0]["index"], 1, "order is the upstream's");
    assert_eq!(body["results"][0]["relevance_score"], 1.0);
    assert_eq!(body["usage"]["prompt_tokens"], 7);

    // The TEI spelling arrives as `documents` upstream — one shape leaves lmgw.
    client
        .post(format!("{base}/v1/rerank"))
        .json(&json!({
            "model": "embed/bge-reranker-v2-m3", "query": "q", "texts": ["a", "b", "c"],
        }))
        .send()
        .await
        .unwrap();

    let sent = seen.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    for s in sent.iter() {
        assert_eq!(
            s["model"], "bge-reranker-v2-m3",
            "the section id, not the public alias: {s}"
        );
        assert!(s.get("texts").is_none(), "TEI spelling must not be relayed");
        assert!(s.get("top_n").is_none(), "nothing asked for a truncation");
    }
    assert_eq!(sent[1]["documents"].as_array().unwrap().len(), 3);

    let logs = store::query_logs(
        &state.db,
        &store::LogFilter {
            alias: None,
            upstream_name: None,
            errors_only: false,
            limit: 10,
            before_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        logs.iter()
            .any(|r| r.requested_alias == "embed/bge-reranker-v2-m3" && r.status == 200),
        "a rerank call is logged like any other request: {logs:?}"
    );
}

// ---------------------------------------------------------------------------
// Config compatibility
// ---------------------------------------------------------------------------

/// A settings blob written before the rename (or restored from a backup of
/// one) has to keep its class definition rather than silently falling back to
/// the defaults — a wrong models dir is not something an owner would notice
/// until a model failed to load. The router-mode keys alongside it
/// (`container_name`, `listen_port`, `models_max`, `auto_start`) are simply
/// ignored: `Settings` has no `deny_unknown_fields`, which is what makes the
/// §6 shape change a no-op for deserialization.
#[test]
fn settings_written_as_embed_router_load_as_aux_router() {
    let legacy = json!({
        "bind_addr": "127.0.0.1:8001",
        "embed_router": {
            "image": "ghcr.io/ggml-org/llama.cpp:server-cuda",
            "container_name": "lmgw-llama-embed",
            "listen_port": 9393,
            "models_dir": "/srv/models/embed",
            "extra_run_args": [],
            "models_max": 0,
            "public_prefix": "embed",
            "auto_start": true
        }
    });
    let s: Settings = serde_json::from_value(legacy).unwrap();
    assert_eq!(s.aux_router.models_dir, "/srv/models/embed");
    assert_eq!(s.aux_router.public_prefix, "embed");

    // …and saving it back writes the new spelling only, without the four
    // router-mode keys.
    let round = serde_json::to_value(&s).unwrap();
    let aux = round.get("aux_router").unwrap();
    assert!(round.get("embed_router").is_none());
    for gone in ["container_name", "listen_port", "models_max", "auto_start"] {
        assert!(aux.get(gone).is_none(), "`{gone}` survived the save");
    }
}

/// The renderer refuses to emit `pooling` for a rerank section either way, but
/// dropping a value somebody typed is exactly the silent behavior that hides a
/// broken reranker. Both routes into the key are rejected where they are
/// entered instead.
#[tokio::test]
async fn a_rerank_model_cannot_be_saved_with_pooling() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    let client = base.client();

    let post = |body: Value| {
        let client = client.clone();
        let url = format!("{base}/api/op/aux_model_set");
        async move { client.post(url).json(&body).send().await.unwrap() }
    };

    for bad in [
        json!({"action": "create", "model_id": "r", "gguf_path": "r.gguf",
               "kind": "rerank", "pooling": "rank"}),
        json!({"action": "create", "model_id": "r", "gguf_path": "r.gguf",
               "kind": "rerank", "args": ["--pooling", "rank"]}),
    ] {
        let resp = post(bad.clone()).await;
        assert_eq!(resp.status(), 400, "{bad}");
        let body: Value = resp.json().await.unwrap();
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("pooling"),
            "the refusal has to name the field: {body}"
        );
    }

    // The same model without pooling saves, and an embedder may still set it.
    assert_eq!(
        post(
            json!({"action": "create", "model_id": "r", "gguf_path": "r.gguf",
                    "kind": "rerank"})
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        post(
            json!({"action": "create", "model_id": "e", "gguf_path": "e.gguf",
                    "kind": "embed", "pooling": "mean"})
        )
        .await
        .status(),
        200
    );
}

/// The zero-vector footgun from the config side. `--reranking` left in an
/// embedding model's extra args — a leftover from a model that used to be a
/// reranker, or a flag copied off a forum post — makes llama-server answer
/// `/v1/embeddings` with HTTP 200 and an all-zero vector, which fills a corpus
/// with noise that looks healthy until every search comes back wrong. The
/// argv renderer claims both kind flags so the escape hatch cannot emit
/// either, and the CRUD says so at the field, exactly as it does for
/// `pooling`.
#[tokio::test]
async fn an_aux_container_never_carries_the_other_kinds_flag() {
    use lmgw_core::config::{AuxModel, Snapshot};
    use lmgw_core::runtime::argv::render_llama_args;
    use lmgw_core::runtime::descriptor::model_runtime;
    use lmgw_core::runtime::Class;

    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    let client = base.client();
    let url = format!("{base}/api/op/aux_model_set");

    for (bad, named) in [
        (
            json!({"action": "create", "model_id": "e", "gguf_path": "e.gguf",
                   "kind": "embed", "args": ["--reranking"]}),
            "reranking",
        ),
        (
            json!({"action": "create", "model_id": "r", "gguf_path": "r.gguf",
                   "kind": "rerank", "args": ["--embedding", "true"]}),
            "embedding",
        ),
    ] {
        let resp = client.post(&url).json(&bad).send().await.unwrap();
        assert_eq!(resp.status(), 400, "{bad}");
        let body: Value = resp.json().await.unwrap();
        let msg = body["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains(named),
            "the refusal has to name the flag: {body}"
        );
    }

    // A row that predates the check — the migration defaulted every existing
    // model to `embed`, extra args and all — still cannot reach the container.
    let stale = AuxModel {
        id: 1,
        model_id: "e".into(),
        gguf_path: "e.gguf".into(),
        kind: AuxKind::Embed,
        pooling: None,
        ctx_size: None,
        args: vec!["--reranking".into()],
        idle_seconds: 0,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    };
    let snap = Snapshot {
        aux_models: vec![stale],
        ..Snapshot::default()
    };
    let rt = model_runtime(&snap, Class::Aux, "e").unwrap();
    let spec = rt
        .render_spec(
            "lmgw",
            9002,
            "/srv/aux-models",
            std::path::Path::new("/tmp"),
        )
        .unwrap();
    let argv = render_llama_args(&spec);
    assert!(argv.iter().any(|a| a == "--embeddings"), "{argv:?}");
    assert!(
        !argv.iter().any(|a| a.contains("reranking")),
        "the kind lmgw recorded decides, not the escape hatch: {argv:?}"
    );
}

/// The class survives a store round-trip, and existing rows (which the
/// migration defaults to `embed`) keep working.
#[tokio::test]
async fn aux_models_round_trip_their_kind() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::insert_aux_model(
        &state.db,
        &NewAuxModel {
            model_id: "bge-reranker-v2-m3".into(),
            gguf_path: "bge-reranker-v2-m3-Q8_0.gguf".into(),
            kind: AuxKind::Rerank,
            pooling: None,
            ctx_size: Some(8192),
            args: vec!["--ubatch-size".into(), "8192".into()],
            idle_seconds: 600,
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

    let got = store::get_aux_model(&state.db, id).await.unwrap().unwrap();
    assert_eq!(got.kind, AuxKind::Rerank);
    assert_eq!(got.ctx_size, Some(8192));

    // The snapshot carries them too — that is what the embeddings gate reads.
    let snap = state.reload_snapshot().await.unwrap();
    assert_eq!(snap.aux_models.len(), 1);
    assert_eq!(snap.aux_models[0].kind, AuxKind::Rerank);
}
