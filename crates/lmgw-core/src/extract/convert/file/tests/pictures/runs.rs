//! The picture runs beside the text (design §5.3, WP10 review round):
//! rotated pages and mirrored images, the `pdfimages` gate, a run that
//! fails costing its pictures only, `output: text` without picture work,
//! and the request budget counting a picture's bytes once.

use std::time::Duration;

use lmgw_api_types::documents::convert::Pictures;

use super::super::super::pictured;
use super::*;
use crate::extract::convert::doc::{Block, BlockKind, Doc, Figure, Part, PartKind, Rect, Source};
use crate::extract::convert::pictures::pdf::runs;
use crate::extract::convert::pictures::{self as pics, Image, Picture};

const RED: [u8; 3] = [220, 20, 20];

/// The colour in the middle of a PNG, and its size.
fn middle(png: &[u8]) -> ([u8; 3], (u32, u32)) {
    let mut dec = png::Decoder::new(std::io::Cursor::new(png));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = dec.read_info().unwrap();
    let mut buf = vec![0u8; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut buf).unwrap();
    let ch = info.color_type.samples();
    let at = (info.height as usize / 2) * info.line_size + (info.width as usize / 2) * ch;
    (
        [buf[at], buf[at + 1], buf[at + 2]],
        (info.width, info.height),
    )
}

fn referenced() -> Plan {
    mode(Pictures::Referenced, Output::Markdown)
}

#[tokio::test]
async fn pictures_on_rotated_pages_and_mirrored_ones_are_found_and_cropped() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    // A red image of 300 × 200 pt at the same place on each page, the page
    // turned 0/90/180/270; then one drawn mirrored both ways. Each has its
    // own pixel count, so none repeats another.
    let page = |rotate: i32, flip: bool| {
        let px = (4 + rotate as u32 / 90 + u32::from(flip) * 5, 2);
        let mut d = Drawn::at(100.0, 300.0, 300.0, 200.0, px, RED);
        (d.flip_x, d.flip_y) = (flip, flip);
        let mut p = Page::new(vec![(72.0, 80.0, 12.0, "Example text")], vec![d]);
        p.rotate = rotate;
        p
    };
    let pdf = build::pdf(&[
        page(0, false),
        page(90, false),
        page(180, false),
        page(270, false),
        page(0, true),
    ]);
    let d = convert(input(pdf), &referenced(), &facts).await.doc;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    assert_eq!(d.pictures.len(), 5, "{:?}", d.warnings);
    for (p, turned) in d.pictures.iter().zip([false, true, false, true, false]) {
        let [x0, y0, x1, y1] = p.bbox.unwrap();
        // Inside the page as displayed (842 × 595 when turned).
        let (w, h) = if turned {
            (842.0, 595.0)
        } else {
            (595.0, 842.0)
        };
        assert!(x0 >= 0.0 && y0 >= 0.0 && x1 <= w && y1 <= h, "{p:?}");
        let size = if turned {
            (200.0, 300.0)
        } else {
            (300.0, 200.0)
        };
        assert!(((x1 - x0) - size.0).abs() < 1.0, "{p:?}");
        assert!(((y1 - y0) - size.1).abs() < 1.0, "{p:?}");
        let png = base64::engine::general_purpose::STANDARD
            .decode(p.data_base64.as_ref().unwrap())
            .unwrap();
        let (rgb, px) = middle(&png);
        assert_eq!(rgb, RED, "page {}", p.part);
        // 144 dpi: twice the points.
        assert!(px.0.abs_diff((size.0 * 2.0) as u32) <= 2, "{px:?}");
    }
    assert!(
        !d.warnings
            .iter()
            .any(|w| w.code == WarningCode::PicturesLeftOut),
        "{:?}",
        d.warnings
    );
}

/// Pages that are one page-sized image each, the first with text.
fn scan(pages: usize) -> Vec<u8> {
    let page = |text: bool| {
        Page::new(
            if text {
                vec![(72.0, 80.0, 12.0, "Example text")]
            } else {
                Vec::new()
            },
            vec![Drawn::at(
                0.0,
                0.0,
                595.0,
                842.0,
                (595, 842),
                [240, 240, 240],
            )],
        )
    };
    let all: Vec<Page<'_>> = (0..pages).map(|i| page(i == 0)).collect();
    build::pdf(&all)
}

