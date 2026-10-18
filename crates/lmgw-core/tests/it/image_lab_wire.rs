//! The Image lab's routes against their documented types
//! (`lmgw-api-types::image_lab`): each answer, read into its type and
//! written back, is the answer again, key by key. A field the gateway adds
//! to an answer without adding it to the type is dropped by the read and
//! fails here.

use lmgw_api_types::image_lab::{ImageLabAnswer, ImageLabModels};
use lmgw_api_types::lab_store::{LabGroupView, LabMultipartSummary, LabPresetView};
use lmgw_api_types::ImageCapabilities;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::round_trips;
use crate::image_lab::{image_row, mount_ready, one_png, run_group, setup_cloud, setup_local};

#[tokio::test]
async fn a_local_row_lists_and_generates_as_its_types() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .mount(&mock)
        .await;
    let (_state, base) = setup_local(&mock, &[image_row("z-image-turbo", false)]).await;

    // A generation starts the container, so its state and probed
    // capabilities are in the list.
    let resp = base
        .client()
        .post(format!("{base}/image-lab/api/groups"))
        .json(
            &json!({"model": "image/z-image-turbo", "prompt": "a cat", "width": "512",
                       "height": "512"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let live: Value = resp.json().await.unwrap();
    let group = round_trips::<LabGroupView>("generation's group", &live);
    assert!(group.runs.is_empty() && !group.in_flight);
    let resp = run_group(&base, group.id).await;
    assert_eq!(resp.status(), 200);
    let live: Value = resp.json().await.unwrap();
    let made = round_trips::<ImageLabAnswer>("generate", &live);
    let run = &made.group.runs[0];
    assert_eq!(run.endpoint, "/v1/images/generations");
    let request: Value = serde_json::from_str(&run.request).unwrap();
    assert_eq!(request["size"], "512x512");

    let live: Value = base
        .client()
        .get(format!("{base}/image-lab/api/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list = round_trips::<ImageLabModels>("models", &live);
    let row = &list.models[0];
    assert!(row.local && row.state.is_some(), "{live}");
    let caps: &ImageCapabilities = row.image_capabilities.as_ref().expect("probed");
    assert!(caps.samplers.iter().any(|s| s == "euler"));
}

#[tokio::test]
async fn a_cloud_alias_lists_and_edits_as_its_types() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/edits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .mount(&mock)
        .await;
    let (_state, base) = setup_cloud(&mock).await;

    let live: Value = base
        .client()
        .get(format!("{base}/image-lab/api/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list = round_trips::<ImageLabModels>("models", &live);
    assert!(list.models.iter().any(|m| m.name == "my-edit" && m.edit));
    assert!(list.models.iter().all(|m| !m.local && m.args.is_none()));

    let upload = reqwest::multipart::Form::new()
        .text(
            "form",
            json!({"model": "my-edit", "prompt": "night", "n": "1"}).to_string(),
        )
        .part(
            "image",
            reqwest::multipart::Part::bytes(b"\x89PNGfake".to_vec())
                .file_name("in.png")
                .mime_str("image/png")
                .unwrap(),
        )
        .part(
            "mask",
            reqwest::multipart::Part::bytes(b"\x89PNGmask".to_vec()).file_name("mask.png"),
        );
    let resp = base
        .client()
        .post(format!("{base}/image-lab/api/groups/edit"))
        .multipart(upload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let live: Value = resp.json().await.unwrap();
    let group = round_trips::<LabGroupView>("edit's group", &live);
    assert_eq!(group.artifacts.len(), 2);
    let resp = run_group(&base, group.id).await;
    assert_eq!(resp.status(), 200);
    let live: Value = resp.json().await.unwrap();
    let edited = round_trips::<ImageLabAnswer>("edit", &live);
    let request: LabMultipartSummary = serde_json::from_str(&edited.group.runs[0].request).unwrap();
    assert_eq!(request.files[0].name, "image");
    assert_eq!(
        request.files[1].content_type.as_deref(),
        Some("application/octet-stream"),
        "a part goes out as the type the store kept: these bytes are of no kind lmgw knows"
    );
}

/// A saved prompt preset answers its type, and its kind is the Image lab's
/// alone.
#[tokio::test]
async fn an_image_prompt_preset_answers_its_type() {
    let mock = MockServer::start().await;
    let (_state, base) = setup_cloud(&mock).await;
    let body = json!({"name": "night city", "kind": "image_prompt",
                      "body": {"model": "my-image", "prompt": "a city at night", "steps": "8"}});
    let resp = base
        .client()
        .post(format!("{base}/labs/api/image/presets"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let live: Value = resp.json().await.unwrap();
    round_trips::<LabPresetView>("image prompt preset", &live);
}
