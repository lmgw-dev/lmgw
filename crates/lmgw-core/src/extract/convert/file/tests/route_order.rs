//! The route's own order (security round A review S7): requests through
//! `server::documents::convert` itself take their answer share before their
//! body is read and their steps' slots after, on a pool whose memory admits
//! one PDF step at three slots and answers with two shares for six callers.
//! Each request carries PDFs with embedded pictures (a geometry pair asked
//! for two slots, picture crops side by side and cut again on their own
//! slot when the output runs short) and office files read in the gateway,
//! so a step that held a slot while it waited for a share, or a share taken
//! after the slots, would hang here.

use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use http_body_util::BodyExt;

use super::pictures::build::{self, Drawn, Page};
use super::with_poppler;
use crate::extract::convert::budget::answers::{self, Answers};
use crate::extract::convert::pool::test_pool;
use crate::extract::office::MAX_UNCOMPRESSED_BYTES;
use crate::principal::Principal;
use crate::proxy::RequestCtx;

const HANG: Duration = Duration::from_secs(180);

fn pictured_pdf() -> Vec<u8> {
    let pages: Vec<Page<'_>> = (0..3u8)
        .map(|i| {
            Page::new(
                vec![(72.0, 60.0, 12.0, "Example text")],
                vec![Drawn::at(
                    60.0,
                    100.0,
                    450.0,
                    600.0,
                    (30 + u32::from(i), 40),
                    [200, 20 + 30 * i, 20],
                )],
            )
        })
        .collect();
    build::pdf(&pages)
}

/// A multipart body of `files` and `pictures=embedded`.
fn form(files: &[(&str, &[u8])]) -> (String, Vec<u8>) {
    let b = "example-boundary-7d1";
    let mut out = Vec::new();
    for (name, bytes) in files {
        out.extend_from_slice(
            format!(
                "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        out.extend_from_slice(bytes);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(
        format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"pictures\"\r\n\r\nembedded\r\n--{b}--\r\n"
        )
        .as_bytes(),
    );
    (format!("multipart/form-data; boundary={b}"), out)
}

fn ctx(id: i64) -> RequestCtx {
    RequestCtx {
        principal: Principal::Key {
            id,
            name: format!("example-{id}"),
            kind: crate::config::ApiKeyKind::Key,
            agent_id: None,
            fingerprint: String::new(),
            account: None,
            session: None,
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn requests_through_the_route_with_fewer_shares_and_slots_than_they_need_all_finish() {
    // Without poppler the PDFs are left out and the office files alone go
    // through the route, so the order is still proven, without the PDF
    // steps' slots and crops; said as the other poppler tests say a skip
    // (verification N-g).
    let poppler = with_poppler().await.is_some();
    if !poppler {
        eprintln!(
            "partly skipped: poppler-utils is not installed, so the route's order (review S7) \
             is proven with its office files only"
        );
    }
    let state = crate::state::AppState::init_for_tests().await.unwrap();
    state
        .doc_tools
        .set_search_path(std::env::var_os("PATH").unwrap_or_default());
    let pool = test_pool::small(3, 1);
    // Two shares: each request's upload is small beside its output budget.
    let shares = Answers::new(Some(2 * MAX_UNCOMPRESSED_BYTES + (8 << 20)));
    let pdf = pictured_pdf();
    let docx = crate::extract::test_files::docx(&["Hello", "World"]);
    let files: Vec<(&str, &[u8])> = if poppler {
        vec![
            ("example-1.pdf", &pdf),
            ("example-2.docx", &docx),
            ("example-3.pdf", &pdf),
            ("example-4.docx", &docx),
        ]
    } else {
        vec![("example-2.docx", &docx), ("example-4.docx", &docx)]
    };
    let want = files.len() as u64;
    let (ct, body) = form(&files);
    let body = bytes::Bytes::from(body);
    let tasks: Vec<_> = (0..6)
        .map(|c| {
            let (state, shares, ct, body) =
                (state.clone(), shares.clone(), ct.clone(), body.clone());
            tokio::spawn(test_pool::within(
                pool,
                answers::within(shares, async move {
                    let req = Request::builder()
                        .method("POST")
                        .uri(crate::server::documents::PATH)
                        .header("content-type", ct)
                        .header("content-length", body.len())
                        .body(Body::from(body))
                        .unwrap();
                    let r = crate::server::documents::convert(
                        State(state),
                        axum::Extension(ctx(c)),
                        req,
                    )
                    .await;
                    let status = r.status().as_u16();
                    let bytes = r.into_body().collect().await.unwrap().to_bytes();
                    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    (status, v["summary"]["converted"].as_u64())
                }),
            ))
        })
        .collect();
    let done = tokio::time::timeout(HANG, futures::future::join_all(tasks))
        .await
        .expect("no request waits for ever");
    for d in done {
        assert_eq!(d.unwrap(), (200, Some(want)));
    }
    // The bodies' shares go back once their last chunk is taken.
    for _ in 0..100 {
        if shares.facts().holding == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (a, p) = (shares.facts(), pool.facts());
    assert_eq!((a.holding, a.waiting, p.running, p.waiting), (0, 0, 0, 0));
}
