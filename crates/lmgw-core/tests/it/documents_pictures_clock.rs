//! Picture descriptions (WP11) and the file's clock (`pool::clock`):
//! `document_timeout` in the stage keeps what was answered, an e-mail's
//! attachments after the deadline are left and named once a picture stage
//! took the message's end over, and a client that goes away closes its
//! calls.

use std::sync::atomic::Ordering;
use std::time::Duration;

use base64::Engine as _;
use serde_json::json;

use crate::common::serve;
use crate::documents_model_help::{convert, settings, state_on, Model, ALIAS, PATH};
use crate::documents_model_review::{silent_upstream, until};
use crate::documents_pictures::{big, describing, docx, picture_rows, request, Pic};

/// The deadline in the stage ends the calls still going and keeps what was
/// answered; the file is partial, never failed for it.
#[tokio::test]
async fn the_deadline_in_the_stage_keeps_the_descriptions_answered() {
    let m = Model::start(Duration::from_millis(1000)).await;
    let state = describing(&m).await;
    settings(&state, |s| s.documents.tool_slots = 1).await;
    let gw = serve(state.clone()).await;
    let pics: Vec<Pic> = (0..5).map(|i| big(400 + i, 300)).collect();
    let started = std::time::Instant::now();
    let (status, a) = convert(
        &gw,
        &request(
            &docx(&pics),
            json!({"output": "json", "document_timeout": 2.5}),
        ),
    )
    .await;
    assert_eq!(status, 200, "{a}");
    assert!(
        started.elapsed() < Duration::from_millis(4500),
        "the stage ended at the deadline: {:?}",
        started.elapsed()
    );
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial", "{d}");
    let described = d["pictures"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["description"].is_string())
        .count();
    assert!((1..5).contains(&described), "{d}");
    let late = d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|w| w["message"].as_str())
        .find(|w| w.contains("document_timeout of 2.5 s"))
        .unwrap_or_else(|| panic!("the pictures left are named: {d}"));
    assert!(
        late.starts_with("the picture-description model did not describe"),
        "{late}"
    );
    assert!(
        m.bodies().await.len() < 5,
        "no picture was sent after the deadline"
    );
}

/// A client that goes away: the call in flight closes its connection,
/// leaves the gauge closed and a `499` row; the pictures waiting are never
/// sent, and the tool pool is left empty.
#[tokio::test]
async fn a_cancelled_request_closes_its_calls_and_sends_no_more() {
    let (base, arrived, closed) = silent_upstream().await;
    let state = state_on(&base, false).await;
    settings(&state, |s| {
        s.documents.picture_alias = ALIAS.into();
        s.documents.tool_slots = 1;
    })
    .await;
    let gw = serve(state.clone()).await;
    let body = request(
        &docx(&[big(400, 300), big(300, 200), big(200, 100)]),
        json!({}),
    );
    let client = gw.client();
    let url = format!("{gw}{PATH}");
    let call = tokio::spawn(async move { client.post(url).json(&body).send().await });
    until("the picture call never reached the upstream", || {
        arrived.load(Ordering::SeqCst) == 1
    })
    .await;
    call.abort();
    let _ = call.await;
    until("the dropped call's connection stayed open", || {
        closed.load(Ordering::SeqCst) == 1
    })
    .await;
    let rows = picture_rows(&state, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (rows[0].status, rows[0].error_kind.as_deref()),
        (499, Some("client_disconnected"))
    );
    let telemetry = &state.telemetry;
    until("the in-flight gauge still counts the call", || {
        telemetry.stats().active_requests == 0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        arrived.load(Ordering::SeqCst),
        1,
        "no picture after the client left"
    );
    let pool = lmgw_core::extract::convert::pool::global().facts();
    assert_eq!((pool.running, pool.waiting), (0, 0), "{pool:?}");
}

/// A message with `attachments` (name, content type, bytes).
pub(crate) fn mail(attachments: &[(&str, &str, Vec<u8>)]) -> Vec<u8> {
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let mut m = String::from(
        "From: ann@example.com\r\nTo: bob@example.com\r\nSubject: Example\r\nMIME-Version: \
         1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: \
         text/plain\r\n\r\nSee the files.\r\n",
    );
    for (name, ty, bytes) in attachments {
        m.push_str(&format!(
            "--b\r\nContent-Type: {ty}\r\nContent-Disposition: attachment; \
             filename=\"{name}\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n",
            b64(bytes)
        ));
    }
    m.push_str("--b--\r\n");
    m.into_bytes()
}

/// S1: an attachment's picture stage takes the message's end over, so the
/// batch no longer ends the message at its deadline; the message does: the
/// attachment after the deadline is not read, and is named.
#[tokio::test]
async fn an_email_s_attachments_after_the_deadline_are_left_and_named() {
    let m = Model::start(Duration::from_millis(4000)).await;
    let state = describing(&m).await;
    // lmgw's reader reads the message: the one cargo built beside the tests.
    let mut dirs: Vec<std::path::PathBuf> =
        std::env::split_paths(&lmgw_core::extract::convert::reader::own_dirs()).collect();
    dirs.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    state
        .doc_tools
        .set_search_path(std::env::join_paths(dirs).unwrap());
    let gw = serve(state.clone()).await;
    let message = mail(&[
        (
            "first.docx",
            "application/octet-stream",
            docx(&[big(400, 300)]),
        ),
        (
            "second.txt",
            "text/plain",
            b"Example second attachment.".to_vec(),
        ),
    ]);
    let body = json!({
        "files": [{"name": "example.eml",
                   "data_base64": base64::engine::general_purpose::STANDARD.encode(&message)}],
        "describe_pictures": true,
        "document_timeout": 2.0,
    });
    let started = std::time::Instant::now();
    let (status, a) = convert(&gw, &body).await;
    assert_eq!(status, 200, "{a}");
    assert!(
        started.elapsed() < Duration::from_millis(3800),
        "the message ended at its deadline: {:?}",
        started.elapsed()
    );
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial", "{d}");
    let content = d["content"].as_str().unwrap_or_default();
    assert!(
        !content.contains("Example second attachment"),
        "the attachment after the deadline was read: {content}"
    );
    let warnings: Vec<&str> = d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|w| w["message"].as_str())
        .collect();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("second.txt") && w.contains("document_timeout of 2 s")),
        "{warnings:?}"
    );
    assert_eq!(m.bodies().await.len(), 1);
}
