//! Expressive speech at the gateway (audio-class gap 9): `instructions`
//! reach a model where it reads them or are dropped and said so, inline
//! tags are rendered, mapped or stripped — never read out — and
//! `capabilities.speech` publishes which. A cloud alias gets its tags
//! stripped unless the owner's override says the model renders them. The
//! packages are synthetic (`support::audiocpp_gguf`).

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

use crate::support::audio_world::{wav, world, World};
use crate::support::audiocpp_gguf;
use crate::support::realtime_fakes::{events_until, send, user_text, Turn};
use crate::support::realtime_tts::{speech_gateway, spoken_session};

fn header(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("x-lmgw-speech")
        .map(|v| v.to_str().unwrap().to_string())
}

#[tokio::test]
async fn instructions_go_where_the_family_reads_them_or_are_dropped() {
    let w = world().await;
    // Kokoro declares no instruction option and refuses unknown ones.
    w.row("kokoro", "kokoro_tts", |r| {
        audiocpp_gguf::with_options(r, "kokoro_tts", &["language", "speed"], &["offline"])
    })
    .await;
    // Auk reads only `instruct`.
    w.row("auk", "auk", |r| {
        audiocpp_gguf::with_options(r, "auk", &["instruct"], &["offline"])
    })
    .await;
    // CosyVoice3 reads `instruction`, which audio.cpp makes of `instructions`.
    w.row("cosy", "cosyvoice3", |r| {
        audiocpp_gguf::with_options(r, "cosyvoice3", &["instruction"], &["offline"])
    })
    .await;
    w.answer_wav().await;

    let resp = w
        .speak(
            json!({"model": "audio/kokoro", "input": "Hello [laughs] there.",
                      "instructions": "cheerful"}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp).as_deref(),
        Some("instructions=dropped; tags=stripped:1")
    );
    let resp = w
        .speak(json!({"model": "audio/auk", "input": "Hi.", "instructions": "calm"}))
        .await;
    assert_eq!(
        header(&resp).as_deref(),
        Some("instructions=options.instruct")
    );
    // CosyVoice3 clones from a clip: it needs a voice (live run 3, N4).
    let resp = w
        .speak(json!({"model": "audio/cosy", "input": "(laughs) Fine.",
                      "instructions": "warm", "voice": "anna"}))
        .await;
    assert_eq!(header(&resp).as_deref(), Some("tags=mapped:1"));

    let sent = w.sent().await;
    assert_eq!(
        sent[0],
        json!({"model": "kokoro", "input": "Hello there."}),
        "no instructions reach a model that refuses them, no tag is read out"
    );
    assert_eq!(
        sent[1],
        json!({"model": "auk", "input": "Hi.", "options": {"instruct": "calm"}})
    );
    assert_eq!(
        sent[2],
        json!({"model": "cosy", "input": "[laughter] Fine.", "instructions": "warm",
               "voice": "anna"})
    );
}

#[tokio::test]
async fn nothing_but_tags_is_refused_before_anything_starts() {
    let w = world().await;
    w.row("omni", "omnivoice", |r| {
        audiocpp_gguf::with_options(r, "omnivoice", &[], &["offline"])
    })
    .await;
    w.answer_wav().await;
    let resp = w
        .speak(json!({"model": "audio/omni", "input": " [laughter] (sighs) "}))
        .await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "empty_input", "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(w.runs(), 0, "nothing was started for it");
    assert!(w.sent().await.is_empty());

    // With text around it, OmniVoice renders its own tag.
    let resp = w
        .speak(json!({"model": "audio/omni", "input": "Ha [laughter] good one."}))
        .await;
    assert_eq!(resp.status(), 200);
    assert!(header(&resp).is_none(), "a kept tag is no change");
    assert_eq!(w.sent().await[0]["input"], "Ha [laughter] good one.");
}

