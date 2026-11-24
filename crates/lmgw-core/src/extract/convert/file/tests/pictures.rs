//! Picture export end to end (design §5.3, WP10): real poppler on PDFs
//! with raster images, and office files with media.

use base64::Engine as _;
use lmgw_api_types::documents::convert::{Pictures, WarningCode};

use super::*;

pub(super) mod build;
mod deadline;
mod model;
mod runs;

use build::{Drawn, Page};

fn logo() -> Drawn {
    Drawn::at(40.0, 20.0, 90.0, 30.0, (12, 4), [200, 120, 0])
}

/// Two pages: a heading, a chart with its caption, text; a photo with its
/// caption; the same logo at the top of both.
fn report() -> Vec<u8> {
    build::pdf(&[
        Page::new(
            vec![
                (72.0, 80.0, 20.0, "Example Report"),
                (72.0, 120.0, 11.0, "The example text before the chart."),
                (72.0, 350.0, 11.0, "Figure 1: Example chart"),
                (72.0, 380.0, 11.0, "The example text after the chart."),
            ],
            vec![
                logo(),
                Drawn::at(72.0, 140.0, 300.0, 200.0, (30, 20), [10, 60, 200]),
            ],
        ),
        Page::new(
            vec![
                (72.0, 120.0, 11.0, "The second page starts here."),
                (72.0, 360.0, 11.0, "Figure 2: Example photo"),
            ],
            vec![
                logo(),
                Drawn::at(72.0, 140.0, 200.0, 200.0, (20, 20), [20, 160, 40]),
            ],
        ),
    ])
}

pub(super) fn mode(m: Pictures, output: Output) -> Plan {
    plan_of(ConvertOptions {
        pictures: m,
        output,
        ..Default::default()
    })
}

#[tokio::test]
async fn a_pdf_s_pictures_are_placeholders_in_reading_order_with_their_captions() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    let d = convert(
        input(report()),
        &mode(Pictures::Placeholder, Output::Markdown),
        &facts,
    )
    .await
    .doc;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    let md = d.content.unwrap();
    let before = md.find("text before the chart").unwrap();
    let image = md.find("<!-- image -->").unwrap();
    let caption = md.find("Figure 1: Example chart").unwrap();
    assert!(before < image && image < caption, "{md}");
    assert_eq!(md.matches("<!-- image -->").count(), 2, "{md}");
    assert_eq!(d.stats.pictures, 2);
    assert_eq!(
        d.pictures[0].caption.as_deref(),
        Some("Figure 1: Example chart")
    );
    assert_eq!(d.pictures[1].part, 2);
    assert!(d.pictures.iter().all(|p| p.data_base64.is_none()));
    // The logo at the top of both pages is left out and said.
    let w = d
        .warnings
        .iter()
        .find(|w| w.code == WarningCode::PicturesLeftOut)
        .unwrap();
    assert!(
        w.message.contains("2 the same picture in the same place"),
        "{}",
        w.message
    );
}

#[tokio::test]
async fn embedded_and_referenced_pictures_are_poppler_s_crops() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    let d = convert(
        input(report()),
        &mode(Pictures::Embedded, Output::Markdown),
        &facts,
    )
    .await
    .doc;
    let md = d.content.unwrap();
    assert_eq!(
        md.matches("![Image](data:image/png;base64,").count(),
        2,
        "{md}"
    );
    // At the default scale (144 dpi) a 300 × 200 pt picture is 600 × 400 px.
    assert_eq!(
        (d.pictures[0].width_px, d.pictures[0].height_px),
        (Some(600), Some(400))
    );
    assert!(d.pictures[0].data_base64.is_none(), "the markdown holds it");

    let d = convert(
        input(report()),
        &mode(Pictures::Referenced, Output::Markdown),
        &facts,
    )
    .await
    .doc;
    let md = d.content.unwrap();
    let file = d.pictures[0].file.clone().unwrap();
    assert!(md.contains(&format!("![Image]({file})")), "{md}");
    let png = base64::engine::general_purpose::STANDARD
        .decode(d.pictures[0].data_base64.as_ref().unwrap())
        .unwrap();
    assert!(png.starts_with(b"\x89PNG"));
    // The crop is the chart's colour, not the page around it.
    let mut dec = png::Decoder::new(std::io::Cursor::new(&png))
        .read_info()
        .unwrap();
    let mut buf = vec![0; dec.output_buffer_size().unwrap()];
    dec.next_frame(&mut buf).unwrap();
    let mid = (200 * 600 + 300) * 3;
    assert_eq!(&buf[mid..mid + 3], &[10, 60, 200]);

    // A scale of 1 crops at 72 dpi.
    let half = plan_of(ConvertOptions {
        pictures: Pictures::Referenced,
        images_scale: 1.0,
        ..Default::default()
    });
    let d = convert(input(report()), &half, &facts).await.doc;
    assert_eq!(d.pictures[0].width_px, Some(300));
}

#[tokio::test]
async fn json_has_figure_blocks_and_the_bytes_in_the_list() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    let d = convert(
        input(report()),
        &mode(Pictures::Embedded, Output::Json),
        &facts,
    )
    .await
    .doc;
    let s = serde_json::to_value(d.structure.unwrap()).unwrap();
    let blocks = s[0]["blocks"].as_array().unwrap();
    let kinds: Vec<&str> = blocks.iter().map(|b| b["kind"].as_str().unwrap()).collect();
    let fig = kinds.iter().position(|k| *k == "figure").unwrap();
    assert_eq!(kinds[fig + 1], "caption", "{kinds:?}");
    assert_eq!(blocks[fig]["picture"], "p1");
    assert_eq!(blocks[fig]["caption"], "Figure 1: Example chart");
    assert!(blocks[fig].get("text").is_none());
    assert!(d.pictures[0].data_base64.is_some());
}

