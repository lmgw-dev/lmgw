//! Chat voice WP3 (chat-voice design §4, §5): dictation's `transcribe` with
//! its own body limit and the fallbacks taken as configured, the press's
//! `voice/warm` through the request admission — `loading` then `ready`,
//! `held` with its cause, eviction where a Background warm skips, a group
//! that does not fit evicting nothing, a press that ends dropping its wait —
//! and a chat turn's own `state` frames. Mock upstreams and the GPU-world
//! fake only; speech is synthetic (silence).

use std::time::Duration;

use lmgw_core::bench::lease::lease;
use lmgw_core::config::{HoldFallbackMode, Protocol, Settings, UpstreamKind};
use lmgw_core::runtime::descriptor::model_runtime;
use lmgw_core::runtime::lifecycle::acquire_spec;
use lmgw_core::runtime::registry::RuntimeState;
use lmgw_core::runtime::Class;
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_actions::sse_events;
use crate::chat_attach_kinds::{new_thread, stt_mock, wav};
use crate::chat_voice_settings::{gateway, mocks, set_voice};
use crate::common::Gw;
use crate::support::gpu_world::{Gpu, GIB};

// -- harness -------------------------------------------------------------------

pub(crate) async fn tweak(state: &SharedState, f: impl FnOnce(&mut Settings)) {
    let mut s = state.snapshot().settings.clone();
    f(&mut s);
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

pub(crate) async fn until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// `seconds` of 16 kHz mono PCM16 silence as a WAV — a recording.
pub(crate) fn recording(seconds: usize) -> Vec<u8> {
    lmgw_core::realtime::audio::pcm::write_wav_pcm16_mono(&vec![0; seconds * 16_000], 16_000)
        .unwrap()
}

async fn transcribe(gw: &Gw, tid: i64, body: Vec<u8>) -> (u16, Value) {
    let (status, _, v) = transcribe_with(gw, tid, Some("audio/wav"), body).await;
    (status, v)
}

/// POST `body` to the thread's `transcribe` as `content_type` (none when
/// `None`): the status, the `x-lmgw-fallback` header, and the JSON.
async fn transcribe_with(
    gw: &Gw,
    tid: i64,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> (u16, Option<String>, Value) {
    let mut req = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/transcribe"))
        .body(body);
    if let Some(ct) = content_type {
        req = req.header("content-type", ct);
    }
    let r = req.send().await.unwrap();
    let status = r.status().as_u16();
    let fallback = r
        .headers()
        .get("x-lmgw-fallback")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (status, fallback, r.json().await.unwrap())
}

/// Press: warm `stages` and read the frames to the end.
pub(crate) async fn warm(gw: &Gw, tid: i64, stages: &[&str]) -> Vec<(String, Value)> {
    let r = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/voice/warm"))
        .json(&json!({ "stages": stages }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let events = sse_events(&r.text().await.unwrap());
    assert_eq!(
        events.last().map(|e| e.0.as_str()),
        Some("done"),
        "{events:?}"
    );
    events
}

/// The `state` frames of `stage`, in order.
pub(crate) fn states(events: &[(String, Value)], stage: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|(e, d)| e == "state" && d["stage"] == stage)
        .map(|(_, d)| d.clone())
        .collect()
}

/// Store `voice` as the thread's own, past the settings route's checks.
pub(crate) async fn store_voice(state: &SharedState, tid: i64, voice: Value) {
    sqlx::query("UPDATE chat_threads SET voice = ?1 WHERE id = ?2")
        .bind(voice.to_string())
        .bind(tid)
        .execute(&state.db)
        .await
        .unwrap();
}

/// A speech-to-text audio row `id` on the GPU world (`audio/<id>`): lazy,
/// on the CPU when `cpu`, falling back to `fallback` under the hold.
pub(crate) async fn asr_row(g: &Gpu, id: &str, cpu: bool, fallback: Option<&str>) {
    let root = g.models_dir().join(id);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("model.gguf"), vec![0u8; 4096]).unwrap();
    let mut row = crate::support::audio_world::tts_row(id, "qwen3_asr");
    row.task = "asr".into();
    row.backend = cpu.then(|| "cpu".to_string());
    row.hold_fallback_mode = if fallback.is_some() {
        HoldFallbackMode::Alias
    } else {
        HoldFallbackMode::Inherit
    };
    row.hold_fallback = fallback.map(str::to_string);
    store::insert_audio_model(&g.state.db, &row).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
}

/// A provider's speech-to-text alias `name` (not on this machine) answering
/// `text`.
pub(crate) async fn cloud_asr(state: &SharedState, name: &str, text: &str) -> MockServer {
    cloud_alias(state, name, stt_mock(text).await).await
}

/// A provider's speech-to-text alias `name` that answers every upload with
/// a 500.
async fn failing_cloud_asr(state: &SharedState, name: &str) -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": {"message": "the provider fell over", "type": "server_error"}
        })))
        .mount(&mock)
        .await;
    cloud_alias(state, name, mock).await
}

