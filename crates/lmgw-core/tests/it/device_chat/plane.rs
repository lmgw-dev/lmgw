//! The Chat plane's reads beside the threads (chat-plane design §2): the
//! settings, the knowledge bases and the source viewer, and the thread
//! rows, each as the owner and as a paired device reads it — a device never
//! learns more through them than it reaches today.

use lmgw_core::knowledge::{originals, store as kstore};
use quickdoc_core::embed::EmbedIdentity;
use serde_json::{json, Value};

use super::{admin_thread, chat_thread, device_world, get, op, pair, post, self_admin_thread};
use crate::realtime_chat_thread::{world, World};

/// A knowledge base of the owner's, with nothing in it: its id.
async fn knowledge_base(w: &World, name: &str) -> i64 {
    lmgw_core::knowledge::store::insert_kb(
        &w.state.knowledge.pool,
        &lmgw_core::knowledge::store::NewKb {
            name: name.into(),
            description: String::new(),
            embed_alias: "e".into(),
            embed: EmbedIdentity::new("u", "m", 4),
            rerank_alias: String::new(),
            vision_alias: String::new(),
            chunk_tokens: 512,
            chunk_overlap: 64,
            // Not visible to the `kb__*` tools on /mcp: a thread's own
            // choice ignores the switch, so the Chat lists it anyway.
            mcp_visible: false,
        },
    )
    .await
    .unwrap()
}

