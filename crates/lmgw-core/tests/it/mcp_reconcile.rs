//! The MCP reconcile after a reload, and the connects it starts (MCP
//! gateway design §9; the begin-write review's B-1, B-2 and B-11).
//!
//! - A settings writer publishes its snapshot under the settings lock and
//!   reconciles once it let go of it: a server whose connect hangs holds up
//!   that writer's answer, never the GPU hold or the next settings save.
//! - A connect that settles after its server was deleted or disabled stores
//!   nothing: its session is closed, and the entry stays as the delete or
//!   the disable left it.
//! - A connect whose caller goes away runs to its end on its own task.

use std::time::Duration;

use lmgw_core::config::McpTransport;
use lmgw_core::ops::{self, SettingsPatch};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewMcpServer};

use crate::support::mcp_stub::{echo_stub, held_stub, McpStub};

/// A `timeout_ms` longer than any test here runs: a connect held by the
/// stub still waits when the test ends.
const HANGS: u64 = 120_000;

/// The row of an HTTP server at `stub`, `autostart` and `enabled` as
/// given, its `timeout_ms` [`HANGS`].
fn row(stub: &McpStub, autostart: bool, enabled: bool) -> NewMcpServer {
    NewMcpServer {
        name: "held".into(),
        enabled,
        transport: McpTransport::Http,
        command: None,
        args: vec![],
        env: vec![],
        cwd: None,
        container_image: None,
        extra_run_args: vec![],
        url: Some(stub.url.clone()),
        headers: vec![],
        tool_prefix: "held".into(),
        timeout_ms: HANGS,
        autostart,
        idle_seconds: 0,
        allow_sampling: false,
        sampling_alias: None,
        agent_id: None,
    }
}

/// An autostart server at `stub`, stored and not yet reconciled: the next
/// reload's reconcile connects it. Its id.
async fn autostart(state: &SharedState, stub: &McpStub) -> i64 {
    store::insert_mcp_server(&state.db, &row(stub, true, true))
        .await
        .unwrap()
}

/// A server held by `stub`, its reconcile's connect waiting on the held
/// `tools/list`: its id, and the reload that waits on that connect.
async fn connecting(state: &SharedState, stub: &McpStub) -> (i64, tokio::task::JoinHandle<()>) {
    let id = autostart(state, stub).await;
    let st = state.clone();
    let reload = tokio::spawn(async move {
        st.reload_snapshot().await.unwrap();
    });
    until_connecting(state, id).await;
    (id, reload)
}

/// `stub` answers its held `tools/list`, and the connect waiting on it
/// settles: the reload that waited on it returns.
async fn settle(stub: &McpStub, reload: tokio::task::JoinHandle<()>) {
    stub.release();
    tokio::time::timeout(Duration::from_secs(10), reload)
        .await
        .expect("the connect settles once the server answers")
        .unwrap();
}

/// A reload, which the delete or the edit of a server runs: its reconcile.
async fn reload(state: &SharedState) {
    tokio::time::timeout(Duration::from_secs(5), state.reload_snapshot())
        .await
        .expect("this reload's reconcile starts nothing that waits")
        .unwrap();
}

/// Until `stub` saw `n` sessions closed; fails after 5 s.
async fn until_closed(stub: &McpStub, n: usize) {
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        while stub.closed() < n {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "the server saw {} sessions closed, not {n}",
        stub.closed()
    );
}

/// The tools of server `id` in the aggregate.
async fn tools_of(state: &SharedState, id: i64) -> Vec<String> {
    let agg = state.mcp.aggregate(&state.snapshot()).await;
    agg.reverse
        .iter()
        .filter(|(_, (owner, _))| *owner == id)
        .map(|(name, _)| name.clone())
        .collect()
}

/// MCP server `id`'s badge status, `None` without an entry.
async fn status(state: &SharedState, id: i64) -> Option<&'static str> {
    state
        .mcp
        .status_view(id, &state.snapshot())
        .await
        .map(|v| v.status)
}

