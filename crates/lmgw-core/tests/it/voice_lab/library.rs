//! Writes into the voice library at once (saves, an upload), what an
//! overwrite replaces, the transcript as the engine was sent it, and a
//! records file lmgw cannot read.

use lmgw_api_types::voice_lab::VoiceRound;
use serde_json::{json, Value};

use super::{lab_world, post};
use crate::support::audio_world::World;

fn voices(w: &World) -> std::path::PathBuf {
    w.models.path().join("voices")
}

async fn round(w: &World, count: u32) -> VoiceRound {
    let (status, body) = post(
        w,
        "/voice-lab/api/rounds",
        json!({"alias": "audio/design", "language": "en", "count": count, "seed": 1,
               "description": "a warm, calm voice"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_value(body).unwrap()
}

async fn save(w: &World, round: &str, candidate: u32, name: &str, overwrite: bool) -> (u16, Value) {
    post(
        w,
        "/voice-lab/api/samples",
        json!({"round": round, "candidate": candidate, "name": name, "overwrite": overwrite}),
    )
    .await
}

#[tokio::test]
async fn saves_at_once_neither_lose_a_line_nor_both_take_one_name() {
    let w = lab_world().await;
    let r = round(&w, 2).await;

    // One name, eight saves at once: the name check and the write are one
    // step, so exactly one gets it.
    let same =
        futures::future::join_all((0..8).map(|k| save(&w, &r.id, 1 + k % 2, "anna", false))).await;
    let ok = same.iter().filter(|(s, _)| *s == 200).count();
    let taken = same
        .iter()
        .filter(|(s, b)| *s == 409 && b["code"] == "name_taken")
        .count();
    assert_eq!((ok, taken), (1, 7), "{same:?}");

    // Eight names at once: every transcript line and every record is kept.
    let names: Vec<String> = (0..8).map(|k| format!("v{k}")).collect();
    let all = futures::future::join_all(names.iter().map(|n| save(&w, &r.id, 1, n, false))).await;
    assert!(all.iter().all(|(s, _)| *s == 200), "{all:?}");
    let index = std::fs::read_to_string(voices(&w).join("prompt_text")).unwrap();
    for n in names.iter().chain(std::iter::once(&"anna".to_string())) {
        assert!(
            index.lines().any(|l| l.starts_with(&format!("{n}|"))),
            "{n}: {index}"
        );
    }
    let records =
        lmgw_core::voice_lab::provenance::read(&lmgw_core::voice_lab::lab_dir(&w.state)).unwrap();
    assert_eq!(records.len(), 9);
    // No temporary file is left beside the clips.
    let left: Vec<String> = std::fs::read_dir(voices(&w))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert_eq!(left, Vec::<String>::new());
}

#[tokio::test]
async fn an_overwrite_replaces_only_audio_and_writes_the_new_clip_first() {
    let w = lab_world().await;
    let r = round(&w, 1).await;
    std::fs::create_dir_all(voices(&w)).unwrap();
    // Notes beside a voice are no voice: the name is free.
    std::fs::write(voices(&w).join("anna.txt"), b"my notes").unwrap();
    std::fs::write(voices(&w).join("anna.json"), b"{}").unwrap();
    let (status, body) = save(&w, &r.id, 1, "anna", false).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["replaced"], json!([]));

    std::fs::write(voices(&w).join("anna.mp3"), b"ID3 an upload").unwrap();
    let (status, body) = save(&w, &r.id, 1, "anna", false).await;
    assert_eq!(status, 409, "{body}");
    assert!(!body["message"].as_str().unwrap().contains("anna.txt"));

    let (status, body) = save(&w, &r.id, 1, "anna", true).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["replaced"], json!(["anna.mp3", "anna.wav"]));
    assert!(!voices(&w).join("anna.mp3").exists());
    assert!(voices(&w).join("anna.wav").exists());
    assert_eq!(
        std::fs::read(voices(&w).join("anna.txt")).unwrap(),
        b"my notes"
    );
    assert!(voices(&w).join("anna.json").exists());
}

#[tokio::test]
async fn the_transcript_is_the_text_the_engine_was_sent() {
    let w = lab_world().await;
    let asked = "Hello there [laughs] my friend.";
    let (status, body) = post(
        &w,
        "/voice-lab/api/rounds",
        json!({"alias": "audio/design", "text": asked, "count": 1, "description": "calm"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let r: VoiceRound = serde_json::from_value(body).unwrap();
    let sent = w.sent().await[0]["input"].as_str().unwrap().to_string();
    assert_ne!(sent, asked, "shaping changed the text for this model");
    assert_eq!(r.text, asked);
    assert_eq!(r.sent_text.as_deref(), Some(sent.as_str()));

    let (status, saved) = save(&w, &r.id, 1, "laugh", false).await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["transcript"], sent);
    assert_eq!(saved["provenance"]["text"], sent);
    assert_eq!(saved["provenance"]["asked_text"], asked);
    let index = std::fs::read_to_string(voices(&w).join("prompt_text")).unwrap();
    assert_eq!(index, format!("laugh|{sent}\n"));

    // Unshaped, the round says nothing more.
    let plain = round(&w, 1).await;
    assert_eq!(plain.sent_text, None);
}

#[tokio::test]
async fn a_records_file_lmgw_cannot_read_refuses_the_save_before_anything_is_written() {
    let w = lab_world().await;
    let r = round(&w, 1).await;
    let lab = lmgw_core::voice_lab::lab_dir(&w.state);
    std::fs::write(lab.join("samples.json"), b"{broken").unwrap();
    let (status, body) = save(&w, &r.id, 1, "anna", false).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "unreadable_file");
    assert!(body["message"].as_str().unwrap().contains("samples.json"));
    assert!(!voices(&w).join("anna.wav").exists());
    assert_eq!(std::fs::read(lab.join("samples.json")).unwrap(), b"{broken");
    let (status, list) = super::get(&w, "/voice-lab/api/samples").await;
    assert_eq!(status, 200);
    assert!(list["records_error"]
        .as_str()
        .unwrap()
        .contains("samples.json"));
}

/// WP13: a candidate is generated audio the owner keeps, so the route sends
/// nosniff and the sandbox policy on the 200, the 206 and the 416.
#[tokio::test]
async fn a_candidate_is_served_sandboxed_on_200_206_and_416() {
    use crate::support::stored::assert_sandboxed;
    let w = lab_world().await;
    let first = round(&w, 1).await;
    let url = format!("{}{}", w.gw, first.candidates[0].url);
    for (range, want) in [
        (None, 200),
        (Some("bytes=0-9"), 206),
        (Some("bytes=99999999-"), 416),
    ] {
        let mut req = w.gw.client().get(&url);
        if let Some(r) = range {
            req = req.header("range", r);
        }
        let r = req.send().await.unwrap();
        assert_eq!(r.status().as_u16(), want, "{range:?}");
        assert_sandboxed(r.headers(), &format!("{range:?}"));
    }
}
