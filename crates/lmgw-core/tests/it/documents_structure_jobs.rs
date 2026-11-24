//! WP3 (structure, the block model's renderers) with WP5 (async jobs, the
//! docling-serve routes): a job's stored result carries `structure` as the
//! sync route answers it, the docling route's text form is the native
//! `output: text`, and a removed header's text stays in the response body.

use std::io::Write as _;

use base64::Engine as _;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

use crate::chat_attach_kinds::{docx, pdf_bytes};
use crate::common::{serve, Gw};

async fn gateway() -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    state
        .doc_tools
        .set_search_path(std::env::var_os("PATH").unwrap_or_default());
    let gw = serve(state.clone()).await;
    (state, gw)
}

fn has_poppler() -> bool {
    ["pdftotext", "pdftohtml"]
        .iter()
        .all(|t| std::process::Command::new(t).arg("-v").output().is_ok())
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Pages of body text with `header` at the top of each.
fn pdf_with_header(header: &str, bodies: &[&str]) -> Vec<u8> {
    let n = bodies.len();
    let mut objs: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {n} >>",
            (0..n)
                .map(|i| format!("{} 0 R", 4 + 2 * i))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ];
    for (i, body) in bodies.iter().enumerate() {
        objs.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 600] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
            5 + 2 * i
        ));
        let stream = format!(
            "BT /F1 10 Tf 40 570 Td ({header}) Tj ET BT /F1 12 Tf 40 400 Td ({body}) Tj ET \
             BT /F1 12 Tf 40 380 Td (A second line of the body text.) Tj ET"
        );
        objs.push(format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ));
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        write!(out, "{} 0 obj\n{o}\nendobj\n", i + 1).unwrap();
    }
    let xref = out.len();
    write!(out, "xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).unwrap();
    for off in offsets {
        writeln!(out, "{off:010} 00000 n ").unwrap();
    }
    write!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objs.len() + 1
    )
    .unwrap();
    out
}

fn files(with_pdf: bool) -> Value {
    let mut f = vec![
        json!({"name": "example-a.md", "data_base64": b64(b"# Notes\n\n- one\n- two")}),
        json!({"name": "example-b.docx", "data_base64": b64(&docx("Hello from Word"))}),
    ];
    if with_pdf {
        f.push(json!({"name": "example-c.pdf",
            "data_base64": b64(&pdf_bytes(&[Some("Page one"), Some("Page two")]))}));
    }
    Value::Array(f)
}

async fn get(gw: &Gw, path: &str) -> (u16, Value) {
    let r = gw.client().get(format!("{gw}{path}")).send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

async fn ended(state: &SharedState, gw: &Gw, id: &str) -> Value {
    if let Some(mut rx) = state.convert_jobs.watch(id) {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while rx.changed().await.is_ok() {
                if state.convert_jobs.watch(id).is_none() {
                    break;
                }
            }
        })
        .await;
    }
    get(gw, &format!("/v1/documents/jobs/{id}")).await.1
}

fn quiet(v: &mut Value) {
    for d in v["documents"].as_array_mut().unwrap() {
        d["stats"]["time_ms"] = json!(0);
    }
}

#[tokio::test]
async fn an_async_json_job_stores_the_structure_the_sync_route_answers() {
    let (state, gw) = gateway().await;
    let body = |asynchronous: bool| json!({"files": files(has_poppler()), "output": "json", "async": asynchronous});
    let r = gw
        .client()
        .post(format!("{gw}/v1/documents/convert"))
        .json(&body(true))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 202);
    let id = r.json::<Value>().await.unwrap()["job"]
        .as_str()
        .unwrap()
        .to_string();
    let job = ended(&state, &gw, &id).await;
    assert_eq!(job["state"], "succeeded", "{job}");
    let (s, mut a) = get(&gw, &format!("/v1/documents/jobs/{id}/result")).await;
    assert_eq!(s, 200, "{a}");
    let r = gw
        .client()
        .post(format!("{gw}/v1/documents/convert"))
        .json(&body(false))
        .send()
        .await
        .unwrap();
    let mut sync: Value = r.json().await.unwrap();
    quiet(&mut sync);
    quiet(&mut a);
    assert_eq!(a, sync, "the job's result is the sync answer");
    for d in a["documents"].as_array().unwrap() {
        assert!(d["content"].is_null(), "{d}");
        assert!(d["structure"][0]["blocks"].is_array(), "{d}");
    }
    assert_eq!(
        a["documents"][0]["structure"][0]["blocks"][0]["kind"],
        "heading"
    );
    // One file's entry carries it too, and so does the stored entry.
    let (s, e) = get(&gw, &format!("/v1/documents/jobs/{id}/result?index=1")).await;
    assert_eq!(s, 200);
    assert_eq!(
        e["structure"][0]["blocks"][0]["text"], "Hello from Word",
        "{e}"
    );
    let stored = std::fs::read_to_string(
        state
            .data_dir
            .join("convert")
            .join(&id)
            .join("1")
            .join("entry.json"),
    )
    .unwrap();
    let stored: Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored["structure"], e["structure"]);
    assert!(stored["content"].is_null());
}

