//! A local row's image call over sd-server's native job API (design §4.1),
//! reached through `/mcp`: the body it builds, the poll to the job's end,
//! its errors and its one `request_logs` row.

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    b64, call, gateway, image_row, job, mount_sd, png, rows_of, source_png, submitted, text_of,
    Z_IMAGE,
};

#[tokio::test]
async fn a_generation_maps_its_parameters_polls_the_job_and_writes_one_row() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("queued"), job("generating"), job("completed")]).await;
    let (state, gw) = gateway(&sd, &[image_row("z-image-turbo", Z_IMAGE, false)]).await;

    let r = call(
        &gw,
        "image__generate",
        json!({
            "model": "image/z-image-turbo", "prompt": "a red square",
            "negative_prompt": "blur", "size": "512x384", "steps": 6, "cfg": 2.5,
        }),
    )
    .await;
    assert_eq!(r["isError"], json!(false), "{r}");
    let text = text_of(&r);
    assert!(
        text.starts_with("Made an image (64x64) with image/z-image-turbo: seed "),
        "{text}"
    );
    assert!(text.contains("6 steps, cfg 2.5"), "{text}");
    assert!(
        text.contains("(1.0 s of it drawing)"),
        "the job's own time: {text}"
    );
    assert_eq!(r["content"][1]["type"], json!("image"));
    assert_eq!(r["content"][1]["data"], json!(b64(&png())));

    let sent = submitted(&sd).await;
    assert_eq!(sent.len(), 1);
    let body = &sent[0];
    assert_eq!(body["prompt"], json!("a red square"));
    assert_eq!(body["negative_prompt"], json!("blur"));
    assert_eq!(
        (body["width"].clone(), body["height"].clone()),
        (json!(512), json!(384))
    );
    assert_eq!(body["sample_params"]["sample_steps"], json!(6));
    assert_eq!(body["sample_params"]["guidance"]["txt_cfg"], json!(2.5));
    let seed = body["seed"]
        .as_i64()
        .expect("a seed is drawn when none was set");
    assert!((0..=i64::from(i32::MAX)).contains(&seed));
    assert!(text.contains(&format!("seed {seed}")), "{text}");
    assert!(body.get("ref_images").is_none() && body.get("init_image").is_none());

    let rows = rows_of(&state, "image", 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["alias"], json!("image/z-image-turbo"));
    assert_eq!(rows[0]["images_out"], json!(1));
    assert_eq!(rows[0]["status"], json!(200));
}

#[tokio::test]
async fn unset_parameters_are_left_to_the_row() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let (_state, gw) = gateway(&sd, &[image_row("z-image-turbo", Z_IMAGE, false)]).await;
    let r = call(
        &gw,
        "image__generate",
        json!({"model": "image/z-image-turbo", "prompt": "x", "seed": 7}),
    )
    .await;
    assert_eq!(r["isError"], json!(false), "{r}");
    let body = &submitted(&sd).await[0];
    let keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut keys = keys;
    keys.sort();
    // The quick sketch is asked for so the job reports its steps (§5.2).
    assert_eq!(
        keys,
        vec![
            "batch_count",
            "output_format",
            "preview",
            "preview_interval",
            "prompt",
            "seed"
        ]
    );
    assert_eq!(body["seed"], json!(7));
    // What the row used comes from the server's own defaults.
    assert!(
        text_of(&r).contains("seed 7, 8 steps, cfg 1"),
        "{}",
        text_of(&r)
    );
}

#[tokio::test]
async fn an_edit_row_takes_the_picture_as_ref_images() {
    // A row that cannot edit is not offered (`edit_eligibility`).
    let picture = format!("data:image/png;base64,{}", b64(&source_png()));
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let (_state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, true)]).await;
    let r = call(
        &gw,
        "image__edit",
        json!({"model": "image/z", "prompt": "make it blue", "image": picture}),
    )
    .await;
    assert_eq!(r["isError"], json!(false), "{r}");
    let body = &submitted(&sd).await[0];
    assert!(body.get("ref_images").is_some(), "{body}");
    assert!(
        body.get("init_image").is_none() && body.get("strength").is_none(),
        "{body}"
    );
    assert!(text_of(&r).contains("as an edit"), "{}", text_of(&r));
}

#[tokio::test]
async fn a_full_queue_a_failed_job_and_a_bad_size_come_back_as_tool_errors() {
    // The queue is full.
    let sd = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_string(super::CAPS))
        .mount(&sd)
        .await;
    Mock::given(method("POST"))
        .and(path("/sdcpp/v1/img_gen"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({"error": "queue full"})))
        .mount(&sd)
        .await;
    let (_state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let r = call(
        &gw,
        "image__generate",
        json!({"model": "image/z", "prompt": "x"}),
    )
    .await;
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(
        text_of(&r).contains("queue is full (64 jobs"),
        "{}",
        text_of(&r)
    );

    // The job failed.
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("generating"), job("failed")]).await;
    let (_state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let r = call(
        &gw,
        "image__generate",
        json!({"model": "image/z", "prompt": "x"}),
    )
    .await;
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(text_of(&r).contains("out of memory"), "{}", text_of(&r));

    // A size the model cannot read, and a model outside the enum.
    let r = call(
        &gw,
        "image__generate",
        json!({"model": "image/z", "prompt": "x", "size": "large"}),
    )
    .await;
    assert!(text_of(&r).contains("WIDTHxHEIGHT"), "{}", text_of(&r));
    let r = call(
        &gw,
        "image__generate",
        json!({"model": "image/other", "prompt": "x"}),
    )
    .await;
    assert!(
        text_of(&r).contains("pick one of: image/z"),
        "{}",
        text_of(&r)
    );
}

#[tokio::test]
async fn a_job_past_the_routes_ceiling_is_a_timeout() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("generating")]).await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let mut s = state.snapshot().settings.clone();
    s.image.request_timeout_seconds = 1;
    lmgw_core::store::save_settings(&state.db, &s)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();

    let r = call(
        &gw,
        "image__generate",
        json!({"model": "image/z", "prompt": "x"}),
    )
    .await;
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(
        text_of(&r).to_lowercase().contains("timed out") || text_of(&r).contains("timeout"),
        "{}",
        text_of(&r)
    );
}

#[tokio::test]
async fn att_refs_name_nothing_without_a_thread() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let (_state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, true)]).await;
    let r = call(
        &gw,
        "image__edit",
        json!({"model": "image/z", "prompt": "x", "image": "att:3"}),
    )
    .await;
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(
        text_of(&r).contains("pass the file as a data: URL"),
        "{}",
        text_of(&r)
    );
    assert!(submitted(&sd).await.is_empty(), "nothing was sent");
}
