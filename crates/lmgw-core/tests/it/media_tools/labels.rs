//! The labels of an Admin Chat thread's media rows (design §8.3, §14 item
//! 10): the picture's model row is `admin`, as the thread's model turns,
//! voice and card actions are, and its tool row `chat-tool`, as the
//! thread's other tools'.

use serde_json::{json, Value};

use super::chat_turn::{answer, chat_model, rows_where, send, tool_call};
use super::{gateway, image_row, job, mount_sd, Z_IMAGE};

#[tokio::test]
async fn an_admin_threads_picture_is_labelled_admin() {
    let sd = wiremock::MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let chat = wiremock::MockServer::start().await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    chat_model(
        &state,
        &chat,
        &[
            tool_call(
                "c1",
                "image__generate",
                json!({"model": "image/z", "prompt": "a red square"}),
            ),
            answer("here it is"),
        ],
    )
    .await;
    let t: Value = gw
        .client()
        .post(format!("{gw}/chat/api/threads"))
        .json(&json!({"model_alias": "my-model", "kind": "admin"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = t["id"].as_i64().unwrap();
    gw.client()
        .post(format!("{gw}/chat/api/threads/{tid}/settings"))
        .json(&json!({"mcp_tools": [{"server_label": "image"}]}))
        .send()
        .await
        .unwrap();

    let body = send(&gw, tid, "draw me a red square").await;
    assert!(body.contains("here it is"), "{body}");
    let rows = rows_where(&state, (Some("image"), Some("admin")), 1).await;
    assert_eq!(rows, vec![(200, None)]);
    let chat_rows = rows_where(&state, (Some("image"), Some("chat")), 0).await;
    assert!(chat_rows.is_empty(), "{chat_rows:?}");
    let tool = rows_where(&state, (None, Some("chat-tool")), 1).await;
    assert_eq!(tool.len(), 1, "{tool:?}");
}
