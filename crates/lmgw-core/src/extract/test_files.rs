//! Tiny office files for the extraction tests, built in memory: no binary
//! fixtures in the repository.

use std::io::Write;

use zip::write::SimpleFileOptions;

/// A ZIP archive of `entries`, deflated, in order.
pub fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        w.start_file(*name, opts).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// An archive whose central directory *claims* `entry` is `declared` bytes
/// once unpacked (a header can lie; the extractor must not need the entry
/// to be really that big to refuse it).
pub fn zip_with_declared_size(entry: &str, declared: u64) -> Vec<u8> {
    zip_lying_about_size(entry, b"<x/>", declared)
}

/// An archive of one `entry` holding `data`, whose headers claim it is
/// `declared` bytes — bigger (refused early) or smaller (the lie the
/// decompressor must not be trusted past).
pub fn zip_lying_about_size(entry: &str, data: &[u8], declared: u64) -> Vec<u8> {
    let mut bytes = zip(&[(entry, data)]);
    let declared = u32::try_from(declared).expect("a size that fits a zip32 header");
    for (sig, offset) in [(*b"PK\x01\x02", 24usize), (*b"PK\x03\x04", 22usize)] {
        let at = bytes
            .windows(4)
            .position(|w| w == sig)
            .expect("header present");
        bytes[at + offset..at + offset + 4].copy_from_slice(&declared.to_le_bytes());
    }
    bytes
}

/// A one-sheet xlsx (`Sheet1`) with the given `<sheetData>` inner XML and,
/// when given, the raw `xl/sharedStrings.xml`.
pub fn xlsx_raw(sheet_data: &str, shared_strings: Option<&str>) -> Vec<u8> {
    let ns = r#"xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main""#;
    let types = r#"<Types><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#;
    let workbook = format!(
        r#"<workbook {ns} xmlns:r="r"><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#
    );
    let rels = r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#;
    let sheet = format!(r#"<worksheet {ns}><sheetData>{sheet_data}</sheetData></worksheet>"#);
    let mut entries: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", types.as_bytes()),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
    ];
    if let Some(sst) = shared_strings {
        entries.push(("xl/sharedStrings.xml", sst.as_bytes()));
    }
    zip(&entries)
}

