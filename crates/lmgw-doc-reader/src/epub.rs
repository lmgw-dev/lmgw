//! EPUB (document-conversion design §4.2, §5.2), lmgw's own reader:
//! `META-INF/container.xml` names the package document, whose spine lists
//! the chapters in reading order; each chapter (XHTML) goes through the
//! HTML path ([`super::html`]).
//!
//! An EPUB is a ZIP. Every byte unpacked from it (the container, the
//! package, the encryption list, each chapter, a chapter the spine names
//! twice each time) is counted against the request's budget as it is read,
//! and so is everything kept from the package document, so a zip bomb is
//! refused by name. The XML files are streamed: of the package document
//! only its `item` and `itemref` elements are kept, whatever else it holds.
//! Paths are resolved inside the archive only (`..` cannot leave it); a
//! spine item outside it (an address) is never fetched. A ZIP inside the
//! book is not opened. Pictures become placeholders (`html.rs`).
//!
//! What is not read is said once per kind of gap, with how many chapters
//! it is about and the first of them ([`Notes`]), so a spine of a million
//! unknown entries is one note. A chapter `META-INF/encryption.xml` lists is
//! encrypted (a book with DRM): it is named, not read as noise.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Cursor, Read};

use lmgw_doc_reader::wire::{Book, Failure, MAX_HTML_DEPTH};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use super::html;

type Zip<'a> = zip::ZipArchive<Cursor<&'a [u8]>>;

/// What one kept manifest item or spine entry is counted at on top of its
/// text: its map entry and string headers.
const KEPT_COST_BYTES: u64 = 96;

/// How much of a name a note quotes: 120 characters, then `…`.
const NOTE_EXAMPLE_CHARS: usize = 120;

/// The bytes the book may still unpack and keep.
struct Budget {
    left: Cell<u64>,
    over: Cell<bool>,
}

impl Budget {
    fn spend(&self, n: u64) -> bool {
        match self.left.get().checked_sub(n) {
            Some(left) => {
                self.left.set(left);
                true
            }
            None => {
                self.left.set(0);
                self.over.set(true);
                false
            }
        }
    }
}

/// A ZIP entry read against the budget.
struct Limited<'b, R> {
    inner: R,
    budget: &'b Budget,
}

impl<R: Read> Read for Limited<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if !self.budget.spend(n as u64) {
            return Err(std::io::Error::other("past the budget"));
        }
        Ok(n)
    }
}

/// What was not read, by kind.
#[derive(Default)]
struct Notes {
    kinds: Vec<(&'static str, u64, String)>,
}

impl Notes {
    fn add(&mut self, kind: &'static str, example: &str) {
        match self.kinds.iter_mut().find(|(k, _, _)| *k == kind) {
            Some((_, n, _)) => *n += 1,
            None => {
                let mut e: String = example.chars().take(NOTE_EXAMPLE_CHARS).collect();
                if e.len() < example.len() {
                    e.push('…');
                }
                self.kinds.push((kind, 1, e));
            }
        }
    }

    fn words(self) -> Vec<String> {
        self.kinds
            .into_iter()
            .map(|(kind, n, first)| format!("{kind}: {n} (the first: {first})"))
            .collect()
    }
}

const UNLISTED: &str = "spine entries naming an item the package does not list";
const NOT_A_PAGE: &str = "spine items that are not pages, not read";
const OUTSIDE: &str = "spine items outside the book, not fetched";
const MISSING: &str = "chapters missing from the book";
const ENCRYPTED: &str = "chapters that are encrypted (META-INF/encryption.xml lists them: a book \
                         with DRM), not read";
const UNPARSED: &str = "chapters that could not be parsed";

fn local(e: &BytesStart<'_>) -> String {
    String::from_utf8_lossy(e.local_name().as_ref()).into_owned()
}

/// The value of attribute `name` (by its local name), unescaped.
fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        (a.key.local_name().as_ref() == name.as_bytes()).then(|| {
            let raw = String::from_utf8_lossy(a.value.as_ref()).into_owned();
            match quick_xml::escape::unescape(&raw) {
                Ok(v) => v.into_owned(),
                Err(_) => raw,
            }
        })
    })
}

