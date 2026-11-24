//! The formats document conversion reads through LibreOffice, pandoc and
//! ffmpeg (document-conversion design §4.2, WP7), told apart by their bytes
//! on top of [`super::more`]:
//!
//! | format | recognised by |
//! |---|---|
//! | legacy Word (`doc`) | an OLE2 file with a `WordDocument` stream |
//! | legacy PowerPoint (`ppt`) | an OLE2 file with a `PowerPoint Document` stream |
//! | a password-protected Office file | an OLE2 file with `EncryptionInfo` and `EncryptedPackage` (an encrypted OOXML package), a `.doc` whose FIB says `fEncrypted`, a `.ppt` with `EncryptedSummary`: refused by name |
//! | RTF | `{\rtf` |
//! | a Word file with equations (`docx+math`) | a docx whose `word/document.xml` holds an OMML `oMath` element |
//! | FictionBook (`fb2`) | XML whose root element is `FictionBook` |
//! | DocBook | XML whose root element carries DocBook 5's namespace, or a DocBook doctype |
//! | a video container | what [`super::sniff`] refuses as one (`video`) |
//!
//! LaTeX, reStructuredText, Org and MediaWiki are plain text to the bytes;
//! a text file's name says which it is (`caps::Format::markup_by_name`), as
//! it does for a web page that starts with another tag.
//!
//! Only a container's directory and a few bytes of a stream are read (the
//! first 12 bytes of `WordDocument`); the docx's body is searched for the
//! equation element as it is unpacked, at most `MAX_UNCOMPRESSED_BYTES` of
//! it (the archive's budget), in pieces, never held whole.

use std::io::{Cursor, Read};

use super::{ok, Kind, SniffError, Sniffed};
use crate::extract::office::MAX_UNCOMPRESSED_BYTES;

/// An OLE2 file of a kind conversion reads through LibreOffice, or one
/// protected by a password; `None` for any other.
pub(super) fn ole2<R: Read + std::io::Seek>(
    file: &mut cfb::CompoundFile<R>,
) -> Option<Result<Sniffed, SniffError>> {
    if file.is_stream("/EncryptionInfo") && file.is_stream("/EncryptedPackage") {
        return Some(Err(SniffError::Encrypted));
    }
    if file.is_stream("/WordDocument") {
        if word_encrypted(file) {
            return Some(Err(SniffError::Encrypted));
        }
        return Some(ok(Kind::Office, "application/msword", "doc"));
    }
    if file.is_stream("/PowerPoint Document") {
        if file.is_stream("/EncryptedSummary") {
            return Some(Err(SniffError::Encrypted));
        }
        return Some(ok(Kind::Office, "application/vnd.ms-powerpoint", "ppt"));
    }
    None
}

/// The `fEncrypted` bit of a Word file's FIB: bit 8 of the 16 flag bits at
/// offset 10 of the `WordDocument` stream ([MS-DOC] 2.5.2, FibBase).
fn word_encrypted<R: Read + std::io::Seek>(file: &mut cfb::CompoundFile<R>) -> bool {
    let Ok(mut stream) = file.open_stream("/WordDocument") else {
        return false;
    };
    let mut head = [0u8; 12];
    if stream.read_exact(&mut head).is_err() {
        return false;
    }
    u16::from_le_bytes([head[10], head[11]]) & 0x0100 != 0
}

/// RTF: `{\rtf`, after a byte-order mark if one is there.
pub(super) fn rtf(bytes: &[u8]) -> bool {
    bytes
        .strip_prefix(b"\xEF\xBB\xBF")
        .unwrap_or(bytes)
        .starts_with(b"{\\rtf")
}

/// What the docx's body is searched for: an OMML element (`<m:oMath>`,
/// `<m:oMathPara>`), by its local name after any prefix.
const OMML: &[u8] = b":oMath";

