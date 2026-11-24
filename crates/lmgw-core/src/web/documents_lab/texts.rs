//! The texts of a conversion's answer as output artifacts (WP12): each
//! document's text found where it is in the route's answer, so the stored
//! answer names its kept file as `lab-artifact:<id>` in its place and every
//! other byte stays as the route sent it (the Image lab's pictures, read the
//! same way through borrowed `RawValue`s).
//!
//! - The native answer: each `documents[i]`'s `content` (markdown or text),
//!   `structure` (the parts, kept as the JSON they are) and `text`.
//! - docling's one-file answer: `document`'s `md_content`, `text_content`,
//!   `html_content`, `doctags_content`, `doclang_content` and `json_content`.
//! - docling's zip: the archive as it came, and each entry as its own file
//!   ([`zip_entries`]).

use axum::body::Bytes;
use lmgw_api_types::systemone::Ordered;
use serde_json::value::RawValue;

/// One text of the answer: its JSON token's byte range in the answer, what
/// its file is called and its bytes (a string's text, an array's JSON).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Found {
    pub start: usize,
    pub end: usize,
    pub name: String,
    pub mime: &'static str,
    pub bytes: Bytes,
}

/// `name` without its extension, as a file stem.
fn stem_of(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = base.rsplit_once('.').map_or(base, |(s, _)| s);
    if stem.trim().is_empty() {
        return "document".into();
    }
    super::super::model_stem(stem)
}

fn string_of(raw: &RawValue) -> Option<String> {
    serde_json::from_str::<String>(raw.get()).ok()
}

/// The token `raw` of `text` as a [`Found`] of `bytes`.
fn found(text: &str, raw: &RawValue, name: String, mime: &'static str, bytes: Vec<u8>) -> Found {
    let start = raw.get().as_ptr() as usize - text.as_ptr() as usize;
    Found {
        start,
        end: start + raw.get().len(),
        name,
        mime,
        bytes: Bytes::from(bytes),
    }
}

/// Every document text of the native answer `text`, in order. `markdown`:
/// the request asked for markdown, so `content` is a `.md` file.
pub(super) fn native(text: &str, markdown: bool) -> Vec<Found> {
    let Ok(body) = serde_json::from_str::<Ordered<&RawValue>>(text) else {
        return Vec::new();
    };
    let Some(docs) = body
        .get("documents")
        .and_then(|d| serde_json::from_str::<Vec<&RawValue>>(d.get()).ok())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for doc in docs {
        let Ok(fields) = serde_json::from_str::<Ordered<&RawValue>>(doc.get()) else {
            continue;
        };
        let stem = fields
            .get("name")
            .and_then(|n| string_of(n))
            .map_or_else(|| "document".into(), |n| stem_of(&n));
        for (key, raw) in fields.iter() {
            match key {
                "content" => {
                    if let Some(s) = string_of(raw) {
                        let (ext, mime) = if markdown {
                            ("md", "text/markdown")
                        } else {
                            ("txt", "text/plain")
                        };
                        out.push(found(
                            text,
                            raw,
                            format!("{stem}.{ext}"),
                            mime,
                            s.into_bytes(),
                        ));
                    }
                }
                "text" => {
                    if let Some(s) = string_of(raw) {
                        out.push(found(
                            text,
                            raw,
                            format!("{stem}.txt"),
                            "text/plain",
                            s.into_bytes(),
                        ));
                    }
                }
                "structure" if raw.get().starts_with('[') => {
                    out.push(found(
                        text,
                        raw,
                        format!("{stem}.structure.json"),
                        "application/json",
                        raw.get().as_bytes().to_vec(),
                    ));
                }
                _ => {}
            }
        }
    }
    out
}