#[tokio::test]
async fn a_scan_s_page_covers_are_counted_from_the_list_and_text_does_no_picture_work() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    let d = convert(
        input(scan(3)),
        &mode(Pictures::Placeholder, Output::Markdown),
        &facts,
    )
    .await
    .doc;
    assert!(d.pictures.is_empty());
    let w = d
        .warnings
        .iter()
        .find(|w| w.code == WarningCode::PicturesLeftOut)
        .expect("left out");
    assert!(w.message.contains("3 covering"), "{}", w.message);
    assert_eq!(w.parts, [1, 2, 3]);
    // output text: no gate ran, so nothing was counted.
    let d = convert(
        input(scan(3)),
        &mode(Pictures::Placeholder, Output::Text),
        &facts,
    )
    .await
    .doc;
    assert!(d.content.unwrap().contains("Example text"));
    assert!(
        !d.warnings.iter().any(|w| matches!(
            w.code,
            WarningCode::PicturesLeftOut | WarningCode::PicturesPartial
        )),
        "{:?}",
        d.warnings
    );
}

fn bins() -> runs::Bins {
    runs::Bins {
        html: "pdftohtml".into(),
        info: "pdfinfo".into(),
        list: "pdfimages".into(),
    }
}

#[tokio::test]
async fn a_picture_run_that_times_out_or_overflows_costs_its_pages_pictures_only() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    let pdf = Bytes::from(report());
    let tight =
        |timeout: Duration, output: u64| crate::extract::pdf::wide::Within { timeout, output };
    for (bounds, said) in [
        (tight(Duration::ZERO, 1 << 30), "PDF_TIMEOUT"),
        (tight(Duration::from_secs(60), 16), "MAX_UNCOMPRESSED_BYTES"),
    ] {
        let src = std::sync::Arc::new(
            crate::extract::convert::ocr::spool::Spool::of(None, pdf.clone()).await,
        );
        let found = runs::find_within(&bins(), src, None, None, 0.05, bounds).await;
        assert!(found.pages.is_empty());
        assert!(!found.gone.is_empty());
        for g in &found.gone {
            assert!(!g.partial);
            assert!(g.why.contains(said), "{}", g.why);
        }
        // Placed: the text keeps everything, the note names the pages.
        let mut doc = Doc {
            parts: vec![Part {
                kind: PartKind::Page,
                number: 1,
                name: None,
                size: Some((595.0, 842.0)),
                source: Some(Source::TextLayer),
                blocks: Vec::new(),
            }],
        };
        let f = pics::pdf::place(&mut doc, &found, &pics::Settings::default());
        let w = &f.warnings[0];
        assert_eq!(w.code, WarningCode::PicturesPartial);
        assert!(w.message.contains("text was read in full"), "{}", w.message);
        assert!(w.message.contains(said), "{}", w.message);
    }
    // The same file with the real bounds: its pictures, and the text as ever.
    let d = convert(input(report()), &referenced(), &facts).await.doc;
    assert_eq!(d.pictures.len(), 2);
    assert!(d.content.unwrap().contains("Figure 2: Example photo"));
}

#[tokio::test]
async fn pages_with_figures_run_apart_and_the_answer_keeps_reading_order() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    // Six pages, a figure on 1, 2, 3, 5 and 6, none on 4: four runs at
    // least (4 splits them), the pictures in page order.
    let colours = [
        [200, 0, 0],
        [0, 200, 0],
        [0, 0, 200],
        [9, 9, 9],
        [200, 200, 0],
        [0, 200, 200],
    ];
    let pages: Vec<Page<'_>> = colours
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let images = if i == 3 {
                Vec::new()
            } else {
                vec![Drawn::at(72.0, 200.0, 300.0, 200.0, (3, 2), *c)]
            };
            Page::new(vec![(72.0, 80.0, 12.0, "Example page text")], images)
        })
        .collect();
    let d = convert(input(build::pdf(&pages)), &referenced(), &facts)
        .await
        .doc;
    let parts: Vec<u32> = d.pictures.iter().map(|p| p.part).collect();
    assert_eq!(parts, [1, 2, 3, 5, 6]);
    let ids: Vec<&str> = d.pictures.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["p1", "p2", "p3", "p4", "p5"]);
    let src =
        crate::extract::convert::ocr::spool::Spool::of(None, Bytes::from(build::pdf(&pages))).await;
    let found = runs::find(&bins(), std::sync::Arc::new(src), None, None, 0.05).await;
    let numbers: Vec<u32> = found.pages.iter().map(|p| p.number).collect();
    assert_eq!(numbers, [1, 2, 3, 5, 6]);
}

