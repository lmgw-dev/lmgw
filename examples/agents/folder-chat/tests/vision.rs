//! Reading PDF pages with a vision model: which pages are read and how, the
//! cache, the refresh pass, failures and holds — the sync driven directly with
//! a fake `pdftotext`, a fake `pdftoppm` and the fake lmgw's chat route; and
//! once end to end, with the real poppler tools, through the MCP face.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use base64::Engine;
use common::fake::{gateway, serve, structure_reading, Fake, Shared, OCR_READING};
use common::*;
use folder_chat::gateway::GPU_HOLD_MESSAGE;
use folder_chat::index::IndexDir;
use folder_chat::sync::{AbortKind, SyncError, SyncEvent, SyncOptions, TextlessPages};
use folder_chat::vision::{VisionOptions, OCR_PROMPT, STRUCTURE_PROMPT};
use folder_chat::FolderChat;
use quickdoc_core::embed::FixtureEmbedder;
use quickdoc_core::store;
use serde_json::{json, Value};

/// A wage slip page as `pdftotext -layout` gives it: four columns.
const TABLE_PAGE: &str = "Wage slip September\n\
    Item          Month        Year to date     Rate\n\
    Gross         3.250,00     29.250,00        1,00\n\
    Tax             715,00     6.435,00         0,22\n";

/// Prose: never table-like.
const PLAIN_PAGE: &str = "Heron survey notes, written as plain prose.\n";

const ALIAS: &str = "vision-fixture";

/// A folder with one PDF whose `pdftotext` output is `pages` (a trailing
/// empty string is the real tool's closing form feed), its index, and the
/// fakes a sync is pointed at.
struct Env {
    tmp: tempfile::TempDir,
    idx: IndexDir,
    base: String,
    fake: Shared,
    text: FakePdftotext,
    ppm: FakePdftoppm,
}

async fn env(fake: Fake, pages: &[&str]) -> Env {
    let (base, fake) = serve(fake).await;
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "notes.md", "# Notes\n\nNothing about birds.\n");
    write(
        tmp.path(),
        "scan.pdf",
        b"%PDF-1.4 the fakes read nothing of it",
    );
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    Env {
        tmp,
        idx,
        base,
        fake,
        text: fake_pdftotext("1.0-fake", pages, None),
        ppm: fake_pdftoppm(false),
    }
}

impl Env {
    fn opts(&self, alias: Option<&str>, every_page: bool) -> SyncOptions {
        SyncOptions {
            pdftotext: self.text.path.clone(),
            pdftoppm: self.ppm.path.clone(),
            vision: alias.map(|a| VisionOptions {
                model: a.to_string(),
                every_page,
                gateway: gateway(&self.base),
            }),
            ..SyncOptions::new(400, None)
        }
    }

    fn vision_calls(&self) -> usize {
        self.fake.vision_calls.load(Ordering::SeqCst)
    }

    fn vision_body(&self, i: usize) -> Value {
        self.fake.vision_bodies.lock().unwrap()[i].clone()
    }
}

async fn chunks(idx: &IndexDir, url: &str) -> Vec<store::Chunk> {
    let c = store::get_corpus_by_id(idx.pool(), "folder@1")
        .await
        .unwrap()
        .unwrap();
    match store::list_documents(idx.pool(), c.id)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.url == url)
    {
        Some(d) => store::list_chunks(idx.pool(), d.id).await.unwrap(),
        None => Vec::new(),
    }
}

fn headings(chunks: &[store::Chunk]) -> Vec<&str> {
    chunks.iter().map(|c| c.heading_path.as_str()).collect()
}

/// The page-reading cache: page, alias, mode, status.
async fn vision_rows(idx: &IndexDir) -> Vec<(i64, String, String, String)> {
    sqlx::query_as(
        "SELECT page, model, mode, status FROM folder_chat_reading ORDER BY page, model, mode",
    )
    .fetch_all(idx.pool())
    .await
    .unwrap()
}

fn aborted(out: &Outcome) -> (AbortKind, String) {
    match &out.result {
        Err(SyncError::Aborted { kind, reason, .. }) => (*kind, reason.clone()),
        other => panic!("expected a stop, got {:?}", other.as_ref().map(|_| ())),
    }
}

/// A sync with a vision model on another gateway.
fn with_gateway(opts: SyncOptions, base: &str, alias: &str) -> SyncOptions {
    SyncOptions {
        vision: Some(VisionOptions {
            model: alias.to_string(),
            every_page: false,
            gateway: gateway(base),
        }),
        ..opts
    }
}

