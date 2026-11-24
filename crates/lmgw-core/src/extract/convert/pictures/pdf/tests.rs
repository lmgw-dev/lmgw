use lmgw_api_types::documents::convert::WarningCode;

use super::super::super::doc::Part;
use super::*;

fn r(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
    Rect { x0, y0, x1, y1 }
}

fn para(text: &str, rect: Rect) -> Block {
    Block::new(
        BlockKind::Paragraph,
        text.into(),
        Some(rect),
        Source::TextLayer,
    )
}

fn page(number: u32, blocks: Vec<Block>) -> Part {
    Part {
        kind: PartKind::Page,
        number,
        name: None,
        size: Some((595.0, 842.0)),
        source: Some(Source::TextLayer),
        blocks,
    }
}

fn img(page: u32, boxes: &[(Rect, &str)]) -> String {
    let mut s = format!(
        r#"<page number="{page}" position="absolute" top="0" left="0" height="842" width="595">"#
    );
    for (b, data) in boxes {
        s.push_str(&format!(
            r#"<image top="{}" left="{}" width="{}" height="{}" src="data:image/png;base64,{data}"/>"#,
            b.y0,
            b.x0,
            b.width(),
            b.height()
        ));
    }
    s.push_str("</page>");
    s
}

fn xml(pages: &[String]) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0"?><pdf2xml producer="poppler">{}</pdf2xml>"#,
        pages.concat()
    )
    .into_bytes()
}

fn kinds(p: &Part) -> Vec<&str> {
    p.blocks
        .iter()
        .map(|b| match b.kind {
            BlockKind::Figure => "figure",
            BlockKind::Caption => "caption",
            BlockKind::Paragraph => "para",
            BlockKind::Heading(_) => "heading",
            _ => "other",
        })
        .collect()
}

#[test]
fn image_elements_are_read_per_page_and_garbage_stops_with_a_note() {
    let x = xml(&[
        img(3, &[(r(72.0, 150.0, 300.0, 320.0), "AAAA")]),
        img(4, &[]),
    ]);
    let i = images(&x);
    assert_eq!(i.pages.len(), 2);
    assert_eq!(i.pages[0].number, 3);
    assert_eq!((i.pages[0].width, i.pages[0].height), (595.0, 842.0));
    assert_eq!(i.pages[0].boxes[0].rect, r(72.0, 150.0, 300.0, 320.0));
    assert!(i.error.is_none());
    // Numbers that are none, a negative size, and a cut-off file.
    let bad = br#"<pdf2xml><page number="1" height="nan" width="595"><image top="x" left="1" width="-5" height="3" src="a"/><image top="1" left="2" width="3"#;
    let i = images(bad);
    assert_eq!(i.pages[0].height, 0.0);
    // A negative width runs left from `left`.
    assert_eq!(i.pages[0].boxes[0].rect, r(-4.0, 0.0, 1.0, 3.0));
    assert!(i.error.is_some());
    // An image outside a page is not anywhere.
    assert!(images(
        br#"<pdf2xml><image top="1" left="1" width="1" height="1" src="a"/></pdf2xml>"#
    )
    .pages
    .is_empty());
}

#[test]
fn a_picture_goes_before_the_block_below_it_and_takes_its_caption() {
    let mut doc = Doc {
        parts: vec![page(
            1,
            vec![
                Block::new(
                    BlockKind::Heading(1),
                    "Example Report".into(),
                    Some(r(72.0, 60.0, 523.0, 80.0)),
                    Source::TextLayer,
                ),
                para("The example text before.", r(72.0, 100.0, 523.0, 140.0)),
                para("Figure 1: Example revenue", r(72.0, 330.0, 300.0, 345.0)),
                para("The example text after.", r(72.0, 360.0, 523.0, 400.0)),
            ],
        )],
    };
    let x = xml(&[img(1, &[(r(72.0, 150.0, 300.0, 320.0), "AAAA")])]);
    let found = place(&mut doc, &images(&x), &Settings::default());
    assert_eq!(
        kinds(&doc.parts[0]),
        ["heading", "para", "figure", "caption", "para"]
    );
    let p = &found.pictures[0];
    assert_eq!(p.caption.as_deref(), Some("Figure 1: Example revenue"));
    assert_eq!(p.before.as_deref(), Some("The example text before."));
    assert_eq!(p.part, 1);
    let fig = doc.parts[0].blocks[2].figure.as_ref().unwrap();
    assert_eq!(
        (fig.index, fig.caption.as_deref()),
        (0, Some("Figure 1: Example revenue"))
    );
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
}

