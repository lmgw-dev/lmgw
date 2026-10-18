//! The Audio lab's kept hand-offs, beyond their routes (labs-store design
//! §5.1): a request naming another model than its group's is refused before
//! it is sent; a task's base64 audio is kept as files, its stored answer
//! naming them; an upload's filename-less file is a file, its text parts
//! fields; a run is kept when its client goes while it is being written.

use std::time::Duration;

use base64::Engine as _;
use lmgw_api_types::lab_store::{LabGroupView, LabMultipartSummary};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::kept::{files_in_artifacts, group, read};
use super::lab_world;
use crate::common::round_trips;
use crate::support::audio_world::wav_bytes;

#[tokio::test]
async fn a_hand_off_naming_another_model_is_refused_before_it_is_sent() {
    let w = lab_world().await;
    let g = group(&w, "audio/tts", "speech", json!({})).await;
    let post = |body: Value| {
        let w = &w;
        async move {
            w.gw.client()
                .post(format!("{}/audio-lab/api/speech?group={}", w.gw, g.id))
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };
    let resp = post(json!({"model": "audio/other", "input": "hi"})).await;
    assert_eq!(resp.status(), 400);
    let e: Value = resp.json().await.unwrap();
    assert_eq!(e["error"]["code"], "wrong_model", "{e}");
    let message = e["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("'audio/other'") && message.contains("'audio/tts'"),
        "both named: {message}"
    );
    let resp = post(json!({"input": "hi"})).await;
    assert_eq!(resp.status(), 400, "no model is another model too");
    assert!(w.sent().await.is_empty(), "nothing was sent");

    // The group was not taken by the refusal: the right request is its run.
    let resp = post(json!({"model": "audio/tts", "input": "hi"})).await;
    assert_eq!(resp.status(), 200);
    resp.bytes().await.unwrap();
    let kept = round_trips::<LabGroupView>("kept", &read(&w, g.id).await);
    assert_eq!(kept.runs[0].model_asked, "audio/tts");

    // An upload's `model` field is checked the same way.
    let g = group(&w, "audio/asr", "transcription", json!({})).await;
    let part = reqwest::multipart::Part::bytes(wav_bytes(16_000, 800))
        .file_name("c.wav")
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
                    .text("model", "audio/tts")
                    .part("file", part),
            )
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 400);
    let e: Value = resp.json().await.unwrap();
    assert_eq!(e["error"]["code"], "wrong_model", "{e}");
}

#[tokio::test]
async fn a_tasks_audio_is_kept_as_files_and_its_answer_names_them() {
    let w = lab_world().await;
    let clip = wav_bytes(24_000, 4_800);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&clip);
    Mock::given(method("POST"))
        .and(path("/v1/tasks/run"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "audio": b64,
            "sample_rate": 24000,
            "named_audio_outputs": [{"id": "vocals", "audio": b64, "sample_rate": 24000}],
            "timing": {"wall_ms": 12.0}
        })))
        .mount(&w.container)
        .await;
    let g = group(&w, "audio/tts", "task_run", json!({"generic": true})).await;
    let resp =
        w.gw.client()
            .post(format!("{}/audio-lab/api/tasks/run?group={}", w.gw, g.id))
            .json(&json!({"model": "audio/tts", "request": {"text": "hi"}}))
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    let relayed: Value = resp.json().await.unwrap();
    assert_eq!(relayed["audio"], b64, "the page gets the answer as it came");

    let kept = round_trips::<LabGroupView>("kept", &read(&w, g.id).await);
    let run = &kept.runs[0];
    let text = run.response.as_deref().unwrap();
    assert!(!text.contains(&b64), "no base64 audio in the database");
    let said: Value = serde_json::from_str(text).unwrap();
    assert_eq!(run.artifacts.len(), 2, "{run:?}");
    let (a, b) = (&run.artifacts[0], &run.artifacts[1]);
    assert_eq!(said["audio"], format!("lab-artifact:{}", a.id));
    assert_eq!(
        said["named_audio_outputs"][0]["audio"],
        format!("lab-artifact:{}", b.id)
    );
    assert_eq!(said["timing"]["wall_ms"], 12.0, "the rest as it was");
    assert_eq!(a.name, "audio-tts.wav");
    assert_eq!(b.name, "audio-tts-vocals.wav");
    assert_eq!(a.mime, "audio/wav");
    assert_eq!(a.sha256, b.sha256, "one file for the same bytes");
    let back =
        w.gw.client()
            .get(format!("{}{}", w.gw, a.href))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .to_vec();
    assert_eq!(back, clip);
    assert_eq!(files_in_artifacts(&w), vec![a.sha256.clone()]);
}

