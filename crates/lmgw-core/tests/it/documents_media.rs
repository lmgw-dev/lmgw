//! Audio and video through `POST /v1/documents/convert` (document-conversion
//! design §4.2, §7.5, WP7) against a mock speech-to-text upstream: nothing
//! is transcribed until a model is chosen, then the file is, as the calling
//! key, with its own usage row; a video's sound is taken out by ffmpeg
//! first (skipped without it). Chat attachments and knowledge bases read an
//! old Word file through the table (skipped without LibreOffice).

use serde_json::{json, Value};

use crate::chat_attach_kinds::{chat_mock, setup, stt_mock, transcriptions, wav};
use crate::common::Gw;

const PATH: &str = "/v1/documents/convert";

async fn post_file(gw: &Gw, name: &str, bytes: Vec<u8>, output: &str) -> (u16, Value) {
    let form = reqwest::multipart::Form::new()
        .part(
            "file",
            reqwest::multipart::Part::bytes(bytes).file_name(name.to_string()),
        )
        .text("output", output.to_string());
    let r = gw
        .client()
        .post(format!("{gw}{PATH}"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

/// Whether lmgw's reader beside the tests confines a program here.
pub(crate) fn confines() -> bool {
    std::env::split_paths(&lmgw_core::extract::convert::reader::own_dirs())
        .map(|d| d.join("lmgw-doc-reader"))
        .find(|p| p.is_file())
        .and_then(|p| {
            std::process::Command::new(p)
                .args(["run", "--probe"])
                .output()
                .ok()
        })
        .is_some_and(|o| o.status.success())
}

/// `PATH`, and where lmgw's reader is built beside the tests.
pub(crate) fn path_with_reader() -> std::ffi::OsString {
    let mut dirs: Vec<std::path::PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    dirs.extend(std::env::split_paths(
        &lmgw_core::extract::convert::reader::own_dirs(),
    ));
    std::env::join_paths(dirs).unwrap()
}

async fn choose(gw: &Gw, alias: &str) -> (u16, Value) {
    let r = gw
        .client()
        .post(format!("{gw}/api/op/settings_set"))
        .json(&json!({ "documents_asr_alias": alias }))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn audio_is_transcribed_only_once_a_model_is_chosen() {
    let (chat, stt) = (
        chat_mock().await,
        stt_mock("hello from the example recording").await,
    );
    let (state, gw) = setup(&chat, &stt).await;
    let (status, a) = post_file(&gw, "example.wav", wav(), "markdown").await;
    assert_eq!(status, 415, "{a}");
    assert_eq!(a["documents"][0]["error"]["code"], "model_not_chosen");
    assert_eq!(transcriptions(&stt).await, 0);
    // A model that does not transcribe is refused by the setting.
    let (status, e) = choose(&gw, "plain").await;
    assert_eq!(status, 400, "{e}");
    let (status, e) = choose(&gw, "my-asr").await;
    assert_eq!(status, 200, "{e}");
    assert_eq!(state.snapshot().settings.documents.asr_alias, "my-asr");
    let (status, a) = post_file(&gw, "example.wav", wav(), "markdown").await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    assert_eq!(d["format"], "audio");
    assert_eq!(d["content"], "hello from the example recording");
    assert_eq!(d["stats"]["parts_by_source"]["asr"], 1);
    assert_eq!(d["stats"]["model_calls"], 1);
    assert_eq!(transcriptions(&stt).await, 1);
    // json: the transcript's blocks are the model's.
    let (status, a) = post_file(&gw, "example.wav", wav(), "json").await;
    assert_eq!(status, 200, "{a}");
    let part = &a["documents"][0]["structure"][0];
    assert_eq!(part["source"], "asr", "{part}");
    assert_eq!(
        part["blocks"][0]["text"],
        "hello from the example recording"
    );
    // The call wrote its own usage row, class audio, beside the request's.
    let rows = lmgw_core::store::query_logs(
        &state.db,
        &lmgw_core::store::LogFilter {
            alias: Some("my-asr".into()),
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows.iter().all(|r| r.class.as_deref() == Some("audio")),
        "{rows:?}"
    );
}

#[tokio::test]
async fn a_videos_sound_is_taken_out_by_ffmpeg_and_transcribed() {
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let mp4 = t.path().join("example.mp4");
    // A second of tone and a black picture; the index is written at the
    // end, as most cameras write it, so the file cannot be read from a pipe.
    let made = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=32x32:d=1",
            "-shortest",
            "-c:v",
            "mpeg4",
            "-c:a",
            "aac",
        ])
        .arg(&mp4)
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    let (chat, stt) = (chat_mock().await, stt_mock("a tone").await);
    let (state, gw) = setup(&chat, &stt).await;
    // ffmpeg runs confined where lmgw's reader beside the tests can confine.
    state.doc_tools.set_search_path(path_with_reader());
    assert_eq!(choose(&gw, "my-asr").await.0, 200);
    let (status, a) = post_file(&gw, "example.mp4", std::fs::read(&mp4).unwrap(), "markdown").await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["format"], "video", "{d}");
    assert_eq!(d["content"], "a tone");
    assert!(
        a["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == "ffmpeg"),
        "{a}"
    );
    // What went up is 16 kHz mono WAV with its lengths written in.
    let sent = stt.received_requests().await.unwrap();
    let up = sent
        .iter()
        .find(|r| r.url.path() == "/v1/audio/transcriptions")
        .unwrap();
    let body = &up.body;
    let at = body.windows(4).position(|w| w == b"RIFF").unwrap();
    assert_eq!(&body[at + 8..at + 12], b"WAVE");
    assert_ne!(&body[at + 4..at + 8], &[0xff, 0xff, 0xff, 0xff]);
}

/// An old Word file, made here by LibreOffice, in Chat and in a knowledge
/// base: read through the conversion's table, where Chat's own sniffer
/// refused it before.
#[tokio::test]
async fn chat_and_knowledge_bases_read_an_old_word_file_through_the_table() {
    let Ok(_) = std::process::Command::new("soffice")
        .arg("--version")
        .output()
    else {
        eprintln!("skipped: LibreOffice (soffice) is not on PATH");
        return;
    };
    if !confines() {
        eprintln!("skipped: LibreOffice runs only confined, and nothing confines here");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("source.rtf");
    std::fs::write(&src, b"{\\rtf1\\ansi An example memo\\par}").unwrap();
    let made = std::process::Command::new("soffice")
        .arg("--headless")
        .arg(format!(
            "-env:UserInstallation=file://{}",
            t.path().join("maker").display()
        ))
        .args(["--convert-to", "doc", "--outdir"])
        .arg(t.path())
        .arg(&src)
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    let doc = std::fs::read(t.path().join("source.doc")).unwrap();
    let (chat, stt) = (chat_mock().await, stt_mock("never used").await);
    let (state, gw) = setup(&chat, &stt).await;
    // LibreOffice runs confined by lmgw's reader, found beside the tests.
    state.doc_tools.set_search_path(path_with_reader());
    let tid = crate::chat_attach_kinds::new_thread(&gw, "plain", false).await;
    let r = crate::chat_attach_kinds::upload(&gw, tid, "memo.doc", doc.clone()).await;
    assert_eq!(r.status(), 200);
    let a: Value = r.json().await.unwrap();
    assert_eq!(a["kind"], "office", "{a}");
    assert_eq!(a["meta"]["format"], "doc", "{a}");
    let text = gw
        .client()
        .get(format!("{gw}/chat/api/attachments/{}/text", a["id"]))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.contains("An example memo"), "{text}");
    // A knowledge base takes it, reads it at ingest and chunks its text.
    let mock = wiremock::MockServer::start().await;
    crate::knowledge::mount(&mock).await;
    let state = crate::knowledge::setup(&mock).await;
    state.doc_tools.set_search_path(path_with_reader());
    let gw = crate::common::serve(state.clone()).await;
    let id = crate::knowledge::ingested(
        &state,
        &gw,
        json!({"name": "Memos", "embed_alias": "embed-model"}),
        &[("memo.doc", doc)],
    )
    .await;
    let (_, d) = crate::knowledge::get(&gw, &format!("/api/knowledge/bases/{id}")).await;
    let f = &d["files"][0];
    assert_eq!(f["status"], "ready", "{d}");
    assert_eq!(f["kind"], "office", "{f}");
    assert!(f["chunk_count"].as_i64().unwrap() > 0, "{f}");
    crate::knowledge::cleanup(&state);
}