#[test]
fn a_picture_in_the_right_column_is_read_with_that_column() {
    let mut doc = Doc {
        parts: vec![page(
            1,
            vec![
                para("left one", r(50.0, 100.0, 290.0, 300.0)),
                para("left two", r(50.0, 310.0, 290.0, 600.0)),
                para("right one", r(310.0, 100.0, 550.0, 200.0)),
                para("right two", r(310.0, 420.0, 550.0, 600.0)),
            ],
        )],
    };
    let x = xml(&[img(1, &[(r(310.0, 210.0, 550.0, 410.0), "AAAA")])]);
    place(&mut doc, &images(&x), &Settings::default());
    let texts: Vec<&str> = doc.parts[0]
        .blocks
        .iter()
        .map(|b| {
            if b.kind == BlockKind::Figure {
                "FIG"
            } else {
                b.text.as_str()
            }
        })
        .collect();
    assert_eq!(
        texts,
        ["left one", "left two", "right one", "FIG", "right two"]
    );
}

#[test]
fn logos_backgrounds_and_small_pictures_are_left_out_and_counted() {
    let logo = (r(20.0, 10.0, 120.0, 40.0), "TE9HTw");
    let mut doc = Doc {
        parts: (1..=3).map(|n| page(n, Vec::new())).collect(),
    };
    let x = xml(&[
        img(1, &[logo, (r(0.0, 0.0, 595.0, 842.0), "U0NBTg")]),
        img(2, &[logo, (r(100.0, 300.0, 110.0, 310.0), "U01BTEw")]),
        // The same logo drawn twice in one place is one picture.
        img(3, &[logo, logo, (r(72.0, 300.0, 400.0, 600.0), "QklH")]),
    ]);
    let found = place(&mut doc, &images(&x), &Settings::default());
    assert_eq!(found.pictures.len(), 1);
    assert_eq!(found.pictures[0].part, 3);
    let w = found
        .warnings
        .iter()
        .find(|w| w.code == WarningCode::PicturesLeftOut)
        .unwrap();
    assert!(w.message.contains("1 smaller"), "{}", w.message);
    assert!(
        w.message.contains("3 the same picture in the same place"),
        "{}",
        w.message
    );
    assert!(w.message.contains("1 covering"), "{}", w.message);
    assert_eq!(w.parts, [1, 2, 3]);
    // With no minimum the small one is kept; one page has no repeats.
    let mut one = Doc {
        parts: vec![page(2, Vec::new())],
    };
    let only = xml(&[img(2, &[logo, (r(100.0, 300.0, 110.0, 310.0), "U01BTEw")])]);
    let s = Settings {
        min_area: 0.0,
        ..Default::default()
    };
    assert_eq!(place(&mut one, &images(&only), &s).pictures.len(), 2);
}

#[test]
fn a_figure_caption_without_a_picture_is_a_vector_drawing_note() {
    let mut doc = Doc {
        parts: vec![page(
            4,
            vec![
                para("Figure 2: Example chart", r(72.0, 400.0, 300.0, 415.0)),
                para("Figures are discussed below.", r(72.0, 420.0, 300.0, 440.0)),
            ],
        )],
    };
    let x = xml(&[img(4, &[])]);
    let found = place(&mut doc, &images(&x), &Settings::default());
    let w = &found.warnings[0];
    assert_eq!(w.code, WarningCode::PicturesPartial);
    assert_eq!(w.parts, [4]);
    assert!(w.message.contains("layout model"), "{}", w.message);
}

