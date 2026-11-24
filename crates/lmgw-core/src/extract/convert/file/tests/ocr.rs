//! Text recognition through the conversion, with this machine's tesseract
//! and poppler: a scanned page in a PDF, `ocr` off/auto/force, a picture, a
//! multi-page TIFF, a language that is not installed, pages on their side
//! and upside down. The scans are made here: a text PDF rendered by
//! `pdftoppm` and wrapped as an image-only PDF, or encoded as a PNG or TIFF.
//!
//! Each test needs both tools and is skipped without them; `ci/check.sh`
//! says before the suite runs which document tools it found, so a gate that
//! ran none of these says so in its output. lmgw's own reader is built by
//! cargo and never missing.

use lmgw_api_types::documents::convert::Ocr;

use super::built;
use super::*;
use crate::extract::convert::ocr::image;

/// What this machine has, when it has both poppler and tesseract.
pub(super) async fn with_ocr() -> Option<std::sync::Arc<ToolFacts>> {
    let facts = with_poppler().await?;
    if matches!(facts.fact(ToolId::Tesseract), Some(Fact::Found(_))) {
        Some(facts)
    } else {
        eprintln!("skipped: tesseract is not on PATH");
        None
    }
}

/// `facts` as a machine without tesseract's orientation data has them
/// (whatever this one has): `osd` named absent, so no page is detected.
pub(super) fn without_osd(facts: &ToolFacts) -> ToolFacts {
    use lmgw_api_types::documents::ExtraKind;
    let mut f = facts.clone();
    if let Some(Fact::Found(t)) = f.tools.get_mut(&ToolId::Tesseract).map(|t| &mut t.fact) {
        for e in t
            .extras
            .iter_mut()
            .filter(|e| e.kind == ExtraKind::Languages)
        {
            if !e.absent.iter().any(|a| a == "osd") {
                e.absent.push("osd".into());
            }
        }
    }
    f
}

/// Grey pixels as a PNG.
pub(super) fn png(w: u32, h: u32, px: Vec<u8>) -> Vec<u8> {
    let img = ::image::GrayImage::from_raw(w, h, px).unwrap();
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, ::image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// Grey pixels turned `degrees` clockwise.
fn turned(w: u32, h: u32, px: &[u8], degrees: u32) -> (u32, u32, Vec<u8>) {
    let image::PageImage::Pgm {
        data,
        offset,
        width,
        height,
    } = image::pgm(w, h, px).rotated(degrees).unwrap()
    else {
        unreachable!()
    };
    (width, height, data[offset..].to_vec())
}

pub(super) const PAGE_LINES: &[&str] = &[
    "Example lines of text that are long enough",
    "for tesseract to find words on this page",
    "even when the page is not the right way up.",
    "A fourth line of several words follows them.",
];

/// An A4 page with `lines` of text, rendered at 150 DPI as grey pixels.
pub(super) async fn scanned(lines: &[&str]) -> (u32, u32, Vec<u8>) {
    let placed: Vec<(f32, f32, f32, &str)> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| (60.0, 80.0 + 30.0 * i as f32, 16.0, *l))
        .collect();
    let pdf = test_files::pdf_placed(595.0, 842.0, &[placed]);
    let args = [
        "-gray",
        "-r",
        "150",
        "-singlefile",
        "-f",
        "1",
        "-l",
        "1",
        "-",
    ];
    let pgm = crate::extract::convert::run::piped(
        std::path::Path::new("pdftoppm"),
        &args,
        Bytes::from(pdf),
        crate::extract::convert::run::Bounds {
            timeout: std::time::Duration::from_secs(60),
            output: 64 << 20,
            memory: 2 << 30,
        },
    )
    .await
    .unwrap();
    match image::parse_pgm(pgm).unwrap() {
        image::PageImage::Pgm {
            data,
            width,
            height,
            offset,
        } => (width, height, data[offset..].to_vec()),
        image::PageImage::File(_) => unreachable!(),
    }
}