/// `%20` and the like decoded.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `href` relative to the directory `base`, inside the archive; `None` for
/// an address (a scheme) or a path that would leave the archive.
fn resolve(base: &str, href: &str) -> Option<String> {
    let href = href.split(['#', '?']).next().unwrap_or_default();
    if href.is_empty() || href.contains("://") || href.starts_with("data:") {
        return None;
    }
    let href = percent_decode(href);
    let mut parts: Vec<&str> = if href.starts_with('/') {
        Vec::new()
    } else {
        base.split('/').filter(|p| !p.is_empty()).collect()
    };
    for p in href.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

fn over(what: &str) -> Failure {
    Failure::OverBudget {
        what: what.to_string(),
    }
}

/// Every start or empty element of the entry `name`, streamed to `each`;
/// `Ok(false)` when the book has no such entry. Malformed XML ends the read
/// with what came before it.
fn elements(
    zip: &mut Zip<'_>,
    name: &str,
    budget: &Budget,
    mut each: impl FnMut(&BytesStart<'_>) -> bool,
) -> Result<bool, Failure> {
    let Ok(entry) = zip.by_name(name) else {
        return Ok(false);
    };
    let mut r = Reader::from_reader(BufReader::new(Limited {
        inner: entry,
        budget,
    }));
    r.config_mut().check_end_names = false;
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if !each(&e) {
                    break;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            Ok(_) => {}
        }
        buf.clear();
    }
    if budget.over.get() {
        return Err(over(name));
    }
    Ok(true)
}

/// The whole entry `name`, or `None` when the book has none.
fn entry(zip: &mut Zip<'_>, name: &str, budget: &Budget) -> Result<Option<Vec<u8>>, Failure> {
    let Ok(e) = zip.by_name(name) else {
        return Ok(None);
    };
    let mut out = Vec::new();
    let read = Limited { inner: e, budget }.read_to_end(&mut out);
    if budget.over.get() {
        return Err(over(name));
    }
    read.map_err(|e| Failure::Unreadable(format!("{name} could not be unpacked: {e}")))?;
    Ok(Some(out))
}

/// The book in `bytes` (module doc), unpacking and keeping at most `budget`
/// bytes.
pub fn read(bytes: &[u8], budget: u64) -> Result<Book, Failure> {
    let unreadable = |why: String| Failure::Unreadable(why);
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| unreadable(format!("the file is not a readable EPUB archive: {e}")))?;
    let budget = Budget {
        left: Cell::new(budget),
        over: Cell::new(false),
    };
    let mut opf_path: Option<String> = None;
    let found = elements(&mut zip, "META-INF/container.xml", &budget, |e| {
        if local(e) != "rootfile" {
            return true;
        }
        let media = attr(e, "media-type");
        if media
            .as_deref()
            .is_some_and(|m| m != "application/oebps-package+xml")
        {
            return true;
        }
        opf_path = attr(e, "full-path").and_then(|p| resolve("", &p));
        opf_path.is_none()
    })?;
    if !found {
        return Err(unreadable(
            "the EPUB has no META-INF/container.xml".to_string(),
        ));
    }
    let opf_path = opf_path
        .ok_or_else(|| unreadable("the EPUB's container names no package document".to_string()))?;
    // What the book says is encrypted, by its path in the archive.
    let mut encrypted: HashSet<String> = HashSet::new();
    elements(&mut zip, "META-INF/encryption.xml", &budget, |e| {
        if local(e) == "CipherReference" {
            if let Some(path) = attr(e, "URI").and_then(|u| resolve("", &u)) {
                if !budget.spend(path.len() as u64 + KEPT_COST_BYTES) {
                    return false;
                }
                encrypted.insert(path);
            }
        }
        true
    })?;
    if budget.over.get() {
        return Err(over("META-INF/encryption.xml"));
    }
    let base = opf_path.rsplit_once('/').map_or("", |(d, _)| d).to_string();
    let mut manifest: HashMap<String, (String, String)> = HashMap::new();
    let mut spine: Vec<String> = Vec::new();
    let found = elements(&mut zip, &opf_path, &budget, |e| {
        match local(e).as_str() {
            "item" => {
                let id = attr(e, "id").unwrap_or_default();
                let href = attr(e, "href").unwrap_or_default();
                let media = attr(e, "media-type").unwrap_or_default();
                let kept = (id.len() + href.len() + media.len()) as u64 + KEPT_COST_BYTES;
                if !budget.spend(kept) {
                    return false;
                }
                manifest.insert(id, (href, media));
            }
            "itemref" => {
                if let Some(id) = attr(e, "idref") {
                    if !budget.spend(id.len() as u64 + KEPT_COST_BYTES) {
                        return false;
                    }
                    spine.push(id);
                }
            }
            _ => {}
        }
        true
    })?;
    if budget.over.get() {
        return Err(over(&opf_path));
    }
    if !found {
        return Err(unreadable(format!(
            "the EPUB's package document {opf_path} is missing"
        )));
    }
    if spine.is_empty() {
        return Err(unreadable(
            "the EPUB's package lists no chapters in its spine".to_string(),
        ));
    }
    let mut book = Book::default();
    let mut notes = Notes::default();
    let mut too_deep: Option<(u64, String, usize)> = None;
    for id in &spine {
        let Some((href, media)) = manifest.get(id) else {
            notes.add(UNLISTED, id);
            continue;
        };
        if !matches!(media.as_str(), "application/xhtml+xml" | "text/html" | "") {
            notes.add(NOT_A_PAGE, &format!("{href} is {media}"));
            continue;
        }
        let Some(path) = resolve(&base, href) else {
            notes.add(OUTSIDE, href);
            continue;
        };
        if encrypted.contains(&path) {
            notes.add(ENCRYPTED, &path);
            continue;
        }
        let Some(page) = entry(&mut zip, &path, &budget)? else {
            notes.add(MISSING, &path);
            continue;
        };
        match html::markdown(&lmgw_doc_reader::charset::decode(&page)) {
            Ok(md) => match md.too_deep {
                Some(deep) => match &mut too_deep {
                    Some((n, _, _)) => *n += 1,
                    None => too_deep = Some((1, path.clone(), deep)),
                },
                None if !md.text.trim().is_empty() => book.chapters.push(md.text),
                None => {}
            },
            Err(_) => notes.add(UNPARSED, &path),
        }
    }
    book.notes = notes.words();
    if let Some((n, first, deep)) = too_deep {
        book.notes.push(format!(
            "chapters whose elements nest deeper than MAX_HTML_DEPTH ({MAX_HTML_DEPTH}), not \
             read: {n} (the first: {first}, {deep} deep)"
        ));
    }
    Ok(book)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lmgw_doc_reader::wire::PICTURE;
    use std::io::Write;

    pub(crate) fn epub(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        let mut z = zip::ZipWriter::new(&mut buf);
        let o = zip::write::SimpleFileOptions::default();
        for (n, c) in entries {
            z.start_file(*n, o).unwrap();
            z.write_all(c).unwrap();
        }
        z.finish().unwrap();
        buf.into_inner()
    }

    pub(crate) const CONTAINER: &str = r#"<?xml version="1.0"?><container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;

    const GIB: u64 = 1 << 30;

    #[test]
    fn chapters_are_read_in_spine_order_and_gaps_named_once_per_kind() {
        let opf = r#"<package xmlns="http://www.idpf.org/2007/opf"><manifest>
            <item id="b" href="text/two%20b.xhtml" media-type="application/xhtml+xml"/>
            <item id="a" href="text/one.xhtml" media-type="application/xhtml+xml"/>
            <item id="css" href="style.css" media-type="text/css"/>
            <item id="far" href="https://books.example/x.xhtml" media-type="application/xhtml+xml"/>
            <item id="up" href="../../etc/passwd" media-type="application/xhtml+xml"/>
            </manifest><spine><itemref idref="a"/><itemref idref="b"/><itemref idref="css"/>
            <itemref idref="far"/><itemref idref="up"/><itemref idref="gone"/>
            <itemref idref="gone2"/><itemref idref="gone3"/></spine></package>"#;
        let book = read(
            &epub(&[
                ("mimetype", b"application/epub+zip"),
                ("META-INF/container.xml", CONTAINER.as_bytes()),
                ("OEBPS/content.opf", opf.as_bytes()),
                (
                    "OEBPS/text/one.xhtml",
                    b"<html><body><h1>One</h1><p>First.</p><img src='../i.png'/></body></html>",
                ),
                (
                    "OEBPS/text/two b.xhtml",
                    b"<html><body><h2>Two</h2></body></html>",
                ),
            ]),
            GIB,
        )
        .unwrap();
        assert_eq!(book.chapters.len(), 2);
        assert!(
            book.chapters[0].starts_with("# One\n\nFirst."),
            "{}",
            book.chapters[0]
        );
        assert!(book.chapters[0].contains(PICTURE));
        assert_eq!(book.chapters[1], "## Two");
        assert_eq!(book.notes.len(), 3, "{:?}", book.notes);
        assert!(book.notes.iter().any(|n| n.contains("not fetched: 2")));
        assert!(book
            .notes
            .iter()
            .any(|n| n.contains("does not list: 3 (the first: gone)")));
    }

    #[test]
    fn paths_stay_inside_the_archive() {
        assert_eq!(
            resolve("OEBPS", "text/a.xhtml#x").as_deref(),
            Some("OEBPS/text/a.xhtml")
        );
        assert_eq!(
            resolve("OEBPS/text", "../b.xhtml").as_deref(),
            Some("OEBPS/b.xhtml")
        );
        assert_eq!(resolve("OEBPS", "../../x"), None);
        assert_eq!(resolve("", "http://books.example/a"), None);
        assert!(read(b"PK\x05\x06not really", GIB).is_err());
        let e = read(&epub(&[("mimetype", b"application/epub+zip")]), GIB).unwrap_err();
        assert!(matches!(e, Failure::Unreadable(w) if w.contains("container.xml")));
    }

    /// The reviewed package bomb: megabytes of `<a/>` that deflate to
    /// nothing. Streamed, nothing of it is kept, and what is unpacked is
    /// counted, so a budget below its size refuses it by name.
    #[test]
    fn a_package_of_filler_elements_is_streamed_under_the_budget() {
        let mut opf = String::from(r#"<package><manifest>"#);
        opf.push_str(&"<a/>".repeat(4 << 20));
        opf.push_str(
            r#"<item id="a" href="a.xhtml" media-type="application/xhtml+xml"/></manifest>
               <spine><itemref idref="a"/></spine></package>"#,
        );
        let file = epub(&[
            ("META-INF/container.xml", CONTAINER.as_bytes()),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/a.xhtml", b"<html><body><p>Text.</p></body></html>"),
        ]);
        assert!(file.len() < 200 << 10, "{}", file.len());
        let book = read(&file, GIB).unwrap();
        assert_eq!(book.chapters, ["Text."]);
        let e = read(&file, 8 << 20).unwrap_err();
        assert_eq!(e, over("OEBPS/content.opf"));
        // A spine that names one chapter again and again pays for each read.
        let again = format!(
            r#"<package><manifest><item id="a" href="a.xhtml"/></manifest><spine>{}</spine></package>"#,
            r#"<itemref idref="a"/>"#.repeat(1000)
        );
        let chapter = format!("<html><body><p>{}</p></body></html>", "x".repeat(10_000));
        let file = epub(&[
            ("META-INF/container.xml", CONTAINER.as_bytes()),
            ("OEBPS/content.opf", again.as_bytes()),
            ("OEBPS/a.xhtml", chapter.as_bytes()),
        ]);
        assert_eq!(read(&file, 1 << 20).unwrap_err(), over("OEBPS/a.xhtml"));
    }

    #[test]
    fn an_encrypted_chapter_is_named_not_read() {
        let opf = r#"<package><manifest><item id="a" href="a.xhtml"/><item id="b" href="b.xhtml"/>
            </manifest><spine><itemref idref="a"/><itemref idref="b"/></spine></package>"#;
        let enc = r#"<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
            <EncryptedData xmlns="http://www.w3.org/2001/04/xmlenc#"><CipherData>
            <CipherReference URI="OEBPS/b.xhtml"/></CipherData></EncryptedData></encryption>"#;
        let book = read(
            &epub(&[
                ("META-INF/container.xml", CONTAINER.as_bytes()),
                ("META-INF/encryption.xml", enc.as_bytes()),
                ("OEBPS/content.opf", opf.as_bytes()),
                ("OEBPS/a.xhtml", b"<html><body><p>Open.</p></body></html>"),
                ("OEBPS/b.xhtml", b"\x93\x02\xfe noise \x00\x11"),
            ]),
            GIB,
        )
        .unwrap();
        assert_eq!(book.chapters, ["Open."]);
        assert_eq!(book.notes.len(), 1);
        assert!(
            book.notes[0].contains("DRM") && book.notes[0].contains("OEBPS/b.xhtml"),
            "{:?}",
            book.notes
        );
    }
}
