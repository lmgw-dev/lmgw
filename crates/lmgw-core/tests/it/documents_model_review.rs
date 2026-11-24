//! WP6's review round on `POST /v1/documents/convert` (document-conversion
//! design §14 WP6), against `documents_model_help.rs`' mock model: a
//! refusal and a finish that is no stop (S1), a file that fails because of
//! the model (S6), the file's deadline in the model stage (S7), a dropped
//! call's gauge, row and connection (S8), a page too large to render whole
//! (S13), the native `missing` and docling's `table_mode: fast` (S5), and
//! the fixed code lists of `scripts/convert-compare.py`. The key's policy is
//! `documents_model_policy.rs`; the fallback's side `documents_model_hold.rs`.

use std::io::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use lmgw_api_types::documents::convert::{ErrorCode, WarningCode};
use lmgw_core::extract::convert::model_help::{prompts, render};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::chat_attach_kinds::pdf_bytes;
use crate::common::serve;
use crate::documents_model_help::{
    codes, convert, gateway, has_poppler, page_rows_of, request, sent, settings, state_on, Model,
    ALIAS, PATH,
};

fn warning<'a>(doc: &'a Value, code: &str) -> Option<&'a Value> {
    doc["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["code"] == code)
}

/// S1: a refusal, and an answer that did not end with a stop, are failed
/// pages: the page keeps the text the tools read.
#[tokio::test]
async fn a_refusal_or_a_finish_that_is_no_stop_keeps_the_page_s_own_text() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let (_state, gw) = gateway(&m).await;
    let body = request(&[Some("Page one")], json!({"model_help": "every_page"}));

    m.will_refuse("I'm sorry, I can't assist with that");
    let (status, a) = convert(&gw, &body).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["content"], "Page one", "the page's exact text is kept");
    assert_eq!(codes(d), [("part_failed".to_string(), vec![1])]);
    let message = warning(d, "part_failed").unwrap()["message"]
        .as_str()
        .unwrap();
    assert!(message.contains("refused to read the page"), "{message}");
    assert!(!message.contains("sorry"), "{message}");
    assert_eq!(d["stats"]["parts_by_source"]["vision"], 0);
    assert_eq!(d["stats"]["model_calls"], 1);

    // Anthropic's word for an answer cut at the context window, as an
    // OpenAI-shaped upstream would relay it.
    m.will_answer("most of the page", "model_context_window_exceeded");
    let (_, a) = convert(&gw, &body).await;
    let d = &a["documents"][0];
    assert_eq!(d["content"], "Page one");
    let message = warning(d, "part_failed").unwrap()["message"]
        .as_str()
        .unwrap();
    assert!(
        message.contains("the finish reason 'model_context_window_exceeded'"),
        "{message}"
    );
    assert!(!message.contains("most of the page"), "{message}");

    // A tool call is no page either.
    m.will_answer("text beside a call", "tool_calls");
    let (_, a) = convert(&gw, &body).await;
    assert_eq!(a["documents"][0]["content"], "Page one");
}

/// S6 (and nit 6): a scan the model's upstream fails for is a 502
/// `model_failed`, not "nothing reads scanned pages"; a 429 from the
/// upstream fails the page by name, with no retry yet.
#[tokio::test]
async fn an_upstream_error_fails_a_scan_as_the_model_s_failure_with_its_calls_kept() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let (_state, gw) = gateway(&m).await;
    m.will_fail(500);
    let (status, a) = convert(&gw, &request(&[None], json!({}))).await;
    let d = &a["documents"][0];
    assert_eq!(
        (status, &d["error"]["code"]),
        (502, &json!("model_failed")),
        "{a}"
    );
    assert_eq!(d["error"]["tool"], Value::Null);
    let message = d["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("the page-reading model did not read page 1"),
        "{message}"
    );
    assert!(!message.contains("not read as text yet"), "{message}");
    // The call was made and is in the file's stats, though the file failed.
    assert_eq!(d["stats"]["model_calls"], 1, "{d}");
    assert_eq!(
        (
            &d["stats"]["models"][0]["alias"],
            &d["stats"]["models"][0]["calls"]
        ),
        (&json!(ALIAS), &json!(1))
    );

    // A rate limit at the upstream: the page fails by name, once.
    m.will_fail(429);
    let before = m.bodies().await.len();
    let (status, a) = convert(&gw, &request(&[Some("Page one"), None], json!({}))).await;
    assert_eq!(status, 200);
    let d = &a["documents"][0];
    assert!(
        codes(d).contains(&("part_failed".to_string(), vec![2])),
        "{d}"
    );
    let message = warning(d, "part_failed").unwrap()["message"]
        .as_str()
        .unwrap();
    assert!(message.contains("429"), "the page fails by name: {message}");
    assert_eq!(m.bodies().await.len() - before, 1, "no retry is built yet");
}

