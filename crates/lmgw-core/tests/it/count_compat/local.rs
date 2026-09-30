//! The compatibility counters on a local llama.cpp row (api-docs design
//! §5.2, §5.3), on the `gpu_world` containers: what reaches the admission
//! gate, what a count starts, and what the running server is asked — the
//! review R1 fixes and the gaps it named.

use lmgw_core::config::{hash_api_key, HoldFallbackMode, LlamaParams};
use lmgw_core::store::{NewCandidateAlias, NewLocalModel};
use serde_json::{json, Value};

use super::{head, post, sdk_body, APPROX};
use crate::common::{self, Gw};
use crate::support::gpu_world::{Gpu, GIB};

/// A card with room for one small chat row, `chat-model`, and two container
/// ports — a start, and one restart.
async fn local() -> (Gpu, Gw) {
    let gpu = Gpu::new(8 * GIB, 2, 5).await;
    gpu.model("chat-model", GIB).await;
    let gw = common::serve(gpu.state.clone()).await;
    (gpu, gw)
}

/// A user turn of `parts` on `model`, in Anthropic's shape.
fn turn(model: &str, parts: Value) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": parts}]})
}

fn image() -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                       "data": "iVBORw0KGgo="}})
}

// ---------------------------------------------------------------------------
// Before admission (review R1 #2, #3)
// ---------------------------------------------------------------------------

/// A body only an Anthropic route could count — a server tool — is refused
/// before admission: a cold local row is not started just to answer 400.
#[tokio::test]
async fn an_unreadable_body_is_refused_before_a_local_row_starts() {
    let (gpu, gw) = local().await;
    let mut body = sdk_body("chat-model");
    body["tools"] = json!([{"type": "web_search_20250305", "name": "web_search"}]);

    let resp = post(&gw, "/v1/messages/count_tokens", body).await;
    assert_eq!(resp.status(), 400);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["type"], "error");
    assert_eq!(err["error"]["type"], "invalid_request_error", "{err}");
    assert!(
        gpu.runs().is_empty(),
        "nothing started for a body it cannot count"
    );
}

/// A candidate alias counts the way it sends: the model that counted is
/// named in `x-lmgw-candidate` on all three counters, and a facet the alias
/// does not enable is refused, as on `/v1/messages`, before anything starts.
#[tokio::test]
async fn a_candidate_alias_names_its_candidate_and_refuses_a_facet() {
    let (gpu, gw) = local().await;
    gpu.candidate(NewCandidateAlias {
        alias: "writer".into(),
        candidates: vec!["chat-model".into()],
        background: false,
        fallback_mode: HoldFallbackMode::None,
        fallback: None,
        capabilities_disabled: vec![],
        capabilities_enabled: vec![],
        enabled: true,
        notes: String::new(),
    })
    .await;
    gpu.world().prompt_tokens = 5;

    let resp = post(
        &gw,
        "/v1/messages/count_tokens",
        turn(
            "writer",
            json!([image(), {"type": "text", "text": "What is this?"}]),
        ),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["type"], "error");
    let message = err["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("'writer' does not enable Vision"),
        "{message}"
    );
    assert!(gpu.runs().is_empty(), "refused before anything started");

    for (route, body) in [
        ("/v1/messages/count_tokens", turn("writer", json!("hi"))),
        (
            "/v1/count_tokens",
            json!({"model": "writer", "input": "hi"}),
        ),
        ("/tokenize", json!({"model": "writer", "content": "hi"})),
    ] {
        let resp = post(&gw, route, body).await;
        assert_eq!(resp.status(), 200, "{route}");
        assert_eq!(
            head(&resp, "x-lmgw-candidate"),
            Some("chat-model"),
            "{route}"
        );
        assert_eq!(head(&resp, "x-lmgw-fallback"), None, "{route}");
    }
    assert_eq!(gpu.runs(), ["chat-model"], "one start, for all three");
}

// ---------------------------------------------------------------------------
// Media (review R1 #1; §5.2's local row)
// ---------------------------------------------------------------------------