#[test]
fn pictures_past_the_page_bound_are_placeholders_at_the_page_end() {
    let mut doc = Doc {
        parts: vec![page(1, vec![para("text", r(72.0, 100.0, 523.0, 120.0))])],
    };
    let boxes: Vec<(Rect, String)> = (0..PICTURES_PER_PAGE_MAX + 4)
        .map(|i| {
            let x = (i % 50) as f32 * 10.0;
            let y = 200.0 + (i / 50) as f32 * 10.0;
            (r(x, y, x + 5.0, y + 5.0), format!("{i}"))
        })
        .collect();
    let refs: Vec<(Rect, &str)> = boxes.iter().map(|(b, d)| (*b, d.as_str())).collect();
    let x = xml(&[img(1, &refs)]);
    let s = Settings {
        min_area: 0.0,
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let found = place(&mut doc, &images(&x), &s);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(found.pictures.len(), PICTURES_PER_PAGE_MAX + 4);
    let missing = found
        .pictures
        .iter()
        .filter(|p| p.missing.is_some())
        .count();
    assert_eq!(missing, 4);
    assert!(found
        .warnings
        .iter()
        .any(|w| w.message.contains("PICTURES_PER_PAGE_MAX")));
    // Indexes follow the blocks' order.
    let idx: Vec<u32> = doc.parts[0]
        .blocks
        .iter()
        .filter_map(|b| b.figure.as_ref().map(|f| f.index))
        .collect();
    assert_eq!(idx, (0..idx.len() as u32).collect::<Vec<_>>());
}

#[test]
fn no_picture_list_is_said_by_its_reason() {
    let mut doc = Doc {
        parts: vec![page(1, Vec::new())],
    };
    let missing = Images {
        missing: true,
        ..Default::default()
    };
    let f = place(&mut doc, &missing, &Settings::default());
    assert_eq!(f.warnings[0].code, WarningCode::ToolMissing);
    let gone = Images {
        gone: vec![
            Gone {
                range: Some((3, 4)),
                why: "example".into(),
                partial: false,
            },
            Gone {
                range: Some((7, 7)),
                why: "stops being readable".into(),
                partial: true,
            },
        ],
        ..Default::default()
    };
    let f = place(&mut doc, &gone, &Settings::default());
    assert!(
        f.warnings[0].message.contains("pages 3-4 (example)"),
        "{:?}",
        f.warnings
    );
    assert!(f.warnings[0].message.contains("text was read in full"));
    assert_eq!(f.warnings[0].parts, [3, 4]);
    assert!(f.warnings[1]
        .message
        .contains("page 7 stops being readable"));
}

#[test]
fn boxes_on_rotated_pages_and_of_mirrored_images_are_normalised() {
    // poppler 26.08 on a 612 × 792 page with /Rotate 90 and 180, and a
    // mirroring matrix: negative widths and heights.
    let x = br#"<pdf2xml><page number="2" height="612" width="792"><image top="300" left="400" width="100" height="-200" src="a"/></page><page number="3" height="792" width="612"><image top="500" left="512" width="-200" height="-100" src="b"/></page></pdf2xml>"#;
    let i = images(x);
    assert_eq!((i.pages[0].width, i.pages[0].height), (792.0, 612.0));
    assert_eq!(i.pages[0].boxes[0].rect, r(400.0, 100.0, 500.0, 300.0));
    assert_eq!(i.pages[1].boxes[0].rect, r(312.0, 400.0, 512.0, 500.0));
    // Placed against the page as displayed, the layout's record of it
    // (pdftohtml's page from the font run): a box past x = 612 on a
    // landscape page stays whole, one past 792 is cut there.
    let x = br#"<pdf2xml><page number="2" height="612" width="792"><image top="100" left="700" width="-300" height="200" src="a"/><image top="300" left="600" width="300" height="250" src="c"/></page></pdf2xml>"#;
    let mut doc = Doc {
        parts: vec![Part {
            size: Some((792.0, 612.0)),
            ..page(2, Vec::new())
        }],
    };
    let found = place(&mut doc, &images(x), &Settings::default());
    assert_eq!(found.pictures.len(), 2);
    assert_eq!(found.pictures[0].bbox, Some(r(400.0, 100.0, 700.0, 300.0)));
    assert_eq!(found.pictures[1].bbox, Some(r(600.0, 300.0, 792.0, 550.0)));
    // A part without a size: the picture run's own page.
    let mut doc = Doc {
        parts: vec![Part {
            size: None,
            ..page(2, Vec::new())
        }],
    };
    let found = place(&mut doc, &images(x), &Settings::default());
    assert_eq!(found.pictures[1].bbox, Some(r(600.0, 300.0, 792.0, 550.0)));
}

#[test]
fn a_watermark_in_mid_page_on_most_pages_is_left_out_once_counted() {
    let mark = (r(150.0, 350.0, 450.0, 500.0), "V0FURVI");
    let mut doc = Doc {
        parts: (1..=4).map(|n| page(n, Vec::new())).collect(),
    };
    let x = xml(&[
        img(1, &[mark]),
        img(2, &[mark, (r(72.0, 100.0, 400.0, 300.0), "RklH")]),
        img(3, &[mark]),
        img(4, &[]),
    ]);
    let found = place(&mut doc, &images(&x), &Settings::default());
    assert_eq!(found.pictures.len(), 1, "{:?}", found.pictures);
    let w = found
        .warnings
        .iter()
        .find(|w| w.code == WarningCode::PicturesLeftOut)
        .unwrap();
    assert!(
        w.message.contains("3 the same picture in the same place"),
        "{}",
        w.message
    );
    assert_eq!(w.parts, [1, 2, 3]);
}

#[test]
fn a_hundred_thousand_draws_on_a_page_cost_their_count() {
    // 100 000 distinct 160 × 160 pt boxes (each over the 5% area) on one
    // page with text: the bound places 256, the rest go to the page's end
    // without a caption lookup.
    let mut boxes = Vec::with_capacity(100_000);
    'outer: for y in 0..600 {
        for x in 0..400 {
            if boxes.len() == 100_000 {
                break 'outer;
            }
            let (x, y) = (x as f32, y as f32);
            boxes.push(ImgBox {
                rect: r(x, y, x + 160.0, y + 160.0),
                hash: 1,
            });
        }
    }
    let imgs = Images {
        pages: vec![PageImages {
            number: 1,
            width: 595.0,
            height: 842.0,
            boxes,
        }],
        ..Default::default()
    };
    let blocks: Vec<Block> = (0..200)
        .map(|i| {
            let y = 100.0 + i as f32 * 3.0;
            para("Figure 1: Example", r(72.0, y, 523.0, y + 2.0))
        })
        .collect();
    let mut doc = Doc {
        parts: vec![page(1, blocks)],
    };
    let started = std::time::Instant::now();
    let found = place(&mut doc, &imgs, &Settings::default());
    let took = started.elapsed();
    assert!(took < std::time::Duration::from_secs(1), "{took:?}");
    assert_eq!(found.pictures.len(), 100_000);
    let over = found
        .pictures
        .iter()
        .filter(|p| p.missing.is_some())
        .count();
    assert_eq!(over, 100_000 - PICTURES_PER_PAGE_MAX);
    assert!(found
        .pictures
        .iter()
        .filter(|p| p.missing.is_some())
        .all(|p| p.caption.is_none() && p.before.is_none()));
}

