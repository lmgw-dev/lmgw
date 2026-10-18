//! The answers of the docs tools that no OpenAPI document describes
//! (`lmgw__docs_corpora`, `lmgw__docs_requests`, `lmgw__docs_corpus_set`,
//! `lmgw__docs_ingest`, `lmgw__docs_request_set`): each tool's text is read
//! into its type and written out again, and the two texts are equal byte for
//! byte. The text is the answer's pretty JSON with sorted keys, so a key a
//! type adds, drops, renames or turns from absent into `null` (or an integer
//! into a float) shows up here.
//!
//! Every job is a fake executor (`support::docs_world`): none reaches the
//! network, and the state a test reads is the one it waited for.

use lmgw_api_types as dto;
use lmgw_api_types::self_admin::{
    CorpusJob, DocsCorpusList, DocsCorpusOne, DocsIngestStarted, DocsRequestQueue,
};
use lmgw_core::jobs::{self, JobKind};
use lmgw_core::state::SharedState;
use quickdoc_core::store as qstore;
use serde_json::{json, Value};
use wiremock::MockServer;

use crate::common::patience;
use crate::support::docs_world::{seed, setup};
use crate::support::self_admin::{parse, read_back, Admin};

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

const JOB_KEYS: [&str; 10] = [
    "id",
    "kind",
    "status",
    "stage",
    "done",
    "total",
    "percent",
    "error",
    "started_at",
    "finished_at",
];

/// Start a fake job of `kind` for corpus `cid`, and wait for it to be in the
/// state a test reads: `working` is a held job that has reported once, any
/// other is the status it ends in.
async fn job(state: &SharedState, kind: JobKind, cid: i64, input: Value, until: &str) -> i64 {
    let id = jobs::spawn(
        state,
        kind,
        Some(lmgw_core::quickdoc::ingest::job_key(cid)),
        format!("job for {cid}"),
        input,
    )
    .await
    .unwrap()
    .id();
    patience::until_async(&format!("job {id} to be {until}"), || async {
        let rows = jobs::list(state, None, false, 0).await.unwrap();
        rows.iter().any(|j| {
            j.id == id && (j.status == until || (until == "working" && j.stage == "working"))
        })
    })
    .await;
    id
}

#[tokio::test]
async fn the_corpus_list_text_of_an_empty_gateway_is_an_empty_list_and_a_hint() {
    let mock = MockServer::start().await;
    let state = setup(&mock).await;
    let admin = Admin::new(&state).await;

    let text = admin.text("lmgw__docs_corpora", json!({})).await;
    let live = parse(&text);
    assert_keys(
        &live,
        &[
            "corpora",
            "hint",
            "pending_requests",
            "rerank_model",
            "schema_version",
        ],
    );
    assert_eq!(live["corpora"], json!([]));
    assert!(live["rerank_model"].is_null());
    assert_eq!(live["pending_requests"], 0);
    assert!(live["hint"]
        .as_str()
        .unwrap()
        .contains("lmgw__docs_requests"));
    let list: DocsCorpusList = read_back(&text);
    assert!(list.corpora.is_empty());
}