/// A one-sheet xlsx with its own `xl/styles.xml` and, when `date1904`, the
/// workbook flag of that date system.
pub fn xlsx_styled(sheet_data: &str, styles: &str, date1904: bool) -> Vec<u8> {
    let ns = r#"xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main""#;
    let pr = if date1904 {
        r#"<workbookPr date1904="1"/>"#
    } else {
        ""
    };
    let workbook = format!(
        r#"<workbook {ns} xmlns:r="r">{pr}<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#
    );
    let rels = r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#;
    let sheet = format!(r#"<worksheet {ns}><sheetData>{sheet_data}</sheetData></worksheet>"#);
    let styles = format!(r#"<styleSheet {ns}>{styles}</styleSheet>"#);
    zip(&[
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
        ("xl/styles.xml", styles.as_bytes()),
    ])
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

const W_NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006""#;

/// A docx whose body is the given raw `<w:p>`/`<w:tbl>` XML, and optionally
/// its own `word/styles.xml`.
pub fn docx_xml(body: &str, styles: Option<&str>) -> Vec<u8> {
    let types = r#"<Types><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
    let doc = format!(r#"<w:document {W_NS}><w:body>{body}</w:body></w:document>"#);
    let styles = styles.map(|s| s.replacen("<w:styles>", &format!("<w:styles {W_NS}>"), 1));
    let mut entries: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", types.as_bytes()),
        ("word/document.xml", doc.as_bytes()),
    ];
    if let Some(s) = &styles {
        entries.push(("word/styles.xml", s.as_bytes()));
    }
    zip(&entries)
}

/// A docx of plain paragraphs.
pub fn docx(paras: &[&str]) -> Vec<u8> {
    let body: String = paras
        .iter()
        .map(|p| format!("<w:p><w:r><w:t>{}</w:t></w:r></w:p>", esc(p)))
        .collect();
    docx_xml(&body, None)
}

/// The XML of one slide with a paragraph per entry.
pub fn slide_xml(paras: &[&str]) -> String {
    let ps: String = paras
        .iter()
        .map(|p| format!("<a:p><a:r><a:t>{}</a:t></a:r></a:p>", esc(p)))
        .collect();
    format!(
        r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree><p:sp><p:txBody>{ps}</p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
    )
}

/// A pptx of slides in file order (`slide1.xml`, …), presented in that order.
pub fn pptx(slides: &[&[&str]]) -> Vec<u8> {
    let names: Vec<String> = (1..=slides.len())
        .map(|i| format!("slide{i}.xml"))
        .collect();
    let named: Vec<(&str, &[&str])> = names
        .iter()
        .zip(slides)
        .map(|(n, s)| (n.as_str(), *s))
        .collect();
    let order: Vec<&str> = names.iter().map(String::as_str).collect();
    pptx_ordered(&named, &order)
}

/// A pptx with named slide parts, presented in `order`.
pub fn pptx_ordered(slides: &[(&str, &[&str])], order: &[&str]) -> Vec<u8> {
    let types = r#"<Types><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/></Types>"#;
    let ids: String = order
        .iter()
        .enumerate()
        .map(|(i, _)| format!(r#"<p:sldId id="{}" r:id="rId{}"/>"#, 256 + i, i + 1))
        .collect();
    let pres = format!(
        r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst>{ids}</p:sldIdLst></p:presentation>"#
    );
    let rels: String = order
        .iter()
        .enumerate()
        .map(|(i, n)| {
            format!(
                r#"<Relationship Id="rId{}" Type="slide" Target="slides/{n}"/>"#,
                i + 1
            )
        })
        .collect();
    let rels = format!("<Relationships>{rels}</Relationships>");
    let slide_files: Vec<(String, String)> = slides
        .iter()
        .map(|(n, p)| (format!("ppt/slides/{n}"), slide_xml(p)))
        .collect();
    let mut entries: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", types.as_bytes()),
        ("ppt/presentation.xml", pres.as_bytes()),
        ("ppt/_rels/presentation.xml.rels", rels.as_bytes()),
    ];
    for (n, x) in &slide_files {
        entries.push((n, x.as_bytes()));
    }
    zip(&entries)
}

/// An OpenDocument file of `kind` (`text`, `spreadsheet`, `presentation`)
/// whose `office:body` holds `body`.
pub fn odf(kind: &str, body: &str) -> Vec<u8> {
    let mimetype = format!("application/vnd.oasis.opendocument.{kind}");
    let content = format!(
        r#"<?xml version="1.0"?><office:document-content xmlns:office="o" xmlns:text="t" xmlns:table="tb" xmlns:draw="d" xmlns:presentation="p"><office:body>{body}</office:body></office:document-content>"#
    );
    // The mimetype entry comes first and is stored, as the format wants.
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    w.start_file(
        "mimetype",
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
    )
    .unwrap();
    w.write_all(mimetype.as_bytes()).unwrap();
    w.start_file("META-INF/manifest.xml", SimpleFileOptions::default())
        .unwrap();
    w.write_all(br#"<manifest:manifest xmlns:manifest="m"/>"#)
        .unwrap();
    w.start_file(
        "content.xml",
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
    )
    .unwrap();
    w.write_all(content.as_bytes()).unwrap();
    w.finish().unwrap().into_inner()
}

/// An xlsx of sheets of rows of cells. A cell `"n:12.5"` is the number 12.5,
/// an empty one is left out, anything else is a string.
pub fn xlsx(sheets: &[(&str, &[&[&str]])]) -> Vec<u8> {
    let types = r#"<Types><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#;
    let root_rels = r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
    let sheet_tags: String = sheets
        .iter()
        .enumerate()
        .map(|(i, (n, _))| {
            format!(
                r#"<sheet name="{}" sheetId="{}" r:id="rId{}"/>"#,
                esc(n),
                i + 1,
                i + 1
            )
        })
        .collect();
    let workbook = format!(
        r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>{sheet_tags}</sheets></workbook>"#
    );
    let rels: String = (1..=sheets.len())
        .map(|i| {
            format!(
                r#"<Relationship Id="rId{i}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{i}.xml"/>"#
            )
        })
        .collect();
    let wb_rels = format!(r#"<Relationships>{rels}</Relationships>"#);
    let sheet_files: Vec<(String, String)> = sheets
        .iter()
        .enumerate()
        .map(|(i, (_, rows))| {
            let mut data = String::new();
            for (r, row) in rows.iter().enumerate() {
                data.push_str(&format!(r#"<row r="{}">"#, r + 1));
                for (c, cell) in row.iter().enumerate() {
                    let at = format!("{}{}", (b'A' + c as u8) as char, r + 1);
                    if cell.is_empty() {
                        continue;
                    }
                    match cell.strip_prefix("n:") {
                        Some(n) => data.push_str(&format!(r#"<c r="{at}"><v>{n}</v></c>"#)),
                        None => data.push_str(&format!(
                            r#"<c r="{at}" t="inlineStr"><is><t>{}</t></is></c>"#,
                            esc(cell)
                        )),
                    }
                }
                data.push_str("</row>");
            }
            (
                format!("xl/worksheets/sheet{}.xml", i + 1),
                format!(
                    r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{data}</sheetData></worksheet>"#
                ),
            )
        })
        .collect();
    let mut entries: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", types.as_bytes()),
        ("_rels/.rels", root_rels.as_bytes()),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", wb_rels.as_bytes()),
    ];
    for (n, x) in &sheet_files {
        entries.push((n, x.as_bytes()));
    }
    zip(&entries)
}

/// A valid PDF with one page per entry: `Some(text)` draws it, `None` leaves
/// the page blank (what a scan looks like to `pdftotext`). Text is Latin-1
/// through the standard Helvetica; parentheses and backslashes are escaped.
pub fn pdf(pages: &[Option<&str>]) -> Vec<u8> {
    let n = pages.len();
    // Objects: 1 catalog, 2 pages, 3 font, then per page a page and a stream.
    let mut objs: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {n} >>",
            (0..n)
                .map(|i| format!("{} 0 R", 4 + 2 * i))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ];
    for (i, text) in pages.iter().enumerate() {
        objs.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents {} 0 R \
             /Resources << /Font << /F1 3 0 R >> >> >>",
            5 + 2 * i
        ));
        let content = match text {
            Some(t) => format!(
                "BT /F1 14 Tf 20 100 Td ({}) Tj ET",
                t.replace('\\', "\\\\")
                    .replace('(', "\\(")
                    .replace(')', "\\)")
            ),
            None => String::new(),
        };
        objs.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ));
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).into_bytes());
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).into_bytes());
    for o in offsets {
        out.extend(format!("{o:010} 00000 n \n").into_bytes());
    }
    out.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .into_bytes(),
    );
    out
}