fn readings(events: &[SyncEvent]) -> Vec<(String, u32, u32)> {
    events
        .iter()
        .filter_map(|e| match e {
            SyncEvent::Reading { file, page, of } => Some((file.clone(), *page, *of)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_scanned_page_is_read_from_its_image_alone_and_indexed_under_its_heading() {
    let e = env(Fake::default(), &["", PLAIN_PAGE, ""]).await;
    let embed = Counting::new();
    let out = sync_opts(&e.idx, embed.clone(), &e.opts(Some(ALIAS), false)).await;
    let r = out.report();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!((r.vision_pages_read, r.vision_pages_reused), (1, 0));
    assert_eq!(r.vision_model.as_deref(), Some(ALIAS));
    assert_eq!(r.vision_dpi, 150);
    assert!(
        r.vision_rule.contains("TABLE_MIN_COLUMNS (4)"),
        "{}",
        r.vision_rule
    );
    assert!(
        r.notes.iter().any(
            |n| n.contains("the vision model vision-fixture read 1 PDF page(s)")
                && n.contains("1 without text")
        ),
        "{:?}",
        r.notes
    );
    // Announced before it was read, with the page count.
    assert_eq!(readings(&out.events), [("scan.pdf".to_string(), 1, 2)]);
    assert_eq!(e.ppm.calls(), ["-r 150 -png -singlefile -f 1 -l 1 -"]);

    // One request: the image and the OCR prompt, temperature 0, not
    // streamed, not capped.
    assert_eq!(e.vision_calls(), 1);
    let body = e.vision_body(0);
    assert_eq!(body["model"], ALIAS);
    assert_eq!(body["temperature"], 0);
    assert_eq!(body["stream"], false);
    assert!(body.get("max_tokens").is_none(), "{body}");
    let parts = &body["messages"][0]["content"];
    assert_eq!(
        parts[0]["image_url"]["url"],
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(TINY_PNG)
        )
    );
    assert_eq!(parts[1]["text"], OCR_PROMPT);

    // Indexed under its heading, the fence stripped, found by BM25, and no
    // longer a page without text.
    let got = chunks(&e.idx, "scan.pdf").await;
    assert_eq!(headings(&got), ["page 2", "page 1, read by vision-fixture"]);
    assert_eq!(got[1].payload, OCR_READING);
    assert_eq!(fts_count(&e.idx, "kittiwake").await, 1);
    assert!(
        r.pdf_pages_without_text.is_empty(),
        "{:?}",
        r.pdf_pages_without_text
    );
    assert_eq!(
        vision_rows(&e.idx).await,
        [(1, ALIAS.into(), "ocr".into(), "read".into())]
    );

    // A re-sync reads nothing: the file is current.
    let again = sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false)).await;
    assert_eq!(again.report().unchanged, 2);
    assert_eq!(e.vision_calls(), 1);
    assert!(readings(&again.events).is_empty());

    // Taken through again (here: its page record is missing), it reads from
    // the cache — no request, no Reading event, no embedding.
    sqlx::query("DELETE FROM folder_chat_pdf_pages")
        .execute(e.idx.pool())
        .await
        .unwrap();
    let embed = Counting::new();
    let out = sync_opts(&e.idx, embed.clone(), &e.opts(Some(ALIAS), false)).await;
    let r = out.report();
    assert_eq!(r.pdf_pages_recorded_files, 1);
    assert_eq!((r.vision_pages_read, r.vision_pages_reused), (0, 1));
    assert_eq!(e.vision_calls(), 1, "a re-read page comes from the cache");
    assert!(readings(&out.events).is_empty());
    assert_eq!(embed.calls(), 0, "every vector was kept");
    assert!(out.report().pdf_pages_without_text.is_empty());
}

#[tokio::test]
async fn a_table_page_is_read_with_its_text_and_a_plain_page_only_with_every_page_on() {
    let e = env(Fake::default(), &[TABLE_PAGE, PLAIN_PAGE, ""]).await;
    let out = sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false)).await;
    let r = out.report();
    assert_eq!(r.vision_pages_read, 1);
    assert_eq!(e.ppm.calls(), ["-r 150 -png -singlefile -f 1 -l 1 -"]);
    // The structure prompt, carrying the page's own text.
    let text = e.vision_body(0)["messages"][0]["content"][1]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text.starts_with(STRUCTURE_PROMPT), "{text}");
    assert!(text.ends_with(TABLE_PAGE.trim_end()), "{text}");
    let got = chunks(&e.idx, "scan.pdf").await;
    assert_eq!(
        headings(&got),
        ["page 1", "page 2", "page 1, read by vision-fixture"]
    );
    assert_eq!(got[2].payload, structure_reading(TABLE_PAGE));
    assert!(got[0].payload.contains("29.250,00"), "the text layer stays");
    assert_eq!(
        vision_rows(&e.idx).await,
        [(1, ALIAS.into(), "structure".into(), "read".into())]
    );

    // Every page on: a settings change, so the PDF is refreshed — the table
    // page from the cache, the plain page read with its text, and only the
    // new reading embedded.
    let embed = Counting::new();
    let out = sync_opts(&e.idx, embed.clone(), &e.opts(Some(ALIAS), true)).await;
    let r = out.report();
    assert!(r.vision_every_page);
    assert_eq!(r.vision_refreshed_files, 1);
    assert_eq!((r.vision_pages_read, r.vision_pages_reused), (1, 1));
    assert_eq!(readings(&out.events), [("scan.pdf".to_string(), 2, 2)]);
    let text = e.vision_body(1)["messages"][0]["content"][1]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text.ends_with(PLAIN_PAGE.trim_end()), "{text}");
    assert_eq!(embed.texts(), 1, "only the new reading was embedded");
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("indexed with other vision settings")
                && n.contains("every_page=false")
                && n.contains("every_page=true")),
        "{:?}",
        r.notes
    );
    let got = chunks(&e.idx, "scan.pdf").await;
    assert_eq!(
        headings(&got),
        [
            "page 1",
            "page 2",
            "page 1, read by vision-fixture",
            "page 2, read by vision-fixture"
        ]
    );
}

#[tokio::test]
async fn changing_the_alias_reads_again_and_clearing_it_keeps_the_cache() {
    let e = env(Fake::default(), &["", PLAIN_PAGE, ""]).await;
    sync_opts(&e.idx, Counting::new(), &e.opts(Some("vision-a"), false))
        .await
        .report();
    assert_eq!(e.vision_calls(), 1);

    // Another alias: read again, under its own name; the old reading leaves
    // the index's text but stays cached.
    let out = sync_opts(&e.idx, Counting::new(), &e.opts(Some("vision-b"), false)).await;
    let r = out.report();
    assert_eq!(r.vision_refreshed_files, 1);
    assert_eq!((r.vision_pages_read, r.vision_pages_reused), (1, 0));
    assert_eq!(e.vision_calls(), 2);
    assert_eq!(
        headings(&chunks(&e.idx, "scan.pdf").await),
        ["page 2", "page 1, read by vision-b"]
    );
    assert_eq!(fts_count(&e.idx, "kittiwake").await, 1);
    assert_eq!(
        vision_rows(&e.idx).await,
        [
            (1, "vision-a".into(), "ocr".into(), "read".into()),
            (1, "vision-b".into(), "ocr".into(), "read".into())
        ]
    );

    // No alias: the reading's chunk goes, nothing is embedded or read, and
    // the page is a page without text again.
    let embed = Counting::new();
    let out = sync_opts(&e.idx, embed.clone(), &e.opts(None, false)).await;
    let r = out.report();
    assert_eq!(r.vision_refreshed_files, 1);
    assert_eq!(r.vision_model, None);
    assert_eq!(e.vision_calls(), 2);
    assert_eq!(embed.calls(), 0);
    assert_eq!(headings(&chunks(&e.idx, "scan.pdf").await), ["page 2"]);
    assert_eq!(fts_count(&e.idx, "kittiwake").await, 0);
    assert_eq!(
        r.pdf_pages_without_text,
        [TextlessPages {
            path: "scan.pdf".into(),
            pages: 2,
            textless: vec![1],
        }]
    );
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("set a vision model (vision_model)")),
        "{:?}",
        r.notes
    );
    // Settled: the next sync reads nothing.
    let quiet = sync_opts(&e.idx, Counting::new(), &e.opts(None, false)).await;
    assert_eq!(quiet.report().unchanged, 2);
    assert_eq!(quiet.report().vision_refreshed_files, 0);

    // The cache is kept while the file is: the first alias again costs no
    // model call.
    let out = sync_opts(&e.idx, Counting::new(), &e.opts(Some("vision-a"), false)).await;
    assert_eq!(out.report().vision_pages_reused, 1);
    assert_eq!(e.vision_calls(), 2);
    assert_eq!(
        headings(&chunks(&e.idx, "scan.pdf").await),
        ["page 2", "page 1, read by vision-a"]
    );
}

