//! The MCP page's count (MCP Tasks design §6): `GET /api/mcp-servers` says
//! how many tasks each server runs for Chat threads now, and the count
//! drops once a task ended.

use serde_json::{json, Value};

use super::thread::{build_call, tool_thread, until_results};
use super::{task_world, Script};
use crate::chat_approvals::send;
use crate::realtime_chat_thread::World;
use crate::support::realtime_fakes::Turn;

/// The `open_tasks` of server row `id` in the list.
async fn open_tasks(w: &World, id: i64) -> Value {
    let v = w.get("/api/mcp-servers").await;
    v["mcp_servers"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["id"] == id))
        .map(|r| r["open_tasks"].clone())
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn the_server_list_counts_a_running_task_until_it_ended() {
    let (w, _d, dev, server) = task_world(Script::default()).await;
    assert_eq!(open_tasks(&w, server).await, json!(0), "nothing runs yet");
    let tid = tool_thread(&w, json!({})).await;
    build_call(&w);
    w.chat.push(Turn::text(&["Started", " it."]));
    send(&w, &w.gw.client(), tid, "build it").await;
    assert_eq!(open_tasks(&w, server).await, json!(1), "the job runs");

    dev.complete("t1", "42 files", true);
    until_results(&w, tid, 1).await;
    assert_eq!(open_tasks(&w, server).await, json!(0), "the job ended");
}
