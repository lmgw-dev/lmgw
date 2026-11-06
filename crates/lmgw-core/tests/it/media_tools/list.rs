//! What the toolsets offer (design §3.2, §3.4): the `model` enum holds
//! exactly the eligible rows and aliases, each with its prompt style and
//! defaults, and a label with nothing behind it says so.

use lmgw_core::mcp::exec::resolve;
use lmgw_core::mcp::scope::ToolScope;
use lmgw_core::mcp::spec::{ApprovalRule, McpToolSpec};
use serde_json::{json, Value};
use wiremock::MockServer;

use super::{cloud_alias, gateway, image_row, mcp, Z_IMAGE};

/// `/mcp`'s tool `name`, if listed.
async fn listed(gw: &crate::common::Gw, name: &str) -> Option<Value> {
    let v = mcp(gw, "tools/list", json!({})).await;
    v["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .find(|t| t["name"] == name)
        .cloned()
}

fn enum_of(tool: &Value) -> Vec<String> {
    tool["inputSchema"]["properties"]["model"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn the_enum_holds_exactly_the_rows_that_draw_with_their_prompt_style() {
    let sd = MockServer::start().await;
    let mut keywords = image_row("hand-sdxl", "someone/sdxl/own.safetensors", false);
    keywords.prompt_style = Some(lmgw_api_types::chat_media::PromptStyle::Keywords);
    let mut off = image_row("off", Z_IMAGE, false);
    off.enabled = false;
    let mut video = image_row("video", Z_IMAGE, false);
    video.modes = vec!["vid_gen".into()];
    let rows = [
        image_row("z-image-turbo", Z_IMAGE, false),
        keywords,
        image_row("mystery", "someone/else/weights.gguf", false),
        off,
        video,
    ];
    let (state, gw) = gateway(&sd, &rows).await;
    let cloud = MockServer::start().await;
    cloud_alias(&state, &cloud, "cloud-draw.example", "image_generation").await;
    cloud_alias(&state, &cloud, "cloud-chat.example", "chat").await;

    let tool = listed(&gw, "image__generate").await.expect("listed");
    assert_eq!(
        enum_of(&tool),
        vec![
            "image/hand-sdxl",
            "image/mystery",
            "image/z-image-turbo",
            "cloud-draw.example"
        ]
    );
    let d = tool["description"].as_str().unwrap();
    assert!(
        d.contains(
            "- image/z-image-turbo (Z-Image-Turbo): write the prompt as plain sentences. \
             Defaults: 1024x1024, 8 steps, cfg 1 (negative_prompt has no effect at cfg 1)."
        ),
        "{d}"
    );
    assert!(
        d.contains("- image/hand-sdxl: write the prompt as short comma-separated phrases."),
        "the column over no recipe: {d}"
    );
    assert!(
        d.contains(
            "- image/mystery: prompt style not set: describe the picture in plain sentences."
        ),
        "{d}"
    );
    assert!(
        d.contains("- cloud-draw.example (cloud model via cloud): write the prompt as plain sentences. It takes no seed"),
        "{d}"
    );
    assert_eq!(tool["inputSchema"]["required"], json!(["model", "prompt"]));

    // No row edits and the cloud alias publishes no edits route: no edit
    // tool (`edit_eligibility`).
    assert!(listed(&gw, "image__edit").await.is_none());

    // No audio row and no speech alias: no audio tool.
    assert!(listed(&gw, "audio__speak").await.is_none());
    assert!(listed(&gw, "audio__transcribe").await.is_none());
}

#[tokio::test]
async fn a_label_with_nothing_behind_it_says_so() {
    let sd = MockServer::start().await;
    let (state, _gw) = gateway(&sd, &[]).await;
    let spec = |label: &str| McpToolSpec {
        server_label: label.into(),
        allowed_tools: None,
        require_approval: ApprovalRule::Never,
        server_url: None,
        connector_id: None,
    };
    let r = resolve(
        &state,
        &[spec("image"), spec("audio")],
        &ToolScope::gateway(),
    )
    .await;
    assert!(r.tools.is_empty());
    let why: Vec<&str> = r.failed.iter().map(|f| f.why.as_str()).collect();
    assert!(
        why[0].contains("no enabled image model serves generations"),
        "{why:?}"
    );
    assert!(why[1].contains("no enabled audio model"), "{why:?}");
}

#[tokio::test]
async fn the_labels_resolve_to_their_tools_as_builtins() {
    let sd = MockServer::start().await;
    let (state, _gw) = gateway(&sd, &[image_row("z-image-turbo", Z_IMAGE, true)]).await;
    let spec = McpToolSpec {
        server_label: "image".into(),
        allowed_tools: Some(vec!["generate".into()]),
        require_approval: ApprovalRule::Never,
        server_url: None,
        connector_id: None,
    };
    let r = resolve(&state, &[spec], &ToolScope::gateway()).await;
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    let names: Vec<&str> = r.tools.iter().map(|t| t.def.name.as_str()).collect();
    assert_eq!(names, vec!["image__generate"]);
    assert_eq!(r.builtin, vec!["image__generate".to_string()]);
}

#[tokio::test]
async fn the_namespaces_are_reserved_from_southbound_servers() {
    for ns in ["image__", "audio__"] {
        assert!(
            lmgw_core::mcp::RESERVED_NAMESPACES
                .iter()
                .any(|(_, n)| *n == ns),
            "{ns}"
        );
    }
    let sd = MockServer::start().await;
    let (_state, gw) = gateway(&sd, &[]).await;
    let refused = gw
        .client()
        .post(format!("{gw}/api/op/mcp_server_set"))
        .json(&json!({
            "action": "create", "name": "pics", "transport": "stdio",
            "command": "true", "tool_prefix": "image",
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(refused.contains("reserved"), "{refused}");
}

/// The tool block stays the same from turn to turn (review finding 5): an
/// image row's probed size range stays in its line after its container
/// stops, and a cloud speech alias is listed by its model's name before any
/// look read its catalog — and listing reads none.
#[tokio::test]
async fn the_descriptions_do_not_follow_containers_or_catalog_reads() {
    let sd = MockServer::start().await;
    super::mount_sd(&sd, &[super::job("completed")]).await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let cloud = MockServer::start().await;
    let up = lmgw_core::store::insert_upstream(
        &state.db,
        &lmgw_core::store::NewUpstream {
            name: "voice-cloud".into(),
            protocol: lmgw_core::config::Protocol::OpenaiCompatible,
            kind: lmgw_core::config::UpstreamKind::Generic,
            base_url: format!("{}/v1", cloud.uri()),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    lmgw_core::store::insert_alias(
        &state.db,
        &lmgw_core::store::NewAlias {
            alias: "speaker.example".into(),
            upstream_id: up,
            upstream_model_id: "tts-1".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let speak = listed(&gw, "audio__speak")
        .await
        .expect("listed by its name");
    assert_eq!(enum_of(&speak), vec!["speaker.example"]);
    assert!(
        cloud.received_requests().await.unwrap().is_empty(),
        "listing read no catalog"
    );

    let before = listed(&gw, "image__generate").await.unwrap()["description"].clone();
    assert!(!before.as_str().unwrap().contains("Sizes"), "{before}");
    let r = super::call(
        &gw,
        "image__generate",
        json!({"model": "image/z", "prompt": "a red square"}),
    )
    .await;
    assert_eq!(r["isError"], json!(false), "{r}");
    let up = listed(&gw, "image__generate").await.unwrap()["description"].clone();
    assert!(
        up.as_str().unwrap().contains("Sizes 64–4096 px a side."),
        "{up}"
    );
    state
        .runtime()
        .stop(lmgw_core::runtime::Class::Image, "z", true)
        .await
        .unwrap();
    assert!(state.runtime().list().is_empty());
    let after = listed(&gw, "image__generate").await.unwrap()["description"].clone();
    assert_eq!(after, up, "the line outlives the container");
}