/// S7: the caller's `document_timeout` ends the model stage, not the file:
/// the pages already read and the tools' text are kept, the pages not
/// answered are named, and the file is partial.
///
/// The deadline is derived from this machine's pace now (a load-sensitive
/// failure at 32 test threads, 2026-11-20: under load the tools took more
/// than the 1.5 s a fixed 2.5 s deadline left them before the first 1 s
/// call): a one-page scan is converted twice first, and the shorter time
/// until its call arrived is the tools' time `t` (the first conversion
/// carries one-time costs, the confinement probe and the first connection
/// to the stub; review 3). The deadline is `t` + one call + 1.5 s,
/// so the first page is read when the six-page file's tools take up to
/// 1.5 s more than the one page's, and five calls do not fit when they take
/// up to 1.5 s less.
#[tokio::test]
async fn the_deadline_in_the_model_stage_keeps_what_was_read() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    const CALL: Duration = Duration::from_millis(1000);
    let m = Model::start(CALL).await;
    let (state, gw) = gateway(&m).await;
    // One call at a time: five scan pages are five seconds of calls.
    settings(&state, |s| s.documents.tool_slots = 1).await;
    let post = |pages: &[Option<&str>], timeout: Option<String>| {
        let mut form = reqwest::multipart::Form::new().part(
            "files",
            reqwest::multipart::Part::bytes(pdf_bytes(pages)).file_name("example-scan.pdf"),
        );
        if let Some(t) = timeout {
            form = form.text("document_timeout", t);
        }
        gw.client()
            .post(format!("{gw}/v1/convert/file"))
            .multipart(form)
            .send()
    };
    let mut tools = Duration::MAX;
    for i in 0..2 {
        let started = std::time::Instant::now();
        let r = post(&[None], None).await.unwrap();
        assert_eq!(r.status().as_u16(), 200);
        tools = tools.min(m.arrivals.lock().unwrap()[i].duration_since(started));
    }
    let before = (
        m.bodies().await.len(),
        page_rows_of(&state, ALIAS, 2).await.len(),
    );
    // In tenths of a second, as the request sends it.
    let tenths = (tools + CALL + Duration::from_millis(1500))
        .as_millis()
        .div_ceil(100) as u64;
    let deadline = Duration::from_millis(tenths * 100);
    let pages = [Some("Page one"), None, None, None, None, None];
    let started = std::time::Instant::now();
    let r = post(&pages, Some(format!("{}.{}", tenths / 10, tenths % 10)))
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let a: Value = r.json().await.unwrap();
    let calls: Vec<Duration> = m.arrivals.lock().unwrap()[2..]
        .iter()
        .map(|t| t.saturating_duration_since(started))
        .collect();
    assert!(
        started.elapsed() < deadline + Duration::from_secs(2),
        "the stage ended at the deadline ({deadline:?}): {:?}",
        started.elapsed()
    );
    assert_eq!(a["status"], "partial_success", "{a}");
    let md = a["document"]["md_content"].as_str().unwrap();
    assert!(md.contains("Page one"), "the tools' text is kept: {md}");
    assert!(
        md.contains("# Scanned page"),
        "a page the model read is kept (tools {tools:?}, deadline {deadline:?}, calls at \
         {calls:?}): {a}"
    );
    let errors = a["errors"].as_array().unwrap();
    let late = errors
        .iter()
        .filter_map(|e| e["error_message"].as_str())
        .find(|e| e.contains("document_timeout of "))
        .unwrap_or_else(|| panic!("the unanswered pages are named: {errors:?}"));
    assert!(
        late.contains("the page-reading model did not read page"),
        "{late}"
    );
    // The calls that were answered are rows; the one cut at the deadline,
    // if one was in flight, is a 499 row.
    let rows = page_rows_of(&state, ALIAS, before.1 + 2).await;
    let answered = rows.iter().filter(|r| r.status == 200).count() - before.1;
    let dropped = rows.iter().filter(|r| r.status == 499).count();
    assert!(answered >= 1 && dropped <= 1, "{rows:?}");
    assert!(
        m.bodies().await.len() - before.0 < 5,
        "no page was sent after the deadline (calls at {calls:?})"
    );

    // Nothing read at all by the deadline: the file's own deadline code.
    let form = reqwest::multipart::Form::new()
        .part(
            "files",
            reqwest::multipart::Part::bytes(pdf_bytes(&[None])).file_name("example-scan.pdf"),
        )
        .text("document_timeout", "0.3");
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/file"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    let a: Value = r.json().await.unwrap();
    assert_eq!(a["status"], "failure", "{a}");
    assert_eq!(a["errors"][0]["category"], "timeout", "{a}");
}

