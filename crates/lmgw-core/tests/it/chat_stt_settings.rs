//! The in-process `transcribe` (chat-complete design §8) and the three Chat
//! settings of §11: `chat_pdf_mode`, `chat_stt_alias`, `chat_kb_budget_tokens`.

use bytes::Bytes;
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::ops::{settings_set, SettingsPatch};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{serve, Gw};

/// A gateway with two aliases on one OpenAI-protocol upstream: `my-asr`
/// declared a speech-to-text model and `my-chat` an ordinary chat one.
async fn setup(upstream_base: &str) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "audiocpp".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::AudioCpp,
            base_url: format!("{}/v1", upstream_base.trim_end_matches('/')),
            api_key: Some("sk-up".into()),
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
    for (alias, model, task) in [("my-asr", "qwen3-asr", "asr"), ("my-chat", "gpt", "chat")] {
        store::insert_alias(
            &state.db,
            &NewAlias {
                alias: alias.into(),
                upstream_id: up,
                upstream_model_id: model.into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: Some(json!({ "capabilities": {
                    "task": task, "endpoints": ["/v1/audio/transcriptions"], "source": "owner"
                } })),
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

fn patch(v: Value) -> SettingsPatch {
    serde_json::from_value(v).unwrap()
}

#[tokio::test]
async fn transcribe_sends_the_upload_and_logs_an_audio_row() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "text": " hello there " })))
        .expect(1)
        .mount(&mock)
        .await;
    let (state, _gw) = setup(&mock.uri()).await;

    let text = lmgw_core::proxy::transcribe(
        &state,
        "my-asr",
        Bytes::from_static(b"RIFFxxxxWAVE"),
        "memo.wav",
        "audio/wav",
    )
    .await
    .unwrap();
    assert_eq!(text, "hello there");

    // The upstream saw the concrete model id, the filename and the bytes.
    let req = &mock.received_requests().await.unwrap()[0];
    let body = String::from_utf8_lossy(&req.body);
    assert!(body.contains("qwen3-asr"), "{body}");
    assert!(body.contains("filename=\"memo.wav\""), "{body}");
    assert!(body.contains("RIFFxxxxWAVE"), "{body}");

    let logs = store::query_logs(&state.db, &Default::default())
        .await
        .unwrap();
    let row = logs
        .iter()
        .find(|r| r.requested_alias == "my-asr")
        .unwrap_or_else(|| panic!("no usage row: {logs:?}"));
    assert_eq!(row.status, 200);
    assert_eq!(row.class.as_deref(), Some("audio"));
}

#[tokio::test]
async fn transcribe_surfaces_the_upstream_error_and_logs_it() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": { "message": "unsupported container", "type": "invalid_request_error" }
        })))
        .mount(&mock)
        .await;
    let (state, _gw) = setup(&mock.uri()).await;

    let err = lmgw_core::proxy::transcribe(&state, "my-asr", Bytes::from_static(b"x"), "a.bin", "")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unsupported container"), "{err}");
    let logs = store::query_logs(&state.db, &Default::default())
        .await
        .unwrap();
    assert!(
        logs.iter()
            .any(|r| r.requested_alias == "my-asr" && r.status == 400),
        "{logs:?}"
    );

    // An alias that does not resolve fails without reaching any upstream.
    let err = lmgw_core::proxy::transcribe(&state, "nope", Bytes::new(), "a.wav", "audio/wav")
        .await
        .unwrap_err();
    assert!(!err.to_string().is_empty());
}

#[tokio::test]
async fn the_chat_settings_have_defaults_and_validate() {
    let mock = MockServer::start().await;
    let (state, gw) = setup(&mock.uri()).await;

    let s: Value = gw
        .client()
        .get(format!("{gw}/api/settings-full"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["chat_pdf_mode"], "text");
    assert_eq!(s["chat_stt_alias"], "");
    assert_eq!(s["chat_kb_budget_tokens"], 4000);

    // Set through the MCP/op path; read back through both reads.
    settings_set(
        &state,
        patch(json!({
            "chat_pdf_mode": " Images ",
            "chat_stt_alias": "my-asr",
            "chat_kb_budget_tokens": 1200,
        })),
    )
    .await
    .unwrap();
    let cur = state.snapshot().settings.clone();
    assert_eq!(cur.chat_pdf_mode, "images");
    assert_eq!(cur.chat_stt_alias, "my-asr");
    assert_eq!(cur.chat_kb_budget_tokens, 1200);
    let read = lmgw_core::ops::settings(&state).await.unwrap();
    assert_eq!(read["chat_stt_alias"], "my-asr");
    assert_eq!(read["chat_kb_budget_tokens"], 1200);

    // Refusals, each naming the reason; nothing changes.
    let e = settings_set(&state, patch(json!({ "chat_pdf_mode": "both" })))
        .await
        .unwrap_err();
    assert!(e.contains("text, images, ask"), "{e}");
    let e = settings_set(&state, patch(json!({ "chat_kb_budget_tokens": 0 })))
        .await
        .unwrap_err();
    assert!(e.contains("above 0"), "{e}");
    let e = settings_set(&state, patch(json!({ "chat_kb_budget_tokens": -5 })))
        .await
        .unwrap_err();
    assert!(e.contains("above 0"), "{e}");
    let e = settings_set(&state, patch(json!({ "chat_stt_alias": "my-chat" })))
        .await
        .unwrap_err();
    assert!(e.contains("asr"), "{e}");
    let e = settings_set(&state, patch(json!({ "chat_stt_alias": "nope" })))
        .await
        .unwrap_err();
    assert!(e.contains("does not resolve"), "{e}");
    assert_eq!(state.snapshot().settings.chat_stt_alias, "my-asr");

    // Empty clears the alias.
    settings_set(&state, patch(json!({ "chat_stt_alias": "" })))
        .await
        .unwrap();
    assert_eq!(state.snapshot().settings.chat_stt_alias, "");
}

#[tokio::test]
async fn the_dashboard_settings_path_validates_the_same_way() {
    let mock = MockServer::start().await;
    let (state, gw) = setup(&mock.uri()).await;
    let op = |args: Value| {
        let gw = &gw;
        async move {
            let r = gw
                .client()
                .post(format!("{gw}/api/op/settings_set_full"))
                .json(&args)
                .send()
                .await
                .unwrap();
            (
                r.status().as_u16(),
                r.json::<Value>().await.unwrap_or_default(),
            )
        }
    };
    let (status, res) = op(json!({
        "chat_pdf_mode": "ask", "chat_stt_alias": "my-asr", "chat_kb_budget_tokens": 900
    }))
    .await;
    assert_eq!(status, 200, "{res}");
    let s = state.snapshot().settings.clone();
    assert_eq!(
        (
            s.chat_pdf_mode.as_str(),
            s.chat_stt_alias.as_str(),
            s.chat_kb_budget_tokens
        ),
        ("ask", "my-asr", 900)
    );
    for bad in [
        json!({ "chat_pdf_mode": "pages" }),
        json!({ "chat_kb_budget_tokens": 0 }),
        json!({ "chat_stt_alias": "my-chat" }),
    ] {
        let (status, res) = op(bad.clone()).await;
        assert_eq!(status, 400, "{bad} -> {res}");
    }
}