/// `mock` as the provider behind the speech-to-text alias `name`.
async fn cloud_alias(state: &SharedState, name: &str, mock: MockServer) -> MockServer {
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: format!("provider-{name}"),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: Some("sk-test".into()),
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
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: name.into(),
            upstream_id: up,
            upstream_model_id: "whisper-1".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({ "capabilities": {
                "task": "asr", "endpoints": ["/v1/audio/transcriptions"], "source": "owner"
            } })),
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    mock
}

/// A GPU world of `total` bytes with its audio models dir set, served; the
/// thread's chat model `talk` (1 GiB unless it is to be sized otherwise).
pub(crate) async fn world(total: u64, containers: usize) -> (Gpu, Gw) {
    let g = Gpu::new(total, containers, 30).await;
    let models = g.models_dir().display().to_string();
    tweak(&g.state, |s| s.audio.models_dir = models).await;
    let gw = crate::common::serve(g.state.clone()).await;
    (g, gw)
}

/// Make the GPU world's chat rows `ids` speech-to-text models.
pub(crate) async fn asr_chat_rows(g: &Gpu, ids: &[&str]) {
    for id in ids {
        sqlx::query("UPDATE local_models SET capabilities_override = ?1 WHERE model_id = ?2")
            .bind(
                json!({ "capabilities": {
                    "task": "asr", "endpoints": ["/v1/audio/transcriptions"], "source": "owner"
                } })
                .to_string(),
            )
            .bind(id)
            .execute(&g.state.db)
            .await
            .unwrap();
    }
    g.state.reload_snapshot().await.unwrap();
}

/// Start chat row `id` and leave it resident, unclaimed.
pub(crate) async fn resident(g: &Gpu, id: &str) {
    let snap = g.state.snapshot();
    let rt = model_runtime(&snap, Class::Chat, id).unwrap();
    let spec = acquire_spec(&g.state, &snap, &rt);
    drop(g.state.runtime().acquire(&spec).await.unwrap());
}

// -- transcribe ------------------------------------------------------------------

#[tokio::test]
async fn dictation_is_transcribed_by_the_threads_asr_within_max_body_mb() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    let tid = new_thread(&gw, "plain", false).await;

    // No ASR at any level: refused, naming where to set one.
    let (status, v) = transcribe(&gw, tid, wav()).await;
    assert_eq!(status, 422, "{v}");
    assert_eq!(v["code"], "asr_not_configured");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("Settings → Chat → Voice"),
        "{v}"
    );

    tweak(&state, |s| s.chat_stt_alias = "my-asr".into()).await;
    let (status, v) = transcribe(&gw, tid, wav()).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["text"], "from the settings alias");
    assert_eq!(v["alias"], "my-asr");
    assert_eq!(v["asr_answered_by"], Value::Null);
    assert_eq!(v["audio_ms"], 5, "40 samples at 8 kHz");
    assert!(v["asr_ms"].is_u64(), "{v}");
    assert_eq!(v["language"], Value::Null);

    // The thread's own ASR and language hint, and a recording of 70 s —
    // over axum's 2 MiB default, under `max_body_mb`.
    set_voice(
        &gw,
        tid,
        json!({ "asr_alias": "other-asr", "language": "de" }),
    )
    .await;
    let long = recording(70);
    assert!(long.len() > 2 * 1024 * 1024);
    let (status, v) = transcribe(&gw, tid, long.clone()).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["text"], "from the thread's alias");
    assert_eq!(v["alias"], "other-asr");
    assert_eq!(v["audio_ms"], 70_000);
    assert_eq!(v["language"], "de");
    let reqs = stt2.received_requests().await.unwrap();
    let upload = reqs
        .iter()
        .find(|r| r.url.path() == "/v1/audio/transcriptions")
        .expect("the thread's alias was called");
    assert!(
        upload.body.len() > long.len(),
        "the whole recording went up"
    );
    let text = String::from_utf8_lossy(&upload.body);
    assert!(
        text.contains("name=\"language\"\r\n\r\nde"),
        "the hint went up"
    );

    // Over the configured bound: refused, naming it.
    tweak(&state, |s| s.max_body_mb = 1).await;
    let (status, v) = transcribe(&gw, tid, long).await;
    assert_eq!(status, 413, "{v}");
    assert_eq!(v["code"], "body_limit");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("max_body_mb (1 MiB)"),
        "{v}"
    );

    let (status, v) = transcribe(&gw, tid, Vec::new()).await;
    assert_eq!((status, v["code"].as_str()), (400, Some("empty_audio")));
}

