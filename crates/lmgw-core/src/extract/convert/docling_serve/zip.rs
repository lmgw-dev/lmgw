//! `converted_docs.zip` (document-conversion design §7.4.3, D26): the
//! answer for several files or the `zip` target.
//!
//! **Layout.** docling-jobkit's zip export on `main` (read 2026-11-18,
//! `convert/results.py` `_export_documents_as_files`) and docling-serve
//! release 1.38.2 (written flat, measured by WP0a) put every document
//! **flat** into the archive's root: `<stem>.md`, `<stem>.txt`, … with
//! `artifacts/` for pictures. The numbered directories (`000000/`) are
//! docling-jobkit's export to remote storage targets, not the zip. D26 says
//! the adapter follows `main`, so [`LAYOUT`] is flat; the numbered form is
//! kept beside it for a change in one place. Where two files share a stem,
//! docling's flat zip loses the earlier one; lmgw keeps both, the later as
//! `<stem>_<index>` (then `_<n>` until the name is free; names are compared
//! without regard to case, as the archive may be extracted on a system that
//! does not tell them apart). A stem longer than [`MAX_STEM_BYTES`] is cut
//! to it, and the answer's `x-lmgw-documents` says how many names were
//! cut or renamed. A stem that starts with `-` or is a Windows device name
//! (`CON`, `NUL`, `COM1`, …) gets `_` in front (`guard`, security round B).
//!
//! A file that failed or was skipped is absent, as in docling.

use std::collections::HashSet;
use std::io::Write;

use lmgw_api_types::documents::convert::{ConvertedDocument, DocStatus};

use super::Spec;

mod guard;

/// How the zip lays out its documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// `<stem>.md` at the root (docling-jobkit `main`).
    Flat,
    /// `<index:06>/<stem>.md` (docling-jobkit's remote-target export).
    Numbered,
}

/// The layout in use.
pub const LAYOUT: Layout = Layout::Flat;

/// The most bytes of a file's stem an entry name keeps: 255 is what common
/// file systems take for a whole name, which is the stem with its extension
/// and a collision suffix (`_<index>_<n>`, `.md`).
pub const MAX_STEM_BYTES: usize = 200;

/// `stem` cut to [`MAX_STEM_BYTES`] at a character boundary.
fn cut(stem: &str) -> &str {
    if stem.len() <= MAX_STEM_BYTES {
        return stem;
    }
    let mut end = MAX_STEM_BYTES;
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    &stem[..end]
}

/// A name's stem, as a zip entry may hold it: no directories, no `..`.
fn stem(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let stem = std::path::Path::new(base)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let clean: String = stem
        .chars()
        .map(|c| if c.is_control() || c == ':' { '_' } else { c })
        .collect();
    if clean.is_empty() || clean == ".." || clean == "." {
        "file".to_string()
    } else {
        clean
    }
}

/// The text outputs of `d` as `spec` asks for them: (extension, text).
pub fn outputs(spec: &Spec, d: &ConvertedDocument) -> Vec<(&'static str, String)> {
    let Some(content) = &d.content else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if spec.md {
        out.push(("md", content.clone()));
        if spec.text {
            // The conversion wrote it (`Plan::also_text`) from the same
            // document, the way `output: text` does.
            if let Some(text) = &d.text {
                out.push(("txt", text.clone()));
            }
        }
    } else if spec.text {
        out.push(("txt", content.clone()));
    }
    out
}

/// The archive's entries, and how many names were not the file's own.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Entries {
    /// Name and text, in request order.
    pub files: Vec<(String, String)>,
    /// Referenced pictures (`artifacts/image_<n:06>_<sha256>.<ext>`, the
    /// names the markdown links, beside it), each name once: two files'
    /// pictures share a name only when their bytes are the same.
    pub artifacts: Vec<(String, Vec<u8>)>,
    /// Names given a suffix because another file has the stem.
    pub renamed: usize,
    /// Stems cut to [`MAX_STEM_BYTES`].
    pub shortened: usize,
}

