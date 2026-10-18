//! The settings page's `mcp` and `forge_tokens` keys over the real router
//! (`GET /api/settings-full`): `mcp` is the host link's four limits as
//! `HostSettings` (`ops::mcp_settings_view`), and `forge_tokens` names the
//! hosts that have a token and shows none of them (`ops::redact_map`).

use lmgw_api_types::mcp_host::HostSettings;
use lmgw_core::state::AppState;
use lmgw_core::store;
use serde_json::{json, Value};

use crate::common;

/// The settings page's `mcp` is the host link's four limits as the config
/// holds them, and `forge_tokens` names the hosts that have a token and shows
/// none of them.
#[tokio::test]
async fn settings_full_shows_the_mcp_limits_and_only_the_hosts_with_a_token() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state.clone()).await;
    let get = || async {
        let r = gw
            .client()
            .get(format!("{gw}/api/settings-full"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.json::<Value>().await.unwrap()
    };
    let before = get().await;
    assert_eq!(before["forge_tokens"], json!({}));
    let defaults: HostSettings = serde_json::from_value(before["mcp"].clone()).unwrap();
    assert_eq!(serde_json::to_value(&defaults).unwrap(), before["mcp"]);

    let mut s = state.snapshot().settings.clone();
    s.forge_tokens
        .insert("github.com".into(), "ghp_never_shown".into());
    s.forge_tokens
        .insert("git.example:8443".into(), "glpat_never_shown".into());
    s.mcp.host_max_message_mb = 64;
    s.mcp.host_max_frame_mb = 0;
    s.mcp.host_ping_interval_s = 7;
    s.mcp.task_poll_interval_s = 3;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let body = get().await;
    assert_eq!(
        body["forge_tokens"],
        json!({"git.example:8443": "<set>", "github.com": "<set>"})
    );
    assert!(!body.to_string().contains("never_shown"));
    let mcp: HostSettings = serde_json::from_value(body["mcp"].clone()).unwrap();
    assert_eq!(
        mcp,
        HostSettings {
            host_max_message_mb: 64,
            host_max_frame_mb: 0,
            host_ping_interval_s: 7,
            task_poll_interval_s: 3,
        }
    );
    assert_eq!(serde_json::to_value(&mcp).unwrap(), body["mcp"]);
}
