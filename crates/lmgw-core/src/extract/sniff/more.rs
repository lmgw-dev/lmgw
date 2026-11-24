//! The formats only document conversion reads (document-conversion design
//! §4.2, WP4), told apart by their bytes on top of [`super::sniff`]: Chat
//! attachments and knowledge bases keep [`super::sniff`] and its refusals
//! until they move to the capability table too (WP7).
//!
//! | format | recognised by |
//! |---|---|
//! | TIFF | `II*\0` or `MM\0*` |
//! | legacy Excel (`xls`) | an OLE2 file with a `Workbook` (BIFF8) or `Book` (BIFF5) stream |
//! | Outlook `.msg` | an OLE2 file with `__properties_version1.0` (refused by name, not read yet) |
//! | EPUB | a ZIP whose `mimetype` entry is `application/epub+zip` |
//! | e-mail (`eml`) | a header block of RFC 5322 fields with `From` and two more known ones |
//! | HTML | `<!doctype html`, `<html`, `<head` or `<body` first (after a BOM, an XML declaration, comments and white space) |
//!
//! Legacy Word and PowerPoint files, RTF, FictionBook, DocBook, a docx with
//! equations and video containers are told apart in [`super::tools`] (WP7).
//! An OLE2 file of any other kind stays [`SniffError::LegacyOffice`]. E-mail and HTML are recognised from the raw
//! bytes, so a message or a page in another charset than UTF-8 is one too;
//! their readers decode the charset.
//!
//! Telling a format apart reads a container's directory, never its
//! content: a ZIP's central directory and at most
//! [`ZIP_MIMETYPE_BYTES`](super::ZIP_MIMETYPE_BYTES) of its `mimetype` entry
//! (and its content types, against the archive's budget), an OLE2 file's
//! stream names. That is still work on
//! an untrusted file, so it runs in a slot of the tool pool
//! ([`sniff_convert_async`]), where the pool counts it, not on a thread of
//! its own beside every other file of the request.

use std::io::Cursor;

use bytes::Bytes;

use super::{ok, sniff, zip_mimetype, Kind, SniffError, Sniffed, OLE_MAGIC};

/// How much of a file the e-mail and HTML tests look at: 64 KiB. Only the
/// recognition reads this far (a long `Received` chain fits in it); the
/// readers take the whole file.
pub const SNIFF_HEAD_BYTES: usize = 64 << 10;

/// [`sniff`], plus the formats of the module doc.
pub fn sniff_convert(bytes: &[u8]) -> Result<Sniffed, SniffError> {
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return ok(Kind::Image, "image/tiff", "tiff");
    }
    if bytes.starts_with(&OLE_MAGIC) {
        return ole2(bytes);
    }
    if super::tools::rtf(bytes) {
        return ok(Kind::Text, "application/rtf", "rtf");
    }
    match sniff(bytes) {
        Ok(s) if s.kind == Kind::Text => Ok(text_kind(bytes)
            .or_else(|| super::tools::xml_markup(bytes))
            .unwrap_or(s)),
        Ok(s) if s.sub == "docx" && super::tools::docx_math(bytes) => {
            ok(Kind::Office, s.mime, "docx+math")
        }
        Err(SniffError::UnknownZip) if epub(bytes) => {
            ok(Kind::Office, "application/epub+zip", "epub")
        }
        Err(SniffError::Video(what)) => super::tools::video(what),
        Err(SniffError::Unrecognised) => text_kind(bytes).ok_or(SniffError::Unrecognised),
        other => other,
    }
}

/// [`sniff_convert`] in a slot of the tool pool (a ZIP's or an OLE2 file's
/// directory is read): given back before the file's reader takes its own.
pub async fn sniff_convert_async(bytes: Bytes) -> Result<Sniffed, SniffError> {
    match crate::extract::convert::pool::blocking(move || sniff_convert(&bytes)).await {
        Ok(r) => r,
        Err(_) => Err(SniffError::Unrecognised),
    }
}

