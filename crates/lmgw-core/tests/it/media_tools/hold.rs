//! Under the GPU hold (design §4.4, decision 7): a row with a fallback
//! alias is answered by it over the OpenAI route, and the result says who
//! answered and what it did not take; a row without one is the hold's
//! refusal, as the call's error.

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    b64, call, cloud_alias, gateway, image_row, png, rows_of, submitted, text_of, Z_IMAGE,
};

#[tokio::test]
async fn a_held_row_with_a_fallback_is_drawn_by_it_and_says_so() {
    let sd = MockServer::start().await;
    let cloud = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created": 1, "data": [{"b64_json": b64(&png())}],
        })))
        .mount(&cloud)
        .await;
    let mut row = image_row("z", Z_IMAGE, false);
    row.fallback = Some("image-cloud.example".into());
    let (state, gw) = gateway(&sd, &[]).await;
    cloud_alias(&state, &cloud, "image-cloud.example", "image_generation").await;
    lmgw_core::store::insert_image_model(&state.db, &row)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    lmgw_core::ops::hold_set(&state, true).await.unwrap();

    let r = call(
        &gw,
        "image__generate",
        json!({"model": "image/z", "prompt": "a red square", "seed": 5, "steps": 4, "size": "64x64"}),
    )
    .await;
    assert_eq!(r["isError"], json!(false), "{r}");
    let text = text_of(&r);
    assert!(
        text.contains(
            "with image-cloud.example, the fallback for image/z while the GPU hold is on"
        ),
        "{text}"
    );
    assert!(text.contains("seed and steps were not applied"), "{text}");
    assert_eq!(r["content"][1]["type"], json!("image"));

    // The cloud got the OpenAI body and nothing smuggled into it.
    let reqs = cloud.received_requests().await.unwrap();
    let sent: serde_json::Value = reqs
        .iter()
        .find(|r| r.url.path() == "/v1/images/generations")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .expect("the fallback was asked");
    assert_eq!(
        sent,
        json!({"model": "image-cloud.example-model", "prompt": "a red square", "n": 1,
               "size": "64x64", "response_format": "b64_json"})
    );
    assert!(
        submitted(&sd).await.is_empty(),
        "no local job under the hold"
    );

    let rows = rows_of(&state, "image", 1).await;
    assert_eq!(rows[0]["images_out"], json!(1));
}

#[tokio::test]
async fn a_held_row_without_a_fallback_is_the_holds_refusal() {
    let sd = MockServer::start().await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    lmgw_core::ops::hold_set(&state, true).await.unwrap();
    let r = call(
        &gw,
        "image__generate",
        json!({"model": "image/z", "prompt": "x"}),
    )
    .await;
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(text_of(&r).contains("holding the GPU"), "{}", text_of(&r));
    assert!(submitted(&sd).await.is_empty());
}