/// Live run 3, N4: CosyVoice3 asked to speak with no voice passed
/// preflight, its container started, and the engine answered 500
/// "CosyVoice3 requires reference audio". A family that clones from a clip
/// is refused before anything starts when nothing gives it one; OmniVoice,
/// which speaks with a voice of its own, is not.
#[tokio::test]
async fn a_cloning_family_is_refused_without_a_reference_before_anything_starts() {
    let w = world().await;
    w.row("cosy", "cosyvoice3", |r| {
        audiocpp_gguf::with_options(r, "cosyvoice3", &["instruction"], &["offline"])
    })
    .await;
    w.row("omni", "omnivoice", |r| {
        audiocpp_gguf::with_options(r, "omnivoice", &[], &["offline"])
    })
    .await;
    w.answer_wav().await;
    let resp = w
        .speak(json!({"model": "audio/cosy", "input": "Hello."}))
        .await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "reference_required", "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(w.runs(), 0, "nothing was started for it");
    // A clip named, and OmniVoice with none: spoken.
    for req in [
        json!({"model": "audio/cosy", "input": "Hello.", "voice": "anna"}),
        json!({"model": "audio/omni", "input": "Hello."}),
    ] {
        let resp = w.speak(req.clone()).await;
        assert_eq!(resp.status(), 200, "{req}");
    }
    assert_eq!(w.sent().await.len(), 2);
}

#[tokio::test]
async fn a_voice_design_row_needs_a_description_and_is_refused_without_one() {
    let w = world().await;
    w.row_with(
        "design",
        "qwen3_tts",
        |r| audiocpp_gguf::qwen3(r, "voice_design"),
        |row| row.task = "vdes".into(),
    )
    .await;
    w.answer_wav().await;
    let resp = w
        .speak(json!({"model": "audio/design", "input": "Hello."}))
        .await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "instructions_required", "{body}");
    assert_eq!(w.runs(), 0);

    let resp = w
        .speak(json!({"model": "audio/design", "input": "Hello.",
                      "instructions": "a deep, slow narrator's voice"}))
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        w.sent().await[0]["instructions"],
        "a deep, slow narrator's voice"
    );
}

/// A row that describes its voice under `instruct` — the key lmgw's own
/// `instructions_required` message recommends — and a request with
/// `instructions`: audio.cpp makes those `options.instruction`, merges the
/// row's `instruct` in beside it, and Qwen3 refuses the two as conflicting
/// (the stand-in answers as it would, `support::audiocpp_options`). So the
/// text goes under `instruct` too, and replaces the row's (R2).
#[tokio::test]
async fn instructions_over_a_rows_own_description_replace_it_under_both_keys() {
    let w = world().await;
    w.row_with(
        "design",
        "qwen3_tts",
        |r| audiocpp_gguf::qwen3(r, "voice_design"),
        |row| {
            row.task = "vdes".into();
            row.default_request_options = json!({"instruct": "a deep, slow narrator"})
                .as_object()
                .cloned()
                .unwrap();
        },
    )
    .await;
    w.answer_wav().await;

    let sailor = "an old sailor, hoarse and slow";
    let resp = w
        .speak(json!({"model": "audio/design", "input": "Hello.", "instructions": sailor}))
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp).as_deref(),
        Some("instructions=also:options.instruct")
    );
    // Without any of its own, the row's description speaks.
    let resp = w
        .speak(json!({"model": "audio/design", "input": "Hello."}))
        .await;
    assert_eq!(resp.status(), 200);
    assert!(header(&resp).is_none());
    // A client's own keys are its own: sent as they came — and two that
    // disagree are the engine's to refuse (its 500, a 502 from lmgw).
    let resp = w
        .speak(
            json!({"model": "audio/design", "input": "Hello.", "instructions": sailor,
                      "options": {"instruct": "a child"}}),
        )
        .await;
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicting option values for instruction and instruct"),
        "{body}"
    );

    let sent = w.sent().await;
    assert_eq!(
        sent[0],
        json!({"model": "design", "input": "Hello.", "instructions": sailor,
               "options": {"instruct": sailor}})
    );
    assert_eq!(sent[1], json!({"model": "design", "input": "Hello."}));
}