/// An upstream that takes a chat request and never answers it: how many
/// chat requests arrived, and how many of their connections were closed by
/// the client. Anything else (a catalog read) is answered 404.
pub(crate) async fn silent_upstream() -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (arrived, closed) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (a, c) = (arrived.clone(), closed.clone());
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let (a, c) = (a.clone(), c.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 64 * 1024];
                let Ok(n) = sock.read(&mut buf).await else {
                    return;
                };
                if !buf[..n].starts_with(b"POST /v1/chat/completions") {
                    let _ = sock
                        .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                        .await;
                    return;
                }
                a.fetch_add(1, Ordering::SeqCst);
                // The rest of the body, then nothing: the read ends when
                // the client closes its side.
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                c.fetch_add(1, Ordering::SeqCst);
            });
        }
    });
    (base, arrived, closed)
}

pub(crate) async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what}");
}

/// S8: a page call dropped with its conversion (the client went away)
/// closes its upstream connection, leaves the in-flight gauge at zero and
/// writes its row, as a public request whose client left does.
#[tokio::test]
async fn a_dropped_page_call_closes_its_connection_its_gauge_and_leaves_a_row() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let (base, arrived, closed) = silent_upstream().await;
    let state = state_on(&base, true).await;
    let gw = serve(state.clone()).await;
    let body = request(&[None], json!({}));
    let client = gw.client();
    let url = format!("{gw}{PATH}");
    let call = tokio::spawn(async move { client.post(url).json(&body).send().await });
    until("the page call never reached the upstream", || {
        arrived.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(state.telemetry.stats().active_requests >= 1);
    // The client goes away mid-call.
    call.abort();
    let _ = call.await;
    until(
        "the upstream connection of the dropped call stayed open",
        || closed.load(Ordering::SeqCst) == 1,
    )
    .await;
    let rows = page_rows_of(&state, ALIAS, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (rows[0].status, rows[0].error_kind.as_deref()),
        (499, Some("client_disconnected"))
    );
    assert_eq!(rows[0].ingress_proto, "documents-page");
    assert!(rows[0].key_id.is_some(), "the row is the caller's key's");
    assert_eq!(rows[0].prompt_tokens, None, "nothing was answered");
    let telemetry = &state.telemetry;
    until(
        "the in-flight gauge still counts a call that is over",
        || telemetry.stats().active_requests == 0,
    )
    .await;
    assert_eq!(arrived.load(Ordering::SeqCst), 1);
}

/// A one-page PDF whose page is `side` points square, with no text.
fn square_pdf(side: u32) -> Vec<u8> {
    let stream = "0.9 g 10 10 50 50 re f";
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {side} {side}] /Contents 4 0 R >>"),
        format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ),
    ];
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

