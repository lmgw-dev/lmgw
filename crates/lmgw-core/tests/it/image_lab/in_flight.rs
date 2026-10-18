//! An Image lab group while its run is out (labs-store design §5.1): the
//! group is in flight from its claim to its run, the history says so, and
//! its delete is the 409 until then; a group deleted meanwhile still gets
//! its pictures back (not kept, `store_error` saying why); a client gone
//! once the route has answered does not take the picture with it; a kept
//! picture ref names a picture and goes out under its kept name.

use std::time::Duration;

use lmgw_api_types::lab_store::{LabCell, LabKind};
use lmgw_core::labs::record::{self, NewArtifact, NewGroup};
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    edit, image_row, mount_ready, one_png, run_group, setup_cloud, setup_local, text_json, PNG_B64,
};
use crate::common::Gw;

/// `POST /image-lab/api/groups` with a generation's form: the group's id.
async fn make_group(base: &Gw) -> i64 {
    let resp = base
        .client()
        .post(format!("{base}/image-lab/api/groups"))
        .json(&json!({"model": "image/z-image-turbo", "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let g: Value = resp.json().await.unwrap();
    g["id"].as_i64().unwrap()
}

/// Group `id` as the lab store answers it now.
async fn read(base: &Gw, id: i64) -> (u16, Value) {
    let resp = base
        .client()
        .get(format!("{base}/labs/api/image/groups/{id}"))
        .send()
        .await
        .unwrap();
    (
        resp.status().as_u16(),
        resp.json().await.unwrap_or(Value::Null),
    )
}

/// Wait until group `id` reads `in_flight` as `want`.
async fn until_in_flight(base: &Gw, id: i64, want: bool) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (_, g) = read(base, id).await;
        if g["in_flight"] == want {
            return g;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "group {id} never read in_flight {want}: {g}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// An sd-server whose generation answers after `delay`.
async fn slow_generations(delay: Duration) -> MockServer {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(one_png())
                .set_delay(delay),
        )
        .mount(&mock)
        .await;
    mock
}

#[tokio::test]
async fn a_group_is_in_flight_while_its_run_is_out_and_cannot_be_deleted_until_then() {
    let mock = slow_generations(Duration::from_millis(800)).await;
    let (_state, base) = setup_local(&mock, &[image_row("z-image-turbo", false)]).await;
    let id = make_group(&base).await;
    let (_, g) = read(&base, id).await;
    assert_eq!(g["in_flight"], false, "made, not sent: {g}");

    let running = {
        let base = base.clone();
        tokio::spawn(async move { run_group(&base, id).await })
    };
    let g = until_in_flight(&base, id, true).await;
    assert!(g["runs"].as_array().unwrap().is_empty(), "{g}");
    // The history lists it in flight, not as stopped.
    let page: Value = base
        .client()
        .get(format!("{base}/labs/api/image/groups"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["groups"][0]["id"], id);
    assert_eq!(page["groups"][0]["in_flight"], true, "{page}");
    // Its delete waits for its run: the 409, nothing deleted.
    let resp = base
        .client()
        .post(format!("{base}/labs/api/image/groups/{id}/delete"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let e: Value = resp.json().await.unwrap();
    assert_eq!(e["code"], "in_flight", "{e}");
    // A second run of it is the 409 too, sent nowhere.
    let again = run_group(&base, id).await;
    assert_eq!(again.status(), 409);

    let resp = running.await.unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["group"]["in_flight"], false);
    let (_, g) = read(&base, id).await;
    assert_eq!(g["in_flight"], false, "its run is in: {g}");
    assert_eq!(g["runs"][0]["http_status"], 200);
    let resp = base
        .client()
        .post(format!("{base}/labs/api/image/groups/{id}/delete"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "deleted once its run is in");
    let generations = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/v1/images/generations")
        .count();
    assert_eq!(generations, 1);
}

#[tokio::test]
async fn a_group_cleared_mid_generation_still_answers_its_pictures() {
    let mock = slow_generations(Duration::from_millis(600)).await;
    let (_state, base) = setup_local(&mock, &[image_row("z-image-turbo", false)]).await;
    let id = make_group(&base).await;
    let running = {
        let base = base.clone();
        tokio::spawn(async move { run_group(&base, id).await })
    };
    until_in_flight(&base, id, true).await;
    // Clear is not refused: it empties the lab, the run in flight with it.
    let resp = base
        .client()
        .post(format!("{base}/labs/api/image/clear"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = running.await.unwrap();
    assert_eq!(resp.status(), 200, "the answer is never dropped");
    let v: Value = resp.json().await.unwrap();
    let store_error = v["store_error"].as_str().expect("not kept");
    assert!(store_error.contains("deleted"), "{store_error}");
    assert_eq!(v["group"]["id"], id);
    let run = &v["group"]["runs"][0];
    assert!(run["id"].is_null(), "{run}");
    assert_eq!(run["http_status"], 200);
    assert_eq!(
        text_json(&run["response"])["data"][0]["b64_json"],
        PNG_B64,
        "the picture inline, as the route answered it"
    );
    // Nothing of it stayed behind: no group, no file.
    let (status, _) = read(&base, id).await;
    assert_eq!(status, 404);
    let usage: Value = base
        .client()
        .get(format!("{base}/labs/api/image/usage"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(usage["artifact_files"], 0, "{usage}");
    assert_eq!(
        usage["orphan_files"], 0,
        "the written picture went: {usage}"
    );
}

/// The page stops once sd-server has answered: the answer is read and kept
/// on a task of its own, so the picture is the group's all the same.
#[tokio::test]
async fn a_client_gone_once_the_route_answered_does_not_take_the_picture() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .mount(&mock)
        .await;
    let (state, base) = setup_local(&mock, &[image_row("z-image-turbo", false)]).await;
    let id = make_group(&base).await;

    // The record waits for the artifacts lock while the client goes.
    let held = lmgw_core::labs::artifacts::hold().await;
    let client = {
        let base = base.clone();
        tokio::spawn(async move { run_group(&base, id).await })
    };
    // The route's log row is written once its answer has been relayed whole
    // into the lab, which by then reads it on its own task.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while store::query_logs(&state.db, &store::LogFilter::default())
        .await
        .unwrap()
        .is_empty()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the route never answered"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    client.abort();
    let _ = client.await;
    // Long enough for the gateway to see the client go and drop its handler.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (_, g) = read(&base, id).await;
    assert_eq!(g["in_flight"], true, "the record holds the claim: {g}");
    assert!(g["runs"].as_array().unwrap().is_empty());
    drop(held);

    let g = until_in_flight(&base, id, false).await;
    let run = &g["runs"][0];
    assert_eq!(run["http_status"], 200, "{g}");
    let art = &run["artifacts"][0];
    assert_eq!(art["mime"], "image/png", "{g}");
    assert!(!run["id"].is_null());
}

/// A kept picture ref names a picture: an artifact of another type is the
/// 400 naming it and its type. A ref's file goes out under the name the store
/// keeps it by, not the label the page sent with it.
#[tokio::test]
async fn a_kept_ref_is_a_picture_and_goes_out_under_its_kept_name() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/edits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .mount(&mock)
        .await;
    let (state, base) = setup_cloud(&mock).await;
    // A text artifact the store keeps (a decision group's input).
    let (_, ids) = record::group(
        &state.db,
        &state.data_dir,
        NewGroup {
            lab: LabKind::Decision,
            title: "t".into(),
            plan: vec![LabCell {
                model: "decide/laya".into(),
                variant: "as_written".into(),
            }],
            inputs: vec![NewArtifact::Bytes {
                name: "notes.txt".into(),
                bytes: bytes::Bytes::from_static(b"just some text"),
                declared_mime: None,
                data_url_head: None,
            }],
        },
        |_| Ok(("{}".into(), None)),
    )
    .await
    .unwrap();
    let text_id = ids[0];
    let form = json!({"model": "my-edit", "prompt": "x",
                      "image": {"id": text_id, "name": "notes.txt"}});
    let resp = edit(
        &base,
        reqwest::multipart::Form::new().text("form", form.to_string()),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let e: Value = resp.json().await.unwrap();
    let message = e["error"]["message"].as_str().unwrap();
    assert!(message.contains(&format!("lab-artifact:{text_id}")), "{e}");
    assert!(message.contains("text/plain"), "{e}");
    assert_eq!(e["error"]["code"], "image_lab_form");

    // A picture, kept: the page's label for it is not what goes out.
    let upload = reqwest::multipart::Form::new()
        .text(
            "form",
            json!({"model": "my-edit", "prompt": "night"}).to_string(),
        )
        .part(
            "image",
            reqwest::multipart::Part::bytes(b"\x89PNG\r\n\x1a\nimage".to_vec())
                .file_name("in.png")
                .mime_str("image/png")
                .unwrap(),
        );
    let v: Value = edit(&base, upload).await.json().await.unwrap();
    let kept = v["group"]["artifacts"][0]["id"].as_i64().unwrap();
    let again = json!({"model": "my-edit", "prompt": "dawn",
                       "image": {"id": kept, "name": "a.png\r\nX-Evil: 1"}});
    let resp = edit(
        &base,
        reqwest::multipart::Form::new().text("form", again.to_string()),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["group"]["form"]["image"]["name"], "in.png", "{v}");
    let reqs = mock.received_requests().await.unwrap();
    let body = reqs
        .iter()
        .filter(|r| r.url.path() == "/v1/images/edits")
        .nth(1)
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .unwrap();
    assert!(body.contains("filename=\"in.png\""), "{body}");
    assert!(!body.contains("X-Evil"), "{body}");
}