/// Until MCP server `id` is `connecting`; fails after 10 s.
async fn until_connecting(state: &SharedState, id: i64) {
    let reached = tokio::time::timeout(Duration::from_secs(10), async {
        while status(state, id).await != Some("connecting") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        reached.is_ok(),
        "the connect: {:?}",
        status(state, id).await
    );
}

/// B-1: a settings save's reconcile waits on a server whose `tools/list`
/// never answers. The GPU hold engages, and a second settings save goes
/// through, while the first save still waits: both take the settings lock,
/// and the first save let go of it once it published. The second save
/// keeps the hold, which it copied under the lock.
#[tokio::test]
async fn the_hold_and_a_settings_save_go_through_while_a_save_s_reconcile_hangs() {
    let state = AppState::init_for_tests().await.unwrap();
    let stub = held_stub().await;
    let id = autostart(&state, &stub).await;
    let st = state.clone();
    let first = tokio::spawn(async move {
        ops::settings_set(
            &st,
            SettingsPatch {
                retention_days: Some(7),
                ..Default::default()
            },
        )
        .await
    });
    until_connecting(&state, id).await;

    let hold = tokio::time::timeout(Duration::from_secs(5), ops::hold_set(&state, true))
        .await
        .expect("the hold does not wait for the save's reconcile")
        .unwrap();
    assert_eq!(hold["active"], true, "{hold}");
    let second = tokio::time::timeout(
        Duration::from_secs(5),
        ops::settings_set(
            &state,
            SettingsPatch {
                retention_days: Some(9),
                ..Default::default()
            },
        ),
    )
    .await
    .expect("the second save does not wait for the first one's reconcile")
    .unwrap();
    assert_eq!(second["ok"], true, "{second}");
    let settings = state.snapshot().settings.clone();
    assert_eq!(settings.retention_days, 9);
    assert!(settings.hold.active, "the second save kept the hold");
    assert!(
        !first.is_finished(),
        "the first save still waits on its reconcile"
    );
    first.abort();
    let _ = first.await;
}

/// B-2: a server deleted while its connect waits. The delete's reconcile
/// prunes its entry, and the connect that settles afterwards stores
/// nothing: no entry, no tools, its session closed. It came back `ready`,
/// a live session for a server that is not there.
#[tokio::test]
async fn a_connect_that_settles_after_its_server_was_deleted_stores_nothing() {
    let state = AppState::init_for_tests().await.unwrap();
    let stub = held_stub().await;
    let (id, first) = connecting(&state, &stub).await;

    store::delete_mcp_server(&state.db, id).await.unwrap();
    reload(&state).await;
    assert_eq!(status(&state, id).await, None, "the delete pruned it");
    settle(&stub, first).await;

    assert_eq!(status(&state, id).await, None);
    assert_eq!(tools_of(&state, id).await, Vec::<String>::new());
    until_closed(&stub, 1).await;
}

/// B-2: a server disabled while its connect waits. The connect that
/// settles afterwards stores nothing, and the server is `stopped`, as a
/// disabled one is. It came back `ready`, and no reconcile stopped it.
#[tokio::test]
async fn a_connect_that_settles_after_its_server_was_disabled_stores_nothing() {
    let state = AppState::init_for_tests().await.unwrap();
    let stub = held_stub().await;
    let (id, first) = connecting(&state, &stub).await;

    store::update_mcp_server(&state.db, id, &row(&stub, true, false))
        .await
        .unwrap();
    reload(&state).await;
    settle(&stub, first).await;

    assert_eq!(status(&state, id).await, Some("stopped"));
    assert_eq!(tools_of(&state, id).await, Vec::<String>::new());
    until_closed(&stub, 1).await;
}

/// B-2: a server edited while its connect waits, its URL moved to another
/// server. The edit's reconcile cannot start it (a connect holds it), so
/// the connect that settles afterwards closes its session and connects the
/// new URL. It used to store the old URL's session, until the next reload.
#[tokio::test]
async fn a_connect_whose_server_was_edited_meanwhile_connects_the_new_config() {
    let state = AppState::init_for_tests().await.unwrap();
    let old = held_stub().await;
    let new = echo_stub().await;
    let (id, first) = connecting(&state, &old).await;

    store::update_mcp_server(&state.db, id, &row(&new, true, true))
        .await
        .unwrap();
    reload(&state).await;
    assert_eq!(new.hits(), 0, "the edit's reconcile left the claim alone");
    settle(&old, first).await;

    assert_eq!(status(&state, id).await, Some("ready"));
    assert_eq!(tools_of(&state, id).await, vec!["held__echo".to_string()]);
    assert!(new.hits() > 0, "the new URL was connected");
    until_closed(&old, 1).await;
}

/// B-11: a tool call on a lazy server connects it, and its client hangs up
/// while the handshake waits on a `tools/list` that never answers. The
/// connect runs on, on a task of its own, and the server ends `error` at
/// its `timeout_ms`. Without that task the dropped call took the connect
/// with it, and the server stayed `connecting` for good. (A reconcile's
/// connect was kept alive by the reconcile's own task, so the test of a
/// dropped reconcile did not show it.)
#[tokio::test]
async fn a_connect_whose_caller_hangs_up_still_settles() {
    let state = AppState::init_for_tests().await.unwrap();
    let stub = held_stub().await;
    let id = store::insert_mcp_server(
        &state.db,
        &NewMcpServer {
            timeout_ms: 2_000,
            ..row(&stub, false, true)
        },
    )
    .await
    .unwrap();
    reload(&state).await;
    assert_eq!(
        status(&state, id).await,
        Some("stopped"),
        "lazy: not started"
    );

    let st = state.clone();
    let call = tokio::spawn(async move {
        let snap = st.snapshot();
        let _ = st
            .mcp
            .call_listed(
                &snap,
                "held__echo",
                id,
                None,
                &lmgw_core::mcp::host::CallFrom::gateway(),
            )
            .await;
    });
    until_connecting(&state, id).await;
    call.abort();
    let _ = call.await;

    let settled = tokio::time::timeout(Duration::from_secs(10), async {
        while status(&state, id).await == Some("connecting") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(settled.is_ok(), "the connect never settled");
    let view = state.mcp.status_view(id, &state.snapshot()).await.unwrap();
    assert_eq!(view.status, "error", "{:?}", view.detail);
    assert!(
        view.detail
            .as_deref()
            .is_some_and(|d| d.contains("no answer to the MCP handshake within 2000 ms")),
        "{:?}",
        view.detail
    );
}