/// An OLE2 compound file by its streams.
fn ole2(bytes: &[u8]) -> Result<Sniffed, SniffError> {
    let Ok(mut file) = cfb::CompoundFile::open(Cursor::new(bytes)) else {
        return Err(SniffError::LegacyOffice);
    };
    if let Some(known) = super::tools::ole2(&mut file) {
        return known;
    }
    if file.is_stream("/Workbook") || file.is_stream("/Book") {
        return ok(Kind::Office, "application/vnd.ms-excel", "xls");
    }
    if file.is_stream("/__properties_version1.0") {
        return Err(SniffError::OutlookMessage);
    }
    Err(SniffError::LegacyOffice)
}

/// An EPUB container: the `mimetype` entry the format puts first, of which
/// at most [`ZIP_MIMETYPE_BYTES`](super::ZIP_MIMETYPE_BYTES) are unpacked.
fn epub(bytes: &[u8]) -> bool {
    let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return false;
    };
    zip_mimetype(&mut zip).flatten().as_deref() == Some("application/epub+zip")
}

/// HTML or an e-mail message, from the file's first bytes.
fn text_kind(bytes: &[u8]) -> Option<Sniffed> {
    let head = &bytes[..bytes.len().min(SNIFF_HEAD_BYTES)];
    if email(head) {
        return Some(Sniffed {
            kind: Kind::Text,
            mime: "message/rfc822",
            sub: "eml",
        });
    }
    html(head).then_some(Sniffed {
        kind: Kind::Text,
        mime: "text/html",
        sub: "html",
    })
}

/// The header fields that make a header block a message's.
const MAIL_FIELDS: &[&str] = &[
    "to",
    "cc",
    "subject",
    "date",
    "message-id",
    "mime-version",
    "received",
    "return-path",
    "delivered-to",
    "reply-to",
    "in-reply-to",
    "references",
    "content-type",
    "x-mailer",
    "user-agent",
];

/// Whether `head` starts with a header block (every line a field or a
/// field's folded continuation, up to the first empty line or the end of
/// what is looked at) holding `From` (or `Sender`) and at least two other
/// fields of [`MAIL_FIELDS`].
fn email(head: &[u8]) -> bool {
    let (mut from, mut others) = (false, 0usize);
    let mut lines = head.split(|b| *b == b'\n').peekable();
    let mut first = true;
    while let Some(line) = lines.next() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        // The last line seen may be cut by the window: not judged.
        if lines.peek().is_none() && head.len() == SNIFF_HEAD_BYTES {
            break;
        }
        if line.is_empty() {
            break;
        }
        if line[0] == b' ' || line[0] == b'\t' {
            if first {
                return false;
            }
            continue;
        }
        first = false;
        let Some(colon) = line.iter().position(|b| *b == b':') else {
            return false;
        };
        let name = &line[..colon];
        if name.is_empty() || name.len() > 76 || !name.iter().all(|b| b.is_ascii_graphic()) {
            return false;
        }
        let name = String::from_utf8_lossy(name).to_ascii_lowercase();
        if name == "from" || name == "sender" {
            from = true;
        } else if MAIL_FIELDS.contains(&name.as_str()) {
            others += 1;
        }
    }
    from && others >= 2
}