/// A PDF of image-only pages (`Some`: grey pixels) and text pages (`None`:
/// one line of text), A4.
pub(super) fn image_pdf(pages: &[Option<&(u32, u32, Vec<u8>)>]) -> Vec<u8> {
    let mut objs: Vec<Vec<u8>> = vec![b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), Vec::new()];
    objs.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());
    let mut kids = Vec::new();
    for p in pages {
        let page_id = objs.len() + 1;
        kids.push(format!("{page_id} 0 R"));
        let (res, content) = match p {
            Some((w, h, px)) => {
                let img_id = page_id + 2;
                (
                    format!("/XObject << /Im0 {img_id} 0 R >>"),
                    (b"q 595 0 0 842 0 0 cm /Im0 Do Q".to_vec(), Some((w, h, px))),
                )
            }
            None => (
                "/Font << /F1 3 0 R >>".to_string(),
                (
                    b"BT /F1 16 Tf 60 760 Td (A page with its own text layer) Tj ET".to_vec(),
                    None,
                ),
            ),
        };
        objs.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents {} 0 R \
                 /Resources << {res} >> >>",
                page_id + 1
            )
            .into_bytes(),
        );
        let (stream, img) = content;
        let mut s = format!("<< /Length {} >>\nstream\n", stream.len()).into_bytes();
        s.extend_from_slice(&stream);
        s.extend_from_slice(b"\nendstream");
        objs.push(s);
        if let Some((w, h, px)) = img {
            let mut s = format!(
                "<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace \
                 /DeviceGray /BitsPerComponent 8 /Length {} >>\nstream\n",
                px.len()
            )
            .into_bytes();
            s.extend_from_slice(px);
            s.extend_from_slice(b"\nendstream");
            objs.push(s);
        }
    }
    objs[1] = format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids.join(" "),
        pages.len()
    )
    .into_bytes();
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}