/// Review m2, m7d: the recording goes up as its `Content-Type` says, with a
/// file name the upstream reads the format from; a type dictation does not
/// send on is `415` before anything goes up; and a chunked body (no
/// `Content-Length`) over `max_body_mb` is bounded like a declared one.
#[tokio::test]
async fn a_recording_goes_up_as_its_type_and_a_chunked_one_is_bounded_too() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    let tid = new_thread(&gw, "plain", false).await;
    tweak(&state, |s| s.chat_stt_alias = "my-asr".into()).await;

    let webm = b"\x1a\x45\xdf\xa3 a stand-in for a WebM recording".to_vec();
    let (status, _, v) = transcribe_with(&gw, tid, Some("audio/webm;codecs=opus"), webm).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["audio_ms"], Value::Null, "not a WAV: no length read");
    let reqs = stt.received_requests().await.unwrap();
    let upload = String::from_utf8_lossy(&reqs.last().unwrap().body).into_owned();
    assert!(
        upload.contains("filename=\"dictation.webm\"")
            && upload.contains("Content-Type: audio/webm"),
        "{upload}"
    );

    let (status, _, v) = transcribe_with(&gw, tid, Some("text/plain"), wav()).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (415, Some("unsupported_media_type")),
        "{v}"
    );
    assert_eq!(crate::chat_attach_kinds::transcriptions(&stt).await, 1);

    tweak(&state, |s| s.max_body_mb = 1).await;
    let big = recording(40);
    assert!(big.len() > 1024 * 1024);
    let stream = futures::stream::once(async move { Ok::<_, std::io::Error>(big) });
    let r = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/transcribe"))
        .header("content-type", "audio/wav")
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413, "chunked bodies are bounded too");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["code"], "body_limit", "{v}");
    assert_eq!(crate::chat_attach_kinds::transcriptions(&stt).await, 1);
}

/// Ruling 6 (§4.4): mic audio takes the fallbacks exactly as configured.
/// Under the GPU hold a GPU ASR row with a provider fallback is answered by
/// the provider, and the answer names it; a row on the CPU is not held and
/// serves itself; a GPU row with no fallback is refused `gpu_hold`.
#[tokio::test]
async fn under_the_hold_the_fallback_answers_a_gpu_row_and_a_cpu_row_serves_itself() {
    let (g, gw) = world(10 * GIB, 2).await;
    g.model("talk", GIB).await;
    let cloud = cloud_asr(&g.state, "cloud-asr", "from the provider").await;
    asr_row(&g, "gpu-ears", false, Some("cloud-asr")).await;
    asr_row(&g, "cpu-ears", true, None).await;
    asr_row(&g, "bare-ears", false, None).await;
    tweak(&g.state, |s| s.hold.active = true).await;
    let tid = new_thread(&gw, "talk", false).await;

    store_voice(&g.state, tid, json!({ "asr_alias": "audio/gpu-ears" })).await;
    let (status, v) = transcribe(&gw, tid, recording(1)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["text"], "from the provider");
    assert_eq!(v["alias"], "audio/gpu-ears");
    assert_eq!(v["asr_answered_by"], "cloud-asr");
    assert_eq!(crate::chat_attach_kinds::transcriptions(&cloud).await, 1);
    assert!(g.runs().is_empty(), "nothing local started: {:?}", g.runs());

    store_voice(&g.state, tid, json!({ "asr_alias": "audio/cpu-ears" })).await;
    let (status, v) = transcribe(&gw, tid, recording(1)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["asr_answered_by"], Value::Null);
    assert_eq!(g.world().transcriptions, ["cpu-ears"]);
    assert_eq!(g.runs(), ["cpu-ears"]);

    store_voice(&g.state, tid, json!({ "asr_alias": "audio/bare-ears" })).await;
    let (status, v) = transcribe(&gw, tid, recording(1)).await;
    assert_eq!((status, v["code"].as_str()), (503, Some("gpu_hold")), "{v}");

    // One row per transcription, the Chat's (`chat`, like its turns), and
    // the fallback's reason on the one the provider answered.
    assert_eq!(
        audio_rows(&g.state).await,
        [
            row("audio/gpu-ears", 200, Some("hold")),
            row("audio/cpu-ears", 200, None),
            row("audio/bare-ears", 503, None),
        ]
    );
}

