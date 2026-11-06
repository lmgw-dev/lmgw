//! An owner's op the client hangs up on while the work after its store
//! write is still running (*2026-10-23*, *2026-10-24*): the op is written
//! by hand to the plain listener, its answer never read, and a slow MCP
//! server holds every snapshot reload in its reconcile until the test lets
//! it go, so the hang-up reaches the handler while the write's tail runs,
//! however slow the machine is.

use std::sync::Arc;

use tokio::io::AsyncWriteExt;

/// An autostart MCP server whose socket accepts and never answers until the
/// test releases it (the returned handle's `notify_one`), which then closes
/// the sockets it held and fails the handshake. Every snapshot reload waits
/// in its reconcile until then, the way a reload with an unreachable tool
/// server does, so the op's tail is still running when the client's
/// hang-up reaches the handler, however slow the machine is. Its handshake
/// timeout is an hour: only the release ends the wait. The listener's task
/// lives as long as the test's runtime.
pub async fn slow_mcp_server(
    state: &lmgw_core::state::SharedState,
) -> (
    Arc<tokio::sync::Notify>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let release = Arc::new(tokio::sync::Notify::new());
    let released = release.clone();
    let accepted_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = accepted_count.clone();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((socket, _)) => {
                        held.push(socket);
                        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    Err(_) => break,
                },
                () = released.notified() => break,
            }
        }
        drop(held);
    });
    lmgw_core::store::insert_mcp_server(
        &state.db,
        &lmgw_core::store::NewMcpServer {
            name: "slow".into(),
            enabled: true,
            transport: lmgw_core::config::McpTransport::Http,
            command: None,
            args: vec![],
            env: vec![],
            cwd: None,
            container_image: None,
            extra_run_args: vec![],
            url: Some(url),
            headers: vec![],
            tool_prefix: "slow".into(),
            timeout_ms: 3_600_000,
            autostart: true,
            idle_seconds: 300,
            allow_sampling: false,
            sampling_alias: None,
            agent_id: None,
        },
    )
    .await
    .unwrap();
    (release, accepted_count)
}

/// The owner's op `name`, written to the plain listener as an HTTP/1.1
/// request with the owner's key, the connection handed back, the answer
/// unread: the caller hangs up by dropping it.
pub async fn op_unanswered(
    gw: &crate::common::Gw,
    name: &str,
    body: serde_json::Value,
) -> tokio::net::TcpStream {
    let addr = gw.base.trim_start_matches("http://").to_string();
    let mut tcp = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let body = body.to_string();
    let request = format!(
        "POST /api/op/{name} HTTP/1.1\r\nhost: {addr}\r\n\
         authorization: Bearer {}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n\r\n{body}",
        gw.key,
        body.len()
    );
    tcp.write_all(request.as_bytes()).await.unwrap();
    tcp.flush().await.unwrap();
    tcp
}

/// The owner's op `name` with `body`, hung up on while the reload after its
/// write waits on [`slow_mcp_server`]: the reload is known to have reached
/// the server before the hang-up, and is let go a short settle after it
/// (the plain listener has no connection count to wait on). The caller
/// then waits for what the op's tail does.
pub async fn hang_up_mid_reload(
    gw: &crate::common::Gw,
    state: &lmgw_core::state::SharedState,
    name: &str,
    body: serde_json::Value,
) {
    let (release, reached) = slow_mcp_server(state).await;
    let tcp = op_unanswered(gw, name, body).await;
    crate::common::patience::until_async("the reload reached the MCP server", || async {
        reached.load(std::sync::atomic::Ordering::SeqCst) > 0
    })
    .await;
    drop(tcp);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    release.notify_one();
}
