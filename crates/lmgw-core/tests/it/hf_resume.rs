//! WP1 of the find-a-model design against a mock hub and a Range-capable
//! mock CDN: the header read before a download (§2.1, D1, D4), the projector
//! rule (D121, bug H1) and a download that outlives a restart (D122, bug H2).
//!
//! The hub answers a file's resolve URL with a `302` carrying the headers
//! it sent for a public file on 2026-11-03 (`x-linked-etag`,
//! `x-linked-size`, `x-repo-commit`), and the CDN answers a `Range` with a
//! `206` and its `Content-Range`, as the xet bridge does. The names are
//! fake, the hashes are made up, and nothing here reaches the network.
//!
//! Every test sets `HF_ENDPOINT`, so each holds `common::process_env_lock`
//! for its whole body.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_core::gguf::synth;
use lmgw_core::hf;
use lmgw_core::jobs::hf_download;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const REPO: &str = "demo-org/demo-model-GGUF";
const COMMIT: &str = "82d411acf4a06cfb8d9b073a5211bf410bfc29bf";
const OID: &str = "aaaa000000000000000000000000000000000000000000000000000000000001";

/// A file the CDN serves as the xet bridge does: a `Range: bytes=N-` gets a
/// `206` from byte N, anything else the whole file. `whole_only` plays a
/// CDN that ignores the Range. Every Range asked is recorded.
#[derive(Clone)]
struct Cdn {
    bytes: Arc<Vec<u8>>,
    whole_only: bool,
    /// A `206` that names byte 0 as its start whatever was asked: another
    /// part than the rest.
    wrong_offset: bool,
    /// The body ends this many bytes before the end its headers name.
    short_by: usize,
    ranges: Arc<Mutex<Vec<Option<String>>>>,
}

impl Cdn {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Arc::new(bytes),
            whole_only: false,
            wrong_offset: false,
            short_by: 0,
            ranges: Default::default(),
        }
    }

    fn ranges(&self) -> Vec<Option<String>> {
        self.ranges.lock().unwrap().clone()
    }

    fn answer(&self, r: &Request) -> ResponseTemplate {
        assert!(
            !r.headers.contains_key("authorization"),
            "the token never reaches the CDN"
        );
        // The file as it is: a decoded answer has no length (D141).
        assert_eq!(
            r.headers
                .get("accept-encoding")
                .and_then(|v| v.to_str().ok()),
            Some("identity"),
            "a file's bytes are asked for as they are"
        );
        let range = r
            .headers
            .get("range")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        self.ranges.lock().unwrap().push(range.clone());
        let total = self.bytes.len();
        let from = range
            .as_deref()
            .and_then(|v| v.strip_prefix("bytes="))
            .and_then(|v| v.strip_suffix('-'))
            .and_then(|v| v.parse::<usize>().ok());
        let end = total - self.short_by;
        match from {
            Some(from) if !self.whole_only && from >= total => ResponseTemplate::new(416)
                .insert_header("content-range", format!("bytes */{total}").as_str()),
            Some(from) if !self.whole_only => {
                let named = if self.wrong_offset { 0 } else { from };
                ResponseTemplate::new(206)
                    .insert_header("accept-ranges", "bytes")
                    .insert_header("etag", "\"xet-demo\"")
                    .insert_header(
                        "content-range",
                        format!("bytes {named}-{}/{total}", total - 1).as_str(),
                    )
                    .set_body_bytes(self.bytes[named..end].to_vec())
            }
            _ => ResponseTemplate::new(200)
                .insert_header("etag", "\"xet-demo\"")
                .set_body_bytes(self.bytes[..end].to_vec()),
        }
    }
}

/// The hub's `302` for `file`, naming `oid` as the file's hash, to `cdn`.
async fn resolve_to(hub: &MockServer, cdn: &MockServer, file: &str, oid: &str, size: usize) {
    resolve_naming(hub, &cdn.uri(), file, Some(oid), Some(size)).await;
}

/// [`resolve_to`] to the CDN at `cdn`, with the hash and the size the hub's
/// hop names, each when given.
async fn resolve_naming(
    hub: &MockServer,
    cdn: &str,
    file: &str,
    oid: Option<&str>,
    size: Option<usize>,
) {
    let mut answer = ResponseTemplate::new(302)
        .insert_header("x-repo-commit", COMMIT)
        .insert_header("accept-ranges", "bytes")
        .insert_header(
            "location",
            format!("{cdn}/xet-bridge-us/demo/{file}?Expires=1").as_str(),
        );
    if let Some(oid) = oid {
        answer = answer.insert_header("x-linked-etag", format!("\"{oid}\"").as_str());
    }
    if let Some(size) = size {
        answer = answer.insert_header("x-linked-size", size.to_string().as_str());
    }
    Mock::given(method("GET"))
        .and(path(format!("/{REPO}/resolve/main/{file}")))
        .and(|r: &Request| !r.headers.contains_key("range"))
        .respond_with(answer)
        .mount(hub)
        .await;
}

/// The sha256 of `bytes` as the hub names a file's: 64 lowercase hex digits.
fn sha(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// The CDN's answers for `file`.
async fn serve(cdn_server: &MockServer, file: &str, cdn: &Cdn) {
    let c = cdn.clone();
    Mock::given(method("GET"))
        .and(path(format!("/xet-bridge-us/demo/{file}")))
        .respond_with(move |r: &Request| c.answer(r))
        .mount(cdn_server)
        .await;
}

/// A state whose chat models dir is `dir`.
async fn state_in(dir: &std::path::Path) -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.router.models_dir = dir.display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    state
}

