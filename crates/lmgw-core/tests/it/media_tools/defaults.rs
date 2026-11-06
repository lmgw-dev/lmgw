//! The media tools' default models (design §3.8): Settings → Chat's, a
//! folder's and a thread's, the narrowest that says something winning; a
//! call that leaves out `model` uses the one that resolves, the schema
//! stops requiring `model` and the description names it; a default that no
//! longer resolves — or one the caller's key may not use — is none, said
//! with its reason, and never replaced by another model.

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

use lmgw_api_types::chat_media::MediaDefaults;
use lmgw_core::mcp::exec::resolve_attached_in_thread;
use lmgw_core::mcp::media::MediaLayers;
use lmgw_core::mcp::scope::ToolScope;
use lmgw_core::mcp::spec::{ApprovalRule, McpToolSpec};

use super::chat_turn::{answer, chat_model, send, tool_call};
use super::scope::{key, mcp_as};
use super::{call, cloud_alias, gateway, image_row, job, mcp, mount_sd, rows_of, text_of, Z_IMAGE};
use crate::common::Gw;
use crate::support::audio_world::wav;

/// `/mcp`'s tool `name`.
async fn listed(gw: &Gw, name: &str) -> Value {
    let v = mcp(gw, "tools/list", json!({})).await;
    v["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .find(|t| t["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("{name} is not listed: {v}"))
}

