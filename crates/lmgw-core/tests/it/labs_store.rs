//! The lab store (labs-store design §1): groups in keyset pages newest first
//! with their total; a group delete that cascades to its runs and artifact
//! rows and unlinks only the files nothing else names; Clear; the usage read
//! against hand sums; an orphan file counted, then removed; the artifact
//! route's bytes and headers; and the history across a restart. The groups
//! are recorded through `labs::record`, the path the labs' dispatch routes
//! take; the reads go through the routes.

use lmgw_api_types::decision_lab::{DecisionForm, StateMode};
use lmgw_api_types::lab_store::{LabCell, LabGroupPage, LabGroupView, LabKind, LabUsage};
use lmgw_core::labs::record::{self, NewArtifact, NewGroup, NewRun};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::LabRunData;
use serde_json::Value;

use crate::common::{serve, Gw};

/// A 1×1 PNG.
pub const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xfc, 0xcf, 0xc0, 0x50,
    0x0f, 0x00, 0x04, 0x85, 0x01, 0x80, 0x84, 0xa9, 0x8c, 0x21, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// A second picture: the PNG with a different tail, which no sniffer minds.
pub fn other_png() -> Vec<u8> {
    let mut b = PNG.to_vec();
    b.extend_from_slice(b"second");
    b
}

/// A decision form whose state is `state`.
pub fn form(state: &str) -> DecisionForm {
    DecisionForm {
        state_mode: StateMode::Text,
        state: state.into(),
        ..Default::default()
    }
}

/// Record a decision group asking `models` (one cell each, as written) with
/// `inputs`; its id and its inputs' ids.
pub async fn group(
    state: &SharedState,
    state_text: &str,
    models: &[&str],
    inputs: Vec<NewArtifact>,
) -> (i64, Vec<i64>) {
    let form = form(state_text);
    let title = form.title();
    let text = serde_json::to_string(&form).unwrap();
    record::group(
        &state.db,
        &state.data_dir,
        NewGroup {
            lab: LabKind::Decision,
            title,
            plan: models
                .iter()
                .map(|m| LabCell {
                    model: (*m).into(),
                    variant: "as_written".into(),
                })
                .collect(),
            inputs,
        },
        move |_| Ok((text, None)),
    )
    .await
    .unwrap()
}

/// The new bytes `bytes` as an input named `name`.
pub fn bytes(name: &str, bytes: &[u8]) -> NewArtifact {
    NewArtifact::Bytes {
        name: name.into(),
        bytes: bytes::Bytes::copy_from_slice(bytes),
        declared_mime: None,
        data_url_head: None,
    }
}

