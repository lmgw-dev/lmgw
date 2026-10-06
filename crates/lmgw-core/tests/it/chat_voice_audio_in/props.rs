//! Voice turns the model hears, by what its llama-server says of itself
//! (`GET /props` `modalities.audio`, llama egress design §4.1;
//! `capabilities::hears` step 3): a server that loaded no audio projector
//! is refused the audio whatever the row publishes, and one that did
//! lifts a projector lmgw could not read. Read from the container the
//! send's claim is on, at the send; the turn seam
//! (`web::spoken_turn_for_tests`) on `gpu_world`'s fake containers.

use lmgw_core::store::NewLocalModel;
use lmgw_core::web::spoken_turn_for_tests;
use serde_json::{json, Value};

use super::turns::{audio, row, thread, world};
use crate::support::gpu_world::{ANSWER, GIB};

/// A `/props` body whose server did (`true`) or did not load an audio
/// projector.
fn props(audio: bool) -> Value {
    json!({"modalities": {"vision": false, "audio": audio},
           "default_generation_settings": {"n_ctx": 4096}, "build_info": "b1-test"})
}

#[tokio::test]
async fn a_server_with_no_audio_projector_is_refused_the_audio() {
    let g = world().await;
    g.world().props.insert("gemma".into(), props(false));
    let tid = thread(&g, "gemma", "chat").await;
    let frames = spoken_turn_for_tests(&g.state, tid, vec![audio()]).await;
    let error = &frames
        .iter()
        .find(|(e, _)| e == "error")
        .unwrap_or_else(|| panic!("refused: {frames:?}"))
        .1;
    assert_eq!(error["code"], "audio_input_unsupported", "{frames:?}");
    let message = error["message"].as_str().unwrap();
    assert!(
        message.starts_with("gemma's server loaded no audio projector"),
        "{message}"
    );
    assert!(g.world().streamed_bodies.is_empty(), "nothing was sent");
}

/// A row whose projector lmgw cannot read publishes no modalities: unknown,
/// which the server's own word lifts.
#[tokio::test]
async fn a_server_that_loaded_an_audio_projector_lifts_an_unknown() {
    let g = world().await;
    let mut unsure = NewLocalModel {
        capabilities_override: None,
        ..row("unsure")
    };
    unsure.params.mmproj_path = Some("unreadable-mmproj.gguf".into());
    g.row(unsure, 8 * GIB).await;
    g.world().thinking.insert("unsure".into());
    g.world().props.insert("unsure".into(), props(true));
    let tid = thread(&g, "unsure", "chat").await;
    let frames = spoken_turn_for_tests(&g.state, tid, vec![audio()]).await;
    assert!(
        !frames.iter().any(|(e, _)| e == "error"),
        "not refused: {frames:?}"
    );
    let sent = g.world().streamed_bodies.clone();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].to_string().contains("input_audio"), "{}", sent[0]);
    let rows = lmgw_core::store::list_chat_messages(&g.state.db, tid)
        .await
        .unwrap();
    assert_eq!(rows.last().unwrap().content, ANSWER);

    // The same row with a server that says nothing of audio: unknown is
    // no, and the audio stays.
    let g = world().await;
    let mut unsure = NewLocalModel {
        capabilities_override: None,
        ..row("unsure")
    };
    unsure.params.mmproj_path = Some("unreadable-mmproj.gguf".into());
    g.row(unsure, 8 * GIB).await;
    let tid = thread(&g, "unsure", "chat").await;
    let frames = spoken_turn_for_tests(&g.state, tid, vec![audio()]).await;
    let error = &frames
        .iter()
        .find(|(e, _)| e == "error")
        .expect("refused")
        .1;
    assert_eq!(error["code"], "audio_input_unsupported", "{frames:?}");
    assert!(g.world().streamed_bodies.is_empty());
}

/// Review V11: the verdict (no claim held) reads the facts of the container
/// that is up — the thread JSON says the server loaded no audio projector.
#[tokio::test]
async fn the_verdict_reads_the_running_containers_props() {
    use lmgw_core::runtime::descriptor::model_runtime;
    use lmgw_core::runtime::lifecycle::acquire_spec;
    use lmgw_core::runtime::Class;

    let (g, w) = super::session::hearing("on", 24 * GIB, 30).await;
    g.world().props.insert("gemma".into(), props(false));
    let tid = w.thread("gemma", json!({})).await;
    // Nothing up: the row's published capabilities, which take audio.
    assert_eq!(verdict(&w, tid).await["path"], "audio");
    // Up, and its `/props` read at the start: no audio projector.
    let snap = g.state.snapshot();
    let rt = model_runtime(&snap, Class::Chat, "gemma").unwrap();
    let pin = g
        .state
        .runtime()
        .acquire(&acquire_spec(&g.state, &snap, &rt))
        .await
        .unwrap();
    drop(pin);
    let v = verdict(&w, tid).await;
    assert_eq!(v["path"], "transcript", "{v}");
    assert_eq!(v["why"], "gemma's server loaded no audio projector");
}

/// Review V11: an external `llama_cpp` row's facts come from the background
/// cache — unknown on the first look, which reads the published answer;
/// once the server said it has no audio projector, the verdict says so.
#[tokio::test]
async fn an_external_llama_servers_props_reach_the_verdict_from_the_cache() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (g, w) = super::session::hearing("on", 24 * GIB, 30).await;
    let lan = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(props(false)))
        .mount(&lan)
        .await;
    let up = lmgw_core::store::insert_upstream(
        &g.state.db,
        &lmgw_core::store::NewUpstream {
            name: "lan".into(),
            protocol: lmgw_core::config::Protocol::LlamaCpp,
            kind: lmgw_core::config::UpstreamKind::LlamaServer,
            base_url: format!("{}/v1", lan.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    lmgw_core::store::insert_alias(
        &g.state.db,
        &lmgw_core::store::NewAlias {
            alias: "lan-model".into(),
            upstream_id: up,
            upstream_model_id: "gguf".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(super::turns::hears()),
        },
    )
    .await
    .unwrap();
    g.state.reload_snapshot().await.unwrap();
    let tid = w.thread("lan-model", json!({})).await;
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let v = verdict(&w, tid).await;
        if v["path"] == "transcript" {
            assert_eq!(
                v["why"], "lan-model's server loaded no audio projector",
                "{v}"
            );
            break;
        }
        assert_eq!(v["path"], "audio", "the published answer meanwhile: {v}");
        assert!(std::time::Instant::now() < until, "never read: {v}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// The thread JSON's audio-input verdict of thread `tid`.
async fn verdict(w: &crate::realtime_chat_thread::World, tid: i64) -> Value {
    w.get(&format!("/chat/api/threads/{tid}")).await["thread"]["voice_resolved"]["audio_input"]
        .clone()
}
