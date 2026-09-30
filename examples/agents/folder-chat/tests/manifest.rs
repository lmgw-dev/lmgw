//! The shipped `agent.json`, judged by lmgw's own code rather than a copy of
//! its rules: the manifest parser, the write-time origin checks, the manifest
//! warning pass and the runtime warning pass a card shows, and the token scope
//! the config derives.

use lmgw_core::agents::{self, manifest, service, token, Agent};
use lmgw_core::config::ScopeMode;
use lmgw_core::state::AppState;
use lmgw_core::store::{AgentRow, AGENT_SOURCE_IMPORTED};
use serde_json::{json, Value};

const MANIFEST: &str = include_str!("../agent.json");

fn row(config: Value) -> AgentRow {
    AgentRow {
        id: "folder-chat".into(),
        manifest: MANIFEST.into(),
        config: config.to_string(),
        enabled: true,
        source: AGENT_SOURCE_IMPORTED.into(),
        provenance: "{}".into(),
        dev_url: None,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

#[test]
fn loads_with_the_shape_the_library_expects() {
    let m = manifest::load(MANIFEST).unwrap_or_else(|e| panic!("agent.json is refused: {e}"));
    assert_eq!(m.id, "folder-chat");
    assert_eq!(m.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    assert_eq!(m.image(), Some("localhost/folder-chat:0.2.0"));
    assert!(
        m.image().unwrap().ends_with(env!("CARGO_PKG_VERSION")),
        "the image tag follows the crate version"
    );
    assert_eq!(m.pull(), manifest::PullPolicy::Never);
    assert_eq!(m.limits().memory_mb, 2048);

    // The form, in order.
    let fields = m.fields().unwrap();
    let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "folder",
            "embed_model",
            "chat_model",
            "rerank_model",
            "vision_model",
            "vision_every_page",
            "chunk_tokens",
            "allow_remote"
        ]
    );
    let mounts: Vec<_> = m.mount_fields().collect();
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].name, "folder");
    assert_eq!(mounts[0].access, manifest::Access::Rw);
    assert_eq!(mounts[0].inside(), folder_chat::config::CONTAINER_FOLDER);
    assert!(mounts[0].required);
    let desc = fields[0].description.as_deref().unwrap();
    assert!(desc.contains(folder_chat::config::INDEX_DIR_NAME), "{desc}");

    // The library's constants and the form's numbers are one fact.
    let chunk = fields.iter().find(|f| f.name == "chunk_tokens").unwrap();
    assert_eq!(
        chunk.default,
        Some(json!(folder_chat::config::DEFAULT_CHUNK_TOKENS))
    );
    assert_eq!(
        chunk.minimum,
        Some(folder_chat::config::MIN_CHUNK_TOKENS as f64)
    );
    for f in ["embed_model", "chat_model", "rerank_model", "vision_model"] {
        let field = fields.iter().find(|x| x.name == f).unwrap();
        assert_eq!(field.format, Some(manifest::Format::ModelAlias), "{f}");
    }
    for f in ["rerank_model", "vision_model", "vision_every_page"] {
        assert!(
            !fields.iter().find(|x| x.name == f).unwrap().required,
            "{f}"
        );
    }

    // The vision model sees page images: the field says to keep it local,
    // and reading every page is off unless the owner turns it on.
    let vision = fields.iter().find(|f| f.name == "vision_model").unwrap();
    let desc = vision.description.as_deref().unwrap();
    assert!(desc.contains("local model"), "{desc}");
    let every = fields
        .iter()
        .find(|f| f.name == "vision_every_page")
        .unwrap();
    assert_eq!(every.ty, manifest::FieldType::Boolean);
    assert_eq!(every.default, Some(json!(false)));
    let defaults = json!({ "folder": "/f", "embed_model": "e", "chat_model": "c",
                           "vision_every_page": every.default.clone().unwrap() });
    let c = folder_chat::config::AgentConfig::from_config(&defaults).unwrap();
    assert_eq!((c.vision_model, c.vision_every_page), (None, false));

    // The app face's address rule: off unless the owner turns it on, and the
    // field says what turning it on gives away.
    let remote = fields.iter().find(|f| f.name == "allow_remote").unwrap();
    assert_eq!(remote.ty, manifest::FieldType::Boolean);
    assert_eq!(remote.default, Some(json!(false)));
    assert!(!remote.required);
    assert_eq!(remote.title.as_deref(), Some("Serve other machines"));
    let desc = remote.description.as_deref().unwrap();
    assert!(desc.contains("there is no login"), "{desc}");
    let defaults = json!({ "folder": "/f", "embed_model": "e", "chat_model": "c",
                           "allow_remote": remote.default.clone().unwrap() });
    assert!(
        !folder_chat::config::AgentConfig::from_config(&defaults)
            .unwrap()
            .allow_remote
    );
}

#[tokio::test]
async fn nothing_blocks_start_but_the_unbound_folder() {
    let m = manifest::load(MANIFEST).unwrap();
    let state = AppState::init_for_tests().await.unwrap();

    // The write-time refusals an import would apply before saving.
    assert_eq!(service::origin_label_refusal(&m), None);
    assert_eq!(
        service::origin_shadows_refusal(&state.snapshot().settings, &m),
        None
    );

    let static_warnings = agents::manifest_warnings(&m, AGENT_SOURCE_IMPORTED);
    assert!(
        static_warnings.iter().all(|w| !w.blocks_start),
        "{static_warnings:?}"
    );

    // The card as it stands right after import: nothing bound yet.
    let agent = Agent::from_row(row(json!({}))).unwrap();
    let warnings = agents::runtime_warnings(&state, &agent, &Ok(())).await;
    let blocking: Vec<&str> = warnings
        .iter()
        .filter(|w| w.blocks_start)
        .map(|w| w.code)
        .collect();
    assert!(
        blocking.contains(&"mount_unbound"),
        "the folder must be asked for: {warnings:?}"
    );
    // `image_absent_pull_never` is a fact about this box, not the manifest:
    // the image is built in packaging (phase 2b). Until it exists here, it is
    // the one other blocker, and it names the image.
    for w in warnings.iter().filter(|w| w.blocks_start) {
        match w.code {
            "mount_unbound" => assert!(w.message.contains("bind 'folder' on the Run tab")),
            "image_absent_pull_never" => {
                eprintln!("note: {} (build the image to clear it)", w.message)
            }
            other => panic!("unexpected blocking warning {other}: {}", w.message),
        }
    }
}

#[test]
fn the_token_is_scoped_to_exactly_the_chosen_models() {
    let agent = Agent::from_row(row(json!({
        "embed_model": "embed/bge-m3",
        "chat_model": "qwen3.8",
        "rerank_model": "rerank/bge-reranker",
        "vision_model": "gemma4-12b"
    })))
    .unwrap();
    let (mode, patterns) = token::derive_scope(&agent);
    assert_eq!(mode, ScopeMode::Allow);
    assert_eq!(
        patterns,
        "embed/bge-m3\nqwen3.8\nrerank/bge-reranker\ngemma4-12b"
    );

    // `model.alias` is a template, so it adds nothing a literal would.
    let agent = Agent::from_row(row(json!({
        "embed_model": "embed/bge-m3",
        "chat_model": "qwen3.8"
    })))
    .unwrap();
    assert_eq!(token::derive_scope(&agent).1, "embed/bge-m3\nqwen3.8");
}