#[tokio::test]
async fn the_corpus_list_text_reads_back_equal_with_a_job_in_every_state() {
    let mock = MockServer::start().await;
    let state = setup(&mock).await;
    let running = seed(&state, "axum", "0.8", "ingesting", 3).await;
    let idle = seed(&state, "tower", "0.5", "ready", 3).await;
    let finished = seed(&state, "serde", "1.0", "ready", 1).await;
    let failed = seed(&state, "tokio", "1", "failed", 0).await;
    let unmeasured = seed(&state, "hyper", "1", "ready", 3).await;
    // axum: held, known total. serde: a re-embed that finished. tokio: an
    // ingest that failed. hyper: held, no total. tower: never had one.
    let held = job(
        &state,
        JobKind::Ingest,
        running,
        json!({"done": 4, "total": 10}),
        "working",
    )
    .await;
    job(
        &state,
        JobKind::ReEmbed,
        finished,
        json!({"mode": "done", "done": 7, "total": 7}),
        "done",
    )
    .await;
    job(
        &state,
        JobKind::Ingest,
        failed,
        json!({"mode": "fail"}),
        "failed",
    )
    .await;
    job(&state, JobKind::Ingest, unmeasured, json!({}), "working").await;
    qstore::file_doc_request(&state.corpus, "wgpu", "", None, None)
        .await
        .unwrap();
    let admin = Admin::new(&state).await;

    let text = admin.text("lmgw__docs_corpora", json!({})).await;
    let live = parse(&text);
    assert_eq!(live["pending_requests"], 1);
    let rows = live["corpora"].as_array().unwrap();
    assert_eq!(rows.len(), 5);
    let row = |lib: &str| rows.iter().find(|r| r["library"] == lib).unwrap();

    // The corpus' own keys, and `job` besides when it has had one.
    let mut with_job: Vec<&str> = vec!["job"];
    let corpus_keys: Vec<&str> = row("tower")
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        corpus_keys.len(),
        // The 28 of a corpus view, less the four eval keys a never-evaluated
        // corpus leaves out.
        24,
        "no job key on a corpus with none: {}",
        row("tower")
    );
    assert!(!corpus_keys.contains(&"job"));
    with_job.extend(&corpus_keys);
    assert_keys(row("axum"), &with_job);
    assert_keys(&row("axum")["job"], &JOB_KEYS);
    assert_eq!(row("axum")["job"]["id"], held);
    assert_eq!(row("axum")["job"]["kind"], "ingest");
    assert_eq!(row("axum")["job"]["status"], "running");
    assert_eq!(row("axum")["job"]["stage"], "working");
    assert_eq!(row("axum")["job"]["done"], 4);
    assert_eq!(row("axum")["job"]["total"], 10);
    assert_eq!(row("axum")["job"]["percent"], 40);
    assert!(row("axum")["job"]["error"].is_null());
    // A finished job stays on the corpus: it is how "it succeeded" differs
    // from "it failed in the first second".
    assert_eq!(row("serde")["job"]["kind"], "re_embed");
    assert_eq!(row("serde")["job"]["status"], "done");
    assert_eq!(row("serde")["job"]["percent"], 100);
    assert!(!row("serde")["job"]["finished_at"].is_null());
    assert_eq!(row("tokio")["job"]["status"], "failed");
    assert_eq!(row("tokio")["job"]["error"], "executor said no");
    // No total is `null`, with `percent` beside it.
    assert_eq!(row("hyper")["job"]["done"], 3);
    assert!(row("hyper")["job"]["total"].is_null());
    assert!(row("hyper")["job"]["percent"].is_null());
    let _ = idle;
    let list: DocsCorpusList = read_back(&text);
    assert_eq!(list.corpora.len(), 5);
    let axum = list
        .corpora
        .iter()
        .find(|c| c.corpus.library == "axum")
        .unwrap();
    assert_eq!(axum.job.as_ref().unwrap().percent, Some(40));
    assert!(list
        .corpora
        .iter()
        .find(|c| c.corpus.library == "tower")
        .unwrap()
        .job
        .is_none());

    // One corpus, by either selector: `{corpus}` with the same row, and no
    // hint, queue size or schema version beside it.
    for sel in [running.to_string(), "axum@0.8".to_string()] {
        let text = admin
            .text("lmgw__docs_corpora", json!({"corpus": sel}))
            .await;
        assert_keys(&parse(&text), &["corpus"]);
        let one: DocsCorpusOne = read_back(&text);
        assert_eq!(one.corpus.corpus.corpus_id, "axum@0.8");
        let job: &CorpusJob = one.corpus.job.as_ref().unwrap();
        assert_eq!((job.id, job.done, job.total), (held, 4, Some(10)));
        assert_eq!(parse(&text)["corpus"], *row("axum"));
    }
    let text = admin
        .text("lmgw__docs_corpora", json!({"corpus": "tower@0.5"}))
        .await;
    let one: DocsCorpusOne = read_back(&text);
    assert!(one.corpus.job.is_none());
    assert!(parse(&text)["corpus"].get("job").is_none(), "{text}");
    let (failed_call, text) = admin
        .call("lmgw__docs_corpora", json!({"corpus": "nope@1"}))
        .await;
    assert!(failed_call, "{text}");

    // Cancel the held jobs so nothing outlives the test.
    for j in jobs::list(&state, None, true, 0).await.unwrap() {
        lmgw_core::ops::job_cancel(&state, j.id).await.unwrap();
    }
}

