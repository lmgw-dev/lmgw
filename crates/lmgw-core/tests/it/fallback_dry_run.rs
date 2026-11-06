//! The referrer dry run's view of an upstream a write changes, and an
//! upstream's prefix rewrite (openai egress design §14.2):
//!
//! - the dry run reads the catalog of the upstream as the write would leave
//!   it, fresh, and never stores it: the cache holds the live
//!   configuration's under the same id (review R2). A retarget that changes
//!   a referrer's type is refused whether the cache is warm or cold, and the
//!   refused configuration's catalog is not left behind;
//! - a bare `expose_all` upstream answers any name nothing else does, so its
//!   new prefix rewrites only the fallbacks naming a model its catalog
//!   lists, never a dangling name it merely answers (review R3).

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::ops;
use lmgw_core::store::{self, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::fallback_world::{alias, world, World};
use crate::support::fallbacks::set_fallback;

fn patch<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).unwrap()
}

/// A catalog listing `models`, each with its output modalities.
async fn catalog(models: &[(&str, &str)]) -> MockServer {
    let server = MockServer::start().await;
    let data: Vec<Value> = models
        .iter()
        .map(|(id, out)| json!({"id": id, "architecture": {"output_modalities": [out]}}))
        .collect();
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": data})))
        .mount(&server)
        .await;
    server
}

async fn upstream(w: &World, name: &str, base: &str, expose_all: bool) -> i64 {
    let id = store::insert_upstream(
        &w.state.db,
        &NewUpstream {
            name: name.into(),
            protocol: Protocol::OpenaiCompatible,
            kind: UpstreamKind::Generic,
            base_url: format!("{base}/v1"),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    id
}

/// What the cache holds for upstream `id`'s model `model`: its task, read
/// without fetching anything.
async fn cached_task(w: &World, id: i64, model: &str) -> Option<String> {
    let u = w.state.snapshot().upstreams.get(&id).cloned().unwrap();
    lmgw_core::catalog::peek_only(lmgw_core::catalog::upstream_models(&w.state, &u))
        .await
        .ok()?
        .into_iter()
        .find(|m| m.id == model)
        .and_then(|m| m.task)
}

/// `qwen3` (chat) falls back to `ext-m`, an alias whose type its catalog
/// gives. The upstream moved to an address whose catalog lists that model
/// as an embedder: refused, the cache warm or cold, and what the cache
/// holds afterwards is the live address's catalog.
#[tokio::test]
async fn a_move_that_changes_a_referrers_type_is_judged_on_the_new_catalog() {
    let w = world().await;
    let chat = catalog(&[("ext-m-model", "text")]).await;
    let embed = catalog(&[("ext-m-model", "embedding")]).await;
    let ext = upstream(&w, "ext", &chat.uri(), false).await;
    alias(&w.state, "ext-m", ext, None).await;
    w.state.reload_snapshot().await.unwrap();
    set_fallback(&w.state, "local_models", "qwen3", Some("ext-m")).await;
    let moved =
        || patch(json!({"action": "update", "id": ext, "base_url": format!("{}/v1", embed.uri())}));

    for warm in [true, false] {
        w.state.catalog.invalidate(ext).await;
        if warm {
            let u = w.state.snapshot().upstreams.get(&ext).cloned().unwrap();
            lmgw_core::catalog::upstream_models(&w.state, &u)
                .await
                .unwrap();
        }
        let e = ops::upstream_set(&w.state, moved(), ops::RowWriter::Dashboard)
            .await
            .unwrap_err();
        assert!(
            e.contains(
                "chat model 'qwen3' falls back to 'ext-m', which would be an embedding model"
            ),
            "warm {warm}: {e}"
        );
        assert_eq!(
            cached_task(&w, ext, "ext-m-model").await.as_deref(),
            Some("chat"),
            "warm {warm}: the live address's catalog"
        );
    }
    assert!(
        w.state.snapshot().upstreams[&ext]
            .base_url
            .starts_with(&chat.uri()),
        "not moved"
    );
}

/// A bare `expose_all` upstream gets a prefix: the fallback naming a model
/// its catalog lists follows it; one naming a model it does not list — a
/// dangling name the upstream only answered because it answers any — is
/// left as it is.
#[tokio::test]
async fn a_bare_upstreams_prefix_rewrites_only_the_models_it_lists() {
    let w = world().await;
    let listing = catalog(&[("real-model", "text")]).await;
    let open = upstream(&w, "open", &listing.uri(), true).await;
    set_fallback(&w.state, "local_models", "qwen3", Some("real-model")).await;
    set_fallback(&w.state, "local_models", "other", Some("ghost-model")).await;

    let saved = ops::upstream_set(
        &w.state,
        patch(json!({"action": "update", "id": open, "expose_prefix": "oc"})),
        ops::RowWriter::Dashboard,
    )
    .await
    .unwrap();
    assert_eq!(
        w.fallback("local_models", "qwen3").await.as_deref(),
        Some("oc/real-model")
    );
    assert_eq!(
        w.fallback("local_models", "other").await.as_deref(),
        Some("ghost-model"),
        "a name it does not list is not its model"
    );
    assert!(
        saved
            .fallback_notes
            .iter()
            .any(|n| n.contains("now names 'oc/real-model'")),
        "{:?}",
        saved.fallback_notes
    );
    assert!(
        !saved
            .fallback_notes
            .iter()
            .any(|n| n.contains("ghost-model")),
        "{:?}",
        saved.fallback_notes
    );
}