/// S13: a page too large to render whole at 150 dpi is sent scaled down to
/// `PAGE_MAX_PIXELS`, and the answer says so.
#[tokio::test]
async fn a_page_past_the_pixel_limit_is_sent_scaled_down_with_a_note() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let (_state, gw) = gateway(&m).await;
    // 7000 pt square: 14,583 pixels a side at 150 dpi, 212 million in all.
    let body = json!({"files": [{
        "name": "example-poster.pdf",
        "data_base64": base64::engine::general_purpose::STANDARD.encode(square_pdf(7000)),
    }]});
    let (status, a) = convert(&gw, &body).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    let note = warning(d, "page_scaled").expect("the scaled page is named");
    assert_eq!(note["parts"], json!([1]));
    let message = note["message"].as_str().unwrap();
    assert!(message.starts_with("page 1 would be 212"), "{message}");
    assert!(
        message.contains(&format!("PAGE_MAX_PIXELS ({})", render::PAGE_MAX_PIXELS)),
        "{message}"
    );
    // What was sent: a PNG within the limit (its IHDR says its size).
    let bodies = m.bodies().await;
    let (_, image) = sent(&bodies[0]);
    let png = base64::engine::general_purpose::STANDARD
        .decode(image.trim_start_matches("data:image/png;base64,"))
        .unwrap();
    let side = |at: usize| u64::from(u32::from_be_bytes(png[at..at + 4].try_into().unwrap()));
    let (w, h) = (side(16), side(20));
    assert!(w * h <= render::PAGE_MAX_PIXELS, "{w} x {h}");
    assert!(
        w.max(h) > 2000,
        "scaled to the limit, not below it: {w} x {h}"
    );

    // A page of ordinary size is sent as before, without a note.
    let (_, a) = convert(&gw, &request(&[None], json!({}))).await;
    assert_eq!(a["documents"][0]["warnings"], json!([]));
}

