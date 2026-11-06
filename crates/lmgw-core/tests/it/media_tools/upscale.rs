//! `image__upscale` (design §3.2, §3.3, §9), through `/mcp`: offered only
//! when the image models dir's `upscalers/` holds a file, its `upscaler`
//! enum the file stems; sd-server's `POST /sdcpp/v1/upscale` on a local
//! row with one `request_logs` row; refused under the hold's swap and on a
//! server that reports no upscaler.

use lmgw_core::state::SharedState;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    b64, call, cloud_alias, gateway, image_row, mcp, png, rows_of, text_of, CAPS, Z_IMAGE,
};

pub(crate) const UPSCALER: &str = "RealESRGAN_x4plus";

/// The spike's capabilities as a server that found [`UPSCALER`] reports
/// them.
pub(crate) fn caps_with_upscaler() -> Value {
    let mut caps: Value = serde_json::from_str(CAPS).unwrap();
    caps["upscale"] = json!(true);
    caps["upscalers"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name": UPSCALER, "image_upscale": true}));
    caps
}

/// Put [`UPSCALER`] where sd-server scans.
pub(crate) fn put_upscaler(state: &SharedState) {
    let dir = std::path::Path::new(&state.snapshot().settings.image.models_dir).join("upscalers");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{UPSCALER}.safetensors")), b"weights").unwrap();
}

/// sd-server on `sd` with `caps`, answering an upscale with a picture.
pub(crate) async fn mount_upscale(sd: &MockServer, caps: Value) {
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(caps))
        .mount(sd)
        .await;
    Mock::given(method("POST"))
        .and(path("/sdcpp/v1/upscale"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "images": [{"index": 0, "b64_json": b64(&png())}],
            "upscaler": UPSCALER, "scale": 4, "repeats": 1,
            "width": 256, "height": 256, "output_format": "png",
        })))
        .mount(sd)
        .await;
}

/// The JSON bodies sd-server was sent at `/sdcpp/v1/upscale`.
pub(crate) async fn upscales(sd: &MockServer) -> Vec<Value> {
    sd.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/sdcpp/v1/upscale")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn tool_names(list: &Value) -> Vec<String> {
    list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn the_tool_is_offered_only_with_an_upscaler_on_disk() {
    let sd = MockServer::start().await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let list = mcp(&gw, "tools/list", json!({})).await;
    assert!(
        !tool_names(&list).contains(&"image__upscale".to_string()),
        "{list}"
    );

    put_upscaler(&state);
    let list = mcp(&gw, "tools/list", json!({})).await;
    let tool = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "image__upscale")
        .unwrap_or_else(|| panic!("listed once a file is there: {list}"))
        .clone();
    assert_eq!(
        tool["inputSchema"]["properties"]["upscaler"]["enum"],
        json!([UPSCALER])
    );
    assert_eq!(tool["inputSchema"]["required"], json!(["image"]));
    assert_eq!(
        tool["inputSchema"]["properties"]["repeats"]["maximum"],
        json!(4)
    );
}

#[tokio::test]
async fn an_upscale_runs_on_a_local_row_and_writes_one_row() {
    let sd = MockServer::start().await;
    mount_upscale(&sd, caps_with_upscaler()).await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    put_upscaler(&state);
    let picture = format!("data:image/png;base64,{}", b64(&png()));

    let r = call(
        &gw,
        "image__upscale",
        json!({"image": picture, "upscaler": UPSCALER, "repeats": 2}),
    )
    .await;
    assert_eq!(r["isError"], json!(false), "{r}");
    let text = text_of(&r);
    assert!(
        text.starts_with(&format!(
            "Made an image (256x256) by upscaling the picture with {UPSCALER} (4x) on image/z:"
        )),
        "{text}"
    );
    assert!(text.ends_with("It is in this result."), "{text}");
    assert_eq!(r["content"][1]["type"], json!("image"));

    let sent = upscales(&sd).await;
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0]["image"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"),
        "{}",
        sent[0]
    );
    assert_eq!(sent[0]["upscaler"], json!(UPSCALER));
    assert_eq!(sent[0]["repeats"], json!(2));
    assert_eq!(sent[0]["output_format"], json!("png"));

    let rows = rows_of(&state, "image", 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["alias"], json!("image/z"));
    assert_eq!(rows[0]["images_out"], json!(1));
    assert_eq!(rows[0]["status"], json!(200));

    // Arguments outside what the server and the disk take.
    let r = call(
        &gw,
        "image__upscale",
        json!({"image": picture, "upscaler": "other"}),
    )
    .await;
    assert!(
        text_of(&r).contains(&format!("pick one of: {UPSCALER}")),
        "{r}"
    );
    let r = call(
        &gw,
        "image__upscale",
        json!({"image": picture, "repeats": 5}),
    )
    .await;
    assert!(text_of(&r).contains("outside 1 to 4"), "{r}");
    assert_eq!(upscales(&sd).await.len(), 1, "nothing more was sent");
}

