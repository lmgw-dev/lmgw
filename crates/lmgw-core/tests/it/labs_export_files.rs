//! The Image and Audio labs' files export (`format=files`): a scope is a zip
//! of the files under readable, unique, filesystem-safe names, a streamed
//! speech answer as a WAV and a transcription as its text, and a
//! `manifest.jsonl` naming each file with its run; a file missing from the
//! data dir is named in `MISSING.txt` and its manifest line; one group's one
//! file is answered as it is; the Decision lab makes no files.

use std::collections::BTreeSet;
use std::io::Read;

use lmgw_api_types::audio_lab::AudioLabForm;
use lmgw_api_types::image_lab::ImageLabForm;
use lmgw_api_types::lab_store::{
    LabCell, LabFileKind, LabFilesManifestLine, LabKind, LabMultipartFile, LabMultipartSummary,
    FILES_FORMAT, FILES_MANIFEST,
};
use lmgw_core::labs::record::{self, NewArtifact, NewGroup, NewRun};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::LabRunData;

use crate::common::{serve, Gw};
use crate::labs_store::{other_png, PNG};

/// A WAV of `pcm`: 16-bit mono at 24 kHz.
fn wav(pcm: &[u8]) -> Vec<u8> {
    let mut w = b"RIFF".to_vec();
    w.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0\xc0\x5d\0\0\x80\xbb\0\0\x02\0\x10\0data");
    w.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(pcm);
    w
}

fn declared(name: &str, bytes: &[u8], mime: Option<&str>) -> NewArtifact {
    NewArtifact::Bytes {
        name: name.into(),
        bytes: bytes::Bytes::copy_from_slice(bytes),
        declared_mime: mime.map(String::from),
        data_url_head: None,
    }
}

/// Record a group of `lab` asking `model` through `variant`, with `form`
/// and `inputs`, and its one run: `run` filled in, `outputs` its files.
async fn ask(
    state: &SharedState,
    lab: LabKind,
    title: &str,
    form: serde_json::Value,
    inputs: Vec<NewArtifact>,
    run: LabRunData,
    outputs: Vec<NewArtifact>,
) -> i64 {
    let text = form.to_string();
    let (id, _) = record::group(
        &state.db,
        &state.data_dir,
        NewGroup {
            lab,
            title: title.into(),
            plan: vec![LabCell {
                model: run.model_asked.clone(),
                variant: run.variant.clone(),
            }],
            inputs,
        },
        move |_| Ok((text, None)),
    )
    .await
    .unwrap();
    record::run(
        &state.db,
        &state.data_dir,
        NewRun {
            data: LabRunData {
                group_id: id,
                answered_by: run.model_asked.clone(),
                http_status: 200,
                headers: if run.headers.is_empty() {
                    "{}".into()
                } else {
                    run.headers.clone()
                },
                latency_ms: 12,
                ..run
            },
            outputs,
            replace_failed: false,
        },
    )
    .await
    .unwrap();
    id
}

fn speech_form(voice: &str, input: &str) -> serde_json::Value {
    let mut f = AudioLabForm {
        task: "tts".into(),
        ..Default::default()
    };
    f.settings.speech.voice = voice.into();
    f.settings.speech.input = input.into();
    serde_json::to_value(f).unwrap()
}

fn speech_run(model: &str, input: &str, response_type: &str) -> LabRunData {
    LabRunData {
        variant: "speech".into(),
        endpoint: "/v1/audio/speech".into(),
        model_asked: model.into(),
        request: serde_json::json!({ "model": model, "input": input }).to_string(),
        response_type: Some(response_type.into()),
        ..Default::default()
    }
}

/// The export's status, `Content-Type`, `Content-Disposition` and body.
async fn export(gw: &Gw, lab: &str, query: &str) -> (u16, String, String, Vec<u8>) {
    let resp = gw
        .client()
        .get(format!("{gw}/labs/api/{lab}/export?format=files{query}"))
        .send()
        .await
        .unwrap();
    let h = |n: &str| {
        resp.headers()
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    let (ct, cd) = (h("content-type"), h("content-disposition"));
    (
        resp.status().as_u16(),
        ct,
        cd,
        resp.bytes().await.unwrap().to_vec(),
    )
}

/// A zip's files by name, and its manifest's lines.
fn unzip(body: Vec<u8>) -> (Vec<(String, Vec<u8>)>, Vec<LabFilesManifestLine>) {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(body)).unwrap();
    let mut files = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes).unwrap();
        files.push((f.name().to_string(), bytes));
    }
    let manifest = files
        .iter()
        .find(|(n, _)| n == FILES_MANIFEST)
        .map(|(_, b)| String::from_utf8(b.clone()).unwrap())
        .expect("a manifest");
    let lines = manifest
        .lines()
        .map(|l| {
            let live: serde_json::Value = serde_json::from_str(l).unwrap();
            let typed: LabFilesManifestLine = serde_json::from_value(live.clone()).unwrap();
            assert_eq!(
                serde_json::to_value(&typed).unwrap(),
                live,
                "a lossless line"
            );
            typed
        })
        .collect();
    (files, lines)
}