/// S5: the native `missing` sends only the pages nothing read, and the
/// docling route's `table_mode: fast` asks for it once a model is chosen.
#[tokio::test]
async fn model_help_missing_and_docling_s_fast_tables_send_unread_pages_only() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let (_state, gw) = gateway(&m).await;
    let pages = [Some("Page one"), None];
    let (status, a) = convert(&gw, &request(&pages, json!({"model_help": "missing"}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    let bodies = m.bodies().await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(sent(&bodies[0]).0, prompts::IMAGE_PROMPT);

    let form = reqwest::multipart::Form::new()
        .part(
            "files",
            reqwest::multipart::Part::bytes(pdf_bytes(&pages)).file_name("example-scan.pdf"),
        )
        .text("table_mode", "fast");
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
        notes.contains(
            "table_mode=mapped(lmgw tables and a chosen page-reading model on unread pages only)"
        ),
        "{notes}"
    );
    assert_eq!(
        m.bodies().await.len(),
        2,
        "one more call, for the unread page"
    );
}

/// Every code a file or a warning can carry, as the wire writes it. The
/// matches are exhaustive: a new code does not compile until it is listed.
fn wire_codes() -> (Vec<String>, Vec<String>) {
    use ErrorCode as E;
    use WarningCode as W;
    let errors = [
        E::ToolMissing,
        E::NoReader,
        E::Unsupported,
        E::Unreadable,
        E::InvalidBase64,
        E::PagesOutOfRange,
        E::OutputTooLarge,
        E::DeadlineExceeded,
        E::Interrupted,
        E::SourceRefused,
        E::SourceUnreachable,
        E::SourceTimeout,
        E::SourceTooLarge,
        E::SourceHttpStatus,
        E::ModelNotChosen,
        E::ModelHeld,
        E::ModelNotAllowed,
        E::ModelFailed,
        E::ToolNotStarted,
        E::TooDeep,
        E::MemoryLimit,
    ];
    for e in errors {
        match e {
            E::ToolMissing | E::NoReader | E::Unsupported | E::Unreadable => {}
            E::InvalidBase64 | E::PagesOutOfRange | E::OutputTooLarge => {}
            E::DeadlineExceeded | E::Interrupted | E::SourceRefused => {}
            E::SourceUnreachable | E::SourceTimeout | E::SourceTooLarge => {}
            E::SourceHttpStatus | E::ModelHeld | E::ModelNotAllowed | E::ModelFailed => {}
            E::ModelNotChosen | E::ToolNotStarted | E::TooDeep => {}
            E::MemoryLimit => {}
        }
    }
    let warnings = [
        W::ToolMissing,
        W::NoReader,
        W::ModelNotChosen,
        W::ModelHeld,
        W::ModelNotAllowed,
        W::PartFailed,
        W::ModelCannotSee,
        W::ModelAnswerMalformed,
        W::PageScaled,
        W::HeaderRemoved,
        W::EquationsDropped,
        W::PicturesPartial,
        W::PictureText,
        W::PicturesLeftOut,
        W::PictureContextCut,
        W::FormulasPartial,
        W::OcrLanguageMissing,
        W::OcrLowConfidence,
        W::OcrPageReduced,
        W::OcrPageTurned,
        W::NotYet,
        W::PagesOutOfRange,
        W::PagesNotApplicable,
        W::OptionNotApplicable,
        W::MemoryLimitWidened,
        W::ImageNotDecoded,
    ];
    for w in warnings {
        match w {
            W::ToolMissing | W::NoReader | W::ModelNotChosen | W::ModelHeld => {}
            W::ModelNotAllowed | W::PartFailed | W::ModelCannotSee => {}
            W::ModelAnswerMalformed | W::PageScaled | W::HeaderRemoved => {}
            W::EquationsDropped | W::PicturesPartial | W::FormulasPartial => {}
            W::PictureText | W::PicturesLeftOut | W::OcrLowConfidence => {}
            W::PictureContextCut => {}
            W::OcrPageReduced | W::OcrPageTurned => {}
            W::OcrLanguageMissing | W::NotYet | W::PagesOutOfRange => {}
            W::PagesNotApplicable | W::OptionNotApplicable => {}
            W::MemoryLimitWidened | W::ImageNotDecoded => {}
        }
    }
    let said = |v: Value| v.as_str().unwrap().to_string();
    (
        errors
            .iter()
            .map(|e| said(serde_json::to_value(e).unwrap()))
            .collect(),
        warnings
            .iter()
            .map(|w| said(serde_json::to_value(w).unwrap()))
            .collect(),
    )
}

/// The names in one of `scripts/convert-compare.py`'s fixed tuples.
fn script_list(source: &str, name: &str) -> Vec<String> {
    let start = source
        .find(&format!("\n{name} = ("))
        .unwrap_or_else(|| panic!("{name} is not in the script"));
    let rest = &source[start..];
    let list = &rest[..rest.find(')').unwrap()];
    list.split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// `scripts/convert-compare.py` reports a file's error and warning codes from fixed
/// lists and writes any other as `other`: every code lmgw can answer must
/// be in them, the model codes of this round and the text-recognition,
/// picture, layout-model and picture-description codes of the work packages
/// beside it included.
#[test]
fn the_on_site_script_knows_every_code_the_route_can_answer() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/convert-compare.py"
    );
    let source = std::fs::read_to_string(path).unwrap();
    let (errors, warnings) = wire_codes();
    let known = script_list(&source, "LMGW_ERRORS");
    for code in &errors {
        assert!(known.contains(code), "LMGW_ERRORS lacks {code}");
    }
    let known = script_list(&source, "LMGW_WARNINGS");
    for code in &warnings {
        assert!(known.contains(code), "LMGW_WARNINGS lacks {code}");
    }
    assert_eq!(errors.len(), 21);
    // The models it reports are read from GET /api/settings-full by the
    // names the wire has (`documents_` and the setting's name).
    let full = serde_json::to_value(lmgw_api_types::SettingsFull::default()).unwrap();
    for (word, key) in [
        ("vision", "page_reader"),
        ("picture", "picture_alias"),
        ("asr", "asr_alias"),
    ] {
        assert!(
            source.contains(&format!("(\"{word}\", \"{key}\")")),
            "the script reads {word} as {key}"
        );
        assert!(
            full.get(format!("documents_{key}")).is_some(),
            "settings-full has documents_{key}"
        );
    }
}
