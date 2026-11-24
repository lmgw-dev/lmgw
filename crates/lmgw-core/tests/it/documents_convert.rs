//! `POST /v1/documents/convert` (document-conversion design §7.1, §7.5,
//! §7.6) over HTTP: several files in one request, multipart and JSON, each
//! file's outcome its own, the statuses of D18, the named refusals, and the
//! request's usage row.

use base64::Engine as _;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

use crate::chat_attach_kinds::{docx, pdf_bytes};
use crate::common::{serve, Gw};

const PATH: &str = "/v1/documents/convert";

/// A gateway whose tool detection looks where this machine's tools are.
async fn gateway() -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    state
        .doc_tools
        .set_search_path(std::env::var_os("PATH").unwrap_or_default());
    let gw = serve(state.clone()).await;
    (state, gw)
}

fn has_poppler() -> bool {
    std::process::Command::new("pdftotext")
        .arg("-v")
        .output()
        .is_ok()
}

fn part(name: &str, bytes: Vec<u8>) -> reqwest::multipart::Part {
    reqwest::multipart::Part::bytes(bytes).file_name(name.to_string())
}

async fn post_form(gw: &Gw, form: reqwest::multipart::Form) -> (u16, Value) {
    let r = gw
        .client()
        .post(format!("{gw}{PATH}"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

async fn post_json(gw: &Gw, body: &Value) -> (u16, Value) {
    let r = gw
        .client()
        .post(format!("{gw}{PATH}"))
        .json(body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[tokio::test]
async fn several_files_in_one_form_each_get_their_entry_in_order() {
    let (state, gw) = gateway().await;
    let mut form = reqwest::multipart::Form::new()
        .part(
            "file",
            part("example-a.txt", b"# Notes\n\nplain text".to_vec()),
        )
        .part("file", part("example-b.docx", docx("Hello from Word")))
        .part("file", part("example-c.wav", b"RIFF\0\0\0\0WAVE".to_vec()))
        .text("output", "markdown");
    if has_poppler() {
        form = form.part(
            "file",
            part("example-d.pdf", pdf_bytes(&[Some("Page one")])),
        );
    }
    let (status, a) = post_form(&gw, form).await;
    assert_eq!(status, 200, "{a}");
    let docs = a["documents"].as_array().unwrap();
    let names: Vec<&str> = docs.iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(
        &names[..3],
        ["example-a.txt", "example-b.docx", "example-c.wav"]
    );
    assert_eq!(docs[0]["status"], "converted");
    assert_eq!(docs[0]["stats"]["headings"][0], 1);
    assert_eq!(docs[1]["content"], "Hello from Word");
    assert_eq!(docs[1]["format"], "docx");
    // The recording needs the speech-to-text model, and none is chosen
    // (WP7): it fails alone, with its code.
    assert_eq!(docs[2]["status"], "failed");
    assert_eq!(docs[2]["error"]["code"], "model_not_chosen");
    if has_poppler() {
        assert_eq!(docs[3]["status"], "converted", "{}", docs[3]);
        assert_eq!(docs[3]["content"], "Page one");
        assert_eq!(docs[3]["stats"]["parts_by_source"]["text_layer"], 1);
        assert_eq!(a["tools"][0]["id"], "poppler");
    }
    assert_eq!(a["summary"]["failed"], 1);
    assert_eq!(a["warnings"], json!([]));

    // One usage row for the request, as a row that is no model call.
    let rows = doc_rows(&state, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].ingress_proto, "documents");
    assert_eq!(rows[0].status, 200);
    assert_eq!(rows[0].class.as_deref(), Some("tool"));
}

#[tokio::test]
async fn json_files_are_base64_and_a_bad_one_fails_alone() {
    let (_state, gw) = gateway().await;
    let body = json!({
        "files": [
            {"name": "example-a.txt", "data_base64": b64(b"alpha")},
            {"name": "example-b.txt", "data_base64": "not base64!"},
        ],
        "output": "text",
        "pages": "1",
    });
    let (status, a) = post_json(&gw, &body).await;
    assert_eq!(status, 200, "{a}");
    assert_eq!(a["documents"][0]["content"], "alpha");
    assert_eq!(
        a["documents"][0]["warnings"][0]["code"],
        "pages_not_applicable"
    );
    assert_eq!(a["documents"][1]["error"]["code"], "invalid_base64");
}

#[tokio::test]
async fn when_no_file_converts_the_status_follows_d18() {
    let (_state, gw) = gateway().await;
    let one = json!({"files": [{"name": "example.wav", "data_base64": b64(b"RIFF\0\0\0\0WAVE")}]});
    let (status, a) = post_json(&gw, &one).await;
    assert_eq!(status, 415, "{a}");
    assert_eq!(a["documents"][0]["error"]["code"], "model_not_chosen");
    let two = json!({"files": [
        {"name": "example.wav", "data_base64": b64(b"RIFF\0\0\0\0WAVE")},
        {"name": "example.bin", "data_base64": b64(&[0, 159, 146, 150, 0])},
    ]});
    let (status, a) = post_json(&gw, &two).await;
    assert_eq!(status, 422, "{a}");
    assert_eq!(a["summary"]["failed"], 2);
}

#[tokio::test]
async fn unknown_fields_unbuilt_options_and_empty_requests_are_named_400s() {
    let (_state, gw) = gateway().await;
    let file = json!([{"name": "example.txt", "data_base64": b64(b"x")}]);
    for (body, code, says) in [
        (
            json!({"files": file, "colour": "red"}),
            "invalid_option",
            "colour",
        ),
        (
            json!({"files": file, "pages": "0"}),
            "invalid_option",
            "from 1",
        ),
        (json!({"files": []}), "no_files", "no file"),
    ] {
        let (status, e) = post_json(&gw, &body).await;
        assert_eq!(status, 400, "{body}: {e}");
        assert_eq!(e["error"]["code"], code, "{body}: {e}");
        let m = e["error"]["message"].as_str().unwrap();
        assert!(m.contains(says), "{body}: {m}");
    }
    let form = reqwest::multipart::Form::new()
        .part("file", part("example.txt", b"x".to_vec()))
        .text("colour", "red");
    let (status, e) = post_form(&gw, form).await;
    assert_eq!(
        (status, e["error"]["code"].as_str()),
        (400, Some("invalid_option"))
    );
}

#[tokio::test]
async fn descriptions_without_a_chosen_model_are_said_and_the_file_still_converts() {
    let (_state, gw) = gateway().await;
    let form = reqwest::multipart::Form::new()
        .part("file", part("example.txt", b"words".to_vec()))
        .text("describe_pictures", "true")
        .text("picture_prompt", "Describe the example.");
    let (status, a) = post_form(&gw, form).await;
    assert_eq!(status, 200, "{a}");
    let codes: Vec<&str> = a["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["code"].as_str().unwrap())
        .collect();
    // Picture descriptions are built (WP11): with no model chosen they are
    // the one request-wide note, and the prompt goes with them.
    assert_eq!(codes, ["model_not_chosen"]);
    assert_eq!(a["documents"][0]["content"], "words");
}

#[tokio::test]
async fn a_json_body_over_the_limit_names_the_setting_and_base64() {
    let (state, gw) = gateway().await;
    let mut settings = state.snapshot().settings.clone();
    settings.max_body_mb = 1;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let big = vec![b'a'; 1100 * 1024];
    let body = json!({"files": [{"name": "example.txt", "data_base64": b64(&big)}]});
    let (status, e) = post_json(&gw, &body).await;
    assert_eq!(status, 413, "{e}");
    let m = e["error"]["message"].as_str().unwrap();
    assert!(
        m.contains("max_body_mb (1 MiB)") && m.contains("third larger"),
        "{m}"
    );
}

#[tokio::test]
async fn a_pdf_without_poppler_names_the_package_to_install() {
    let state = AppState::init_for_tests().await.unwrap();
    // `init_for_tests` looks nowhere: no tool is found.
    let gw = serve(state.clone()).await;
    let form = reqwest::multipart::Form::new()
        .part("file", part("example.pdf", pdf_bytes(&[Some("Hello")])))
        .part("file", part("example.txt", b"still read".to_vec()));
    let (status, a) = post_form(&gw, form).await;
    assert_eq!(status, 200, "{a}");
    let e = &a["documents"][0]["error"];
    assert_eq!(e["code"], "tool_missing");
    assert_eq!(e["tool"], "poppler");
    assert!(
        e["message"].as_str().unwrap().contains("poppler-utils"),
        "{e}"
    );
    assert_eq!(a["documents"][1]["status"], "converted");
}

/// Whether `pid` is gone or a zombie waiting to be reaped.
#[cfg(unix)]
fn ended(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z') || rest.starts_with('X')),
    }
}

/// The live process whose command line runs the stand-in at `path` (the
/// tools run confined and can write no pid file: the stand-in stays the
/// shell that names it, `sh <path> …`).
#[cfg(unix)]
fn stand_in_pid(path: &std::path::Path) -> Option<u32> {
    let mark = path.display().to_string();
    std::fs::read_dir("/proc").ok()?.find_map(|e| {
        let pid: u32 = e.ok()?.file_name().to_str()?.parse().ok()?;
        let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let named = cmd.split(|b| *b == 0).any(|w| w == mark.as_bytes());
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let live = stat
            .rsplit_once(") ")
            .is_some_and(|(_, r)| !r.starts_with('Z') && !r.starts_with('X'));
        (named && live).then_some(pid)
    })
}

/// A client that goes away while its PDF is read: the request's row is a 499
/// ("client closed request") and the tool it started is stopped. The PDF
/// tools here are fakes: the probe's version answer, and a `pdftotext` that
/// notes its pid and sleeps.
#[cfg(unix)]
#[tokio::test]
async fn a_client_that_goes_away_mid_conversion_leaves_a_499_row_and_stops_the_tool() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    for name in ["pdftotext", "pdftoppm", "pdftohtml"] {
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = -v ]; then echo \"{name} version 99.0.0\" >&2; exit 0; fi\n\
             sleep 60\n"
        );
        let path = dir.path().join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let state = AppState::init_for_tests().await.unwrap();
    state
        .doc_tools
        .set_search_path(dir.path().as_os_str().to_owned());
    let gw = serve(state.clone()).await;
    let form = reqwest::multipart::Form::new()
        .part("file", part("example.pdf", pdf_bytes(&[Some("Page one")])));
    let client = gw.client();
    let url = format!("{gw}{PATH}");
    let request = tokio::spawn(async move { client.post(url).multipart(form).send().await });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let pid: u32 = loop {
        if let Some(pid) = stand_in_pid(&dir.path().join("pdftotext")) {
            break pid;
        }
        assert!(std::time::Instant::now() < deadline, "pdftotext never ran");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    // Dropping the pending request closes its connection.
    request.abort();
    let _ = request.await;
    let row = loop {
        let rows = store::query_logs(
            &state.db,
            &store::LogFilter {
                alias: Some("documents/convert".into()),
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if let Some(r) = rows.into_iter().next() {
            break r;
        }
        assert!(std::time::Instant::now() < deadline, "no row was written");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(row.status, 499, "{row:?}");
    assert_eq!(row.ingress_proto, "documents");
    assert_eq!(row.class.as_deref(), Some("tool"));
    assert!(
        row.error_msg
            .as_deref()
            .is_some_and(|m| m.contains("client disconnected")),
        "{row:?}"
    );
    while !ended(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "pdftotext {pid} still runs after its client went away"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn json_output_answers_each_file_s_structure() {
    let (_state, gw) = gateway().await;
    let mut form = reqwest::multipart::Form::new()
        .part(
            "file",
            part("example-a.md", b"# Notes\n\n- one\n- two".to_vec()),
        )
        .part("file", part("example-b.docx", docx("Hello from Word")))
        .part(
            "file",
            part("example-d.yaml", b"# a comment\nkey: 1\n".to_vec()),
        )
        .text("output", "json");
    if has_poppler() {
        form = form.part(
            "file",
            part("example-c.pdf", pdf_bytes(&[Some("Page one")])),
        );
    }
    let (status, a) = post_form(&gw, form).await;
    assert_eq!(status, 200, "{a}");
    let docs = a["documents"].as_array().unwrap();
    assert_eq!(docs[0]["content"], Value::Null);
    let blocks = &docs[0]["structure"][0]["blocks"];
    assert_eq!(blocks[0]["kind"], "heading");
    assert_eq!(blocks[1]["kind"], "list_item");
    assert_eq!(blocks[2]["text"], "two");
    assert_eq!(docs[1]["structure"][0]["kind"], "document");
    assert_eq!(
        docs[1]["structure"][0]["blocks"][0]["text"],
        "Hello from Word"
    );
    // A text file that is no markdown keeps its text as it is.
    let yaml = &docs[2]["structure"][0]["blocks"];
    assert_eq!(yaml[0]["kind"], "paragraph");
    assert_eq!(yaml[0]["text"], "# a comment\nkey: 1\n");
    if has_poppler() {
        let page = &docs[3]["structure"][0];
        assert_eq!(page["kind"], "page", "{}", docs[3]);
        assert_eq!(page["blocks"][0]["text"], "Page one");
        assert_eq!(page["blocks"][0]["source"], "text_layer");
    }
}

/// The native route's rows once there are `n` of them (or after 10 s): a
/// row is written once its answer has been handed over (security round A
/// verification, N-b), a moment after its client has read it.
pub(crate) async fn doc_rows(state: &SharedState, n: usize) -> Vec<store::RequestLogRow> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let rows = store::query_logs(
            &state.db,
            &store::LogFilter {
                alias: Some("documents/convert".into()),
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if rows.len() >= n || std::time::Instant::now() > deadline {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