/// `bytes` as a data URL with the head `head`, typed as a page would: the
/// base64 crate's own encoding, not the server's.
pub fn typed_data_url(head: &str, bytes: &[u8]) -> String {
    use base64::Engine as _;
    format!(
        "{head},{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// Record cell `cell` of group `group_id`: a 200 with `response`.
pub async fn run(
    state: &SharedState,
    group_id: i64,
    cell: i64,
    request: &str,
    response: &str,
) -> i64 {
    record::run(
        &state.db,
        &state.data_dir,
        NewRun {
            data: LabRunData {
                group_id,
                cell,
                variant: "as_written".into(),
                endpoint: "/v1/systemone".into(),
                model_asked: "decide/laya".into(),
                answered_by: "decide/laya".into(),
                upstream_model: Some("laya".into()),
                http_status: 200,
                request: request.into(),
                response: Some(response.into()),
                response_type: Some("application/json".into()),
                headers: r#"{"x-lmgw-upstream":"aux"}"#.into(),
                input_tokens: Some(299),
                output_tokens: Some(0),
                latency_ms: 33,
                ..Default::default()
            },
            outputs: vec![],
            replace_failed: false,
        },
    )
    .await
    .unwrap()
    .id
}

async fn get(gw: &Gw, path: &str) -> (u16, Value) {
    let resp = gw.client().get(format!("{gw}{path}")).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn post(gw: &Gw, path: &str) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}{path}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn page(gw: &Gw, query: &str) -> LabGroupPage {
    let (status, body) = get(gw, &format!("/labs/api/decision/groups{query}")).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_value(body).unwrap()
}

fn ids(p: &LabGroupPage) -> Vec<i64> {
    p.groups.iter().map(|g| g.id).collect()
}

/// The files in the artifacts directory.
fn files(state: &SharedState) -> Vec<String> {
    let dir = lmgw_core::labs::artifacts::dir(&state.data_dir);
    let mut out: Vec<String> = match std::fs::read_dir(dir) {
        Ok(e) => e
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

#[tokio::test]
async fn the_history_pages_newest_first_with_its_total() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let mut made = Vec::new();
    for i in 0..5 {
        let model = if i % 2 == 0 {
            "decide/laya"
        } else {
            "decide/kev-4b"
        };
        made.push(
            group(&state, &format!("state {i}"), &[model], vec![])
                .await
                .0,
        );
    }

    let p = page(&gw, "?limit=2").await;
    assert_eq!(ids(&p), [made[4], made[3]]);
    assert_eq!((p.total, p.older), (5, 3));
    let p = page(&gw, &format!("?limit=2&before={}", made[3])).await;
    assert_eq!(ids(&p), [made[2], made[1]]);
    assert_eq!((p.total, p.older), (5, 1));
    let p = page(&gw, &format!("?before={}", made[1])).await;
    assert_eq!(ids(&p), [made[0]]);
    assert_eq!(p.older, 0);
    // No limit is every group: the server sets no maximum.
    assert_eq!(page(&gw, "").await.groups.len(), 5);
    // An empty page leaves every group from its cursor on.
    let p = page(&gw, "?limit=0").await;
    assert_eq!((p.groups.len(), p.total, p.older), (0, 5, 5));
    let p = page(&gw, &format!("?limit=0&before={}", made[2])).await;
    assert_eq!(p.older, 2);

    let p = page(&gw, "?model=decide/kev-4b").await;
    assert_eq!(ids(&p), [made[3], made[1]]);
    assert_eq!(p.total, 2);
    assert_eq!(p.groups[0].title, "state 3");
    assert_eq!(p.groups[0].plan[0].model, "decide/kev-4b");

    // Another lab's history is its own, and a lab that is none is a 404.
    let (status, body) = get(&gw, "/labs/api/image/groups").await;
    assert_eq!((status, body["groups"].as_array().unwrap().len()), (200, 0));
    let (status, body) = get(&gw, "/labs/api/chat/groups").await;
    assert_eq!((status, body["code"].as_str()), (404, Some("not_found")));
    let (status, _) = get(&gw, &format!("/labs/api/image/groups/{}", made[0])).await;
    assert_eq!(status, 404, "a group is its lab's");
}

#[tokio::test]
async fn a_delete_cascades_and_unlinks_only_what_nothing_else_names() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let (a, a_inputs) = group(&state, "first", &["decide/laya"], vec![bytes("x.png", PNG)]).await;
    run(&state, a, 0, "{}", "{}").await;
    // The same picture again, as the re-opened form sends it, and a new one.
    let (b, _) = group(
        &state,
        "second",
        &["decide/laya"],
        vec![
            NewArtifact::Existing { id: a_inputs[0] },
            bytes("y.png", &other_png()),
        ],
    )
    .await;
    assert_eq!(files(&state).len(), 2, "one file per distinct picture");

    let (status, usage) = post(&gw, &format!("/labs/api/decision/groups/{a}/delete")).await;
    assert_eq!(status, 200, "{usage}");
    let usage: LabUsage = serde_json::from_value(usage).unwrap();
    assert_eq!(
        (
            usage.groups,
            usage.runs,
            usage.artifacts,
            usage.artifact_files
        ),
        (1, 0, 2, 2)
    );
    assert_eq!(files(&state).len(), 2, "b still names the shared picture");
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lab_runs")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(runs, 0, "the runs went with their group");

    let (status, body) = post(&gw, &format!("/labs/api/decision/groups/{a}/delete")).await;
    assert_eq!((status, body["code"].as_str()), (404, Some("not_found")));

    let (status, _) = post(&gw, &format!("/labs/api/decision/groups/{b}/delete")).await;
    assert_eq!(status, 200);
    assert!(files(&state).is_empty(), "{:?}", files(&state));
}

#[tokio::test]
async fn clear_empties_one_lab_and_its_files() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    for i in 0..3 {
        let (g, _) = group(
            &state,
            &format!("s{i}"),
            &["decide/laya"],
            vec![bytes("x.png", PNG)],
        )
        .await;
        run(&state, g, 0, "{}", "{}").await;
    }
    // An image-lab group naming the same file keeps it.
    let kept = record::group(
        &state.db,
        &state.data_dir,
        NewGroup {
            lab: LabKind::Image,
            title: "a cat".into(),
            plan: vec![],
            inputs: vec![bytes("x.png", PNG)],
        },
        |_| Ok(("{}".into(), None)),
    )
    .await
    .unwrap();

    let (status, usage) = post(&gw, "/labs/api/decision/clear").await;
    assert_eq!(status, 200, "{usage}");
    let usage: LabUsage = serde_json::from_value(usage).unwrap();
    assert_eq!(
        (usage.groups, usage.runs, usage.artifacts, usage.text_bytes),
        (0, 0, 0, 0)
    );
    assert_eq!(files(&state).len(), 1, "the image lab still names it");
    let (_, image) = get(&gw, "/labs/api/image/usage").await;
    assert_eq!(image["groups"], 1);

    let (status, _) = post(&gw, &format!("/labs/api/image/groups/{}/delete", kept.0)).await;
    assert_eq!(status, 200);
    assert!(files(&state).is_empty());
}

#[tokio::test]
async fn the_usage_is_the_stored_sizes_and_an_orphan_is_counted_then_removed() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let (g, _) = group(
        &state,
        "Grüße",
        &["decide/laya"],
        vec![bytes("x.png", PNG), bytes("again.png", PNG)],
    )
    .await;
    let request = r#"{"model": "decide/laya", "state": "Grüße"}"#;
    let response = r#"{"model": "laya", "answers": {}}"#;
    run(&state, g, 0, request, response).await;

    let row: (String, String, String) =
        sqlx::query_as("SELECT title, form, plan FROM lab_groups WHERE id = ?1")
            .bind(g)
            .fetch_one(&state.db)
            .await
            .unwrap();
    let headers = r#"{"x-lmgw-upstream":"aux"}"#;
    let text =
        row.0.len() + row.1.len() + row.2.len() + request.len() + response.len() + headers.len();

    let (status, usage) = get(&gw, "/labs/api/decision/usage").await;
    assert_eq!(status, 200, "{usage}");
    let usage: LabUsage = serde_json::from_value(usage).unwrap();
    assert_eq!(
        usage,
        LabUsage {
            lab: LabKind::Decision,
            groups: 1,
            batches: 0,
            runs: 1,
            artifacts: 2,
            artifact_files: 1,
            text_bytes: text as u64,
            artifact_bytes: PNG.len() as u64,
            orphan_files: 0,
            orphan_bytes: 0,
        },
        "bytes, not characters: ü is two"
    );

    // A crash between a file's write and its commit leaves a file no row
    // names, and a temporary file mid-write: both orphans.
    let dir = lmgw_core::labs::artifacts::dir(&state.data_dir);
    std::fs::write(
        dir.join(lmgw_core::labs::artifacts::sha256_hex(b"lost")),
        b"lost",
    )
    .unwrap();
    std::fs::write(dir.join("abc.tmp-1-0"), b"half").unwrap();
    let (_, usage) = get(&gw, "/labs/api/decision/usage").await;
    assert_eq!(
        (
            usage["orphan_files"].as_u64(),
            usage["orphan_bytes"].as_u64()
        ),
        (Some(2), Some(8))
    );
    assert_eq!(files(&state).len(), 3, "nothing sweeps on its own");

    let (status, usage) = post(&gw, "/labs/api/decision/orphans/delete").await;
    assert_eq!(status, 200, "{usage}");
    assert_eq!(usage["orphan_files"], 0);
    assert_eq!(files(&state).len(), 1, "the named file stays");
}