/// The entry names and texts of `docs`, in request order.
pub fn entries(spec: &Spec, docs: &[ConvertedDocument], layout: Layout) -> Entries {
    // Lower-cased, so names that differ only by case count as one.
    let mut used: HashSet<String> = HashSet::new();
    let mut out = Entries::default();
    let mut pictures: HashSet<String> = HashSet::new();
    for d in docs
        .iter()
        .filter(|d| matches!(d.status, DocStatus::Converted | DocStatus::Partial))
    {
        // An option's or a Windows device's shape gets a prefix (`guard`),
        // before the cut, so the stem stays within MAX_STEM_BYTES with it.
        let mut whole = stem(&d.name);
        if guard::needs_prefix(&whole) {
            whole = guard::guarded(whole);
            out.renamed += 1;
        }
        let mut s = cut(&whole).to_string();
        if s.len() < whole.len() {
            out.shortened += 1;
        }
        if layout == Layout::Flat && used.contains(&s.to_lowercase()) {
            let base = s.clone();
            s = format!("{base}_{}", d.index);
            let mut n = 2;
            while used.contains(&s.to_lowercase()) {
                s = format!("{base}_{}_{n}", d.index);
                n += 1;
            }
            out.renamed += 1;
        }
        if layout == Layout::Flat {
            used.insert(s.to_lowercase());
        }
        for (ext, text) in outputs(spec, d) {
            let name = match layout {
                Layout::Flat => format!("{s}.{ext}"),
                Layout::Numbered => format!("{:06}/{s}.{ext}", d.index),
            };
            out.files.push((name, text));
        }
        if !spec.md {
            continue;
        }
        for p in &d.pictures {
            let (Some(file), Some(data)) = (&p.file, &p.data_base64) else {
                continue;
            };
            // Names lmgw made; anything else is not written.
            if !crate::extract::convert::pictures::is_artifact(file) {
                continue;
            }
            let name = match layout {
                Layout::Flat => file.clone(),
                Layout::Numbered => format!("{:06}/{file}", d.index),
            };
            if !pictures.insert(name.clone()) {
                continue;
            }
            use base64::Engine as _;
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                out.artifacts.push((name, bytes));
            }
        }
    }
    out
}

/// The archive's bytes.
pub fn write(entries: &[(String, String)]) -> std::io::Result<Vec<u8>> {
    write_with(entries, &[])
}

