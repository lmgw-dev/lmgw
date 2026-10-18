//! The lab store's export (labs-store design §1.7): every JSONL line reads
//! into `LabExportLine` and back, against the document; a scope with
//! artifacts is a zip whose files hash to their rows, one path per file even
//! where its rows differ in type; the Decision lab's CSV
//! has its header, a row per run and question, a failed run as one row, and
//! RFC 4180 quoting of a state with quotes and line breaks; the file names
//! in `Content-Disposition`.

use std::io::Read;

use lmgw_api_types::lab_store::LabExportLine;
use lmgw_core::labs::record::{self, NewArtifact, NewRun};
use lmgw_core::state::AppState;
use lmgw_core::store::LabRunData;
use serde_json::Value;

use crate::common::{round_trips, serve, Gw};
use crate::labs_store::{bytes, group, run, PNG};
use crate::support::decision_world::LLAMA_ANSWER;

/// The export's status, `Content-Type`, `Content-Disposition` and body.
async fn export(gw: &Gw, query: &str) -> (u16, String, String, Vec<u8>) {
    let resp = gw
        .client()
        .get(format!("{gw}/labs/api/decision/export{query}"))
        .send()
        .await
        .unwrap();
    let h = |n: &str| {
        resp.headers()
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    let (ct, cd) = (h("content-type"), h("content-disposition"));
    (
        resp.status().as_u16(),
        ct,
        cd,
        resp.bytes().await.unwrap().to_vec(),
    )
}

fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

#[tokio::test]
async fn every_jsonl_line_reads_into_its_type_and_back() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let (a, _) = group(&state, "first", &["decide/laya", "decide/kev-4b"], vec![]).await;
    run(&state, a, 0, r#"{"model": "decide/laya"}"#, LLAMA_ANSWER).await;
    let (b, _) = group(&state, "second", &["decide/laya"], vec![]).await;

    let (status, ct, cd, body) = export(&gw, "").await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(ct, "application/x-ndjson");
    assert_eq!(
        cd,
        format!(
            "attachment; filename=\"lmgw-lab-decision-{0}.jsonl\"; \
             filename*=UTF-8''lmgw-lab-decision-{0}.jsonl",
            today()
        )
    );
    let text = String::from_utf8(body).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    let mut seen = Vec::new();
    for line in lines {
        let live: Value = serde_json::from_str(line).unwrap();
        let typed = round_trips::<LabExportLine>("export line", &live);
        assert_eq!(typed.format, "lmgw.lab.v1");
        seen.push(typed.group.id);
        if typed.group.id == a {
            assert_eq!(typed.group.runs.len(), 1, "the runs are inside");
            assert_eq!(
                typed.group.runs[0].response.as_deref(),
                Some(LLAMA_ANSWER),
                "the exact text"
            );
        }
    }
    assert_eq!(seen, [a, b], "oldest first");

    let (_, _, cd, body) = export(&gw, &format!("?group={b}")).await;
    assert!(
        cd.contains(&format!("lmgw-lab-decision-group-{b}-{}.jsonl", today())),
        "{cd}"
    );
    assert_eq!(String::from_utf8(body).unwrap().lines().count(), 1);
    let (_, _, _, body) = export(&gw, "?model=decide/kev-4b").await;
    assert_eq!(String::from_utf8(body).unwrap().lines().count(), 1);
    let (status, _, _, body) = export(&gw, "?group=999").await;
    assert_eq!(status, 404, "{}", String::from_utf8_lossy(&body));
    let (status, _, _, _) = export(&gw, "?from=yesterday").await;
    assert_eq!(status, 400);
    let (_, _, _, body) = export(&gw, "?from=2999-01-01T00:00:00Z").await;
    assert!(body.is_empty(), "nothing that new");
}

#[tokio::test]
async fn a_scope_with_artifacts_is_a_zip_whose_files_hash_to_their_rows() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let mut second = PNG.to_vec();
    second.extend_from_slice(b"2");
    let (g, inputs) = group(
        &state,
        "with pictures",
        &["decide/laya"],
        vec![
            bytes("a.png", PNG),
            bytes("b.png", &second),
            bytes("c.png", PNG),
        ],
    )
    .await;
    // A run's output is an artifact too.
    record::run(
        &state.db,
        &state.data_dir,
        NewRun {
            data: LabRunData {
                group_id: g,
                cell: 0,
                variant: "as_written".into(),
                endpoint: "/v1/systemone".into(),
                model_asked: "m".into(),
                answered_by: "m".into(),
                http_status: 200,
                request: "{}".into(),
                headers: "{}".into(),
                ..Default::default()
            },
            outputs: vec![bytes("out.png", PNG)],
            replace_failed: false,
        },
    )
    .await
    .unwrap();

    let (status, ct, cd, body) = export(&gw, "").await;
    assert_eq!(status, 200);
    assert_eq!(ct, "application/zip");
    assert!(
        cd.contains(&format!("lmgw-lab-decision-{}.zip", today())),
        "{cd}"
    );
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(body)).unwrap();
    let names: Vec<String> = zip.file_names().map(String::from).collect();
    let jsonl = format!("lmgw-lab-decision-{}.jsonl", today());
    assert!(names.contains(&jsonl), "{names:?}");
    assert_eq!(
        names.len(),
        3,
        "the JSONL and each distinct file once: {names:?}"
    );

    let mut text = String::new();
    zip.by_name(&jsonl)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    let line: LabExportLine = serde_json::from_str(text.trim_end()).unwrap();
    assert_eq!(
        line.group
            .artifacts
            .iter()
            .map(|a| a.id)
            .collect::<Vec<_>>(),
        inputs
    );
    let all = line
        .group
        .artifacts
        .iter()
        .chain(line.group.runs[0].artifacts.iter());
    for a in all {
        assert_eq!(
            a.href,
            format!("artifacts/{}.png", a.sha256),
            "its path in the zip"
        );
        let mut bytes = Vec::new();
        zip.by_name(&a.href)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(lmgw_core::labs::artifacts::sha256_hex(&bytes), a.sha256);
        assert_eq!(bytes.len() as u64, a.size);
    }
}

