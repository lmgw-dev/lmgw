//! A stream proxied on an agent's MCP face ends with the session that
//! opened it (D58's list, the agent proxy's MCP face).

use std::time::Duration;

use lmgw_core::agents::service;

use super::*;
use crate::agents_service::{doc, install, with_fake};

#[tokio::test]
async fn an_agent_s_mcp_stream_ends_with_the_session_that_opened_it() {
    let (state, gw, _calls) = with_fake().await;
    // `provides.mcp` at the root: the face's `/sse` is the container's held
    // event stream.
    install(
        &gw,
        &doc(
            "localhost/board:1",
            0,
            10,
            r#", "provides": { "mcp": "/" }"#,
        ),
    )
    .await;
    let session = turn_on(&gw).await;
    let stream = browser()
        .get(format!("{gw}/agents/board/mcp/sse"))
        .header("cookie", format!("lmgw_session={session}"))
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    let out = browser()
        .delete(format!("{gw}/api/session"))
        .header("cookie", format!("lmgw_session={session}"))
        .send()
        .await
        .unwrap();
    assert_eq!(out.status(), 204);
    assert!(
        ends_within(stream, Duration::from_secs(10)).await,
        "the proxied stream outlived its session"
    );
    service::stop(&state, "board", "test over").await;
}
