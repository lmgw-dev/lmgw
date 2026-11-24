//! The hostile files of the WP4 review, each built in code and sent through
//! the conversion: a small upload that used to hang a slot for good, ask
//! for more memory than the machine has, or stall the runtime. Each must
//! end as that file's own named outcome within a time bound, with this
//! process (the gateway's stand-in) alive and the tool pool back to no
//! running step.
//!
//! The reader child runs under bounds a test can afford ([`TIMEOUT`],
//! [`MEMORY`]) instead of `READER_TIMEOUT` and `READER_MEMORY_LIMIT_BYTES`;
//! the mechanism is the same, and the messages name the constants.

use std::time::{Duration, Instant};

use super::built;
use super::*;
use crate::extract::convert::batch::{self, Each};
use crate::extract::convert::{pool, reader};

/// What the reader may take in these tests.
const TIMEOUT: Duration = Duration::from_secs(4);
const MEMORY: u64 = 512 << 20;

/// The whole of one hostile file may take this long, the wait for other
/// tests' build load included.
const FILE_BOUND: Duration = Duration::from_secs(60);

fn named(name: &str, bytes: Vec<u8>) -> Input {
    Input {
        index: 0,
        name: name.into(),
        bytes: Ok(Bytes::from(bytes)),
    }
}

