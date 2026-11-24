//! The region pass's checks (WP8 review): turned pages placed by their
//! displayed size, pages mostly inside pictures, staggered table regions,
//! and numbered list items marked by the pass instead of read from text.

use super::super::super::doc::render::{markdown, TableStyle};
use super::super::super::layout_model::{Implausible, Label, PageRegions, Region, Regions};
use super::*;

fn run_with(pages: &[Vec<Vec<L>>], regions: &Regions) -> Layout {
    let (b, h) = xml(pages);
    layout_with(b.as_bytes(), Some(h.as_bytes()), 1, Some(regions)).unwrap()
}

/// One region; the box in points on the synthetic 595 × 842 page.
fn reg(label: Label, score: f32, x0: f32, y0: f32, x1: f32, y1: f32) -> Region {
    Region::checked(label, score, x0 / 595.0, y0 / 842.0, x1 / 595.0, y1 / 842.0).unwrap()
}

fn one_page(regions: Vec<Region>) -> Regions {
    let mut r = Regions::default();
    r.pages.insert(1, PageRegions::Found(regions));
    r
}

fn md(lay: &Layout) -> String {
    markdown(&lay.doc, TableStyle::Padded, "\n\n")
}

/// A page of `n` body lines in one column from `y`.
fn column(n: usize, y: f32) -> Vec<L> {
    (0..n)
        .map(|i| {
            l(
                57.0,
                y + 12.0 * i as f32,
                300.0,
                10.0,
                &format!("Example line {i}."),
            )
        })
        .collect()
}

#[test]
fn a_page_mostly_inside_pictures_goes_by_the_geometry() {
    // A slide: a title, `n` lines in what the model takes for one picture
    // over most of the page, and the rest of the twelve below it.
    let slide = |n: usize| {
        vec![
            vec![bold(l(57.0, 30.0, 300.0, 20.0, "Example Slide"))],
            column(n, 60.0),
            column(12 - n, 700.0),
        ]
    };
    let picture = |x1: f32, y1: f32| {
        one_page(vec![
            reg(Label::Title, 0.9, 50.0, 28.0, 310.0, 52.0),
            reg(Label::Picture, 0.9, 50.0, 58.0, x1, y1),
            reg(Label::Text, 0.9, 50.0, 650.0, 310.0, 830.0),
        ])
    };
    // Seven of thirteen lines in a picture over two fifths of the page
    // by area (300 x 600 of 595 x 842 is 36 %): lines alone do not
    // decide, the area must pass PICTURE_PAGE_MIN_AREA too.
    let page = vec![slide(7)];
    let plain = run(&page).doc;
    let lay = run_with(&page, &picture(350.0, 650.0));
    assert!(lay.model.implausible.is_empty());
    assert_eq!(lay.model.pages[&1].picture_lines, 7);
    // The same lines in a picture over most of the page: the geometry.
    let lay = run_with(&page, &picture(590.0, 650.0));
    assert_eq!(lay.model.implausible, vec![(1, Implausible::PictureText)]);
    assert_eq!(lay.doc, plain);
    assert!(!md(&lay).contains("<!-- image -->"), "{}", md(&lay));
    // Six of thirteen in it: the picture stays, its lines in the figure block.
    let page = vec![slide(6)];
    let lay = run_with(&page, &picture(590.0, 650.0));
    assert!(lay.model.implausible.is_empty());
    assert_eq!(lay.model.pages[&1].picture_lines, 6);
    assert!(md(&lay).contains("<!-- image -->"), "{}", md(&lay));
}

#[test]
fn a_table_region_too_sparse_for_a_grid_is_paragraphs() {
    // A staircase: each line its own row and its own column, so a 6 × 6
    // grid holds six cells (a sixth of it, under TABLE_MIN_FILL).
    let stairs: Vec<L> = (0..6)
        .map(|i| {
            let x = 57.0 + 80.0 * i as f32;
            l(
                x,
                60.0 + 14.0 * i as f32,
                x + 60.0,
                10.0,
                &format!("Step {i}"),
            )
        })
        .collect();
    let page = vec![stairs, column(3, 160.0)];
    let regions = one_page(vec![
        reg(Label::Table, 0.9, 50.0, 58.0, 540.0, 145.0),
        reg(Label::Text, 0.9, 50.0, 158.0, 310.0, 200.0),
    ]);
    let lay = run_with(std::slice::from_ref(&page), &regions);
    assert!(lay.model.implausible.is_empty());
    let kinds: Vec<bool> = lay.doc.parts[0]
        .blocks
        .iter()
        .map(|b| matches!(b.kind, BlockKind::Table(_)))
        .collect();
    assert!(kinds.iter().all(|t| !t), "{:?}", lay.doc.parts[0].blocks);
    let text = md(&lay);
    assert!(text.contains("Step 0") && text.contains("Step 5"), "{text}");
    // A filled region is a table as before.
    let grid: Vec<Vec<L>> = (0..2)
        .map(|c| {
            (0..4)
                .map(|r| {
                    let x = 57.0 + 150.0 * c as f32;
                    l(
                        x,
                        60.0 + 14.0 * r as f32,
                        x + 60.0,
                        10.0,
                        &format!("Cell {r} {c}"),
                    )
                })
                .collect()
        })
        .collect();
    let lay = run_with(
        &[grid],
        &one_page(vec![reg(Label::Table, 0.9, 50.0, 58.0, 300.0, 120.0)]),
    );
    assert!(md(&lay).contains("| Cell 0 0 | Cell 0 1 |"), "{}", md(&lay));
}

