//! Model help's calls (document-conversion design §7.5, §7.7, D6, D20,
//! WP6) against `documents_model_help.rs`' mock model: a key that may not
//! use the model, the bound on calls in flight, a client that goes away, an
//! async job, and the docling route's options that map onto model help.

use std::time::Duration;

use lmgw_core::config::{hash_api_key, KeyPolicy, ScopeMode};
use serde_json::{json, Value};

use crate::chat_attach_kinds::pdf_bytes;
use crate::common::serve;
use crate::documents_model_help::{
    codes, convert, gateway, has_poppler, request, settings, state_with, Model, PATH,
};

#[tokio::test]
async fn a_key_that_may_not_use_the_model_keeps_tier_0_and_is_told() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    const KEY: &str = "lmgw-example-key";
    let m = Model::start(Duration::ZERO).await;
    let state = state_with(&m, true).await;
    settings(&state, |s| s.auth_enabled = true).await;
    let policy = KeyPolicy {
        scope_mode: ScopeMode::Allow,
        scope_patterns: "example-other*".into(),
        ..Default::default()
    };
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, enabled, scope_mode, scope_patterns,
             budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit, expires_at)
         VALUES ('example-agent', ?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(hash_api_key(KEY))
    .bind(policy.scope_mode.as_str())
    .bind(&policy.scope_patterns)
    .bind(policy.budget_micro)
    .bind(policy.budget_period.as_str())
    .bind(policy.rpm_limit)
    .bind(policy.tpm_limit)
    .bind(policy.concurrency_limit)
    .bind(policy.expires_at.clone())
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    let r = reqwest::Client::new()
        .post(format!("{gw}{PATH}"))
        .bearer_auth(KEY)
        .json(&request(&[Some("Page one"), None], json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let a: Value = r.json().await.unwrap();
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial", "{d}");
    assert_eq!(d["content"], "Page one");
    assert!(
        codes(d).contains(&("model_not_allowed".to_string(), vec![2])),
        "{d}"
    );
    assert!(
        m.bodies().await.is_empty(),
        "the key never reached the model"
    );
}

/// D20: with a bound of two, six pages never have three calls in flight,
/// and the answer says which bound was in use.
#[tokio::test]
async fn a_conversion_keeps_at_most_the_bound_of_calls_in_flight() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let delay = Duration::from_millis(400);
    let m = Model::start(delay).await;
    let (state, gw) = gateway(&m).await;
    settings(&state, |s| s.documents.tool_slots = 2).await;
    let pages = [None, None, None, None, None, None];
    let (_, a) = convert(&gw, &request(&pages, json!({}))).await;
    let d = &a["documents"][0];
    assert_eq!(d["stats"]["parts_by_source"]["vision"], 6, "{d}");
    let by = &d["stats"]["models"][0];
    assert_eq!(
        (&by["in_flight_bound"], &by["bound_from"]),
        (&json!(2), &json!("tool_pool"))
    );
    let arrivals = m.arrivals.lock().unwrap().clone();
    assert_eq!(arrivals.len(), 6);
    // A call is in flight from its arrival for `delay`: no arrival may have
    // two others within the `delay` before it (less a margin for timers).
    let window = delay - Duration::from_millis(100);
    for (i, t) in arrivals.iter().enumerate() {
        let before = arrivals[..i]
            .iter()
            .filter(|e| t.duration_since(**e) < window)
            .count();
        assert!(
            before < 2,
            "call {i} arrived with {before} others in flight"
        );
    }
}

/// A client that goes away: the page waiting for its turn is never sent.
#[tokio::test]
async fn a_dropped_request_sends_no_further_pages() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::from_millis(1500)).await;
    let (state, gw) = gateway(&m).await;
    settings(&state, |s| s.documents.tool_slots = 1).await;
    let body = request(&[None, None, None, None], json!({}));
    let r = gw
        .client()
        .post(format!("{gw}{PATH}"))
        .timeout(Duration::from_millis(700))
        .json(&body)
        .send()
        .await;
    assert!(
        r.is_err(),
        "the client gave up while the first page was read"
    );
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let sent = m.bodies().await.len();
    assert!(
        sent <= 1,
        "{sent} pages were sent after the client had gone"
    );
}

#[tokio::test]
async fn an_async_job_and_the_docling_route_read_pages_the_same_way() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let (state, gw) = gateway(&m).await;
    let body = request(&[Some("Page one"), None], json!({"async": true}));
    let (status, a) = convert(&gw, &body).await;
    assert_eq!(status, 202, "{a}");
    let id = a["job"].as_str().unwrap().to_string();
    if let Some(mut rx) = state.convert_jobs.watch(&id) {
        let _ = tokio::time::timeout(Duration::from_secs(60), async {
            while rx.changed().await.is_ok() {
                if state.convert_jobs.watch(&id).is_none() {
                    break;
                }
            }
        })
        .await;
    }
    let r = gw
        .client()
        .get(format!("{gw}/v1/documents/jobs/{id}/result"))
        .send()
        .await
        .unwrap();
    let a: Value = r.json().await.unwrap();
    let d = &a["documents"][0];
    assert_eq!(d["stats"]["parts_by_source"]["vision"], 1, "{d}");
    assert!(d["content"].as_str().unwrap().contains("# Scanned page"));

    // docling's `pipeline: vlm` is every page; formula enrichment is what
    // the model's prompt asks for on the pages it reads.
    let file = reqwest::multipart::Part::bytes(pdf_bytes(&[Some("Page one"), None]))
        .file_name("example-scan.pdf");
    let form = reqwest::multipart::Form::new()
        .part("files", file)
        .text("pipeline", "vlm")
        .text("do_formula_enrichment", "true");
    let before = m.bodies().await.len();
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/file"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let notes = r.headers()["x-lmgw-option-notes"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        notes.contains("pipeline=mapped(page-reading model on every page)"),
        "{notes}"
    );
    assert!(
        notes.contains("do_formula_enrichment=mapped(page-reading model on the pages it reads)"),
        "{notes}"
    );
    let a: Value = r.json().await.unwrap();
    assert_eq!(a["status"], "success", "{a}");
    assert_eq!(
        m.bodies().await.len() - before,
        2,
        "both pages went to the model"
    );
    assert!(a["document"]["md_content"]
        .as_str()
        .unwrap()
        .contains("E = mc^2"));
}