/// Settings → Chat's defaults set to `defaults`: the answer's status and body.
async fn set_settings(gw: &Gw, defaults: Value) -> (u16, String) {
    let r = gw
        .client()
        .post(format!("{gw}/api/op/settings_set_full"))
        .json(&json!({"chat_media_defaults": defaults}))
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

/// `GET /chat/api/media/defaults` with `query`.
async fn view(gw: &Gw, query: &str) -> Value {
    gw.client()
        .get(format!("{gw}/chat/api/media/defaults{query}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn required(tool: &Value) -> Vec<String> {
    tool["inputSchema"]["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn description(tool: &Value) -> &str {
    tool["description"].as_str().unwrap()
}

#[tokio::test]
async fn a_settings_default_lets_an_mcp_call_leave_out_model_and_an_explicit_one_wins() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let rows = [
        image_row("z", Z_IMAGE, false),
        image_row("y", Z_IMAGE, false),
        image_row("e", Z_IMAGE, true),
    ];
    let (state, gw) = gateway(&sd, &rows).await;
    // No default: `model` is required, and the description names none.
    let before = listed(&gw, "image__generate").await;
    assert_eq!(required(&before), vec!["model", "prompt"]);
    assert!(!description(&before).contains("Omit `model`"));

    let (status, body) = set_settings(&gw, json!({"generate": "image/y"})).await;
    assert_eq!(status, 200, "{body}");
    let tool = listed(&gw, "image__generate").await;
    assert_eq!(required(&tool), vec!["prompt"], "the default stands in");
    assert!(
        description(&tool).contains("Omit `model` to use image/y."),
        "{}",
        description(&tool)
    );
    assert!(tool["inputSchema"]["properties"]["model"]["description"]
        .as_str()
        .unwrap()
        .contains("Omit it to use image/y"));
    // The edit tool has no default of its own: the drawing default does not
    // stand in for it, so it still requires `model`.
    let edit = listed(&gw, "image__edit").await;
    assert!(required(&edit).contains(&"model".to_string()));
    // The block is the same from list to list (the prompt cache's rule).
    assert_eq!(listed(&gw, "image__generate").await, tool);

    let r = call(&gw, "image__generate", json!({"prompt": "a red square"})).await;
    assert_eq!(r["isError"], json!(false), "{r}");
    assert!(text_of(&r).contains("with image/y"), "{}", text_of(&r));
    let r = call(
        &gw,
        "image__generate",
        json!({"model": "image/z", "prompt": "a red square"}),
    )
    .await;
    assert_eq!(r["isError"], json!(false), "{r}");
    assert!(
        text_of(&r).contains("with image/z"),
        "explicit wins: {}",
        text_of(&r)
    );
    let rows = rows_of(&state, "image", 2).await;
    assert_eq!(rows[0]["alias"], json!("image/y"));
    assert_eq!(rows[1]["alias"], json!("image/z"));
}

#[tokio::test]
async fn a_name_no_tool_can_use_is_refused_on_save_with_the_choices() {
    let sd = MockServer::start().await;
    let (_state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let (status, body) = set_settings(&gw, json!({"generate": "image/nope"})).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("no model of this name"), "{body}");
    assert!(body.contains("pick one of: image/z"), "{body}");
    // `""` clears a field.
    let (status, body) = set_settings(&gw, json!({"generate": "image/z"})).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = set_settings(&gw, json!({"generate": ""})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(view(&gw, "").await["settings"], json!({}));
}

#[tokio::test]
async fn a_stale_default_is_no_default_and_says_why() {
    let sd = MockServer::start().await;
    let rows = [
        image_row("z", Z_IMAGE, false),
        image_row("y", Z_IMAGE, false),
    ];
    let (state, gw) = gateway(&sd, &rows).await;
    let (status, body) = set_settings(&gw, json!({"generate": "image/y"})).await;
    assert_eq!(status, 200, "{body}");
    sqlx::query("UPDATE image_models SET enabled = 0 WHERE model_id = 'y'")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();

    let tool = listed(&gw, "image__generate").await;
    assert_eq!(required(&tool), vec!["model", "prompt"], "required again");
    assert!(!description(&tool).contains("Omit `model`"));
    let r = call(&gw, "image__generate", json!({"prompt": "a red square"})).await;
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(
        text_of(&r).contains("argument 'model' is required"),
        "never another row: {}",
        text_of(&r)
    );

    let v = view(&gw, "").await;
    assert_eq!(
        v["settings"]["generate"],
        json!("image/y"),
        "kept as stored"
    );
    assert!(v["resolved"].get("generate").is_none(), "{v}");
    assert_eq!(
        v["unavailable"],
        json!([{"field": "generate", "name": "image/y",
                "reason": "the image model is switched off"}])
    );
    // A save of something else does not trip over it.
    let (status, body) = set_settings(&gw, json!({"transcribe": ""})).await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn a_thread_s_default_wins_over_its_folder_s_over_settings() {
    let sd = MockServer::start().await;
    let rows = [
        image_row("a", Z_IMAGE, false),
        image_row("b", Z_IMAGE, false),
        image_row("c", Z_IMAGE, false),
    ];
    let (state, gw) = gateway(&sd, &rows).await;
    assert_eq!(
        set_settings(&gw, json!({"generate": "image/a"})).await.0,
        200
    );
    let folder: Value = gw
        .client()
        .post(format!("{gw}/chat/api/folders"))
        .json(&json!({"name": "Art", "defaults": {"media_defaults": {"generate": "image/b"}}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let fid = folder["id"].as_i64().unwrap_or_else(|| panic!("{folder}"));
    assert_eq!(
        folder["defaults"]["media_defaults"],
        json!({"generate": "image/b"})
    );
    let t: Value = gw
        .client()
        .post(format!("{gw}/chat/api/threads"))
        .json(&json!({"model_alias": "my-model", "folder_id": fid}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = t["id"].as_i64().unwrap();
    // Not copied into the thread: it inherits, live.
    assert_eq!(t["media_defaults"], json!({}), "{t}");
    let resolved = |v: &Value| v["resolved"]["generate"].clone();

    let v = view(&gw, &format!("?thread={tid}")).await;
    assert_eq!(resolved(&v), json!("image/b"), "the folder's: {v}");
    assert_eq!(v["inherited"]["generate"], json!("image/b"));
    let settings = |m: Value| {
        let gw = &gw;
        async move {
            let r = gw
                .client()
                .post(format!("{gw}/chat/api/threads/{tid}/settings"))
                .json(&json!({"media_defaults": m}))
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }
    };
    assert_eq!(settings(json!({"generate": "image/c"})).await.0, 200);
    let v = view(&gw, &format!("?thread={tid}")).await;
    assert_eq!(resolved(&v), json!("image/c"), "the thread's: {v}");
    // An empty override is no default, whatever the levels above say.
    assert_eq!(settings(json!({"generate": ""})).await.0, 200);
    let v = view(&gw, &format!("?thread={tid}")).await;
    assert!(v["resolved"].get("generate").is_none(), "{v}");
    // Inherit again; the folder's defaults change and the thread follows.
    assert_eq!(settings(json!({})).await.0, 200);
    let r = gw
        .client()
        .post(format!("{gw}/chat/api/folders/{fid}"))
        .json(&json!({"defaults_patch": {"media_defaults": null}}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let v = view(&gw, &format!("?thread={tid}")).await;
    assert_eq!(resolved(&v), json!("image/a"), "Settings': {v}");
    // A thread's unknown model is refused by name.
    let (status, body) = settings(json!({"generate": "image/zz"})).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("image/zz"), "{body}");

    // The tool list a turn resolves follows the same levels.
    let spec = McpToolSpec {
        server_label: "image".into(),
        allowed_tools: None,
        require_approval: ApprovalRule::Never,
        server_url: None,
        connector_id: None,
    };
    let layers = MediaLayers::of_thread(
        Some(&MediaDefaults {
            generate: Some("image/b".into()),
            ..Default::default()
        }),
        &MediaDefaults {
            generate: Some("image/c".into()),
            ..Default::default()
        },
    );
    let r = resolve_attached_in_thread(&state, &[spec], &ToolScope::gateway(), &layers).await;
    let g = r
        .tools
        .iter()
        .find(|t| t.def.name == "image__generate")
        .expect("listed");
    assert!(
        g.def
            .description
            .as_deref()
            .unwrap_or_default()
            .contains("Omit `model` to use image/c."),
        "{}",
        g.def.description.as_deref().unwrap_or_default()
    );
}

#[tokio::test]
async fn a_chat_turn_s_call_without_model_uses_the_thread_s_default() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let chat = MockServer::start().await;
    let rows = [
        image_row("z", Z_IMAGE, false),
        image_row("y", Z_IMAGE, false),
    ];
    let (state, gw) = gateway(&sd, &rows).await;
    chat_model(
        &state,
        &chat,
        &[
            tool_call("c1", "image__generate", json!({"prompt": "a red square"})),
            answer("here it is"),
        ],
    )
    .await;
    let tid = super::chat_turn::thread(&gw).await;
    let r = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/settings"))
        .json(&json!({"media_defaults": {"generate": "image/y"}}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());

    let body = send(&gw, tid, "draw me a red square").await;
    assert!(body.contains("here it is"), "{body}");
    let rows = rows_of(&state, "image", 1).await;
    assert_eq!(rows[0]["alias"], json!("image/y"), "{rows:?}");
    // The model was told it may leave `model` out.
    let first = &chat.received_requests().await.unwrap()[0];
    let sent: Value = serde_json::from_slice(&first.body).unwrap();
    let tool = sent["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "image__generate")
        .cloned()
        .unwrap_or_else(|| panic!("{sent}"));
    assert_eq!(
        tool["function"]["parameters"]["required"],
        json!(["prompt"])
    );
    assert!(tool["function"]["description"]
        .as_str()
        .unwrap()
        .contains("Omit `model` to use image/y."));
}

#[tokio::test]
async fn a_default_outside_the_key_s_scope_is_none_for_that_key() {
    let sd = MockServer::start().await;
    let rows = [
        image_row("z", Z_IMAGE, false),
        image_row("y", Z_IMAGE, false),
    ];
    let (state, gw) = gateway(&sd, &rows).await;
    assert_eq!(
        set_settings(&gw, json!({"generate": "image/y"})).await.0,
        200
    );
    let token = key(
        &state,
        &gw,
        "z-only",
        json!({"scope_mode": "allow", "scope_patterns": "image/z"}),
    )
    .await;
    let list = mcp_as(&gw, &token, "tools/list", json!({})).await;
    let tool = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "image__generate")
        .cloned()
        .unwrap_or_else(|| panic!("{list}"));
    assert_eq!(required(&tool), vec!["model", "prompt"]);
    assert!(!description(&tool).contains("image/y"));
    // The owner still has it.
    assert_eq!(
        required(&listed(&gw, "image__generate").await),
        vec!["prompt"]
    );
}

#[tokio::test]
async fn the_default_voice_goes_with_the_default_speech_model() {
    let sd = MockServer::start().await;
    let cloud = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(wav())
        .mount(&cloud)
        .await;
    let (state, gw) = gateway(&sd, &[]).await;
    cloud_alias(&state, &cloud, "tts-cloud.example", "tts").await;
    cloud_alias(&state, &cloud, "tts-other.example", "tts").await;
    let (status, body) =
        set_settings(&gw, json!({"speak": "tts-cloud.example", "voice": "alloy"})).await;
    assert_eq!(status, 200, "{body}");

    let tool = listed(&gw, "audio__speak").await;
    assert_eq!(required(&tool), vec!["text"]);
    let voice = tool["inputSchema"]["properties"]["voice"]["description"]
        .as_str()
        .unwrap();
    assert!(
        voice.contains("Omit it for alloy with tts-cloud.example"),
        "{voice}"
    );

    let voices = || async {
        cloud
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/v1/audio/speech")
            .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["voice"].clone())
            .collect::<Vec<_>>()
    };
    let r = call(&gw, "audio__speak", json!({"text": "Hello there."})).await;
    assert_eq!(r["isError"], json!(false), "{r}");
    assert!(text_of(&r).contains("voice alloy"), "{}", text_of(&r));
    assert_eq!(voices().await, vec![json!("alloy")]);
    // Another model does not take the default model's voice.
    let r = call(
        &gw,
        "audio__speak",
        json!({"model": "tts-other.example", "text": "Hello there."}),
    )
    .await;
    assert_eq!(r["isError"], json!(false), "{r}");
    assert!(!text_of(&r).contains("voice alloy"), "{}", text_of(&r));
    assert_ne!(voices().await.last(), Some(&json!("alloy")));
}

#[tokio::test]
async fn the_edit_default_is_an_edit_model_or_none() {
    let sd = MockServer::start().await;
    let rows = [
        image_row("z", Z_IMAGE, false),
        image_row("e", Z_IMAGE, true),
    ];
    let (state, gw) = gateway(&sd, &rows).await;
    // A row that only draws is no edit model: refused on save, by name.
    let (status, body) = set_settings(&gw, json!({"edit": "image/z"})).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("cannot edit pictures"), "{body}");
    assert!(body.contains("pick one of: image/e"), "{body}");
    let (status, body) = set_settings(&gw, json!({"generate": "image/z", "edit": "image/e"})).await;
    assert_eq!(status, 200, "{body}");
    let edit = listed(&gw, "image__edit").await;
    assert_eq!(required(&edit), vec!["prompt", "image"]);
    assert!(description(&edit).contains("Omit `model` to use image/e."));

    // The edit row loses its Edit switch: no edit default (and no edit
    // tool), the stored name kept and said why.
    sqlx::query("UPDATE image_models SET edit = 0 WHERE model_id = 'e'")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let v = view(&gw, "").await;
    assert_eq!(v["settings"]["edit"], json!("image/e"));
    assert!(v["resolved"].get("edit").is_none(), "{v}");
    assert_eq!(v["resolved"]["generate"], json!("image/z"));
    assert!(
        v["unavailable"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["field"] == "edit"
                && u["reason"]
                    == "the image model cannot edit pictures (Edit is not set on its row)"),
        "{v}"
    );
}

/// A key scoped to one speech row is shown that row's voices only, and a
/// save of a voice only another row has is refused for it (review item 1);
/// a voice the level's own or inherited speech row cannot use is refused
/// naming that row's voices (item 7).
#[tokio::test]
async fn voices_follow_the_key_s_scope_and_the_level_s_speech_model() {
    use crate::support::audio_world::world;
    use crate::support::audiocpp_gguf;
    use lmgw_core::mcp::media::defaults;
    let w = world().await;
    w.row("supertonic", "supertonic", audiocpp_gguf::supertonic)
        .await;
    w.row("magpie", "magpie_tts", audiocpp_gguf::magpie).await;
    let token = key(
        &w.state,
        &w.gw,
        "super-only",
        json!({"scope_mode": "allow", "scope_patterns": "audio/supertonic"}),
    )
    .await;
    let name = token.trim_start_matches("lmgw-");
    let v = defaults::view(&w.state, Some(name), None, None).await;
    assert_eq!(
        v.choices.voices.keys().collect::<Vec<_>>(),
        vec!["audio/supertonic"],
        "{v:?}"
    );
    let jason = MediaDefaults {
        voice: Some("Jason".into()),
        ..Default::default()
    };
    let none = MediaDefaults::default();
    let e = defaults::check(&w.state, Some(name), (jason.clone(), &none), &none, false)
        .await
        .unwrap_err();
    assert!(e.contains("not a voice of any speech model"), "{e}");
    // The owner reaches both rows: with no speech model at the level, a
    // voice some row has passes.
    assert!(
        defaults::check(&w.state, None, (jason.clone(), &none), &none, false)
            .await
            .is_ok()
    );

    // Settings' own speech model names the voices it has.
    let (status, body) = set_settings(
        &w.gw,
        json!({"speak": "audio/supertonic", "voice": "Jason"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.contains("not a voice of audio/supertonic") && body.contains("its voices: F1, M1"),
        "{body}"
    );
    let (status, body) =
        set_settings(&w.gw, json!({"speak": "audio/supertonic", "voice": "M1"})).await;
    assert_eq!(status, 200, "{body}");
    // A folder inherits Settings' speech model: Jason is refused there,
    // and passes with the folder's own magpie.
    let folder = |m: Value| {
        let gw = &w.gw;
        async move {
            let r = gw
                .client()
                .post(format!("{gw}/chat/api/folders"))
                .json(&json!({"name": "Voices", "defaults": {"media_defaults": m}}))
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }
    };
    let (status, body) = folder(json!({"voice": "Jason"})).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("not a voice of audio/supertonic"), "{body}");
    let (status, body) = folder(json!({"speak": "audio/magpie", "voice": "Jason"})).await;
    assert_eq!(status, 200, "{body}");
}

/// A field this build does not know is refused with the fields it does
/// (review item 2), on every input.
#[tokio::test]
async fn an_unknown_field_is_refused_naming_the_known_ones() {
    let sd = MockServer::start().await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let (status, body) = set_settings(&gw, json!({"image__generate": "image/z"})).await;
    assert!((400..500).contains(&status), "{status} {body}");
    assert!(
        body.contains("image__generate") && body.contains("`generate`, `edit`, `speak`"),
        "{body}"
    );
    assert_eq!(view(&gw, "").await["settings"], json!({}));
    let admin = crate::support::self_admin::Admin::new(&state).await;
    let (failed, said) = admin
        .call(
            "lmgw__settings_set",
            json!({"chat_media_defaults": "{\"image__generate\":\"image/z\"}"}),
        )
        .await;
    assert!(failed, "reported as a failure: {said}");
    assert!(
        said.contains("image__generate") && said.contains("`generate`"),
        "{said}"
    );
    assert_eq!(view(&gw, "").await["settings"], json!({}));
    let tid = super::chat_turn::thread(&gw).await;
    let r = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/settings"))
        .json(&json!({"media_defaults": {"image__generate": "image/z"}}))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.text().await.unwrap();
    assert!((400..500).contains(&status), "{status} {body}");
    assert!(body.contains("`generate`"), "{body}");
}

/// A name of the wrong kind says so (review item 3): a speech row as the
/// drawing default, a speech row as the transcription default.
#[tokio::test]
async fn a_name_of_the_wrong_kind_says_which_tool_cannot_use_it() {
    use crate::support::audio_world::world;
    use crate::support::audiocpp_gguf;
    let w = world().await;
    w.row("supertonic", "supertonic", audiocpp_gguf::supertonic)
        .await;
    let (status, body) = set_settings(&w.gw, json!({"generate": "audio/supertonic"})).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.contains("audio/supertonic is not a model image__generate can use"),
        "{body}"
    );
    let (status, body) = set_settings(&w.gw, json!({"transcribe": "audio/supertonic"})).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.contains(
            "the audio model's task is TTS now, and audio__transcribe needs an ASR model"
        ),
        "{body}"
    );
}

/// A device's thread in a folder the device does not see resolves with the
/// folder's defaults, as its turn does, and the answer leaves the folder's
/// own field out (review item 4).
#[tokio::test]
async fn a_device_s_view_of_a_thread_in_a_hidden_folder_resolves_as_its_turn() {
    use crate::device_chat::{chat_thread, device_world, post};
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let (s, f) = post(
        &w,
        &owner,
        "/chat/api/folders",
        json!({"name": "F", "defaults": {"media_defaults": {"generate": ""}}}),
    )
    .await;
    assert_eq!(s, 200, "{f}");
    let folder = f["id"].as_i64().unwrap_or_else(|| panic!("{f}"));
    // A device's delete of a folder holding a thread beyond its reach (an
    // Admin Chat thread) hides the folder from devices; the owner files a
    // thread there after.
    let admin = crate::device_chat::Hidden::Admin.make(&w).await.id;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{admin}/move"),
        json!({"folder_id": folder}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/folders/{folder}/delete"),
        json!({"threads": "keep"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let t = chat_thread(&w, &owner, "chatty").await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{t}/move"),
        json!({"folder_id": folder}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let v: Value = d
        .client
        .get(format!("{}/chat/api/media/defaults?thread={t}", w.gw))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(v.get("folder").is_none(), "the folder stays unseen: {v}");
    assert_eq!(
        v["inherited"]["generate"],
        json!(""),
        "the folder's level counts: {v}"
    );
    let mine: Value = owner
        .get(format!("{}/chat/api/media/defaults?thread={t}", w.gw))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mine["folder"], json!({"generate": ""}), "{mine}");
    assert_eq!(mine["inherited"], v["inherited"]);
}

/// The defaults a turn's tool block named hold for its calls (review item
/// 5): a settings change while the chat model is answering does not move a
/// model-less call to another model.
#[tokio::test]
async fn a_turn_s_call_runs_on_the_default_its_tool_block_named() {
    use super::chat_turn::chat_model_answers;
    use wiremock::ResponseTemplate;
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let chat = MockServer::start().await;
    let rows = [
        image_row("z", Z_IMAGE, false),
        image_row("y", Z_IMAGE, false),
    ];
    let (state, gw) = gateway(&sd, &rows).await;
    let sse = |b: String| {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_raw(b, "text/event-stream")
    };
    chat_model_answers(
        &state,
        &chat,
        vec![
            sse(tool_call(
                "c1",
                "image__generate",
                json!({"prompt": "a red square"}),
            ))
            .set_delay(std::time::Duration::from_millis(1500)),
            sse(answer("here it is")),
        ],
    )
    .await;
    assert_eq!(
        set_settings(&gw, json!({"generate": "image/y"})).await.0,
        200
    );
    let tid = super::chat_turn::thread(&gw).await;
    let turn = {
        let gw = gw.clone();
        tokio::spawn(async move { send(&gw, tid, "draw me a red square").await })
    };
    // The tool block went out naming image/y; the default moves meanwhile.
    for _ in 0..100 {
        if !chat.received_requests().await.unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        set_settings(&gw, json!({"generate": "image/z"})).await.0,
        200
    );
    let body = turn.await.unwrap();
    assert!(body.contains("here it is"), "{body}");
    let rows = rows_of(&state, "image", 1).await;
    assert_eq!(rows[0]["alias"], json!("image/y"), "{rows:?}");
}

/// A folder's `defaults_patch` changes the media defaults field by field,
/// as it does the voice (review item 7): a field's `null` inherits again,
/// `media_defaults: null` unsets the level whole.
#[tokio::test]
async fn a_folder_patch_changes_the_media_defaults_field_by_field() {
    let sd = MockServer::start().await;
    let rows = [
        image_row("b", Z_IMAGE, false),
        image_row("c", Z_IMAGE, false),
    ];
    let (_state, gw) = gateway(&sd, &rows).await;
    let folder: Value = gw
        .client()
        .post(format!("{gw}/chat/api/folders"))
        .json(&json!({"name": "Art", "defaults": {"media_defaults": {"generate": "image/b", "edit": ""}}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let fid = folder["id"].as_i64().unwrap_or_else(|| panic!("{folder}"));
    let patch = |m: Value| {
        let gw = &gw;
        async move {
            let r = gw
                .client()
                .post(format!("{gw}/chat/api/folders/{fid}"))
                .json(&json!({"defaults_patch": {"media_defaults": m}}))
                .send()
                .await
                .unwrap();
            let status = r.status().as_u16();
            let v: Value = r.json().await.unwrap();
            assert_eq!(status, 200, "{v}");
            v["defaults"]["media_defaults"].clone()
        }
    };
    assert_eq!(
        patch(json!({"generate": "image/c"})).await,
        json!({"generate": "image/c", "edit": ""})
    );
    assert_eq!(
        patch(json!({"edit": null})).await,
        json!({"generate": "image/c"})
    );
    assert_eq!(patch(json!({"generate": null})).await, Value::Null);
    assert_eq!(
        patch(json!({"generate": "image/b"})).await,
        json!({"generate": "image/b"})
    );
    assert_eq!(patch(Value::Null).await, Value::Null);
}