/// A file in base `kb`, ingested once into two chunks and failed on its
/// next ingest, with notes and an error that name an upstream and a model,
/// and its original on disk: its id.
async fn ingested_file(w: &World, kb: i64) -> i64 {
    let pool = &w.state.knowledge.pool;
    let bytes = b"# Taxes\n\nThe refund came in March.\n".to_vec();
    let sha = originals::store(&w.state.data_dir, &bytes).await.unwrap();
    let id = kstore::insert_file(
        pool,
        &kstore::NewKbFile {
            kb_id: kb,
            name: "taxes.md".into(),
            kind: "text".into(),
            sub: "markdown".into(),
            mime: "text/markdown".into(),
            size: bytes.len() as i64,
            sha256: sha.clone(),
        },
    )
    .await
    .unwrap();
    let text = "Taxes\n\nThe refund came in March.";
    let chunk = |id: &str, seq: i64, (a, b): (i64, i64)| kstore::NewKbChunk {
        id: id.into(),
        seq,
        page: None,
        heading_path: "Taxes".into(),
        span_start: a,
        span_end: b,
        payload: text[a as usize..b as usize].into(),
        tokens: 4,
        embedding: None,
    };
    let wrote = kstore::finish_file(
        pool,
        kb,
        id,
        &sha,
        4,
        &[chunk("c-head", 0, (0, 5)), chunk("c-body", 1, (7, 31))],
        &kstore::Ingested {
            notes: vec!["read by the vision model 'zebra-vision' on upstream 'cloudy'".into()],
            text: text.into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(wrote);
    kstore::set_file_status(
        pool,
        id,
        "failed",
        Some("upstream 'cloudy' refused the embedding model 'zebra-embed'"),
    )
    .await
    .unwrap();
    id
}

fn out_of_scope(status: u16, v: &Value, tool: &str) {
    assert_eq!(status, 403, "{v}");
    assert_eq!(v["code"], "tool_label_out_of_scope", "{v}");
    let message = v["message"].as_str().unwrap();
    assert!(message.contains(tool), "names {tool}: {v}");
    assert!(!message.contains("Taxes"), "names no base: {v}");
}

/// D3: the settings the Chat uses, the same for every caller and none of
/// the gateway's others.
#[tokio::test]
async fn the_settings_are_the_chat_s_and_the_same_for_everyone() {
    let (w, d) = device_world().await;
    let (s, owner) = get(&w, &w.gw.client(), "/chat/api/settings").await;
    assert_eq!(s, 200, "{owner}");
    let (s, device) = get(&w, &d.client, "/chat/api/settings").await;
    assert_eq!(s, 200, "{device}");
    assert_eq!(owner, device);
    let snap = w.state.snapshot();
    let set = &snap.settings;
    assert_eq!(
        owner,
        json!({
            "kb_budget_tokens": set.chat_kb_budget_tokens,
            "archive_days": set.chat_archive_days,
            "purge_days": set.chat_purge_days,
            "tts_alias": set.chat_tts_alias,
            "realtime_tts_alias": set.realtime.tts_alias,
            "max_body_mb": set.max_body_mb,
            "task_poll_interval_s": set.mcp.task_poll_interval_s,
            "realtime_prefix_padding_ms": set.realtime.prefix_padding_ms,
            "realtime_ping_interval_s": set.realtime.ping_interval_s,
        })
    );
}

/// D7: the owner lists every base; a device only with `kb__search` in its
/// tool scope, and then every base too (a thread's choice ignores the MCP
/// switch); without it the refusal names the tool and no base.
#[tokio::test]
async fn the_bases_need_kb_search_in_a_device_s_scope() {
    let w = world(|_| {}).await;
    let id = knowledge_base(&w, "Taxes").await;
    let listed = |v: &Value| -> Vec<(i64, String)> {
        v["bases"]
            .as_array()
            .unwrap_or_else(|| panic!("no bases in {v}"))
            .iter()
            .map(|b| {
                (
                    b["id"].as_i64().unwrap(),
                    b["name"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let (s, v) = get(&w, &w.gw.client(), "/chat/api/knowledge/bases").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(listed(&v), vec![(id, "Taxes".to_string())]);
    let base = &v["bases"][0];
    assert_eq!(
        (base["files"].as_i64(), base["ready"].as_i64()),
        (Some(0), Some(0))
    );
    assert!(base["embed_resolvable"].is_boolean(), "{v}");
    assert!(
        base.get("embed_upstream").is_none(),
        "the Chat's fields only: {v}"
    );

    // Unscoped: `all` admits the kb tools.
    let all = pair(&w, "phone", json!({})).await;
    let (s, v) = get(&w, &all.client, "/chat/api/knowledge/bases").await;
    assert_eq!(
        (s, listed(&v)),
        (200, vec![(id, "Taxes".to_string())]),
        "{v}"
    );
    let docs = pair(
        &w,
        "tablet",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "docs__*" }),
    )
    .await;
    let (s, v) = get(&w, &docs.client, "/chat/api/knowledge/bases").await;
    out_of_scope(s, &v, "kb__search");
    let search = pair(
        &w,
        "laptop",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "kb__search" }),
    )
    .await;
    let (s, v) = get(&w, &search.client, "/chat/api/knowledge/bases").await;
    assert_eq!(
        (s, listed(&v)),
        (200, vec![(id, "Taxes".to_string())]),
        "{v}"
    );
}

/// D8, D14: the source viewer reads a file's text only with `kb__search`
/// and `kb__read`, and a device never its original; each refusal comes
/// before the file is looked up, so a file that exists and one that does
/// not answer alike.
#[tokio::test]
async fn a_file_s_text_needs_kb_read_too_and_ids_cannot_be_probed() {
    let w = world(|_| {}).await;
    let kb = knowledge_base(&w, "Taxes").await;
    let file = ingested_file(&w, kb).await;
    let search = pair(
        &w,
        "laptop",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "kb__search" }),
    )
    .await;
    let reader = pair(
        &w,
        "phone",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "kb__*" }),
    )
    .await;
    for id in [file, 999_999] {
        for path in [
            format!("/chat/api/knowledge/files/{id}/text"),
            format!("/chat/api/knowledge/files/{id}/text?chunk=x"),
        ] {
            let (s, v) = get(&w, &search.client, &path).await;
            out_of_scope(s, &v, "kb__read");
        }
        // The original is no device's, whatever its scope.
        for d in [&search, &reader] {
            let (s, v) = get(
                &w,
                &d.client,
                &format!("/chat/api/knowledge/files/{id}/original"),
            )
            .await;
            assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");
            assert!(!v["message"].as_str().unwrap().contains("taxes"), "{v}");
        }
    }
    // Within reach, a file that is not there answers as on the Knowledge
    // page: the text's 400, and for the owner the original's 404.
    for client in [&reader.client, &w.gw.client()] {
        let (s, v) = get(&w, client, "/chat/api/knowledge/files/999999/text").await;
        assert_eq!((s, v["code"].as_str()), (400, Some("op_failed")), "{v}");
    }
    let (s, v) = get(
        &w,
        &w.gw.client(),
        "/chat/api/knowledge/files/999999/original",
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (404, Some("not_found")), "{v}");
}

