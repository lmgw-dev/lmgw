//! The trace span records the path and never the query (principals §3.4, §10).
//!
//! Its own test binary — the one file outside `tests/it/` — because it
//! installs a **global** `tracing` subscriber: that is a once-per-process act,
//! and a file with one test is a process with one of them. Inside the shared
//! `it` binary the subscriber also captured every other suite's requests
//! (`session_login`'s `?nonce=` tripped the query assertion), and a
//! thread-scoped `set_default` captured nothing at all once ~1,400 other tests
//! had hit the same callsites first. One extra link is the price of testing
//! the subscriber the way production installs it.
//!
//! What this guards is small and specific. `TraceLayer`'s `DefaultMakeSpan`
//! records `uri`, which is path *and* query — and §3.4's login link carries a
//! one-shot credential in a query string, to a gateway whose owner is told to
//! read that link out of the process log. A span that echoed it would write
//! the credential into the very file the link is copied from.

#[path = "it/common/mod.rs"]
mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_core::state::AppState;

/// Everything the subscriber wrote, for the assertion to read back.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn the_trace_span_carries_the_path_and_never_the_query() {
    let captured = Captured::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(captured.clone())
            // `tower_http` is what emits the request/response events; the span
            // they are emitted inside is the one under test.
            .with_env_filter(tracing_subscriber::EnvFilter::new(
                "tower_http=debug,lmgw_core=debug",
            ))
            .finish(),
    )
    .expect("the only subscriber this process installs");

    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state).await;
    let status = gw
        .client()
        .get(format!("{gw}/api/version?nonce=one-shot-secret-42"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(status, 200);

    // The other plane (§3.11): a request whose `Host` is an agent origin is
    // answered by the `Host` dispatch, which sits **inside** `TraceLayer` — so
    // both planes are in the log. No agent is installed here, so what answers
    // is the dispatch's own `404`; that it was traced at all is the point.
    let status = gw
        .origin_client(&["board.localhost"])
        .get(format!(
            "{}/agent-origin-probe",
            gw.origin("board.localhost")
        ))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(status, 404);

    // The response event is emitted on the server's task after the client has
    // its bytes; a moment for it to land.
    tokio::time::sleep(Duration::from_millis(250)).await;

    let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("/api/version"),
        "the span records the path it is a trace of:\n{log}"
    );
    assert!(
        !log.contains("one-shot-secret-42") && !log.contains("nonce"),
        "the query reached the trace log:\n{log}"
    );
    assert!(
        log.contains("/agent-origin-probe"),
        "the agent plane never reached the trace:\n{log}"
    );
}
