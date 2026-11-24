use lmgw_api_types::documents::convert::{Pictures, WarningCode};

use super::*;
use crate::extract::office::extract_with_media;
use crate::extract::test_files;

const W: &str = r#"xmlns:w="w" xmlns:wp="wp" xmlns:a="a" xmlns:r="r" xmlns:pic="pic""#;

/// A PNG header of `w` × `h` (enough for the sniffer).
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    b.extend_from_slice(&w.to_be_bytes());
    b.extend_from_slice(&h.to_be_bytes());
    b
}

fn drawing(rel: &str, cx: u64, cy: u64) -> String {
    format!(
        r#"<w:r><w:drawing><wp:inline><wp:extent cx="{cx}" cy="{cy}"/><a:graphic><a:graphicData><pic:pic><pic:blipFill><a:blip r:embed="{rel}"/></pic:blipFill></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>"#
    )
}

/// A docx: a heading, a picture with a caption after it, a tiny icon in a
/// paragraph's text, a picture in a table cell, an EMF and a linked one.
fn docx() -> Vec<u8> {
    let body = format!(
        r#"<w:p><w:r><w:t>Before the chart.</w:t></w:r></w:p><w:p>{big}</w:p><w:p><w:r><w:t>Figure 1: Example chart</w:t></w:r></w:p><w:p><w:r><w:t>Text with an icon</w:t></w:r>{icon}</w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell</w:t></w:r>{cell}</w:p></w:tc></w:tr></w:tbl><w:p>{emf}</w:p><w:p>{linked}</w:p><w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr>"#,
        big = drawing("rId1", 5_000_000, 3_000_000),
        icon = drawing("rId2", 100_000, 100_000),
        cell = drawing("rId1", 4_000_000, 3_000_000),
        emf = drawing("rId3", 4_000_000, 3_000_000),
        linked = drawing("rId4", 4_000_000, 3_000_000),
    );
    let doc = format!(r#"<w:document {W}><w:body>{body}</w:body></w:document>"#);
    let rels = r#"<Relationships><Relationship Id="rId1" Target="media/image1.png"/><Relationship Id="rId2" Target="media/icon.png"/><Relationship Id="rId3" Target="media/old.emf"/><Relationship Id="rId4" Target="https://example.org/x.png" TargetMode="External"/></Relationships>"#;
    let big = png(400, 300);
    let icon = png(8, 8);
    test_files::zip(&[
        ("[Content_Types].xml", b"<Types/>"),
        ("word/document.xml", doc.as_bytes()),
        ("word/_rels/document.xml.rels", rels.as_bytes()),
        ("word/media/image1.png", &big),
        ("word/media/icon.png", &icon),
        ("word/media/old.emf", &[1, 0, 0, 0, 0x6c, 0, 0, 0]),
    ])
}

fn settings(mode: Pictures) -> Settings {
    Settings {
        mode,
        ..Default::default()
    }
}

#[test]
fn a_docx_s_pictures_come_in_document_order_with_caption_and_bytes() {
    let bytes = docx();
    let (text, media) = extract_with_media(OfficeFormat::Docx, &bytes).unwrap();
    assert_eq!(media.media.len(), 5);
    assert!(media.page.is_some());
    let found = collect(
        OfficeFormat::Docx,
        &bytes,
        &text,
        media,
        &settings(Pictures::Referenced),
    );
    // The icon is under picture_min_area of the A4 page.
    assert_eq!(found.pictures.len(), 4);
    let p = &found.pictures[0];
    assert_eq!(p.caption.as_deref(), Some("Figure 1: Example chart"));
    assert_eq!(p.before.as_deref(), Some("Before the chart."));
    let img = p.image.as_ref().unwrap();
    assert_eq!(
        (img.mime, img.width_px, img.height_px),
        ("image/png", Some(400), Some(300))
    );
    // The table's picture comes after the table, with the same media.
    assert!(found.pictures[1].image.is_some());
    // EMF and the linked picture keep placeholders, said once.
    assert!(found.pictures[2].image.is_none() && found.pictures[3].image.is_none());
    let partial = found
        .warnings
        .iter()
        .find(|w| w.code == WarningCode::PicturesPartial)
        .unwrap();
    assert!(partial.message.contains("1 linked"), "{}", partial.message);
    assert!(
        partial.message.contains("1 in a format"),
        "{}",
        partial.message
    );
    let left = found
        .warnings
        .iter()
        .find(|w| w.code == WarningCode::PicturesLeftOut)
        .unwrap();
    assert!(left.message.contains("1 smaller"), "{}", left.message);
    // The markdown: each kept picture's line where it was, the icon's
    // marker gone, no blank lines doubled.
    let md = text.markdown();
    let line = |i: u32, out: &mut String| out.push_str(&format!("[P{i}]"));
    let shown = lines(&md, &found, Some(&line), None);
    assert_eq!(
        shown,
        "Before the chart.\n\n[P0]\n\nFigure 1: Example chart\n\nText with an icon\n\n| Cell |\n| --- |\n\n[P1]\n\n[P2]\n\n[P3]",
    );
    let plain = lines(&md, &found, None, None);
    assert!(
        !plain.contains('\u{FFFC}') && !plain.contains("\n\n\n"),
        "{plain:?}"
    );
}

#[test]
fn placeholders_read_no_bytes_and_text_cannot_forge_a_marker() {
    let body = format!(
        "<w:p><w:r><w:t>\u{FFFC}0\u{FFFC}</w:t></w:r></w:p><w:p>{}</w:p>",
        drawing("rId1", 5_000_000, 3_000_000)
    );
    let doc = format!(r#"<w:document {W}><w:body>{body}</w:body></w:document>"#);
    let rels =
        r#"<Relationships><Relationship Id="rId1" Target="media/image1.png"/></Relationships>"#;
    let bytes = test_files::zip(&[
        ("word/document.xml", doc.as_bytes()),
        ("word/_rels/document.xml.rels", rels.as_bytes()),
        ("word/media/image1.png", &png(4, 4)),
    ]);
    let (text, media) = extract_with_media(OfficeFormat::Docx, &bytes).unwrap();
    let found = collect(
        OfficeFormat::Docx,
        &bytes,
        &text,
        media,
        &settings(Pictures::Placeholder),
    );
    assert_eq!(found.pictures.len(), 1);
    assert!(found.pictures[0].image.is_none());
    let md = text.markdown();
    assert_eq!(md.matches('\u{FFFC}').count(), 2, "{md:?}");
    assert!(md.starts_with("0\n\n"), "{md:?}");
    // Without collecting, the reader writes no marker and keeps the text.
    let plain = crate::extract::office::extract(OfficeFormat::Docx, &bytes).unwrap();
    assert_eq!(plain.markdown(), "\u{FFFC}0\u{FFFC}");
}

#[test]
fn slides_and_odf_frames_are_found_where_they_are() {
    let slide = r#"<p:sld xmlns:a="a" xmlns:p="p" xmlns:r="r"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Title</a:t></a:r></a:p></p:txBody></p:sp><p:pic><p:blipFill><a:blip r:embed="rId2"/></p:blipFill><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="4000000" cy="3000000"/></a:xfrm></p:spPr></p:pic></p:spTree></p:cSld></p:sld>"#;
    let pres = r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst><p:sldId r:id="rId1"/></p:sldIdLst><p:sldSz cx="9144000" cy="6858000"/></p:presentation>"#;
    let bytes = test_files::zip(&[
        ("ppt/presentation.xml", pres.as_bytes()),
        (
            "ppt/_rels/presentation.xml.rels",
            br#"<Relationships><Relationship Id="rId1" Target="slides/slide1.xml"/></Relationships>"#,
        ),
        ("ppt/slides/slide1.xml", slide.as_bytes()),
        (
            "ppt/slides/_rels/slide1.xml.rels",
            br#"<Relationships><Relationship Id="rId2" Target="../media/image1.jpeg"/></Relationships>"#,
        ),
        ("ppt/media/image1.jpeg", &[0xFF, 0xD8, 0xFF, 0xD9]),
    ]);
    let (text, media) = extract_with_media(OfficeFormat::Pptx, &bytes).unwrap();
    assert_eq!(media.page, Some((9_144_000, 6_858_000)));
    let found = collect(
        OfficeFormat::Pptx,
        &bytes,
        &text,
        media,
        &settings(Pictures::Embedded),
    );
    assert_eq!(found.pictures.len(), 1);
    assert_eq!(found.pictures[0].part, 1);
    assert_eq!(found.pictures[0].image.as_ref().unwrap().mime, "image/jpeg");

    let body = r#"<office:text><text:p>Before</text:p><text:p><draw:frame svg:width="10cm" svg:height="5cm"><draw:image xlink:href="Pictures/a.png"/></draw:frame></text:p></office:text>"#;
    let odt = test_files::odf("text", body);
    let (text, media) = extract_with_media(OfficeFormat::Odt, &odt).unwrap();
    assert_eq!(media.media.len(), 1);
    assert_eq!(media.media[0].href.as_deref(), Some("Pictures/a.png"));
    let found = collect(
        OfficeFormat::Odt,
        &odt,
        &text,
        media,
        &settings(Pictures::Referenced),
    );
    assert_eq!(found.pictures.len(), 1);
    // The archive has no such entry: a placeholder, said.
    assert!(found.pictures[0].image.is_none());
    assert!(found
        .warnings
        .iter()
        .any(|w| w.message.contains("does not hold")));
}

#[test]
fn json_markers_become_figure_blocks() {
    let bytes = docx();
    let (text, media) = extract_with_media(OfficeFormat::Docx, &bytes).unwrap();
    let found = collect(
        OfficeFormat::Docx,
        &bytes,
        &text,
        media,
        &settings(Pictures::Placeholder),
    );
    let mut doc = Doc {
        parts: vec![crate::extract::convert::doc::Part {
            kind: crate::extract::convert::doc::PartKind::Document,
            number: 1,
            name: None,
            size: None,
            source: Some(Source::Native),
            blocks: crate::extract::convert::doc::from_md::blocks(&text.parts[0].body),
        }],
    };
    figures(&mut doc, &found);
    let figs: Vec<u32> = doc.parts[0]
        .blocks
        .iter()
        .filter_map(|b| b.figure.as_ref().map(|f| f.index))
        .collect();
    assert_eq!(figs, [0, 1, 2, 3]);
    assert!(doc.parts[0]
        .blocks
        .iter()
        .all(|b| !b.text.contains('\u{FFFC}')));
}

#[test]
fn a_workbook_with_media_says_its_pictures_are_not_exported_yet() {
    let book = test_files::xlsx(&[("Budget", &[&["Item"]])]);
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(book)).unwrap();
    for i in 0..z.len() {
        let mut f = z.by_index(i).unwrap();
        let mut b = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut b).unwrap();
        entries.push((f.name().to_string(), b));
    }
    entries.push(("xl/media/image1.png".into(), png(2, 2)));
    let refs: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    let book = test_files::zip(&refs);
    let (text, media) = extract_with_media(OfficeFormat::Xlsx, &book).unwrap();
    let found = collect(
        OfficeFormat::Xlsx,
        &book,
        &text,
        media,
        &Settings::default(),
    );
    assert_eq!(found.warnings[0].code, WarningCode::NotYet);
}
