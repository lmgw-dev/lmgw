//! The MCP face over streamable HTTP, the way lmgw's client speaks it:
//! `initialize` → `tools/list` → `tools/call search` → `tools/call read`, and
//! every refusal of `read` naming its rule.

mod common;

use common::fake::{models, Fake};
use common::harness::*;
use common::write;
use serde_json::{json, Value};

struct Mcp<'a> {
    app: &'a App,
    next: u64,
}

impl Mcp<'_> {
    /// One JSON-RPC request, with the headers lmgw's client sends (see
    /// docs/agents.md, "a Node container that calls a tool") and the ones
    /// lmgw's `/agents/<id>/mcp` proxy adds. No `X-Folder-Chat`: the MCP face
    /// is exempt from that rule.
    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let r = self
            .app
            .http
            .post(self.app.url("/mcp"))
            .header("x-forwarded-host", AUTHORITY)
            .header("x-lmgw-face", "mcp")
            .header("x-forwarded-for", LOCAL_CLIENT)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25")
            .body(
                json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params})
                    .to_string(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{method}");
        assert!(r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json"));
        let v: Value = r.json().await.unwrap();
        assert!(v.get("error").is_none(), "{method}: {v}");
        v["result"].clone()
    }

    async fn call(&mut self, name: &str, args: Value) -> Value {
        self.rpc("tools/call", json!({"name": name, "arguments": args}))
            .await
    }
}

fn text_of(result: &Value) -> String {
    result["content"][0]["text"].as_str().unwrap().to_string()
}

async fn synced() -> App {
    let app = start(
        Fake {
            models: models(json!({"id": "chat-small", "context_length": 4096})),
            ..Fake::default()
        },
        false,
    )
    .await;
    app.sync().await;
    app
}

#[tokio::test]
async fn initialize_list_search_and_read_a_range() {
    let app = synced().await;
    let mut mcp = Mcp { app: &app, next: 0 };

    let init = mcp
        .rpc(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1"}
            }),
        )
        .await;
    assert_eq!(init["serverInfo"]["name"], "folder-chat");
    assert!(init["capabilities"]["tools"].is_object(), "{init}");

    // The initialized notification is accepted without a body.
    let r = app
        .http
        .post(app.url("/mcp"))
        .header("x-forwarded-host", AUTHORITY)
        .header("x-lmgw-face", "mcp")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);

    let list = mcp.rpc("tools/list", json!({})).await;
    let tools = list["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["search", "read"]);
    let search = &tools[0];
    assert_eq!(search["inputSchema"]["required"], json!(["query"]));
    assert_eq!(search["inputSchema"]["properties"]["k"]["type"], "integer");
    assert_eq!(search["annotations"]["readOnlyHint"], true);
    let about = search["description"].as_str().unwrap();
    for part in [
        "k_fts (50)",
        "k_vec (50)",
        "rrf_k 60",
        "k_rerank (20)",
        "beyond the reranked set, in fused order",
        "as it was indexed",
    ] {
        assert!(about.contains(part), "'{part}' in: {about}");
    }
    let read = &tools[1];
    assert_eq!(read["inputSchema"]["required"], json!(["path"]));
    assert!(read["inputSchema"]["properties"]["start_line"].is_object());
    assert!(
        read["description"]
            .as_str()
            .unwrap()
            .contains("no size cap"),
        "the description tells a model to ask for ranges"
    );

    let found = mcp
        .call(
            "search",
            json!({"query": "zeta relabel flag bind mounts", "k": 2}),
        )
        .await;
    assert_ne!(found["isError"], true, "{found}");
    let hits = found["structuredContent"]["hits"].as_array().unwrap();
    assert!(!hits.is_empty() && hits.len() <= 2, "{found}");
    // The depths it ran with: quickdoc's, k_fts and k_vec raised to k only
    // when k is larger.
    let depths = &found["structuredContent"]["retrieval"];
    assert_eq!(
        (&depths["k_fts"], &depths["k_vec"], &depths["k_rerank"]),
        (&json!(50), &json!(50), &json!(20)),
        "{found}"
    );
    let top = &hits[0];
    assert_eq!(top["rank"], 1);
    assert_eq!(top["path"], "notes.md");
    assert_eq!(top["heading_path"], "# Setup > ## SELinux");
    assert!(top["page"].is_null());
    assert!(top["score"].is_number());
    assert!(top["text"].as_str().unwrap().contains("zeta relabel flag"));
    let (from, to) = (
        top["start_line"].as_u64().unwrap(),
        top["end_line"].as_u64().unwrap(),
    );

    let got = mcp
        .call(
            "read",
            json!({"path": "notes.md", "start_line": from, "end_line": to}),
        )
        .await;
    assert_ne!(got["isError"], true, "{got}");
    let text = text_of(&got);
    assert!(
        text.starts_with(&format!("notes.md: lines {from}-{to} of 7.")),
        "{text}"
    );
    assert!(text.contains("zeta relabel flag"), "{text}");
    assert!(!text.contains("Install podman"), "only the range: {text}");

    // No range: the whole file.
    let whole = text_of(&mcp.call("read", json!({"path": "todo.txt"})).await);
    assert_eq!(
        whole,
        "todo.txt: all 3 lines.\n\nbuy milk\n\ncall the gamma office\n"
    );
    // A range past the end ends there and says so.
    let past = text_of(
        &mcp.call(
            "read",
            json!({"path": "todo.txt", "start_line": 3, "end_line": 99}),
        )
        .await,
    );
    assert!(past.contains("past the end"), "{past}");
    assert!(past.ends_with("call the gamma office\n"), "{past}");
}

