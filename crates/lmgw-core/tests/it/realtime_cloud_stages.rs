//! OpenAI's speech models as a session's stages (realtime live run 3, D1 and
//! N6): an expose-all OpenAI upstream whose catalog — like
//! `api.openai.com`'s — states nothing but ids. Their names say what they
//! are, so `/v1/models` publishes their own routes, a session takes them as
//! its TTS and ASR, the realtime settings save them, and an alias of the
//! wrong task is refused as that rather than as an unknown alias. As the
//! primary TTS, OpenAI's speaks the OpenAI voice name a session asks for
//! (D2).

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::realtime_fakes::{
    chat_fake, events_until, gateway, next_event, open, send, user_text, Turn, Ws,
};
use crate::support::realtime_tts::{speech, spoken_session, wav};

/// The TTS and ASR models the upstream lists, under its prefix.
const TTS: &str = "openai/gpt-4o-mini-tts";
const ASR: &str = "openai/gpt-4o-mini-transcribe";

/// An OpenAI-protocol upstream at a mock, exposed whole under `openai/`,
/// listing models the way `api.openai.com` does — and two aliases on it:
/// `chat-override`, OpenAI's TTS declared a chat model by the owner, and
/// `tts-override`, a chat model declared a TTS model.
async fn openai_upstream(state: &SharedState) -> MockServer {
    let mock = MockServer::start().await;
    let entry = |id: &str| json!({"id": id, "object": "model", "created": 1, "owned_by": "openai"});
    let data: Vec<Value> = [
        "gpt-4o-mini",
        "gpt-4o-mini-tts",
        "gpt-4o-mini-transcribe",
        "whisper-1",
        "tts-1",
        "gpt-realtime",
    ]
    .map(entry)
    .into();
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": data})),
        )
        .mount(&mock)
        .await;
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "openai".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 10_000,
            enabled: true,
            expose_all: true,
            expose_prefix: "openai".into(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    for (alias, model, task) in [
        ("chat-override", "gpt-4o-mini-tts", "chat"),
        ("tts-override", "gpt-4o-mini", "tts"),
    ] {
        store::insert_alias(
            &state.db,
            &NewAlias {
                alias: alias.into(),
                upstream_id: up,
                upstream_model_id: model.into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: Some(json!({ "capabilities": { "task": task } })),
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    mock
}

/// A gateway on the chat fake (`chatty`) and the OpenAI upstream, with the
/// realtime stages on OpenAI's TTS and ASR.
async fn setup() -> (SharedState, String, MockServer) {
    let chat = chat_fake().await;
    let (state, addr) = gateway(&chat, false, None, |s| {
        s.realtime.tts_alias = TTS.into();
        s.realtime.asr_alias = ASR.into();
    })
    .await;
    let mock = openai_upstream(&state).await;
    (state, addr, mock)
}

/// `GET path` on the gateway.
async fn get(addr: &str, path: &str) -> Value {
    reqwest::get(format!("http://{addr}{path}"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// `id`'s `(task, endpoints)` in a `/v1/models` list.
fn caps(list: &Value, id: &str) -> (String, Vec<String>) {
    let m = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("{id} not listed"));
    let c = &m["capabilities"];
    (
        c["task"].as_str().unwrap().to_string(),
        serde_json::from_value(c["endpoints"].clone()).unwrap(),
    )
}

/// A `session.update` of `session`; the next event.
async fn update(ws: &mut Ws, session: Value) -> Value {
    let mut s = json!({"type": "realtime"});
    for (k, v) in session.as_object().unwrap() {
        s[k] = v.clone();
    }
    send(ws, json!({"type": "session.update", "session": s})).await;
    next_event(ws).await
}

#[tokio::test]
async fn the_models_list_gives_openai_s_speech_models_their_own_routes() {
    let (_state, addr, _mock) = setup().await;
    let list = get(&addr, "/v1/models").await;
    assert_eq!(
        caps(&list, TTS),
        ("tts".into(), vec!["/v1/audio/speech".into()])
    );
    assert_eq!(caps(&list, "openai/tts-1").0, "tts");
    assert_eq!(
        caps(&list, ASR),
        ("asr".into(), vec!["/v1/audio/transcriptions".into()])
    );
    assert_eq!(caps(&list, "openai/whisper-1").0, "asr");
    // OpenAI's Realtime model has no route here, and is no chat model.
    assert_eq!(
        caps(&list, "openai/gpt-realtime"),
        ("realtime".into(), vec![])
    );
    // A chat model lists the realtime route: the stages are good.
    let (task, endpoints) = caps(&list, "openai/gpt-4o-mini");
    assert_eq!(task, "chat");
    assert!(
        endpoints.iter().any(|e| e == "/v1/realtime"),
        "{endpoints:?}"
    );
    assert!(endpoints.iter().any(|e| e == "/v1/chat/completions"));
    // The owner's task wins, and brings its routes (N6).
    let (task, endpoints) = caps(&list, "chat-override");
    assert_eq!(task, "chat");
    assert!(endpoints.iter().any(|e| e == "/v1/chat/completions"));
    assert_eq!(
        caps(&list, "tts-override"),
        ("tts".into(), vec!["/v1/audio/speech".into()])
    );
    let one = get(&addr, &format!("/v1/models/{TTS}")).await;
    assert_eq!(
        one["capabilities"]["endpoints"],
        json!(["/v1/audio/speech"])
    );
}

#[tokio::test]
async fn a_session_takes_openai_s_speech_models_as_its_tts_and_asr() {
    let (_state, addr, _mock) = setup().await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    let created = next_event(&mut ws).await;
    let resolved = &created["session"]["lmgw"]["resolved"];
    assert_eq!(resolved["tts"], TTS, "{created}");
    assert_eq!(resolved["asr"], ASR, "{created}");
    // Named by the session, prefixed: used as they are.
    let u = update(
        &mut ws,
        json!({"lmgw": {"tts_model": "openai/tts-1"},
               "audio": {"input": {"transcription": {"model": "openai/whisper-1"}}}}),
    )
    .await;
    assert_eq!(u["type"], "session.updated", "{u}");
    let resolved = &u["session"]["lmgw"]["resolved"];
    assert_eq!(
        (&resolved["tts"], &resolved["asr"]),
        (&json!("openai/tts-1"), &json!("openai/whisper-1"))
    );
    // The owner's override says a chat model is a TTS model: so it is.
    let u = update(&mut ws, json!({"lmgw": {"tts_model": "tts-override"}})).await;
    assert_eq!(u["type"], "session.updated", "{u}");
}

/// An alias of the wrong task was refused `unknown_alias`: "unknown model
/// alias: openai/gpt-4o-mini-tts" for a model that exists. It says what it
/// is now, under a code of its own.
#[tokio::test]
async fn an_alias_of_the_wrong_task_is_refused_as_that() {
    let (_state, addr, _mock) = setup().await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    next_event(&mut ws).await;
    let e = update(&mut ws, json!({"lmgw": {"tts_model": ASR}})).await;
    assert_eq!(e["type"], "error", "{e}");
    assert_eq!(e["error"]["code"], "not_a_tts_alias");
    assert_eq!(e["error"]["param"], "session.lmgw.tts_model");
    let msg = e["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains(&format!(
            "'{ASR}' is a 'asr' model, not a text-to-speech model"
        )),
        "{msg}"
    );
    // The owner's word counts here too: declared chat, it is no TTS.
    let e = update(&mut ws, json!({"lmgw": {"tts_model": "chat-override"}})).await;
    assert_eq!(e["error"]["code"], "not_a_tts_alias", "{e}");
    let e = update(
        &mut ws,
        json!({"audio": {"input": {"transcription": {"model": TTS}}}}),
    )
    .await;
    assert_eq!(e["error"]["code"], "not_an_asr_alias", "{e}");
    assert_eq!(
        e["error"]["param"],
        "session.audio.input.transcription.model"
    );
    assert!(e["error"]["message"]
        .as_str()
        .unwrap()
        .contains("is a 'tts' model, not a speech-to-text model"));
    // A name that is no model at all is still unknown.
    let e = update(&mut ws, json!({"lmgw": {"tts_model": "nothing-here"}})).await;
    assert_eq!(e["error"]["code"], "unknown_alias", "{e}");
}

/// The realtime settings' save (WP8) checks the stages by the same
/// predicate the session goes by.
#[tokio::test]
async fn the_realtime_settings_save_takes_openai_s_speech_models() {
    let (state, _addr, _mock) = setup().await;
    let gw = crate::common::serve(state.clone()).await;
    let save = |realtime: Value| {
        let gw = &gw;
        async move {
            let resp = gw
                .client()
                .post(format!("{gw}/api/op/settings_set_full"))
                .json(&json!({ "realtime": realtime }))
                .send()
                .await
                .unwrap();
            let status = resp.status().as_u16();
            (status, resp.json::<Value>().await.unwrap())
        }
    };
    let (status, body) = save(json!({"tts_alias": TTS, "asr_alias": ASR,
                                     "barge_in_check_alias": "openai/whisper-1"}))
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = save(json!({"tts_alias": ASR})).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_string().contains("is a 'asr' model"), "{body}");
}

/// The `voice` of every speech request the upstream got.
async fn voices_sent(mock: &MockServer) -> Vec<Value> {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/v1/audio/speech")
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["voice"].clone())
        .collect()
}

/// OpenAI's TTS as the session's primary, the owner's `default_voice` a
/// local one (`alba`): `alloy` was refused `voice_not_configured` without
/// that setting, and with it `alba` would have gone to OpenAI. OpenAI's
/// names are its own voices — the client's, and the session's default
/// `marin`.
#[tokio::test]
async fn openai_s_tts_speaks_the_openai_voice_the_session_asks_for() {
    let chat = chat_fake().await;
    let (state, addr) = gateway(&chat, false, None, |s| {
        s.realtime.tts_alias = TTS.into();
        s.realtime.default_voice = "alba".into();
    })
    .await;
    let mock = openai_upstream(&state).await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/wav")
                .set_body_bytes(wav(&speech(300), 24_000).to_vec()),
        )
        .mount(&mock)
        .await;
    for (voice, want) in [(json!("alloy"), "alloy"), (Value::Null, "marin")] {
        let mut extra = json!({"lmgw": {"output_lead_ms": 60_000}});
        if !voice.is_null() {
            extra["audio"] = json!({"output": {"voice": voice}});
        }
        let (mut ws, updated) = spoken_session(&addr, &[], 60_000, extra).await;
        assert_eq!(
            updated["session"]["lmgw"]["resolved"]["voice"], want,
            "{updated}"
        );
        chat.push(Turn::text(&["Hello there."]));
        send(&mut ws, user_text("hi")).await;
        events_until(&mut ws, "conversation.item.done").await;
        send(&mut ws, json!({"type": "response.create"})).await;
        let events = events_until(&mut ws, "response.done").await;
        assert_eq!(
            events.last().unwrap()["response"]["status"],
            "completed",
            "{events:?}"
        );
        assert_eq!(voices_sent(&mock).await.last(), Some(&json!(want)));
    }
    assert!(!voices_sent(&mock).await.contains(&json!("alba")));
}