/// An Admin Chat thread's dictation is labelled as its model turns are,
/// `admin` (charged to `internal:admin-chat`), not `chat`.
#[tokio::test]
async fn an_admin_thread_s_dictation_row_says_admin() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    tweak(&state, |s| s.chat_stt_alias = "my-asr".into()).await;
    let admin: Value = gw
        .client()
        .post(format!("{gw}/chat/api/threads"))
        .json(&json!({ "model_alias": "plain", "kind": "admin" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(admin["kind"], "admin", "{admin}");
    let plain = new_thread(&gw, "plain", false).await;
    for tid in [admin["id"].as_i64().unwrap(), plain] {
        let (status, v) = transcribe(&gw, tid, wav()).await;
        assert_eq!(status, 200, "{v}");
    }
    let labels: Vec<String> = sqlx::query_scalar(
        "SELECT ingress_proto FROM request_logs WHERE class = 'audio' ORDER BY id",
    )
    .fetch_all(&state.db)
    .await
    .unwrap();
    assert_eq!(labels, ["admin", "chat"]);
}

type Row = (String, String, i64, Option<String>);

/// An audio row as [`audio_rows`] reads it, labelled `chat`.
pub(crate) fn row(alias: &str, status: i64, fallback_reason: Option<&str>) -> Row {
    (
        "chat".into(),
        alias.into(),
        status,
        fallback_reason.map(str::to_string),
    )
}

/// The `audio` request rows: `(ingress_proto, requested_alias, status,
/// fallback_reason)`, in order.
pub(crate) async fn audio_rows(state: &SharedState) -> Vec<Row> {
    sqlx::query_as(
        "SELECT ingress_proto, requested_alias, status, fallback_reason FROM request_logs
         WHERE class = 'audio' ORDER BY id",
    )
    .fetch_all(&state.db)
    .await
    .unwrap()
}

/// Review M1 (§4.4): the provider a fallback sent the mic audio to is
/// named even when it fails — the audio left this machine although no text
/// came back. Under the hold, a GPU row whose provider fallback answers 500:
/// the error says so in its body and the gate's headers, and the row says
/// it was the hold's fallback.
#[tokio::test]
async fn a_failed_transcription_still_names_the_fallback_that_had_the_audio() {
    let (g, gw) = world(10 * GIB, 2).await;
    g.model("talk", GIB).await;
    // Kept alive: a dropped mock goes back to wiremock's pool, and another
    // test's would answer.
    let _cloud = failing_cloud_asr(&g.state, "cloud-asr").await;
    asr_row(&g, "gpu-ears", false, Some("cloud-asr")).await;
    tweak(&g.state, |s| s.hold.active = true).await;
    let tid = new_thread(&gw, "talk", false).await;
    store_voice(&g.state, tid, json!({ "asr_alias": "audio/gpu-ears" })).await;

    let (status, fallback, v) = transcribe_with(&gw, tid, Some("audio/wav"), recording(1)).await;
    assert!(status >= 500, "{status} {v}");
    assert_eq!(v["code"], "upstream", "{v}");
    assert_eq!(v["asr_answered_by"], "cloud-asr", "{v}");
    assert_eq!(fallback.as_deref(), Some("cloud-asr"));
    assert!(g.runs().is_empty(), "nothing local started: {:?}", g.runs());
    let rows = audio_rows(&g.state).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (&rows[0].0, &rows[0].1, &rows[0].3),
        (
            &"chat".to_string(),
            &"audio/gpu-ears".to_string(),
            &Some("hold".to_string())
        )
    );
    assert!(rows[0].2 >= 500, "{rows:?}");
}

/// §4.4 for the outside-VRAM verdict too, not only the hold: a GPU ASR
/// model that is not up, with memory outside lmgw leaving it no room, is
/// answered by its fallback at once — and the answer names it.
#[tokio::test]
async fn the_outside_vram_fallback_that_answers_is_named() {
    let (g, gw) = world(10 * GIB, 2).await;
    g.model("talk", GIB).await;
    g.model("ears", 3 * GIB).await;
    asr_chat_rows(&g, &["ears"]).await;
    let _cloud = cloud_asr(&g.state, "cloud-asr", "from the provider").await;
    sqlx::query(
        "UPDATE local_models SET hold_fallback_mode = 'alias', hold_fallback = 'cloud-asr'
         WHERE model_id = 'ears'",
    )
    .execute(&g.state.db)
    .await
    .unwrap();
    tweak(&g.state, |s| s.chat_stt_alias = "ears".into()).await;
    {
        let mut w = g.world();
        w.attribution = true;
        w.outside = 8 * GIB;
    }
    let tid = new_thread(&gw, "talk", false).await;

    let (status, fallback, v) = transcribe_with(&gw, tid, Some("audio/wav"), recording(1)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["text"], "from the provider");
    assert_eq!(v["asr_answered_by"], "cloud-asr", "{v}");
    assert_eq!(fallback.as_deref(), Some("cloud-asr"));
    assert!(g.runs().is_empty(), "nothing local started: {:?}", g.runs());
    assert_eq!(
        audio_rows(&g.state).await,
        [row("ears", 200, Some("external_vram"))]
    );
}

/// Review m5: a page that aborts its upload while a provider transcribes it
/// stops the call instead of dropping it, so the call still writes its row
/// — `canceled`, at once, not when the provider answers. The audio was with
/// the provider; Logs and Usage say so.
#[tokio::test]
async fn an_aborted_dictation_still_writes_its_row() {
    let (g, gw) = world(10 * GIB, 2).await;
    g.model("talk", GIB).await;
    let slow = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "text": "too late" }))
                .set_delay(Duration::from_secs(10)),
        )
        .mount(&slow)
        .await;
    let _slow = cloud_alias(&g.state, "slow-asr", slow).await;
    tweak(&g.state, |s| s.chat_stt_alias = "slow-asr".into()).await;
    let tid = new_thread(&gw, "talk", false).await;

    let sent = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/transcribe"))
        .header("content-type", "audio/wav")
        .body(recording(1))
        .timeout(Duration::from_millis(500))
        .send()
        .await;
    assert!(sent.is_err(), "the page gave up first");
    let mut rows = Vec::new();
    for _ in 0..300 {
        rows = sqlx::query_as::<_, (String, i64, Option<String>)>(
            "SELECT ingress_proto, status, error_kind FROM request_logs WHERE class = 'audio'",
        )
        .fetch_all(&g.state.db)
        .await
        .unwrap();
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        rows,
        [("chat".to_string(), 200, Some("canceled".to_string()))]
    );
}