#[tokio::test]
async fn a_failure_that_would_repeat_is_cached_and_not_retried_until_the_alias_changes() {
    let e = env(Fake::default(), &["", PLAIN_PAGE, ""]).await;
    e.fake.vision_empty.store(true, Ordering::SeqCst);
    let out = sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false)).await;
    let r = out.report();
    assert_eq!(r.new_files, 2, "the file is indexed all the same");
    let err = r.errors.iter().find(|e| e.file == "scan.pdf").unwrap();
    for part in ["page 1", ALIAS, "no text", "not read again until"] {
        assert!(err.message.contains(part), "{part} in {}", err.message);
    }
    assert!(out.events.iter().any(|e| matches!(
        e,
        SyncEvent::ReadingFailed { file, page: 1, model, cached: true, .. }
            if file == "scan.pdf" && model == ALIAS
    )));
    assert_eq!(headings(&chunks(&e.idx, "scan.pdf").await), ["page 2"]);
    assert_eq!(
        vision_rows(&e.idx).await,
        [(1, ALIAS.into(), "ocr".into(), "failed".into())]
    );
    let failed = &r.vision_pages_failed;
    assert_eq!(failed.len(), 1);
    assert_eq!((failed[0].path.as_str(), failed[0].page), ("scan.pdf", 1));
    assert_eq!(failed[0].model, ALIAS);
    assert_eq!(r.pdf_pages_without_text[0].textless, [1]);

    // The model would answer now, but the failure is cached: a pass over the
    // file (here its page record is missing) asks nothing, and still reports
    // it — as does a sync that does not read the file at all.
    e.fake.vision_empty.store(false, Ordering::SeqCst);
    sqlx::query("DELETE FROM folder_chat_pdf_pages")
        .execute(e.idx.pool())
        .await
        .unwrap();
    let out = sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false)).await;
    let r = out.report();
    assert_eq!(r.pdf_pages_recorded_files, 1);
    assert_eq!(e.vision_calls(), 1, "a cached failure is not retried");
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.vision_pages_failed.len(), 1);
    let quiet = sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false)).await;
    let r = quiet.report();
    assert_eq!(r.unchanged, 2);
    assert_eq!(r.vision_pages_failed.len(), 1);
    assert!(
        r.notes.iter().any(|n| n
            .contains("stays unread until the file, the vision alias or the prompt")
            && n.contains("scan.pdf page 1")),
        "{:?}",
        r.notes
    );

    // Another alias is another key: read.
    let out = sync_opts(&e.idx, Counting::new(), &e.opts(Some("vision-b"), false)).await;
    let r = out.report();
    assert_eq!(r.vision_pages_read, 1);
    assert!(r.vision_pages_failed.is_empty());
    assert!(r.pdf_pages_without_text.is_empty());
    assert_eq!(fts_count(&e.idx, "kittiwake").await, 1);
}

#[tokio::test]
async fn a_page_that_cannot_be_rendered_is_cached_and_a_missing_pdftoppm_is_retried() {
    let e = env(Fake::default(), &["", PLAIN_PAGE, ""]).await;
    let broken = fake_pdftoppm(true);
    let opts = SyncOptions {
        pdftoppm: broken.path.clone(),
        ..e.opts(Some(ALIAS), false)
    };
    let r = sync_opts(&e.idx, Counting::new(), &opts).await;
    let f = &r.report().vision_pages_failed[0];
    assert!(
        f.reason.contains("pdftoppm failed") && f.reason.contains("the fake cannot render"),
        "{}",
        f.reason
    );
    assert_eq!(e.vision_calls(), 0, "nothing to show the model");
    // Cached: taken through again, it is not rendered again.
    sqlx::query("DELETE FROM folder_chat_pdf_pages")
        .execute(e.idx.pool())
        .await
        .unwrap();
    sync_opts(&e.idx, Counting::new(), &opts).await.report();
    assert_eq!(broken.calls().len(), 1);

    // Not installed at all: nothing is cached, a note says so, and the next
    // sync — with a pdftoppm — reads the page.
    let e = env(Fake::default(), &["", PLAIN_PAGE, ""]).await;
    let missing = SyncOptions {
        pdftoppm: e.tmp.path().join("no-such-pdftoppm"),
        ..e.opts(Some(ALIAS), false)
    };
    let out = sync_opts(&e.idx, Counting::new(), &missing).await;
    let r = out.report();
    assert!(
        r.errors
            .iter()
            .any(|e| e.message.contains("poppler-utils") && e.message.contains("next sync")),
        "{:?}",
        r.errors
    );
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("pdftoppm is not installed")),
        "{:?}",
        r.notes
    );
    assert!(vision_rows(&e.idx).await.is_empty());
    assert!(r.vision_pages_failed.is_empty());
    let r = sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false)).await;
    assert_eq!(r.report().vision_pages_read, 1);
    assert_eq!(fts_count(&e.idx, "kittiwake").await, 1);
}