/// Bytes enough to cross several chunks, each position its own value.
fn file_bytes(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

/// Wait for a tracked row to reach `done` or `failed`, and return it.
async fn settled(state: &SharedState, id: i64) -> store::HfModelRow {
    for _ in 0..500 {
        let row = store::get_hf_model(&state.db, id).await.unwrap().unwrap();
        if (row.status == "done" || row.status == "failed") && state.jobs.live().is_empty() {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("download {id} never settled");
}

/// The last transfer's answer for a row.
async fn last_result(state: &SharedState, id: i64) -> Value {
    let job = store::latest_job_by_key(&state.db, "hf_download", &hf_download::job_key(id))
        .await
        .unwrap()
        .expect("a transfer ran");
    serde_json::from_str(job.result.as_deref().unwrap_or("null")).unwrap()
}

/// A row as a shutdown leaves it: `downloading`, `oid` recorded as the hash
/// it began on, the first `cut` bytes in its `.part`, and its job row still
/// `running`.
async fn cut_download(
    state: &SharedState,
    dir: &std::path::Path,
    bytes: &[u8],
    cut: usize,
    oid: &str,
) -> i64 {
    let file = "demo-model-Q4_K_M.gguf";
    let id = store::upsert_hf_model(&state.db, REPO, file, &format!("{REPO}/{file}"), "chat")
        .await
        .unwrap();
    store::set_hf_status(&state.db, id, "downloading", None)
        .await
        .unwrap();
    store::set_hf_oid(&state.db, id, Some(oid)).await.unwrap();
    let part = dir.join(format!("{REPO}/{file}.part"));
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, &bytes[..cut]).unwrap();
    let job = store::claim_job(
        &state.db,
        "hf_download",
        Some(&hf_download::job_key(id)),
        &format!("{REPO}/{file}"),
        &json!({ "hf_model_id": id }).to_string(),
    )
    .await
    .unwrap()
    .unwrap();
    store::start_job(&state.db, job).await.unwrap();
    id
}

// ---------------------------------------------------------------------------
// The header read (§2.1, D1, D4)
// ---------------------------------------------------------------------------

/// The header is read with `Range: bytes=0-` on the CDN's hop and the
/// token on the hub's only; the stream gives the planner's summary, the
/// file's length from `Content-Range`, and the hash the hub named. A second
/// ask by the listing's `lfs.oid` costs no request.
#[tokio::test]
async fn a_header_read_takes_a_range_from_the_cdn_and_a_cache_hit_costs_no_request() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());

    let mut h = synth::chat("demoarch", 32_768);
    h.tensor_shaped("token_embd.weight", &[64, 256], 64 * 256 * 2)
        .tensor_shaped("blk.0.attn_q.weight", &[64, 64], 64 * 64)
        // Tensor data well past the header, which must not be needed.
        .tensor_shaped("blk.0.ffn_down.weight", &[512, 512], 512 * 512);
    let bytes = h.bytes();
    let file = "demo-model-Q8_0.gguf";
    let cdn = Cdn::new(bytes.clone());
    // The hub's hop: once, with the token.
    Mock::given(method("GET"))
        .and(path(format!("/{REPO}/resolve/main/{file}")))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer hf-demo-token",
        ))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("x-repo-commit", COMMIT)
                .insert_header("x-linked-etag", format!("\"{OID}\"").as_str())
                .insert_header("x-linked-size", bytes.len().to_string().as_str())
                .insert_header(
                    "location",
                    format!("{}/xet-bridge-us/demo/{file}?Expires=1", cdn_server.uri()).as_str(),
                ),
        )
        .expect(1)
        .mount(&hub)
        .await;
    serve(&cdn_server, file, &cdn).await;

    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    let mut settings = state.snapshot().settings.clone();
    settings.hf_token = "hf-demo-token".into();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let listed: Vec<hf::HfFile> = serde_json::from_value(json!([{
        "type": "file", "path": file, "size": bytes.len(),
        "lfs": { "oid": OID, "size": bytes.len(), "pointerSize": 134 }
    }]))
    .unwrap();
    assert_eq!(
        listed[0].oid(),
        Some(OID),
        "the listing's lfs.oid is parsed"
    );

    let first = hf::remote_header(&state, REPO, &listed[0]).await.unwrap();
    assert_eq!(first.total, bytes.len() as u64, "from Content-Range");
    assert_eq!(first.oid.as_deref(), Some(OID));
    assert_eq!(first.commit.as_deref(), Some(COMMIT));
    assert_eq!(first.summary.architecture.as_deref(), Some("demoarch"));
    assert_eq!(first.summary.context_length, Some(32_768));
    assert_eq!(
        first.summary.parameter_count,
        Some(64 * 256 + 64 * 64 + 512 * 512)
    );
    assert_eq!(first.summary.file_size, bytes.len() as u64);
    assert!(first.header_bytes < bytes.len() as u64 - 512 * 512);
    assert_eq!(cdn.ranges(), vec![Some("bytes=0-".to_string())]);

    let again = hf::remote_header(&state, REPO, &listed[0]).await.unwrap();
    assert!(Arc::ptr_eq(&first, &again), "the cached summary");
    assert_eq!(state.hf_headers.reads(), 1);
    assert_eq!(cdn.ranges().len(), 1, "a hit makes no request");
    hub.verify().await;
}

/// A refused read says why, as a download does: the gated sentence for a
/// 401, and a cut stream as the reader's end of file.
#[tokio::test]
async fn a_refused_or_cut_header_read_says_why() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;

    Mock::given(method("GET"))
        .and(path(format!("/{REPO}/resolve/main/gated.gguf")))
        .respond_with(ResponseTemplate::new(401).set_body_string("Invalid credentials"))
        .mount(&hub)
        .await;
    let gated = hf::HfFile {
        path: "gated.gguf".into(),
        kind: "file".into(),
        ..Default::default()
    };
    let err = hf::remote_header(&state, REPO, &gated).await.unwrap_err();
    assert!(err.contains("gated repo"), "{err}");

    // The CDN says the file is long, and sends only its first 40 bytes.
    let bytes = synth::chat("demoarch", 8).bytes();
    Mock::given(method("GET"))
        .and(path(format!("/{REPO}/resolve/main/cut.gguf")))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "location",
            format!("{}/xet-bridge-us/demo/cut.gguf", cdn_server.uri()).as_str(),
        ))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/xet-bridge-us/demo/cut.gguf"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 0-39/16817244384")
                .set_body_bytes(bytes[..40].to_vec()),
        )
        .mount(&cdn_server)
        .await;
    let cut = hf::HfFile {
        path: "cut.gguf".into(),
        kind: "file".into(),
        ..Default::default()
    };
    let err = hf::remote_header(&state, REPO, &cut).await.unwrap_err();
    assert!(
        err.contains("the header of cut.gguf on Hugging Face") && err.contains("end of file"),
        "{err}"
    );
    assert_eq!(state.hf_headers.reads(), 0, "nothing was read whole");
}