/// Billable units (billable-units design §4.2, §10): a dictation that
/// arrives as a WAV records its length on its ASR row, measured from the
/// recording as it went up — the provider reports none — and one that is no
/// WAV records none rather than a guess.
#[tokio::test]
async fn a_wav_dictation_records_its_length_on_the_asr_row() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    let tid = new_thread(&gw, "plain", false).await;
    tweak(&state, |s| s.chat_stt_alias = "my-asr".into()).await;
    let rows = || async {
        store::query_logs(
            &state.db,
            &store::LogFilter {
                alias: Some("my-asr".into()),
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };

    let (status, v) = transcribe(&gw, tid, recording(3)).await;
    assert_eq!(status, 200, "{v}");
    crate::common::patience::until_async("the dictation's row", || async {
        rows().await.len() == 1
    })
    .await;
    let row = rows().await.remove(0);
    assert_eq!(row.ingress_proto, "chat");
    assert_eq!(row.audio_in_ms, Some(3_000), "{row:?}");

    let webm = b"\x1a\x45\xdf\xa3 a stand-in for a WebM recording".to_vec();
    let (status, _, v) = transcribe_with(&gw, tid, Some("audio/webm;codecs=opus"), webm).await;
    assert_eq!(status, 200, "{v}");
    crate::common::patience::until_async("the WebM dictation's row", || async {
        rows().await.len() == 2
    })
    .await;
    let row = rows().await.remove(0);
    assert_eq!(row.audio_in_ms, None, "not a WAV: no length read: {row:?}");
}

// -- the press's warm ---------------------------------------------------------------

/// A cold lazy ASR row: `loading`, then `ready` with its time — the
/// container started through admission and the row loaded on the
/// admission's own claim (half a second of silence), every claim let go
/// after and no request row. A second press finds it up and loaded:
/// `ready` with nothing to load, and nothing sent.
#[tokio::test]
async fn a_press_reports_loading_then_ready_and_loads_the_row_on_its_claim() {
    let (g, gw) = world(10 * GIB, 2).await;
    g.model("talk", GIB).await;
    asr_row(&g, "ears", false, None).await;
    tweak(&g.state, |s| s.chat_stt_alias = "audio/ears".into()).await;
    let tid = new_thread(&gw, "talk", false).await;

    let events = warm(&gw, tid, &["asr"]).await;
    let s = states(&events, "asr");
    assert_eq!(s.len(), 2, "{events:?}");
    assert_eq!(
        s[0],
        json!({"stage": "asr", "alias": "audio/ears", "state": "loading", "ms": null})
    );
    assert_eq!(s[1]["state"], "ready", "{s:?}");
    assert!(s[1]["ms"].is_u64(), "{s:?}");
    assert_eq!(g.runs(), ["ears"]);
    assert_eq!(g.world().transcriptions, ["ears"], "loaded ahead of use");
    assert!(g.state.runtime().list().iter().all(|v| v.in_flight == 0));
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_logs")
        .fetch_one(&g.state.db)
        .await
        .unwrap();
    assert_eq!(rows, 0, "a warm-up is no request");

    let again = states(&warm(&gw, tid, &["asr"]).await, "asr");
    assert_eq!(
        again,
        [json!({"stage": "asr", "alias": "audio/ears", "state": "ready", "ms": null})]
    );
    assert_eq!(g.world().transcriptions.len(), 1);
    assert_eq!(g.runs(), ["ears"]);
}

/// Under the GPU hold a press is answered as any request: `held` with its
/// cause for a GPU row with no fallback, `fallback` naming the alias that
/// would answer (nothing started for it), and a CPU row loads and is
/// ready. A benchmark's lease holds the CPU row too, with its own cause.
#[tokio::test]
async fn a_press_is_held_with_its_cause_and_a_cpu_row_still_loads() {
    let (g, gw) = world(10 * GIB, 2).await;
    g.model("talk", GIB).await;
    cloud_asr(&g.state, "cloud-asr", "from the provider").await;
    asr_row(&g, "gpu-ears", false, Some("cloud-asr")).await;
    asr_row(&g, "bare-ears", false, None).await;
    asr_row(&g, "cpu-ears", true, None).await;
    tweak(&g.state, |s| s.hold.active = true).await;
    let tid = new_thread(&gw, "talk", false).await;
    let press = |alias: &'static str| {
        let (state, gw) = (g.state.clone(), &gw);
        async move {
            store_voice(&state, tid, json!({ "asr_alias": alias })).await;
            states(&warm(gw, tid, &["asr"]).await, "asr")
        }
    };

    let s = press("audio/bare-ears").await;
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(
        (&s[0]["state"], &s[0]["cause"]),
        (&json!("held"), &json!("gpu_hold"))
    );
    let s = press("audio/gpu-ears").await;
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(
        (&s[0]["state"], &s[0]["answered_by"]),
        (&json!("fallback"), &json!("cloud-asr"))
    );
    assert!(g.runs().is_empty(), "{:?}", g.runs());

    let s = press("audio/cpu-ears").await;
    let walk: Vec<&Value> = s.iter().map(|f| &f["state"]).collect();
    assert_eq!(walk, [&json!("loading"), &json!("ready")], "{s:?}");
    assert_eq!(g.runs(), ["cpu-ears"]);
    assert_eq!(g.world().transcriptions, ["cpu-ears"]);

    // A benchmark's lease covers the CPU too: nothing new starts.
    tweak(&g.state, |s| s.hold.active = false).await;
    g.state.set_gpu_lease(Some(lease(7, "talk")));
    let s = press("audio/cpu-ears").await;
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(
        (&s[0]["state"], &s[0]["cause"]),
        (&json!("held"), &json!("benchmark"))
    );
    assert_eq!(g.runs(), ["cpu-ears"], "not started again");
    g.state.set_gpu_lease(None);
}