/// A one-page doc with one figure whose picture has `n` bytes, reserved
/// from `budget` as the export reserves it.
fn pictured_doc(n: usize, budget: &OutputBudget) -> (Body, pics::Found) {
    let r = Rect {
        x0: 72.0,
        y0: 100.0,
        x1: 372.0,
        y1: 300.0,
    };
    let mut fig = Block::new(BlockKind::Figure, String::new(), Some(r), Source::TextLayer);
    fig.figure = Some(Box::new(Figure {
        index: 0,
        caption: None,
        description: None,
    }));
    let doc = Doc {
        parts: vec![Part {
            kind: PartKind::Page,
            number: 1,
            name: None,
            size: Some((595.0, 842.0)),
            source: Some(Source::TextLayer),
            blocks: vec![
                Block::new(
                    BlockKind::Paragraph,
                    "Example text".into(),
                    None,
                    Source::TextLayer,
                ),
                fig,
            ],
        }],
    };
    let mut p = Picture::new(1, Some(r), Source::TextLayer);
    p.image = Some(Image {
        bytes: Bytes::from(vec![7u8; n]),
        mime: "image/png",
        ext: "png",
        width_px: None,
        height_px: None,
    });
    let held = pics::reserve(n, 0);
    assert!(budget.try_take(held));
    let found = pics::Found {
        pictures: vec![p],
        charged: held,
        ..Default::default()
    };
    (Body::Doc(doc), found)
}

#[test]
fn a_picture_s_bytes_count_once_so_an_answer_that_fits_is_not_refused() {
    // 300 000 bytes are 400 000 in base64; the answer is that and its text,
    // under 600 000. Counting the bytes when cropped and again in the list
    // (the review's double count) passed 600 000 and failed the file.
    for (pictures, output) in [
        (Pictures::Referenced, Output::Markdown),
        (Pictures::Embedded, Output::Markdown),
        (Pictures::Embedded, Output::Json),
    ] {
        let budget = OutputBudget::with_limit(600_000);
        let (body, found) = pictured_doc(300_000, &budget);
        let plan = mode(pictures, output);
        let out = pictured::output(body, found, &plan, &budget, 0)
            .unwrap_or_else(|e| panic!("{pictures:?} {output:?}: {e:?}"));
        let text = out.content.as_ref().map_or(0, String::len);
        let structure = out
            .structure
            .as_ref()
            .map_or(0, |p| crate::extract::convert::doc_json::size(p));
        let list = pics::wire_size(&out.pictures);
        // What stays counted is the answer, once.
        assert_eq!(
            budget.used(),
            (text + structure + list) as u64,
            "{pictures:?} {output:?}"
        );
    }
}

#[test]
fn a_request_that_ran_out_stays_refused_for_later_files() {
    let budget = OutputBudget::with_limit(600_000);
    let (body, found) = pictured_doc(300_000, &budget);
    // Another file's text takes what is left and more: the request is spent.
    assert!(budget.take(300_000).is_err());
    // This file's output fails although its picture was reserved, and its
    // reservation coming back does not reopen the budget.
    assert!(pictured::output(body, found, &referenced(), &budget, 0).is_err());
    assert!(budget.take(1).is_err());
}

#[tokio::test]
async fn a_caption_poppler_gives_with_the_paragraph_under_it_keeps_that_paragraph() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    // The caption's line, then three full lines of text a line below it:
    // pdftotext gives them as one block.
    let long = "The example text after the chart goes on over the whole width of the page here";
    let pdf = build::pdf(&[Page::new(
        vec![
            (72.0, 80.0, 11.0, "The example text before the chart."),
            (72.0, 350.0, 11.0, "Figure 1: Example chart"),
            (72.0, 364.0, 11.0, long),
            (72.0, 378.0, 11.0, long),
            (72.0, 392.0, 11.0, "and ends here."),
        ],
        vec![Drawn::at(72.0, 140.0, 300.0, 200.0, (3, 2), RED)],
    )]);
    let d = convert(
        input(pdf),
        &mode(Pictures::Placeholder, Output::Json),
        &facts,
    )
    .await
    .doc;
    let v = serde_json::to_value(d.structure.unwrap()).unwrap();
    let blocks = v[0]["blocks"].as_array().unwrap();
    let kinds: Vec<&str> = blocks.iter().map(|b| b["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["paragraph", "figure", "caption", "paragraph"],
        "{v}"
    );
    assert_eq!(blocks[2]["text"], "Figure 1: Example chart");
    assert_eq!(blocks[1]["caption"], "Figure 1: Example chart");
    assert!(blocks[3]["text"]
        .as_str()
        .unwrap()
        .ends_with("and ends here."));
}