// ---------------------------------------------------------------------------
// The projector rule (D121, bug H1)
// ---------------------------------------------------------------------------

/// A projector header of `bytes` bytes.
fn projector(bytes: u64) -> Vec<u8> {
    let mut h = synth::Header::default();
    h.str("general.architecture", "clip")
        .str("general.type", "mmproj")
        .str("clip.projector_type", "demo")
        .tensor_sized("v.blk.0.attn_q.weight", bytes);
    h.bytes()
}

/// With BF16, F16 and F32 projectors beside the weights, `hf_add` queues
/// the weights and the F16 one (the smallest 16-bit one) and nothing else,
/// and the disk plan of the same directory takes the same file.
#[tokio::test]
async fn one_projector_is_fetched_and_planned_of_three() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    // Sizes as the hub lists them for a public repo's three projectors.
    Mock::given(method("GET"))
        .and(path(format!("/api/models/{REPO}/tree/main")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "type": "file", "path": "mmproj-BF16.gguf", "size": 931_146_304u64 },
            { "type": "file", "path": "mmproj-F16.gguf", "size": 927_607_360u64 },
            { "type": "file", "path": "mmproj-F32.gguf", "size": 1_842_940_480u64 },
            { "type": "file", "path": "demo-model-Q4_K_M.gguf", "size": 16_817_244_384u64 },
            { "type": "file", "path": "demo-model-Q8_0.gguf", "size": 28_600_000_000u64 },
            { "type": "file", "path": "README.md", "size": 10 },
        ])))
        .mount(&hub)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;

    let added = lmgw_core::ops::hf_add(
        &state,
        REPO,
        Some("demo-model-Q4_K_M.gguf"),
        None,
        "chat",
        true,
    )
    .await
    .unwrap();
    let companions: Vec<(&str, &str)> = added
        .companions
        .iter()
        .map(|c| (c.file.as_str(), c.role.as_str()))
        .collect();
    assert_eq!(companions, vec![("mmproj-F16.gguf", "mmproj")]);
    let queued: Vec<&str> = added.downloads.iter().map(|d| d.file.as_str()).collect();
    assert_eq!(queued, vec!["demo-model-Q4_K_M.gguf", "mmproj-F16.gguf"]);
    // The transfers meet a hub with no files: let them end before the disk.
    for d in &added.downloads {
        settled(&state, d.id).await;
    }

    // The same directory on disk: the plan names the F16 projector, though
    // BF16 sorts first.
    let at = dir.path().join(REPO);
    synth::chat("demoarch", 4096).write_to(&at.join("demo-model-Q4_K_M.gguf"));
    std::fs::write(at.join("mmproj-BF16.gguf"), projector(4096 + 32)).unwrap();
    std::fs::write(at.join("mmproj-F16.gguf"), projector(4096)).unwrap();
    std::fs::write(at.join("mmproj-F32.gguf"), projector(8192)).unwrap();
    let plan = lmgw_core::modelinfo::local_model_plan(
        &state,
        &format!("{REPO}/demo-model-Q4_K_M.gguf"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        plan["params"]["mmproj_path"],
        json!(format!("{REPO}/mmproj-F16.gguf")),
        "{plan:#}"
    );
}

// ---------------------------------------------------------------------------
// A download across a restart (D122, bug H2)
// ---------------------------------------------------------------------------

/// The process stopped mid-download; a new one on the same database closes
/// the orphaned job and queues the row again, and its transfer asks the CDN
/// for the rest of the `.part` and ends with the bytes a straight download
/// has.
#[tokio::test]
async fn a_cut_download_goes_on_from_its_part_after_a_restart() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let bytes = file_bytes(300_000);
    let oid = sha(&bytes);
    let cdn = Cdn::new(bytes.clone());
    let file = "demo-model-Q4_K_M.gguf";
    resolve_to(&hub, &cdn_server, file, &oid, bytes.len()).await;
    serve(&cdn_server, file, &cdn).await;

    let dir = tempfile::tempdir().unwrap();
    let before = state_in(dir.path()).await;
    let id = cut_download(&before, dir.path(), &bytes, 100_000, &oid).await;

    // A new process on the same database: the boot's order.
    let after = AppState::init_for_tests_on(before.db.clone())
        .await
        .unwrap();
    assert_eq!(store::fail_orphaned_jobs(&after.db).await.unwrap(), 1);
    let boot = hf_download::resume_at_boot(&after).await;
    assert_eq!(boot.started, vec![id]);
    assert!(boot.paused.is_empty());
    let row = settled(&after, id).await;
    assert_eq!(row.status, "done", "error: {:?}", row.error);
    assert_eq!(cdn.ranges(), vec![Some("bytes=100000-".to_string())]);
    let dest = dir.path().join(format!("{REPO}/{file}"));
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    assert!(!dest.with_extension("gguf.part").exists());
    assert_eq!(row.size_bytes, Some(bytes.len() as i64));
    assert_eq!(row.oid.as_deref(), Some(oid.as_str()));
    assert_eq!(row.resolved_commit.as_deref(), Some(COMMIT));
    let result = last_result(&after, id).await;
    assert_eq!(result["resumed_from"], 100_000);
    assert_eq!(result["restarted"], Value::Null);
}