/// A row that loads a projector with a per-image bound (its own
/// `--image-max-tokens`): each image is counted at that bound on top of the
/// rendered text, and says so — and the template is handed the images,
/// which a server with a projector renders.
#[tokio::test]
async fn an_image_on_a_projector_row_is_counted_at_its_bound() {
    let gpu = Gpu::new(8 * GIB, 2, 5).await;
    gpu.file("seeing-mmproj.gguf", 1);
    gpu.row(
        NewLocalModel {
            model_id: "seeing".into(),
            gguf_path: "seeing.gguf".into(),
            params: LlamaParams {
                mmproj_path: Some("seeing-mmproj.gguf".into()),
                ..Default::default()
            },
            args: vec!["--image-max-tokens".into(), "300".into()],
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
        GIB,
    )
    .await;
    gpu.world().prompt_tokens = 37;
    let gw = common::serve(gpu.state.clone()).await;

    let resp = post(
        &gw,
        "/v1/messages/count_tokens",
        turn(
            "seeing",
            json!([image(), image(), {"type": "text", "text": "Compare these."}]),
        ),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(head(&resp, APPROX), Some("media_bound"));
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({"input_tokens": 37 + 2 * 300})
    );
    let w = gpu.world();
    assert!(
        w.projectors.contains("seeing"),
        "started with its projector"
    );
    let parts = w.templated[0]["messages"][0]["content"].as_array().unwrap();
    assert_eq!(
        parts.iter().filter(|p| p["type"] == "image_url").count(),
        2,
        "the template rendered both images: {parts:?}"
    );
}

// ---------------------------------------------------------------------------
// The key's scope, and /tokenize on the running container
// ---------------------------------------------------------------------------

/// A key scoped away from a local model cannot cold-load it by counting on
/// it: each counter refuses before admission, and nothing starts.
#[tokio::test]
async fn a_refused_key_starts_no_container() {
    let (gpu, gw) = local().await;
    let mut settings = gpu.state.snapshot().settings.clone();
    settings.auth_enabled = true;
    lmgw_core::store::save_settings(&gpu.state.db, &settings)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, enabled, scope_mode, scope_patterns)
         VALUES ('fenced', ?1, 1, 'allow', 'other-model')",
    )
    .bind(hash_api_key("lmgw-fenced"))
    .execute(&gpu.state.db)
    .await
    .unwrap();
    gpu.state.reload_snapshot().await.unwrap();

    for (route, body) in [
        (
            "/v1/count_tokens",
            json!({"model": "chat-model", "input": "hi"}),
        ),
        ("/v1/messages/count_tokens", turn("chat-model", json!("hi"))),
        ("/tokenize", json!({"model": "chat-model", "content": "hi"})),
    ] {
        let resp = reqwest::Client::new()
            .post(format!("{gw}{route}"))
            .bearer_auth("lmgw-fenced")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "{route}");
    }
    assert!(gpu.runs().is_empty(), "no count cold-loaded the model");
}

/// `/tokenize` on a local row: the client's object reaches the running
/// container's own `/tokenize` with `model` rewritten, and a container that
/// died under it is restarted and asked again — `send_local`'s dead-container
/// policy, as for every other local send.
#[tokio::test]
async fn tokenize_on_a_local_row_follows_its_container() {
    let (gpu, gw) = local().await;
    gpu.world().prompt_tokens = 3;
    let body = json!({"model": "chat-model", "content": "Hello", "with_pieces": false});

    let resp = post(&gw, "/tokenize", body.clone()).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(head(&resp, APPROX), None);
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({"tokens": [1, 1, 1]})
    );
    let port = {
        let w = gpu.world();
        assert_eq!(
            w.tokenized,
            vec![json!({"model": "chat-model", "content": "Hello", "with_pieces": false})],
            "forwarded as sent, model rewritten to the row's own"
        );
        w.ports
            .iter()
            .find_map(|(port, model)| (model == "chat-model").then_some(*port))
            .unwrap()
    };

    gpu.kill(port).await;
    let resp = post(&gw, "/tokenize", body).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(gpu.runs(), ["chat-model", "chat-model"], "restarted once");
    assert_eq!(gpu.world().tokenized.len(), 2, "and asked again");
}