#[tokio::test]
async fn rows_sharing_a_file_share_its_path_in_the_zip_whatever_their_types() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    // Bytes of no kind the sniffer knows: each row keeps the type it was
    // declared with, so one file has two types.
    let raw = [0u8, 159, 146, 150];
    let declared = |name: &str, mime: &str| NewArtifact::Bytes {
        name: name.into(),
        bytes: bytes::Bytes::copy_from_slice(&raw),
        declared_mime: Some(mime.into()),
        data_url_head: None,
    };
    let (a, _) = group(&state, "first", &["m"], vec![declared("a", "image/png")]).await;
    let (b, _) = group(&state, "second", &["m"], vec![declared("b", "audio/wav")]).await;

    let (status, ct, _, body) = export(&gw, "").await;
    assert_eq!((status, ct.as_str()), (200, "application/zip"));
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(body)).unwrap();
    let names: Vec<String> = zip.file_names().map(String::from).collect();
    let sha = lmgw_core::labs::artifacts::sha256_hex(&raw);
    let inside = format!("artifacts/{sha}.png");
    assert_eq!(names.len(), 2, "the JSONL and the one file: {names:?}");
    assert!(
        names.contains(&inside),
        "the first row's extension: {names:?}"
    );
    assert!(!names.iter().any(|n| n == "MISSING.txt"));

    let jsonl = format!("lmgw-lab-decision-{}.jsonl", today());
    let mut text = String::new();
    zip.by_name(&jsonl)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    let lines: Vec<LabExportLine> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.iter().map(|l| l.group.id).collect::<Vec<_>>(), [a, b]);
    for line in &lines {
        let art = &line.group.artifacts[0];
        assert_eq!(art.href, inside, "every row names the file the zip has");
    }
    assert_eq!(
        (
            lines[0].group.artifacts[0].mime.as_str(),
            lines[1].group.artifacts[0].mime.as_str()
        ),
        ("image/png", "audio/wav")
    );
}

#[tokio::test]
async fn the_csv_has_a_row_per_run_and_question_quoted_as_rfc_4180_says() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let (g, _) = group(&state, "x", &["decide/laya", "decide/laya"], vec![]).await;
    let request = r#"{"model": "decide/laya", "state": "She said \"refund\",\nthen left."}"#;
    run(&state, g, 0, request, LLAMA_ANSWER).await;
    // A failed cell: one row, its question columns empty.
    record::run(
        &state.db,
        &state.data_dir,
        NewRun {
            data: LabRunData {
                group_id: g,
                cell: 1,
                variant: "reversed".into(),
                endpoint: "/v1/systemone".into(),
                model_asked: "decide/laya".into(),
                answered_by: "decide/laya".into(),
                http_status: 500,
                error_message: Some("input (8519 tokens) is too large".into()),
                request: request.into(),
                response: Some(r#"{"error": {"code": 500}}"#.into()),
                headers: "{}".into(),
                latency_ms: 7,
                ..Default::default()
            },
            outputs: vec![],
            replace_failed: false,
        },
    )
    .await
    .unwrap();

    let (status, ct, cd, body) = export(&gw, "?format=csv").await;
    assert_eq!(status, 200);
    assert_eq!(ct, "text/csv; charset=utf-8");
    assert!(
        cd.contains(&format!("lmgw-lab-decision-{}.csv", today())),
        "{cd}"
    );
    let text = String::from_utf8(body).unwrap();
    let records: Vec<&str> = text.split("\r\n").filter(|r| !r.is_empty()).collect();
    assert_eq!(
        records[0],
        "group_id,batch_id,item_ix,item_key,run_id,created_at,model_asked,answered_by,\
         upstream_model,variant,http_status,error_message,latency_ms,input_tokens,question_id,\
         question_type,answer,expected,correct,confidence,probabilities,state"
    );
    // A group of the history is no set run's item: its item columns are
    // empty.
    assert!(
        records[1].starts_with(&format!("{g},,,,")),
        "{}",
        records[1]
    );
    // LLAMA_ANSWER answers four questions; the failed run is one more row.
    // A group of the history has no labels: `expected` and `correct` empty.
    assert_eq!(records.len(), 1 + 4 + 1, "{text}");
    let state_field = "\"She said \"\"refund\"\",\nthen left.\"";
    assert!(
        records[1].ends_with(&format!(
            ",department,choice,billing,,,0.9561,\
             \"{{\"\"billing\"\":0.9671,\"\"technical\"\":0.0097,\"\"sales\"\":0.0094,\
             \"\"other\"\":0.0138}}\",{state_field}"
        )),
        "{}",
        records[1]
    );
    assert!(
        records[3].contains(",urgency,score,1.6808,,,0.436,"),
        "{}",
        records[3]
    );
    assert!(
        records[4].contains(",angry,noul,0.8319,,,0.8319,,"),
        "{}",
        records[4]
    );
    assert!(
        records[5].ends_with(&format!(
            ",reversed,500,input (8519 tokens) is too large,7,,,,,,,,,{state_field}"
        )),
        "{}",
        records[5]
    );

    // The other labs have their own tables: an empty lab is the header.
    let resp = gw
        .client()
        .get(format!("{gw}/labs/api/image/export?format=csv"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(
        text.starts_with("group_id,run_id,created_at,endpoint,"),
        "{text}"
    );
    assert_eq!(text.matches("\r\n").count(), 1, "{text}");
}