/// The pool has no running step (a stopped child is reaped first).
async fn idle() {
    let started = Instant::now();
    while pool::global().facts().running > 0 {
        assert!(
            started.elapsed() < FILE_BOUND,
            "the tool pool still counts {} running step(s)",
            pool::global().facts().running
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// `bytes` converted under the test's reader bounds; the pool is idle after.
async fn bounded(name: &str, bytes: Vec<u8>) -> ConvertedDocument {
    let facts = reader_only().await;
    let started = Instant::now();
    let d = reader::with_bounds(
        TIMEOUT,
        MEMORY,
        convert(named(name, bytes), &plan_of(Default::default()), &facts),
    )
    .await
    .doc;
    assert!(started.elapsed() < FILE_BOUND, "{:?}", started.elapsed());
    idle().await;
    d
}

/// The file failed as `unreadable`, by one of the reader's bounds.
fn stopped_by_a_bound(d: &ConvertedDocument) {
    assert_eq!(d.status, DocStatus::Failed, "{d:?}");
    let e = d.error.as_ref().unwrap();
    assert_eq!(e.code, ErrorCode::Unreadable, "{e:?}");
    assert!(
        e.message.contains("READER_TIMEOUT") || e.message.contains("READER_MEMORY_LIMIT_BYTES"),
        "{}",
        e.message
    );
}

/// Finding 1: tables nested in captions, 3 KiB. htmd's table writer walked
/// each level twice (2^60 calls); lmgw's writes each caption once.
#[tokio::test]
async fn the_nested_caption_table_converts_at_once() {
    let n = 60;
    let page = format!(
        "<!DOCTYPE html><html><body>{}x{}</body></html>",
        "<table><caption>".repeat(n),
        "</caption><thead></thead></table>".repeat(n)
    );
    assert!(page.len() < 4096);
    let started = Instant::now();
    let d = bounded("nested.html", page.into_bytes()).await;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    assert_eq!(d.content.as_deref(), Some("x"));
    assert!(started.elapsed() < TIMEOUT, "{:?}", started.elapsed());
}

/// Finding 1: one megabyte-wide cell and a hundred thousand rows. Padded to
/// the widest cell that was 100 GiB of output; unpadded it is the page's
/// size.
#[tokio::test]
async fn the_padded_table_is_the_size_of_its_page() {
    let page = format!(
        "<!DOCTYPE html><html><body><table><tr><th>h<tr><td>{}{}</table></body></html>",
        "A".repeat(1 << 20),
        "<tr><td>x".repeat(100_000)
    );
    let size = page.len();
    let d = bounded("padded.html", page.into_bytes()).await;
    assert_eq!(d.status, DocStatus::Converted, "{:?}", d.error);
    let md = d.content.unwrap();
    assert!(md.len() < 3 * size, "{} from {size}", md.len());
    assert_eq!(d.stats.tables, 1);
}

/// Finding 2: the five pages the depth estimate called flat. The parsed
/// tree's own depth is what is counted now, so each is read flat with the
/// bound named, whatever the markup.
#[tokio::test]
async fn the_five_estimator_bypasses_are_counted_on_the_tree() {
    let n = 400;
    for (what, page) in [
        ("self-closed div", "<div/>".repeat(n)),
        ("rt", "<rt>".repeat(n)),
        ("optgroup", "<optgroup>".repeat(n)),
        ("comment end", format!("<!----!>{}", "<div>".repeat(n))),
        ("svg title", format!("<svg><title>{}", "<div>".repeat(n))),
    ] {
        let page = format!("<!DOCTYPE html><html><body><h1>Kept</h1>{page}text");
        let d = bounded("deep.html", page.into_bytes()).await;
        assert_eq!(d.status, DocStatus::Partial, "{what}: {d:?}");
        assert!(
            d.warnings[0].message.contains("MAX_HTML_DEPTH (256)"),
            "{what}: {:?}",
            d.warnings
        );
        assert!(d.content.unwrap().contains("Kept"), "{what}");
    }
}

/// Finding 2: a million nested elements, about 5.7 MiB, which cost html5ever about
/// an hour. Since security round A the reader stops the parse once the page
/// nests past four times `MAX_HTML_DEPTH`, and the page is read flat, named.
#[tokio::test]
async fn a_page_nested_a_million_deep_is_read_flat_by_name() {
    let page = format!("<!DOCTYPE html><html><body>{}x", "<div/>".repeat(1_000_000));
    let d = bounded("million.html", page.into_bytes()).await;
    assert_eq!(d.status, DocStatus::Partial, "{d:?}");
    let w = &d.warnings[0];
    assert!(w.message.contains("MAX_HTML_DEPTH (256)"), "{}", w.message);
}

/// Deep, but within the parse's stop: a thousand levels, two thousand
/// times. html5ever's time is the square of the depth for each, and the
/// child is stopped at its bound.
#[tokio::test]
async fn a_page_deep_many_times_over_is_stopped_by_name() {
    let block = format!("{}{}", "<div>".repeat(1000), "</div>".repeat(1000));
    let page = format!("<!DOCTYPE html><html><body>{}", block.repeat(2000));
    stopped_by_a_bound(&bounded("deep-many.html", page.into_bytes()).await);
}

/// Finding 2: the active-formatting amplifier, within the depth bound: 240
/// open `<b>` and a hundred thousand paragraphs, each rebuilding all 240.
#[tokio::test]
async fn the_active_formatting_amplifier_is_stopped_by_name() {
    let mut page = String::from("<!DOCTYPE html><html><body><p>");
    for i in 0..240 {
        page.push_str(&format!("<b id={i}>"));
    }
    page.push_str(&"<p>x".repeat(100_000));
    stopped_by_a_bound(&bounded("formatting.html", page.into_bytes()).await);
}

/// The amplifier reaches an EPUB's chapter and an e-mail's HTML body the
/// same way: the same child, the same bounds.
#[tokio::test]
async fn the_amplifier_in_a_message_body_is_stopped_by_name() {
    let mut body = String::from("<p>");
    for i in 0..240 {
        body.push_str(&format!("<b id={i}>"));
    }
    body.push_str(&"<p>x".repeat(100_000));
    let mail = format!(
        "From: a@example.com\r\nTo: b@example.com\r\nSubject: s\r\nMIME-Version: 1.0\r\n\
         Content-Type: text/html; charset=utf-8\r\n\r\n{body}"
    );
    stopped_by_a_bound(&bounded("mail.eml", mail.into_bytes()).await);
}

/// Finding 3: one shared string of 8,000 characters and 200,000 cells that
/// name it (2.8 MiB of file). calamine copies the string into every cell:
/// 1.6 GiB, in the child.
#[tokio::test]
async fn the_shared_string_bomb_is_stopped_by_name() {
    let text = vec![b'A'; 8000];
    let mut sst = 1u32.to_le_bytes().to_vec();
    sst.extend_from_slice(&1u32.to_le_bytes());
    sst.extend_from_slice(&(text.len() as u16).to_le_bytes());
    sst.push(0); // one byte a character
    sst.extend_from_slice(&text);
    let mut cells = Vec::new();
    for _ in 0..200_000 {
        // LabelSst at A1: row, column, format, string index.
        cells.extend(built::rec(0x00FD, &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
    }
    let file = built::xls_of(&built::workbook(&built::rec(0x00FC, &sst), "S", &cells));
    assert!(file.len() < 4 << 20, "{}", file.len());
    stopped_by_a_bound(&bounded("strings.xls", file).await);
}

/// Finding 3: the directory chain that loops (6,144 bytes). Whatever the
/// sniffer and calamine make of it, the file ends by name and nothing of
/// it grows in this process.
#[tokio::test]
async fn the_directory_loop_ends_by_name() {
    let file = built::cfb_directory_loop();
    let d = bounded("loop.xls", file.clone()).await;
    assert_eq!(d.status, DocStatus::Failed, "{d:?}");
    // The reader itself, past the sniffer: a named end either way.
    let facts = reader_only().await;
    let e = reader::with_bounds(
        TIMEOUT,
        MEMORY,
        reader::read(
            &facts,
            lmgw_doc_reader::wire::Op::Xls,
            crate::extract::convert::run::Input::Bytes(Bytes::from(file)),
            1 << 30,
        ),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            e,
            reader::ReadError::OutOfMemory
                | reader::ReadError::Timeout
                | reader::ReadError::Named(_)
                | reader::ReadError::Stopped(_)
        ),
        "{e:?}"
    );
    idle().await;
}

/// Finding 3: an unlinked `Workbook` entry before the linked one, holding
/// two cells at opposite corners of a 65,536 × 65,536 sheet. calamine reads
/// the first entry and builds the grid between the corners: 128 GiB, in the
/// child.
#[tokio::test]
async fn the_hidden_workbook_stream_is_stopped_by_name() {
    let mut corners = built::number(0, 0, 1.0);
    corners.extend(built::number(65_535, 65_535, 2.0));
    let hidden = built::workbook(&[], "Hidden", &corners);
    let shown = built::workbook(&[], "Shown", &built::number(0, 0, 7.0));
    let file = built::cfb_hidden_workbook(&hidden, &shown);
    let d = bounded("hidden.xls", file).await;
    // Read as the shown sheet (a reader that walks the tree) or stopped at
    // the hidden one's grid: never a grid in this process.
    match d.status {
        DocStatus::Failed => stopped_by_a_bound(&d),
        _ => assert!(d.content.unwrap().contains("Shown"), "the shown sheet"),
    }
}

/// Finding 10: a formula's string result (`=IF(…,"Text")`) over 2,000 rows.
/// The guard read the result's first bytes as a cell position and refused
/// the workbook as "claims grids"; there is no such guard now.
#[tokio::test]
async fn string_formulas_over_two_thousand_rows_are_read() {
    let mut cells = Vec::new();
    for row in 0..2000u16 {
        // Formula: row, column, format, a string result (0, …, 0xFFFF),
        // flags, chain, then one token (an integer) as its expression.
        let mut f = row.to_le_bytes().to_vec();
        f.extend_from_slice(&[0, 0, 0, 0]);
        f.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0xFF, 0xFF]);
        f.extend_from_slice(&[0; 6]);
        f.extend_from_slice(&[3, 0, 0x1E, 1, 0]);
        cells.extend(built::rec(0x0006, &f));
        // Its value: the String record that follows.
        cells.extend(built::rec(0x0207, &[4, 0, 0, b'T', b'e', b'x', b't']));
    }
    let file = built::xls_of(&built::workbook(&[], "Formulas", &cells));
    let d = bounded("formulas.xls", file).await;
    assert_eq!(d.status, DocStatus::Converted, "{:?}", d.error);
    let md = d.content.unwrap();
    assert_eq!(md.matches("| Text |").count(), 2000, "{}", &md[..200]);
    assert!(md.contains("| 2000 | Text |"), "{}", &md[md.len() - 60..]);
}

/// Finding 4: a 10,000 × 10,000 page of four 16-bit samples from a few
/// kilobytes (about 763 MiB to decode), and a caller whose `document_timeout`
/// passes while it is decoded (the review's was 20 ms, which ends the file
/// before its decode has started; 300 ms ends it in the middle). The file
/// fails by the deadline, its decode is stopped, and the pool counts the
/// step until the child has been reaped, then no longer.
#[tokio::test]
async fn a_decode_outlived_by_its_deadline_keeps_its_slot_until_it_is_gone() {
    use lmgw_api_types::documents::{ExtraKind, ToolExtra};
    let file = built::rgba16_tiff(10_000);
    assert!(file.len() < 64 << 10, "{}", file.len());
    // The real reader, and a tesseract that is never reached.
    let mut facts = (*reader_only().await).clone().with_found(ToolId::Tesseract);
    if let Some(Fact::Found(t)) = facts.tools.get_mut(&ToolId::Tesseract).map(|t| &mut t.fact) {
        t.extras = vec![ToolExtra {
            kind: ExtraKind::Languages,
            present: vec!["eng".into()],
            absent: vec!["osd".into()],
        }];
    }
    let plan = plan_of(Default::default());
    let each = Each {
        deadline: Some(Duration::from_millis(300)),
        ..Default::default()
    };
    let started = Instant::now();
    let (_, a) = batch::convert_each(
        vec![named("page.tif", file)],
        &plan,
        &facts,
        Vec::new(),
        OutputBudget::request(),
        &each,
    )
    .await;
    let d = &a.documents[0];
    assert_eq!(
        d.error.as_ref().map(|e| e.code),
        Some(ErrorCode::DeadlineExceeded),
        "{d:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    // A reader still decoding is a step the pool still counts.
    let readers = || {
        std::fs::read_dir("/proc/self/task")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|t| std::fs::read_to_string(t.path().join("children")).ok())
            .flat_map(|c| c.split_whitespace().map(str::to_string).collect::<Vec<_>>())
            .filter(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .is_ok_and(|c| c.trim() == reader::READER_BIN)
            })
            .count()
    };
    let started = Instant::now();
    loop {
        // In this order: a pool that counts nothing must mean no reader.
        if pool::global().facts().running == 0 {
            assert_eq!(
                readers(),
                0,
                "the slot was freed before the reader was reaped"
            );
            break;
        }
        assert!(started.elapsed() < FILE_BOUND, "never reaped");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Finding 6: a package document of filler elements, 64 MiB that deflate
/// to nothing. Streamed in the child under a memory limit a twentyfold
/// expansion would break, the book converts.
#[tokio::test]
async fn the_package_bomb_is_streamed() {
    use std::io::Write as _;
    let mut opf = String::from("<package><manifest>");
    opf.push_str(&"<a/>".repeat(16 << 20));
    opf.push_str(
        r#"<item id="a" href="a.xhtml" media-type="application/xhtml+xml"/></manifest>
           <spine><itemref idref="a"/></spine></package>"#,
    );
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut z = zip::ZipWriter::new(&mut buf);
    let o = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, content) in [
        ("mimetype", &b"application/epub+zip"[..]),
        (
            "META-INF/container.xml",
            br#"<container><rootfiles><rootfile full-path="book.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
        ),
        ("book.opf", opf.as_bytes()),
        ("a.xhtml", b"<html><body><p>The chapter.</p></body></html>"),
    ] {
        z.start_file(name, o).unwrap();
        z.write_all(content).unwrap();
    }
    z.finish().unwrap();
    let file = buf.into_inner();
    assert!(file.len() < 1 << 20, "{}", file.len());
    let facts = reader_only().await;
    let d = reader::with_bounds(
        Duration::from_secs(60),
        MEMORY,
        convert(
            named("bomb.epub", file),
            &plan_of(Default::default()),
            &facts,
        ),
    )
    .await
    .doc;
    assert_eq!(d.status, DocStatus::Converted, "{:?}", d.error);
    assert_eq!(d.content.as_deref(), Some("The chapter."));
    idle().await;
}

/// Finding 7: a message whose attachments convert to more than the request
/// may produce. Each is charged as it is read; the ones past the budget are
/// named and not read, and the message keeps what fit.
#[tokio::test]
async fn attachments_past_the_budget_are_named_and_left() {
    let mut mail = String::from(
        "From: a@example.com\r\nTo: b@example.com\r\nSubject: Pile\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
         --b\r\nContent-Type: text/plain\r\n\r\nSee the files.\r\n",
    );
    let line = "words of an attachment that add up\r\n";
    for i in 0..20 {
        mail.push_str(&format!(
            "--b\r\nContent-Type: text/plain\r\n\
             Content-Disposition: attachment; filename=\"note-{i:02}.txt\"\r\n\r\n{}",
            line.repeat(300)
        ));
    }
    mail.push_str("--b--\r\n");
    let facts = reader_only().await;
    // Room for about four of the twenty.
    let budget = Arc::new(OutputBudget::with_limit(45_000));
    let d = super::super::convert(
        named("pile.eml", mail.into_bytes()),
        &plan_of(Default::default()),
        &facts,
        &budget,
    )
    .await
    .doc;
    assert_eq!(d.status, DocStatus::Partial, "{:?}", d.error);
    let md = d.content.unwrap();
    assert!(
        md.contains("## Attachment: note-00.txt"),
        "the first is read"
    );
    assert!(
        !md.contains("## Attachment: note-19.txt"),
        "the last is not"
    );
    let read = md.matches("## Attachment: ").count();
    assert!((1..10).contains(&read), "{read} attachments read");
    let w = d
        .warnings
        .iter()
        .find(|w| w.message.contains("output was used up"))
        .expect("the attachments left are named");
    assert_eq!(w.code, WarningCode::PartFailed);
    assert!(
        w.message.contains(&format!("{} attachment(s)", 20 - read))
            && w.message.contains("note-19.txt")
            && w.message.contains("MAX_UNCOMPRESSED_BYTES"),
        "{}",
        w.message
    );
    idle().await;
}

/// Finding 9: a ZIP whose `mimetype` entry unpacks to 64 MiB (the review's
/// to 1 GiB). A few dozen bytes of it are unpacked to see it is no EPUB.
#[tokio::test]
async fn the_mimetype_bomb_is_not_unpacked() {
    use std::io::Write as _;
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut z = zip::ZipWriter::new(&mut buf);
    let o = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    z.start_file("mimetype", o).unwrap();
    let chunk = vec![b'a'; 1 << 20];
    for _ in 0..64 {
        z.write_all(&chunk).unwrap();
    }
    z.finish().unwrap();
    let started = Instant::now();
    let d = bounded("bomb.zip", buf.into_inner()).await;
    assert_eq!(
        d.error.as_ref().map(|e| e.code),
        Some(ErrorCode::Unsupported),
        "{d:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}