/// D8, D15: a device with the `kb` tools reads an existing file's text and
/// the cited chunk, in the Chat's shape: no ingest error or notes (they name
/// upstreams and models), and no original to download. The owner reads the
/// same text and may download the original.
#[tokio::test]
async fn a_device_reads_a_file_s_text_without_its_ingest_record() {
    let w = world(|_| {}).await;
    let kb = knowledge_base(&w, "Taxes").await;
    let file = ingested_file(&w, kb).await;
    let reader = pair(
        &w,
        "phone",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "kb__search\nkb__read" }),
    )
    .await;
    let path = format!("/chat/api/knowledge/files/{file}/text?chunk=c-body");
    let (s, device) = get(&w, &reader.client, &path).await;
    assert_eq!(s, 200, "{device}");
    assert_eq!(device["text"], "Taxes\n\nThe refund came in March.");
    assert_eq!(device["kb_name"], "Taxes");
    assert_eq!(device["highlight"]["id"], "c-body", "{device}");
    assert_eq!(device["chunks"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        device["file"],
        json!({
            "id": file,
            "name": "taxes.md",
            "kind": "text",
            "status": "failed",
            "pages": null,
        })
    );
    assert_eq!(device["original"], false);
    let wire = device.to_string();
    for leak in ["cloudy", "zebra", "error", "notes", "sha256"] {
        assert!(!wire.contains(leak), "{leak}: {device}");
    }

    let (s, owner) = get(&w, &w.gw.client(), &path).await;
    assert_eq!(s, 200, "{owner}");
    assert_eq!(owner["original"], true);
    assert_eq!(
        (&owner["text"], &owner["file"], &owner["highlight"]),
        (&device["text"], &device["file"], &device["highlight"])
    );
    let resp =
        w.gw.client()
            .get(format!("{}/chat/api/knowledge/files/{file}/original", w.gw))
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.bytes().await.unwrap().as_ref(),
        b"# Taxes\n\nThe refund came in March.\n"
    );
}

/// D8: the owner switching `kb__read` off stops a device's viewer as it
/// stops the tool (`resolve_builtin` drops it), said before the file is
/// looked up; the owner still reads. The bases list does not read the
/// switch on `kb__search` (D13): a write of a base and a turn's retrieval
/// do not either.
#[tokio::test]
async fn a_switched_off_kb_read_stops_a_device_s_viewer() {
    let w = world(|_| {}).await;
    let kb = knowledge_base(&w, "Taxes").await;
    let file = ingested_file(&w, kb).await;
    let reader = pair(
        &w,
        "phone",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "kb__*" }),
    )
    .await;
    let path = format!("/chat/api/knowledge/files/{file}/text");
    let (s, v) = get(&w, &reader.client, &path).await;
    assert_eq!(s, 200, "{v}");
    for name in ["kb__read", "kb__search"] {
        let (s, v) = op(&w, "tool_set", json!({ "name": name, "enabled": false })).await;
        assert_eq!(s, 200, "{name}: {v}");
    }
    for p in [path.clone(), "/chat/api/knowledge/files/999999/text".into()] {
        let (s, v) = get(&w, &reader.client, &p).await;
        out_of_scope(s, &v, "kb__read");
        assert!(
            v["message"].as_str().unwrap().contains("switched off"),
            "{v}"
        );
    }
    let (s, v) = get(&w, &w.gw.client(), &path).await;
    assert_eq!(s, 200, "the owner still reads: {v}");
    let (s, v) = get(&w, &reader.client, "/chat/api/knowledge/bases").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["bases"][0]["id"].as_i64(), Some(kb), "{v}");
}