async fn docling(gw: &Gw, name: &str, bytes: Vec<u8>, formats: &[&str]) -> Value {
    let mut form = reqwest::multipart::Form::new().part(
        "files",
        reqwest::multipart::Part::bytes(bytes).file_name(name.to_string()),
    );
    for f in formats {
        form = form.text("to_formats", f.to_string());
    }
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/file"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    r.json().await.unwrap()
}

async fn native_content(gw: &Gw, name: &str, bytes: &[u8], output: &str) -> String {
    let r = gw
        .client()
        .post(format!("{gw}/v1/documents/convert"))
        .json(&json!({"files": [{"name": name, "data_base64": b64(bytes)}], "output": output}))
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    v["documents"][0]["content"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn the_docling_routes_md_and_text_are_the_native_forms() {
    let (_state, gw) = gateway().await;
    let mut cases: Vec<(&str, Vec<u8>)> = vec![
        ("example-a.docx", docx("Hello *from* Word")),
        ("example-b.md", b"# Notes\n\n- one\n- two\n".to_vec()),
    ];
    if has_poppler() {
        cases.push((
            "example-c.pdf",
            pdf_with_header("Example header", &["Body one", "Body two", "Body three"]),
        ));
    }
    for (name, bytes) in cases {
        let both = docling(&gw, name, bytes.clone(), &["md", "text"]).await;
        let md_only = docling(&gw, name, bytes.clone(), &["md"]).await;
        let text_only = docling(&gw, name, bytes.clone(), &["text"]).await;
        let native_md = native_content(&gw, name, &bytes, "markdown").await;
        let native_text = native_content(&gw, name, &bytes, "text").await;
        assert_eq!(both["document"]["md_content"], native_md.as_str(), "{name}");
        assert_eq!(
            both["document"]["text_content"],
            native_text.as_str(),
            "{name}"
        );
        assert_eq!(
            md_only["document"]["md_content"],
            native_md.as_str(),
            "{name}"
        );
        assert!(md_only["document"]["text_content"].is_null(), "{name}");
        assert_eq!(
            text_only["document"]["text_content"],
            native_text.as_str(),
            "{name}"
        );
        assert!(text_only["document"]["md_content"].is_null(), "{name}");
    }
    // The markdown's escapes are not in the text form.
    let both = docling(
        &gw,
        "example-a.docx",
        docx("1. not a list"),
        &["md", "text"],
    )
    .await;
    assert_eq!(both["document"]["text_content"], "1. not a list");
    // DoclingDocument JSON is not built: a named gap, not a made-up shape.
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/file"))
        .multipart(
            reqwest::multipart::Form::new()
                .part(
                    "files",
                    reqwest::multipart::Part::bytes(b"x".to_vec()).file_name("example.txt"),
                )
                .text("to_formats", "json"),
        )
        .send()
        .await
        .unwrap();
    assert!(r.headers()["x-lmgw-option-notes"]
        .to_str()
        .unwrap()
        .contains("to_formats=not_yet"));
}

#[tokio::test]
async fn a_removed_headers_text_stays_in_the_body_and_out_of_rows_and_headers() {
    if !has_poppler() {
        return;
    }
    let (state, gw) = gateway().await;
    let header = "Quarterly secret header";
    let pdf = pdf_with_header(header, &["Body one", "Body two", "Body three"]);
    let native: Value = gw
        .client()
        .post(format!("{gw}/v1/documents/convert"))
        .json(&json!({"files": [{"name": "example.pdf", "data_base64": b64(&pdf)}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let w = native["documents"][0]["warnings"].to_string();
    assert!(
        w.contains("header_removed") && w.contains(header),
        "{native}"
    );
    // The docling route: no header text in its headers (its body carries
    // only what was lost, which a removed header is not).
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/file"))
        .multipart(reqwest::multipart::Form::new().part(
            "files",
            reqwest::multipart::Part::bytes(pdf.clone()).file_name("example.pdf"),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let headers = format!("{:?}", r.headers());
    assert!(!headers.contains(header), "{headers}");
    // An async job: its row, its files' rows and the usage rows.
    let r = gw
        .client()
        .post(format!("{gw}/v1/documents/convert"))
        .json(&json!({"files": [{"name": "example.pdf", "data_base64": b64(&pdf)}], "async": true}))
        .send()
        .await
        .unwrap();
    let id = r.json::<Value>().await.unwrap()["job"]
        .as_str()
        .unwrap()
        .to_string();
    let job = ended(&state, &gw, &id).await;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert!(!job.to_string().contains(header), "{job}");
    let row = store::convert_jobs::get_job(&state.db, &id).await.unwrap();
    let files = store::convert_jobs::job_files(&state.db, &id)
        .await
        .unwrap();
    let calls = store::convert_calls(&state.db, "/v1/").await.unwrap();
    let all = format!("{row:?} {files:?} {calls:?}");
    assert!(!all.contains(header), "{all}");
    let (_, result) = get(&gw, &format!("/v1/documents/jobs/{id}/result")).await;
    assert!(result.to_string().contains(header), "the body has it");
}

/// The one-slot PDF path with the docling route's per-file clock: the clock
/// starts at the file's first slot, so a wait for the slot is not late, and
/// a pdftotext that runs past it is stopped, its slot given back.
#[tokio::test]
async fn a_pdfs_one_slot_starts_the_clock_and_a_late_one_is_stopped_and_frees_it() {
    use lmgw_core::extract::convert::pool;
    if has_poppler() {
        let (_state, gw) = gateway().await;
        pool::configure(1);
        let held = pool::global().acquire().await;
        let c = gw.client();
        let url = format!("{gw}/v1/convert/file");
        let pdf = pdf_bytes(&[Some("Page one")]);
        let req = tokio::spawn(async move {
            let form = reqwest::multipart::Form::new()
                .part(
                    "files",
                    reqwest::multipart::Part::bytes(pdf).file_name("example.pdf"),
                )
                .text("document_timeout", "0.5");
            c.post(url)
                .multipart(form)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        });
        // The file's first step waits: its sniffing, in a slot of its own
        // (the text's slot and the picture gate's beside it come after).
        // Waited for, not slept for: under load the request takes its time
        // to get there (a load-sensitive failure at 32 test threads,
        // 2026-11-20).
        crate::documents_model_review::until("the file's first step waits", || {
            pool::global().facts().waiting == 1
        })
        .await;
        // Held past the file's document_timeout: the wait is not late.
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        assert_eq!(pool::global().facts().waiting, 1);
        drop(held);
        let v = req.await.unwrap();
        assert_eq!(v["status"], "success", "{v}");
        assert_eq!(pool::global().facts().running, 0);
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

#[cfg(unix)]
#[tokio::test]
async fn a_pdf_past_its_document_timeout_fails_by_name_and_its_tool_and_slot_go() {
    use lmgw_core::extract::convert::pool;
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
        .part(
            "files",
            reqwest::multipart::Part::bytes(pdf_bytes(&[Some("Page one")]))
                .file_name("example.pdf"),
        )
        .text("document_timeout", "1");
    let client = gw.client();
    let url = format!("{gw}/v1/convert/file");
    let request = tokio::spawn(async move { client.post(url).multipart(form).send().await });
    let started = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let pid = loop {
        if let Some(pid) = stand_in_pid(&dir.path().join("pdftotext")) {
            break pid;
        }
        assert!(std::time::Instant::now() < started, "pdftotext never ran");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    let r = request.await.unwrap().unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["status"], "failure", "{v}");
    assert!(v["errors"][0]["error_message"]
        .as_str()
        .unwrap()
        .contains("document_timeout"));
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let gone = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| {
            s.rsplit_once(") ")
                .is_some_and(|(_, r)| r.starts_with('Z') || r.starts_with('X'))
        });
        if gone && pool::global().facts().running == 0 {
            break;
        }
        assert!(std::time::Instant::now() < until, "tool or slot still held");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