#[tokio::test]
async fn a_page_whose_crops_pass_the_raster_bound_says_so_and_keeps_its_text() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    // 300 × 200 pt at scale 400: 120 000 × 80 000 px, far past the bound.
    let p = plan_of(ConvertOptions {
        pictures: Pictures::Referenced,
        images_scale: 400.0,
        ..Default::default()
    });
    let d = convert(input(report()), &p, &facts).await.doc;
    assert_eq!(d.status, DocStatus::Converted);
    assert!(d.content.unwrap().contains("Figure 1: Example chart"));
    let w = d
        .warnings
        .iter()
        .find(|w| w.code == WarningCode::PicturesPartial)
        .unwrap();
    assert!(w.message.contains("PAGE_RASTER_MAX_BYTES"), "{}", w.message);
    assert!(d.pictures.iter().all(|p| p.data_base64.is_none()));
}

#[tokio::test]
async fn the_request_budget_bounds_the_pictures_and_says_so() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    let small = Arc::new(OutputBudget::with_limit(1_500));
    let d = super::super::convert(
        input(report()),
        &mode(Pictures::Referenced, Output::Markdown),
        &facts,
        &small,
    )
    .await
    .doc;
    // The text and the list fit; the first crop's PNG, reserved at its
    // base64 size, does not.
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    assert!(d
        .warnings
        .iter()
        .any(|w| w.message.contains("MAX_UNCOMPRESSED_BYTES")));
}

#[tokio::test]
async fn a_docx_s_picture_is_embedded_where_the_document_has_it() {
    let none = ToolFacts::all_missing();
    let mut pngb = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut pngb, 4, 4);
        enc.set_color(png::ColorType::Rgb);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[7u8; 48]).unwrap();
        w.finish().unwrap();
    }
    let doc = r#"<w:document xmlns:w="w" xmlns:wp="wp" xmlns:a="a" xmlns:r="r"><w:body><w:p><w:r><w:t>Before.</w:t></w:r></w:p><w:p><w:r><w:drawing><wp:inline><wp:extent cx="3000000" cy="3000000"/><a:blip r:embed="rId1"/></wp:inline></w:drawing></w:r></w:p><w:p><w:r><w:t>Figure 1: Example</w:t></w:r></w:p></w:body></w:document>"#;
    let docx = crate::extract::test_files::zip(&[
        (
            "[Content_Types].xml",
            br#"<Types><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#,
        ),
        ("word/document.xml", doc.as_bytes()),
        (
            "word/_rels/document.xml.rels",
            br#"<Relationships><Relationship Id="rId1" Target="media/a.png"/></Relationships>"#,
        ),
        ("word/media/a.png", &pngb),
    ]);
    let d = convert(
        input(docx.clone()),
        &mode(Pictures::Embedded, Output::Markdown),
        &none,
    )
    .await
    .doc;
    let md = d.content.unwrap();
    assert!(
        md.starts_with("Before.\n\n![Image](data:image/png;base64,")
            && md.ends_with(")\n\nFigure 1: Example"),
        "{md}"
    );
    assert_eq!(d.pictures[0].caption.as_deref(), Some("Figure 1: Example"));
    // Text writes no picture; placeholder writes docling's line.
    let d = convert(
        input(docx.clone()),
        &mode(Pictures::Placeholder, Output::Text),
        &none,
    )
    .await
    .doc;
    assert_eq!(d.content.as_deref(), Some("Before.\n\nFigure 1: Example"));
    let d = convert(
        input(docx),
        &mode(Pictures::Placeholder, Output::Markdown),
        &none,
    )
    .await
    .doc;
    assert_eq!(
        d.content.as_deref(),
        Some("Before.\n\n<!-- image -->\n\nFigure 1: Example")
    );
}

/// S5: bytes made only for the picture-description model (placeholders, or
/// text) hold their own size of the request's budget, not the base64 the
/// answer never writes, and a picture past it is not told to ask for
/// placeholders; embedded ones hold their base64 and are.
#[test]
fn bytes_made_only_for_descriptions_hold_their_own_size() {
    use super::super::pictured::charge;
    use crate::extract::convert::pictures::{self, Found, Image, Picture};
    let found = || {
        let mut f = Found::default();
        for _ in 0..2 {
            let mut p = Picture::new(1, None, crate::extract::convert::doc::Source::Native);
            p.image = Some(Image {
                bytes: Bytes::from(vec![7u8; 3000]),
                mime: "image/png",
                ext: "png",
                width_px: Some(4),
                height_px: Some(4),
            });
            f.pictures.push(p);
        }
        f
    };
    let settings = |mode| pictures::Settings {
        mode,
        describe: true,
        ..Default::default()
    };
    let only_described = settings(Pictures::Placeholder);
    let budget = OutputBudget::with_limit(4000);
    let mut f = found();
    charge(&mut f, &budget, &only_described);
    assert_eq!(
        f.charged, 3000,
        "the first picture holds its own 3000 bytes"
    );
    assert!(f.pictures[0].image.is_some() && f.pictures[1].image.is_none());
    let said = &f.warnings[0].message;
    assert!(
        said.contains("made only to be sent to the picture-description model")
            && !said.contains("placeholders"),
        "{said}"
    );
    let embedded = settings(Pictures::Embedded);
    let budget = OutputBudget::with_limit(4000);
    let mut f = found();
    charge(&mut f, &budget, &embedded);
    assert_eq!(f.charged, 0, "base64 and its entry pass 4000");
    assert!(f.warnings[0].message.contains("ask for placeholders"));
}