#[tokio::test]
async fn the_request_queue_text_is_the_dashboard_s_list_and_a_hint() {
    let mock = MockServer::start().await;
    let state = setup(&mock).await;
    qstore::file_doc_request(
        &state.corpus,
        "hyper",
        "1.0",
        Some("need Service"),
        Some("agent"),
    )
    .await
    .unwrap();
    qstore::file_doc_request(&state.corpus, "reqwest", "", None, None)
        .await
        .unwrap();
    qstore::set_doc_request_status(&state.corpus, 2, "dismissed")
        .await
        .unwrap();
    let admin = Admin::new(&state).await;

    let text = admin.text("lmgw__docs_requests", json!({})).await;
    let live = parse(&text);
    assert_keys(&live, &["hint", "requests"]);
    assert!(live["hint"]
        .as_str()
        .unwrap()
        .contains("lmgw__docs_corpus_set"));
    let queue: DocsRequestQueue = read_back(&text);
    assert_eq!(queue.response.requests.len(), 2);
    // The tool's rows are the dashboard's rows. The queue is ordered by count
    // and then by the second a request was last made in, so the two rows,
    // filed one after the other, can come in either order: find them by name.
    let http = parse(&text);
    let reason_of = |library: &str| {
        http["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["library"] == library)
            .unwrap_or_else(|| panic!("no request for {library}: {text}"))["reason"]
            .clone()
    };
    assert_eq!(reason_of("hyper"), "need Service");
    assert!(reason_of("reqwest").is_null());

    let text = admin
        .text("lmgw__docs_requests", json!({"status": "dismissed"}))
        .await;
    let queue: DocsRequestQueue = read_back(&text);
    assert_eq!(queue.response.requests.len(), 1);
    assert_eq!(queue.response.requests[0].library, "reqwest");
    let text = admin
        .text("lmgw__docs_requests", json!({"status": "fulfilled"}))
        .await;
    let queue: DocsRequestQueue = read_back(&text);
    assert!(queue.response.requests.is_empty());
    assert_eq!(parse(&text)["requests"], json!([]));

    // One answer to the queue, one to the status write.
    let text = admin
        .text(
            "lmgw__docs_request_set",
            json!({"id": 2, "status": "pending"}),
        )
        .await;
    assert_eq!(parse(&text), json!({"ok": true}));
    read_back::<dto::Ack>(&text);
}

#[tokio::test]
async fn creating_and_deleting_a_corpus_answer_with_and_without_a_next_step() {
    let mock = MockServer::start().await;
    let state = setup(&mock).await;
    let admin = Admin::new(&state).await;
    let create = |library: &str, start: Option<bool>| {
        let mut args = json!({
            "action": "create", "library": library, "version": "2",
            "embed_model": "embed-model", "ingest_model": "ingest-model",
            "source_root": format!("https://docs.rs/{library}\nhttps://docs.rs/{library}/llms.txt"),
            "source_kind": "markdown",
        });
        if let Some(s) = start {
            args["start"] = json!(s);
        }
        args
    };

    // Asked not to start: the corpus is empty until the ingest runs.
    let text = admin
        .text("lmgw__docs_corpus_set", create("bytes", Some(false)))
        .await;
    let live = parse(&text);
    assert_keys(&live, &["corpus", "job_id", "next_step", "ok"]);
    assert!(live["job_id"].is_null());
    assert_eq!(
        live["next_step"],
        "lmgw__docs_ingest action=start — the corpus exists but is empty until it runs"
    );
    let made: dto::WithNextStep<dto::CorpusCreated> = read_back(&text);
    assert_eq!(made.answer.corpus.corpus_id, "bytes@2");
    assert_eq!(made.answer.corpus.sources.len(), 2);

    // Started (the default): the job's id, and the poll that follows it.
    let text = admin
        .text("lmgw__docs_corpus_set", create("http", None))
        .await;
    let live = parse(&text);
    assert_keys(&live, &["corpus", "job_id", "next_step", "ok"]);
    assert!(live["job_id"].is_i64() || live["job_id"].is_u64(), "{text}");
    assert_eq!(
        live["next_step"],
        "poll lmgw__docs_corpora corpus=<id> — `job` reports the ingest's stage and percent"
    );
    let made: dto::WithNextStep<dto::CorpusCreated> = read_back(&text);
    assert!(made.answer.job_id.is_some());

    // A delete is the dashboard's acknowledgement, with no next step.
    let text = admin
        .text(
            "lmgw__docs_corpus_set",
            json!({"action": "delete", "corpus": "bytes@2"}),
        )
        .await;
    assert_eq!(
        parse(&text),
        json!({"ok": true, "message": "deleted bytes@2"})
    );
    read_back::<dto::MessageAck>(&text);

    for j in jobs::list(&state, None, true, 0).await.unwrap() {
        lmgw_core::ops::job_cancel(&state, j.id).await.unwrap();
    }
}

#[tokio::test]
async fn the_ingest_tool_answers_the_job_it_queued_or_a_cancel() {
    let mock = MockServer::start().await;
    let state = setup(&mock).await;
    let cid = seed(&state, "axum", "0.8", "ready", 3).await;
    let admin = Admin::new(&state).await;

    let text = admin
        .text(
            "lmgw__docs_ingest",
            json!({"corpus": "axum@0.8", "action": "start"}),
        )
        .await;
    let live = parse(&text);
    assert_keys(
        &live,
        &["already_running", "job_id", "message", "next_step", "ok"],
    );
    assert_eq!(live["already_running"], false);
    assert_eq!(live["message"], "ingesting axum@0.8");
    assert_eq!(live["next_step"], "poll lmgw__docs_corpora corpus=<id>");
    let started: DocsIngestStarted = read_back(&text);
    assert!(started.ok && !started.already_running);

    // Asking again finds the job. A re-embed is a job of its own kind: a
    // fresh one, which a second ask finds in turn.
    let text = admin
        .text(
            "lmgw__docs_ingest",
            json!({"corpus": cid.to_string(), "action": "start"}),
        )
        .await;
    let again: DocsIngestStarted = read_back(&text);
    assert!(again.already_running, "{text}");
    assert_eq!(again.job_id, started.job_id);
    assert_eq!(again.message, "ingesting axum@0.8");
    let text = admin
        .text(
            "lmgw__docs_ingest",
            json!({"corpus": "axum@0.8", "action": "re_embed", "embed_model": "embed-model"}),
        )
        .await;
    let fresh: DocsIngestStarted = read_back(&text);
    assert!(!fresh.already_running, "{text}");
    assert_ne!(fresh.job_id, started.job_id);
    assert_eq!(fresh.message, "re-embedding axum@0.8");
    assert!(whole_id(&parse(&text)["job_id"]));
    let text = admin
        .text(
            "lmgw__docs_ingest",
            json!({"corpus": "axum@0.8", "action": "re_embed"}),
        )
        .await;
    let again: DocsIngestStarted = read_back(&text);
    assert!(again.already_running, "{text}");
    assert_eq!(again.job_id, fresh.job_id);

    // Cancel is the job row's acknowledgement: the newest live job of the
    // corpus first, then the one before it.
    for job in [fresh.job_id, started.job_id] {
        let text = admin
            .text(
                "lmgw__docs_ingest",
                json!({"corpus": "axum@0.8", "action": "cancel"}),
            )
            .await;
        assert_eq!(
            parse(&text),
            json!({"ok": true, "message": format!("cancel requested for job {job}")})
        );
        read_back::<dto::MessageAck>(&text);
        patience::until_async("the cancelled job to end", || async {
            jobs::list(&state, None, true, 0)
                .await
                .unwrap()
                .iter()
                .all(|j| j.id != job)
        })
        .await;
    }

    // With nothing live to cancel, the tool refuses.
    let (failed, text) = admin
        .call(
            "lmgw__docs_ingest",
            json!({"corpus": "axum@0.8", "action": "cancel"}),
        )
        .await;
    assert!(failed && text.contains("nothing to cancel"), "{text}");
}

fn whole_id(v: &Value) -> bool {
    v.is_u64() || v.is_i64()
}