#[tokio::test]
async fn a_server_error_or_an_unreachable_gateway_stops_the_sync() {
    let e = env(Fake::default(), &["", PLAIN_PAGE, ""]).await;
    e.fake.vision_status.store(500, Ordering::SeqCst);
    let out = sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false)).await;
    let (kind, reason) = aborted(&out);
    assert_eq!(kind, AbortKind::Vision);
    for part in [
        ALIAS,
        "scan.pdf, page 1",
        "projector ran out of memory",
        "next sync",
    ] {
        assert!(reason.contains(part), "{part} in {reason}");
    }
    assert!(vision_rows(&e.idx).await.is_empty(), "nothing cached");
    assert_eq!(e.vision_calls(), 1, "one call, not one per page");

    // An unreachable gateway is not about the page either.
    let closed = with_gateway(e.opts(None, false), "http://127.0.0.1:9/v1", ALIAS);
    let (kind, reason) = aborted(&sync_opts(&e.idx, Counting::new(), &closed).await);
    assert_eq!(kind, AbortKind::Vision);
    assert!(reason.contains("cannot reach lmgw"), "{reason}");
}

#[tokio::test]
async fn a_hold_mid_file_stores_nothing_of_that_reading_and_keeps_the_pages_before_it() {
    // A scanned page, then a table page whose reading is held.
    let e = env(
        Fake {
            vision_hold_from: Some(1),
            ..Fake::default()
        },
        &["", TABLE_PAGE, ""],
    )
    .await;
    let out = sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false)).await;
    let (kind, reason) = aborted(&out);
    assert_eq!(kind, AbortKind::GpuHold);
    assert!(reason.starts_with(GPU_HOLD_MESSAGE), "{reason}");
    assert!(reason.contains("scan.pdf, page 2"), "{reason}");
    assert!(matches!(
        out.events.last(),
        Some(SyncEvent::Aborted {
            kind: AbortKind::GpuHold,
            ..
        })
    ));
    // Page 1's reading is cached; nothing of page 2's; no chunk yet.
    assert_eq!(
        vision_rows(&e.idx).await,
        [(1, ALIAS.into(), "ocr".into(), "read".into())]
    );
    assert!(chunks(&e.idx, "scan.pdf").await.is_empty());
    assert_eq!(fts_count(&e.idx, "gross").await, 0);

    // Released (another gateway without the hold): page 1 from the cache,
    // page 2 read.
    let (base, fake) = serve(Fake::default()).await;
    let r = sync_opts(
        &e.idx,
        Counting::new(),
        &with_gateway(e.opts(None, false), &base, ALIAS),
    )
    .await;
    let r = r.report();
    assert_eq!((r.vision_pages_read, r.vision_pages_reused), (1, 1));
    assert_eq!(fake.vision_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        headings(&chunks(&e.idx, "scan.pdf").await),
        [
            "page 2",
            "page 1, read by vision-fixture",
            "page 2, read by vision-fixture"
        ]
    );
    assert!(r.pdf_pages_without_text.is_empty());
}

#[tokio::test]
async fn a_refresh_stopped_by_a_timeout_or_a_hold_is_taken_through_on_the_next_sync() {
    let e = env(Fake::default(), &["", PLAIN_PAGE, ""]).await;
    sync_opts(&e.idx, Counting::new(), &e.opts(Some("vision-a"), false))
        .await
        .report();

    // The alias changes, and pdftotext times out on the refresh: the file
    // stays as it was, and the sync itself completes.
    let slow = fake_pdftotext("1.0-fake", &[], Some(30));
    let timing_out = SyncOptions {
        pdftotext: slow.path.clone(),
        pdf_timeout: Duration::from_millis(300),
        ..e.opts(Some("vision-b"), false)
    };
    let r = sync_opts(&e.idx, Counting::new(), &timing_out).await;
    let r = r.report();
    assert!(
        r.errors
            .iter()
            .any(|e| e.file == "scan.pdf" && e.message.contains("next sync tries again")),
        "{:?}",
        r.errors
    );
    assert_eq!(r.vision_refreshed_files, 0);
    assert_eq!(
        headings(&chunks(&e.idx, "scan.pdf").await),
        ["page 2", "page 1, read by vision-a"]
    );

    // A hold stops the next try part-way — after the file was marked.
    let (held_base, _) = serve(Fake {
        vision_hold_from: Some(0),
        ..Fake::default()
    })
    .await;
    let held = with_gateway(e.opts(None, false), &held_base, "vision-b");
    assert_eq!(
        aborted(&sync_opts(&e.idx, Counting::new(), &held).await).0,
        AbortKind::GpuHold
    );

    // And the next sync does take it through.
    let r = sync_opts(&e.idx, Counting::new(), &e.opts(Some("vision-b"), false)).await;
    let r = r.report();
    assert_eq!(r.vision_refreshed_files, 1);
    assert_eq!(r.vision_pages_read, 1);
    assert_eq!(
        headings(&chunks(&e.idx, "scan.pdf").await),
        ["page 2", "page 1, read by vision-b"]
    );
    let quiet = sync_opts(&e.idx, Counting::new(), &e.opts(Some("vision-b"), false)).await;
    assert_eq!(quiet.report().vision_refreshed_files, 0);
}

#[tokio::test]
async fn a_fallback_answer_stops_the_sync_whatever_its_status_and_is_not_stored() {
    for status in [0u16, 502, 400] {
        let e = env(
            Fake {
                vision_fallback: Some("openai/gpt-5".into()),
                ..Fake::default()
            },
            &["", PLAIN_PAGE, ""],
        )
        .await;
        e.fake.vision_status.store(status, Ordering::SeqCst);
        let out = sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false)).await;
        let (kind, reason) = aborted(&out);
        assert_eq!(kind, AbortKind::GpuHold, "status {status}");
        assert!(
            reason.contains("fallback alias 'openai/gpt-5'") && reason.contains("not stored"),
            "status {status}: {reason}"
        );
        assert_eq!(e.vision_calls(), 1, "status {status}: no second page sent");
        assert!(vision_rows(&e.idx).await.is_empty(), "status {status}");
        assert!(!out
            .events
            .iter()
            .any(|e| matches!(e, SyncEvent::ReadingFailed { .. })));
        assert!(chunks(&e.idx, "scan.pdf").await.is_empty());
    }
}