/// The archive's bytes, the texts first and the pictures after them
/// (stored: PNG and JPEG are compressed already).
pub fn write_with(
    entries: &[(String, String)],
    artifacts: &[(String, Vec<u8>)],
) -> std::io::Result<Vec<u8>> {
    let mut w = ::zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = ::zip::write::SimpleFileOptions::default()
        .compression_method(::zip::CompressionMethod::Deflated);
    for (name, text) in entries {
        w.start_file(name.as_str(), opts)
            .map_err(std::io::Error::other)?;
        w.write_all(text.as_bytes())?;
    }
    let stored = ::zip::write::SimpleFileOptions::default()
        .compression_method(::zip::CompressionMethod::Stored)
        .large_file(true);
    for (name, bytes) in artifacts {
        w.start_file(name.as_str(), stored)
            .map_err(std::io::Error::other)?;
        w.write_all(bytes)?;
    }
    Ok(w.finish().map_err(std::io::Error::other)?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::convert::docling_serve::Target;

    fn doc(index: u32, name: &str, status: DocStatus) -> ConvertedDocument {
        ConvertedDocument {
            index,
            name: name.into(),
            status,
            format: Some("text".into()),
            parts: 1,
            content: (status != DocStatus::Failed).then(|| "# Example\n\nText.".into()),
            structure: None,
            pictures: Vec::new(),
            text: (status != DocStatus::Failed).then(|| "Example\n\nText.".into()),
            warnings: Vec::new(),
            stats: Default::default(),
            error: None,
        }
    }

    #[test]
    fn the_txt_entry_is_the_text_the_conversion_wrote_beside_the_markdown() {
        let d = doc(0, "example.pdf", DocStatus::Converted);
        let both = outputs(&spec(true, true), &d);
        assert_eq!(
            both,
            [
                ("md", "# Example\n\nText.".to_string()),
                ("txt", "Example\n\nText.".to_string())
            ]
        );
        let only_text = outputs(&spec(false, true), &d);
        assert_eq!(only_text, [("txt", "# Example\n\nText.".to_string())]);
        // No text written beside the markdown: no txt entry made up from it.
        let mut bare = d;
        bare.text = None;
        assert_eq!(outputs(&spec(true, true), &bare).len(), 1);
    }

    fn spec(md: bool, text: bool) -> Spec {
        Spec {
            md,
            text,
            missing_outputs: Vec::new(),
            target: Target::Zip,
            abort_on_error: false,
            notes: None,
        }
    }

    #[test]
    fn the_flat_layout_names_each_output_by_stem_and_keeps_repeats() {
        let docs = [
            doc(0, "dir/example.pdf", DocStatus::Converted),
            doc(1, "example.docx", DocStatus::Partial),
            doc(2, "../bad.txt", DocStatus::Failed),
        ];
        let e = entries(&spec(true, true), &docs, Layout::Flat).files;
        let names: Vec<&str> = e.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["example.md", "example.txt", "example_1.md", "example_1.txt"]
        );
        assert_eq!(e[1].1, "Example\n\nText.");
        let e = entries(&spec(true, false), &docs, Layout::Numbered).files;
        let names: Vec<&str> = e.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["000000/example.md", "000001/example.md"]);
    }

    fn names(docs: &[ConvertedDocument]) -> (Vec<String>, Entries) {
        let e = entries(&spec(true, false), docs, Layout::Flat);
        (e.files.iter().map(|(n, _)| n.clone()).collect(), e)
    }

    /// Security round B, hardening 2.
    #[test]
    fn option_shaped_and_device_names_get_a_prefix() {
        let docs = [
            doc(0, "-rf.txt", DocStatus::Converted),
            doc(1, "CON.txt", DocStatus::Converted),
            doc(2, "nul.tar.gz", DocStatus::Converted),
            doc(3, "plain.txt", DocStatus::Converted),
        ];
        let (n, e) = names(&docs);
        assert_eq!(n, ["_-rf.md", "_CON.md", "_nul.tar.md", "plain.md"]);
        assert_eq!(e.renamed, 3);
        // The review of round B, N4: the prefix comes before the cut, and
        // Windows' console and clock devices count.
        let long = format!("-{}.txt", "a".repeat(299));
        let docs = [
            doc(0, &long, DocStatus::Converted),
            doc(1, "conin$.txt", DocStatus::Converted),
            doc(2, "CONOUT$", DocStatus::Converted),
            doc(3, "clock$.txt", DocStatus::Converted),
        ];
        let (n, e) = names(&docs);
        let first = n[0].strip_suffix(".md").unwrap();
        assert_eq!(first.len(), MAX_STEM_BYTES, "{first}");
        assert!(first.starts_with("_-a"), "{first}");
        assert_eq!(&n[1..], ["_conin$.md", "_CONOUT$.md", "_clock$.md"]);
        assert_eq!((e.renamed, e.shortened), (4, 1));
    }

    #[test]
    fn a_name_another_file_took_by_its_suffix_gets_another_and_the_archive_writes() {
        let docs = [
            doc(0, "report.pdf", DocStatus::Converted),
            doc(1, "report_2.pdf", DocStatus::Converted),
            doc(2, "report.pdf", DocStatus::Converted),
            doc(3, "report.docx", DocStatus::Converted),
        ];
        let (n, e) = names(&docs);
        assert_eq!(
            n,
            ["report.md", "report_2.md", "report_2_2.md", "report_3.md"]
        );
        assert_eq!(e.renamed, 2);
        // The archive refuses duplicates: it must write whatever the names.
        write(&e.files).unwrap();
    }

    #[test]
    fn names_that_differ_only_by_case_stay_two_entries() {
        let docs = [
            doc(0, "Report.pdf", DocStatus::Converted),
            doc(1, "report.pdf", DocStatus::Converted),
            doc(2, "REPORT.pdf", DocStatus::Converted),
        ];
        let (n, e) = names(&docs);
        assert_eq!(n, ["Report.md", "report_1.md", "REPORT_2.md"]);
        assert_eq!(e.renamed, 2);
        let lower: HashSet<String> = n.iter().map(|n| n.to_lowercase()).collect();
        assert_eq!(lower.len(), 3);
    }

    #[test]
    fn a_stem_past_the_constant_is_cut_visibly_and_the_archive_writes() {
        let long = format!("{}.pdf", "é".repeat(40_000));
        let docs = [
            doc(0, &long, DocStatus::Converted),
            doc(1, &long, DocStatus::Converted),
        ];
        let (n, e) = names(&docs);
        assert_eq!(e.shortened, 2);
        assert_eq!(e.renamed, 1);
        assert!(
            n.iter().all(|n| n.len() <= 255),
            "{:?}",
            n.iter().map(String::len).collect::<Vec<_>>()
        );
        assert_ne!(n[0], n[1]);
        write(&e.files).unwrap();
    }

    #[test]
    fn referenced_pictures_are_artifacts_beside_the_markdown_once_each() {
        use base64::Engine as _;
        let file = format!("artifacts/image_000000_{}.png", "cd".repeat(32));
        let pic = |name: &str| lmgw_api_types::documents::pictures::DocPicture {
            id: "p1".into(),
            part: 1,
            bbox: None,
            caption: None,
            mime: Some("image/png".into()),
            width_px: None,
            height_px: None,
            bytes: Some(3),
            file: Some(name.into()),
            data_base64: Some(base64::engine::general_purpose::STANDARD.encode(b"png")),
            description: None,
        };
        let mut a = doc(0, "a.pdf", DocStatus::Converted);
        a.content = Some(format!("![Image]({file})"));
        a.pictures = vec![pic(&file), pic("../evil.png")];
        let mut b = doc(1, "b.pdf", DocStatus::Converted);
        b.pictures = vec![pic(&file)];
        let e = entries(&spec(true, false), &[a, b], Layout::Flat);
        let names: Vec<&str> = e.artifacts.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, [file.as_str()]);
        assert_eq!(e.artifacts[0].1, b"png");
        let bytes = write_with(&e.files, &e.artifacts).unwrap();
        let mut z = ::zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        assert!(z.by_name(&file).is_ok());
        assert!(z.by_name("a.md").is_ok());
        let numbered = entries(
            &spec(true, false),
            &[{
                let mut d = doc(3, "c.pdf", DocStatus::Converted);
                d.pictures = vec![pic(&file)];
                d
            }],
            Layout::Numbered,
        );
        assert_eq!(numbered.artifacts[0].0, format!("000003/{file}"));
    }

    #[test]
    fn the_archive_reads_back() {
        let bytes = write(&[("example.md".into(), "hello".into())]).unwrap();
        let mut z = ::zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(z.len(), 1);
        let mut s = String::new();
        std::io::Read::read_to_string(&mut z.by_index(0).unwrap(), &mut s).unwrap();
        assert_eq!(s, "hello");
    }
}