/// The hub names another hash than the one the `.part` began on: the file
/// changed, so the bytes kept are dropped, a second request without a Range
/// starts from byte 0, the new hash is recorded, and the answer says so.
#[tokio::test]
async fn a_changed_hash_starts_the_download_again_and_says_so() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let bytes = file_bytes(200_000);
    let new_oid = sha(&bytes);
    let cdn = Cdn::new(bytes.clone());
    let file = "demo-model-Q4_K_M.gguf";
    resolve_to(&hub, &cdn_server, file, &new_oid, bytes.len()).await;
    serve(&cdn_server, file, &cdn).await;

    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    // The `.part` holds another file's bytes, under that file's hash.
    let id = cut_download(&state, dir.path(), &[7u8; 50_000], 50_000, OID).await;
    store::fail_orphaned_jobs(&state.db).await.unwrap();
    hf_download::resume_at_boot(&state).await;
    let row = settled(&state, id).await;
    assert_eq!(row.status, "done", "error: {:?}", row.error);
    assert_eq!(
        cdn.ranges(),
        vec![Some("bytes=50000-".to_string()), None],
        "asked for the rest, then the whole file"
    );
    assert_eq!(
        std::fs::read(dir.path().join(format!("{REPO}/{file}"))).unwrap(),
        bytes
    );
    assert_eq!(row.oid.as_deref(), Some(new_oid.as_str()));
    let result = last_result(&state, id).await;
    assert_eq!(result["resumed_from"], Value::Null);
    let said = result["restarted"].as_str().unwrap();
    assert!(said.contains("the file changed on Hugging Face"), "{said}");
}

/// A CDN that answers the Range with the whole file (`200`): the bytes kept
/// are dropped and the `200` is written from byte 0, with no second
/// request.
#[tokio::test]
async fn a_cdn_answering_200_to_the_range_starts_again() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let bytes = file_bytes(150_000);
    let cdn = Cdn {
        whole_only: true,
        ..Cdn::new(bytes.clone())
    };
    let file = "demo-model-Q4_K_M.gguf";
    resolve_to(&hub, &cdn_server, file, &sha(&bytes), bytes.len()).await;
    serve(&cdn_server, file, &cdn).await;

    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    let id = cut_download(&state, dir.path(), &bytes, 60_000, &sha(&bytes)).await;
    store::fail_orphaned_jobs(&state.db).await.unwrap();
    hf_download::resume_at_boot(&state).await;
    let row = settled(&state, id).await;
    assert_eq!(row.status, "done", "error: {:?}", row.error);
    assert_eq!(cdn.ranges(), vec![Some("bytes=60000-".to_string())]);
    assert_eq!(
        std::fs::read(dir.path().join(format!("{REPO}/{file}"))).unwrap(),
        bytes
    );
    let said = last_result(&state, id).await["restarted"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(said.contains("from its beginning"), "{said}");
}

/// A stated machine whose default route leaves through `eth0`, which
/// NetworkManager calls metered.
fn metered_machine() -> Arc<lmgw_machine::fake::FakeFacts> {
    let f = lmgw_machine::fake::FakeFacts::new();
    f.file(
        "/proc/net/route",
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
         eth0\t00000000\t010200C0\t0003\t0\t0\t425\t00000000\t0\t0\t0\n",
    );
    f.program("/usr/bin/nmcli", |_| {
        Ok(lmgw_machine::CmdOut::success(
            "GENERAL.METERED:yes (guessed)\n",
        ))
    });
    Arc::new(f)
}

/// On a metered connection the boot goes on with nothing: the row stays
/// queued with the paused sentence, no request reaches the hub and the
/// `.part` stays. "Resume" (`hf_set redownload`) then goes on from it.
#[tokio::test]
async fn on_a_metered_connection_a_cut_download_waits_for_resume() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let bytes = file_bytes(120_000);
    let cdn = Cdn::new(bytes.clone());
    let file = "demo-model-Q4_K_M.gguf";
    resolve_to(&hub, &cdn_server, file, &sha(&bytes), bytes.len()).await;
    serve(&cdn_server, file, &cdn).await;

    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    state.machine.set_machine(
        metered_machine(),
        Arc::new(|| None::<Result<Vec<lmgw_core::vram::GpuMemory>, String>>),
    );
    let id = cut_download(&state, dir.path(), &bytes, 40_000, &sha(&bytes)).await;
    store::fail_orphaned_jobs(&state.db).await.unwrap();

    let boot = hf_download::resume_at_boot(&state).await;
    assert_eq!(boot.paused, vec![id]);
    assert!(boot.started.is_empty());
    assert_eq!(boot.metered.as_deref(), Some("eth0: yes (guessed)"));
    let row = store::get_hf_model(&state.db, id).await.unwrap().unwrap();
    assert_eq!(row.status, "queued");
    assert_eq!(row.error.as_deref(), Some(hf_download::PAUSED_METERED_NOTE));
    assert!(hub.received_requests().await.unwrap().is_empty());
    assert!(state.jobs.live().is_empty());
    assert!(dir.path().join(format!("{REPO}/{file}.part")).exists());

    // Resume, as the Downloads page's button asks.
    lmgw_core::ops::hf_set(&state, "redownload", Some(id), "chat")
        .await
        .unwrap();
    let row = settled(&state, id).await;
    assert_eq!(row.status, "done", "error: {:?}", row.error);
    assert_eq!(row.error, None);
    assert_eq!(cdn.ranges(), vec![Some("bytes=40000-".to_string())]);
    assert_eq!(
        std::fs::read(dir.path().join(format!("{REPO}/{file}"))).unwrap(),
        bytes
    );
}