#[test]
fn an_existing_figure_block_over_an_image_is_one_figure_with_poppler_s_box() {
    // A figure region another reader found (a layout model's), placed
    // between two paragraphs, a little larger than the image in it.
    let mut region = Block::new(
        BlockKind::Figure,
        String::new(),
        Some(r(60.0, 140.0, 320.0, 330.0)),
        Source::Onnx,
    );
    region.figure = None;
    let mut doc = Doc {
        parts: vec![page(
            1,
            vec![
                para("The example text before.", r(72.0, 100.0, 523.0, 130.0)),
                region,
                para("Figure 1: Example chart", r(72.0, 335.0, 300.0, 350.0)),
                para("The example text after.", r(72.0, 400.0, 523.0, 440.0)),
            ],
        )],
    };
    let x = xml(&[img(
        1,
        &[
            (r(72.0, 150.0, 300.0, 320.0), "RklH"),
            // Another image beside it, outside the region.
            (r(330.0, 150.0, 520.0, 320.0), "T1RIRVI"),
        ],
    )]);
    let found = place(&mut doc, &images(&x), &Settings::default());
    assert_eq!(found.pictures.len(), 2);
    let b = &doc.parts[0].blocks;
    assert_eq!(b[1].kind, BlockKind::Figure);
    assert_eq!(b[1].source, Source::Onnx);
    assert_eq!(b[1].bbox, Some(r(72.0, 150.0, 300.0, 320.0)));
    assert_eq!(found.pictures[0].bbox, Some(r(72.0, 150.0, 300.0, 320.0)));
    assert_eq!(
        found.pictures[0].caption.as_deref(),
        Some("Figure 1: Example chart")
    );
    let figures = b.iter().filter(|b| b.kind == BlockKind::Figure).count();
    assert_eq!(figures, 2);
}

