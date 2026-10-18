//! What the Hugging Face, job and image-recipe ops answer over HTTP: each
//! answer reads back through the type the document describes for it, and keeps
//! the keys it has always sent. Which are `null`, which are whole numbers (a
//! byte count past 2^32 included), which are lists and which an action leaves
//! out.
//!
//! The hub is a mock server behind `HF_ENDPOINT`, so every test holds
//! `common::process_env_lock` for its whole body, and a download is a fake
//! `hf_download` executor that reports some bytes and waits to be cancelled:
//! the rows and the live columns are the gateway's own, no transfer is real.

use std::sync::Arc;
use std::time::Duration;

use lmgw_api_types as dto;
use lmgw_core::erased::Erased;
use lmgw_core::jobs::{self, JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{self, serve, Gw};

/// Any job kind: reports `done` of `total` once, then ends as its input's
/// `mode` says (`hold` until cancelled, `done`, `fail`).
struct Scripted {
    kind: JobKind,
}

impl JobExecutor for Scripted {
    fn kind(&self) -> JobKind {
        self.kind
    }

    fn run<'a>(&'a self, ctx: JobCtx, input: Value) -> Erased<'a, Result<JobOutcome, String>> {
        Box::pin(async move {
            // A download names its row; its total is known for an odd row id only.
            let (done, total, detail) = match input["hf_model_id"].as_i64() {
                Some(id) => (
                    500,
                    (id % 2 == 1).then_some(1000),
                    json!({"hf_model_id": id}),
                ),
                None => (
                    input["done"].as_u64().unwrap_or(3),
                    input["total"].as_u64(),
                    json!({"url": "https://example.test/x"}),
                ),
            };
            ctx.progress(JobProgress {
                done,
                total,
                stage: "working".into(),
                detail,
            })
            .await;
            match input["mode"].as_str().unwrap_or("hold") {
                "done" => Ok(JobOutcome::Done(json!({"steps": done}))),
                "fail" => Err("executor said no".into()),
                _ => loop {
                    if ctx.canceled() {
                        return Ok(JobOutcome::Canceled);
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                },
            }
        })
    }
}

struct Fixture {
    state: SharedState,
    gw: Gw,
    hub: MockServer,
    _dirs: Vec<tempfile::TempDir>,
    _env: common::EnvGuard,
}

/// A gateway with all four models dirs set, a mock hub behind `HF_ENDPOINT`,
/// and the fake executor for every kind a test spawns.
async fn fixture() -> Fixture {
    let env = common::process_env_lock().await;
    let hub = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let state = AppState::init_for_tests().await.unwrap();
    for kind in [
        JobKind::HfDownload,
        JobKind::Ingest,
        JobKind::ReEmbed,
        JobKind::EvalRun,
        JobKind::GoldenGen,
    ] {
        state.jobs.register(Arc::new(Scripted { kind }));
    }
    let dirs: Vec<tempfile::TempDir> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut s = state.snapshot().settings.clone();
    s.router.models_dir = dirs[0].path().display().to_string();
    s.aux_router.models_dir = dirs[1].path().display().to_string();
    s.audio.models_dir = dirs[2].path().display().to_string();
    s.image.models_dir = dirs[3].path().display().to_string();
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    Fixture {
        state,
        gw,
        hub,
        _dirs: dirs,
        _env: env,
    }
}

async fn tree(hub: &MockServer, repo: &str, entries: Value) {
    Mock::given(method("GET"))
        .and(path(format!("/api/models/{repo}/tree/main")))
        .respond_with(ResponseTemplate::new(200).set_body_json(entries))
        .mount(hub)
        .await;
}

async fn get(gw: &Gw, p: &str) -> Value {
    let resp = gw.client().get(format!("{gw}{p}")).send().await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "GET {p}: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("GET {p} is not JSON ({e}): {body}"))
}

/// `POST /api/op/{name}`, which has to answer 200.
async fn op(gw: &Gw, name: &str, args: Value) -> Value {
    let (status, body) = op_status(gw, name, args.clone()).await;
    assert_eq!(status, 200, "{name} {args}: {body}");
    body
}

async fn op_status(gw: &Gw, name: &str, args: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    let json = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{name} is not JSON ({e})"));
    (status, json)
}