#[tokio::test]
async fn an_artifact_is_served_as_its_bytes_sandboxed() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let (_, inputs) = group(&state, "s", &["m"], vec![bytes("x.png", PNG)]).await;
    let resp = gw
        .client()
        .get(format!("{gw}/labs/api/artifacts/{}", inputs[0]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let h = |n: &str| resp.headers()[n].to_str().unwrap().to_string();
    assert_eq!(h("content-type"), "image/png", "sniffed from the bytes");
    assert_eq!(h("x-content-type-options"), "nosniff");
    assert_eq!(h("content-security-policy"), "sandbox");
    assert_eq!(resp.bytes().await.unwrap().as_ref(), PNG);

    let (status, body) = get(&gw, "/labs/api/artifacts/999").await;
    assert_eq!((status, body["code"].as_str()), (404, Some("not_found")));
}

#[tokio::test]
async fn the_history_survives_a_restart() {
    let state = AppState::init_for_tests().await.unwrap();
    let (g, _) = group(&state, "kept", &["decide/laya"], vec![]).await;
    run(
        &state,
        g,
        0,
        r#"{"model":"decide/laya"}"#,
        r#"{"answers":{}}"#,
    )
    .await;
    let before: LabGroupView = {
        let gw = serve(state.clone()).await;
        let (status, body) = get(&gw, &format!("/labs/api/decision/groups/{g}")).await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_value(body).unwrap()
    };

    let again = AppState::init_for_tests_on(state.db.clone()).await.unwrap();
    let gw = serve(again).await;
    let p = page(&gw, "").await;
    assert_eq!(p.groups, std::slice::from_ref(&before));
    assert_eq!(before.runs.len(), 1);
    assert_eq!(
        before.runs[0].response.as_deref(),
        Some(r#"{"answers":{}}"#)
    );
    let form: DecisionForm = serde_json::from_value(before.form).unwrap();
    assert_eq!(form.state, "kept");
}

/// A record whose second file cannot be written (its digest's name is
/// taken by a directory) fails, and the first file it wrote goes with it:
/// a failed record leaves no file nothing names.
#[tokio::test]
async fn a_record_that_fails_part_way_leaves_none_of_its_files() {
    let state = AppState::init_for_tests().await.unwrap();
    let second = other_png();
    let blocked = lmgw_core::labs::artifacts::path(
        &state.data_dir,
        &lmgw_core::labs::artifacts::sha256_hex(&second),
    )
    .unwrap();
    std::fs::create_dir_all(blocked.join("in-the-way")).unwrap();
    let form = form("x");
    let text = serde_json::to_string(&form).unwrap();
    let failed = record::group(
        &state.db,
        &state.data_dir,
        NewGroup {
            lab: LabKind::Decision,
            title: form.title(),
            plan: vec![LabCell {
                model: "m".into(),
                variant: "as_written".into(),
            }],
            inputs: vec![bytes("a.png", PNG), bytes("b.png", &second)],
        },
        move |_| Ok((text, None)),
    )
    .await;
    assert!(
        matches!(failed, Err(record::RecordError::Failed(_))),
        "{failed:?}"
    );
    let first = lmgw_core::labs::artifacts::sha256_hex(PNG);
    assert!(
        !files(&state).contains(&first),
        "the first file went: {:?}",
        files(&state)
    );
    // Nothing was kept, so nothing else is in the directory but the
    // obstacle.
    assert_eq!(
        files(&state),
        vec![lmgw_core::labs::artifacts::sha256_hex(&second)]
    );
}
