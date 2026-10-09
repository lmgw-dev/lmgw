//! Which calls become tasks, and what they carry (§1.1, §1.2, T5, T15).

use serde_json::{json, Value};

use super::{call, executor, next, task_tool, task_world, text, thread, Script};
use crate::mcp_host::{mcp_rpc, mcp_session};

/// A `/mcp` `tools/call` of `name` as the owner: its answer.
async fn mcp_call(w: &crate::realtime_chat_thread::World, name: &str) -> Value {
    let client = w.gw.client();
    let sid = mcp_session(w, &client).await;
    mcp_rpc(
        w,
        &client,
        &sid,
        "tools/call",
        json!({"name": name, "arguments": {"text": "go"}}),
    )
    .await
}

/// A `required` tool on a device that declares the capability is called
/// with `task: {}` and `_meta["lmgw/task"]`: `wait` for a caller with no
/// thread, `thread` with its id on the late path. `optional`, `forbidden`
/// and absent are called normally, with no `lmgw/task`.
#[tokio::test]
async fn a_required_tool_is_a_task_and_the_others_are_normal_calls() {
    let script = Script {
        tools: vec![
            task_tool("build", Some("required")),
            task_tool("opt", Some("optional")),
            task_tool("plain", None),
            task_tool("never", Some("forbidden")),
        ],
        ..Script::default()
    };
    let (w, _d, mut dev, server) = task_world(script).await;

    for name in ["opt", "plain", "never"] {
        let v = mcp_call(&w, &format!("desktop__{name}")).await;
        assert_eq!(
            v["result"]["content"][0]["text"],
            format!("ran {name}"),
            "{v}"
        );
        let sent = next("the device's tools/call", &mut dev.seen.calls).await;
        assert!(sent["params"].get("task").is_none(), "{sent}");
        assert!(sent["params"]["_meta"].get("lmgw/task").is_none(), "{sent}");
        assert!(
            sent["params"]["_meta"].get("lmgw/caller").is_some(),
            "{sent}"
        );
    }

    // The late path: answered at once, the task stored.
    let tid = thread(&w).await;
    let out = call(&executor(&w, server, Some(tid)), "desktop__build").await;
    assert!(!out.is_error, "{out:?}");
    assert_eq!(text(&out), "started, job t1");
    let sent = next("the device's tools/call", &mut dev.seen.calls).await;
    assert_eq!(sent["params"]["task"], json!({}), "{sent}");
    assert_eq!(
        sent["params"]["_meta"]["lmgw/task"],
        json!({"delivery": "thread", "thread_id": tid}),
        "{sent}"
    );
    let row = super::the_row(&w).await;
    assert_eq!(
        (
            row.state.as_str(),
            row.status.as_str(),
            row.task_id.as_str()
        ),
        ("open", "working", "t1")
    );
    assert_eq!(
        (row.thread_id, row.tool.as_str(), row.server_label.as_str()),
        (Some(tid), "desktop__build", "desktop")
    );
    assert!(w.state.mcp.has_open_tasks(server));

    // The bridge: `/mcp` waits for the result.
    let waiting = {
        let w2 = w.gw.client();
        let (gw, sid) = (w.gw.to_string(), mcp_session(&w, &w2).await);
        tokio::spawn(async move {
            w2.post(format!("{gw}/mcp"))
                .header("accept", "application/json, text/event-stream")
                .header("mcp-session-id", sid)
                .json(&json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                              "params": {"name": "desktop__build", "arguments": {}}}))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        })
    };
    let sent = next("the bridged tools/call", &mut dev.seen.calls).await;
    assert_eq!(
        sent["params"]["_meta"]["lmgw/task"],
        json!({"delivery": "wait", "thread_id": null}),
        "{sent}"
    );
    dev.complete("t2", "built", true);
    let answer: Value = serde_json::from_str(&waiting.await.unwrap()).unwrap();
    assert_eq!(answer["result"]["content"][0]["text"], "built", "{answer}");
    // Nothing stored for the bridge.
    assert_eq!(super::the_row(&w).await.task_id, "t1");
}

/// A `required` tool on a device that does not declare
/// `tasks.requests.tools.call` is not called: the error says why.
#[tokio::test]
async fn a_required_tool_without_the_capability_is_refused_by_name() {
    let script = Script {
        declares: false,
        ..Script::default()
    };
    let (w, _d, mut dev, _) = task_world(script).await;
    let v = mcp_call(&w, "desktop__build").await;
    assert!(
        v.to_string().contains(
            "server 'device:desktop' requires a task for 'build' but does not declare \
             `tasks.requests.tools.call`"
        ),
        "{v}"
    );
    assert!(dev.seen.calls.try_recv().is_err(), "nothing was sent");
}

/// The server's `model-immediate-response` is the line after
/// `started, job …` (T5).
#[tokio::test]
async fn the_immediate_response_reaches_the_tool_result() {
    let script = Script {
        immediate: Some("Building now; I will say when it is done.".into()),
        ..Script::default()
    };
    let (w, _d, _dev, server) = task_world(script).await;
    let tid = thread(&w).await;
    let out = call(&executor(&w, server, Some(tid)), "desktop__build").await;
    assert_eq!(
        text(&out),
        "started, job t1\nBuilding now; I will say when it is done."
    );
}

/// A tool result to an augmented call is a normal result; a task to a call
/// lmgw did not augment is a protocol error, reported as the call's.
#[tokio::test]
async fn unexpected_answers_are_a_result_and_an_error() {
    let script = Script {
        tools: vec![
            task_tool("build", Some("required")),
            task_tool("opt", Some("optional")),
        ],
        never_task: true,
        ..Script::default()
    };
    let (w, _d, dev, server) = task_world(script).await;
    let tid = thread(&w).await;
    let out = call(&executor(&w, server, Some(tid)), "desktop__build").await;
    assert_eq!(text(&out), "ran build");
    assert!(lmgw_core::store::mcp_tasks::all(&w.state.db)
        .await
        .unwrap()
        .is_empty());

    {
        let mut s = dev.script.lock().unwrap();
        s.never_task = false;
        s.always_task = true;
    }
    let v = mcp_call(&w, "desktop__opt").await;
    assert!(v.to_string().contains("protocol error"), "{v}");
}
