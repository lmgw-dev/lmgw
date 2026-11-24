//! The Documents lab's server (`/documents-lab/api/*`, document-conversion
//! design WP12): a form and its files kept as a group, the run that sends
//! them through the real `/v1/documents/convert` and `/v1/convert/file`
//! handlers, and the run as the lab store keeps it: the route's answer with
//! each text an output artifact named `lab-artifact:<id>`, served back. An
//! async run answers with its job and lands when the job ends; a re-run
//! names kept files instead of uploading them; the route's own bounds
//! (`max_body_mb`) and refusals hold for the lab as for a client.

use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

use crate::chat_attach_kinds::docx;
use crate::common::{serve, Gw};

async fn gateway() -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    state
        .doc_tools
        .set_search_path(std::env::var_os("PATH").unwrap_or_default());
    let gw = serve(state.clone()).await;
    (state, gw)
}

fn part(name: &str, bytes: Vec<u8>) -> reqwest::multipart::Part {
    reqwest::multipart::Part::bytes(bytes).file_name(name.to_string())
}

/// A form: the route, the options, and the files (id 0 = an upload).
fn form(route: &str, options: Value, files: &[(i64, &str)]) -> String {
    json!({
        "route": route,
        "options": options,
        "files": files.iter().map(|(id, n)| json!({"id": id, "name": n})).collect::<Vec<_>>(),
    })
    .to_string()
}

async fn create(gw: &Gw, form_json: String, uploads: Vec<(&str, Vec<u8>)>) -> (u16, Value) {
    let mut f = reqwest::multipart::Form::new().text("form", form_json);
    for (name, bytes) in uploads {
        f = f.part("file", part(name, bytes));
    }
    let r = gw
        .client()
        .post(format!("{gw}/documents-lab/api/groups"))
        .multipart(f)
        .send()
        .await
        .unwrap();
    let s = r.status().as_u16();
    (s, r.json().await.unwrap_or(Value::Null))
}

async fn run(gw: &Gw, id: i64) -> (u16, Value) {
    let r = gw
        .client()
        .post(format!("{gw}/documents-lab/api/groups/{id}/run"))
        .send()
        .await
        .unwrap();
    let s = r.status().as_u16();
    (s, r.json().await.unwrap_or(Value::Null))
}