#[tokio::test]
async fn a_server_that_still_reports_no_upscaler_when_asked_again_says_so() {
    let sd = MockServer::start().await;
    // 0518b34's own capabilities: `upscale: false`, on every read.
    mount_upscale(&sd, serde_json::from_str(CAPS).unwrap()).await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    put_upscaler(&state);
    let picture = format!("data:image/png;base64,{}", b64(&png()));
    let r = call(&gw, "image__upscale", json!({"image": picture})).await;
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(
        text_of(&r).contains(&format!(
            "reports no upscaler it can run although upscalers/ holds {UPSCALER}"
        )),
        "{}",
        text_of(&r)
    );
    assert!(upscales(&sd).await.is_empty());
}

#[tokio::test]
async fn the_holds_swap_is_refused_rather_than_sent_to_a_fallback() {
    let sd = MockServer::start().await;
    let cloud = MockServer::start().await;
    mount_upscale(&sd, caps_with_upscaler()).await;
    let mut row = image_row("z", Z_IMAGE, false);
    row.fallback = Some("image-cloud.example".into());
    let (state, gw) = gateway(&sd, &[]).await;
    cloud_alias(&state, &cloud, "image-cloud.example", "image_generation").await;
    lmgw_core::store::insert_image_model(&state.db, &row)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    put_upscaler(&state);
    lmgw_core::ops::hold_set(&state, true).await.unwrap();

    let picture = format!("data:image/png;base64,{}", b64(&png()));
    let r = call(&gw, "image__upscale", json!({"image": picture})).await;
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(
        text_of(&r).contains("an upscale has no fallback"),
        "{}",
        text_of(&r)
    );
    assert!(upscales(&sd).await.is_empty());
    let asked: Vec<String> = cloud
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_string())
        .filter(|p| p.contains("/images/"))
        .collect();
    assert!(asked.is_empty(), "the fallback was not asked: {asked:?}");
}

/// The shipped upscaler lands in `upscalers/<file>`, where sd-server scans,
/// through the download queue; listed in `image_recipes`, and not queued
/// twice. One test fn because it owns the process-wide `HF_ENDPOINT`.
#[tokio::test]
async fn the_shipped_upscaler_downloads_into_the_directory_sd_server_scans() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let u = lmgw_core::image_upscalers::find("realesrgan-x4plus").unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/api/models/{}/tree/main", u.repo)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"type": "file", "path": u.file, "size": 7},
        ])))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/{}/resolve/main/{}", u.repo, u.file)))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"e1\"")
                .set_body_bytes(b"weights".to_vec()),
        )
        .mount(&hub)
        .await;
    let sd = MockServer::start().await;
    let (state, _gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;

    let added = lmgw_core::ops::image_upscaler_add(&state, "realesrgan-x4plus")
        .await
        .unwrap();
    let queued = added.download.expect("queued");
    assert_eq!(queued.dest_path, "upscalers/RealESRGAN_x4plus.safetensors");
    for _ in 0..200 {
        let rows = lmgw_core::store::list_hf_models(&state.db).await.unwrap();
        if rows.iter().all(|r| r.status == "done") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let models_dir = state.snapshot().settings.image.models_dir.clone();
    assert!(std::path::Path::new(&models_dir)
        .join("upscalers/RealESRGAN_x4plus.safetensors")
        .is_file());
    assert_eq!(
        lmgw_core::image_upscalers::names(&models_dir),
        vec![UPSCALER.to_string()]
    );

    let view = lmgw_core::ops::image_recipes(&state).await.unwrap();
    let entry = &view.upscalers.shipped[0];
    assert!(entry.present && entry.done, "{entry:?}");
    assert_eq!(view.upscalers.on_disk[0].name, UPSCALER);

    let again = lmgw_core::ops::image_upscaler_add(&state, "realesrgan-x4plus")
        .await
        .unwrap();
    assert!(
        again.already_present && again.download.is_none(),
        "{again:?}"
    );
    let bad = lmgw_core::ops::image_upscaler_add(&state, "nope")
        .await
        .unwrap_err();
    assert!(bad.contains("realesrgan-x4plus"), "{bad}");
}
