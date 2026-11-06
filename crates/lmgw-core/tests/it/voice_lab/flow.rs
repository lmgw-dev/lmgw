use lmgw_api_types::voice_lab::{
    VoiceModelFacts, VoiceRound, VoiceRounds, VoiceSampleSaved, VoiceSamples, VoiceSentences,
};
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use lmgw_core::voice_lab::sentences::TABLE;
use lmgw_core::voice_lab::wav;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{bytes, get, lab_world, padded_tone, post};
use crate::common::round_trips;
use crate::support::audio_world::{world, World};
use crate::support::audiocpp_gguf;

/// A round of `count` on the design row, from `seed`, described.
async fn round(w: &World, count: u32, seed: u32) -> Value {
    let (status, body) = post(
        w,
        "/voice-lab/api/rounds",
        json!({"alias": "audio/design", "language": "en", "count": count, "seed": seed,
               "description": "a warm, calm voice"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body
}

fn voices(w: &World) -> std::path::PathBuf {
    w.models.path().join("voices")
}

fn prompt_text(w: &World) -> String {
    std::fs::read_to_string(voices(w).join("prompt_text")).unwrap_or_default()
}

#[tokio::test]
async fn a_round_renders_its_candidates_with_their_own_seeds_and_is_kept() {
    let w = lab_world().await;
    let (status, facts) = get(&w, "/voice-lab/api/model?alias=audio/design&language=en").await;
    assert_eq!(status, 200, "{facts}");
    round_trips::<VoiceModelFacts>("voice lab facts", &facts);
    assert_eq!(facts["description"], "voice_design");
    assert_eq!(facts["description_required"], true);
    assert_eq!(facts["takes_voice"], false);
    assert_eq!(facts["reads_seed"], true);
    assert_eq!(facts["local"], true);
    assert_eq!(facts["row"]["family"], "qwen3_tts");
    assert_eq!(facts["held"], false);

    // A voice-design model speaks only with a description: refused before
    // anything starts.
    let (status, body) = post(
        &w,
        "/voice-lab/api/rounds",
        json!({"alias": "audio/design", "language": "en"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "instructions_required", "{body}");
    assert_eq!(w.runs(), 0);

    let r = round(&w, 3, 100).await;
    let typed: VoiceRound = round_trips("voice round", &r);
    assert_eq!(typed.answered_by, "audio/design");
    assert_eq!(typed.fallback_reason, None);
    assert!(typed.seeds_sent);
    assert_eq!(typed.text, TABLE[0].sentence);
    assert_eq!(typed.asked, 3);
    assert_eq!(typed.stopped, None);
    let seeds: Vec<Option<u32>> = typed.candidates.iter().map(|c| c.seed).collect();
    assert_eq!(seeds, [Some(100), Some(101), Some(102)]);
    assert!(typed
        .candidates
        .iter()
        .all(|c| c.duration_ms == Some(1800) && c.mime == "audio/wav"));
    let sent = w.sent().await;
    assert_eq!(sent.len(), 3, "one request per candidate");
    for (body, seed) in sent.iter().zip([100, 101, 102]) {
        assert_eq!(body["seed"], seed, "{body}");
        assert!(body.to_string().contains("a warm, calm voice"), "{body}");
        assert_eq!(body["input"], TABLE[0].sentence);
    }

    let (status, list) = get(&w, "/voice-lab/api/rounds").await;
    assert_eq!(status, 200);
    let list: VoiceRounds = round_trips("voice rounds", &list);
    assert_eq!(list.rounds, std::slice::from_ref(&typed));
    assert!(list.bytes >= 3 * padded_tone().len() as u64);

    let (status, ty, audio) = bytes(&w, &typed.candidates[1].url).await;
    assert_eq!(status, 200);
    assert_eq!(ty.as_deref(), Some("audio/wav"));
    assert_eq!(audio, padded_tone());
    // A player's resume (web::ranged): the rest from byte 100, as 206.
    let part =
        w.gw.client()
            .get(format!("{}{}", w.gw, typed.candidates[1].url))
            .header("range", "bytes=100-")
            .send()
            .await
            .unwrap();
    assert_eq!(part.status(), 206);
    assert_eq!(part.headers()["accept-ranges"], "bytes");
    assert_eq!(part.bytes().await.unwrap(), padded_tone()[100..]);
    let (status, _) = get(
        &w,
        &format!("/voice-lab/api/rounds/{}/candidates/9", typed.id),
    )
    .await;
    assert_eq!(status, 404);

    // Discarded, it is gone with its files.
    let (status, left) = post(
        &w,
        &format!("/voice-lab/api/rounds/{}/delete", typed.id),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{left}");
    assert_eq!(left["rounds"], json!([]));
    let (status, _) = post(&w, "/voice-lab/api/rounds/../delete", json!({})).await;
    assert_ne!(status, 200);
}

#[tokio::test]
async fn a_saved_sample_is_trimmed_and_keeps_its_transcript_and_how_it_was_made() {
    let w = lab_world().await;
    let r: VoiceRound = serde_json::from_value(round(&w, 2, 7).await).unwrap();
    let (status, saved) = post(
        &w,
        "/voice-lab/api/samples",
        json!({"round": r.id, "candidate": 2, "name": "anna.wav"}),
    )
    .await;
    assert_eq!(status, 200, "{saved}");
    let saved: VoiceSampleSaved = round_trips("voice sample saved", &saved);
    assert_eq!(
        (saved.clip.as_str(), saved.voice.as_str()),
        ("anna.wav", "anna")
    );
    assert!(saved.library_active);
    assert_eq!(saved.replaced, Vec::<String>::new());
    let p = &saved.provenance;
    assert_eq!(
        (p.trim.lead_ms, p.trim.tail_ms, p.trim.kept_ms),
        (460, 180, 1160)
    );
    assert_eq!(p.seed, Some(8));
    assert_eq!(p.candidate, 2);
    assert_eq!(p.round, r.id);
    assert_eq!(p.alias, "audio/design");
    assert_eq!(p.answered_by, "audio/design");
    assert_eq!(p.description.as_deref(), Some("a warm, calm voice"));
    assert_eq!(p.language.as_deref(), Some("en"));
    assert_eq!(p.text, TABLE[0].sentence);
    assert_eq!(p.row.as_ref().unwrap().model_id, "design");
    assert_eq!(p.generated_at, r.created_at);

    let file = std::fs::read(voices(&w).join("anna.wav")).unwrap();
    assert_eq!(wav::duration_ms(&file), Some(1160));
    assert_eq!(lmgw_core::voice_lab::provenance::sha256(&file), p.sha256);
    assert_eq!(prompt_text(&w), format!("anna|{}\n", TABLE[0].sentence));

    // The library shows it with its record.
    let (status, list) = get(&w, "/voice-lab/api/samples").await;
    assert_eq!(status, 200);
    let list: VoiceSamples = round_trips("voice samples", &list);
    let anna = &list.samples[0];
    assert_eq!(anna.clip.voice, "anna");
    assert_eq!(anna.clip.transcript, TABLE[0].sentence);
    assert_eq!(anna.provenance.as_ref(), Some(p));
    assert!(anna.provenance_current);

    // The name is taken: refused, nothing written.
    let (status, body) = post(
        &w,
        "/voice-lab/api/samples",
        json!({"round": r.id, "candidate": 1, "name": "anna"}),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "name_taken");
    assert!(
        body["message"].as_str().unwrap().contains("anna.wav"),
        "{body}"
    );
    assert_eq!(std::fs::read(voices(&w).join("anna.wav")).unwrap(), file);

    // Confirmed: every file of that voice name goes, the new one is in.
    std::fs::write(voices(&w).join("anna.mp3"), b"ID3 an upload").unwrap();
    let (status, body) = post(
        &w,
        "/voice-lab/api/samples",
        json!({"round": r.id, "candidate": 1, "name": "anna", "overwrite": true}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["replaced"], json!(["anna.mp3", "anna.wav"]));
    assert!(!voices(&w).join("anna.mp3").exists());
    assert_eq!(body["provenance"]["seed"], 7);
    assert_eq!(prompt_text(&w), format!("anna|{}\n", TABLE[0].sentence));

    // Replaced by hand: the record no longer describes it.
    std::fs::write(voices(&w).join("anna.wav"), padded_tone()).unwrap();
    let (_, list) = get(&w, "/voice-lab/api/samples").await;
    assert_eq!(list["samples"][0]["provenance_current"], false);
    assert_eq!(list["samples"][0]["provenance"]["seed"], 7);
}

#[tokio::test]
async fn the_cloning_check_names_the_sample_as_its_voice() {
    let w = lab_world().await;
    let r: VoiceRound = serde_json::from_value(round(&w, 1, 1).await).unwrap();
    let (status, _) = post(
        &w,
        "/voice-lab/api/samples",
        json!({"round": r.id, "candidate": 1, "name": "anna"}),
    )
    .await;
    assert_eq!(status, 200);
    let (status, facts) = get(&w, "/voice-lab/api/model?alias=audio/clone").await;
    assert_eq!(status, 200, "{facts}");
    assert_eq!(facts["clones"], true, "{facts}");

    let resp =
        w.gw.client()
            .post(format!("{}/voice-lab/api/clone-check", w.gw))
            .json(&json!({"alias": "audio/clone", "sample": "anna", "language": "de"}))
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().to_vec(), padded_tone());
    let sent = w.sent().await;
    let last = sent.last().unwrap();
    assert_eq!(last["voice"], "anna", "{last}");
    assert_eq!(last["input"], TABLE[1].check_line);
    assert!(last.get("voice_ref").is_none());

    // Not the class's voice directory: the clip goes as a path, with its
    // transcript.
    let mut s = w.state.snapshot().settings.clone();
    s.audio.voice_dir = "/elsewhere".into();
    store::save_settings(&w.state.db, &s).await.unwrap();
    w.state.reload_snapshot().await.unwrap();
    let (status, _) = post(
        &w,
        "/voice-lab/api/clone-check",
        json!({"alias": "audio/clone", "sample": "anna.wav", "text": "Hi."}),
    )
    .await;
    assert_eq!(status, 200);
    let sent = w.sent().await;
    let last = sent.last().unwrap();
    assert_eq!(last["voice_ref"], "/models/voices/anna.wav", "{last}");
    assert_eq!(last["reference_text"], TABLE[0].sentence);
    assert_eq!(last["input"], "Hi.");

    let (status, body) = post(
        &w,
        "/voice-lab/api/clone-check",
        json!({"alias": "audio/clone", "sample": "nobody", "text": "Hi."}),
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found", "{body}");
}

#[tokio::test]
async fn the_owners_sentence_is_what_a_round_says() {
    let w = lab_world().await;
    let (status, t) = post(
        &w,
        "/voice-lab/api/sentences",
        json!({"language": "de", "text": "Mein eigener Satz."}),
    )
    .await;
    assert_eq!(status, 200, "{t}");
    let t: VoiceSentences = round_trips("voice sentences", &t);
    assert!(t.languages[1].saved);
    let (_, again) = get(&w, "/voice-lab/api/sentences").await;
    assert_eq!(again["languages"][1]["text"], "Mein eigener Satz.");

    let (status, r) = post(
        &w,
        "/voice-lab/api/rounds",
        json!({"alias": "audio/design", "language": "de", "count": 1,
               "description": "tief"}),
    )
    .await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["text"], "Mein eigener Satz.");
    assert_eq!(w.sent().await[0]["input"], "Mein eigener Satz.");

    // A language with no sentence: the text has to be written.
    let (status, body) = post(
        &w,
        "/voice-lab/api/rounds",
        json!({"alias": "audio/design", "language": "fr", "description": "x"}),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        body["message"].as_str().unwrap().contains("French"),
        "{body}"
    );
}

/// A cloud text-to-speech alias `cloud-tts` answering with [`padded_tone`].
async fn cloud_tts(w: &World) -> MockServer {
    let cloud = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/wav")
                .set_body_bytes(padded_tone()),
        )
        .mount(&cloud)
        .await;
    let up = store::insert_upstream(
        &w.state.db,
        &NewUpstream {
            name: "cloud".into(),
            protocol: Protocol::OpenaiCompatible,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", cloud.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 10_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &w.state.db,
        &NewAlias {
            alias: "cloud-tts".into(),
            upstream_id: up,
            upstream_model_id: "tts-1".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({"capabilities": {
                "task": "tts", "endpoints": ["/v1/audio/speech"], "source": "owner"
            }})),
            fallback: None,
        },
    )
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    cloud
}

#[tokio::test]
async fn a_held_row_renders_through_its_fallback_and_the_round_says_so() {
    let w = world().await;
    w.row_with(
        "design",
        "qwen3_tts",
        |r| audiocpp_gguf::qwen3(r, "voice_design"),
        |row| {
            row.task = "vdes".into();
            row.fallback = Some("cloud-tts".into());
        },
    )
    .await;
    let cloud = cloud_tts(&w).await;
    lmgw_core::ops::hold_set(&w.state, true).await.unwrap();

    let (status, facts) = get(&w, "/voice-lab/api/model?alias=audio/design").await;
    assert_eq!(status, 200, "{facts}");
    assert_eq!(facts["held"], true);
    assert_eq!(facts["fallback"], "cloud-tts");

    let r: VoiceRound = serde_json::from_value(round(&w, 2, 5).await).unwrap();
    assert_eq!(r.alias, "audio/design");
    assert_eq!(r.answered_by, "cloud-tts");
    assert_eq!(r.fallback_reason.as_deref(), Some("hold"));
    assert_eq!(r.upstream, "cloud");
    assert_eq!(r.upstream_model, "tts-1");
    assert_eq!(r.row, None, "a cloud model answered");
    assert!(!r.seeds_sent, "a cloud model is sent no seed");
    assert!(r.candidates.iter().all(|c| c.seed.is_none()));
    assert!(r
        .language_note
        .as_deref()
        .unwrap()
        .contains("cloud-tts is a cloud model"));
    let heard = cloud.received_requests().await.unwrap();
    assert_eq!(heard.len(), 2);
    assert_eq!(w.runs(), 0, "the held row never started");

    // The sample remembers who answered.
    let (status, saved) = post(
        &w,
        "/voice-lab/api/samples",
        json!({"round": r.id, "candidate": 1, "name": "held"}),
    )
    .await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["provenance"]["answered_by"], "cloud-tts");
    assert_eq!(saved["provenance"]["fallback_reason"], "hold");
    lmgw_core::ops::hold_set(&w.state, false).await.unwrap();
}