/// §4.2: a press may evict an idle model exactly as the request it
/// announces would. The same card with the same idle model: a group that
/// cannot fit on it together says `does_not_fit` with the sizes and is
/// warmed in Background — `skipped: full`, nothing evicted — while a press
/// of the one stage evicts the idle model and comes up.
#[tokio::test]
async fn a_press_evicts_where_a_background_warm_skips_and_a_group_that_does_not_fit_evicts_nothing()
{
    let (g, gw) = world(10 * GIB, 3).await;
    g.model("big", 8 * GIB).await;
    g.model("ears", 6 * GIB).await;
    g.model("talk", 6 * GIB).await;
    asr_chat_rows(&g, &["ears"]).await;
    tweak(&g.state, |s| s.chat_stt_alias = "ears".into()).await;
    resident(&g, "big").await;
    let tid = new_thread(&gw, "talk", false).await;

    let events = warm(&gw, tid, &["asr", "chat"]).await;
    for stage in ["asr", "chat"] {
        let s = states(&events, stage);
        assert_eq!(s.len(), 2, "{stage}: {events:?}");
        assert_eq!(
            (&s[0]["state"], &s[0]["reason"]),
            (&json!("skipped"), &json!("does_not_fit")),
            "{stage}: {s:?}"
        );
        assert_eq!(s[0]["needed_bytes"], 12 * GIB, "{s:?}");
        assert_eq!(s[0]["capacity_bytes"], 10 * GIB, "{s:?}");
        assert_eq!(
            (&s[1]["state"], &s[1]["reason"]),
            (&json!("skipped"), &json!("full")),
            "{stage}: {s:?}"
        );
    }
    assert!(g.stops().is_empty(), "evicted: {:?}", g.stops());
    assert_eq!(g.runs(), ["big"]);

    let s = states(&warm(&gw, tid, &["asr"]).await, "asr");
    let walk: Vec<&Value> = s.iter().map(|f| &f["state"]).collect();
    assert_eq!(walk, [&json!("loading"), &json!("ready")], "{s:?}");
    assert_eq!(g.stops(), ["big"]);
    assert_eq!(g.runs(), ["big", "ears"]);
    assert!(g.state.runtime().list().iter().all(|v| v.in_flight == 0));
}

