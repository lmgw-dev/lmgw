//! Which events of a bound self-admin session are over 256 KiB (calls
//! design §6.3, §16's open question), measured here and recorded in the
//! design: the parts tests (`sizes`) are sized on them.
//!
//! A bound session's own events are small but for the chat turn's frames it
//! relays (`lmgw.chat.frame`), and among those the tool results: a `tool`
//! frame's `result` carries the tool's text twice — as `output`, and again as
//! the MCP `content` block an MCP Apps view reads — each JSON-escaped once
//! more inside the frame, and an image result its image base64-encoded in
//! full. `session.created`, the bound session's other large event, has no
//! tools in it (the thread's tools run in its chat turn).
//!
//! What the test builds: each self-admin read tool a model calls without
//! arguments, run on a test gateway, its text wrapped in the frame the
//! session relays; the frame of an image result, its overhead and the image
//! size from which it passes 256 KiB; and a bound call's `session.created`.
//! The tool texts that list the gateway's own data (models, prices, logs,
//! usage) are a test gateway's here and grow with the box; the shipped image
//! recipes are the same everywhere. Two catalogs are not measured, needing
//! what a test gateway has not: `llama_flags` reads a container image,
//! `audio_catalog` a catalog refreshed from GitHub. So the test also says
//! from which text size a tool result passes the limit, at the escaping of
//! the largest text measured. The table goes to stderr (`--no-capture`).

use lmgw_api_types::chat_frames::ToolFrame;
use lmgw_api_types::mcp_apps::ToolResultFrame;
use serde_json::{json, Value};

use super::bound;
use crate::realtime_calls::calls_world;
use crate::support::self_admin::Admin;

/// libwebrtc's `max-message-size`, which every Android phone advertises.
const LIMIT: usize = 256 << 10;

/// The self-admin tools a model reads the gateway with, no argument needed.
const TOOLS: &[&str] = &[
    "lmgw__status",
    "lmgw__models",
    "lmgw__upstreams",
    "lmgw__mcp_servers",
    "lmgw__logs",
    "lmgw__settings",
    "lmgw__usage",
    "lmgw__prices",
    "lmgw__image_recipes",
    "lmgw__docs_corpora",
    "lmgw__agents",
    "lmgw__profiles",
];

/// The `lmgw.chat.frame` a bound session relays for a tool result (the chat
/// turn's `tool` frame, `web::agentchat`): `output` the result's text,
/// `content` its MCP blocks. Its ids as long as the session mints them.
fn result_frame(name: &str, output: &str, content: Vec<Value>) -> String {
    let data = ToolFrame::Result(ToolResultFrame {
        index: 0,
        name: name.to_string(),
        output: output.to_string(),
        is_error: false,
        ms: 12,
        server_label: Some("lmgw".into()),
        ui_resource: None,
        structured_content: None,
        content: Some(content),
        call_id: Some("call_0123456789ab1f".into()),
        task: None,
    });
    json!({"event_id": "event_0123456789ab2a", "type": "lmgw.chat.frame",
           "response_id": "resp_0123456789ab1e", "event": "tool",
           "data": serde_json::to_value(data).unwrap()})
    .to_string()
}

/// The frame of an image result whose image is `bytes` long: its base64 in
/// an MCP image block, the text the model reads beside it.
fn image_frame(bytes: usize) -> String {
    let data = "A".repeat(bytes.div_ceil(3) * 4);
    let block = json!({"type": "image", "data": data, "mimeType": "image/png"});
    result_frame("screen__capture", "[image]", vec![block])
}

/// How many `lmgw.event.part`s `frame` takes at [`LIMIT`] (1 when it
/// fits), as lmgw-client's splitter cuts it: its data escaped once more.
fn parts(frame: &str) -> usize {
    lmgw_client::realtime::parts::split(frame, "event_0123456789ab2a", LIMIT as u64)
        .unwrap()
        .len()
}

#[tokio::test]
async fn which_events_of_a_bound_self_admin_session_are_over_256_kib() {
    let w = calls_world(|_| {}).await;
    let admin = Admin::new(&w.state).await;
    let mut table = Vec::new();
    for tool in TOOLS {
        let (failed, text) = admin.call(tool, json!({})).await;
        let frame = result_frame(tool, &text, vec![json!({"type": "text", "text": text})]);
        // The text is carried twice.
        assert!(frame.len() > 2 * text.len(), "{tool}");
        table.push((
            tool.to_string(),
            failed,
            text.len(),
            frame.len(),
            parts(&frame),
        ));
    }
    let overhead = image_frame(0).len();
    let over_from = (LIMIT - overhead) / 4 * 3 + 1;
    assert!(image_frame(over_from).len() > LIMIT);
    assert!(image_frame(over_from - 1).len() <= LIMIT);
    let call = bound(&w).await;
    let created = call.rec.messages[0].len();
    assert!(call.rec.messages[0].contains("\"session.created\""));

    eprintln!("tool result frames of a bound self-admin session (test gateway):");
    eprintln!(
        "{:<22} {:>10} {:>10} {:>6}",
        "tool", "text", "frame", "parts"
    );
    for (tool, failed, text, frame, parts) in &table {
        let note = if *failed { " (refused)" } else { "" };
        eprintln!("{tool:<22} {text:>10} {frame:>10} {parts:>6}{note}");
    }
    let (_, _, text, frame, _) = table.iter().max_by_key(|r| r.2).unwrap();
    let ratio = *frame as f64 / *text as f64;
    eprintln!(
        "the largest text's frame is {ratio:.2} times the text: a text like it passes 256 KiB \
         from about {} KiB",
        (LIMIT as f64 / ratio / 1024.0) as usize
    );
    eprintln!("an image result's frame: {overhead} bytes of its own, past 256 KiB from an image of {over_from} bytes");
    for (what, bytes) in [
        ("a 100 KiB JPEG", 100 << 10),
        ("a 500 KiB PNG", 500 << 10),
        ("a 2 MiB screenshot", 2 << 20),
    ] {
        let frame = image_frame(bytes);
        eprintln!(
            "  {what}: a frame of {} bytes, {} parts",
            frame.len(),
            parts(&frame)
        );
    }
    eprintln!("a bound call's session.created: {created} bytes");
}