#[tokio::test]
async fn an_index_rebuild_keeps_the_readings_and_a_stopped_sync_prunes_nothing() {
    let e = env(Fake::default(), &["", PLAIN_PAGE, ""]).await;
    sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false))
        .await
        .report();
    assert_eq!(e.vision_calls(), 1);

    // Another embedding model: the index is rebuilt — and the rebuild is held
    // at its first file, before the PDF has a document again. A stopped sync
    // prunes nothing.
    let other = || FixtureEmbedder::new(DIMS).with_model("other-model");
    let held = Counting::with(other(), Some(0));
    let out = sync_opts(&e.idx, held, &e.opts(Some(ALIAS), false)).await;
    match &out.result {
        Err(SyncError::Aborted { kind, partial, .. }) => {
            assert_eq!(*kind, AbortKind::GpuHold);
            assert!(partial.index_reset.is_some(), "rebuilt");
        }
        other => panic!("expected a hold, got {:?}", other.as_ref().map(|_| ())),
    }
    assert_eq!(vision_rows(&e.idx).await.len(), 1, "still cached");

    // Finished: every file indexed again, no page read again.
    let out = sync_opts(
        &e.idx,
        Counting::with(other(), None),
        &e.opts(Some(ALIAS), false),
    )
    .await;
    let r = out.report();
    assert_eq!(r.new_files, 2, "every file indexed again");
    assert_eq!((r.vision_pages_read, r.vision_pages_reused), (0, 1));
    assert_eq!(e.vision_calls(), 1);
    assert_eq!(fts_count(&e.idx, "kittiwake").await, 1);
    assert_eq!(r.vision_readings_pruned, 0);
}

#[tokio::test]
async fn a_deleted_file_takes_its_cached_readings_with_it() {
    let e = env(Fake::default(), &["", PLAIN_PAGE, ""]).await;
    sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false))
        .await
        .report();
    assert_eq!(vision_rows(&e.idx).await.len(), 1);
    std::fs::remove_file(e.tmp.path().join("scan.pdf")).unwrap();
    let r = sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false)).await;
    let r = r.report();
    assert_eq!(r.removed_files, 1);
    assert_eq!(r.vision_readings_pruned, 1);
    assert!(vision_rows(&e.idx).await.is_empty());
}

#[tokio::test]
async fn the_real_pdftoppm_renders_one_page_from_stdin() {
    if !folder_chat::pdf::pdftoppm_available().await {
        eprintln!("skipped: pdftoppm is not on PATH (install poppler-utils to run this test)");
        return;
    }
    let ppm = std::path::Path::new(folder_chat::pdf::PDFTOPPM);
    let pdf = std::sync::Arc::new(minimal_pdf(&["first", "second"]));
    let t = folder_chat::pdf::PDF_EXTRACT_TIMEOUT;
    let png = folder_chat::pdf::render_page(ppm, pdf.clone(), 2, 30, t)
        .await
        .unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "a PNG");
    let e = folder_chat::pdf::render_page(ppm, pdf, 9, 30, t)
        .await
        .unwrap_err();
    assert!(matches!(e, folder_chat::pdf::RenderError::Failed(_)), "{e}");
}

// ---------------------------------------------------------------------------
// End to end: the app, the real poppler tools, the MCP face
// ---------------------------------------------------------------------------

async fn poppler() -> bool {
    let ok = folder_chat::pdf::available().await && folder_chat::pdf::pdftoppm_available().await;
    if !ok {
        eprintln!(
            "skipped: pdftotext or pdftoppm is not on PATH (install poppler-utils to run this test)"
        );
    }
    ok
}