/// Cancel still deletes the `.part`, and forgets its hash: a cancelled
/// download is one the owner stopped, not one to go on with.
#[tokio::test]
async fn cancel_deletes_the_part_and_its_hash() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let bytes = file_bytes(100_000);
    let file = "demo-model-Q4_K_M.gguf";
    resolve_to(&hub, &cdn_server, file, &sha(&bytes), bytes.len()).await;
    // A CDN slow to answer, so the cancel lands before the first chunk.
    let tail = bytes[30_000..].to_vec();
    Mock::given(method("GET"))
        .and(path(format!("/xet-bridge-us/demo/{file}")))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 30000-99999/100000")
                .set_body_bytes(tail)
                .set_delay(Duration::from_millis(500)),
        )
        .mount(&cdn_server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    let id = cut_download(&state, dir.path(), &bytes, 30_000, &sha(&bytes)).await;
    store::fail_orphaned_jobs(&state.db).await.unwrap();
    hf_download::resume_at_boot(&state).await;
    lmgw_core::ops::hf_set(&state, "cancel", Some(id), "chat")
        .await
        .unwrap();
    let row = settled(&state, id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error.as_deref(), Some(hf_download::CANCELED_NOTE));
    assert_eq!(row.oid, None);
    assert!(!dir.path().join(format!("{REPO}/{file}.part")).exists());
}

// ---------------------------------------------------------------------------
// What is renamed into place (D128, D130, D132, D48)
// ---------------------------------------------------------------------------

/// What a stop between recording a new hash and emptying the `.part` left
/// before D128: the new file's hash over the old file's first bytes. The
/// rest of the new file appended to them has the right length and the
/// wrong sha256, so it is deleted, its hash forgotten, and the transfer
/// fails saying so; Retry then downloads it whole.
#[tokio::test]
async fn an_old_part_under_a_new_hash_fails_its_sha256_and_retry_starts_again() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let bytes = file_bytes(200_000);
    let oid = sha(&bytes);
    let cdn = Cdn::new(bytes.clone());
    let file = "demo-model-Q4_K_M.gguf";
    resolve_to(&hub, &cdn_server, file, &oid, bytes.len()).await;
    serve(&cdn_server, file, &cdn).await;

    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    let old = vec![9u8; 80_000];
    let id = cut_download(&state, dir.path(), &old, 80_000, &oid).await;
    store::fail_orphaned_jobs(&state.db).await.unwrap();
    hf_download::resume_at_boot(&state).await;
    let row = settled(&state, id).await;
    assert_eq!(row.status, "failed");
    let said = row.error.unwrap_or_default();
    assert!(said.contains("doesn't match the sha256"), "{said}");
    assert_eq!(row.oid, None, "the hash is forgotten");
    let part = dir.path().join(format!("{REPO}/{file}.part"));
    assert!(!part.exists(), "the mixed bytes are deleted");
    let dest = dir.path().join(format!("{REPO}/{file}"));
    assert!(!dest.exists(), "nothing was renamed into place");

    lmgw_core::ops::hf_set(&state, "redownload", Some(id), "chat")
        .await
        .unwrap();
    let row = settled(&state, id).await;
    assert_eq!(row.status, "done", "error: {:?}", row.error);
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    assert_eq!(
        cdn.ranges(),
        vec![Some("bytes=80000-".to_string()), None],
        "the Retry asked for the whole file"
    );
}

/// The hash is recorded once the `.part` is emptied for the new bytes, not
/// before (D128): a `.part` that can't be opened fails the transfer and
/// leaves no hash behind for a later one to continue against.
#[tokio::test]
async fn no_hash_is_recorded_for_a_part_that_couldnt_be_emptied() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let bytes = file_bytes(50_000);
    let file = "demo-model-Q4_K_M.gguf";
    resolve_to(&hub, &cdn_server, file, &sha(&bytes), bytes.len()).await;
    serve(&cdn_server, file, &Cdn::new(bytes.clone())).await;

    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    let id = store::upsert_hf_model(&state.db, REPO, file, &format!("{REPO}/{file}"), "chat")
        .await
        .unwrap();
    // Where the `.part` goes, a directory: `File::create` fails.
    std::fs::create_dir_all(dir.path().join(format!("{REPO}/{file}.part"))).unwrap();
    let row = store::get_hf_model(&state.db, id).await.unwrap().unwrap();
    hf_download::start(&state, &row).await.unwrap();
    let row = settled(&state, id).await;
    assert_eq!(row.status, "failed");
    assert!(
        row.error.as_deref().unwrap_or("").contains("opening"),
        "{:?}",
        row.error
    );
    assert_eq!(row.oid, None);
}

/// A fresh download whose bytes don't hash to the sha256 the hub names is
/// deleted, not renamed into place.
#[tokio::test]
async fn a_fresh_download_that_doesnt_hash_to_the_hubs_sha256_is_deleted() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let bytes = file_bytes(40_000);
    let file = "demo-model-Q4_K_M.gguf";
    resolve_to(&hub, &cdn_server, file, &sha(b"another file"), bytes.len()).await;
    serve(&cdn_server, file, &Cdn::new(bytes)).await;

    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    let id = store::upsert_hf_model(&state.db, REPO, file, &format!("{REPO}/{file}"), "chat")
        .await
        .unwrap();
    let row = store::get_hf_model(&state.db, id).await.unwrap().unwrap();
    hf_download::start(&state, &row).await.unwrap();
    let row = settled(&state, id).await;
    assert_eq!(row.status, "failed");
    assert!(row
        .error
        .as_deref()
        .unwrap_or("")
        .contains("doesn't match the sha256"));
    assert!(!dir.path().join(format!("{REPO}/{file}")).exists());
    assert!(!dir.path().join(format!("{REPO}/{file}.part")).exists());
}