/// The keys of `live`, to be exactly `want` (in any order).
fn assert_keys(live: &Value, want: &[&str]) {
    let mut got: Vec<&str> = live
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {live}"))
        .keys()
        .map(String::as_str)
        .collect();
    got.sort_unstable();
    let mut want = want.to_vec();
    want.sort_unstable();
    assert_eq!(got, want, "{live}");
}

fn is_whole(v: &Value) -> bool {
    v.is_u64() || v.is_i64()
}

fn required(name: &str) -> Vec<String> {
    let doc = lmgw_core::openapi::admin_doc();
    let schemas = common::documented_schemas(doc, name);
    assert_eq!(schemas.len(), 1, "{name}: {schemas:?}");
    let mut r: Vec<String> = schemas[0]["required"]
        .as_array()
        .unwrap_or_else(|| panic!("no required list in {}", schemas[0]))
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    r.sort();
    r
}

#[tokio::test]
async fn the_document_names_the_type_each_download_op_answers_with() {
    let doc = lmgw_core::openapi::admin_doc();
    // What every answer sends is required; `note` and the new add keys too.
    assert_eq!(required("RepoFiles"), ["files", "note", "repo", "target"]);
    assert_eq!(
        required("HfAddResult"),
        [
            "companions",
            "dest_prefix",
            "downloads",
            "files_queued",
            "gguf_path",
            "message",
            "ok",
            "repo",
            "target",
            "warnings",
            "weights",
        ]
    );
    assert_eq!(
        required("HfUpdatesChecked"),
        ["errors", "ok", "updates_available"]
    );

    let answer = |op: &str| {
        doc["paths"][format!("/api/op/{op}")]["post"]["responses"]["200"]["content"]
            ["application/json"]["schema"]
            .clone()
    };
    assert_eq!(answer("hf_add")["title"], "HfAddResult");
    assert_eq!(answer("job_cancel")["title"], "MessageAck");
    // `hf_set` answers a message for three actions and an update count for
    // the fourth: two arms, and a reader reads the one for the action it sent.
    let schema = answer("hf_set");
    let arms: Vec<&str> = schema["anyOf"]
        .as_array()
        .unwrap_or_else(|| panic!("an anyOf of the two answers: {schema}"))
        .iter()
        .filter_map(|a| a["$ref"].as_str())
        .collect();
    assert_eq!(
        arms,
        [
            "#/components/schemas/MessageAck",
            "#/components/schemas/HfUpdatesChecked",
        ]
    );
}