#[tokio::test]
async fn the_mcp_read_lines_of_a_reading_are_the_lines_its_citation_names() {
    if !poppler().await {
        return;
    }
    use common::harness::*;
    let app = start_configured(
        Fake {
            models: common::fake::models(json!({"id": "chat-small", "context_length": 4096})),
            ..Fake::default()
        },
        |c| folder_chat::config::AgentConfig {
            vision_model: Some(ALIAS.into()),
            ..c
        },
    )
    .await;
    write(
        app.tmp.path(),
        "scan.pdf",
        minimal_pdf(&["", "Heron survey text"]),
    );
    app.sync().await;
    let status: Value = app
        .get("/api/status")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["config"]["vision_model"], ALIAS);
    assert_eq!(status["config"]["vision_every_page"], false);
    assert_eq!(status["last_sync"]["vision_pages_read"], 1, "{status}");

    let call = |name: &'static str, args: Value| {
        app.http
            .post(app.url("/mcp"))
            .header("x-forwarded-host", AUTHORITY)
            .header("x-lmgw-face", "mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                       "params": {"name": name, "arguments": args}})
                .to_string(),
            )
            .send()
    };
    let found: Value = call("search", json!({"query": "kittiwake ledger nests", "k": 3}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let hits = found["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap()
        .clone();
    let hit = hits
        .iter()
        .find(|h| h["path"] == "scan.pdf")
        .unwrap_or_else(|| panic!("{found}"));
    assert_eq!(hit["heading_path"], "page 1, read by vision-fixture");
    assert_eq!(hit["page"], 1);
    assert_eq!(hit["text"], OCR_READING);
    let (from, to) = (
        hit["start_line"].as_u64().unwrap(),
        hit["end_line"].as_u64().unwrap(),
    );

    let read: Value = call(
        "read",
        json!({"path": "scan.pdf", "start_line": from, "end_line": to}),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let text = read["result"]["content"][0]["text"].as_str().unwrap();
    let body = text.split_once("\n\n").unwrap().1;
    assert_eq!(body, format!("{OCR_READING}\n"), "{text}");

    // The whole file: pdftotext's text, then the reading under its header.
    let whole: Value = call("read", json!({"path": "scan.pdf"}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = whole["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Heron survey text"), "{text}");
    assert!(
        text.ends_with(&format!(
            "\n\n[page 1, read by vision-fixture from the page image]\n{OCR_READING}\n"
        )),
        "{text}"
    );
}

/// One sync of the whole app, its events dropped.
async fn sync_app(app: &FolderChat) -> Result<folder_chat::sync::SyncReport, SyncError> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.sync(tx).await
}

/// Every search hit in `scan.pdf` is found at the lines its citation names in
/// the text the MCP `read` tool returns ([`FolderChat::read_file`]); how many
/// were checked, and how many of them are readings.
async fn citations_resolve(app: &FolderChat) -> (usize, usize) {
    let text = app.read_file("scan.pdf").await.unwrap();
    let mut n = (0, 0);
    let mut seen = std::collections::HashSet::new();
    for query in [
        "kittiwake ledger nests cliff",
        "heron survey osprey nesting",
    ] {
        let r = app.search(query, 10).await.unwrap();
        for h in r.hits.iter().filter(|h| h.path == "scan.pdf") {
            if !seen.insert(h.chunk_id.clone()) {
                continue;
            }
            let slice = text.lines(h.start_line, h.end_line).unwrap();
            assert!(
                slice.text.contains(&h.text),
                "{} (lines {:?}-{:?}): the lines hold {:?}, the excerpt is {:?}",
                h.heading_path,
                h.start_line,
                h.end_line,
                slice.text,
                h.text
            );
            n.0 += 1;
            n.1 += usize::from(h.heading_path.contains("read by"));
        }
    }
    n
}

#[tokio::test]
async fn a_stopped_pass_then_the_old_setting_again_keeps_the_read_lines_right() {
    if !poppler().await {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "scan.pdf",
        minimal_pdf(&["", "Heron survey text", "Osprey nesting season"]),
    );
    let models = || common::fake::models(json!({"id": "chat-small"}));
    let config = |alias: &str, every_page: bool| folder_chat::config::AgentConfig {
        vision_model: Some(alias.to_string()),
        vision_every_page: every_page,
        ..common::fake::config(tmp.path(), false)
    };
    let (base_a, fake_a) = serve(Fake {
        models: models(),
        ..Fake::default()
    })
    .await;

    // Indexed with vision-a: page 1 read.
    let a = FolderChat::open(config("vision-a", false), gateway(&base_a))
        .await
        .unwrap();
    sync_app(&a).await.unwrap();
    assert_eq!(citations_resolve(&a).await.1, 1);
    drop(a);

    // vision-b, every page: page 1 read, page 2 held — the pass stops.
    let (base_b, _) = serve(Fake {
        models: models(),
        vision_hold_from: Some(1),
        ..Fake::default()
    })
    .await;
    let b = FolderChat::open(config("vision-b", true), gateway(&base_b))
        .await
        .unwrap();
    match sync_app(&b).await {
        Err(SyncError::Aborted { kind, .. }) => assert_eq!(kind, AbortKind::GpuHold),
        other => panic!("expected a hold, got {:?}", other.map(|_| ())),
    }
    drop(b);

    // Back to vision-a. Before its sync, the index still holds vision-a's
    // text, and `read` returns that text.
    let a = FolderChat::open(config("vision-a", false), gateway(&base_a))
        .await
        .unwrap();
    assert!(a.load_existing().await.unwrap());
    let (hits, readings) = citations_resolve(&a).await;
    assert!(
        hits >= 2 && readings == 1,
        "{hits} hits, {readings} readings"
    );
    // The stopped pass left it pending: taken through, from the cache.
    let r = sync_app(&a).await.unwrap();
    assert_eq!(r.vision_refreshed_files, 1);
    assert_eq!((r.vision_pages_read, r.vision_pages_reused), (0, 1));
    assert_eq!(fake_a.vision_calls.load(Ordering::SeqCst), 1);
    let (hits, readings) = citations_resolve(&a).await;
    assert!(
        hits >= 2 && readings == 1,
        "{hits} hits, {readings} readings"
    );
}

#[tokio::test]
async fn a_refusal_the_probe_meets_too_stops_the_sync_and_one_it_does_not_is_cached() {
    // The model refuses every image: the page, then the probe.
    let e = env(Fake::default(), &["", PLAIN_PAGE, ""]).await;
    e.fake.vision_status.store(400, Ordering::SeqCst);
    let out = sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false)).await;
    let (kind, reason) = aborted(&out);
    assert_eq!(kind, AbortKind::Vision);
    for part in [
        ALIAS,
        "the image is not a valid PNG",
        "probe image of the same size as well",
        "fix it in lmgw",
    ] {
        assert!(reason.contains(part), "{part} in {reason}");
    }
    assert_eq!(e.vision_calls(), 2, "the page, then the probe");
    let probe = e.vision_body(1);
    assert_eq!(
        probe["messages"][0]["content"][1]["text"],
        folder_chat::vision::PROBE_PROMPT
    );
    assert!(vision_rows(&e.idx).await.is_empty(), "nothing cached");

    // The model refuses these pages only: the probe is read, so the refusal
    // is cached — and probed once per sync, not once per page.
    let e = env(Fake::default(), &["", "", ""]).await;
    e.fake.vision_page_status.store(400, Ordering::SeqCst);
    let out = sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false)).await;
    let r = out.report();
    assert_eq!(r.vision_probes, 1);
    assert_eq!(e.vision_calls(), 3, "page 1, the probe, page 2");
    assert_eq!(r.vision_pages_failed.len(), 2);
    assert!(
        r.vision_pages_failed[0]
            .reason
            .contains("the page's size, was read"),
        "{:?}",
        r.vision_pages_failed
    );
    assert_eq!(
        vision_rows(&e.idx).await,
        [
            (1, ALIAS.into(), "ocr".into(), "failed".into()),
            (2, ALIAS.into(), "ocr".into(), "failed".into())
        ]
    );
}

#[tokio::test]
async fn a_moved_pdf_keeps_its_readings() {
    let e = env(Fake::default(), &["", PLAIN_PAGE, ""]).await;
    sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false))
        .await
        .report();
    std::fs::create_dir(e.tmp.path().join("archive")).unwrap();
    std::fs::rename(
        e.tmp.path().join("scan.pdf"),
        e.tmp.path().join("archive/scan.pdf"),
    )
    .unwrap();
    let out = sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false)).await;
    let r = out.report();
    assert_eq!((r.removed_files, r.new_files), (1, 1));
    assert_eq!((r.vision_pages_read, r.vision_pages_reused), (0, 1));
    assert_eq!(e.vision_calls(), 1, "no page read again");
    assert_eq!(r.vision_readings_pruned, 0);
    assert_eq!(
        headings(&chunks(&e.idx, "archive/scan.pdf").await),
        ["page 2", "page 1, read by vision-fixture"]
    );
}