/// Two stages that fit together are admitted side by side, and each keeps
/// its claim until both are up.
#[tokio::test]
async fn a_group_that_fits_is_admitted_side_by_side() {
    let (g, gw) = world(10 * GIB, 3).await;
    g.model("ears", 3 * GIB).await;
    g.model("talk", 3 * GIB).await;
    asr_chat_rows(&g, &["ears"]).await;
    tweak(&g.state, |s| s.chat_stt_alias = "ears".into()).await;
    let tid = new_thread(&gw, "talk", false).await;
    let release = g.gate_runs();
    let gw2 = Gw {
        base: gw.base.clone(),
        key: gw.key.clone(),
    };
    let pressed = tokio::spawn(async move { warm(&gw2, tid, &["asr", "chat"]).await });
    until("both starts in flight", || {
        let mut runs = g.runs();
        runs.sort();
        runs == ["ears", "talk"]
    })
    .await;
    release.send(true).unwrap();
    let events = pressed.await.unwrap();
    for stage in ["asr", "chat"] {
        let walk: Vec<Value> = states(&events, stage)
            .iter()
            .map(|f| f["state"].clone())
            .collect();
        assert_eq!(
            walk,
            [json!("loading"), json!("ready")],
            "{stage}: {events:?}"
        );
    }
    assert!(g.state.runtime().list().iter().all(|v| v.in_flight == 0));
}