/// Whether the docx `bytes` has equations: its `word/document.xml`
/// unpacked piece by piece, at most `MAX_UNCOMPRESSED_BYTES`, searched for
/// [`OMML`] across the pieces' seams. A body past that is no docx with
/// equations here; its reader names it.
pub(super) fn docx_math(bytes: &[u8]) -> bool {
    let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return false;
    };
    let Ok(entry) = zip.by_name("word/document.xml") else {
        return false;
    };
    let mut reader = entry.take(MAX_UNCOMPRESSED_BYTES);
    let mut buf = vec![0u8; 64 << 10];
    // The last bytes of the piece before, so a match across a seam counts.
    let mut carry: Vec<u8> = Vec::with_capacity(OMML.len());
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => return false,
            Ok(n) => n,
        };
        carry.extend_from_slice(&buf[..n.min(OMML.len())]);
        if carry.windows(OMML.len()).any(|w| w == OMML)
            || buf[..n].windows(OMML.len()).any(|w| w == OMML)
        {
            return true;
        }
        let keep = n.min(OMML.len() - 1);
        carry.clear();
        carry.extend_from_slice(&buf[n - keep..n]);
    }
}

/// How much of an XML file is looked at for its root element: 64 KiB (a
/// long doctype with an internal subset fits).
const XML_HEAD_BYTES: usize = 64 << 10;

/// FictionBook or DocBook, from the XML's prolog and root element.
pub(super) fn xml_markup(bytes: &[u8]) -> Option<Sniffed> {
    let head = &bytes[..bytes.len().min(XML_HEAD_BYTES)];
    let head = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let mut rest = head;
    let mut doctype_docbook = false;
    loop {
        let start = rest.iter().position(|b| !b.is_ascii_whitespace())?;
        rest = &rest[start..];
        if rest.starts_with(b"<?") {
            rest = &rest[find(rest, b"?>")? + 2..];
        } else if rest.starts_with(b"<!--") {
            rest = &rest[find(rest, b"-->")? + 3..];
        } else if rest.starts_with(b"<!DOCTYPE") || rest.starts_with(b"<!doctype") {
            let end = doctype_end(rest)?;
            doctype_docbook = find(&rest[..end], b"DocBook").is_some();
            rest = &rest[end..];
        } else {
            break;
        }
    }
    if !rest.starts_with(b"<") {
        return None;
    }
    let tag_end = rest.iter().position(|b| *b == b'>')?;
    let tag = &rest[1..tag_end];
    let name_end = tag
        .iter()
        .position(|b| b.is_ascii_whitespace() || *b == b'/')
        .unwrap_or(tag.len());
    let name = &tag[..name_end];
    let local = name.rsplit(|b| *b == b':').next().unwrap_or(name);
    if local == b"FictionBook" {
        return Some(Sniffed {
            kind: Kind::Text,
            mime: "application/x-fictionbook+xml",
            sub: "fb2",
        });
    }
    let docbook_ns = find(tag, b"http://docbook.org/ns/docbook").is_some();
    (docbook_ns || doctype_docbook).then_some(Sniffed {
        kind: Kind::Text,
        mime: "application/docbook+xml",
        sub: "docbook",
    })
}

