//! The review of security round B (document-conversion design §14): every
//! refusal of the documents and docling routes is bounded where it becomes
//! an answer, and a job's stored error too, whatever the message formats
//! (S1); a transcription cut off mid-call counts itself under the model
//! that answered (N3).

use std::time::Duration;

use lmgw_core::store;
use serde_json::{json, Value};

use crate::docling_serve::{b64, gateway};

/// What an answer may take at most: the bounded message and its JSON.
const ANSWER_MAX: usize = 8 << 10;

#[tokio::test]
async fn every_refusal_is_bounded_where_it_becomes_an_answer() {
    let (state, gw) = gateway().await;
    let z = "Z".repeat(5 << 20);
    let file = json!([{"name": "a.txt", "data_base64": b64(b"one")}]);
    let mut field = serde_json::Map::new();
    field.insert("files".into(), file.clone());
    field.insert(format!("q`{z}"), json!(1));
    for (what, body) in [
        (
            "a backtick in output",
            json!({"files": file, "output": format!("a`{z}")}),
        ),
        (
            "a quote in ocr_languages",
            json!({"files": file, "ocr_languages": format!("x\"{z}")}),
        ),
        ("an unknown field's name", Value::Object(field)),
        ("files as a string", json!({"files": z})),
    ] {
        let r = gw
            .client()
            .post(format!("{gw}/v1/documents/convert"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400, "{what}");
        let text = r.text().await.unwrap();
        assert!(text.len() < ANSWER_MAX, "{what}: {} bytes", text.len());
    }
    // docling's body error.
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/source"))
        .json(&json!({"sources": z}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 422);
    let text = r.text().await.unwrap();
    assert!(text.len() < ANSWER_MAX, "docling: {} bytes", text.len());
    // A job id in `job_not_found`, as long as a request line may be.
    let id = "j".repeat(60 << 10);
    let r = gw
        .client()
        .get(format!("{gw}/v1/documents/jobs/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
    let text = r.text().await.unwrap();
    assert!(text.len() < ANSWER_MAX, "job: {} bytes", text.len());
    assert!(text.contains("ERROR_MSG_MAX_BYTES"), "{text}");
    // A job's stored error is bounded as a usage row's is.
    let jid = format!("cj_{}", hex::encode(rand::random::<[u8; 16]>()));
    store::convert_jobs::insert_job(
        &state.db,
        &store::convert_jobs::NewJob {
            id: &jid,
            key_id: None,
            origin: "native",
            created_at: chrono::Utc::now(),
            input_bytes: 0,
            warnings: "[]",
            adapter: None,
        },
        &[store::convert_jobs::NewJobFile {
            idx: 0,
            name: "a.txt".into(),
            bytes: 1,
            state: "pending",
        }],
    )
    .await
    .unwrap();
    let long = "x".repeat(1 << 20);
    store::convert_jobs::file_done(&state.db, &jid, 0, "failed", 0, Some(&long))
        .await
        .unwrap();
    store::convert_jobs::finish_job(&state.db, &jid, "failed", "failed", "[]", Some(&long))
        .await
        .unwrap();
    let max = store::error_msg::ERROR_MSG_MAX_BYTES + 64;
    let job = store::convert_jobs::get_job(&state.db, &jid)
        .await
        .unwrap()
        .unwrap();
    assert!(job.error.as_deref().unwrap().len() <= max);
    let files = store::convert_jobs::job_files(&state.db, &jid)
        .await
        .unwrap();
    assert!(files[0].error.as_deref().unwrap().len() <= max);
}

/// N3: under the GPU hold the local speech-to-text model's fallback has the
/// call; a message's attachment cut while that call is in flight counts it
/// under the fallback, as the attachment before it (answered) does.
#[tokio::test]
async fn a_cut_transcription_counts_under_the_model_that_answered() {
    use crate::chat_voice_dictation::{asr_row, cloud_alias, tweak, world};
    use crate::documents_pictures_clock::mail;
    use crate::support::gpu_world::GIB;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (g, gw) = world(10 * GIB, 2).await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "fast example"})))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"text": "slow example"}))
                .set_delay(Duration::from_secs(6)),
        )
        .with_priority(2)
        .mount(&mock)
        .await;
    let _cloud = cloud_alias(&g.state, "cloud-ears", mock).await;
    asr_row(&g, "gpu-ears", false, Some("cloud-ears")).await;
    g.state
        .doc_tools
        .set_search_path(crate::documents_media::path_with_reader());
    tweak(&g.state, |s| {
        s.hold.active = true;
        s.documents.asr_alias = "audio/gpu-ears".into();
    })
    .await;
    let wav = crate::chat_attach_kinds::wav();
    let message = mail(&[
        ("first.wav", "audio/wav", wav.clone()),
        ("second.wav", "audio/wav", wav),
    ]);
    let r = gw
        .client()
        .post(format!("{gw}/v1/documents/convert"))
        .json(&json!({
            "files": [{"name": "example.eml", "data_base64": b64(&message)}],
            "document_timeout": 1.5,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let a: Value = r.json().await.unwrap();
    let d = &a["documents"][0];
    assert!(
        d["content"].as_str().unwrap().contains("fast example"),
        "{d}"
    );
    let models = d["stats"]["models"].as_array().unwrap();
    assert_eq!(models.len(), 1, "one model had both calls: {d}");
    assert_eq!(
        (
            models[0]["alias"].as_str(),
            models[0]["answered_for"].as_str(),
            models[0]["calls"].as_u64()
        ),
        (Some("cloud-ears"), Some("audio/gpu-ears"), Some(2)),
        "{d}"
    );
}