#[tokio::test]
async fn a_new_pdf_whose_embedding_fails_keeps_its_readings_for_the_next_sync() {
    let e = env(Fake::default(), &["", PLAIN_PAGE, ""]).await;
    let out = sync_opts(
        &e.idx,
        Counting::failing_on("Kittiwake"),
        &e.opts(Some(ALIAS), false),
    )
    .await;
    let r = out.report();
    assert!(
        r.errors
            .iter()
            .any(|e| e.file == "scan.pdf" && e.message.contains("EMBED_BATCH_ATTEMPTS")),
        "{:?}",
        r.errors
    );
    assert!(chunks(&e.idx, "scan.pdf").await.is_empty(), "not indexed");
    assert_eq!(
        vision_rows(&e.idx).await.len(),
        1,
        "the reading survived the prune"
    );

    let out = sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false)).await;
    let r = out.report();
    assert_eq!(r.new_files, 1);
    assert_eq!((r.vision_pages_read, r.vision_pages_reused), (0, 1));
    assert_eq!(e.vision_calls(), 1, "not read again");
    assert_eq!(fts_count(&e.idx, "kittiwake").await, 1);
}

#[tokio::test]
async fn retry_failed_pages_forgets_the_failures_and_reads_them_again() {
    if !poppler().await {
        return;
    }
    use common::harness::*;
    let app = start_configured(
        Fake {
            models: common::fake::models(json!({"id": "chat-small", "context_length": 4096})),
            ..Fake::default()
        },
        |c| folder_chat::config::AgentConfig {
            vision_model: Some(ALIAS.into()),
            ..c
        },
    )
    .await;
    write(
        app.tmp.path(),
        "scan.pdf",
        minimal_pdf(&["", "Heron survey text"]),
    );
    app.fake.vision_empty.store(true, Ordering::SeqCst);
    app.sync().await;
    let status = || async {
        app.get("/api/status")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    let s = status().await;
    assert_eq!(
        s["last_sync"]["vision_pages_failed"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "{s}"
    );

    // The owner fixed the model. Without the UI's header, refused as CSRF.
    app.fake.vision_empty.store(false, Ordering::SeqCst);
    let r = app
        .http
        .post(app.url("/api/vision/retry"))
        .header("x-forwarded-host", AUTHORITY)
        .header("x-lmgw-face", "app")
        .header("x-forwarded-for", LOCAL_CLIENT)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = app
        .post("/api/vision/retry", &json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let body: Value = r.json().await.unwrap();
    assert!(body["run"].is_u64(), "{body}");
    app.wait_idle().await;
    // The sync itself forgot the failure, under its own permit.
    let s = status().await;
    assert_eq!(s["last_sync"]["vision_failures_cleared"], 1, "{s}");
    assert!(
        s["last_sync"]["notes"].as_array().unwrap().iter().any(|n| n
            .as_str()
            .unwrap()
            .starts_with("Retry failed pages: 1 cached")),
        "{s}"
    );
    assert_eq!(s["last_sync"]["vision_pages_read"], 1, "{s}");
    assert_eq!(
        s["last_sync"]["vision_pages_failed"],
        json!([]),
        "{}",
        s["last_sync"]
    );
    assert_eq!(app.fake.vision_calls.load(Ordering::SeqCst), 2);
}

/// Every chunk of `scan.pdf` stored in the index is found at the lines stored
/// with it, in the text the MCP `read` tool returns ([`FolderChat::read_file`]);
/// the readings among them, by alias.
async fn stored_chunks_resolve(app: &FolderChat) -> Vec<String> {
    let text = app.read_file("scan.pdf").await.unwrap();
    let got = chunks(app.index(), "scan.pdf").await;
    let ids: Vec<String> = got.iter().map(|c| c.id.clone()).collect();
    let lines = app.index().chunk_lines(&ids).await.unwrap();
    let mut readings = Vec::new();
    for c in &got {
        let (a, b) = lines[&c.id];
        let slice = text.lines(Some(a), Some(b)).unwrap();
        assert!(
            slice.text.contains(&c.payload),
            "{} (lines {a}-{b}): the lines hold {:?}, the chunk is {:?}",
            c.heading_path,
            slice.text,
            c.payload
        );
        if c.heading_path.contains(", read by ") {
            readings.push(c.heading_path.rsplit(' ').next().unwrap().to_string());
        }
    }
    readings
}

#[tokio::test]
async fn a_stop_mid_embedding_leaves_read_and_the_stored_lines_agreeing() {
    if !poppler().await {
        return;
    }
    // Forty pages of text, every one read in structure mode: forty readings,
    // more than one embedding batch.
    let texts: Vec<String> = (1..=40).map(|i| format!("Ledger entry {i}")).collect();
    let pages: Vec<&str> = texts.iter().map(String::as_str).collect();
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "scan.pdf", minimal_pdf(&pages));
    let models = || common::fake::models(json!({"id": "chat-small"}));
    let config = |alias: &str| folder_chat::config::AgentConfig {
        vision_model: Some(alias.to_string()),
        vision_every_page: true,
        ..common::fake::config(tmp.path(), false)
    };
    let (base_a, _) = serve(Fake {
        models: models(),
        ..Fake::default()
    })
    .await;
    let a = FolderChat::open(config("vision-a"), gateway(&base_a))
        .await
        .unwrap();
    sync_app(&a).await.unwrap();
    assert_eq!(stored_chunks_resolve(&a).await.len(), 40);
    drop(a);

    // vision-b: its readings are new, and the embeddings are held from the
    // second batch on (call 0 is the connect probe, call 1 the first batch).
    let (base_b, _) = serve(Fake {
        models: models(),
        hold_embeddings_from: Some(2),
        ..Fake::default()
    })
    .await;
    let b = FolderChat::open(config("vision-b"), gateway(&base_b))
        .await
        .unwrap();
    match sync_app(&b).await {
        Err(SyncError::Aborted { kind, .. }) => assert_eq!(kind, AbortKind::GpuHold),
        other => panic!("expected a hold, got {:?}", other.map(|_| ())),
    }
    // Part of vision-b's readings are stored, none of vision-a's is left,
    // and `read` returns the text their lines count in.
    let readings = stored_chunks_resolve(&b).await;
    assert_eq!(
        readings.len(),
        folder_chat::sync::EMBED_BATCH_SIZE,
        "{readings:?}"
    );
    assert!(readings.iter().all(|m| m == "vision-b"), "{readings:?}");
    drop(b);

    // vision-a again: the pending PDF is taken through from the cache.
    let (base_a2, fake_a2) = serve(Fake {
        models: models(),
        ..Fake::default()
    })
    .await;
    let a = FolderChat::open(config("vision-a"), gateway(&base_a2))
        .await
        .unwrap();
    let r = sync_app(&a).await.unwrap();
    assert_eq!((r.vision_pages_read, r.vision_pages_reused), (0, 40));
    assert_eq!(fake_a2.vision_calls.load(Ordering::SeqCst), 0);
    let readings = stored_chunks_resolve(&a).await;
    assert_eq!(readings.len(), 40);
    assert!(readings.iter().all(|m| m == "vision-a"), "{readings:?}");
}

#[tokio::test]
async fn a_pre_release_readings_table_is_taken_in_and_its_pdfs_refreshed() {
    let e = env(Fake::default(), &["", PLAIN_PAGE, ""]).await;
    sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false))
        .await
        .report();
    // What a pre-release build of 0.2.0 left: its own table, keyed by
    // document, with a reading by another alias.
    let (doc, hash): (i64, String) =
        sqlx::query_as("SELECT id, content_hash FROM document WHERE url = 'scan.pdf'")
            .fetch_one(e.idx.pool())
            .await
            .unwrap();
    sqlx::query(
        "CREATE TABLE folder_chat_vision (
             document_id INTEGER NOT NULL REFERENCES document(id) ON DELETE CASCADE,
             page INTEGER NOT NULL, content_hash TEXT NOT NULL, model TEXT NOT NULL,
             prompt_version TEXT NOT NULL, mode TEXT NOT NULL, status TEXT NOT NULL,
             text TEXT NOT NULL, PRIMARY KEY (document_id, page))",
    )
    .execute(e.idx.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO folder_chat_vision VALUES (?1, 1, ?2, 'vision-old', ?3, 'ocr', 'read',
                                                'Kittiwake ledger, the old reading')",
    )
    .bind(doc)
    .bind(&hash)
    // The pre-release build wrote prompt version "1", which the current
    // prompts no longer reuse; the current version shows the reading carried
    // over and reused as it is.
    .bind(folder_chat::vision::VISION_PROMPT_VERSION)
    .execute(e.idx.pool())
    .await
    .unwrap();

    let idx = IndexDir::open(e.tmp.path()).await.unwrap();
    let gone: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'folder_chat_vision'",
    )
    .fetch_optional(idx.pool())
    .await
    .unwrap();
    assert_eq!(gone, None, "the table is dropped");
    assert_eq!(
        vision_rows(&idx).await,
        [
            (1, ALIAS.into(), "ocr".into(), "read".into()),
            (1, "vision-old".into(), "ocr".into(), "read".into())
        ],
        "its reading is in the cache"
    );
    // Every PDF is pending: the next sync takes it through, from the cache.
    let out = sync_opts(&idx, Counting::new(), &e.opts(Some("vision-old"), false)).await;
    let r = out.report();
    assert_eq!(r.vision_refreshed_files, 1);
    assert_eq!((r.vision_pages_read, r.vision_pages_reused), (0, 1));
    assert_eq!(e.vision_calls(), 1);
    assert_eq!(
        headings(&chunks(&idx, "scan.pdf").await),
        ["page 2", "page 1, read by vision-old"]
    );
}