pub(super) fn with_ocr_plan(ocr: Ocr, langs: Option<&[&str]>) -> Plan {
    plan_of(ConvertOptions {
        ocr,
        ocr_languages: langs.map(|l| l.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    })
}

#[tokio::test]
async fn a_scanned_page_is_recognised_and_counted_as_ocr() {
    let Some(facts) = with_ocr().await else {
        return;
    };
    let page = scanned(&["Quarterly example report", "The widgets sold well."]).await;
    let pdf = image_pdf(&[None, Some(&page)]);
    let d = convert(input(pdf.clone()), &with_ocr_plan(Ocr::Auto, None), &facts).await;
    let doc = d.doc;
    assert_eq!(doc.status, DocStatus::Converted, "{doc:?}");
    assert_eq!(
        (
            doc.stats.parts_by_source.text_layer,
            doc.stats.parts_by_source.ocr
        ),
        (1, 1)
    );
    let md = doc.content.unwrap();
    assert!(md.contains("own text layer"), "{md}");
    assert!(md.contains("widgets sold well"), "{md}");
    assert!(d.also.contains(&ToolId::Tesseract));

    // json: the page's blocks say they were recognised.
    let mut p = with_ocr_plan(Ocr::Auto, None);
    p.output = Output::Json;
    let doc = convert(input(pdf.clone()), &p, &facts).await.doc;
    let parts = doc.structure.unwrap();
    let json = serde_json::to_value(&parts[1]).unwrap();
    assert_eq!(json["source"], "ocr", "{json}");
    assert!(json["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|b| b["source"] == "ocr"));

    // off: the scanned page is named, not read.
    let doc = convert(input(pdf), &with_ocr_plan(Ocr::Off, None), &facts)
        .await
        .doc;
    assert_eq!(doc.status, DocStatus::Partial);
    assert!(
        doc.warnings[0].message.contains("ocr is off"),
        "{:?}",
        doc.warnings
    );
}

#[tokio::test]
async fn force_reads_every_page_and_a_scan_alone_no_longer_fails() {
    let Some(facts) = with_ocr().await else {
        return;
    };
    let text = test_files::pdf_placed(
        595.0,
        842.0,
        &[vec![(60.0, 80.0, 16.0, "Forced example line")]],
    );
    let doc = convert(input(text), &with_ocr_plan(Ocr::Force, None), &facts)
        .await
        .doc;
    assert_eq!(doc.status, DocStatus::Converted, "{doc:?}");
    assert_eq!(
        (
            doc.stats.parts_by_source.text_layer,
            doc.stats.parts_by_source.ocr
        ),
        (0, 1)
    );
    assert!(doc.content.unwrap().contains("Forced example line"));

    let page = scanned(&["Only a picture of text"]).await;
    let doc = convert(
        input(image_pdf(&[Some(&page)])),
        &with_ocr_plan(Ocr::Auto, None),
        &facts,
    )
    .await
    .doc;
    assert_eq!(doc.status, DocStatus::Converted, "{doc:?}");
    assert!(doc.content.unwrap().contains("picture of text"));
}

#[tokio::test]
async fn a_picture_is_read_and_a_missing_language_is_named() {
    let Some(facts) = with_ocr().await else {
        return;
    };
    let (w, h, px) = scanned(&["Example picture words"]).await;
    let png = png(w, h, px);
    let d = convert(
        input(png.clone()),
        &with_ocr_plan(Ocr::Auto, Some(&["eng", "xyz", "qqq"])),
        &facts,
    )
    .await;
    let doc = d.doc;
    assert_eq!(doc.status, DocStatus::Converted, "{doc:?}");
    assert_eq!(doc.format.as_deref(), Some("image"));
    assert_eq!(doc.stats.parts_by_source.ocr, 1);
    assert!(doc.content.unwrap().contains("picture words"));
    // lmgw's reader read the picture's size before tesseract saw it.
    assert_eq!(
        (d.tool, d.also),
        (Some(ToolId::Tesseract), vec![ToolId::Reader])
    );
    let missing: Vec<_> = doc
        .warnings
        .iter()
        .filter(|w| w.code == WarningCode::OcrLanguageMissing)
        .collect();
    assert_eq!(missing.len(), 1, "one warning for both codes: {missing:?}");
    assert!(
        missing[0]
            .message
            .contains("tesseract-langpack-xyz tesseract-langpack-qqq"),
        "{}",
        missing[0].message
    );

    let doc = convert(
        input(png.clone()),
        &with_ocr_plan(Ocr::Auto, Some(&["xyz"])),
        &facts,
    )
    .await
    .doc;
    let e = doc.error.unwrap();
    assert_eq!(e.code, ErrorCode::ToolMissing);
    assert!(
        e.message.contains("tesseract-langpack-xyz"),
        "{}",
        e.message
    );

    let doc = convert(input(png), &with_ocr_plan(Ocr::Off, None), &facts)
        .await
        .doc;
    assert!(doc.error.unwrap().message.contains("ocr is off"));
}

/// A picture of text turned upside down, read without the orientation
/// data: its lines still run across, so nothing says which way it is
/// turned; tesseract is unsure of what it read, and the warning says so.
#[tokio::test]
async fn an_upside_down_page_without_orientation_data_is_flagged() {
    let Some(facts) = with_ocr().await else {
        return;
    };
    let facts = without_osd(&facts);
    let (w, h, px) = scanned(PAGE_LINES).await;
    let (w, h, px) = turned(w, h, &px, 180);
    let doc = convert(
        input(png(w, h, px)),
        &with_ocr_plan(Ocr::Auto, None),
        &facts,
    )
    .await
    .doc;
    let w = doc
        .warnings
        .iter()
        .find(|w| w.code == WarningCode::OcrLowConfidence);
    assert!(
        w.is_some_and(|w| w.message.contains("upside down") && w.message.contains("tesseract-osd")),
        "{doc:?}"
    );
}

/// Finding 11: a page scanned on its side, read without the orientation
/// data. tesseract reads it as vertical lines with full confidence, so the
/// lines' shape is what tells: the page is turned and read again, whichever
/// side it lies on, and the turn is said.
#[tokio::test]
async fn a_page_on_its_side_without_orientation_data_is_turned_and_read() {
    let Some(facts) = with_ocr().await else {
        return;
    };
    let facts = without_osd(&facts);
    let (w, h, px) = scanned(PAGE_LINES).await;
    // Lying on its right side it needs 270° more, on its left side 90°.
    for (lies, needs) in [(90, 270), (270, 90)] {
        let (tw, th, tpx) = turned(w, h, &px, lies);
        let doc = convert(
            input(png(tw, th, tpx)),
            &with_ocr_plan(Ocr::Auto, None),
            &facts,
        )
        .await
        .doc;
        assert_eq!(doc.status, DocStatus::Converted, "{lies}: {doc:?}");
        let md = doc.content.unwrap();
        // In reading order, as the upright page reads.
        let at = |words: &str| {
            md.find(words)
                .unwrap_or_else(|| panic!("{lies}: {words} in {md}"))
        };
        assert!(at("long enough") < at("find words"), "{lies}: {md}");
        assert!(at("find words") < at("fourth line"), "{lies}: {md}");
        let w = doc
            .warnings
            .iter()
            .find(|w| w.code == WarningCode::OcrPageTurned)
            .unwrap_or_else(|| panic!("{lies}: the turn is said: {:?}", doc.warnings));
        assert!(
            w.message.contains(&format!("turned {needs}° clockwise"))
                && w.message.contains("lines read as vertical")
                && w.message.contains("tesseract-osd"),
            "{lies}: {}",
            w.message
        );
        assert_eq!(w.parts, vec![1]);
        assert!(
            doc.warnings
                .iter()
                .all(|w| w.code != WarningCode::OcrLowConfidence),
            "{lies}: {:?}",
            doc.warnings
        );
    }
    // The upright page costs no second read and gets no note.
    let doc = convert(
        input(png(w, h, px)),
        &with_ocr_plan(Ocr::Auto, None),
        &facts,
    )
    .await
    .doc;
    assert!(doc.warnings.is_empty(), "{:?}", doc.warnings);
}

/// Finding 12: the path the orientation data takes, run for real: detected
/// (`--psm 0`), the picture decoded by lmgw's reader, turned, read again.
///
/// The detection's answer is a stand-in: this machine may not have
/// `osd.traineddata`, and the suite neither ships nor fetches a third
/// party's data file. The stand-in is a script in tesseract's place that
/// answers `--psm 0` as tesseract does for a page lying on its left side
/// and hands every other run to the real tesseract, so everything after the
/// answer is the real path.
#[cfg(unix)]
#[tokio::test]
async fn a_page_the_orientation_data_calls_turned_is_turned_before_it_is_read() {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    let Some(facts) = with_ocr().await else {
        return;
    };
    let Some(Fact::Found(real)) = facts.fact(ToolId::Tesseract).cloned() else {
        unreachable!()
    };
    let dir = tempfile::tempdir().unwrap();
    let stand_in = dir.path().join("tesseract");
    let mut f = std::fs::File::create(&stand_in).unwrap();
    writeln!(
        f,
        "#!/bin/sh\ncase \"$*\" in\n*'--psm 0'*) cat >/dev/null\n\
         printf 'Page number: 0\\nOrientation in degrees: 270\\nRotate: 90\\n\
         Orientation confidence: 2.81\\nScript: Latin\\nScript confidence: 90.00\\n';;\n\
         *) exec '{}' \"$@\";;\nesac",
        real.located.path.display()
    )
    .unwrap();
    drop(f);
    std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The orientation data counted as installed, tesseract the stand-in.
    let mut facts = (*facts).clone();
    if let Some(Fact::Found(t)) = facts.tools.get_mut(&ToolId::Tesseract).map(|t| &mut t.fact) {
        t.located.path = stand_in;
        for e in &mut t.extras {
            e.absent.retain(|a| a != "osd");
        }
    }
    assert!(crate::extract::convert::ocr::langs::osd_installed(&facts));
    let (w, h, px) = scanned(PAGE_LINES).await;
    let (w, h, px) = turned(w, h, &px, 270);
    let doc = convert(
        input(png(w, h, px)),
        &with_ocr_plan(Ocr::Auto, None),
        &facts,
    )
    .await
    .doc;
    assert_eq!(doc.status, DocStatus::Converted, "{doc:?}");
    let md = doc.content.unwrap();
    assert!(
        md.find("long enough") < md.find("fourth line") && md.contains("find words"),
        "{md}"
    );
    let w = doc
        .warnings
        .iter()
        .find(|w| w.code == WarningCode::OcrPageTurned)
        .expect("the turn is said");
    assert!(
        w.message.contains("turned 90° clockwise") && w.message.contains("orientation data said"),
        "{}",
        w.message
    );
}

/// A TIFF of two pages: listed once by lmgw's reader, each page decoded by
/// it from the spooled file and read on its own.
#[tokio::test]
async fn a_tiffs_pages_are_read_one_by_one() {
    let Some(facts) = with_ocr().await else {
        return;
    };
    let facts = without_osd(&facts);
    let (w, h, one) = scanned(&["The first page of the fax"]).await;
    let (_, _, two) = scanned(&["Another page follows it"]).await;
    let file = built::grey_tiff(w, h, &[&one, &two]);
    let spool = tempfile::tempdir().unwrap();
    let plan = with_ocr_plan(Ocr::Auto, None).spooling_under(spool.path());
    let doc = convert(input(file), &plan, &facts).await.doc;
    assert_eq!(doc.status, DocStatus::Converted, "{doc:?}");
    assert_eq!((doc.parts, doc.stats.parts_by_source.ocr), (2, 2));
    let md = doc.content.unwrap();
    assert!(
        md.find("first page of the fax") < md.find("Another page follows"),
        "{md}"
    );
    // Nothing is left of the spool file.
    let left = std::fs::read_dir(spool.path().join("convert-spool")).unwrap();
    assert_eq!(left.count(), 0);
}

/// A scan of several pages is rendered page by page from the spooled PDF
/// (poppler opens it as a file), not from a pipe per page.
#[tokio::test]
async fn a_scanned_pdfs_pages_are_rendered_from_the_spooled_file() {
    let Some(facts) = with_ocr().await else {
        return;
    };
    let facts = without_osd(&facts);
    let one = scanned(&["Page one of the scan"]).await;
    let two = scanned(&["Page two of the scan"]).await;
    let pdf = image_pdf(&[Some(&one), Some(&two)]);
    let spool = tempfile::tempdir().unwrap();
    let plan = with_ocr_plan(Ocr::Auto, None).spooling_under(spool.path());
    let doc = convert(input(pdf), &plan, &facts).await.doc;
    assert_eq!(doc.status, DocStatus::Converted, "{doc:?}");
    assert_eq!(doc.stats.parts_by_source.ocr, 2);
    let md = doc.content.unwrap();
    assert!(
        md.contains("Page one of the scan") && md.contains("Page two of the scan"),
        "{md}"
    );
    assert!(
        spool.path().join("convert-spool").is_dir(),
        "it was spooled"
    );
}
