use lmgw_api_types::voice_lab::{VoiceRound, VoiceSampleSaved};
use serde_json::{json, Value};

use super::{lab_world, post};
use crate::common::round_trips;
use crate::support::self_admin::Admin;

#[tokio::test]
async fn the_ops_check_their_arguments_and_answer_their_types() {
    let w = lab_world().await;
    let (status, body) = post(
        &w,
        "/api/op/voice_generate",
        json!({"alias": "audio/design", "seeds": [1, 2]}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_string().contains("unknown field `seeds`"), "{body}");
    let (status, body) = post(&w, "/api/op/voice_generate", json!({"language": "en"})).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_string().contains("alias"), "{body}");
    let (status, body) = post(
        &w,
        "/api/op/voice_generate",
        json!({"alias": "audio/design", "language": "en", "count": 0}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_string().contains("at least 1"), "{body}");
    let (status, body) = post(
        &w,
        "/api/op/voice_generate",
        json!({"alias": " ", "text": "Hello."}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(w.sent().await.len(), 0, "nothing was rendered");

    let (status, body) = post(
        &w,
        "/api/op/voice_generate",
        json!({"alias": "audio/design", "text": "Hello there.", "count": 2,
               "description": "bright"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let r: VoiceRound = round_trips("voice_generate", &body);
    assert_eq!(r.candidates.len(), 2);
    assert_eq!(r.language, None);

    let save = |name: &str, overwrite: bool| json!({"round": r.id, "candidate": 1, "name": name, "overwrite": overwrite});
    let (status, body) = post(&w, "/api/op/voice_sample_save", save("bright", false)).await;
    assert_eq!(status, 200, "{body}");
    round_trips::<VoiceSampleSaved>("voice_sample_save", &body);
    let (status, body) = post(&w, "/api/op/voice_sample_save", save("bright", false)).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"].as_str().unwrap().starts_with("name_taken:"),
        "{body}"
    );
    let (status, body) = post(&w, "/api/op/voice_sample_save", save("bright", true)).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(
        &w,
        "/api/op/voice_sample_save",
        json!({"round": "nope", "candidate": 1, "name": "x"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_string().contains("no round 'nope'"), "{body}");
    let (status, body) = post(
        &w,
        "/api/op/voice_sample_save",
        json!({"round": "../x", "candidate": 1, "name": "x"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_string().contains("not a round id"), "{body}");
}

#[tokio::test]
async fn the_tools_render_and_save_and_refuse_a_taken_name() {
    let w = lab_world().await;
    let admin = Admin::new(&w.state).await;
    let (failed, text) = admin
        .call(
            "lmgw__voice_generate",
            json!({"alias": "audio/design", "language": "en", "bogus": 1}),
        )
        .await;
    assert!(failed);
    assert!(text.contains("bogus"), "{text}");

    let text = admin
        .text(
            "lmgw__voice_generate",
            json!({"alias": "audio/design", "language": "en", "count": 1,
                   "description": "calm", "seed": 42}),
        )
        .await;
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["candidates"][0]["seed"], 42);
    let id = v["id"].as_str().unwrap();
    assert!(
        v["next_step"]
            .as_str()
            .unwrap()
            .contains(&format!("round={id}")),
        "{v}"
    );

    let args = json!({"round": id, "candidate": 1, "name": "calm"});
    admin.text("lmgw__voice_sample_save", args.clone()).await;
    let (failed, text) = admin.call("lmgw__voice_sample_save", args).await;
    assert!(failed);
    assert!(
        text.contains("name_taken") && text.contains("overwrite"),
        "{text}"
    );
}