/// audio.cpp hands `instructions` to the engine as `options.instruction`,
/// and OmniVoice and MOSS-VoiceGen read it although their specs declare no
/// such option (`audio::families::undeclared_instructions`): OmniVoice gets
/// them as sent, MOSS-VoiceGen designs its voice from them and is refused
/// without one before anything starts — and `capabilities.speech` says so.
#[tokio::test]
async fn engines_reading_instructions_their_spec_does_not_declare_get_them() {
    let w = world().await;
    w.row("omni", "omnivoice", |r| {
        audiocpp_gguf::with_options(r, "omnivoice", &[], &["offline"])
    })
    .await;
    w.row_with(
        "moss",
        "moss_voicegen",
        audiocpp_gguf::moss_voicegen,
        |row| row.task = "vdes".into(),
    )
    .await;
    w.answer_wav().await;

    let resp = w
        .speak(json!({"model": "audio/omni", "input": "Hello.",
                      "instructions": "a soft whisper"}))
        .await;
    assert_eq!(resp.status(), 200);
    assert!(header(&resp).is_none(), "nothing was dropped");
    let started = w.runs();

    let resp = w
        .speak(json!({"model": "audio/moss", "input": "Hello."}))
        .await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "instructions_required", "{body}");
    assert_eq!(w.runs(), started, "nothing was started for it");
    let resp = w
        .speak(json!({"model": "audio/moss", "input": "Hello.",
                      "instructions": "an old sailor, hoarse and slow"}))
        .await;
    assert_eq!(resp.status(), 200);

    let sent = w.sent().await;
    assert_eq!(
        sent,
        [
            json!({"model": "omni", "input": "Hello.", "instructions": "a soft whisper"}),
            json!({"model": "moss", "input": "Hello.",
                   "instructions": "an old sailor, hoarse and slow"}),
        ]
    );

    let models: Value =
        w.gw.client()
            .get(format!("{}/v1/models", w.gw))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    let speech = |id: &str| {
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap_or_else(|| panic!("{id} in {models}"))["capabilities"]["speech"]
            .clone()
    };
    let omni = speech("audio/omni");
    assert_eq!(omni["instructions"], "passthrough", "{omni}");
    assert_eq!(omni["instructions_required"], false);
    let moss = speech("audio/moss");
    assert_eq!(moss["instructions"], "voice_design", "{moss}");
    assert_eq!(moss["instructions_required"], true);
}

/// A cloud TTS alias on its own wiremock upstream.
async fn cloud_alias(w: &World, cloud: &MockServer, alias: &str, speech: Option<Value>) {
    let up = store::insert_upstream(
        &w.state.db,
        &NewUpstream {
            name: format!("{alias}-up"),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", cloud.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 10_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    let mut caps = json!({"task": "tts", "endpoints": ["/v1/audio/speech"], "source": "owner"});
    if let Some(s) = speech {
        caps["speech"] = s;
    }
    store::insert_alias(
        &w.state.db,
        &NewAlias {
            alias: alias.into(),
            upstream_id: up,
            upstream_model_id: "gpt-4o-mini-tts".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({ "capabilities": caps })),
        },
    )
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
}

#[tokio::test]
async fn a_cloud_alias_has_its_tags_stripped_unless_the_owner_says_they_render() {
    let w = world().await;
    let cloud = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(wav())
        .mount(&cloud)
        .await;
    cloud_alias(&w, &cloud, "cloud-tts", None).await;
    cloud_alias(
        &w,
        &cloud,
        "tagged-tts",
        Some(json!({"inline_tags": "free", "instructions": "style"})),
    )
    .await;

    let resp = w
        .speak(json!({"model": "cloud-tts", "input": "Hi [laughs].",
                      "instructions": "cheerful"}))
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp).as_deref(), Some("tags=stripped:1"));
    let resp = w
        .speak(json!({"model": "tagged-tts", "input": "Hi [laughs]."}))
        .await;
    assert!(header(&resp).is_none());

    let sent: Vec<Value> = cloud
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(
        sent[0],
        json!({"model": "gpt-4o-mini-tts", "input": "Hi.", "instructions": "cheerful"}),
        "an OpenAI TTS reads instructions; the tag would have been read out"
    );
    assert_eq!(sent[1]["input"], "Hi [laughs].");

    // The override's speech words are checked like every other.
    let models: Value =
        w.gw.client()
            .get(format!("{}/v1/models", w.gw))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    let tagged = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "tagged-tts")
        .unwrap();
    assert_eq!(
        tagged["capabilities"]["speech"],
        json!({"inline_tags": "free", "instructions": "style"})
    );
}