/// Whether `head` starts as an HTML document.
fn html(head: &[u8]) -> bool {
    let mut rest = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    loop {
        let start = rest.iter().position(|b| !b.is_ascii_whitespace());
        rest = &rest[start.unwrap_or(rest.len())..];
        if rest.starts_with(b"<!--") {
            match find(rest, b"-->") {
                Some(end) => rest = &rest[end + 3..],
                None => return false,
            }
        } else if rest.starts_with(b"<?xml") {
            match find(rest, b"?>") {
                Some(end) => rest = &rest[end + 2..],
                None => return false,
            }
        } else {
            break;
        }
    }
    let lower: Vec<u8> = rest.iter().take(16).map(u8::to_ascii_lowercase).collect();
    let tag = |t: &[u8]| {
        lower.starts_with(t)
            && lower
                .get(t.len())
                .is_none_or(|b| b.is_ascii_whitespace() || *b == b'>')
    };
    tag(b"<!doctype") && lower.get(10..14) == Some(b"html")
        || tag(b"<html")
        || tag(b"<head")
        || tag(b"<body")
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(b: &[u8]) -> Result<(Kind, &'static str), SniffError> {
        sniff_convert(b).map(|s| (s.kind, s.sub))
    }

    #[test]
    fn mail_html_and_tiff_are_told_apart_and_plain_text_stays_text() {
        let mail = b"From: Ann Example <ann@example.com>\r\nTo: bob@example.com\r\n\
                     Subject: Hi\r\n  folded\r\nMIME-Version: 1.0\r\n\r\nHello\r\n";
        assert_eq!(sub(mail), Ok((Kind::Text, "eml")));
        // Latin-1 in the body is still a message.
        let mut latin = b"From: a@example.com\nTo: b@example.com\nDate: x\n\n".to_vec();
        latin.push(0xE9);
        assert_eq!(sub(&latin), Ok((Kind::Text, "eml")));
        // One known field besides From is not a message: a note with fields.
        assert_eq!(sub(b"From: me\nTo: you\n\nnotes"), Ok((Kind::Text, "text")));
        assert_eq!(sub(b"Title: x\nFrom: y\n"), Ok((Kind::Text, "text")));
        assert_eq!(sub(b"# From: nobody\n"), Ok((Kind::Text, "text")));

        for page in [
            &b"<!DOCTYPE html><title>x</title>"[..],
            b"\xEF\xBB\xBF  <!-- a --> <html lang=\"en\">",
            b"<?xml version=\"1.0\"?>\n<html xmlns=\"http://www.w3.org/1999/xhtml\">",
            b"<HEAD><title>x</title>",
        ] {
            assert_eq!(sub(page), Ok((Kind::Text, "html")), "{page:?}");
        }
        assert_eq!(sub(b"<htmlish>"), Ok((Kind::Text, "text")));
        assert_eq!(sub(b"<p align=center>README</p>"), Ok((Kind::Text, "text")));

        assert_eq!(sub(b"II*\0\x08\0\0\0"), Ok((Kind::Image, "tiff")));
        assert_eq!(sub(b"MM\0*\0\0\0\x08"), Ok((Kind::Image, "tiff")));
        // Chat's sniffer does not know them.
        assert_eq!(super::super::sniff(b"II*\0"), Err(SniffError::Unrecognised));
    }

    #[test]
    fn an_epub_is_a_zip_with_its_mimetype() {
        use std::io::Write;
        let mut buf = Cursor::new(Vec::new());
        let mut z = zip::ZipWriter::new(&mut buf);
        let o = zip::write::SimpleFileOptions::default();
        z.start_file("mimetype", o).unwrap();
        z.write_all(b"application/epub+zip").unwrap();
        z.start_file("META-INF/container.xml", o).unwrap();
        z.write_all(b"<container/>").unwrap();
        z.finish().unwrap();
        let bytes = buf.into_inner();
        assert_eq!(sub(&bytes), Ok((Kind::Office, "epub")));
        assert_eq!(super::super::sniff(&bytes), Err(SniffError::UnknownZip));
    }

    /// The reviewed bomb: a ZIP whose `mimetype` entry unpacks to far more
    /// than it is (64 MiB here; the review's was 1 GiB). A few dozen bytes
    /// of it are unpacked, and it is no EPUB.
    #[test]
    fn a_mimetype_bomb_is_not_unpacked() {
        use std::io::Write;
        let mut buf = Cursor::new(Vec::new());
        let mut z = zip::ZipWriter::new(&mut buf);
        let o = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        z.start_file("mimetype", o).unwrap();
        // Starts as an EPUB's would, then goes on for 64 MiB.
        z.write_all(b"application/epub+zip").unwrap();
        let chunk = vec![b' '; 1 << 20];
        for _ in 0..64 {
            z.write_all(&chunk).unwrap();
        }
        z.finish().unwrap();
        let bytes = buf.into_inner();
        assert!(bytes.len() < 1 << 20, "{} bytes", bytes.len());
        let started = std::time::Instant::now();
        assert_eq!(sub(&bytes), Err(SniffError::UnknownZip));
        // Unpacking it all takes most of a second in a test build; 65
        // bytes take none.
        assert!(
            started.elapsed().as_millis() < 200,
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_broken_ole2_file_stays_legacy_office() {
        let mut b = OLE_MAGIC.to_vec();
        b.extend_from_slice(&[0; 600]);
        assert_eq!(sub(&b), Err(SniffError::LegacyOffice));
    }
}
