//! When the hub refuses, and when the connection is metered (D66, D68).
//!
//! - A 401 (a gated repo), a 429 (the rate limit) or a hub that can't be
//!   reached answers the first header read: no second read is asked, the
//!   files are marked by their size, nothing is recommended, and the list's
//!   notes say why in plain words.
//! - On a metered connection the dashboard's own ask (`unless_metered`)
//!   reads nothing and marks by size; an explicit `fit=true` reads (D59).

use std::sync::Arc;

use lmgw_api_types as dto;
use lmgw_api_types::model_find::FitMark;
use lmgw_core::ops;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common;
use crate::plan_hub::{hub, on_card, quants, MIB, REPO};

/// A hub listing `files` whose every resolve answers `answer`.
async fn refusing(files: &[(String, Vec<u8>)], answer: ResponseTemplate) -> MockServer {
    let hub = MockServer::start().await;
    let listing: Vec<serde_json::Value> = files
        .iter()
        .map(|(p, b)| {
            serde_json::json!({"type": "file", "path": p, "size": b.len(),
                   "lfs": {"oid": format!("{:064x}", b.len()), "size": b.len()}})
        })
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("/api/models/{REPO}/tree/main")))
        .respond_with(ResponseTemplate::new(200).set_body_json(listing))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(format!("^/{REPO}/resolve/main/.*")))
        .respond_with(answer)
        .mount(&hub)
        .await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    hub
}

/// The resolve requests `hub` was asked.
async fn resolves(hub: &MockServer) -> usize {
    hub.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().contains("/resolve/"))
        .count()
}

/// One refusal stops the reads: one request, every file by its size,
/// nothing recommended, the reason in the list's notes.
async fn stops_at_once(answer: ResponseTemplate, says: &str) {
    let files = quants();
    let hub = refusing(&files, answer).await;
    let state = on_card(192 * MIB, 0, false).await;
    let listed = ops::hf_repo(&state, REPO, None, "chat", dto::RepoFit::Headers)
        .await
        .unwrap();
    assert_eq!(resolves(&hub).await, 1, "{:?}", listed.fit_notes);
    assert_eq!(listed.recommended, None);
    let notes = listed.fit_notes.join("\n");
    assert!(
        notes.contains("Hugging Face stopped the header reads"),
        "{notes}"
    );
    assert!(notes.contains(says), "{notes}");
    let picks = listed.picks.unwrap();
    assert!(
        picks.beside.why.contains("stopped the header reads"),
        "{picks:?}"
    );
    for f in &listed.files {
        let fit = f.fit.as_ref().unwrap();
        assert!(
            matches!(
                fit.whole_card.mark,
                FitMark::FitsBySize | FitMark::DoesNotFitBySize
            ),
            "{f:?}"
        );
        assert_eq!(f.estimated_from, None);
    }
}

#[tokio::test]
async fn a_gated_repo_is_asked_once() {
    let _env = common::process_env_lock().await;
    stops_at_once(ResponseTemplate::new(401), "gated repo").await;
}

#[tokio::test]
async fn the_rate_limit_is_asked_once() {
    let _env = common::process_env_lock().await;
    stops_at_once(
        ResponseTemplate::new(429).insert_header("retry-after", "120"),
        "rate limit reached",
    )
    .await;
}

#[tokio::test]
async fn a_hub_that_cant_be_reached_is_asked_once() {
    let _env = common::process_env_lock().await;
    // The CDN the hub sends to is a port nothing listens on.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let gone = format!("http://{}/cdn/x", closed.local_addr().unwrap());
    drop(closed);
    stops_at_once(
        ResponseTemplate::new(302).insert_header("location", gone.as_str()),
        "GET http://127.0.0.1",
    )
    .await;
}

/// A stated machine whose default route NetworkManager calls metered.
fn metered_machine() -> Arc<lmgw_machine::fake::FakeFacts> {
    let f = lmgw_machine::fake::FakeFacts::new();
    f.file(
        "/proc/net/route",
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
         eth0\t00000000\t010200C0\t0003\t0\t0\t425\t00000000\t0\t0\t0\n",
    );
    f.program("/usr/bin/nmcli", |_| {
        Ok(lmgw_machine::CmdOut::success(
            "GENERAL.METERED:yes (guessed)\n",
        ))
    });
    Arc::new(f)
}

/// D123: the page's own ask reads nothing on a metered connection, marks by
/// size and recommends nothing, saying why; `fit=true` reads (D59).
#[tokio::test]
async fn a_metered_connection_reads_only_when_asked() {
    let _env = common::process_env_lock().await;
    let files = quants();
    let (_hub, _cdn_server, cdn) = hub(&files, &[]).await;
    let state = on_card(192 * MIB, 0, false).await;
    state.machine.set_machine(
        metered_machine(),
        Arc::new(|| None::<Result<Vec<lmgw_core::vram::GpuMemory>, String>>),
    );
    let page = ops::hf_repo(&state, REPO, None, "chat", dto::RepoFit::UnlessMetered)
        .await
        .unwrap();
    assert!(cdn.asked().is_empty(), "{:?}", cdn.asked());
    assert_eq!(page.header_bytes_read, 0);
    assert_eq!(page.recommended, None);
    let why = page.metered.clone().expect("the metered sentence");
    assert!(why.contains("eth0: yes (guessed)"), "{why}");
    assert!(page.card.is_some());
    for f in &page.files {
        let fit = f.fit.as_ref().unwrap();
        assert_eq!(fit.whole_card.mark, FitMark::FitsBySize, "{f:?}");
        assert!(fit
            .whole_card
            .notes
            .iter()
            .any(|n| n.contains("the connection is metered")));
    }
    let asked = ops::hf_repo(&state, REPO, None, "chat", dto::RepoFit::Headers)
        .await
        .unwrap();
    assert!(!cdn.asked().is_empty());
    assert_eq!(asked.metered, None);
    assert_eq!(
        asked.recommended.as_deref(),
        Some("quants/demo-chat-Q8_0.gguf")
    );
}