/// Every text of docling's one-file answer `text`, in order.
pub(super) fn docling(text: &str) -> Vec<Found> {
    let Ok(body) = serde_json::from_str::<Ordered<&RawValue>>(text) else {
        return Vec::new();
    };
    let Some(fields) = body
        .get("document")
        .and_then(|d| serde_json::from_str::<Ordered<&RawValue>>(d.get()).ok())
    else {
        return Vec::new();
    };
    let stem = fields
        .get("filename")
        .and_then(|n| string_of(n))
        .map_or_else(|| "document".into(), |n| stem_of(&n));
    let mut out = Vec::new();
    for (key, raw) in fields.iter() {
        let (ext, mime) = match key {
            "md_content" => ("md", "text/markdown"),
            "text_content" => ("txt", "text/plain"),
            "html_content" => ("html", "text/plain"),
            "doctags_content" => ("doctags.txt", "text/plain"),
            "doclang_content" => ("doclang.txt", "text/plain"),
            "json_content" => {
                if raw.get().starts_with('{') {
                    out.push(found(
                        text,
                        raw,
                        format!("{stem}.json"),
                        "application/json",
                        raw.get().as_bytes().to_vec(),
                    ));
                }
                continue;
            }
            _ => continue,
        };
        if let Some(s) = string_of(raw) {
            out.push(found(
                text,
                raw,
                format!("{stem}.{ext}"),
                mime,
                s.into_bytes(),
            ));
        }
    }
    out
}

/// `text` with each found token replaced by the JSON string of its
/// replacement, in order: `refs[i]` for `found[i]`.
pub(super) fn splice(text: &str, found: &[Found], refs: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (f, with) in found.iter().zip(refs) {
        out.push_str(&text[at..f.start]);
        out.push_str(&serde_json::Value::String(with.clone()).to_string());
        at = f.end;
    }
    out.push_str(&text[at..]);
    out
}

/// The entries of docling's zip answer, by name, in the archive's order.
/// The archive is lmgw's own answer, already bounded by the request's
/// output budget (`MAX_UNCOMPRESSED_BYTES`); each entry is read to its end
/// with no size taken from its header.
pub(super) fn zip_entries(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    use std::io::Read;
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(z.len());
    for i in 0..z.len() {
        let mut f = z.by_index(i).map_err(|e| e.to_string())?;
        if f.is_dir() {
            continue;
        }
        let name = f.name().to_string();
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        out.push((name, buf));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_native_texts_are_found_in_place_and_spliced_out() {
        let text = r##"{"documents":[{"index":0,"name":"a/example-a.pdf","status":"converted","content":"# Title\n","structure":null},{"index":1,"name":"b.docx","content":null,"structure":[{"kind":"document","blocks":[]}]}],"summary":{}}"##;
        let f = native(text, true);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].name, "example-a.md");
        assert_eq!(&f[0].bytes[..], b"# Title\n");
        assert_eq!(f[1].name, "b.structure.json");
        assert_eq!(&f[1].bytes[..], br#"[{"kind":"document","blocks":[]}]"#);
        let out = splice(
            text,
            &f,
            &["lab-artifact:5".into(), "lab-artifact:6".into()],
        );
        assert!(
            out.contains(r#""content":"lab-artifact:5","structure":null"#),
            "{out}"
        );
        assert!(out.contains(r#""structure":"lab-artifact:6"}"#), "{out}");
        assert!(out.ends_with(r#""summary":{}}"#));
    }

    #[test]
    fn the_docling_texts_are_its_contents_that_are_set() {
        let text = r#"{"document":{"filename":"example.pdf","md_content":"x","text_content":"y","json_content":null,"html_content":null},"status":"success"}"#;
        let f = docling(text);
        let names: Vec<&str> = f.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["example.md", "example.txt"]);
        assert!(native("not json", true).is_empty());
        assert!(docling(r#"{"detail":"x"}"#).is_empty());
    }

    #[test]
    fn a_zip_answer_reads_entry_by_entry() {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default();
            z.start_file("a.md", o).unwrap();
            z.write_all(b"# a").unwrap();
            z.start_file("b.md", o).unwrap();
            z.write_all(b"# b").unwrap();
            z.finish().unwrap();
        }
        let e = zip_entries(buf.get_ref()).unwrap();
        assert_eq!(e[0], ("a.md".into(), b"# a".to_vec()));
        assert_eq!(e.len(), 2);
        assert!(zip_entries(b"not a zip").is_err());
    }
}