/// D2: the rows a feed's `message.*` named are every Chat client's now, and
/// a device's answer leaves out what L3 hides, as its list does.
#[tokio::test]
async fn a_device_reads_rows_within_its_reach() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let plain = chat_thread(&w, &owner, "chatty").await;
    let admin = admin_thread(&w).await.id;
    let tools = self_admin_thread(&w).await.id;
    let path = format!("/chat/api/threads/rows?ids={plain},{admin},{tools}");
    let ids = |v: &Value| -> Vec<i64> {
        let mut ids: Vec<i64> = v["threads"]
            .as_array()
            .unwrap_or_else(|| panic!("no rows in {v}"))
            .iter()
            .map(|t| t["id"].as_i64().unwrap())
            .collect();
        ids.sort_unstable();
        ids
    };
    let (s, v) = get(&w, &d.client, &path).await;
    assert_eq!((s, ids(&v)), (200, vec![plain]), "{v}");
    let (s, v) = get(&w, &owner, &path).await;
    let mut all = vec![plain, admin, tools];
    all.sort_unstable();
    assert_eq!((s, ids(&v)), (200, all), "{v}");
}

/// D2: a row in a folder a device cannot see comes without its folder
/// (`hide_folders`, review W4-7), as its list does; a device whose admin
/// tools are at read only reads the toolset thread's row and that folder,
/// never Admin Chat's.
#[tokio::test]
async fn a_device_s_rows_hide_a_folder_out_of_its_reach_and_follow_its_level() {
    let w = world(|s| s.self_admin = lmgw_core::config::SelfAdmin::Full).await;
    let owner = w.gw.client();
    let plain_device = pair(&w, "phone", json!({})).await;
    let reader = pair(&w, "laptop", json!({ "self_admin": "read_only" })).await;
    let (s, v) = post(&w, &owner, "/chat/api/folders", json!({ "name": "Ops" })).await;
    assert_eq!(s, 200, "{v}");
    let folder = v["id"].as_i64().unwrap();
    let plain = chat_thread(&w, &owner, "chatty").await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{plain}/move"),
        json!({ "folder_id": folder }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    // The folder's defaults take the toolset: the plain thread in it stays a
    // device's, the folder does not.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults": { "mcp_tools": [{ "server_label": "lmgw" }] } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let tools = self_admin_thread(&w).await.id;
    let admin = admin_thread(&w).await.id;
    let path = format!("/chat/api/threads/rows?ids={plain},{tools},{admin}");
    let rows = |v: &Value| -> Vec<(i64, Option<i64>)> {
        let mut rows: Vec<(i64, Option<i64>)> = v["threads"]
            .as_array()
            .unwrap_or_else(|| panic!("no rows in {v}"))
            .iter()
            .map(|t| (t["id"].as_i64().unwrap(), t["folder_id"].as_i64()))
            .collect();
        rows.sort_unstable();
        rows
    };
    let sorted = |mut v: Vec<(i64, Option<i64>)>| {
        v.sort_unstable();
        v
    };

    let (s, v) = get(&w, &plain_device.client, &path).await;
    assert_eq!((s, rows(&v)), (200, vec![(plain, None)]), "{v}");
    let (s, v) = get(&w, &reader.client, &path).await;
    assert_eq!(
        (s, rows(&v)),
        (200, sorted(vec![(plain, Some(folder)), (tools, None)])),
        "{v}"
    );
    let (s, v) = get(&w, &owner, &path).await;
    assert_eq!(
        (s, rows(&v)),
        (
            200,
            sorted(vec![(plain, Some(folder)), (tools, None), (admin, None)])
        ),
        "{v}"
    );
}
