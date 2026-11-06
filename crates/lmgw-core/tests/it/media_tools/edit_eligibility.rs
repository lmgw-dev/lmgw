//! Which models `image__edit` offers (design §3.2, §14 item 76): only the
//! ones that edit — an image row with `edit` set, a cloud alias with the
//! edits route. A row that only draws would take the picture as an img2img
//! start and hand back nearly the same picture, so it is neither offered
//! nor run under the edit's name. And a result that is its input, byte for
//! byte, is an error, never a new picture.

use serde_json::{json, Value};
use wiremock::MockServer;

use super::actions::{act, frame, made, picture};
use super::{
    b64, call, cloud_alias, gateway, image_row, job, mcp, mount_sd, png, rows_of, source_png,
    submitted, text_of, Z_IMAGE,
};
use crate::common::Gw;

/// `/mcp`'s tool `name`, if listed.
async fn listed(gw: &Gw, name: &str) -> Option<Value> {
    let v = mcp(gw, "tools/list", json!({})).await;
    v["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .find(|t| t["name"] == name)
        .cloned()
}

fn enum_of(tool: &Value) -> Vec<&str> {
    tool["inputSchema"]["properties"]["model"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect()
}

fn data_url(bytes: &[u8]) -> String {
    format!("data:image/png;base64,{}", b64(bytes))
}

#[tokio::test]
async fn the_edit_enum_holds_only_the_models_that_edit() {
    let sd = MockServer::start().await;
    let rows = [
        image_row("z-image-turbo", Z_IMAGE, false),
        image_row("qwen-edit", Z_IMAGE, true),
    ];
    let (state, gw) = gateway(&sd, &rows).await;
    let cloud = MockServer::start().await;
    cloud_alias(&state, &cloud, "cloud-draw.example", "image_generation").await;
    cloud_alias(&state, &cloud, "cloud-edit.example", "image_edit").await;

    let edit = listed(&gw, "image__edit").await.expect("listed");
    assert_eq!(
        enum_of(&edit),
        vec!["image/qwen-edit", "cloud-edit.example"]
    );
    let d = edit["description"].as_str().unwrap();
    assert!(!d.contains("image/z-image-turbo"), "{d}");
    assert!(!d.to_lowercase().contains("variation"), "{d}");
    assert!(
        d.contains("- image/qwen-edit (Z-Image-Turbo): write the prompt as plain sentences. Edits by instruction"),
        "{d}"
    );
    assert!(
        edit["inputSchema"]["properties"].get("strength").is_none(),
        "no variation parameter: {edit}"
    );

    // Drawing still offers every row, and points at the edit tool.
    let generate = listed(&gw, "image__generate").await.expect("listed");
    assert_eq!(
        enum_of(&generate),
        vec![
            "image/qwen-edit",
            "image/z-image-turbo",
            "cloud-draw.example",
            "cloud-edit.example"
        ]
    );
    assert!(generate["description"]
        .as_str()
        .unwrap()
        .contains("which image__edit takes"));
}

#[tokio::test]
async fn no_model_that_edits_means_no_edit_tool() {
    let sd = MockServer::start().await;
    let (state, gw) = gateway(&sd, &[image_row("z-image-turbo", Z_IMAGE, false)]).await;
    let cloud = MockServer::start().await;
    cloud_alias(&state, &cloud, "cloud-draw.example", "image_generation").await;

    assert!(listed(&gw, "image__edit").await.is_none());
    let generate = listed(&gw, "image__generate").await.expect("listed");
    let d = generate["description"].as_str().unwrap();
    assert!(!d.contains("image__edit"), "{d}");
    assert!(d.contains("None of these models can edit a picture"), "{d}");
}

#[tokio::test]
async fn an_edit_naming_a_model_that_cannot_edit_is_refused_with_the_ones_that_can() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let rows = [
        image_row("z-image-turbo", Z_IMAGE, false),
        image_row("qwen-edit", Z_IMAGE, true),
    ];
    let (_state, gw) = gateway(&sd, &rows).await;
    let r = call(
        &gw,
        "image__edit",
        json!({"model": "image/z-image-turbo", "prompt": "add spaceships",
               "image": data_url(&source_png())}),
    )
    .await;
    assert_eq!(r["isError"], json!(true), "{r}");
    let t = text_of(&r);
    assert!(
        t.contains("model 'image/z-image-turbo' cannot edit pictures"),
        "{t}"
    );
    assert!(
        t.contains("The models that can edit: image/qwen-edit."),
        "{t}"
    );
    assert!(submitted(&sd).await.is_empty(), "nothing was sent");

    // With no model that edits, the refusal says so.
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let (_state, gw) = gateway(&sd, &[image_row("z-image-turbo", Z_IMAGE, false)]).await;
    let r = call(
        &gw,
        "image__edit",
        json!({"model": "image/z-image-turbo", "prompt": "add spaceships",
               "image": data_url(&source_png())}),
    )
    .await;
    assert_eq!(r["isError"], json!(true), "{r}");
    let t = text_of(&r);
    assert!(t.contains("cannot edit pictures"), "{t}");
    assert!(
        t.contains("No model on this gateway can edit pictures"),
        "{t}"
    );
    assert!(submitted(&sd).await.is_empty(), "nothing was sent");
}

#[tokio::test]
async fn a_result_that_is_its_input_is_an_error_and_nothing_is_stored() {
    // The job hands back the test picture; the edit started from it.
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let (state, gw) = gateway(&sd, &[image_row("q", Z_IMAGE, true)]).await;
    let r = call(
        &gw,
        "image__edit",
        json!({"model": "image/q", "prompt": "make it blue", "image": data_url(&png())}),
    )
    .await;
    assert_eq!(r["isError"], json!(true), "{r}");
    let t = text_of(&r);
    assert!(
        t.contains("image/q returned the input picture unchanged, byte for byte"),
        "{t}"
    );
    assert!(t.contains("no new picture was kept"), "{t}");
    assert_eq!(rows_of(&state, "image", 1).await[0]["status"], json!(502));

    // A card's Vary the same: an error frame, and no action file.
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let (tid, _mid, id) = picture(&state, Some(made()), "tool").await;
    sqlx::query("UPDATE chat_attachments SET data = ?1 WHERE id = ?2")
        .bind(png())
        .bind(id)
        .execute(&state.db)
        .await
        .unwrap();
    let (status, frames) = act(&gw, id, json!({"action": "vary", "strength": 0.3})).await;
    assert_eq!(status, 200, "{frames:?}");
    let err = frame(&frames, "error");
    assert!(
        err.to_string()
            .contains("returned the input picture unchanged"),
        "{err}"
    );
    assert!(frames.iter().all(|(e, _)| e != "done"), "{frames:?}");
    let files: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_attachments WHERE thread_id = ?1 AND source = 'action'",
    )
    .bind(tid)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(files, 0);
}

#[tokio::test]
async fn a_variation_sends_its_picture_as_init_image_and_says_the_strength_that_applied() {
    // No strength in the body: the server's own default applies, and the
    // card says which (the probed defaults' 0.75).
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let (_tid, _mid, id) = picture(&state, Some(made()), "tool").await;
    let (status, frames) = act(
        &gw,
        id,
        json!({"action": "vary", "prompt": "the square at dusk"}),
    )
    .await;
    assert_eq!(status, 200, "{frames:?}");
    let done = frame(&frames, "done");
    let text = done["text"].as_str().unwrap();
    assert!(text.contains("as a variation"), "{text}");
    assert!(text.contains("strength 0.75"), "{text}");
    let body = &submitted(&sd).await[0];
    assert_eq!(body["init_image"], json!(data_url(&source_png())));
    assert!(body.get("strength").is_none(), "{body}");
    assert!(body.get("ref_images").is_none(), "{body}");
}
