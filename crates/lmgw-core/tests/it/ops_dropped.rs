//! The `ops/` writes an owner hangs up on still finish (*2026-10-24*): each
//! runs from its store write on as a task of its own, so what follows the
//! reload — a flushed catalog cache, a removed container, a moved device
//! listener — happens though hyper dropped the handler
//! (`support::dropped_op`). Every other `ops/` write runs as such a task
//! too; their tails after the reload have nothing to observe here.

use lmgw_core::state::{AppState, SharedState};
use serde_json::json;

use crate::common::{self, patience};
use crate::support::dropped_op::hang_up_mid_reload;
use crate::support::gpu_world::{Gpu, GIB};

/// An `/v1/models` list with the one model `id`.
async fn catalog_of(id: &str) -> wiremock::MockServer {
    use wiremock::matchers::{method, path};
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(
                json!({ "object": "list", "data": [{ "id": id, "object": "model" }] }),
            ),
        )
        .mount(&mock)
        .await;
    mock
}

/// `upstream_set` (the tool plane's and `/api/op/upstream_set`'s, in
/// `ops::routing`) moves an upstream while the owner hangs up in its MCP
/// reconcile: its cached catalog is flushed — before the reconcile since
/// 2026-10-24, so no request meanwhile reads the old one — and stays so.
#[tokio::test]
async fn an_upstream_move_the_owner_hung_up_on_still_flushes_its_catalog() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state.clone()).await;
    let (old, new) = (catalog_of("alpha").await, catalog_of("beta").await);
    let id = lmgw_core::store::insert_upstream(
        &state.db,
        &lmgw_core::store::NewUpstream {
            name: "up".into(),
            protocol: lmgw_core::config::Protocol::OpenaiCompatible,
            kind: lmgw_core::config::UpstreamKind::Generic,
            base_url: format!("{}/v1", old.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: true,
            expose_prefix: "up".into(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let listed = || async {
        gw.client()
            .get(format!("{gw}/v1/models"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    };
    assert!(
        listed().await.contains("alpha"),
        "the old catalog is cached"
    );

    hang_up_mid_reload(
        &gw,
        &state,
        "upstream_set",
        json!({ "action": "update", "id": id, "base_url": format!("{}/v1", new.uri()) }),
    )
    .await;

    patience::until_async("the move's tail flushed the catalog", || async {
        listed().await.contains("beta")
    })
    .await;
}

/// The id of local model `model_id`.
fn local_id(state: &SharedState, model_id: &str) -> i64 {
    state
        .snapshot()
        .local_models
        .iter()
        .find(|m| m.model_id == model_id)
        .map(|m| m.id)
        .expect("the row exists")
}

/// `local_model_set action=delete` on a running model: its container is
/// stopped and removed though that comes after the reload the owner hung
/// up on.
#[tokio::test]
async fn a_local_model_delete_the_owner_hung_up_on_still_removes_its_container() {
    let g = Gpu::new(24 * GIB, 2, 30).await;
    g.model("m", 4 * GIB).await;
    lmgw_core::vram::admit(&g.state, &g.route("m"), "m")
        .await
        .unwrap()
        .expect("a local model");
    assert_eq!(g.runs(), ["m"]);
    let gw = common::serve(g.state.clone()).await;
    let id = local_id(&g.state, "m");

    hang_up_mid_reload(
        &gw,
        &g.state,
        "local_model_set",
        json!({ "action": "delete", "id": id }),
    )
    .await;

    patience::until("the delete's tail removed the container", || {
        g.stops().contains(&"m".to_string())
    })
    .await;
}

/// `settings_set` (`ops::settings_patch`) moving the device listener's
/// port: the listener moves though the move comes after the reconcile the
/// owner hung up on.
#[tokio::test]
async fn a_settings_save_the_owner_hung_up_on_still_moves_the_device_listener() {
    let state = AppState::init_for_tests().await.unwrap();
    let before = crate::support::device_pair::listening(&state).await;
    let gw = common::serve(state.clone()).await;
    let port = crate::support::device_link::free_port();
    assert_ne!(port, before);

    hang_up_mid_reload(
        &gw,
        &state,
        "settings_set",
        json!({ "devices": { "port": port } }),
    )
    .await;

    patience::until("the save's tail moved the listener", || {
        state
            .devices
            .listener
            .bound()
            .iter()
            .any(|a| a.port() == port)
    })
    .await;
}

/// The dashboard's own save (`settings_set_full`, `web::api_settings`)
/// moving the device listener's port: the listener moves though the move
/// comes after the reconcile the owner hung up on.
#[tokio::test]
async fn a_dashboard_settings_save_the_owner_hung_up_on_still_moves_the_device_listener() {
    let state = AppState::init_for_tests().await.unwrap();
    let before = crate::support::device_pair::listening(&state).await;
    let gw = common::serve(state.clone()).await;
    let port = crate::support::device_link::free_port();
    assert_ne!(port, before);

    hang_up_mid_reload(
        &gw,
        &state,
        "settings_set_full",
        json!({ "devices": { "port": port } }),
    )
    .await;

    patience::until("the save's tail moved the listener", || {
        state
            .devices
            .listener
            .bound()
            .iter()
            .any(|a| a.port() == port)
    })
    .await;
}