async fn artifact(gw: &Gw, r: &str) -> Vec<u8> {
    let id = r.strip_prefix("lab-artifact:").expect("a ref");
    gw.client()
        .get(format!("{gw}/labs/api/artifacts/{id}"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap()
        .to_vec()
}

fn response(answer: &Value) -> Value {
    let text = answer["group"]["runs"][0]["response"]
        .as_str()
        .unwrap_or("null");
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn a_run_is_the_native_answer_with_each_text_a_kept_file() {
    let (_state, gw) = gateway().await;
    let (s, g) = create(
        &gw,
        form(
            "native",
            json!({"output": "markdown"}),
            &[
                (0, "example-a.md"),
                (0, "example-b.docx"),
                (0, "example-c.png"),
            ],
        ),
        vec![
            ("example-a.md", b"# Notes\n\nplain text\n".to_vec()),
            ("example-b.docx", docx("Hello from Word")),
            ("example-c.png", b"\x89PNG\r\n\x1a\nrest".to_vec()),
        ],
    )
    .await;
    assert_eq!(s, 200, "{g}");
    assert_eq!(g["lab"], "documents");
    assert_eq!(g["plan"][0]["variant"], "native");
    assert_eq!(g["plan"][0]["model"], "documents/convert");
    assert_eq!(
        g["artifacts"].as_array().unwrap().len(),
        3,
        "the uploads are inputs"
    );
    assert!(
        g["form"]["files"][0]["id"].as_i64().unwrap() > 0,
        "named by their artifacts"
    );
    let id = g["id"].as_i64().unwrap();

    let (s, a) = run(&gw, id).await;
    assert_eq!(s, 200, "{a}");
    let r = &a["group"]["runs"][0];
    assert_eq!(r["http_status"], 200, "{a}");
    assert_eq!(r["endpoint"], "/v1/documents/convert");
    let req: Value = serde_json::from_str(r["request"].as_str().unwrap()).unwrap();
    assert_eq!(req["files"].as_array().unwrap().len(), 3);
    assert_eq!(req["files"][0]["name"], "file");
    assert!(req["files"][0]["artifact"]
        .as_str()
        .unwrap()
        .starts_with("lab-artifact:"));
    let body = response(&a);
    assert_eq!(body["summary"]["converted"], 2, "{body}");
    assert_eq!(body["documents"][2]["status"], "failed");
    let content = body["documents"][0]["content"].as_str().unwrap();
    assert!(content.starts_with("lab-artifact:"), "{content}");
    let md = String::from_utf8(artifact(&gw, content).await).unwrap();
    assert!(md.contains("Notes"), "{md}");
    let word = artifact(&gw, body["documents"][1]["content"].as_str().unwrap()).await;
    assert!(String::from_utf8_lossy(&word).contains("Hello from Word"));

    // The kept history reads it back the same way.
    let page: Value = gw
        .client()
        .get(format!("{gw}/labs/api/documents/groups"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["groups"][0]["runs"][0]["http_status"], 200);
    // A second run of the same group is the lab's 409.
    assert_eq!(run(&gw, id).await.0, 409);
}

#[tokio::test]
async fn json_output_keeps_the_structure_as_a_file() {
    let (_state, gw) = gateway().await;
    let (_, g) = create(
        &gw,
        form("native", json!({"output": "json"}), &[(0, "example.md")]),
        vec![("example.md", b"# Title\n\n- one\n- two\n".to_vec())],
    )
    .await;
    let (_, a) = run(&gw, g["id"].as_i64().unwrap()).await;
    let body = response(&a);
    let r = body["documents"][0]["structure"].as_str().unwrap();
    let parts: Value = serde_json::from_slice(&artifact(&gw, r).await).unwrap();
    assert_eq!(parts[0]["kind"], "document");
    assert_eq!(parts[0]["blocks"][0]["kind"], "heading");
    assert!(body["documents"][0]["content"].is_null());
}

#[tokio::test]
async fn the_docling_route_answers_its_json_for_one_file_and_its_zip_for_several() {
    let (_state, gw) = gateway().await;
    let (_, g) = create(
        &gw,
        form(
            "docling",
            json!({"output": "markdown"}),
            &[(0, "example.md")],
        ),
        vec![("example.md", b"# Docling\n\ntext\n".to_vec())],
    )
    .await;
    assert_eq!(g["plan"][0]["variant"], "docling");
    let (_, a) = run(&gw, g["id"].as_i64().unwrap()).await;
    let r = &a["group"]["runs"][0];
    assert_eq!(r["endpoint"], "/v1/convert/file");
    assert_eq!(r["http_status"], 200, "{a}");
    assert!(r["headers"]["x-lmgw-documents"]
        .as_str()
        .unwrap()
        .contains("converted=1"));
    let body = response(&a);
    let md = artifact(&gw, body["document"]["md_content"].as_str().unwrap()).await;
    assert!(String::from_utf8_lossy(&md).contains("Docling"));
    let req: Value = serde_json::from_str(r["request"].as_str().unwrap()).unwrap();
    assert_eq!(req["files"][0]["name"], "files");
    assert_eq!(req["fields"][0]["name"], "to_formats");

    let (_, g) = create(
        &gw,
        form("docling", json!({}), &[(0, "a.md"), (0, "b.md")]),
        vec![("a.md", b"# A\n".to_vec()), ("b.md", b"# B\n".to_vec())],
    )
    .await;
    let (_, a) = run(&gw, g["id"].as_i64().unwrap()).await;
    let r = &a["group"]["runs"][0];
    assert_eq!(r["response_type"], "application/zip", "{a}");
    let names: Vec<&str> = r["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names[0], "converted_docs.zip");
    assert!(
        names.contains(&"a.md") && names.contains(&"b.md"),
        "{names:?}"
    );
}

#[tokio::test]
async fn an_async_run_answers_its_job_and_lands_when_the_job_ends() {
    let (state, gw) = gateway().await;
    let (_, g) = create(
        &gw,
        form("native", json!({"async": true}), &[(0, "example.md")]),
        vec![("example.md", b"# Later\n\nbody\n".to_vec())],
    )
    .await;
    assert_eq!(g["plan"][0]["variant"], "async");
    let id = g["id"].as_i64().unwrap();
    let (s, a) = run(&gw, id).await;
    assert_eq!(s, 200, "{a}");
    let job = a["job"].as_str().expect("a job").to_string();
    assert!(job.starts_with("cj_"));
    // The owner's job, never a keyless one (security round A, L7).
    let row = lmgw_core::store::convert_jobs::get_job(&state.db, &job)
        .await
        .unwrap()
        .unwrap();
    assert!(row.key_id.is_some(), "the lab's job has the owner's key");
    // The page follows the job on the public route.
    let st = gw
        .client()
        .get(format!("{gw}/v1/documents/jobs/{job}"))
        .send()
        .await
        .unwrap();
    assert_eq!(st.status().as_u16(), 200);
    // The group settles with the job's result as its run.
    let mut landed = Value::Null;
    for _ in 0..400 {
        let g: Value = gw
            .client()
            .get(format!("{gw}/labs/api/documents/groups/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if !g["in_flight"].as_bool().unwrap_or(true) && !g["runs"].as_array().unwrap().is_empty() {
            landed = g;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let r = &landed["runs"][0];
    assert_eq!(r["http_status"], 200, "{landed}");
    assert!(r["headers"]["location"].as_str().unwrap().ends_with(&job));
    let body: Value = serde_json::from_str(r["response"].as_str().unwrap()).unwrap();
    let md = artifact(&gw, body["documents"][0]["content"].as_str().unwrap()).await;
    assert!(String::from_utf8_lossy(&md).contains("Later"));
}

#[tokio::test]
async fn a_rerun_names_kept_files_and_uploads_nothing() {
    let (_state, gw) = gateway().await;
    let (_, g) = create(
        &gw,
        form("native", json!({}), &[(0, "example.md")]),
        vec![("example.md", b"# Kept\n".to_vec())],
    )
    .await;
    let kept = g["artifacts"][0]["id"].as_i64().unwrap();
    let (s, again) = create(
        &gw,
        form("native", json!({"output": "text"}), &[(kept, "example.md")]),
        Vec::new(),
    )
    .await;
    assert_eq!(s, 200, "{again}");
    assert_eq!(again["artifacts"][0]["sha256"], g["artifacts"][0]["sha256"]);
    let (_, a) = run(&gw, again["id"].as_i64().unwrap()).await;
    let body = response(&a);
    let text = artifact(&gw, body["documents"][0]["content"].as_str().unwrap()).await;
    assert!(String::from_utf8_lossy(&text).contains("Kept"));
    let req: Value =
        serde_json::from_str(a["group"]["runs"][0]["request"].as_str().unwrap()).unwrap();
    assert_eq!(req["fields"][0], json!({"name": "output", "value": "text"}));

    // A ref the store does not keep, and an upload the form does not list.
    let (s, e) = create(
        &gw,
        form("native", json!({}), &[(999_999, "gone.md")]),
        Vec::new(),
    )
    .await;
    assert_eq!(s, 400, "{e}");
    assert_eq!(e["error"]["code"], "documents_lab_form");
    let (s, _) = create(
        &gw,
        form("native", json!({}), &[]),
        vec![("x.md", b"x".to_vec())],
    )
    .await;
    assert_eq!(s, 400);
    // Several page spans have no docling spelling: refused before keeping.
    let (s, e) = create(
        &gw,
        form("docling", json!({"pages": "1-2,5"}), &[(0, "x.md")]),
        vec![("x.md", b"x".to_vec())],
    )
    .await;
    assert_eq!(s, 400);
    assert!(
        e["error"]["message"].as_str().unwrap().contains("one span"),
        "{e}"
    );
    let page: Value = gw
        .client()
        .get(format!("{gw}/labs/api/documents/groups"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["total"], 2, "refused forms keep no group");
}

#[tokio::test]
async fn files_past_max_body_mb_get_the_routes_own_413_and_nothing_is_kept() {
    let (state, gw) = gateway().await;
    let mut settings = state.snapshot().settings.clone();
    settings.max_body_mb = 1;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let big = vec![b'a'; 1536 * 1024];
    let (s, e) = create(
        &gw,
        form("native", json!({}), &[(0, "big.txt")]),
        vec![("big.txt", big.clone())],
    )
    .await;
    assert_eq!(s, 413, "refused while the upload streams in: {e}");
    assert_eq!(e["error"]["code"], "body_limit");
    assert!(e["error"]["message"]
        .as_str()
        .unwrap()
        .contains("max_body_mb (1 MiB)"));
    // Two files that pass it together are refused as one request.
    let half = vec![b'b'; 700 * 1024];
    let (s, _) = create(
        &gw,
        form("native", json!({}), &[(0, "a.txt"), (0, "b.txt")]),
        vec![("a.txt", half.clone()), ("b.txt", half.clone())],
    )
    .await;
    assert_eq!(s, 413);
    // The docling route answers in docling's own envelope.
    let (s, e) = create(
        &gw,
        form("docling", json!({}), &[(0, "big.txt")]),
        vec![("big.txt", big)],
    )
    .await;
    assert_eq!(s, 413, "{e}");
    assert!(e.get("detail").is_some() || e.get("error").is_some(), "{e}");
    let page: Value = gw
        .client()
        .get(format!("{gw}/labs/api/documents/groups"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["total"], 0, "a refused upload keeps nothing: {page}");
    let left: Vec<_> = std::fs::read_dir(state.data_dir.join("labs").join("artifacts"))
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "no spool file stays: {left:?}");
    // One that fits is kept and runs.
    let (s, g) = create(
        &gw,
        form("native", json!({}), &[(0, "ok.txt")]),
        vec![("ok.txt", half)],
    )
    .await;
    assert_eq!(s, 200, "{g}");
    let (s, a) = run(&gw, g["id"].as_i64().unwrap()).await;
    assert_eq!(s, 200);
    assert_eq!(a["group"]["runs"][0]["http_status"], 200, "{a}");
}

/// A lab run is a request like a client's: its usage row is the route's own
/// (the alias `documents/convert`, no model call).
#[tokio::test]
async fn a_run_writes_the_routes_usage_row() {
    let (state, gw) = gateway().await;
    let (_, g) = create(
        &gw,
        form("native", json!({"output": "text"}), &[(0, "example.md")]),
        vec![("example.md", b"# Row\n".to_vec())],
    )
    .await;
    let (s, _) = run(&gw, g["id"].as_i64().unwrap()).await;
    assert_eq!(s, 200);
    let rows = crate::documents_convert::doc_rows(&state, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].ingress_proto, "documents");
    assert_eq!(rows[0].status, 200);
    assert_eq!(rows[0].class.as_deref(), Some("tool"));
}

/// The lab's routes need the owner's admin capability: a client's inference
/// key, which `/v1` takes, is a 403 on `/documents-lab/api/*`, and no
/// credential a 401.
#[tokio::test]
async fn a_plain_inference_key_is_forbidden_on_the_labs_routes() {
    let (state, gw) = gateway().await;
    let plaintext = "lmgw-client-key";
    sqlx::query("INSERT INTO api_keys (name, key_hash, enabled) VALUES ('laptop', ?1, 1)")
        .bind(lmgw_core::config::hash_api_key(plaintext))
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    for path in [
        "/documents-lab/api/groups",
        "/documents-lab/api/groups/1/run",
    ] {
        let f = reqwest::multipart::Form::new().text("form", "{}");
        let keyed = gw
            .anon()
            .post(format!("{gw}{path}"))
            .header("authorization", format!("Bearer {plaintext}"))
            .multipart(f)
            .send()
            .await
            .unwrap();
        assert_eq!(keyed.status().as_u16(), 403, "POST {path} with a key");
        let body: Value = keyed.json().await.unwrap();
        assert_eq!(body["code"], "forbidden", "{body}");
        let bare = gw.anon().post(format!("{gw}{path}")).send().await.unwrap();
        assert_eq!(bare.status().as_u16(), 401, "POST {path} with nothing");
    }
    let page = gw
        .anon()
        .get(format!("{gw}/labs/api/documents/groups"))
        .header("authorization", format!("Bearer {plaintext}"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status().as_u16(), 403);
    // The key is fine on the route itself.
    let ok = gw
        .anon()
        .post(format!("{gw}/v1/documents/convert"))
        .header("authorization", format!("Bearer {plaintext}"))
        .json(&json!({"files": [], "output": "text"}))
        .send()
        .await
        .unwrap();
    assert_ne!(ok.status().as_u16(), 403);
}

/// Cancelling an async run's job keeps the run as the 409 `job_cancelled`
/// the route's result answers for it, not as a lost group.
#[tokio::test]
async fn cancelling_an_async_job_keeps_the_run_as_job_cancelled() {
    use lmgw_core::extract::convert::pool;
    let (_state, gw) = gateway().await;
    // One slot, held here: the job's file waits for it.
    pool::configure(1);
    let held = pool::global().acquire().await;
    let (_, g) = create(
        &gw,
        form("native", json!({"async": true}), &[(0, "example.md")]),
        vec![("example.md", b"# Never\n".to_vec())],
    )
    .await;
    let id = g["id"].as_i64().unwrap();
    let (s, a) = run(&gw, id).await;
    assert_eq!(s, 200, "{a}");
    let job = a["job"].as_str().expect("a job").to_string();
    let r = gw
        .client()
        .delete(format!("{gw}/v1/documents/jobs/{job}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    drop(held);
    let mut landed = Value::Null;
    for _ in 0..400 {
        let g: Value = gw
            .client()
            .get(format!("{gw}/labs/api/documents/groups/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if !g["in_flight"].as_bool().unwrap_or(true) && !g["runs"].as_array().unwrap().is_empty() {
            landed = g;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let r = &landed["runs"][0];
    assert_eq!(r["http_status"], 409, "{landed}");
    assert_eq!(r["error_code"], "job_cancelled");
}

#[tokio::test]
async fn document_options_are_a_preset_of_the_documents_lab() {
    let (_state, gw) = gateway().await;
    let r = gw
        .client()
        .post(format!("{gw}/labs/api/documents/presets"))
        .json(&json!({
            "name": "pages and json",
            "kind": "document_options",
            "body": {"route": "native", "options": {"output": "json", "pages": "1-3"}},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "{}", r.text().await.unwrap());
    let list: Value = gw
        .client()
        .get(format!(
            "{gw}/labs/api/documents/presets?kind=document_options"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["presets"][0]["body"]["options"]["pages"], "1-3");
    assert_eq!(list["presets"][0]["lab"], "documents");
}