#[test]
fn a_caption_the_model_marked_is_taken_once_before_any_paragraph() {
    let fig = |y0: f32, y1: f32| {
        Block::new(
            BlockKind::Figure,
            String::new(),
            Some(r(72.0, y0, 400.0, y1)),
            Source::Onnx,
        )
    };
    let marked = |text: &str, rect: Rect| {
        Block::new(
            BlockKind::Caption,
            text.into(),
            Some(rect),
            Source::TextLayer,
        )
    };
    let mut doc = Doc {
        parts: vec![page(
            1,
            vec![
                // A paragraph that reads as a caption above the first figure,
                // the model's caption below it: the model's wins, the
                // paragraph stays a paragraph.
                para("Chart: example lead", r(72.0, 80.0, 300.0, 95.0)),
                fig(100.0, 200.0),
                marked("An example chart", r(72.0, 205.0, 300.0, 220.0)),
                // The next figure right below that caption does not take it
                // again; the table caption under it is the table's.
                fig(225.0, 320.0),
                marked("Table 2: Example values", r(72.0, 325.0, 300.0, 340.0)),
            ],
        )],
    };
    let found = place(&mut doc, &Images::default(), &Settings::default());
    let captions: Vec<Option<&str>> = found
        .pictures
        .iter()
        .map(|p| p.caption.as_deref())
        .collect();
    assert_eq!(captions, [Some("An example chart"), None]);
    let b = &doc.parts[0].blocks;
    assert_eq!(b[0].kind, BlockKind::Paragraph);
    assert_eq!(found.pictures[0].source, Source::Onnx);
    // No caption without its picture is said for the marked ones.
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
}

#[test]
fn another_reader_s_figures_are_left_out_as_poppler_s_pictures_would_be() {
    let region =
        |rect: Rect| Block::new(BlockKind::Figure, String::new(), Some(rect), Source::Onnx);
    let logo = (r(56.0, 28.0, 141.0, 57.0), "TE9HTw");
    // A page-sized background behind a real figure the model found.
    let cover = (r(0.0, 0.0, 595.0, 842.0), "QkFDSw");
    let mut doc = Doc {
        parts: (1..=3)
            .map(|n| {
                let mut blocks = vec![region(r(56.4, 27.7, 141.4, 56.9))];
                if n == 2 {
                    // A tiny drawing, and a figure over the background.
                    blocks.push(region(r(300.0, 300.0, 320.0, 320.0)));
                    blocks.push(region(r(72.0, 400.0, 400.0, 600.0)));
                }
                page(n, blocks)
            })
            .collect(),
    };
    let x = xml(&[img(1, &[logo]), img(2, &[logo, cover]), img(3, &[logo])]);
    let found = place(&mut doc, &images(&x), &Settings::default());
    // The logo's regions went with the logo; the tiny drawing is counted
    // small; the figure over the background stays, with the region's box.
    assert_eq!(found.pictures.len(), 1, "{:?}", found.pictures);
    assert_eq!(found.pictures[0].bbox, Some(r(72.0, 400.0, 400.0, 600.0)));
    assert!(doc.parts[0].blocks.is_empty() && doc.parts[2].blocks.is_empty());
    let w = found
        .warnings
        .iter()
        .find(|w| w.code == WarningCode::PicturesLeftOut)
        .unwrap();
    assert!(w.message.contains("1 smaller"), "{}", w.message);
    assert!(w.message.contains("3 the same picture"), "{}", w.message);
    assert!(w.message.contains("1 covering"), "{}", w.message);
}