/// The end of a doctype that starts `rest`, past an internal subset
/// (`[ … ]`).
fn doctype_end(rest: &[u8]) -> Option<usize> {
    let mut depth = 0usize;
    for (i, b) in rest.iter().enumerate() {
        match b {
            b'[' => depth += 1,
            b']' => depth = depth.saturating_sub(1),
            b'>' if depth == 0 => return Some(i + 1),
            _ => {}
        }
    }
    None
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A video container [`super::sniff`] refused, as conversion reads it: its
/// sound (`Kind::Audio`, sub `video`).
pub(super) fn video(what: &str) -> Result<Sniffed, SniffError> {
    let mime = if what == "Matroska" {
        "video/x-matroska"
    } else {
        "video/mp4"
    };
    ok(Kind::Audio, mime, "video")
}

#[cfg(test)]
mod tests {
    use super::super::sniff_convert;
    use super::*;

    fn sub(b: &[u8]) -> Result<&'static str, SniffError> {
        sniff_convert(b).map(|s| s.sub)
    }

    #[test]
    fn rtf_fb2_and_docbook_are_told_apart_and_other_xml_stays_text() {
        assert_eq!(sub(b"{\\rtf1\\ansi Hello}"), Ok("rtf"));
        assert_eq!(sub(b"\xEF\xBB\xBF{\\rtf1 x}"), Ok("rtf"));
        assert_eq!(sub(b"{\\rt"), Ok("text"));
        let fb2 = b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<FictionBook \
                    xmlns=\"http://www.gribuser.ru/xml/fictionbook/2.0\"><body/></FictionBook>";
        assert_eq!(sub(fb2), Ok("fb2"));
        let db5 = b"<?xml version=\"1.0\"?><!-- x --><article \
                    xmlns=\"http://docbook.org/ns/docbook\" version=\"5.0\"><title>x</title>";
        assert_eq!(sub(db5), Ok("docbook"));
        let db4 = b"<?xml version=\"1.0\"?>\n<!DOCTYPE book PUBLIC \"-//OASIS//DTD DocBook XML \
                    V4.5//EN\" \"http://www.oasis-open.org/docbook/xml/4.5/docbookx.dtd\" [ \
                    <!ENTITY x \"y\"> ]>\n<book><title>x</title></book>";
        assert_eq!(sub(db4), Ok("docbook"));
        assert_eq!(
            sub(b"<?xml version=\"1.0\"?><note><to>x</to></note>"),
            Ok("text")
        );
        assert_eq!(
            sub(b"<article><title>not docbook</title></article>"),
            Ok("text")
        );
    }

    #[test]
    fn a_docx_with_equations_is_its_own_format() {
        use std::io::Write;
        let docx = |body: &str| {
            let mut buf = Cursor::new(Vec::new());
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default();
            z.start_file("[Content_Types].xml", o).unwrap();
            z.write_all(b"<Types><Override ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/></Types>").unwrap();
            z.start_file("word/document.xml", o).unwrap();
            z.write_all(body.as_bytes()).unwrap();
            z.finish().unwrap();
            buf.into_inner()
        };
        let math = docx(
            "<w:document><w:body><w:p><m:oMathPara><m:oMath><m:r><m:t>x</m:t>\
                         </m:r></m:oMath></m:oMathPara></w:p></w:body></w:document>",
        );
        assert_eq!(sub(&math), Ok("docx+math"));
        // An equation past the first piece, across a seam.
        let mut long = "<w:document><w:body>".to_string();
        long.push_str(&"<w:p><w:r><w:t>words</w:t></w:r></w:p>".repeat(3000));
        long.push_str("<m:oMath/></w:body></w:document>");
        assert_eq!(sub(&docx(&long)), Ok("docx+math"));
        let plain = docx(
            "<w:document><w:body><w:p><w:r><w:t>oMath</w:t></w:r></w:p></w:body>\
                          </w:document>",
        );
        assert_eq!(sub(&plain), Ok("docx"));
        // Chat's sniffer keeps calling it a docx.
        assert_eq!(super::super::sniff(&math).map(|s| s.sub), Ok("docx"));
    }

    #[test]
    fn video_containers_are_read_for_their_sound() {
        // An `ftyp` with `isom` and no track: a video container to `sniff`.
        let mut mp4 = vec![0, 0, 0, 16];
        mp4.extend_from_slice(b"ftypisom\0\0\0\0");
        assert_eq!(
            super::super::sniff(&mp4),
            Err(SniffError::Video("MP4/QuickTime"))
        );
        let s = sniff_convert(&mp4).unwrap();
        assert_eq!((s.kind, s.sub, s.mime), (Kind::Audio, "video", "video/mp4"));
    }
}