/// Safe on any common filesystem: no path, no reserved character, no
/// leading dot, lower case, at most 255 bytes.
fn safe(name: &str) {
    assert!(name.len() <= 255, "{name}");
    assert!(!name.starts_with('.'), "{name}");
    assert_eq!(name, name.to_lowercase(), "{name}");
    assert!(
        name.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-')),
        "{name}"
    );
}

#[tokio::test]
async fn an_audio_lab_zip_names_its_files_readably_and_its_manifest_traces_each() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let clip = wav(b"\x01\x02\x03\x04");
    let text = "Grüße/aus: München? Hello there, this is a test.";
    let spoken = ask(
        &state,
        LabKind::Audio,
        "spoken",
        speech_form("af_heart", text),
        vec![],
        speech_run("audio/kokoro", text, "audio/wav"),
        vec![declared("audio-kokoro.wav", &clip, None)],
    )
    .await;
    // A stream of PCM, kept as it came; the gateway stated its rate.
    let sse = "event: speech.audio.delta\ndata: {\"audio\": \"QUJD\"}\n\n\
               data: {\"type\": \"speech.audio.delta\", \"audio\": \"REVG\"}\n\ndata: [DONE]\n\n";
    let mut streamed = speech_run("audio/kokoro", "Streamed words.", "text/event-stream");
    streamed.headers = r#"{"x-lmgw-sample-rate": "16000"}"#.into();
    let stream = ask(
        &state,
        LabKind::Audio,
        "streamed",
        speech_form("af_heart", "Streamed words."),
        vec![],
        streamed,
        vec![declared(
            "audio-kokoro.sse",
            sse.as_bytes(),
            Some("text/event-stream"),
        )],
    )
    .await;
    // A transcription: its text, named after the clip it transcribed.
    let mut asr = AudioLabForm {
        task: "asr".into(),
        ..Default::default()
    };
    asr.asr.file_name = "memo.wav".into();
    let summary = LabMultipartSummary {
        fields: vec![],
        files: vec![LabMultipartFile {
            name: "file".into(),
            filename: "memo.wav".into(),
            content_type: Some("audio/wav".into()),
            bytes: clip.len(),
            artifact: None,
        }],
    };
    let heard = ask(
        &state,
        LabKind::Audio,
        "memo.wav",
        serde_json::to_value(&asr).unwrap(),
        vec![declared("memo.wav", &clip, None)],
        LabRunData {
            variant: "transcription".into(),
            endpoint: "/v1/audio/transcriptions".into(),
            model_asked: "audio/parakeet".into(),
            request: serde_json::to_string(&summary).unwrap(),
            response: Some(r#"{"text": "Remember the milk."}"#.into()),
            response_type: Some("application/json".into()),
            ..Default::default()
        },
        vec![],
    )
    .await;
    // A separation: two stems, each named after its own id.
    let split = ask(
        &state,
        LabKind::Audio,
        // A task's title is its task.
        "separate",
        serde_json::to_value(AudioLabForm {
            task: "separate".into(),
            ..Default::default()
        })
        .unwrap(),
        vec![],
        LabRunData {
            variant: "task_run".into(),
            endpoint: "/v1/tasks/run".into(),
            model_asked: "audio/demucs".into(),
            request: r#"{"task": "separate"}"#.into(),
            response: Some(r#"{"named_audio_outputs": []}"#.into()),
            response_type: Some("application/json".into()),
            ..Default::default()
        },
        vec![
            declared("audio-demucs-vocals.wav", &wav(b"\x05\x06"), None),
            declared("audio-demucs-drums.wav", &wav(b"\x07\x08"), None),
        ],
    )
    .await;

    let (status, ct, cd, body) = export(&gw, "audio", "").await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(ct, "application/zip");
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    assert!(cd.contains(&format!("lmgw-lab-audio-{today}.zip")), "{cd}");
    let (files, lines) = unzip(body);
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    let id = |g: i64| format!("{g:06}");
    assert_eq!(
        names,
        [
            format!(
                "{}-audio-kokoro-af_heart-grüße-aus-münchen-hello-there-this-is-a-test.wav",
                id(spoken)
            ),
            format!("{}-audio-kokoro-af_heart-streamed-words.wav", id(stream)),
            format!("{}-audio-parakeet-asr-memo.wav.txt", id(heard)),
            format!("{}-audio-demucs-separate-vocals.wav", id(split)),
            format!("{}-audio-demucs-separate-drums.wav", id(split)),
            FILES_MANIFEST.to_string(),
        ],
        "oldest group first, each run's outputs in order, the manifest last"
    );
    let unique: BTreeSet<String> = names.iter().map(|n| n.to_lowercase()).collect();
    assert_eq!(unique.len(), names.len(), "{names:?}");
    for n in &names {
        safe(n);
    }

    let bytes = |name: &str| files.iter().find(|(n, _)| n == name).unwrap().1.clone();
    assert_eq!(bytes(&lines[0].file), clip, "a WAV as it was kept");
    let made = bytes(&lines[1].file);
    assert_eq!(&made[..4], b"RIFF");
    assert_eq!(&made[24..28], &16_000u32.to_le_bytes(), "the stated rate");
    assert_eq!(&made[44..], b"ABCDEF", "the stream's PCM in order");
    assert_eq!(bytes(&lines[2].file), b"Remember the milk.");

    assert_eq!(lines.len(), 5, "a line per file");
    for (line, (name, b)) in lines.iter().zip(&files) {
        assert_eq!(&line.file, name);
        assert_eq!(line.format, FILES_FORMAT);
        assert_eq!(line.lab, LabKind::Audio);
        assert_eq!(line.size, b.len() as u64, "{name}");
        assert!(!line.missing);
        assert!(!line.created_at.is_empty());
    }
    let l = &lines[0];
    assert_eq!(
        (l.group_id, l.model.as_str(), l.task.as_str(), l.kind),
        (spoken, "audio/kokoro", "tts", LabFileKind::Output)
    );
    assert_eq!(l.voice.as_deref(), Some("af_heart"));
    assert_eq!(l.text.as_deref(), Some(text), "the whole text");
    assert_eq!(l.parameters["input"], text, "the request as sent");
    assert_eq!(l.mime, "audio/wav");
    assert!(l.artifact_id.is_some() && l.sha256.is_some());
    assert_eq!(lines[1].kind, LabFileKind::Wav);
    assert_eq!(lines[1].mime, "audio/wav");
    let t = &lines[2];
    assert_eq!(t.kind, LabFileKind::Transcript);
    assert_eq!((t.voice.as_deref(), t.text.as_deref()), (None, None));
    assert_eq!(
        (t.artifact_id, t.parameters["files"][0]["filename"].as_str()),
        (None, Some("memo.wav"))
    );
    assert_eq!(t.inputs.len(), 1, "the clip it transcribed, named");
    assert_eq!(t.inputs[0].name, "memo.wav");
    assert_eq!(
        files.iter().filter(|(_, b)| *b == clip).count(),
        1,
        "only the spoken WAV has the clip's bytes; the upload is not in the zip"
    );

    // One model only.
    let (_, _, cd, body) = export(&gw, "audio", "&model=audio/demucs").await;
    assert!(cd.contains("lmgw-lab-audio-audio-demucs-"), "{cd}");
    let (files, lines) = unzip(body);
    assert_eq!(files.len(), 3, "two stems and the manifest");
    assert!(lines.iter().all(|l| l.group_id == split));

    // A file gone from the data dir: named, not carried.
    let gone = lines[0].sha256.clone().unwrap();
    std::fs::remove_file(lmgw_core::labs::artifacts::path(&state.data_dir, &gone).unwrap())
        .unwrap();
    let (status, _, _, body) = export(&gw, "audio", "").await;
    assert_eq!(status, 200);
    let (files, lines) = unzip(body);
    let missing_name = format!("{}-audio-demucs-separate-vocals.wav", id(split));
    assert!(!files.iter().any(|(n, _)| *n == missing_name));
    let note = String::from_utf8(
        files
            .iter()
            .find(|(n, _)| n == "MISSING.txt")
            .expect("a MISSING.txt")
            .1
            .clone(),
    )
    .unwrap();
    assert!(note.contains(&missing_name), "{note}");
    let line = lines.iter().find(|l| l.file == missing_name).unwrap();
    assert!(line.missing, "its line says so");
    assert_eq!(lines.len(), 5, "still a line per file");
}

#[tokio::test]
async fn one_group_with_one_file_is_that_file_and_with_more_a_zip() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let clip = wav(b"\x01\x02\x03\x04");
    let spoken = ask(
        &state,
        LabKind::Audio,
        "spoken",
        speech_form("af_heart", "Hello there."),
        vec![],
        speech_run("audio/kokoro", "Hello there.", "audio/wav"),
        vec![declared("audio-kokoro.wav", &clip, None)],
    )
    .await;
    let (status, ct, cd, body) = export(&gw, "audio", &format!("&group={spoken}")).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(ct, "audio/wav");
    let name = format!("{spoken:06}-audio-kokoro-af_heart-hello-there.wav");
    assert!(cd.contains(&format!("filename=\"{name}\"")), "{cd}");
    assert_eq!(body, clip, "the file as it is");

    // Raw PCM: a WAV of it with the form's format.
    let mut form = AudioLabForm {
        task: "tts".into(),
        ..Default::default()
    };
    form.settings.speech.input = "Raw.".into();
    form.settings.pcm.rate = "22050".into();
    let raw = ask(
        &state,
        LabKind::Audio,
        "raw",
        serde_json::to_value(form).unwrap(),
        vec![],
        speech_run("audio/kokoro", "Raw.", "audio/pcm"),
        vec![declared("audio-kokoro.pcm", b"\x10\x20", Some("audio/pcm"))],
    )
    .await;
    let (_, ct, cd, body) = export(&gw, "audio", &format!("&group={raw}")).await;
    assert_eq!(ct, "audio/wav");
    assert!(
        cd.contains(&format!("{raw:06}-audio-kokoro-tts-raw.wav")),
        "{cd}"
    );
    assert_eq!(&body[24..28], &22_050u32.to_le_bytes());
    assert_eq!(&body[44..], b"\x10\x20");

    // A picture.
    let image_form = |prompt: &str| {
        let mut f = ImageLabForm::default();
        f.form.prompt = prompt.into();
        serde_json::to_value(f).unwrap()
    };
    let image_run = |model: &str| LabRunData {
        variant: "generate".into(),
        endpoint: "/v1/images/generations".into(),
        model_asked: model.into(),
        request: r#"{"prompt": "x"}"#.into(),
        response: Some(r#"{"data": []}"#.into()),
        response_type: Some("application/json".into()),
        ..Default::default()
    };
    let one = ask(
        &state,
        LabKind::Image,
        "a red fox",
        image_form("A red fox in snow"),
        vec![],
        image_run("image/sdxl"),
        vec![declared("sdxl-0.png", PNG, None)],
    )
    .await;
    let (_, ct, cd, body) = export(&gw, "image", &format!("&group={one}")).await;
    assert_eq!(ct, "image/png");
    assert!(
        cd.contains(&format!(
            "{one:06}-image-sdxl-generate-a-red-fox-in-snow.png"
        )),
        "{cd}"
    );
    assert_eq!(body, PNG);

    // Two pictures: a zip, each named by its place.
    let two = ask(
        &state,
        LabKind::Image,
        "two",
        image_form("Two cats"),
        vec![],
        image_run("image/sdxl"),
        vec![
            declared("sdxl-0.png", PNG, None),
            declared("sdxl-1.png", &other_png(), None),
        ],
    )
    .await;
    let (_, ct, cd, body) = export(&gw, "image", &format!("&group={two}")).await;
    assert_eq!(ct, "application/zip");
    assert!(cd.contains(&format!("lmgw-lab-image-group-{two}-")), "{cd}");
    let (files, lines) = unzip(body);
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            format!("{two:06}-image-sdxl-generate-two-cats-1.png").as_str(),
            &format!("{two:06}-image-sdxl-generate-two-cats-2.png"),
            FILES_MANIFEST,
        ]
    );
    assert_eq!(lines[1].text.as_deref(), Some("Two cats"));
    assert_eq!(lines[1].task, "generate");

    // The one file gone: the zip says so rather than a broken answer.
    let sha = lmgw_core::labs::artifacts::sha256_hex(&clip);
    std::fs::remove_file(lmgw_core::labs::artifacts::path(&state.data_dir, &sha).unwrap()).unwrap();
    let (status, ct, _, body) = export(&gw, "audio", &format!("&group={spoken}")).await;
    assert_eq!((status, ct.as_str()), (200, "application/zip"));
    let (files, lines) = unzip(body);
    assert!(files.iter().any(|(n, _)| n == "MISSING.txt"));
    assert_eq!(lines.len(), 1);
    assert!(lines[0].missing);
}

#[tokio::test]
async fn the_decision_lab_makes_no_files() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let (status, _, _, body) = export(&gw, "decision", "").await;
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        v.to_string().contains("jsonl or csv"),
        "it says what to ask for instead: {v}"
    );
    // The media labs keep their JSONL and CSV on the API.
    let resp = gw
        .client()
        .get(format!("{gw}/labs/api/audio/export?format=csv"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
