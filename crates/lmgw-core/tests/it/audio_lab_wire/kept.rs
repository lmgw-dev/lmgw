//! The Audio lab on the lab store (labs-store design §5): a group made
//! first, its hand-off with `?group=` kept as its run. Speech is an output
//! artifact served back as it was answered; a transcription's upload is the
//! group's input artifact and its answer text; a speech stream the client
//! stops is kept as far as it got, `stopped`; a group is one route's and
//! answered once; the export carries the files.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use lmgw_api_types::lab_store::{LabGroupView, LabMultipartSummary};
use lmgw_core::runtime::registry::Registry;
use serde_json::{json, Value};

use super::{lab_world, post_json};
use crate::common::round_trips;
use crate::support::audio_world::{wav_bytes, world, World};
use crate::support::audiocpp_gguf;

/// `POST /audio-lab/api/groups` for `route` on `model`: the group, as its
/// type.
pub(super) async fn group(w: &World, model: &str, route: &str, form: Value) -> LabGroupView {
    let (status, v) = post_json(
        w,
        "/audio-lab/api/groups",
        json!({"model": model, "route": route, "form": form}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    round_trips::<LabGroupView>("group", &v)
}

/// The names of the files in the artifacts directory, sorted.
pub(super) fn files_in_artifacts(w: &World) -> Vec<String> {
    let dir = lmgw_core::labs::artifacts::dir(&w.state.data_dir);
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Group `id` as the store has it now.
pub(super) async fn read(w: &World, id: i64) -> Value {
    w.gw.client()
        .get(format!("{}/labs/api/audio/groups/{id}", w.gw))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn speech_is_kept_as_an_output_artifact_and_served_back() {
    let w = lab_world().await;
    let form = json!({"task": "tts", "settings": {"speech": {"model": "audio/tts",
                      "input": "hello there\nsecond line", "voice": "af_heart"}}});
    let g = group(&w, "audio/tts", "speech", form).await;
    assert_eq!(g.title, "hello there");
    assert!(g.runs.is_empty(), "no run before its request");
    assert_eq!(g.plan[0].variant, "speech");

    let body = r#"{"model":"audio/tts","input":"hello there","voice":"af_heart"}"#;
    let resp =
        w.gw.client()
            .post(format!("{}/audio-lab/api/speech?group={}", w.gw, g.id))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "audio/wav");
    let wav = resp.bytes().await.unwrap().to_vec();
    assert_eq!(wav, wav_bytes(24_000, 480), "relayed as it came");

    // Recorded before the last byte reached the client.
    let v = read(&w, g.id).await;
    let kept = round_trips::<LabGroupView>("kept group", &v);
    let run = &kept.runs[0];
    assert_eq!(run.endpoint, "/v1/audio/speech");
    assert_eq!(run.http_status, 200);
    assert_eq!(run.request, body, "the request as sent, byte for byte");
    assert_eq!(run.response, None, "speech is no text in the database");
    assert_eq!(run.response_type.as_deref(), Some("audio/wav"));
    assert_eq!(run.error_code, None);
    let art = &run.artifacts[0];
    assert_eq!(art.mime, "audio/wav");
    assert_eq!(art.name, "audio-tts.wav");
    assert_eq!(art.size, wav.len() as u64);
    let back =
        w.gw.client()
            .get(format!("{}{}", w.gw, art.href))
            .send()
            .await
            .unwrap();
    assert_eq!(back.headers()["content-type"], "audio/wav");
    assert_eq!(back.bytes().await.unwrap().to_vec(), wav);
    assert_eq!(kept.form["settings"]["speech"]["voice"], "af_heart");
    // Spooled to its file as it streamed: the spool is the file now.
    assert_eq!(files_in_artifacts(&w), vec![art.sha256.clone()]);

    // Answered once: the same group again is the 409, sent nowhere.
    let sent = w.sent().await.len();
    let again =
        w.gw.client()
            .post(format!("{}/audio-lab/api/speech?group={}", w.gw, g.id))
            .json(&json!({"model": "audio/tts", "input": "again"}))
            .send()
            .await
            .unwrap();
    assert_eq!(again.status(), 409);
    let e: Value = again.json().await.unwrap();
    assert_eq!(e["error"]["code"], "cell_answered", "{e}");
    assert_eq!(w.sent().await.len(), sent);

    // A group is its route's: speech's group is no transcription's.
    let part = reqwest::multipart::Part::bytes(wav_bytes(16_000, 800))
        .file_name("c.wav")
        .mime_str("audio/wav")
        .unwrap();
    let g2 = group(&w, "audio/tts", "speech", json!({})).await;
    let wrong =
        w.gw.client()
            .post(format!(
                "{}/audio-lab/api/transcriptions?group={}",
                w.gw, g2.id
            ))
            .multipart(
                reqwest::multipart::Form::new()
                    .text("model", "audio/asr")
                    .part("file", part),
            )
            .send()
            .await
            .unwrap();
    assert_eq!(wrong.status(), 400);
    let e: Value = wrong.json().await.unwrap();
    assert_eq!(e["error"]["code"], "wrong_route", "{e}");
    // Another lab's group, or none, is the 404.
    let missing =
        w.gw.client()
            .post(format!("{}/audio-lab/api/speech?group=99999", w.gw))
            .json(&json!({"model": "audio/tts", "input": "x"}))
            .send()
            .await
            .unwrap();
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn a_transcriptions_upload_is_an_input_artifact_and_its_answer_text() {
    let w = lab_world().await;
    let form = json!({"task": "asr", "asr": {"source": "upload", "file_name": "memo.wav",
                      "details": true}});
    let g = group(&w, "audio/asr", "transcription_details", form).await;
    assert_eq!(g.title, "memo.wav");
    let clip = wav_bytes(16_000, 800);
    let part = reqwest::multipart::Part::bytes(clip.clone())
        .file_name("memo.wav")
        .mime_str("audio/wav")
        .unwrap();
    let resp =
        w.gw.client()
            .post(format!(
                "{}/audio-lab/api/transcriptions?details=1&group={}",
                w.gw, g.id
            ))
            .multipart(
                reqwest::multipart::Form::new()
                    .text("model", "audio/asr")
                    .text("language", "en")
                    .part("file", part),
            )
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(answer["text"], "hello there");

    let kept = round_trips::<LabGroupView>("kept", &read(&w, g.id).await);
    let input = &kept.artifacts[0];
    assert_eq!(
        input.role,
        lmgw_api_types::lab_store::LabArtifactRole::Input
    );
    assert_eq!(input.run_id, None, "the group's, not the run's");
    assert_eq!(input.name, "memo.wav");
    assert_eq!(input.mime, "audio/wav");
    assert_eq!(input.size, clip.len() as u64);
    let run = &kept.runs[0];
    assert_eq!(run.endpoint, "/v1/audio/transcriptions/details");
    assert!(run.artifacts.is_empty(), "the answer is text");
    let said: Value = serde_json::from_str(run.response.as_deref().unwrap()).unwrap();
    assert_eq!(said["words"][0]["word"], "hello");
    let sent: LabMultipartSummary = serde_json::from_str(&run.request).unwrap();
    assert_eq!(sent.field("model"), Some("audio/asr"));
    assert_eq!(sent.field("language"), Some("en"));
    assert_eq!(sent.files[0].name, "file");
    assert_eq!(
        sent.files[0].artifact.as_deref(),
        Some(format!("lab-artifact:{}", input.id).as_str())
    );

    // The export: a zip with the clip, and the table naming the text.
    let zip_bytes =
        w.gw.client()
            .get(format!("{}/labs/api/audio/export", w.gw))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .to_vec();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).unwrap();
    let mut f = zip
        .by_name(&format!("artifacts/{}.wav", input.sha256))
        .expect("the clip is in the zip");
    let mut back = Vec::new();
    std::io::Read::read_to_end(&mut f, &mut back).unwrap();
    assert_eq!(back, clip);
    let csv_text =
        w.gw.client()
            .get(format!("{}/labs/api/audio/export?format=csv", w.gw))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    let mut rows = csv::Reader::from_reader(csv_text.as_bytes());
    let head = rows.headers().unwrap().clone();
    let row = rows.records().next().unwrap().unwrap();
    let col = |n: &str| {
        row.get(head.iter().position(|h| h == n).unwrap())
            .unwrap()
            .to_string()
    };
    assert_eq!(col("text"), "hello there");
    assert_eq!(col("endpoint"), "/v1/audio/transcriptions/details");
    assert_eq!(
        col("files"),
        "0",
        "the upload is the group's, not the run's"
    );
}

/// The first event of the streamed answer below.
const FIRST: &[u8] = b"data: {\"type\":\"speech.audio.delta\",\"audio\":\"AAAAAAAA\"}\n\n";

/// A container whose speech answer is [`FIRST`] and then a comment every
/// 20 ms, for as long as anyone reads.
async fn streaming_container() -> u16 {
    use axum::routing::{get, post};
    let app = axum::Router::new()
        .route(
            "/v1/models",
            get(|| async { axum::Json(json!({"data": []})) }),
        )
        .route(
            "/v1/audio/speech",
            post(|| async {
                let first = futures::stream::once(async {
                    Ok::<_, std::io::Error>(Bytes::from_static(FIRST))
                });
                let more = futures::stream::unfold((), |()| async {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Some((
                        Ok::<_, std::io::Error>(Bytes::from_static(b": more\n\n")),
                        (),
                    ))
                });
                (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(futures::StreamExt::chain(first, more)),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    port
}

#[tokio::test]
async fn a_stopped_speech_stream_is_kept_as_far_as_it_got() {
    let w = world().await;
    let port = streaming_container().await;
    w.state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        w.podman.clone(),
        reqwest::Client::new(),
        Arc::new(move || Ok(port)),
    )));
    w.row_with(
        "live",
        "supertonic",
        |r| audiocpp_gguf::with_options(r, "supertonic", &[], &["offline", "streaming"]),
        |row| row.mode = "streaming".into(),
    )
    .await;
    let g = group(
        &w,
        "audio/live",
        "speech",
        json!({"settings": {"speech": {"input": "Hi.", "stream": true}}}),
    )
    .await;

    let mut resp =
        w.gw.client()
            .post(format!("{}/audio-lab/api/speech?group={}", w.gw, g.id))
            .json(
                &json!({"model": "audio/live", "input": "Hi.", "stream_format": "sse",
                          "response_format": "pcm"}),
            )
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    let mut got = Vec::new();
    while got.len() < FIRST.len() {
        got.extend_from_slice(&resp.chunk().await.unwrap().expect("the first event"));
    }
    assert!(got.starts_with(FIRST));
    // The page stops: the body is dropped mid-stream.
    drop(resp);

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let kept = loop {
        let v = read(&w, g.id).await;
        if v["runs"].as_array().is_some_and(|r| !r.is_empty()) {
            break round_trips::<LabGroupView>("stopped group", &v);
        }
        assert!(std::time::Instant::now() < deadline, "no run was kept: {v}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let run = &kept.runs[0];
    assert_eq!(run.http_status, 200);
    assert_eq!(run.error_code.as_deref(), Some("stopped"));
    assert!(
        run.error_message
            .as_deref()
            .unwrap()
            .starts_with("the page stopped the answer after "),
        "{:?}",
        run.error_message
    );
    assert_eq!(run.response_type.as_deref(), Some("text/event-stream"));
    let art = &run.artifacts[0];
    assert_eq!(art.name, "audio-live.sse");
    assert!(art.size >= FIRST.len() as u64, "{}", art.size);
    let bytes =
        w.gw.client()
            .get(format!("{}{}", w.gw, art.href))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
    assert!(bytes.starts_with(FIRST), "the bytes so far, as they came");
    assert_eq!(bytes.len() as u64, art.size);
    assert_eq!(
        files_in_artifacts(&w),
        vec![art.sha256.clone()],
        "no spool left"
    );

    // The cell is free of its claim once kept: a re-post is the store's 409.
    let again =
        w.gw.client()
            .post(format!("{}/audio-lab/api/speech?group={}", w.gw, g.id))
            .json(&json!({"model": "audio/live", "input": "Hi."}))
            .send()
            .await
            .unwrap();
    assert_eq!(again.status(), 409);
}

/// A recording is as large as it is: a 3 MB upload is kept whole as the
/// group's input (the lab reads the multipart under the route's own "no
/// body limit", not axum's default 2 MB).
#[tokio::test]
async fn an_upload_over_two_megabytes_is_kept_whole() {
    let w = lab_world().await;
    let g = group(&w, "audio/asr", "transcription", json!({})).await;
    let clip = wav_bytes(16_000, 1_600_000);
    assert!(clip.len() > 3_000_000);
    let part = reqwest::multipart::Part::bytes(clip.clone())
        .file_name("long.wav")
        .mime_str("audio/wav")
        .unwrap();
    let resp =
        w.gw.client()
            .post(format!(
                "{}/audio-lab/api/transcriptions?group={}",
                w.gw, g.id
            ))
            .multipart(
                reqwest::multipart::Form::new()
                    .text("model", "audio/asr")
                    .part("file", part),
            )
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    resp.bytes().await.unwrap();
    let kept = round_trips::<LabGroupView>("kept", &read(&w, g.id).await);
    assert_eq!(kept.artifacts.len(), 1, "{kept:?}");
    assert_eq!(kept.artifacts[0].size, clip.len() as u64);
}
