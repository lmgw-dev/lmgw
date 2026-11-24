//! A PDF's notes: the repeated lines named for the pages picked, and what
//! the font run's gaps say.

use super::*;

/// Four A4 pages, each with a header, a footer with its page number and
/// body text; pages 2 and 3 have a "Draft" line at the top as well.
fn report() -> Vec<u8> {
    let footers: Vec<String> = (1..=4).map(|n| format!("Page {n} of 4")).collect();
    let pages: Vec<Vec<(f32, f32, f32, &str)>> = (0..4)
        .map(|i| {
            let mut p = vec![
                (57.0, 30.0, 10.0, "Example Corp Report"),
                (
                    57.0,
                    120.0,
                    10.0,
                    "The example widget team reviewed the numbers.",
                ),
                (
                    57.0,
                    140.0,
                    10.0,
                    "Throughput rose while the error rate fell.",
                ),
                (57.0, 800.0, 10.0, footers[i].as_str()),
            ];
            if i == 1 || i == 2 {
                p.push((300.0, 30.0, 10.0, "Draft"));
            }
            p
        })
        .collect();
    test_files::pdf_placed(595.0, 842.0, &pages)
}

/// `header_removed` names what left the pages picked, with their count; a
/// line removed only from pages not picked is not named.
#[tokio::test]
async fn header_removed_names_what_left_the_picked_pages() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    let plan = plan_of(ConvertOptions {
        pages: Some("1,4".into()),
        ..Default::default()
    });
    let d = convert(input(report()), &plan, &facts).await.doc;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    let content = d.content.unwrap();
    assert!(
        !content.contains("Example Corp") && !content.contains("Page 1"),
        "{content}"
    );
    assert!(content.contains("The example widget team"), "{content}");
    let w = d
        .warnings
        .iter()
        .find(|w| w.code == WarningCode::HeaderRemoved)
        .expect("header_removed");
    assert!(w.message.starts_with("2 line(s)"), "{}", w.message);
    assert!(
        w.message.contains("\"Example Corp Report\" (top, 2 pages)")
            && w.message.contains(
                "\"Page 1 of 4\" (bottom, 2 pages, its page number changing with the page)"
            )
            && !w.message.contains("Draft"),
        "{}",
        w.message
    );
}

fn bbox(pages: u32) -> String {
    let mut x = String::from("<html><body><doc>");
    for p in 1..=pages {
        x.push_str(&format!(
            r#"<page width="300" height="200"><flow><block><line xMin="20" yMin="90" xMax="120" yMax="101"><word xMin="20" yMin="90" xMax="120" yMax="101">page{p}</word></line></block></flow></page>"#
        ));
    }
    x + "</doc></body></html>"
}

fn html(pages: &[u32], tail: &str) -> Vec<u8> {
    let mut x =
        String::from(r##"<pdf2xml><fontspec id="0" size="10" family="A+Serif" color="#000000"/>"##);
    for p in pages {
        x.push_str(&format!(
            r#"<page number="{p}"><text top="90" left="20" width="100" height="11" font="0">page{p}</text></page>"#
        ));
    }
    (x + tail).into_bytes()
}

/// A failed font run, one that covers fewer pages than the layout run, and
/// one that stops being XML: each is a `part_failed` note, never silence.
#[test]
fn the_font_run_s_gaps_are_named() {
    let plan = plan_of(Default::default());
    let notes = |fonts| {
        pdf::assemble(bbox(3).as_bytes(), fonts, 1, None, &plan)
            .unwrap()
            .1
    };
    let n = notes(pdf::Fonts::Failed("exit status 1".into()));
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].code, WarningCode::PartFailed);
    assert!(n[0].message.contains("exit status 1"), "{}", n[0].message);

    let n = notes(pdf::Fonts::Read(html(&[1], "</pdf2xml>")));
    assert_eq!(n.len(), 1, "{n:?}");
    assert_eq!(n[0].code, WarningCode::PartFailed);
    assert_eq!(n[0].parts, [2, 3]);
    assert!(n[0].message.contains("pages 2-3"), "{}", n[0].message);

    let n = notes(pdf::Fonts::Read(html(&[1, 2], "<page><<")));
    assert_eq!(n.len(), 1, "{n:?}");
    assert!(
        n[0].message.contains("stops being readable XML") && n[0].parts == [3],
        "{n:?}"
    );

    // All pages covered: nothing to say.
    assert!(notes(pdf::Fonts::Read(html(&[1, 2, 3], "</pdf2xml>"))).is_empty());
    // Not one run for text that is there.
    let n = notes(pdf::Fonts::Read(html(&[], "</pdf2xml>")));
    assert!(n[0].message.contains("found no text"), "{n:?}");
}

/// A layout output past its bound names what was large and what to do, and
/// the pool counts the outputs a PDF holds on its slot.
#[test]
fn a_too_large_layout_output_names_it_and_pages() {
    let m = pdf::layout_too_large();
    assert!(
        m.contains("layout output")
            && m.contains("pdftotext -bbox-layout")
            && m.contains("MAX_UNCOMPRESSED_BYTES (1.0 GiB)")
            && m.ends_with("with pages"),
        "{m}"
    );
    assert_eq!(super::super::pool::charge::PDF_FILE, 5 << 29);
}
