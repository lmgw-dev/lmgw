//! The in-process reads a slot is shrunk to after its runs
//! (`charge::after_runs`) stay within `charge::READ_PER_OUTPUT_BYTE` per
//! byte of their input, on the shapes with the most structure per byte
//! poppler writes: one one-letter word a line, each line a block of its own,
//! coordinates as short as poppler's `%f` makes them; rows of side-by-side
//! cells; the same line on every page (header candidates); a font run per
//! line; pages without pictures and one-pixel pictures.

use std::fmt::Write;

use super::charge::READ_PER_OUTPUT_BYTE;
use super::peak::peak_of;
use crate::extract::convert::pdf_layout;
use crate::extract::convert::pictures::pdf::images;

const PAGES: usize = 200;
const LINES: usize = 200;

/// A `pdftotext -bbox-layout` page of `LINES` blocks; `cells` blocks side
/// by side a row; `same` the same text on every page.
fn bbox_xml(cells: usize, same: bool) -> String {
    let mut s = String::from("<html><body><doc>\n");
    for _ in 0..PAGES {
        s.push_str("<page width=\"612.000000\" height=\"792.000000\">\n<flow>\n");
        for i in 0..LINES {
            let y = 10.0 + i as f64 * 3.5;
            for c in 0..cells {
                let x = 10.0 + c as f64 * 70.0;
                let r = format!(
                    "xMin=\"{x:.6}\" yMin=\"{y:.6}\" xMax=\"{:.6}\" yMax=\"{:.6}\"",
                    x + 5.0,
                    y + 3.0
                );
                let word = match same {
                    true => format!("h{i}"),
                    false => char::from(b'a' + (i % 26) as u8).to_string(),
                };
                let _ = write!(
                    s,
                    "<block {r}>\n<line {r}>\n<word {r}>{word}</word>\n</line>\n</block>\n"
                );
            }
        }
        s.push_str("</flow>\n</page>\n");
    }
    s.push_str("</doc></body></html>\n");
    s
}

/// `pdftohtml -xml` with one run per line of [`bbox_xml`]'s one-cell pages.
fn fonts_xml() -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<pdf2xml>\n");
    for p in 1..=PAGES {
        let _ = writeln!(
            s,
            "<page number=\"{p}\" position=\"absolute\" top=\"0\" left=\"0\" height=\"792\" \
             width=\"612\">\n<fontspec id=\"0\" size=\"3\" family=\"Example\" color=\"#000000\"/>"
        );
        for i in 0..LINES {
            let y = 10.0 + i as f64 * 3.5;
            let _ = writeln!(
                s,
                "<text top=\"{y:.6}\" left=\"1.000000\" width=\"1.000000\" height=\"3.000000\" \
                 font=\"0\"><b>{}</b></text>",
                char::from(b'a' + (i % 26) as u8)
            );
        }
        s.push_str("</page>\n");
    }
    s.push_str("</pdf2xml>\n");
    s
}

fn within_bound(what: &str, input: usize, peak: u64) {
    let bound = input as u64 * READ_PER_OUTPUT_BYTE;
    assert!(
        peak <= bound,
        "{what}: the read held {peak} B for {input} B of input, more than \
         READ_PER_OUTPUT_BYTE ({READ_PER_OUTPUT_BYTE}) per byte"
    );
}

#[test]
fn the_layout_pass_stays_within_its_read_charge() {
    let fonts = fonts_xml();
    for (what, bbox, fonts) in [
        ("a word a line", bbox_xml(1, false), None),
        (
            "a word a line, a font run each",
            bbox_xml(1, false),
            Some(&fonts),
        ),
        ("rows of eight cells", bbox_xml(8, false), None),
        ("the same lines on every page", bbox_xml(1, true), None),
    ] {
        let fonts = fonts.map(|f| f.as_bytes());
        let input = bbox.len() + fonts.map_or(0, <[u8]>::len);
        let (layout, peak) = peak_of(|| pdf_layout::layout(bbox.as_bytes(), fonts, 1));
        let layout = layout.unwrap();
        assert_eq!(layout.doc.parts.len(), PAGES, "{what}");
        within_bound(what, input, peak);
    }
}

#[test]
fn reading_a_picture_run_stays_within_its_read_charge() {
    let mut empty = String::from("<pdf2xml>\n");
    let mut tiny = String::from("<pdf2xml>\n");
    for p in 1..=20_000 {
        let _ = writeln!(
            empty,
            "<page number=\"{p}\" position=\"absolute\" top=\"0\" left=\"0\" height=\"9\" \
             width=\"9\">\n</page>"
        );
    }
    for p in 1..=PAGES {
        let _ = writeln!(
            tiny,
            "<page number=\"{p}\" position=\"absolute\" top=\"0\" left=\"0\" height=\"9\" \
             width=\"9\">"
        );
        for i in 0..LINES {
            let _ = writeln!(
                tiny,
                "<image top=\"{i}\" left=\"0\" width=\"1\" height=\"1\" \
                 src=\"data:image/png;base64,iVBORw0KGgo=\"/>"
            );
        }
        tiny.push_str("</page>\n");
    }
    empty.push_str("</pdf2xml>\n");
    tiny.push_str("</pdf2xml>\n");
    for (what, xml) in [
        ("pages without pictures", empty),
        ("one-pixel pictures", tiny),
    ] {
        let (got, peak) = peak_of(|| images(xml.as_bytes()));
        assert!(got.error.is_none() && !got.pages.is_empty(), "{what}");
        within_bound(what, xml.len(), peak);
    }
}