#[tokio::test]
async fn capabilities_speech_says_what_takes_effect_on_a_local_row() {
    let w = world().await;
    w.row("qwen3", "qwen3_tts", |r| {
        audiocpp_gguf::qwen3(r, "custom_voice")
    })
    .await;
    w.row("omni", "omnivoice", |r| {
        audiocpp_gguf::with_options(r, "omnivoice", &[], &["offline"])
    })
    .await;
    let models: Value =
        w.gw.client()
            .get(format!("{}/v1/models", w.gw))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    let entry = |id: &str| {
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap_or_else(|| panic!("{id} in {models}"))
            .clone()
    };
    let qwen3 = entry("audio/qwen3");
    assert_eq!(
        qwen3["capabilities"]["speech"],
        json!({"instructions": "style", "instructions_required": false,
               "inline_tags": "none", "languages": ["de", "en", "zh"],
               "streaming": false})
    );
    assert!(
        qwen3["notes"].as_array().unwrap().iter().any(|n| n
            .as_str()
            .unwrap()
            .starts_with("Speech: instructions set a speaking style")),
        "{qwen3}"
    );
    let omni = entry("audio/omni");
    assert_eq!(omni["capabilities"]["speech"]["inline_tags"], "fixed");
    assert_eq!(
        omni["capabilities"]["speech"]["instructions"], "passthrough",
        "OmniVoice's engine reads `instruction`, though its spec declares no such option"
    );
    assert_eq!(
        omni["capabilities"]["speech"]["instructions_required"],
        false
    );
    assert!(omni["capabilities"]["speech"]["tags"]
        .as_array()
        .unwrap()
        .contains(&json!("laughter")));
}

/// Realtime gets the same shaping (`Synthesis::speak`) and two texts per
/// clause (WP10 D9, D10): the TTS is sent the clause with its tags — shaped
/// for the route, here a TTS nobody described, so stripped — the transcript
/// is what was said, without them, and the model's history keeps what it
/// wrote, tags and all. A tag at the end has no words to go with: it is not
/// sent, and the history keeps it.
#[tokio::test]
async fn a_spoken_answer_keeps_tags_out_of_the_transcript_and_in_the_history() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    chat.push(Turn::text(&["Ha [laughs] that is funny.", " [sighs]"]));
    let (mut ws, _) = spoken_session(&addr, &[], 60_000, json!({})).await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = events_until(&mut ws, "response.done").await;
    let said: Vec<String> = (0..tts.seen.count())
        .map(|n| tts.seen.body(n)["input"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(said, ["Ha that is funny."]);
    let done: &Value = &events.last().unwrap()["response"];
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(
        done["output"][0]["content"][0]["transcript"],
        "Ha that is funny."
    );
    let deltas: String = events
        .iter()
        .filter(|e| e["type"] == "response.output_audio_transcript.delta")
        .map(|e| e["delta"].as_str().unwrap())
        .collect();
    assert_eq!(deltas, "Ha that is funny.");

    // The next request renders what the model wrote.
    chat.push(Turn::text(&["Sure."]));
    send(&mut ws, user_text("again")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(
        &mut ws,
        json!({"type": "response.create", "response": {"output_modalities": ["text"]}}),
    )
    .await;
    events_until(&mut ws, "response.done").await;
    let messages = chat.seen.chat(1)["messages"].clone();
    assert_eq!(messages[2]["role"], "assistant", "{messages}");
    assert_eq!(messages[2]["content"], "Ha [laughs] that is funny. [sighs]");
}
