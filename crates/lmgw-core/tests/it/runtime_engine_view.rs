//! The engine argv in a view other than a container's (Windows design §4.1):
//! the half of a render every launcher shares, with the paths and the listen
//! address a host process needs.

use lmgw_core::config::LlamaParams;
use lmgw_core::runtime::argv::{
    engine_argv, podman_run_argv, render_engine_args, AudioArgs, EngineArgs, EngineView, LlamaArgs,
    RenderSpec,
};
use lmgw_core::runtime::Class;

fn spec(engine: EngineArgs) -> RenderSpec {
    RenderSpec {
        class: Class::Chat,
        model_id: "m".into(),
        image: "ghcr.io/ggml-org/llama.cpp:server-cuda".into(),
        container_name: "lmgw-chat-m-abcdef".into(),
        container_prefix: "lmgw".into(),
        host_port: 9001,
        models_dir: "/srv/models".into(),
        config_mount: None,
        extra_run_args: vec![],
        globals: vec![],
        init: false,
        entrypoint: None,
        engine,
        health_path: "/health",
        fitted: None,
    }
}

fn chat() -> RenderSpec {
    spec(EngineArgs::Llama(LlamaArgs::Chat {
        gguf_path: "gemma.gguf".into(),
        params: Box::new(LlamaParams {
            mmproj_path: Some("proj/mmproj.gguf".into()),
            draft_gguf_path: Some("/MTP/draft.gguf".into()),
            spec_type: Some("draft-mtp".into()),
            ..LlamaParams::default()
        }),
        args: vec![],
    }))
}

fn host() -> EngineView {
    EngineView::host("/srv/models", "/var/lib/lmgw/audio/m", 9001)
}

fn has_pair(args: &[String], a: &str, b: &str) -> bool {
    args.windows(2).any(|w| w[0] == a && w[1] == b)
}

#[test]
fn the_container_view_is_what_podman_runs() {
    let s = chat();
    assert_eq!(
        engine_argv(&s, &EngineView::container()).unwrap(),
        render_engine_args(&s)
    );
}

#[test]
fn a_host_view_names_host_paths_and_the_host_port() {
    let args = engine_argv(&chat(), &host()).unwrap();
    assert!(has_pair(&args, "-m", "/srv/models/gemma.gguf"), "{args:?}");
    assert!(
        has_pair(&args, "--mmproj", "/srv/models/proj/mmproj.gguf"),
        "{args:?}"
    );
    assert!(
        has_pair(&args, "--model-draft", "/srv/models/MTP/draft.gguf"),
        "{args:?}"
    );
    assert!(has_pair(&args, "--host", "127.0.0.1"), "{args:?}");
    assert!(has_pair(&args, "--port", "9001"), "{args:?}");
    assert!(!args.iter().any(|a| a.starts_with("/models")), "{args:?}");
}

/// An audio row's inputs, as a test needs them.
pub(crate) fn audio_args() -> EngineArgs {
    EngineArgs::Audio(Box::new(AudioArgs {
        settings: Default::default(),
        model: serde_json::from_value(serde_json::json!({
            "id": 1, "model_id": "m", "family": "supertonic", "path": "tts/supertonic",
            "task": "tts", "mode": "offline", "load_options": {}, "session_options": {},
            "voice_presets": {}, "enabled": true, "warm_start": false
        }))
        .unwrap(),
    }))
}

#[test]
fn audio_reads_its_config_from_the_views_config_dir() {
    // The container's entrypoint takes the server as a subcommand; the
    // executable itself takes its flags.
    assert_eq!(
        engine_argv(&spec(audio_args()), &EngineView::container()).unwrap(),
        ["server", "--config", "/config/server.json"]
    );
    assert_eq!(
        engine_argv(&spec(audio_args()), &host()).unwrap(),
        ["--config", "/var/lib/lmgw/audio/m/server.json"]
    );
}

#[test]
fn audio_server_json_names_the_views_paths_and_address() {
    let EngineArgs::Audio(a) = audio_args() else {
        unreachable!()
    };
    let container: serde_json::Value = serde_json::from_str(
        &lmgw_core::runtime::audio::render_single_model_config(&a.settings, &a.model),
    )
    .unwrap();
    assert_eq!(container["host"], "0.0.0.0");
    assert_eq!(container["port"], 8080);
    assert_eq!(container["models"][0]["path"], "/models/tts/supertonic");
    let native: serde_json::Value = serde_json::from_str(
        &lmgw_core::runtime::audio::render_single_model_config_in(&a.settings, &a.model, &host()),
    )
    .unwrap();
    assert_eq!(native["host"], "127.0.0.1");
    assert_eq!(native["port"], 9001);
    assert_eq!(native["models"][0]["path"], "/srv/models/tts/supertonic");
}

/// The `podman run` wrapper mounts and publishes exactly what the
/// container's view says the engine sees: the engine argv after the image
/// and the mounts before it cannot drift apart.
#[test]
fn podmans_mounts_and_port_are_the_container_view() {
    let view = EngineView::container();
    let mut s = spec(audio_args());
    s.config_mount = Some("/var/lib/lmgw/audiocpp/m".into());
    let argv = podman_run_argv(&s);
    assert!(
        has_pair(&argv, "-v", &format!("/srv/models:{}:ro", view.models_dir)),
        "{argv:?}"
    );
    assert!(
        has_pair(
            &argv,
            "-v",
            &format!("/var/lib/lmgw/audiocpp/m:{}:ro", view.config_dir)
        ),
        "{argv:?}"
    );
    assert!(
        has_pair(&argv, "-p", &format!("127.0.0.1:9001:{}", view.listen_port)),
        "{argv:?}"
    );
    // What the engine is told after the image lives under those mounts.
    let config = argv.last().unwrap();
    assert!(config.starts_with(&view.config_dir), "{config}");
    let chat = podman_run_argv(&chat());
    let m = chat.iter().position(|a| a == "-m").unwrap();
    assert!(chat[m + 1].starts_with(&view.models_dir), "{chat:?}");
    assert!(has_pair(&chat, "--port", &view.listen_port.to_string()));
}
