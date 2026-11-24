//! The layout pass on synthetic poppler output: what a page becomes, how
//! long a large document takes, and what hostile output does (never a
//! panic, never a quadratic pass).

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use super::super::doc::render::{self, TableStyle};
use super::super::doc::BlockKind;
use super::*;

/// One synthetic line: box, text, font size, bold.
#[derive(Clone)]
struct L {
    x0: f32,
    y0: f32,
    x1: f32,
    size: f32,
    bold: bool,
    text: String,
}

fn l(x0: f32, y0: f32, x1: f32, size: f32, text: &str) -> L {
    L {
        x0,
        y0,
        x1,
        size,
        bold: false,
        text: text.into(),
    }
}

fn bold(mut x: L) -> L {
    x.bold = true;
    x
}

/// Pages of blocks of lines, as `pdftotext -bbox-layout` and `pdftohtml
/// -xml -zoom 1` write them (one word per line is enough for the pass: a
/// line's text is its words joined).
fn xml(pages: &[Vec<Vec<L>>]) -> (String, String) {
    let mut bbox = String::from("<html><body><doc>\n");
    let mut html = String::from("<pdf2xml>\n");
    let mut fonts: Vec<(f32, bool)> = Vec::new();
    for (p, blocks) in pages.iter().enumerate() {
        let _ = writeln!(bbox, r#"<page width="595" height="842">"#);
        let _ = writeln!(
            html,
            r#"<page number="{}" width="595" height="842">"#,
            p + 1
        );
        for b in blocks {
            bbox.push_str("<flow><block>");
            for x in b {
                let y1 = x.y0 + x.size * 1.1;
                let _ = write!(
                    bbox,
                    r#"<line xMin="{}" yMin="{}" xMax="{}" yMax="{y1}">"#,
                    x.x0, x.y0, x.x1
                );
                let _ = write!(
                    bbox,
                    r#"<word xMin="{}" yMin="{}" xMax="{}" yMax="{y1}">{}</word></line>"#,
                    x.x0,
                    x.y0,
                    x.x1,
                    x.text.replace('&', "&amp;").replace('<', "&lt;")
                );
                let id = match fonts.iter().position(|f| *f == (x.size, x.bold)) {
                    Some(i) => i,
                    None => {
                        fonts.push((x.size, x.bold));
                        let _ = writeln!(
                            html,
                            r##"<fontspec id="{}" size="{}" family="A+Serif" color="#000000"/>"##,
                            fonts.len() - 1,
                            x.size
                        );
                        fonts.len() - 1
                    }
                };
                let (o, c) = if x.bold { ("<b>", "</b>") } else { ("", "") };
                let _ = writeln!(
                    html,
                    r#"<text top="{}" left="{}" width="{}" height="{}" font="{id}">{o}{}{c}</text>"#,
                    x.y0,
                    x.x0,
                    x.x1 - x.x0,
                    x.size * 1.1,
                    x.text.replace('&', "&amp;").replace('<', "&lt;")
                );
            }
            bbox.push_str("</block></flow>\n");
        }
        bbox.push_str("</page>\n");
        html.push_str("</page>\n");
    }
    bbox.push_str("</doc></body></html>");
    html.push_str("</pdf2xml>");
    (bbox, html)
}

mod real;
mod regions;
mod regions_checks;

fn run(pages: &[Vec<Vec<L>>]) -> Layout {
    let (b, h) = xml(pages);
    layout(b.as_bytes(), Some(h.as_bytes()), 1).unwrap()
}

fn md(lay: &Layout) -> String {
    render::markdown(&lay.doc, TableStyle::Padded, "\n\n")
}

/// A two-column page under a title, with a running header and footer.
fn two_column(n: u32) -> Vec<Vec<L>> {
    let mut blocks = vec![
        vec![l(57.0, 28.0, 280.0, 10.0, "Example Corp Quarterly Report")],
        vec![l(
            57.0,
            800.0,
            140.0,
            10.0,
            &format!("Page {n} of example.org"),
        )],
    ];
    if n == 1 {
        blocks.push(vec![l(57.0, 66.0, 200.0, 20.0, "Example Report")]);
        blocks.push(vec![
            l(57.0, 97.0, 537.0, 10.0, "An abstract across the full"),
            l(57.0, 109.0, 300.0, 10.0, "width of the page."),
        ]);
    }
    for (col, x0) in [(0, 57.0f32), (1, 310.0)] {
        let x1 = x0 + 228.0;
        let mut y = 140.0;
        for s in 0..2 {
            let id = n * 10 + col * 2 + s;
            blocks.push(vec![l(x0, y, x0 + 110.0, 14.0, &format!("Section {id}"))]);
            y += 20.0;
            blocks.push(vec![
                bold(l(x0, y, x0 + 80.0, 10.0, &format!("Detail {id}"))),
                l(
                    x0,
                    y + 12.0,
                    x1,
                    10.0,
                    &format!("Text {id} begins and is exam-"),
                ),
                l(x0, y + 24.0, x1, 10.0, "ple prose that goes on."),
            ]);
            y += 200.0;
        }
    }
    blocks
}

#[test]
fn columns_headings_and_repeated_headers_come_out_in_reading_order() {
    let pages: Vec<_> = (1..=3).map(two_column).collect();
    let lay = run(&pages);
    assert!(!lay.sizes_from_boxes);
    let m = md(&lay);
    let want = "# Example Report\n\nAn abstract across the full width of the page.\n\n\
                ## Section 10\n\n### Detail 10\n\nText 10 begins and is example prose that goes \
                on.\n\n## Section 11";
    assert!(m.starts_with(want), "{m}");
    // The left column before the right one.
    let (left, right) = (m.find("Section 11").unwrap(), m.find("Section 12").unwrap());
    assert!(left < right, "{m}");
    assert!(
        !m.contains("Example Corp") && !m.contains("example.org"),
        "{m}"
    );
    let texts: Vec<&str> = lay.repeated.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(
        texts,
        ["Example Corp Quarterly Report", "Page 1 of example.org"]
    );
    // json keeps them, marked.
    let first = &lay.doc.parts[0];
    assert_eq!(first.blocks[0].kind, BlockKind::PageHeader);
    assert_eq!(first.blocks.last().unwrap().kind, BlockKind::PageFooter);
    assert_eq!((first.number, first.size), (1, Some((595.0, 842.0))));
    assert!(first.blocks.iter().all(|b| b.bbox.is_some()));
}

#[test]
fn a_table_of_aligned_cells_becomes_a_table_in_its_place() {
    let mut blocks = vec![vec![l(
        57.0,
        60.0,
        300.0,
        10.0,
        "The table below lists items.",
    )]];
    // poppler writes each column as a block of its own.
    let cols = [
        (57.0, 140.0),
        (177.0, 200.0),
        (300.0, 330.0),
        (400.0, 420.0),
    ];
    for (c, (x0, x1)) in cols.iter().enumerate() {
        let mut b = Vec::new();
        for r in 0..4 {
            let y = 80.0 + r as f32 * 12.0;
            let text = if r == 0 {
                ["Item", "Unit", "Price", "Stock"][c].to_string()
            } else {
                format!("v{r}{c}")
            };
            b.push(l(*x0, y, *x1, 10.0, &text));
        }
        if c == 0 {
            b.push(l(57.0, 140.0, 200.0, 10.0, "Prices are examples."));
        }
        blocks.push(b);
    }
    let lay = run(&[blocks]);
    let m = md(&lay);
    assert_eq!(
        m,
        "The table below lists items.\n\n| Item | Unit | Price | Stock |\n| --- | --- | --- | --- |\n\
         | v10 | v11 | v12 | v13 |\n| v20 | v21 | v22 | v23 |\n| v30 | v31 | v32 | v33 |\n\n\
         Prices are examples."
    );
    assert_eq!(lay.doc.counts().tables, 1);
}

#[test]
fn bullets_are_list_items_and_long_large_text_is_no_heading() {
    let big: Vec<L> = (0..5)
        .map(|i| {
            l(
                57.0,
                100.0 + i as f32 * 20.0,
                400.0,
                16.0,
                &format!("Large line {i}"),
            )
        })
        .collect();
    let body = vec![
        l(57.0, 300.0, 400.0, 10.0, "• first item"),
        l(57.0, 312.0, 400.0, 10.0, "goes on here"),
        l(57.0, 324.0, 400.0, 10.0, "- second item"),
    ];
    let filler: Vec<L> = (0..10)
        .map(|i| {
            l(
                57.0,
                400.0 + i as f32 * 12.0,
                500.0,
                10.0,
                "Body text body text",
            )
        })
        .collect();
    let lay = run(&[vec![big, body, filler]]);
    let kinds: Vec<&BlockKind> = lay.doc.parts[0].blocks.iter().map(|b| &b.kind).collect();
    assert_eq!(kinds[0], &BlockKind::Paragraph, "{:?}", lay.doc);
    assert_eq!(kinds[1], &BlockKind::ListItem);
    assert_eq!(lay.doc.parts[0].blocks[1].text, "first item goes on here");
    assert_eq!(kinds[2], &BlockKind::ListItem);
}

#[test]
fn without_font_runs_sizes_come_from_the_boxes() {
    let (b, _) = xml(&[two_column(1)]);
    let lay = layout(b.as_bytes(), None, 4).unwrap();
    assert!(lay.sizes_from_boxes);
    assert_eq!(lay.doc.parts[0].number, 4);
    // One page: its header is no repeat, and stays.
    assert!(
        md(&lay).contains("\n\n# Example Report\n\n"),
        "{}",
        md(&lay)
    );
}

/// 1 000 pages of 60 lines of eight words, as a long text book: the pass is
/// linear in the words. The bound is generous for a debug build; a
/// quadratic step over the lines would take far longer.
#[test]
fn a_thousand_page_book_is_laid_out_in_linear_time() {
    let words = "the example widget team reviewed the quarterly numbers";
    let page: Vec<Vec<L>> = (0..6)
        .map(|b| {
            (0..10)
                .map(|i| {
                    let y = 60.0 + (b * 10 + i) as f32 * 12.0;
                    l(57.0, y, 537.0, 10.0, words)
                })
                .collect()
        })
        .collect();
    let pages = vec![page; 1000];
    let (b, h) = xml(&pages);
    let started = Instant::now();
    let lay = layout(b.as_bytes(), Some(h.as_bytes()), 1).unwrap();
    let took = started.elapsed();
    assert_eq!(lay.doc.parts.len(), 1000);
    assert!(took < Duration::from_secs(30), "took {took:?}");
    eprintln!("1000 pages, 60 000 lines: {took:?}");
}

/// One page of 20 000 lines on one row (a table run wild), 20 000 lines
/// stacked on the same spot over as many runs, and 20 000 rows of four
/// one-glyph lines each at x-positions no other row uses (a "table" of
/// 80 000 columns, whose grid would be 1.6e9 slots): sorted and sampled,
/// never lines × lines, and the staggered rows are no table, so their
/// markdown is the size of their text, not of a grid.
#[test]
fn degenerate_pages_stay_linear() {
    let row: Vec<L> = (0..20_000)
        .map(|i| l(i as f32 * 3.0, 100.0, i as f32 * 3.0 + 2.0, 10.0, "x1"))
        .collect();
    let stacked: Vec<L> = (0..20_000)
        .map(|_| l(57.0, 100.0, 300.0, 10.0, "same"))
        .collect();
    // Rows 0.3 pt tall and 0.7 pt apart; each row's four lines 0.2 pt to
    // the right of the row above's.
    let staggered: Vec<Vec<L>> = (0..4)
        .map(|c| {
            (0..20_000)
                .map(|r| {
                    let x0 = 57.0 + c as f32 * 100.0 + r as f32 * 0.2;
                    l(x0, r as f32 * 0.7, x0 + 0.15, 0.3 / 1.1, "x")
                })
                .collect()
        })
        .collect();
    for page in [vec![row], vec![stacked], staggered] {
        let (b, h) = xml(&[page]);
        let started = Instant::now();
        let lay = layout(b.as_bytes(), Some(h.as_bytes()), 1).unwrap();
        let m = md(&lay);
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(lay.doc.parts.len(), 1);
        assert_eq!(lay.doc.counts().tables, 0);
        assert!(m.len() < 1 << 20, "{} bytes", m.len());
    }
}

#[test]
fn hostile_output_is_an_error_or_text_never_a_panic() {
    // Not XML: the error says where.
    let _ = layout(b"<doc><page><word>x</wo", None, 1);
    let e = layout(b"<doc><page></nope></page></doc><<<", None, 1).unwrap_err();
    assert!(e.contains("not readable XML"), "{e}");
    // Numbers that are no numbers, boxes backwards, words without a line,
    // lines without a page.
    let xml = br#"<word xMin="NaN" yMin="inf" xMax="-1e40" yMax="1e40">a&amp;b&#x41;&bogus;</word>
<page width="-5" height="x"><line xMin="9" yMin="9" xMax="1" yMax="1"><word>yy</word></line>
<flow><block><block><line><line><word>zz</word></line></line></block></block></flow></page>"#;
    let lay = layout(xml, Some(b"<pdf2xml><b><b><b><text font=\"9\">q</text>"), 1).unwrap();
    let text = render::text(&lay.doc, "\n");
    assert!(
        text.contains("a&bA") && text.contains("yy") && text.contains("zz"),
        "{text}"
    );
    // Deep nesting in the font runs is a count.
    let deep = format!(
        "<pdf2xml><page number=\"1\"><text>{}</text></page></pdf2xml>",
        "<b>".repeat(100_000)
    );
    assert!(layout(
        b"<doc><page width=\"1\" height=\"1\"></page></doc>",
        Some(deep.as_bytes()),
        1
    )
    .is_ok());
    // Nothing at all: no pages.
    assert!(layout(b"", None, 1).unwrap().doc.parts.is_empty());
}

#[test]
fn a_page_s_size_is_pdftotext_s_turned_as_pdftohtml_shows_it() {
    let a4 = (595.276, 841.89);
    // pdftohtml gives whole points; the exact size stays pdftotext's.
    assert_eq!(displayed_size(a4, Some((595.0, 841.0))), a4);
    assert_eq!(displayed_size(a4, Some((841.0, 595.0))), (841.89, 595.276));
    assert_eq!(displayed_size(a4, None), a4);
    // A square page cannot be told turned; nothing changes.
    assert_eq!(
        displayed_size((500.0, 500.0), Some((500.0, 500.0))),
        (500.0, 500.0)
    );
}