#[tokio::test]
async fn the_probe_is_the_refused_page_s_size_so_a_context_too_small_for_it_stops_the_sync() {
    // Page images of 120×160 pixels, from a model whose context holds at most
    // 64×64: a small probe would pass and blame every page.
    let page = folder_chat::vision::probe_png(120, 160);
    let e = env(
        Fake {
            vision_max_pixels: Some(64 * 64),
            ..Fake::default()
        },
        &["", PLAIN_PAGE, ""],
    )
    .await;
    let ppm = fake_pdftoppm_png(&page);
    let opts = SyncOptions {
        pdftoppm: ppm.path.clone(),
        ..e.opts(Some(ALIAS), false)
    };
    let (kind, reason) = aborted(&sync_opts(&e.idx, Counting::new(), &opts).await);
    assert_eq!(kind, AbortKind::Vision);
    for part in [
        "exceeds the available context size",
        "blank 120×160 probe image",
    ] {
        assert!(reason.contains(part), "{part} in {reason}");
    }
    assert!(vision_rows(&e.idx).await.is_empty(), "nothing cached");
    // The probe: the page's size, blank — nothing of the page.
    let probe = e.vision_body(1);
    assert_eq!(
        probe["messages"][0]["content"][0]["image_url"]["url"],
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD
                .encode(folder_chat::vision::probe_png(120, 160))
        )
    );
}

#[tokio::test]
async fn a_moved_pdf_whose_extraction_fails_keeps_its_readings() {
    let e = env(Fake::default(), &["", PLAIN_PAGE, ""]).await;
    sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false))
        .await
        .report();
    std::fs::rename(
        e.tmp.path().join("scan.pdf"),
        e.tmp.path().join("moved.pdf"),
    )
    .unwrap();
    // pdftotext times out on the new path: the file is skipped this time.
    let slow = fake_pdftotext("1.0-fake", &[], Some(30));
    let r = sync_opts(
        &e.idx,
        Counting::new(),
        &SyncOptions {
            pdftotext: slow.path.clone(),
            pdf_timeout: Duration::from_millis(300),
            ..e.opts(Some(ALIAS), false)
        },
    )
    .await;
    let r = r.report();
    assert_eq!(r.removed_files, 1);
    assert_eq!(r.vision_readings_pruned, 0);
    assert_eq!(vision_rows(&e.idx).await.len(), 1, "kept for the next try");
    // It extracts next time, and no page is read again.
    let r = sync_opts(&e.idx, Counting::new(), &e.opts(Some(ALIAS), false)).await;
    assert_eq!(r.report().vision_pages_reused, 1);
    assert_eq!(e.vision_calls(), 1);
}