#[test]
fn a_list_item_is_numbered_only_without_a_bullet() {
    let page = vec![
        vec![l(57.0, 60.0, 300.0, 10.0, "1. First example item")],
        vec![l(57.0, 74.0, 300.0, 10.0, "• 24. Dezember: an example day")],
        vec![l(57.0, 88.0, 300.0, 10.0, "12) # not a heading")],
    ];
    let regions = one_page(vec![
        reg(Label::ListItem, 0.9, 50.0, 58.0, 310.0, 72.0),
        reg(Label::ListItem, 0.9, 50.0, 72.5, 310.0, 86.0),
        reg(Label::ListItem, 0.9, 50.0, 86.5, 310.0, 100.0),
    ]);
    let lay = run_with(std::slice::from_ref(&page), &regions);
    let kinds: Vec<&BlockKind> = lay.doc.parts[0].blocks.iter().map(|b| &b.kind).collect();
    assert_eq!(
        kinds,
        vec![
            &BlockKind::NumberedItem("1.".into()),
            &BlockKind::ListItem,
            &BlockKind::NumberedItem("12)".into()),
        ]
    );
    assert_eq!(
        md(&lay),
        "1. First example item\n\n- 24\\. Dezember: an example day\n\n12) \\# not a heading"
    );
    // Without the model the same lines are what WP3 made of them.
    let plain = run(std::slice::from_ref(&page));
    assert!(plain.doc.parts[0]
        .blocks
        .iter()
        .all(|b| !matches!(b.kind, BlockKind::NumberedItem(_))));
}

/// A page with `/Rotate` 90 and 270, through real poppler: the regions are
/// shares of the page as displayed (792 × 612), and they find their lines
/// by the size `pdftohtml` gives. With `pdftotext`'s unturned size
/// (612 × 792) the heading region would land elsewhere.
#[cfg(unix)]
#[tokio::test]
async fn a_turned_page_s_regions_land_on_its_lines() {
    use crate::extract::convert::pool;
    use crate::extract::pdf::{self, geometry};
    if !pdf::available().await {
        eprintln!("skipped: poppler is not on PATH");
        return;
    }
    for rotate in [90, 270] {
        let file = bytes::Bytes::from(crate::extract::test_files::pdf_turned(
            612.0,
            792.0,
            rotate,
            &[
                // Wholly right of x = 612, where the unturned page ends.
                (620.0, 400.0, 20.0, "Example heading"),
                (
                    60.0,
                    100.0,
                    11.0,
                    "An example line of body text on the left.",
                ),
                (60.0, 116.0, 11.0, "Another example line of body text."),
            ],
        ));
        let slot = pool::global().acquire().await;
        let bin = std::path::Path::new(pdf::PDFTOTEXT);
        let file = crate::extract::convert::ocr::spool::Spool::of(None, file).await;
        let bbox = geometry::bbox_layout_with(&slot, bin, &file, None, pdf::PDF_TIMEOUT)
            .await
            .unwrap();
        let html = std::path::Path::new(geometry::PDFTOHTML);
        let fonts = geometry::font_runs_with(&slot, html, &file, None, pdf::PDF_TIMEOUT)
            .await
            .unwrap();
        drop(slot);
        // Shares of the displayed 792 × 612 page.
        let at = |label, x0: f32, y0: f32, x1: f32, y1: f32| {
            Region::checked(label, 0.9, x0 / 792.0, y0 / 612.0, x1 / 792.0, y1 / 612.0).unwrap()
        };
        let regions = {
            let mut r = Regions::default();
            r.pages.insert(
                1,
                PageRegions::Found(vec![
                    at(Label::SectionHeader, 610.0, 395.0, 785.0, 425.0),
                    at(Label::Text, 50.0, 95.0, 400.0, 135.0),
                ]),
            );
            r
        };
        let lay = layout_with(&bbox, Some(&fonts), 1, Some(&regions)).unwrap();
        assert!(
            lay.model.implausible.is_empty(),
            "rotate {rotate}: {:?}",
            lay.model
        );
        let blocks = &lay.doc.parts[0].blocks;
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Heading(_)) && b.text == "Example heading"),
            "rotate {rotate}: {blocks:?}"
        );
        assert!(
            blocks.iter().any(|b| b.kind == BlockKind::Paragraph
                && b.text
                    == "An example line of body text on the left. Another example line of body \
                        text."),
            "rotate {rotate}: {blocks:?}"
        );
        // Without pdftohtml's size the turned page is not placed at all.
        let lay = layout_with(&bbox, None, 1, Some(&regions)).unwrap();
        assert_eq!(lay.model.implausible, vec![(1, Implausible::UnknownSize)]);
    }
}
