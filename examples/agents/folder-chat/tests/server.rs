//! The web server end to end over a loopback socket, against the fake lmgw:
//! the four guards (provenance, face, client address, CSRF) and their order,
//! the CSP's frame-ancestors, one sync at a time and the owner's stop, the
//! sync event replay for a late subscriber, the chat stream's order and
//! errors, and the chat body limit derived from the model's context.

mod common;

use common::fake::{models, Fake, Gate};
use common::harness::*;
use serde_json::{json, Value};

fn chat_model(ctx: Option<u64>) -> Fake {
    let chat = match ctx {
        Some(c) => json!({"id": "chat-small", "context_length": c}),
        None => json!({"id": "chat-small"}),
    };
    Fake {
        models: models(chat),
        ..Fake::default()
    }
}

async fn error_code(r: reqwest::Response) -> (u16, String, String) {
    let status = r.status().as_u16();
    let v: Value = r.json().await.unwrap();
    (
        status,
        v["error"]["code"].as_str().unwrap_or_default().to_string(),
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    )
}

#[tokio::test]
async fn provenance_requires_lmgw_s_forwarded_host() {
    let app = start(chat_model(Some(4096)), false).await;

    // Missing: a request straight at the published port.
    let r = app.http.get(app.url("/api/status")).send().await.unwrap();
    let (s, code, msg) = error_code(r).await;
    assert_eq!((s, code.as_str()), (403, "not_via_lmgw"));
    assert!(msg.contains("rule: provenance"), "{msg}");

    // Wrong: some other origin's name (a DNS-rebinding page, another agent).
    let r = app
        .http
        .get(app.url("/"))
        .header("x-forwarded-host", "evil.example:8001")
        .send()
        .await
        .unwrap();
    let (s, code, msg) = error_code(r).await;
    assert_eq!((s, code.as_str()), (403, "not_via_lmgw"));
    assert!(msg.contains("evil.example:8001"), "{msg}");

    // Two of them: lmgw inserts exactly one.
    let r = app
        .http
        .get(app.url("/api/status"))
        .header("x-forwarded-host", AUTHORITY)
        .header("x-forwarded-host", AUTHORITY)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // Right.
    let r = app.get("/api/status").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["config"]["chat_model"], "chat-small");
    assert_eq!(v["sync"]["running"], false);
    assert!(v["skip_reasons"]["ignored"].as_str().is_some());
    // The skips a file earns only when it is read carry their own text too.
    for reason in ["too_large_for_memory", "pdf_timeout"] {
        assert!(
            v["skip_reasons"][reason].as_str().is_some(),
            "{reason}: {}",
            v["skip_reasons"]
        );
    }
    // The retrieval depths a question runs with, by quickdoc's names.
    let c = &v["constants"];
    assert_eq!(
        (&c["k_fts"], &c["k_vec"], &c["rrf_k"], &c["k_rerank"]),
        (&json!(50), &json!(50), &json!(60.0), &json!(20)),
        "{c}"
    );
    let r = app.get("/").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let csp = r.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("script-src 'self'"), "{csp}");
    // The App tab frames the page: the loopback dashboard at the gateway's
    // port (ORIGIN's, 8001) may, nothing else.
    assert!(
        csp.ends_with(
            "; frame-ancestors 'self' http://127.0.0.1:8001 http://localhost:8001 \
             http://[::1]:8001"
        ),
        "{csp}"
    );
    assert!(r
        .text()
        .await
        .unwrap()
        .contains("the folder bound on the Run tab"));

    // The health probe needs no header, and an unknown path is still guarded.
    let r = app.http.get(app.url("/healthz")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = app.http.get(app.url("/nope")).send().await.unwrap();
    assert_eq!(r.status(), 403);
    let r = app.get("/nope").send().await.unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn csrf_needs_the_custom_header_json_and_the_own_origin() {
    let app = start(chat_model(Some(4096)), false).await;
    let post = || {
        app.http
            .post(app.url("/api/sync"))
            .header("x-forwarded-host", AUTHORITY)
            .header("x-lmgw-face", "app")
            .header("x-forwarded-for", LOCAL_CLIENT)
    };

    // No custom header: what a cross-site form post looks like.
    let r = post()
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    let (s, code, msg) = error_code(r).await;
    assert_eq!((s, code.as_str()), (403, "csrf_header"));
    assert!(msg.contains("X-Folder-Chat: 1"), "{msg}");

    // The header, but a form's content type.
    let r = post()
        .header("x-folder-chat", "1")
        .header("content-type", "text/plain")
        .body("{}")
        .send()
        .await
        .unwrap();
    let (s, code, _) = error_code(r).await;
    assert_eq!((s, code.as_str()), (403, "csrf_content_type"));

    // Both, but from another page's origin.
    let r = post()
        .header("x-folder-chat", "1")
        .header("content-type", "application/json")
        .header("origin", "http://evil.example")
        .body("{}")
        .send()
        .await
        .unwrap();
    let (s, code, _) = error_code(r).await;
    assert_eq!((s, code.as_str()), (403, "csrf_origin"));

    // Both, from the app's own page (charset parameter and all).
    let r = post()
        .header("x-folder-chat", "1")
        .header("content-type", "application/json; charset=utf-8")
        .header("origin", ORIGIN)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    app.wait_idle().await;

    // No CORS headers, ever.
    let r = app
        .get("/api/status")
        .header("origin", "http://evil.example");
    let r = r.send().await.unwrap();
    assert!(r.headers().get("access-control-allow-origin").is_none());
}

/// A request as lmgw's proxy would forward it, with the face and client
/// address the test chooses; `None` leaves the header off.
fn via(
    app: &App,
    method: reqwest::Method,
    path: &str,
    face: Option<&str>,
    client: Option<&str>,
) -> reqwest::RequestBuilder {
    let mut r = app
        .http
        .request(method, app.url(path))
        .header("x-forwarded-host", AUTHORITY);
    if let Some(f) = face {
        r = r.header("x-lmgw-face", f);
    }
    if let Some(c) = client {
        r = r.header("x-forwarded-for", c);
    }
    r
}

/// The UI's own `POST` headers, which pass the CSRF rules.
fn as_ui(r: reqwest::RequestBuilder, body: &Value) -> reqwest::RequestBuilder {
    r.header("x-folder-chat", "1")
        .header("content-type", "application/json")
        .body(body.to_string())
}

#[tokio::test]
async fn the_face_rule_keeps_each_face_to_its_own_routes() {
    let app = start(chat_model(Some(4096)), false).await;
    let get = reqwest::Method::GET;
    let post = reqwest::Method::POST;

    // lmgw's MCP client, on the UI's API: the Admin-gated face does not drive
    // the app, even from this machine and with every CSRF header right.
    let question = json!({"history": [], "question": "zeta?"});
    let r = as_ui(
        via(
            &app,
            post.clone(),
            "/api/chat",
            Some("mcp"),
            Some(LOCAL_CLIENT),
        ),
        &question,
    );
    let (s, code, msg) = error_code(r.send().await.unwrap()).await;
    assert_eq!((s, code.as_str()), (403, "not_app_face"));
    assert!(
        msg.contains("'mcp' face") && msg.contains("rule: face"),
        "{msg}"
    );
    assert!(
        msg.contains(ORIGIN),
        "names where the route is served: {msg}"
    );

    // The agent origin, on `/mcp`: what anyone who reaches the gateway port
    // could send with this agent's host name. The tools are not there.
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let r = via(&app, post.clone(), "/mcp", Some("app"), Some(LOCAL_CLIENT))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(list.to_string());
    let (s, code, msg) = error_code(r.send().await.unwrap()).await;
    assert_eq!((s, code.as_str()), (403, "not_mcp_face"));
    assert!(
        msg.contains(
            "the MCP tools are served only through lmgw's admin-gated /agents/folder-chat/mcp"
        ),
        "{msg}"
    );

    // No face at all — an lmgw that does not set it — on either kind of route.
    let r = via(&app, get.clone(), "/api/status", None, Some(LOCAL_CLIENT));
    let (s, code, msg) = error_code(r.send().await.unwrap()).await;
    assert_eq!((s, code.as_str()), (403, "not_app_face"));
    assert!(msg.contains("no X-Lmgw-Face"), "{msg}");
    let r = via(&app, post.clone(), "/mcp", None, Some(LOCAL_CLIENT))
        .header("content-type", "application/json")
        .body(list.to_string());
    let (s, code, _) = error_code(r.send().await.unwrap()).await;
    assert_eq!((s, code.as_str()), (403, "not_mcp_face"));

    // Two: lmgw inserts exactly one.
    let r = via(
        &app,
        get.clone(),
        "/api/status",
        Some("app"),
        Some(LOCAL_CLIENT),
    )
    .header("x-lmgw-face", "app");
    let (s, code, msg) = error_code(r.send().await.unwrap()).await;
    assert_eq!((s, code.as_str()), (403, "not_app_face"));
    assert!(msg.contains("2 X-Lmgw-Face"), "{msg}");

    // The right face, on each.
    let r = via(&app, get.clone(), "/", Some("app"), Some(LOCAL_CLIENT));
    assert_eq!(r.send().await.unwrap().status(), 200);
    let r = via(&app, post.clone(), "/mcp", Some("mcp"), Some(LOCAL_CLIENT))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(list.to_string());
    assert_eq!(r.send().await.unwrap().status(), 200);
}

#[tokio::test]
async fn only_this_machine_is_served_until_the_owner_allows_other_machines() {
    let app = start(chat_model(Some(4096)), false).await;
    let status = |client: Option<&str>| {
        via(
            &app,
            reqwest::Method::GET,
            "/api/status",
            Some("app"),
            client,
        )
    };

    // Loopback, both families, anywhere in 127.0.0.0/8.
    for local in ["127.0.0.1", "::1", "127.0.0.2", "::ffff:127.0.0.1"] {
        let r = status(Some(local)).send().await.unwrap();
        assert_eq!(r.status(), 200, "{local} is this machine");
    }

    // A machine on the LAN, and another container on this box (which reaches
    // the gateway through host.containers.internal and arrives from the host's
    // own interface address, not loopback).
    for remote in ["192.168.1.20", "192.168.1.21", "fe80::1", "2001:db8::7"] {
        let r = status(Some(remote)).send().await.unwrap();
        let (s, code, msg) = error_code(r).await;
        assert_eq!((s, code.as_str()), (403, "remote_client"), "{remote}");
        assert!(msg.contains(remote), "{msg}");
        assert!(
            msg.contains("Serve other machines") && msg.contains("allow_remote"),
            "names the owner's switch: {msg}"
        );
        assert!(msg.contains("rule: client address"), "{msg}");
    }

    // Not through lmgw: missing, not an address, a list, two headers.
    let r = status(None).send().await.unwrap();
    let (s, code, msg) = error_code(r).await;
    assert_eq!((s, code.as_str()), (403, "not_via_lmgw"));
    assert!(msg.contains("no X-Forwarded-For"), "{msg}");
    for bad in ["localhost", "127.0.0.1:5000", "192.168.1.20, 127.0.0.1", ""] {
        let r = status(Some(bad)).send().await.unwrap();
        let (s, code, _) = error_code(r).await;
        assert_eq!((s, code.as_str()), (403, "not_via_lmgw"), "'{bad}'");
    }
    let r = status(Some("127.0.0.1"))
        .header("x-forwarded-for", "127.0.0.1")
        .send()
        .await
        .unwrap();
    let (s, code, _) = error_code(r).await;
    assert_eq!((s, code.as_str()), (403, "not_via_lmgw"));

    // A remote chat is refused before anything is asked of the model.
    let r = as_ui(
        via(
            &app,
            reqwest::Method::POST,
            "/api/chat",
            Some("app"),
            Some("192.168.1.20"),
        ),
        &json!({"history": [], "question": "zeta?"}),
    );
    let (s, code, _) = error_code(r.send().await.unwrap()).await;
    assert_eq!((s, code.as_str()), (403, "remote_client"));
    assert!(app.fake.chat_bodies.lock().unwrap().is_empty());

    // The owner's switch.
    let open = start_with(chat_model(Some(4096)), false, true).await;
    let r = via(
        &open,
        reqwest::Method::GET,
        "/api/status",
        Some("app"),
        Some("192.168.1.20"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let r = as_ui(
        via(
            &open,
            reqwest::Method::POST,
            "/api/sync",
            Some("app"),
            Some("192.168.1.20"),
        ),
        &json!({}),
    );
    assert_eq!(r.send().await.unwrap().status(), 202);
    open.wait_idle().await;
    // It opens the address rule and nothing else.
    let r = via(
        &open,
        reqwest::Method::GET,
        "/api/status",
        Some("app"),
        None,
    )
    .send()
    .await
    .unwrap();
    let (s, code, _) = error_code(r).await;
    assert_eq!((s, code.as_str()), (403, "not_via_lmgw"));
    let r = via(
        &open,
        reqwest::Method::GET,
        "/api/status",
        Some("mcp"),
        Some("192.168.1.20"),
    )
    .send()
    .await
    .unwrap();
    let (s, code, _) = error_code(r).await;
    assert_eq!((s, code.as_str()), (403, "not_app_face"));
}

/// Provenance, face, client address, CSRF: a request wrong on several counts
/// is refused for the first.
#[tokio::test]
async fn the_guards_run_in_order() {
    let app = start(chat_model(Some(4096)), false).await;
    let post = reqwest::Method::POST;
    // Everything wrong: no X-Forwarded-Host, the MCP face, a LAN address, no
    // CSRF header.
    let r = app
        .http
        .post(app.url("/api/sync"))
        .header("x-lmgw-face", "mcp")
        .header("x-forwarded-for", "192.168.1.20")
        .body("{}");
    let (_, code, _) = error_code(r.send().await.unwrap()).await;
    assert_eq!(code, "not_via_lmgw", "provenance first");
    let r = via(
        &app,
        post.clone(),
        "/api/sync",
        Some("mcp"),
        Some("192.168.1.20"),
    )
    .body("{}");
    let (_, code, _) = error_code(r.send().await.unwrap()).await;
    assert_eq!(code, "not_app_face", "then face");
    let r = via(
        &app,
        post.clone(),
        "/api/sync",
        Some("app"),
        Some("192.168.1.20"),
    )
    .body("{}");
    let (_, code, _) = error_code(r.send().await.unwrap()).await;
    assert_eq!(code, "remote_client", "then client address");
    let r = via(
        &app,
        post.clone(),
        "/api/sync",
        Some("app"),
        Some(LOCAL_CLIENT),
    )
    .body("{}");
    let (_, code, _) = error_code(r.send().await.unwrap()).await;
    assert_eq!(code, "csrf_header", "then CSRF");
}

#[tokio::test]
async fn a_second_sync_while_one_runs_is_409() {
    // Call 0 is the embedder's width probe; hold everything after it.
    let (gate, held) = Gate::closed(1);
    let app = start(
        Fake {
            gate: Some(held),
            ..chat_model(Some(4096))
        },
        false,
    )
    .await;
    let r = app.post("/api/sync", &json!({})).send().await.unwrap();
    assert_eq!(r.status(), 202);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["run"], 1);

    let r = app.post("/api/sync", &json!({})).send().await.unwrap();
    let (s, code, _) = error_code(r).await;
    assert_eq!((s, code.as_str()), (409, "sync_running"));
    let v: Value = app
        .get("/api/status")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["sync"]["running"], true);

    gate.open();
    app.wait_idle().await;
    let r = app.post("/api/sync", &json!({})).send().await.unwrap();
    assert_eq!(r.status(), 202, "a finished run frees the slot");
    app.wait_idle().await;
}

fn names(evs: &[Sse]) -> Vec<String> {
    evs.iter()
        .map(|e| match e.name.as_str() {
            "sync" => e.data["type"].as_str().unwrap().to_string(),
            other => other.to_string(),
        })
        .collect()
}

#[tokio::test]
async fn a_subscriber_that_arrives_mid_sync_gets_the_run_so_far_then_live() {
    let (gate, held) = Gate::closed(1);
    let app = start(
        Fake {
            gate: Some(held),
            ..chat_model(Some(4096))
        },
        false,
    )
    .await;
    app.post("/api/sync", &json!({})).send().await.unwrap();
    // Held on the first file's first embedding batch: scanning, planned and
    // that embedding event have happened before anyone is listening.
    // Call 1 reaching the fake means the embedding event was already sent.
    tokio::time::timeout(PATIENCE, async {
        while app
            .fake
            .embed_calls
            .load(std::sync::atomic::Ordering::SeqCst)
            < 2
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    let resp = app.get("/api/sync/events").send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let opener = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        gate.open();
    });
    let evs = sse_until(resp, |e| e.name == "run" && e.data["running"] == false).await;
    opener.await.unwrap();
    let n = names(&evs);
    assert_eq!(
        &n[..4],
        ["run", "scanning", "planned", "embedding"],
        "the replay comes first, in order: {n:?}"
    );
    assert_eq!(evs[0].data["run"], 1);
    assert_eq!(evs[0].data["running"], true);
    let tail: Vec<&str> = n[4..].iter().map(String::as_str).collect();
    assert_eq!(
        tail,
        ["file_done", "embedding", "file_done", "done", "run"],
        "then the live events, none twice: {n:?}"
    );
}

#[tokio::test]
async fn a_subscriber_after_the_sync_gets_the_whole_last_run() {
    let app = start(chat_model(Some(4096)), false).await;
    app.sync().await;
    let resp = app.get("/api/sync/events").send().await.unwrap();
    let evs = sse_until(resp, |e| e.name == "sync" && e.data["type"] == "done").await;
    assert_eq!(
        names(&evs),
        [
            "run",
            "scanning",
            "planned",
            "embedding",
            "file_done",
            "embedding",
            "file_done",
            "done"
        ]
    );
    assert_eq!(evs[0].data["running"], false);
    assert_eq!(evs.last().unwrap().data["report"]["new_files"], 2);
}

#[tokio::test]
async fn chat_streams_meta_first_then_deltas_finish_and_usage() {
    let app = start(chat_model(Some(4096)), false).await;

    // Before any index: a typed error event, not an HTTP error.
    let resp = app
        .post("/api/chat", &json!({"history": [], "question": "zeta?"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let evs = sse_all(resp).await;
    assert_eq!(names(&evs), ["error"]);
    assert_eq!(evs[0].data["code"], "no_index");

    app.sync().await;
    let resp = app
        .post(
            "/api/chat",
            &json!({"history": [], "question": "Which relabel flag do bind mounts need?"}),
        )
        .send()
        .await
        .unwrap();
    assert!(resp.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let evs = sse_all(resp).await;
    let n = names(&evs);
    assert_eq!(n[0], "meta", "{n:?}");
    assert_eq!(&n[n.len() - 2..], ["finish", "usage"], "{n:?}");
    assert!(!n.contains(&"error".to_string()), "{n:?}");
    assert!(n.contains(&"reasoning".to_string()), "{n:?}");

    let meta = &evs[0].data;
    assert_eq!(meta["chat_model"], "chat-small");
    assert_eq!(meta["budget"]["context_tokens"], 4096);
    assert_eq!(meta["budget"]["answer_reserve_reported"], false);
    // The excerpt that answers, wherever reading order numbered it.
    let selinux = |meta: &Value| {
        meta["citations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["heading_path"] == "# Setup > ## SELinux")
            .cloned()
            .unwrap_or_else(|| panic!("{meta}"))
    };
    let first = &selinux(meta);
    assert_eq!(first["path"], "notes.md");
    assert!(first["text"]
        .as_str()
        .unwrap()
        .contains("zeta relabel flag"));
    // The citation carries its line numbers in the file.
    assert_eq!(first["start_line"], 5);
    assert_eq!(first["end_line"], 7);

    // Edited after the sync: the citation still has the lines of the file
    // as it was indexed (stored with the chunk), not a fresh read of it.
    let notes = app.tmp.path().join("notes.md");
    let edited = format!(
        "# Preface\n\nthree new\nlines\n\n{}",
        std::fs::read_to_string(&notes).unwrap()
    );
    std::fs::write(&notes, edited).unwrap();
    let again = sse_all(
        app.post(
            "/api/chat",
            &json!({"history": [], "question": "Which relabel flag do bind mounts need?"}),
        )
        .send()
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(again[0].name, "meta", "{again:?}");
    let cited = &selinux(&again[0].data);
    assert_eq!(cited["path"], "notes.md");
    assert_eq!(
        (&cited["start_line"], &cited["end_line"]),
        (&json!(5), &json!(7))
    );
    assert_eq!(cited["text"], first["text"]);

    let text: String = evs
        .iter()
        .filter(|e| e.name == "text")
        .map(|e| e.data["text"].as_str().unwrap())
        .collect();
    assert_eq!(text, "Use the zeta flag [1].");
    assert_eq!(evs[n.len() - 2].data["reason"], "stop");
    assert_eq!(evs[n.len() - 1].data["prompt_tokens"], 120);

    // An empty question is its own error.
    let evs = sse_all(
        app.post("/api/chat", &json!({"question": "   "}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(evs[0].data["code"], "empty_question");

    // A body that is not a chat request is a plain 400.
    let r = app
        .post("/api/chat", &json!({"prompt": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn a_conversation_too_long_is_an_error_event_saying_start_a_new_chat() {
    // No context_length: the budget assumes FALLBACK_CONTEXT_TOKENS (8192).
    let app = start(chat_model(None), false).await;
    app.sync().await;
    let resp = app
        .post(
            "/api/chat",
            &json!({
                "history": [
                    {"role": "user", "content": "word ".repeat(8000)},
                    {"role": "assistant", "content": "ok"}
                ],
                "question": "and now?"
            }),
        )
        .send()
        .await
        .unwrap();
    let evs = sse_all(resp).await;
    assert_eq!(names(&evs), ["error"], "no meta, no answer");
    assert_eq!(evs[0].data["code"], "conversation_too_long");
    let msg = evs[0].data["message"].as_str().unwrap();
    assert!(msg.contains("start a new chat"), "{msg}");
    assert!(msg.contains("8192"), "the numbers are in it: {msg}");
    assert!(
        app.fake.chat_bodies.lock().unwrap().is_empty(),
        "the model was never asked"
    );

    // Past axum's default 2 MB body limit, under the one derived from a large
    // model's context (100 000 × 48 bytes): still the budget's answer, not a
    // 413 from a cap nobody set.
    let big = start(chat_model(Some(100_000)), false).await;
    big.sync().await;
    let resp = big
        .post(
            "/api/chat",
            &json!({
                "history": [{"role": "user", "content": "x".repeat(3 << 20)}],
                "question": "and now?"
            }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let evs = sse_all(resp).await;
    assert_eq!(evs[0].data["code"], "conversation_too_long");
}

async fn status_json(app: &App) -> Value {
    app.get("/api/status")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// A `POST /api/chat` whose body is `body`, sent chunked (no
/// Content-Length), so the server finds the size by reading.
fn chunked(app: &App, body: String) -> reqwest::RequestBuilder {
    let pieces: Vec<Result<Vec<u8>, std::io::Error>> = body
        .into_bytes()
        .chunks(16 << 10)
        .map(|c| Ok(c.to_vec()))
        .collect();
    app.post("/api/chat", &json!({}))
        .body(reqwest::Body::wrap_stream(futures::stream::iter(pieces)))
}

#[tokio::test]
async fn a_chat_body_over_context_times_max_bytes_per_token_is_413() {
    // 1000 tokens × MAX_BYTES_PER_TOKEN (48) = 48 000 bytes; a small answer
    // reserve, so a conversation under the limit meets the budget's
    // `conversation_too_long` rather than `context_too_small`.
    let app = start(
        Fake {
            models: models(json!({
                "id": "chat-small", "context_length": 1000, "max_output_tokens": 100
            })),
            ..Fake::default()
        },
        false,
    )
    .await;
    app.sync().await;
    let v = status_json(&app).await;
    let limit = &v["chat_body_limit"];
    assert_eq!(limit["bytes"], 48_000, "{v}");
    assert_eq!(limit["context_tokens"], 1000);
    assert_eq!(limit["context_reported"], true);
    assert_eq!(limit["max_bytes_per_token"], 48);
    assert_eq!(limit["chat_model"], "chat-small");
    assert_eq!(v["constants"]["max_bytes_per_token"], 48);

    let over = json!({
        "history": [{"role": "user", "content": "x".repeat(50_000)}],
        "question": "and now?"
    });
    // Declared too long: refused before a byte of it is read.
    let r = app.post("/api/chat", &over).send().await.unwrap();
    let (s, code, msg) = error_code(r).await;
    assert_eq!((s, code.as_str()), (413, "body_too_large"));
    for part in [
        "over the limit of 48000 bytes",
        "chat-small's context_length of 1000 tokens",
        "MAX_BYTES_PER_TOKEN (48)",
        "start a new chat",
    ] {
        assert!(msg.contains(part), "'{part}' in: {msg}");
    }
    // Undeclared (chunked): refused once the reading passes the limit.
    let r = chunked(&app, over.to_string()).send().await.unwrap();
    let (s, code, msg) = error_code(r).await;
    assert_eq!((s, code.as_str()), (413, "body_too_large"));
    assert!(msg.contains("more than 48000 bytes"), "{msg}");
    assert!(
        app.fake.chat_bodies.lock().unwrap().is_empty(),
        "the model was never asked"
    );

    // Under the limit and still too long for the context: the budget's
    // answer, which says the same thing with the token numbers.
    let under = json!({
        "history": [{"role": "user", "content": "x".repeat(40_000)}],
        "question": "and now?"
    });
    let resp = chunked(&app, under.to_string()).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let evs = sse_all(resp).await;
    assert_eq!(evs[0].data["code"], "conversation_too_long");

    // A model that reports no context: the budget's stand-in, named.
    let blind = start(chat_model(None), false).await;
    let v = status_json(&blind).await;
    assert_eq!(v["chat_body_limit"]["bytes"], 8192 * 48, "{v}");
    assert_eq!(v["chat_body_limit"]["context_reported"], false);
    let r = chunked(
        &blind,
        json!({"history": [{"role": "user", "content": "x".repeat(8192 * 48)}], "question": "?"})
            .to_string(),
    )
    .send()
    .await
    .unwrap();
    let (s, code, msg) = error_code(r).await;
    assert_eq!((s, code.as_str()), (413, "body_too_large"));
    assert!(
        msg.contains(
            "FALLBACK_CONTEXT_TOKENS (8192), because chat-small reports no context_length"
        ),
        "{msg}"
    );
}

#[tokio::test]
async fn the_owner_can_stop_a_running_sync() {
    // Call 0 is the embedder's width probe; hold everything after it.
    let (gate, held) = Gate::closed(1);
    let app = start(
        Fake {
            gate: Some(held),
            ..chat_model(Some(4096))
        },
        false,
    )
    .await;
    let stop = || app.post("/api/sync/stop", &json!({}));

    let (s, code, _) = error_code(stop().send().await.unwrap()).await;
    assert_eq!((s, code.as_str()), (409, "no_sync"), "nothing to stop yet");

    assert_eq!(
        app.post("/api/sync", &json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    tokio::time::timeout(PATIENCE, async {
        while app
            .fake
            .embed_calls
            .load(std::sync::atomic::Ordering::SeqCst)
            < 2
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let resp = app.get("/api/sync/events").send().await.unwrap();

    // The same guards as every write.
    let post = reqwest::Method::POST;
    let r = via(
        &app,
        post.clone(),
        "/api/sync/stop",
        Some("app"),
        Some(LOCAL_CLIENT),
    )
    .header("content-type", "application/json")
    .body("{}");
    let (_, code, _) = error_code(r.send().await.unwrap()).await;
    assert_eq!(code, "csrf_header");
    let r = as_ui(
        via(
            &app,
            post.clone(),
            "/api/sync/stop",
            Some("app"),
            Some("192.168.1.20"),
        ),
        &json!({}),
    );
    let (_, code, _) = error_code(r.send().await.unwrap()).await;
    assert_eq!(code, "remote_client");
    let r = as_ui(
        via(
            &app,
            post.clone(),
            "/api/sync/stop",
            Some("mcp"),
            Some(LOCAL_CLIENT),
        ),
        &json!({}),
    );
    let (_, code, _) = error_code(r.send().await.unwrap()).await;
    assert_eq!(code, "not_app_face");
    let r = as_ui(
        app.http
            .post(app.url("/api/sync/stop"))
            .header("x-lmgw-face", "app")
            .header("x-forwarded-for", LOCAL_CLIENT),
        &json!({}),
    );
    let (_, code, _) = error_code(r.send().await.unwrap()).await;
    assert_eq!(code, "not_via_lmgw");
    assert!(app.server.hub().running(), "none of those stopped it");

    // The owner's stop answers once the sync has stopped.
    let r = stop().send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["run"], 1);
    assert!(!app.server.hub().running());
    let evs = sse_until(resp, |e| e.name == "run" && e.data["running"] == false).await;
    let n = names(&evs);
    assert_eq!(
        &n[n.len() - 2..],
        ["aborted", "run"],
        "the aborted event ends the run: {n:?}"
    );
    let aborted = &evs[n.len() - 2].data;
    assert_eq!(aborted["reason"], "stopped by the owner");
    assert_eq!(aborted["kind"], "stopped", "its own kind, not a hold");

    let (s, code, _) = error_code(stop().send().await.unwrap()).await;
    assert_eq!((s, code.as_str()), (409, "no_sync"));
    // The slot is free: the next sync runs to the end.
    gate.open();
    app.sync().await;
}