#[tokio::test]
async fn read_refuses_each_path_rule_by_name() {
    let app = synced().await;
    let root = app.tmp.path();
    write(root, "sub/deep.md", "# deep\n");
    write(root, ".private/notes.md", "# private\n");
    write(root, "photo.png", [0x89, b'P', b'N', b'G']);
    write(root, ".gitignore", "secret.md\nbuild/\n");
    write(root, "secret.md", "# secret\n");
    write(root, "build/out.md", "# built\n");
    std::os::unix::fs::symlink(root.join("notes.md"), root.join("link.md")).unwrap();
    std::os::unix::fs::symlink(root.join("sub"), root.join("linkdir")).unwrap();
    let mut mcp = Mcp { app: &app, next: 0 };

    let cases = [
        (json!({"path": "/etc/hostname"}), "no absolute paths"),
        (json!({"path": "sub/../notes.md"}), "no '..'"),
        (json!({"path": ".private/notes.md"}), "no hidden components"),
        (json!({"path": ".gitignore"}), "no hidden components"),
        (json!({"path": "link.md"}), "no symlinks on the path"),
        (
            json!({"path": "linkdir/deep.md"}),
            "no symlinks on the path",
        ),
        (json!({"path": "photo.png"}), "supported types only"),
        (
            json!({"path": "secret.md"}),
            ".gitignore and .ignore are honoured",
        ),
        (
            json!({"path": "build/out.md"}),
            ".gitignore and .ignore are honoured",
        ),
        (json!({"path": "nope.md"}), "does not exist"),
        (
            json!({"path": "notes.md", "start_line": 0}),
            "counts from 1",
        ),
        (
            json!({"path": "notes.md", "start_line": 99}),
            "past the end",
        ),
        (json!({"path": "notes.md", "lines": 3}), "unknown field"),
    ];
    for (args, rule) in cases {
        let r = mcp.call("read", args.clone()).await;
        assert_eq!(r["isError"], true, "{args} should be refused: {r}");
        let msg = text_of(&r);
        assert!(msg.contains(rule), "{args}: '{msg}' should name '{rule}'");
    }
    // What the rules do not refuse still reads.
    let ok = mcp.call("read", json!({"path": "./sub/deep.md"})).await;
    assert_ne!(ok["isError"], true, "{ok}");
    assert!(text_of(&ok).ends_with("# deep\n"));

    // The provenance guard still applies to the MCP face.
    let r = app
        .http
        .post(app.url("/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // So does the content-type rule: a form cannot post to it.
    let r = app
        .http
        .post(app.url("/mcp"))
        .header("x-forwarded-host", AUTHORITY)
        .header("x-lmgw-face", "mcp")
        .header("content-type", "text/plain")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

/// The MCP face's trust is lmgw's Admin gate, not an address: its peer is
/// lmgw's own MCP client, so it is served whatever `X-Forwarded-For` says —
/// and only on the face lmgw marks as that gate's.
#[tokio::test]
async fn the_mcp_face_is_not_address_checked_and_the_app_face_cannot_reach_it() {
    let app = synced().await;
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string();
    let send = |face: &'static str, client: Option<&'static str>| {
        let mut r = app
            .http
            .post(app.url("/mcp"))
            .header("x-forwarded-host", AUTHORITY)
            .header("x-lmgw-face", face)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(list.clone());
        if let Some(c) = client {
            r = r.header("x-forwarded-for", c);
        }
        r.send()
    };
    for client in [Some("192.168.1.20"), Some("192.168.1.21"), None] {
        let r = send("mcp", client).await.unwrap();
        assert_eq!(r.status(), 200, "{client:?}");
    }
    // `allow_remote` is off, and the request is from this machine: neither
    // matters, the agent origin does not serve the tools.
    let r = send("app", Some(LOCAL_CLIENT)).await.unwrap();
    assert_eq!(r.status(), 403);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "not_mcp_face", "{v}");
}

#[tokio::test]
async fn search_before_any_sync_says_there_is_no_index_yet() {
    let app = start(
        Fake {
            models: models(json!({"id": "chat-small"})),
            ..Fake::default()
        },
        false,
    )
    .await;
    let mut mcp = Mcp { app: &app, next: 0 };
    let r = mcp.call("search", json!({"query": "zeta"})).await;
    assert_eq!(r["isError"], true);
    assert!(text_of(&r).contains("no index yet"), "{r}");
    let r = mcp.call("search", json!({"query": "zeta", "k": 0})).await;
    assert!(text_of(&r).contains("at least 1"), "{r}");
}

#[tokio::test]
async fn read_returns_a_pdf_s_text_as_the_sync_extracted_it() {
    if !folder_chat::pdf::available().await {
        eprintln!("skipped: pdftotext is not on PATH");
        return;
    }
    let app = synced().await;
    let root = app.tmp.path();
    write(
        root,
        "papers/paper.pdf",
        common::minimal_pdf(&["alpha page", "beta page"]),
    );
    let mut mcp = Mcp { app: &app, next: 0 };
    let got = mcp.call("read", json!({"path": "papers/paper.pdf"})).await;
    assert_ne!(got["isError"], true, "{got}");
    let text = text_of(&got);
    // Piped through stdin, byte for byte what the sync gets by path.
    let by_path = folder_chat::pdf::extract(&root.join("papers/paper.pdf"))
        .await
        .unwrap();
    let body = text.split_once("\n\n").unwrap().1;
    assert_eq!(body, by_path);
    assert!(body.contains("alpha page") && body.contains("beta page"));
    assert!(body.contains('\u{c}'), "pages are separated by form feeds");
}
