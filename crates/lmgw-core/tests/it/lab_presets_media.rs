//! The Image and Audio labs' presets (labs-store design §5): an
//! `image_prompt` is the Image lab's form, `audio_settings` the speech
//! panel and the PCM format, each saved, listed, read, updated and deleted
//! through the shared `/labs/api/{lab}/presets`; a kind is its own lab's
//! only, and a body whose numbers do not read is refused by name.

use lmgw_api_types::audio_lab::AudioSettings;
use lmgw_api_types::image_lab::ImageGenForm;
use lmgw_api_types::lab_store::{LabPresetBody, LabPresetList, LabPresetView};
use lmgw_core::state::AppState;
use serde_json::{json, Value};

use crate::common::{round_trips, serve, Gw};

async fn post(gw: &Gw, path: &str, body: Value) -> (u16, Value) {
    let r = gw
        .client()
        .post(format!("{gw}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

async fn get(gw: &Gw, path: &str) -> (u16, Value) {
    let r = gw.client().get(format!("{gw}{path}")).send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn an_image_prompt_and_audio_settings_are_saved_loaded_updated_and_deleted() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state).await;

    let prompt = json!({"model": "image/z", "prompt": "a city at night", "steps": "8",
                        "loras": [{"path": "add_detail", "multiplier": "0.8"}]});
    let (status, v) = post(
        &gw,
        "/labs/api/image/presets",
        json!({"name": "night", "kind": "image_prompt", "body": prompt}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let saved = round_trips::<LabPresetView>("image prompt", &v);
    let LabPresetBody::ImagePrompt(f) = &saved.preset else {
        panic!("an image prompt: {v}");
    };
    assert_eq!(f.steps, "8");
    assert_eq!(f.loras[0].path, "add_detail");

    let mut settings = AudioSettings::default();
    settings.speech.voice = "af_heart".into();
    settings.speech.speed = "1.25".into();
    settings.speech.stream = true;
    settings.pcm.rate = "44100".into();
    let (status, v) = post(
        &gw,
        "/labs/api/audio/presets",
        json!({"name": "fast heart", "kind": "audio_settings", "body": settings}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let audio = round_trips::<LabPresetView>("audio settings", &v);
    assert_eq!(
        audio.preset,
        LabPresetBody::AudioSettings(Box::new(settings.clone()))
    );

    // Listed by lab and kind, read back by id.
    let (status, v) = get(&gw, "/labs/api/image/presets?kind=image_prompt").await;
    assert_eq!(status, 200, "{v}");
    let list = round_trips::<LabPresetList>("image presets", &v);
    assert_eq!(list.presets.len(), 1);
    let (_, v) = get(&gw, "/labs/api/audio/presets").await;
    assert_eq!(
        round_trips::<LabPresetList>("audio presets", &v)
            .presets
            .len(),
        1
    );
    let (status, v) = get(&gw, &format!("/labs/api/audio/presets/{}", audio.id)).await;
    assert_eq!(status, 200, "{v}");

    // Updated whole, from what was read.
    let changed = ImageGenForm {
        prompt: "a city at dawn".into(),
        ..Default::default()
    };
    let (status, v) = post(
        &gw,
        &format!("/labs/api/image/presets/{}", saved.id),
        json!({"name": "dawn", "kind": "image_prompt", "body": changed,
               "updated_at": saved.updated_at}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let updated = round_trips::<LabPresetView>("updated", &v);
    assert_eq!(updated.name, "dawn");
    assert_eq!(
        updated.preset,
        LabPresetBody::ImagePrompt(Box::new(changed))
    );
    // An update from the old read is the 409.
    let (status, v) = post(
        &gw,
        &format!("/labs/api/image/presets/{}", saved.id),
        json!({"name": "again", "kind": "image_prompt", "body": {},
               "updated_at": saved.updated_at}),
    )
    .await;
    assert_eq!(status, 409, "{v}");
    assert_eq!(v["code"], "set_changed");

    // Deleted: the list after it.
    let (status, v) = post(
        &gw,
        &format!("/labs/api/image/presets/{}/delete", saved.id),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert!(round_trips::<LabPresetList>("after delete", &v)
        .presets
        .is_empty());
}

#[tokio::test]
async fn a_kind_is_its_own_labs_and_a_body_must_read() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state).await;

    for (lab, kind, body) in [
        ("decision", "image_prompt", json!({"prompt": "x"})),
        ("audio", "image_prompt", json!({"prompt": "x"})),
        ("image", "audio_settings", json!({})),
        ("decision", "audio_settings", json!({})),
        (
            "image",
            "question_set",
            json!({"questions": [], "default_model": "m"}),
        ),
    ] {
        let (status, v) = post(
            &gw,
            &format!("/labs/api/{lab}/presets"),
            json!({"name": "p", "kind": kind, "body": body}),
        )
        .await;
        assert_eq!(status, 400, "{kind} posted to the {lab} lab: {v}");
        assert!(
            v["message"]
                .as_str()
                .unwrap()
                .contains(&format!("not the {lab} lab's")),
            "{v}"
        );
    }

    let (status, v) = post(
        &gw,
        "/labs/api/image/presets",
        json!({"name": "bad", "kind": "image_prompt", "body": {"steps": "eight"}}),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert!(v["message"].as_str().unwrap().contains("steps"), "{v}");
    let (status, v) = post(
        &gw,
        "/labs/api/audio/presets",
        json!({"name": "bad", "kind": "audio_settings",
               "body": {"speech": {"options": "{nope"}}}),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert!(
        v["message"].as_str().unwrap().starts_with("options:"),
        "{v}"
    );
    let (_, v) = get(&gw, "/labs/api/image/presets").await;
    assert!(v["presets"].as_array().unwrap().is_empty(), "{v}");
}
