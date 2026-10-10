//! The Image lab's routes against their documented types
//! (`lmgw-api-types::image_lab`): each answer, read into its type and
//! written back, is the answer again, key by key. A field the gateway adds
//! to an answer without adding it to the type is dropped by the read and
//! fails here.

use lmgw_api_types::image_lab::{ImageEditResult, ImageGenerateResult, ImageLabModels};
use lmgw_api_types::ImageCapabilities;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::round_trips;
use crate::image_lab::{image_row, mount_ready, one_png, setup_cloud, setup_local};

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
        .post(format!("{base}/image-lab/api/generate"))
        .json(
            &json!({"model": "image/z-image-turbo", "prompt": "a cat", "width": "512",
                       "height": "512"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let live: Value = resp.json().await.unwrap();
    let made = round_trips::<ImageGenerateResult>("generate", &live);
    assert_eq!(made.endpoint, "/v1/images/generations");
    assert_eq!(made.request["size"], "512x512");

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
        .post(format!("{base}/image-lab/api/edit"))
        .multipart(upload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let live: Value = resp.json().await.unwrap();
    let edited = round_trips::<ImageEditResult>("edit", &live);
    assert_eq!(edited.request.files[0].name, "image");
    assert_eq!(edited.request.files[1].content_type, None);
}