#[tokio::test]
async fn a_repo_listing_carries_a_note_and_whole_byte_counts() {
    let f = fixture().await;
    tree(
        &f.hub,
        "p/chat",
        json!([
            {"type": "file", "path": "chat-Q8_0.gguf", "size": 8_123_456_789u64},
            {"type": "file", "path": "split-UD-Q4_K_XL-00001-of-00002.gguf", "size": 5_000_000_000u64},
            {"type": "file", "path": "split-UD-Q4_K_XL-00002-of-00002.gguf", "size": 3_000_000_000u64},
            {"type": "file", "path": "nosize.gguf"},
            {"type": "file", "path": "README.md", "size": 10},
        ]),
    )
    .await;
    tree(
        &f.hub,
        "p/image",
        json!([
            {"type": "file", "path": "split_files/vae/ae.safetensors", "size": 335_304_388u64},
            {"type": "file", "path": "sd_xl_base.safetensors", "size": 6_938_078_334u64},
        ]),
    )
    .await;

    let live = get(&f.gw, "/api/hf/repo?repo=p/chat").await;
    let listed = common::round_trips::<dto::RepoFiles>("GET /api/hf/repo", &live);
    assert_keys(&live, &["repo", "target", "files", "note"]);
    assert_eq!(listed.target, "chat");
    assert!(listed.note.contains("lmgw__hf_add"), "{}", listed.note);
    let by_file = |name: &str| {
        listed
            .files
            .iter()
            .find(|f| f.file == name)
            .unwrap_or_else(|| panic!("{name} in {:?}", listed.files))
    };
    // A byte count past 2^32 is still a whole number, never a float.
    let big = by_file("chat-Q8_0.gguf");
    assert_eq!(big.size_bytes, 8_123_456_789);
    let row = live["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["file"] == "chat-Q8_0.gguf")
        .unwrap();
    assert!(
        is_whole(&row["size_bytes"]) && is_whole(&row["parts"]),
        "{row}"
    );
    assert_keys(
        row,
        &["file", "role", "quant", "parts", "size_bytes", "size"],
    );
    assert_eq!(row["quant"], "Q8_0");
    // A split GGUF is one entry: the first part, the parts' total.
    let split = by_file("split-UD-Q4_K_XL-00001-of-00002.gguf");
    assert_eq!((split.parts, split.size_bytes), (2, 8_000_000_000));
    // No size from the hub is 0; no quant label is `null`, the key still there.
    let none = live["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["file"] == "nosize.gguf")
        .unwrap();
    assert_eq!(none["size_bytes"], 0);
    assert!(
        none["quant"].is_null() && none.get("quant").is_some(),
        "{none}"
    );

    // The image class has its own note and its own roles.
    let live = get(&f.gw, "/api/hf/repo?repo=p/image&target=image").await;
    let image = common::round_trips::<dto::RepoFiles>("GET /api/hf/repo (image)", &live);
    assert_eq!(image.target, "image");
    assert!(image.note.contains("lmgw__image_recipes"), "{}", image.note);
    assert_ne!(image.note, listed.note);
    let roles: Vec<&str> = image.files.iter().map(|f| f.role.as_str()).collect();
    assert_eq!(roles, ["vae", "checkpoint"]);
    assert!(image.files.iter().all(|f| f.quant.is_none()));
}

#[tokio::test]
async fn an_add_names_what_it_queued_what_came_with_it_and_what_to_look_out_for() {
    let f = fixture().await;
    tree(
        &f.hub,
        "p/chat",
        json!([
            {"type": "file", "path": "chat-Q4_K_M.gguf", "size": 4_000_000_000u64},
            {"type": "file", "path": "mmproj-F16.gguf", "size": 900_000_000u64},
            {"type": "file", "path": "mtp-draft-Q4_K_M.gguf", "size": 100},
        ]),
    )
    .await;
    tree(
        &f.hub,
        "p/image",
        json!([
            {"type": "file", "path": "sharded/m-00001-of-00002.safetensors", "size": 5},
            {"type": "file", "path": "sharded/m-00002-of-00002.safetensors", "size": 4},
        ]),
    )
    .await;

    // The projector and the drafter come along by default.
    let live = op(
        &f.gw,
        "hf_add",
        json!({"repo": "p/chat", "file": "chat-Q4_K_M.gguf"}),
    )
    .await;
    let added = common::round_trips::<dto::HfAddResult>("POST /api/op/hf_add", &live);
    assert_keys(
        &live,
        &[
            "ok",
            "repo",
            "weights",
            "companions",
            "files_queued",
            "downloads",
            "warnings",
            "gguf_path",
            "dest_prefix",
            "target",
            "message",
        ],
    );
    let companions: Vec<(&str, &str)> = added
        .companions
        .iter()
        .map(|c| (c.file.as_str(), c.role.as_str()))
        .collect();
    assert_eq!(
        companions,
        [
            ("mmproj-F16.gguf", "mmproj"),
            ("mtp-draft-Q4_K_M.gguf", "drafter")
        ]
    );
    assert_eq!(added.files_queued, 3);
    assert!(is_whole(&live["files_queued"]));
    assert_eq!(added.downloads.len(), 3);
    assert!(added
        .downloads
        .iter()
        .all(|d| d.id > 0 && d.status == "queued"));
    assert_eq!(added.dest_prefix, "p/chat/");
    assert_eq!(added.gguf_path, "p/chat/chat-Q4_K_M.gguf");
    assert_eq!(added.target, "chat");
    assert!(added.warnings.is_empty());
    assert_eq!(live["warnings"], json!([]), "an empty list, not absent");

    // Without them: still an empty list, and one file.
    let live = op(
        &f.gw,
        "hf_add",
        json!({"repo": "p/chat", "quant": "q4_k_m", "companions": false, "target": "aux"}),
    )
    .await;
    let added = common::round_trips::<dto::HfAddResult>("POST /api/op/hf_add (aux)", &live);
    assert_eq!(live["companions"], json!([]));
    assert_eq!((added.files_queued, added.target.as_str()), (1, "aux"));

    // An aux row takes a projector but has no drafter (decision-models design
    // §3.2), so aux companions are the projector alone.
    tree(
        &f.hub,
        "p/vl-decision",
        json!([
            {"type": "file", "path": "Clef-Q4_K_M.gguf", "size": 16_000_000_000u64},
            {"type": "file", "path": "mmproj-Clef-F16.gguf", "size": 900_000_000u64},
            {"type": "file", "path": "mtp-draft-Q4_K_M.gguf", "size": 100},
        ]),
    )
    .await;
    let live = op(
        &f.gw,
        "hf_add",
        json!({"repo": "p/vl-decision", "file": "Clef-Q4_K_M.gguf", "target": "aux"}),
    )
    .await;
    let added = common::round_trips::<dto::HfAddResult>("POST /api/op/hf_add (aux vl)", &live);
    assert_eq!(
        live["companions"],
        json!([{"file": "mmproj-Clef-F16.gguf", "role": "mmproj"}]),
        "{live}"
    );
    assert_eq!(added.files_queued, 2);

    // A shard of a sharded .safetensors is queued alone, and says so.
    let live = op(
        &f.gw,
        "hf_add",
        json!({"repo": "p/image", "file": "sharded/m-00001-of-00002.safetensors", "target": "image"}),
    )
    .await;
    let added = common::round_trips::<dto::HfAddResult>("POST /api/op/hf_add (image)", &live);
    assert_eq!(added.warnings.len(), 1, "{:?}", added.warnings);
    assert!(
        added.warnings[0].contains("shard 1 of 2"),
        "{:?}",
        added.warnings
    );
    assert!(added.companions.is_empty() && added.files_queued == 1);
}

/// A split GGUF lists every part it queued, `files_queued` is the length of
/// `downloads`, and a row of the same file fetched for another target is not
/// among them.
#[tokio::test]
async fn hf_add_lists_every_part_it_queued_and_only_those() {
    let f = fixture().await;
    tree(
        &f.hub,
        "p/split",
        json!([
            {"type": "file", "path": "m-Q4_K_M-00001-of-00002.gguf", "size": 5},
            {"type": "file", "path": "m-Q4_K_M-00002-of-00002.gguf", "size": 3},
        ]),
    )
    .await;
    let add = |target: &'static str| {
        let gw = &f.gw;
        async move {
            let live = op(
                gw,
                "hf_add",
                json!({"repo": "p/split", "file": "m-Q4_K_M-00001-of-00002.gguf",
                       "companions": false, "target": target}),
            )
            .await;
            common::round_trips::<dto::HfAddResult>("POST /api/op/hf_add", &live)
        }
    };
    let chat = add("chat").await;
    assert_eq!(chat.files_queued, 2);
    assert_eq!(chat.downloads.len(), 2);
    let mut files: Vec<_> = chat.downloads.iter().map(|d| d.file.as_str()).collect();
    files.sort();
    assert_eq!(
        files,
        [
            "m-Q4_K_M-00001-of-00002.gguf",
            "m-Q4_K_M-00002-of-00002.gguf"
        ]
    );

    let aux = add("aux").await;
    assert_eq!(aux.files_queued as usize, aux.downloads.len());
    for d in &aux.downloads {
        assert!(
            chat.downloads.iter().all(|c| c.id != d.id),
            "an aux answer lists the chat row {}",
            d.id
        );
        let row = store::get_hf_model(&f.state.db, d.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.target, "aux");
    }
}

#[tokio::test]
async fn the_download_list_and_the_job_list_keep_their_nulls_and_their_counts() {
    let f = fixture().await;
    tree(
        &f.hub,
        "p/chat",
        json!([
            {"type": "file", "path": "a-Q4_K_M.gguf", "size": 1},
            {"type": "file", "path": "b-Q4_K_M.gguf", "size": 1},
        ]),
    )
    .await;
    for file in ["a-Q4_K_M.gguf", "b-Q4_K_M.gguf"] {
        op(
            &f.gw,
            "hf_add",
            json!({"repo": "p/chat", "file": file, "companions": false}),
        )
        .await;
    }
    // A finished row and a failed one, with no job behind either.
    let done = store::upsert_hf_model(&f.state.db, "p/old", "d.gguf", "p/old/d.gguf", "chat")
        .await
        .unwrap();
    store::mark_hf_done(
        &f.state.db,
        done,
        Some("e1"),
        4_000_000_000,
        "main",
        Some("abc"),
    )
    .await
    .unwrap();
    let failed = store::upsert_hf_model(&f.state.db, "p/old", "f.gguf", "p/old/f.gguf", "aux")
        .await
        .unwrap();
    store::set_hf_status(&f.state.db, failed, "failed", Some("HTTP 500"))
        .await
        .unwrap();
    common::patience::until("both transfers reported", || {
        let live = f.state.jobs.live();
        live.len() == 2 && live.iter().all(|j| j.done == 500)
    })
    .await;

    let live = get(&f.gw, "/api/hf/downloads").await;
    let view = common::round_trips::<dto::DownloadsView>("GET /api/hf/downloads", &live);
    assert_keys(&live, &["downloads"]);
    assert_eq!(
        view.downloads
            .iter()
            .filter(|d| d.received_bytes.is_some())
            .count(),
        2
    );
    let rows = live["downloads"].as_array().unwrap();
    assert_eq!(rows.len(), 4);
    for r in rows {
        assert_keys(
            r,
            &[
                "id",
                "repo",
                "file",
                "dest_path",
                "target",
                "status",
                "error",
                "size",
                "downloaded_at",
                "received_bytes",
                "percent",
                "job_id",
                "requested_revision",
                "resolved_commit",
            ],
        );
    }
    let row = |file: &str| view.downloads.iter().find(|r| r.file == file).unwrap();
    // A live row: bytes and job; a percent only where the total is known.
    let a = row("a-Q4_K_M.gguf");
    let b = row("b-Q4_K_M.gguf");
    let (with_total, without) = if a.percent.is_some() { (a, b) } else { (b, a) };
    assert_eq!(with_total.percent, Some(50));
    assert_eq!(with_total.received_bytes, Some(500));
    assert!(with_total.job_id.is_some());
    assert_eq!((without.percent, without.received_bytes), (None, Some(500)));
    // A row with no job: the live columns are `null`, the keys still sent.
    let d = row("d.gguf");
    assert_eq!(d.size.as_deref(), Some("3.7 GiB"));
    assert_eq!(d.resolved_commit.as_deref(), Some("abc"));
    assert!(d.job_id.is_none() && d.received_bytes.is_none() && d.percent.is_none());
    assert_eq!(row("f.gguf").error.as_deref(), Some("HTTP 500"));

    // Jobs: the two transfers and a finished and a failed one of other kinds.
    let spawn = |kind, key: &str, input: Value| {
        let state = f.state.clone();
        let key = key.to_string();
        async move {
            jobs::spawn(
                &state,
                kind,
                Some(key.clone()),
                format!("{kind} {key}"),
                input,
            )
            .await
            .unwrap()
            .id()
        }
    };
    let finished = spawn(
        JobKind::ReEmbed,
        "c:1",
        json!({"mode": "done", "total": 5, "done": 5}),
    )
    .await;
    let broken = spawn(
        JobKind::EvalRun,
        "c:2",
        json!({"mode": "fail", "total": 5, "done": 1}),
    )
    .await;
    spawn(
        JobKind::GoldenGen,
        "c:3",
        json!({"mode": "hold", "done": 7}),
    )
    .await;
    common::patience::until("the one-shot jobs ended", || {
        f.state.jobs.live_one(finished).is_none() && f.state.jobs.live_one(broken).is_none()
    })
    .await;
    common::patience::until("the held job reported", || {
        f.state.jobs.live().iter().any(|j| j.done == 7)
    })
    .await;

    let live = get(&f.gw, "/api/jobs").await;
    let all = common::round_trips::<dto::JobsView>("GET /api/jobs", &live);
    assert_eq!(all.jobs.len(), 5);
    assert_keys(&live, &["jobs"]);
    let jobs = live["jobs"].as_array().unwrap();
    for j in jobs {
        assert_keys(
            j,
            &[
                "id",
                "kind",
                "key",
                "label",
                "status",
                "done",
                "total",
                "percent",
                "stage",
                "detail",
                "error",
                "created_at",
                "started_at",
                "finished_at",
            ],
        );
        assert!(is_whole(&j["done"]) && is_whole(&j["id"]), "{j}");
    }
    let job = |kind: &str| all.jobs.iter().find(|j| j.kind == kind).unwrap();
    // `total` and `percent` are `null` while the total is not known.
    let golden = job("golden_gen");
    assert_eq!((golden.done, golden.total, golden.percent), (7, None, None));
    let re_embed = job("re_embed");
    assert_eq!(
        (re_embed.status.as_str(), re_embed.total, re_embed.percent),
        ("done", Some(5), Some(100))
    );
    assert_eq!(job("eval_run").error.as_deref(), Some("executor said no"));
    assert!(re_embed.finished_at.is_some() && golden.finished_at.is_none());

    let live = get(&f.gw, "/api/jobs?kind=hf_download&active=true&limit=1").await;
    let some = common::round_trips::<dto::JobsView>("GET /api/jobs (filtered)", &live);
    assert_eq!(some.jobs.len(), 1);
}

#[tokio::test]
async fn a_cancel_a_redownload_and_a_delete_answer_a_message() {
    let f = fixture().await;
    tree(
        &f.hub,
        "p/chat",
        json!([{"type": "file", "path": "a-Q4_K_M.gguf", "size": 1}]),
    )
    .await;
    let added = op(
        &f.gw,
        "hf_add",
        json!({"repo": "p/chat", "file": "a-Q4_K_M.gguf", "companions": false}),
    )
    .await;
    let id = added["downloads"][0]["id"].as_i64().unwrap();
    let key = format!("hf:{id}");
    common::patience::until("the transfer reported", || {
        f.state
            .jobs
            .live_by_key(JobKind::HfDownload, &key)
            .is_some_and(|j| j.done == 500)
    })
    .await;

    // Cancel by the download row, and the generic job cancel: one message each.
    let live = op(&f.gw, "hf_set", json!({"action": "cancel", "id": id})).await;
    let ack = common::round_trips::<dto::MessageAck>("POST /api/op/hf_set (cancel)", &live);
    assert!(
        ack.ok && ack.message.starts_with("cancel requested for job"),
        "{ack:?}"
    );
    assert_keys(&live, &["ok", "message"]);
    common::patience::until("the cancelled transfer ended", || {
        f.state
            .jobs
            .live_by_key(JobKind::HfDownload, &key)
            .is_none()
    })
    .await;

    let live = op(&f.gw, "hf_set", json!({"action": "redownload", "id": id})).await;
    let ack = common::round_trips::<dto::MessageAck>("POST /api/op/hf_set (redownload)", &live);
    assert_eq!(ack.message, "re-downloading a-Q4_K_M.gguf");
    let job = f
        .state
        .jobs
        .live_by_key(JobKind::HfDownload, &key)
        .expect("the redownload runs as a job")
        .id;
    let live = op(&f.gw, "job_cancel", json!({"id": job})).await;
    let ack = common::round_trips::<dto::MessageAck>("POST /api/op/job_cancel", &live);
    assert_eq!(ack.message, format!("cancel requested for job {job}"));
    common::patience::until("the second cancel ended", || {
        f.state
            .jobs
            .live_by_key(JobKind::HfDownload, &key)
            .is_none()
    })
    .await;

    // A row a crash left running: nothing runs it, so the cancel closes it.
    let stale = store::claim_job(&f.state.db, "benchmark", Some("stale"), "stale", "{}")
        .await
        .unwrap()
        .unwrap();
    store::start_job(&f.state.db, stale).await.unwrap();
    let live = op(&f.gw, "job_cancel", json!({"id": stale})).await;
    let ack = common::round_trips::<dto::MessageAck>("POST /api/op/job_cancel (stale)", &live);
    assert_eq!(
        ack.message,
        format!("job {stale} was not running; marked canceled")
    );

    let live = op(&f.gw, "hf_set", json!({"action": "delete", "id": id})).await;
    let ack = common::round_trips::<dto::MessageAck>("POST /api/op/hf_set (delete)", &live);
    assert_eq!(ack.message, "entry untracked and file removed");

    // The refusals stay refusals.
    for (name, args) in [
        ("job_cancel", json!({"id": 424_242})),
        ("job_cancel", json!({})),
        ("hf_set", json!({"action": "cancel", "id": id})),
        ("hf_set", json!({"action": "redownload"})),
        ("hf_set", json!({"action": "explode"})),
    ] {
        let (status, body) = op_status(&f.gw, name, args.clone()).await;
        assert_eq!(status, 400, "{name} {args}: {body}");
        assert_keys(&body, &["code", "message"]);
    }
}

#[tokio::test]
async fn an_update_check_counts_what_moved_and_is_not_ok_when_a_file_could_not_be_asked() {
    let f = fixture().await;
    let done = |repo: &'static str, file: &'static str, target: &'static str| {
        let state = f.state.clone();
        async move {
            let id =
                store::upsert_hf_model(&state.db, repo, file, &format!("{repo}/{file}"), target)
                    .await
                    .unwrap();
            store::mark_hf_done(&state.db, id, Some("e1"), 1, "main", None)
                .await
                .unwrap();
        }
    };
    done("p/old", "moved.gguf", "chat").await;
    done("p/old", "same.gguf", "chat").await;
    done("p/old", "gone.gguf", "audio").await;
    done("p/old", "also-moved.gguf", "aux").await;
    for (file, answer) in [
        (
            "moved.gguf",
            ResponseTemplate::new(200).insert_header("etag", "\"e2\""),
        ),
        (
            "same.gguf",
            ResponseTemplate::new(200).insert_header("etag", "\"e1\""),
        ),
        (
            "also-moved.gguf",
            ResponseTemplate::new(200).insert_header("etag", "\"e3\""),
        ),
        ("gone.gguf", ResponseTemplate::new(500)),
    ] {
        Mock::given(method("HEAD"))
            .and(path(format!("/p/old/resolve/main/{file}")))
            .respond_with(answer)
            .mount(&f.hub)
            .await;
    }

    // One moved, one same: ok, a count, and an empty list of errors.
    let live = op(&f.gw, "hf_set", json!({"action": "check_updates"})).await;
    let checked = common::round_trips::<dto::HfUpdatesChecked>("POST /api/op/hf_set (chat)", &live);
    assert_eq!(
        checked,
        dto::HfUpdatesChecked {
            ok: true,
            updates_available: 1,
            errors: vec![],
        }
    );
    assert_keys(&live, &["ok", "updates_available", "errors"]);
    assert!(is_whole(&live["updates_available"]));
    assert_eq!(live["errors"], json!([]));

    // Nothing to ask is still ok.
    let live = op(
        &f.gw,
        "hf_set",
        json!({"action": "check_updates", "target": "image"}),
    )
    .await;
    let checked =
        common::round_trips::<dto::HfUpdatesChecked>("POST /api/op/hf_set (image)", &live);
    assert_eq!((checked.ok, checked.updates_available), (true, 0));

    // A hub that answers 500 is an entry in `errors`, and `ok` goes false;
    // the answer is still a 200.
    let live = op(
        &f.gw,
        "hf_set",
        json!({"action": "check_updates", "target": "audio"}),
    )
    .await;
    let checked =
        common::round_trips::<dto::HfUpdatesChecked>("POST /api/op/hf_set (audio)", &live);
    assert!(!checked.ok && checked.updates_available == 0);
    assert_eq!(checked.errors.len(), 1, "{checked:?}");
    assert!(checked.errors[0].contains("500"), "{checked:?}");

    // The key the update check does not send when it fails to ask is not
    // `message`: that is the other arm's.
    assert!(live.get("message").is_none(), "{live}");
}