/// A press that ends while its admission still waits (the page aborts the
/// stream: key released, Esc, thread left) drops the wait: it leaves the
/// queue, and nothing is started for it once room appears.
#[tokio::test]
async fn an_aborted_press_drops_its_admission_wait() {
    let (g, gw) = world(10 * GIB, 3).await;
    g.model("big", 8 * GIB).await;
    g.model("ears", 6 * GIB).await;
    g.model("talk", GIB).await;
    asr_chat_rows(&g, &["ears"]).await;
    tweak(&g.state, |s| s.chat_stt_alias = "ears".into()).await;
    resident(&g, "big").await;
    // `big` is generating for someone lmgw does not see: not a victim.
    g.world().busy.insert("big".into());
    let tid = new_thread(&gw, "talk", false).await;

    let mut r = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/voice/warm"))
        .json(&json!({ "stages": ["asr"] }))
        .send()
        .await
        .unwrap();
    let first = r.chunk().await.unwrap().expect("a first frame");
    assert!(
        String::from_utf8_lossy(&first).contains("\"loading\""),
        "{first:?}"
    );
    let queued = || async {
        g.state
            .vram
            .view(&g.state)
            .await
            .queue
            .iter()
            .any(|w| w.model == "ears")
    };
    for _ in 0..500 {
        if queued().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(queued().await, "the press waits for room");

    drop(r);
    let mut left = false;
    for _ in 0..500 {
        if !queued().await {
            left = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(left, "the wait was dropped with the press");
    g.world().busy.clear();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(g.runs(), ["big"], "nothing started for a press that ended");
    assert!(g.stops().is_empty(), "{:?}", g.stops());
}

#[tokio::test]
async fn a_press_names_what_it_cannot_warm() {
    let (chat, stt, stt2) = mocks().await;
    let (_state, gw) = gateway(&chat, &stt, &stt2).await;
    let tid = new_thread(&gw, "plain", false).await;
    let post = |body: Value| {
        let gw = &gw;
        async move {
            let r = gw
                .client()
                .post(format!("{gw}/chat/api/threads/{tid}/voice/warm"))
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = r.status().as_u16();
            (status, r.json::<Value>().await.unwrap())
        }
    };
    let (status, v) = post(json!({ "stages": ["asr"] })).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (422, Some("asr_not_configured"))
    );
    let (status, v) = post(json!({ "stages": [] })).await;
    assert_eq!((status, v["code"].as_str()), (400, Some("bad_request")));
    let (status, v) = post(json!({ "stages": ["ears"] })).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (400, Some("bad_request")),
        "{v}"
    );
    let r = gw
        .client()
        .post(format!("{gw}/chat/api/threads/999999/voice/warm"))
        .json(&json!({ "stages": ["asr"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

// -- a chat turn's own state ---------------------------------------------------------

/// A plain send to a local model that is not up says `loading` for its
/// `chat` stage before admission and `ready` with the time once it is up —
/// before any of the reply; a send to a model already up says nothing.
#[tokio::test]
async fn a_send_to_a_cold_model_says_loading_then_ready() {
    let (g, gw) = world(10 * GIB, 2).await;
    g.model("talk", GIB).await;
    let tid = new_thread(&gw, "talk", false).await;
    let send = || async {
        let body = gw
            .client()
            .post(format!("{gw}/chat/api/threads/{tid}/send"))
            .json(&json!({ "content": "hello" }))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        sse_events(&body)
    };
    let events = send().await;
    let names: Vec<&str> = events.iter().map(|e| e.0.as_str()).collect();
    let s = states(&events, "chat");
    assert_eq!(s.len(), 2, "{events:?}");
    assert_eq!(
        s[0],
        json!({"stage": "chat", "alias": "talk", "state": "loading", "ms": null})
    );
    assert_eq!(s[1]["state"], "ready");
    assert!(s[1]["ms"].is_u64());
    let ready = names.iter().rposition(|e| *e == "state").unwrap();
    assert!(
        names[..ready].iter().all(|e| *e == "turn" || *e == "state"),
        "{names:?}"
    );
    until("talk is up and idle", || {
        g.state
            .runtime()
            .list()
            .iter()
            .any(|v| v.model_id == "talk" && v.state == RuntimeState::Ready && v.in_flight == 0)
    })
    .await;

    let events = send().await;
    assert!(states(&events, "chat").is_empty(), "{events:?}");
}

/// Review m7e: a tool thread's turn (the agent loop, `agentchat`) says its
/// `chat` stage around an admission that has to start the model, as a plain
/// turn does.
#[tokio::test]
async fn a_tool_thread_s_turn_says_loading_then_ready_too() {
    let (g, gw) = world(10 * GIB, 2).await;
    g.model("talk", GIB).await;
    let url = crate::chat_golden::mcp_stub(Duration::ZERO).await;
    crate::chat_golden::register_stub(&g.state, &url).await;
    let tid = new_thread(&gw, "talk", false).await;
    let (status, v) = crate::chat_voice_settings::post(
        &gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [{ "server_label": "stub" }] }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let body = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "hello" }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let events = sse_events(&body);
    let s = states(&events, "chat");
    assert_eq!(s.len(), 2, "{events:?}");
    assert_eq!(
        s[0],
        json!({"stage": "chat", "alias": "talk", "state": "loading", "ms": null})
    );
    assert_eq!(s[1]["state"], "ready", "{s:?}");
    assert!(s[1]["ms"].is_u64(), "{s:?}");
    assert_eq!(g.runs(), ["talk"]);
}

/// Review m1: a model starting for another caller is not up — the send
/// waits for the whole start, so it says `loading`, then `ready` with the
/// time it waited (not `null`, "nothing had to load").
///
/// No wall-clock floor: the start is released only once the client has read
/// `loading`, so the send's own clock (started before it sent `loading`,
/// stopped after the release) has run at least as long as the client waited
/// between the two.
#[tokio::test]
async fn a_send_to_a_model_starting_for_someone_else_says_loading() {
    let (g, gw) = world(10 * GIB, 2).await;
    g.model("talk", GIB).await;
    let tid = new_thread(&gw, "talk", false).await;
    let release = g.gate_runs();
    let state = g.state.clone();
    let other = tokio::spawn(async move {
        let route = state.snapshot().resolve("talk").unwrap();
        drop(lmgw_core::vram::admit(&state, &route, "talk").await);
    });
    until("talk is starting for the other caller", || {
        g.runs() == ["talk"]
            && g.state
                .runtime()
                .list()
                .iter()
                .any(|v| v.model_id == "talk" && v.state == RuntimeState::Starting)
    })
    .await;
    let gw2 = Gw {
        base: gw.base.clone(),
        key: gw.key.clone(),
    };
    let (loading_tx, loading_rx) = tokio::sync::oneshot::channel();
    let sent = tokio::spawn(async move {
        let mut resp = gw2
            .client()
            .post(format!("{gw2}/chat/api/threads/{tid}/send"))
            .json(&json!({ "content": "hello" }))
            .send()
            .await
            .unwrap();
        let mut body = Vec::new();
        let mut loading = Some(loading_tx);
        while let Some(chunk) = resp.chunk().await.unwrap() {
            body.extend_from_slice(&chunk);
            if loading.is_some()
                && states(&sse_events(&String::from_utf8_lossy(&body)), "chat")
                    .iter()
                    .any(|s| s["state"] == "loading")
            {
                let _ = loading.take().unwrap().send(std::time::Instant::now());
            }
        }
        sse_events(&String::from_utf8_lossy(&body))
    });
    // The send is waiting on the start: it has said `loading`.
    let seen = tokio::time::timeout(Duration::from_secs(30), loading_rx)
        .await
        .expect("the send said loading")
        .expect("the stream said loading before it ended");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let waited = seen.elapsed().as_millis() as u64;
    release.send(true).unwrap();
    other.await.unwrap();
    let events = sent.await.unwrap();
    let s = states(&events, "chat");
    assert_eq!(s.len(), 2, "{events:?}");
    assert_eq!(s[0]["state"], "loading");
    assert_eq!(s[1]["state"], "ready");
    assert!(
        s[1]["ms"].as_u64().is_some_and(|ms| ms >= waited),
        "the send waited at least the {waited} ms between `loading` and the release: {s:?}"
    );
    assert_eq!(g.runs(), ["talk"], "joined, not started again");
}