#[tokio::test]
async fn a_nameless_file_part_is_kept_as_a_file_and_a_text_part_as_a_field() {
    let w = lab_world().await;
    let g = group(&w, "audio/asr", "transcription", json!({})).await;
    let clip = wav_bytes(16_000, 800);
    // A recording sent with a type and no filename: no text in the database.
    let part = reqwest::multipart::Part::bytes(clip.clone())
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
                    .text("prompt", "names: Zoë")
                    .part("file", part),
            )
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    resp.bytes().await.unwrap();
    let kept = round_trips::<LabGroupView>("kept", &read(&w, g.id).await);
    let sent: LabMultipartSummary = serde_json::from_str(&kept.runs[0].request).unwrap();
    assert_eq!(sent.field("model"), Some("audio/asr"));
    assert_eq!(sent.field("prompt"), Some("names: Zoë"));
    assert_eq!(sent.field("file"), None, "the recording is no text field");
    assert_eq!(sent.files.len(), 1, "{sent:?}");
    assert_eq!(sent.files[0].name, "file");
    assert_eq!(sent.files[0].filename, "file", "named after its field");
    assert_eq!(kept.artifacts[0].size, clip.len() as u64);
    assert_eq!(kept.artifacts[0].mime, "audio/wav");
}

/// The page goes while the run is being written (here: while the record
/// waits for the artifacts lock): the run is recorded on a task of its own
/// and kept all the same.
#[tokio::test]
async fn a_speech_run_is_kept_when_its_client_goes_while_it_is_written() {
    let w = lab_world().await;
    let g = group(&w, "audio/tts", "speech", json!({})).await;
    let held = lmgw_core::labs::artifacts::hold().await;
    let mut resp =
        w.gw.client()
            .post(format!("{}/audio-lab/api/speech?group={}", w.gw, g.id))
            .json(&json!({"model": "audio/tts", "input": "hi"}))
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    // The answer's last byte is withheld until its run is in, so the client
    // waits; once the speech is spooled its record follows at once, and
    // stops at the lock.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !files_in_artifacts(&w)
        .iter()
        .any(|f| f.starts_with("spool-"))
    {
        assert!(std::time::Instant::now() < deadline, "nothing was spooled");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let early = tokio::time::timeout(Duration::from_millis(300), resp.chunk()).await;
    assert!(early.is_err(), "the last byte waits for the run: {early:?}");
    // The page goes; the gateway sees it once it writes again.
    drop(resp);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let v = read(&w, g.id).await;
    assert_eq!(v["in_flight"], true, "the record holds the claim: {v}");
    assert!(v["runs"].as_array().unwrap().is_empty());
    drop(held);

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let kept = loop {
        let v = read(&w, g.id).await;
        if v["in_flight"] == false {
            break round_trips::<LabGroupView>("kept", &v);
        }
        assert!(std::time::Instant::now() < deadline, "never in: {v}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let run = &kept.runs[0];
    assert_eq!(run.http_status, 200);
    assert_eq!(run.error_code, None, "the whole answer was in: {run:?}");
    assert_eq!(run.artifacts[0].mime, "audio/wav");
    assert_eq!(run.artifacts[0].size, wav_bytes(24_000, 480).len() as u64);
    assert_eq!(
        files_in_artifacts(&w),
        vec![run.artifacts[0].sha256.clone()]
    );
}