#[tokio::test]
async fn the_image_recipe_ops_read_back_through_their_types() {
    let f = fixture().await;
    let recipe = lmgw_core::image_recipes::find("z-image-turbo").unwrap();

    // The shipped list, joined against a box with nothing on it.
    let live = op(&f.gw, "image_recipes", json!({})).await;
    let list = common::round_trips::<dto::ImageRecipes>("POST /api/op/image_recipes", &live);
    assert!(!list.models_dir_missing && !list.hf_token_set);
    assert!(list.recipes.iter().any(|r| r.key == "z-image-turbo"));
    assert!(is_whole(&live["recipes"][0]["total_bytes"]));

    // Adding one queues what is missing and hands back a row.
    for c in recipe.components {
        tree(
            &f.hub,
            c.repo,
            json!([{"type": "file", "path": c.file, "size": 18}]),
        )
        .await;
    }
    let live = op(&f.gw, "image_recipe_add", json!({"key": "z-image-turbo"})).await;
    let added =
        common::round_trips::<dto::ImageRecipeAddResult>("POST /api/op/image_recipe_add", &live);
    assert_eq!(added.key, "z-image-turbo");
    assert_eq!(added.files_queued as usize, recipe.components.len());
    assert_eq!(added.row.action, "create");
    assert!(is_whole(&live["files_queued"]));

    // The list now says what is on its way.
    let live = op(&f.gw, "image_recipes", json!({})).await;
    let list =
        common::round_trips::<dto::ImageRecipes>("POST /api/op/image_recipes (queued)", &live);
    let z = list
        .recipes
        .iter()
        .find(|r| r.key == "z-image-turbo")
        .unwrap();
    assert!(z
        .components
        .iter()
        .all(|c| c.downloading && c.download_id.is_some()));
}