/// A CDN that answers every request with `bytes`, chunked and without a
/// `Content-Length`, as a proxy that re-chunks does. Its address.
async fn chunked_cdn(bytes: Vec<u8>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let bytes = Arc::new(bytes);
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let bytes = bytes.clone();
            tokio::spawn(async move {
                let (mut req, mut buf) = (Vec::new(), [0u8; 1024]);
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let mut out =
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                        .to_vec();
                for c in bytes.chunks(16 * 1024) {
                    out.extend_from_slice(format!("{:x}\r\n", c.len()).as_bytes());
                    out.extend_from_slice(c);
                    out.extend_from_slice(b"\r\n");
                }
                out.extend_from_slice(b"0\r\n\r\n");
                let _ = sock.write_all(&out).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

/// A download without a `Content-Length` takes the hub's `x-linked-size`
/// as its length (D130): the whole body is done, a short one fails as cut
/// and keeps its `.part` and its hash for Retry.
#[tokio::test]
async fn without_a_content_length_the_hubs_size_is_the_length() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let bytes = file_bytes(100_000);
    let whole = chunked_cdn(bytes.clone()).await;
    let short = chunked_cdn(bytes[..70_000].to_vec()).await;
    resolve_naming(
        &hub,
        &whole,
        "whole.gguf",
        Some(&sha(&bytes)),
        Some(bytes.len()),
    )
    .await;
    resolve_naming(
        &hub,
        &short,
        "short.gguf",
        Some(&sha(&bytes)),
        Some(bytes.len()),
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    let mut ids = Vec::new();
    for file in ["whole.gguf", "short.gguf"] {
        let id = store::upsert_hf_model(&state.db, REPO, file, &format!("{REPO}/{file}"), "chat")
            .await
            .unwrap();
        let row = store::get_hf_model(&state.db, id).await.unwrap().unwrap();
        hf_download::start(&state, &row).await.unwrap();
        ids.push(id);
    }
    let done = settled(&state, ids[0]).await;
    assert_eq!(done.status, "done", "error: {:?}", done.error);
    assert_eq!(
        std::fs::read(dir.path().join(format!("{REPO}/whole.gguf"))).unwrap(),
        bytes
    );
    let cut = settled(&state, ids[1]).await;
    assert_eq!(cut.status, "failed");
    let said = cut.error.unwrap_or_default();
    assert!(
        said.contains("ended at") && said.contains("Retry goes on"),
        "{said}"
    );
    assert_eq!(cut.oid, Some(sha(&bytes)), "kept for Retry");
    let part = dir.path().join(format!("{REPO}/short.gguf.part"));
    assert_eq!(std::fs::metadata(&part).unwrap().len(), 70_000);

    // The Downloads page sees what is kept.
    let view = lmgw_core::ops::hf_downloads(&state, Some(&ids), false)
        .await
        .unwrap();
    let kept: Vec<Option<u64>> = view.downloads.iter().map(|d| d.kept_bytes).collect();
    assert!(
        kept.contains(&Some(70_000)) && kept.contains(&None),
        "{kept:?}"
    );
}

/// A body short of the length its headers name fails as cut and keeps its
/// `.part` (D48); Retry goes on from where it ended.
#[tokio::test]
async fn a_short_body_fails_and_retry_goes_on_from_where_it_ended() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let bytes = file_bytes(120_000);
    let file = "demo-model-Q4_K_M.gguf";
    resolve_to(&hub, &cdn_server, file, &sha(&bytes), bytes.len()).await;
    let short = Cdn {
        short_by: 20_000,
        ..Cdn::new(bytes.clone())
    };
    // The first answer is cut; every later one is whole.
    let s = short.clone();
    Mock::given(method("GET"))
        .and(path(format!("/xet-bridge-us/demo/{file}")))
        .respond_with(move |r: &Request| s.answer(r))
        .up_to_n_times(1)
        .mount(&cdn_server)
        .await;
    let whole = Cdn::new(bytes.clone());
    serve(&cdn_server, file, &whole).await;

    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    let id = cut_download(&state, dir.path(), &bytes, 30_000, &sha(&bytes)).await;
    store::fail_orphaned_jobs(&state.db).await.unwrap();
    hf_download::resume_at_boot(&state).await;
    let row = settled(&state, id).await;
    assert_eq!(row.status, "failed");
    assert!(
        row.error.as_deref().unwrap_or("").contains("ended at"),
        "{:?}",
        row.error
    );
    let part = dir.path().join(format!("{REPO}/{file}.part"));
    assert_eq!(std::fs::metadata(&part).unwrap().len(), 100_000);

    lmgw_core::ops::hf_set(&state, "redownload", Some(id), "chat")
        .await
        .unwrap();
    let row = settled(&state, id).await;
    assert_eq!(row.status, "done", "error: {:?}", row.error);
    assert_eq!(whole.ranges(), vec![Some("bytes=100000-".to_string())]);
    assert_eq!(
        std::fs::read(dir.path().join(format!("{REPO}/{file}"))).unwrap(),
        bytes
    );
}

/// A `206` that starts somewhere else than the byte asked for is dropped:
/// the file is asked for again without a Range, and the answer says why.
#[tokio::test]
async fn a_206_from_another_offset_starts_again() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let bytes = file_bytes(90_000);
    let cdn = Cdn {
        wrong_offset: true,
        ..Cdn::new(bytes.clone())
    };
    let file = "demo-model-Q4_K_M.gguf";
    resolve_to(&hub, &cdn_server, file, &sha(&bytes), bytes.len()).await;
    serve(&cdn_server, file, &cdn).await;

    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    let id = cut_download(&state, dir.path(), &bytes, 45_000, &sha(&bytes)).await;
    store::fail_orphaned_jobs(&state.db).await.unwrap();
    hf_download::resume_at_boot(&state).await;
    let row = settled(&state, id).await;
    assert_eq!(row.status, "done", "error: {:?}", row.error);
    assert_eq!(cdn.ranges(), vec![Some("bytes=45000-".to_string()), None]);
    assert_eq!(
        std::fs::read(dir.path().join(format!("{REPO}/{file}"))).unwrap(),
        bytes
    );
    let said = last_result(&state, id).await["restarted"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(said.contains("another part of it"), "{said}");
}

/// A `.part` that is the whole file already: the CDN's `416` names its
/// length, the hub its hash, and the `.part` is checked and renamed into
/// place with nothing downloaded again (D132). One of the same length with
/// other bytes fails its sha256 and is deleted.
#[tokio::test]
async fn a_416_over_a_whole_part_finishes_it_and_checks_its_hash() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let bytes = file_bytes(64_000);
    let file = "demo-model-Q4_K_M.gguf";
    let cdn = Cdn::new(bytes.clone());
    resolve_to(&hub, &cdn_server, file, &sha(&bytes), bytes.len()).await;
    serve(&cdn_server, file, &cdn).await;

    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    let id = cut_download(&state, dir.path(), &bytes, bytes.len(), &sha(&bytes)).await;
    store::fail_orphaned_jobs(&state.db).await.unwrap();
    hf_download::resume_at_boot(&state).await;
    let row = settled(&state, id).await;
    assert_eq!(row.status, "done", "error: {:?}", row.error);
    assert_eq!(
        cdn.ranges(),
        vec![Some("bytes=64000-".to_string())],
        "one ask"
    );
    assert_eq!(
        std::fs::read(dir.path().join(format!("{REPO}/{file}"))).unwrap(),
        bytes
    );
    assert_eq!(last_result(&state, id).await["resumed_from"], 64_000);

    // The same length, other bytes.
    let other = "demo-model-Q8_0.gguf";
    let cdn2 = Cdn::new(bytes.clone());
    resolve_to(&hub, &cdn_server, other, &sha(&bytes), bytes.len()).await;
    serve(&cdn_server, other, &cdn2).await;
    let id2 = store::upsert_hf_model(&state.db, REPO, other, &format!("{REPO}/{other}"), "chat")
        .await
        .unwrap();
    store::set_hf_oid(&state.db, id2, Some(&sha(&bytes)))
        .await
        .unwrap();
    let part = dir.path().join(format!("{REPO}/{other}.part"));
    std::fs::write(&part, vec![1u8; bytes.len()]).unwrap();
    let row = store::get_hf_model(&state.db, id2).await.unwrap().unwrap();
    hf_download::start(&state, &row).await.unwrap();
    let row = settled(&state, id2).await;
    assert_eq!(row.status, "failed");
    assert!(row
        .error
        .as_deref()
        .unwrap_or("")
        .contains("doesn't match the sha256"));
    assert!(!part.exists());
    assert_eq!(row.oid, None);
}

/// A hub hop that names no hash while the row has one: the bytes kept
/// can't be told to be the same file's, so it starts again and says why.
#[tokio::test]
async fn a_hop_without_a_hash_starts_a_kept_part_again() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let bytes = file_bytes(70_000);
    let file = "demo-model-Q4_K_M.gguf";
    let cdn = Cdn::new(bytes.clone());
    resolve_naming(&hub, &cdn_server.uri(), file, None, Some(bytes.len())).await;
    serve(&cdn_server, file, &cdn).await;

    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    let id = cut_download(&state, dir.path(), &bytes, 20_000, &sha(&bytes)).await;
    store::fail_orphaned_jobs(&state.db).await.unwrap();
    hf_download::resume_at_boot(&state).await;
    let row = settled(&state, id).await;
    assert_eq!(row.status, "done", "error: {:?}", row.error);
    assert_eq!(cdn.ranges(), vec![Some("bytes=20000-".to_string()), None]);
    assert_eq!(row.oid, None, "the hub named none for the new bytes");
    let said = last_result(&state, id).await["restarted"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(said.contains("didn't say which file"), "{said}");
}

// ---------------------------------------------------------------------------
// A hostile header (D129)
// ---------------------------------------------------------------------------

/// A header that declares a 19 GB string under a 20 GB total: the read
/// stops at the declaration, past the memory this machine states as
/// available, with a sentence that names both and the bytes received.
#[tokio::test]
async fn a_header_declaring_more_than_the_memory_available_stops_and_says_so() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let mut body = b"GGUF".to_vec();
    body.extend_from_slice(&3u32.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&1u64.to_le_bytes());
    body.extend_from_slice(&12u64.to_le_bytes());
    body.extend_from_slice(b"general.name");
    body.extend_from_slice(&8u32.to_le_bytes()); // a string
    body.extend_from_slice(&19_000_000_000u64.to_le_bytes());
    body.extend(std::iter::repeat_n(b'a', 300_000));
    let file = "hostile.gguf";
    resolve_naming(&hub, &cdn_server.uri(), file, Some(OID), None).await;
    Mock::given(method("GET"))
        .and(path(format!("/xet-bridge-us/demo/{file}")))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 0-19999999999/20000000000")
                .set_body_bytes(body),
        )
        .mount(&cdn_server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    let machine = lmgw_machine::fake::FakeFacts::new();
    machine.file(
        "/proc/meminfo",
        "MemTotal:        8000000 kB\nMemAvailable:     2097152 kB\n",
    );
    state.machine.set_machine(
        Arc::new(machine),
        Arc::new(|| None::<Result<Vec<lmgw_core::vram::GpuMemory>, String>>),
    );
    let listed = hf::HfFile {
        path: file.into(),
        kind: "file".into(),
        ..Default::default()
    };
    let err = hf::remote_header(&state, REPO, &listed).await.unwrap_err();
    assert!(
        err.contains("declares 17.7 GiB of metadata")
            && err.contains("more than the 2.0 GiB of memory this computer has available")
            && err.contains("stopped reading after"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// The metered pause going on by itself, and its Cancel (D133)
// ---------------------------------------------------------------------------

/// A stated machine whose `eth0` NetworkManager calls metered while
/// `metered` is up, and how many times nmcli was asked.
fn switchable_machine(
    metered: Arc<std::sync::atomic::AtomicBool>,
    asked: Arc<std::sync::atomic::AtomicUsize>,
) -> Arc<lmgw_machine::fake::FakeFacts> {
    use std::sync::atomic::Ordering;
    let f = lmgw_machine::fake::FakeFacts::new();
    f.file(
        "/proc/net/route",
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
         eth0\t00000000\t010200C0\t0003\t0\t0\t425\t00000000\t0\t0\t0\n",
    );
    f.program("/usr/bin/nmcli", move |_| {
        asked.fetch_add(1, Ordering::SeqCst);
        let said = match metered.load(Ordering::SeqCst) {
            true => "yes (guessed)",
            false => "no (guessed)",
        };
        Ok(lmgw_machine::CmdOut::success(format!(
            "GENERAL.METERED:{said}\n"
        )))
    });
    Arc::new(f)
}

/// The boot pauses on a metered connection and watches NetworkManager; a
/// change that leaves it metered asks again and leaves the row waiting, one
/// that ends it lets the row go on from its `.part` by itself.
#[tokio::test]
async fn a_paused_download_goes_on_once_the_connection_isnt_metered() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let bytes = file_bytes(80_000);
    let cdn = Cdn::new(bytes.clone());
    let file = "demo-model-Q4_K_M.gguf";
    resolve_to(&hub, &cdn_server, file, &sha(&bytes), bytes.len()).await;
    serve(&cdn_server, file, &cdn).await;

    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    let (metered, asked) = (
        Arc::new(AtomicBool::new(true)),
        Arc::new(AtomicUsize::new(0)),
    );
    state.machine.set_machine(
        switchable_machine(metered.clone(), asked.clone()),
        Arc::new(|| None::<Result<Vec<lmgw_core::vram::GpuMemory>, String>>),
    );
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let rx = Mutex::new(Some(rx));
    state.machine.set_net_watch(Arc::new(move || {
        rx.lock()
            .unwrap()
            .take()
            .ok_or_else(|| "one watch only".to_string())
    }));
    let id = cut_download(&state, dir.path(), &bytes, 30_000, &sha(&bytes)).await;
    store::fail_orphaned_jobs(&state.db).await.unwrap();
    let boot = hf_download::resume_at_boot(&state).await;
    assert_eq!(boot.paused, vec![id]);
    assert_eq!(asked.load(Ordering::SeqCst), 1);

    // A change, still metered: asked again, and the row waits.
    tx.send(()).await.unwrap();
    for _ in 0..250 {
        if asked.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    let row = store::get_hf_model(&state.db, id).await.unwrap().unwrap();
    assert_eq!(row.error.as_deref(), Some(hf_download::PAUSED_METERED_NOTE));
    assert!(hub.received_requests().await.unwrap().is_empty());

    // Home on an unmetered line: it goes on from the `.part`.
    metered.store(false, Ordering::SeqCst);
    tx.send(()).await.unwrap();
    let row = settled(&state, id).await;
    assert_eq!(row.status, "done", "error: {:?}", row.error);
    assert_eq!(cdn.ranges(), vec![Some("bytes=30000-".to_string())]);
    assert_eq!(
        std::fs::read(dir.path().join(format!("{REPO}/{file}"))).unwrap(),
        bytes
    );
    // The watch is done: the next change finds the channel closed.
    for _ in 0..250 {
        if tx.is_closed() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(tx.is_closed());
}

/// Cancel on a paused row, which nothing transfers: it ends as a cancelled
/// transfer does, its `.part` and its hash gone.
#[tokio::test]
async fn cancel_works_on_a_paused_download() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let bytes = file_bytes(50_000);
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    state.machine.set_machine(
        metered_machine(),
        Arc::new(|| None::<Result<Vec<lmgw_core::vram::GpuMemory>, String>>),
    );
    let id = cut_download(&state, dir.path(), &bytes, 20_000, &sha(&bytes)).await;
    store::fail_orphaned_jobs(&state.db).await.unwrap();
    assert_eq!(hf_download::resume_at_boot(&state).await.paused, vec![id]);
    lmgw_core::ops::hf_set(&state, "cancel", Some(id), "chat")
        .await
        .unwrap();
    let row = store::get_hf_model(&state.db, id).await.unwrap().unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.error.as_deref(), Some(hf_download::CANCELED_NOTE));
    assert_eq!(row.oid, None);
    assert!(!dir
        .path()
        .join(format!("{REPO}/demo-model-Q4_K_M.gguf.part"))
        .exists());
    assert!(hub.received_requests().await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// The capabilities note's projector (D121, review low 9)
// ---------------------------------------------------------------------------

/// The note's sibling projector is the plans' pick: projectors by their
/// header, not their name, sized as the file a symlink points at. A chat
/// model named like a 16-bit projector is passed over, and a symlinked
/// projector is as large as its target.
#[tokio::test]
async fn the_capabilities_note_names_the_projector_the_plans_take() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path()).await;
    let at = dir.path().join(REPO);
    std::fs::create_dir_all(&at).unwrap();
    synth::chat("demoarch", 4096).write_to(&at.join("weights.gguf"));
    // Not a projector, whatever its name says, and the smallest file here.
    synth::chat("demoarch", 8).write_to(&at.join("mmproj-C-F16.gguf"));
    std::fs::write(at.join("mmproj-B-F16.gguf"), projector(4096)).unwrap();
    let elsewhere = dir.path().join("store");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("big.gguf"), projector(65_536)).unwrap();
    std::os::unix::fs::symlink(elsewhere.join("big.gguf"), at.join("mmproj-A-F16.gguf")).unwrap();

    let row = lmgw_core::config::LocalModel {
        id: 1,
        model_id: "demo".into(),
        gguf_path: format!("{REPO}/weights.gguf"),
        params: lmgw_core::config::LlamaParams::default(),
        args: vec![],
        idle_seconds: 300,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        fallback: None,
        capabilities_override: None,
        ladder: vec![],
    };
    let derived = lmgw_core::capabilities::exposed::derived_for_local(&state, &row).await;
    let note = derived
        .notes
        .iter()
        .find(|n| n.contains("A projector file sits next to the weights"))
        .unwrap_or_else(|| panic!("{:?}", derived.notes));
    assert!(note.contains("(mmproj-B-F16.gguf)"), "{note}");
}
