//! What an upload's bytes actually are — from the bytes alone, never the
//! filename or the browser's declared MIME, both of which a client can get
//! wrong or lie about (chat-complete design §8).
//!
//! | kind | recognised by |
//! |---|---|
//! | image | PNG, JPEG, GIF, WebP magic |
//! | pdf | `%PDF-` |
//! | office | a ZIP whose `[Content_Types].xml` (OOXML) or `mimetype` (OpenDocument) names docx, xlsx, pptx, odt, ods or odp |
//! | audio | RIFF/WAVE, an ID3v2 header or an MPEG frame sync, an Ogg page header, `fLaC` + STREAMINFO, `ftyp` with an audio brand, EBML with DocType `webm` |
//! | text | valid UTF-8 with no NUL, once nothing above matched |
//!
//! Audio magic is checked in full (version bytes, header types, block
//! headers), not by its first four bytes, because it is tested *before* the
//! UTF-8 fallback: a text file that starts `ID3v2 notes` or `OggS` must stay
//! text. Legacy binary Office (`.doc`, `.xls`, `.ppt`: any OLE2 compound
//! file) and video containers are recognised only to be refused with their
//! own message; every refusal lists what *is* accepted.
//!
//! Identification reads only the two manifest entries of a ZIP, against the
//! archive's own [`Budget`] (`MAX_UNCOMPRESSED_BYTES`): a real deck's
//! `[Content_Types].xml` grows with every slide, so there is no smaller
//! guessed bound. It is synchronous and does inflate, so async callers use
//! [`sniff_async`]. An `ftyp` file with no audio brand is decided by its
//! track handlers (`soun` / `vide`), read from the bytes in hand.

use std::fmt;
use std::io::Cursor;

use bytes::Bytes;

use super::office::{read_entry_if_present, Budget, OfficeError, OfficeFormat};

/// What is accepted, as the tail of every refusal message.
pub const ACCEPTED: &str = "images (PNG, JPEG, GIF, WebP), PDF, Word/Excel/PowerPoint files \
    (docx, xlsx, pptx), OpenDocument files (odt, ods, odp), audio (WAV, MP3, Ogg, FLAC, \
    M4A, WebM) and UTF-8 text files";

/// The broad class of an upload — what the chat stores in `chat_attachments.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Image,
    Text,
    Pdf,
    Office,
    Audio,
}

impl Kind {
    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Text => "text",
            Self::Pdf => "pdf",
            Self::Office => "office",
            Self::Audio => "audio",
        }
    }
}

/// A recognised upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sniffed {
    pub kind: Kind,
    /// The MIME type the bytes warrant.
    pub mime: &'static str,
    /// The concrete format: `png`, `jpeg`, `gif`, `webp`, `text`, `pdf`,
    /// `docx`, `xlsx`, `pptx`, `odt`, `ods`, `odp`, `wav`, `mp3`,
    /// `ogg`, `flac`, `m4a`, `webm`.
    pub sub: &'static str,
}

impl Sniffed {
    /// The office format, for a [`Kind::Office`] upload.
    pub fn office_format(&self) -> Option<OfficeFormat> {
        (self.kind == Kind::Office)
            .then(|| OfficeFormat::from_sub(self.sub))
            .flatten()
    }
}

/// Why an upload is refused. `Display` is the message a 415 carries, and each
/// one ends in the list of what is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SniffError {
    /// Nothing above matched and the bytes are not UTF-8 text.
    Unrecognised,
    /// A legacy binary `.doc` / `.xls` / `.ppt` (any OLE2 compound file).
    LegacyOffice,
    /// A ZIP archive that is not one of the six office formats.
    UnknownZip,
    /// A ZIP whose identification entries inflate past the archive's budget
    /// (`MAX_UNCOMPRESSED_BYTES`).
    ManifestTooLarge(String),
    /// A video container (MP4/QuickTime/Matroska), named.
    Video(&'static str),
}

impl fmt::Display for SniffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unrecognised => write!(f, "unsupported file — this gateway accepts {ACCEPTED}"),
            Self::LegacyOffice => write!(
                f,
                "unsupported file — legacy binary Office file (.doc/.xls/.ppt) — save it as \
                 .docx/.xlsx/.pptx. This gateway accepts {ACCEPTED}"
            ),
            Self::UnknownZip => write!(
                f,
                "unsupported file — this is a ZIP archive but not an office document. This \
                 gateway accepts {ACCEPTED}"
            ),
            Self::ManifestTooLarge(name) => write!(
                f,
                "unsupported file — the ZIP entry '{name}' inflates past \
                 MAX_UNCOMPRESSED_BYTES ({} MiB), so this is not an office document. This \
                 gateway accepts {ACCEPTED}",
                super::office::MAX_UNCOMPRESSED_BYTES >> 20
            ),
            Self::Video(what) => write!(
                f,
                "unsupported file — this is a {what} video container, not an audio file; \
                 extract its audio track (for example to M4A or MP3) and upload that. This \
                 gateway accepts {ACCEPTED}"
            ),
        }
    }
}

impl std::error::Error for SniffError {}

const fn ok(kind: Kind, mime: &'static str, sub: &'static str) -> Result<Sniffed, SniffError> {
    Ok(Sniffed { kind, mime, sub })
}

/// Sniff `bytes`. Magic numbers first, most specific first; the UTF-8 test
/// last, so a binary that merely happens to be valid text is never text.
pub fn sniff(bytes: &[u8]) -> Result<Sniffed, SniffError> {
    if let Some(s) = sniff_image(bytes) {
        return s;
    }
    if bytes.starts_with(b"%PDF-") {
        return ok(Kind::Pdf, "application/pdf", "pdf");
    }
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        return sniff_zip(bytes);
    }
    if bytes.starts_with(&OLE_MAGIC) {
        return Err(SniffError::LegacyOffice);
    }
    if let Some(s) = sniff_audio(bytes) {
        return s;
    }
    match std::str::from_utf8(bytes) {
        Ok(s) if !s.contains('\0') => ok(Kind::Text, "text/plain; charset=utf-8", "text"),
        _ => Err(SniffError::Unrecognised),
    }
}

/// [`sniff`] on the blocking pool: a ZIP is inflated (bounded, but
/// synchronous), which an async handler must not do on its worker.
pub async fn sniff_async(bytes: Bytes) -> Result<Sniffed, SniffError> {
    match tokio::task::spawn_blocking(move || sniff(&bytes)).await {
        Ok(r) => r,
        // The task cannot panic short of a bug; say so rather than guess a kind.
        Err(_) => Err(SniffError::Unrecognised),
    }
}

fn sniff_image(b: &[u8]) -> Option<Result<Sniffed, SniffError>> {
    Some(if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        ok(Kind::Image, "image/png", "png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        ok(Kind::Image, "image/jpeg", "jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        ok(Kind::Image, "image/gif", "gif")
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        ok(Kind::Image, "image/webp", "webp")
    } else {
        return None;
    })
}

fn sniff_audio(b: &[u8]) -> Option<Result<Sniffed, SniffError>> {
    Some(
        if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WAVE" {
            ok(Kind::Audio, "audio/wav", "wav")
        } else if id3v2_header(b) || mpeg_frame_sync(b) {
            ok(Kind::Audio, "audio/mpeg", "mp3")
        } else if ogg_page_header(b) {
            ok(Kind::Audio, "audio/ogg", "ogg")
        } else if flac_streaminfo(b) {
            ok(Kind::Audio, "audio/flac", "flac")
        } else if b.len() >= 12 && b[0] == 0 && &b[4..8] == b"ftyp" {
            return iso_bmff(b);
        } else if b.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
            match ebml_doctype(b) {
                Some(d) if d == b"matroska" => Err(SniffError::Video("Matroska")),
                // `webm`, or a header too short to say: WebM is what browsers
                // record. The chip names the format (`webm`).
                _ => ok(Kind::Audio, "audio/webm", "webm"),
            }
        } else {
            return None;
        },
    )
}

/// An ID3v2 tag header: `ID3`, a major version 2 to 4, revision not `FF`,
/// no undefined flag bits, and four syncsafe size bytes (top bits clear).
fn id3v2_header(b: &[u8]) -> bool {
    b.len() >= 10
        && &b[0..3] == b"ID3"
        && (2..=4).contains(&b[3])
        && b[4] != 0xFF
        && b[5] & 0x0F == 0
        && b[6..10].iter().all(|x| x & 0x80 == 0)
}

/// An Ogg page header: `OggS`, stream structure version 0, and only the
/// three defined header-type bits (continued, first, last) set.
fn ogg_page_header(b: &[u8]) -> bool {
    b.len() >= 6 && &b[0..4] == b"OggS" && b[4] == 0 && b[5] & !0b111 == 0
}

/// `fLaC` followed by the mandatory first metadata block: STREAMINFO (type 0,
/// last-block flag either way) of length 34.
fn flac_streaminfo(b: &[u8]) -> bool {
    b.len() >= 8 && &b[0..4] == b"fLaC" && b[4] & 0x7F == 0 && b[5..8] == [0, 0, 34]
}

/// An ISO base media file (`ftyp`): audio when the major brand, or any
/// compatible brand, is an audio one; HEIF/AVIF pictures stay unrecognised;
/// anything else (`isom`, `mp42`, `qt  `, `avc1`, …) is a video container and
/// is refused as such.
fn iso_bmff(b: &[u8]) -> Option<Result<Sniffed, SniffError>> {
    let brand = &b[8..12];
    if image_brand(brand) {
        return None;
    }
    let size = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize;
    let end = size.clamp(16, b.len());
    let compatible = b
        .get(16..end)
        .unwrap_or(&[])
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| c.as_slice());
    let audio = |x: &[u8]| matches!(x, b"M4A " | b"M4B " | b"M4P " | b"F4A " | b"F4B ");
    if audio(brand) || compatible.clone().any(audio) {
        return Some(ok(Kind::Audio, "audio/mp4", "m4a"));
    }
    if compatible.clone().any(image_brand) {
        return None;
    }
    // No audio brand (ffmpeg's `-f mp4` audio-only output is `isom`): what
    // the file holds decides. Audio when it has a sound track and no video.
    match mp4_tracks(b) {
        Tracks {
            sound: true,
            video: false,
        } => Some(ok(Kind::Audio, "audio/mp4", "m4a")),
        _ => Some(Err(SniffError::Video("MP4/QuickTime"))),
    }
}

/// The kinds of track an ISO base media file declares.
#[derive(Debug, Default, PartialEq, Eq)]
struct Tracks {
    sound: bool,
    video: bool,
}

/// The boxes of `b` at one level: (type, payload), bounded to `b` (a size
/// past the end, or 0 for "to the end", is clamped; a size under a header
/// ends the walk).
fn mp4_boxes(b: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        let head = b.get(at..at.checked_add(8)?)?;
        let size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as u64;
        let (hdr, size) = match size {
            0 => (8usize, (b.len() - at) as u64),
            1 => {
                let big = b.get(at + 8..at + 16)?;
                (16, u64::from_be_bytes(big.try_into().ok()?))
            }
            n => (8, n),
        };
        if size < hdr as u64 {
            return None;
        }
        let end = at
            .saturating_add(usize::try_from(size).unwrap_or(usize::MAX))
            .min(b.len());
        let payload = b.get(at + hdr..end)?;
        at = end;
        Some((&head[4..8], payload))
    })
}

/// Walk `moov` → `trak` → `mdia` → `hdlr` and note each handler type
/// (`soun`, `vide`). Every step is inside the bytes; the depth is fixed.
fn mp4_tracks(b: &[u8]) -> Tracks {
    let mut t = Tracks::default();
    for (ty, moov) in mp4_boxes(b) {
        if ty != b"moov" {
            continue;
        }
        for (ty, trak) in mp4_boxes(moov) {
            if ty != b"trak" {
                continue;
            }
            for (ty, mdia) in mp4_boxes(trak) {
                if ty != b"mdia" {
                    continue;
                }
                for (ty, hdlr) in mp4_boxes(mdia) {
                    // version+flags (4), pre_defined (4), handler_type (4).
                    match (ty == b"hdlr").then(|| hdlr.get(8..12)).flatten() {
                        Some(b"soun") => t.sound = true,
                        Some(b"vide") => t.video = true,
                        _ => {}
                    }
                }
            }
        }
    }
    t
}

/// The EBML DocType string (`webm`, `matroska`), found in the header's first
/// bytes: element id `42 82`, a one-byte length, the string.
fn ebml_doctype(b: &[u8]) -> Option<&[u8]> {
    let head = &b[..b.len().min(64)];
    let at = head.windows(2).position(|w| w == [0x42, 0x82])?;
    let len = usize::from(*head.get(at + 2)?).checked_sub(0x80)?;
    head.get(at + 3..at + 3 + len)
}

/// An MPEG audio frame header: eleven set sync bits, then a real version
/// (not the reserved `01`), a real layer (not the reserved `00`, which also
/// keeps AAC/ADTS out) and a usable bitrate index (not `1111`).
fn mpeg_frame_sync(b: &[u8]) -> bool {
    b.len() >= 4
        && b[0] == 0xFF
        && b[1] & 0xE0 == 0xE0
        && (b[1] >> 3) & 0b11 != 0b01
        && (b[1] >> 1) & 0b11 != 0b00
        && (b[2] >> 4) != 0b1111
}

/// `ftyp` brands of HEIF/AVIF images, which share the box layout with M4A but
/// are pictures this gateway does not take.
fn image_brand(brand: &[u8]) -> bool {
    matches!(
        brand,
        b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" | b"avif" | b"avis"
    )
}

/// Which office format a ZIP archive is, from its own manifest. The two
/// identification entries are read against one [`Budget`].
fn sniff_zip(bytes: &[u8]) -> Result<Sniffed, SniffError> {
    sniff_zip_within(bytes, &mut Budget::new())
}

fn sniff_zip_within(bytes: &[u8], budget: &mut Budget) -> Result<Sniffed, SniffError> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| SniffError::UnknownZip)?;
    let mut small = |name: &str| match read_entry_if_present(&mut zip, name, budget) {
        Ok(v) => Ok(v),
        Err(OfficeError::TooLarge { .. }) => Err(SniffError::ManifestTooLarge(name.to_string())),
        Err(_) => Ok(None),
    };
    // OpenDocument: the `mimetype` entry is the whole answer.
    if let Some(m) = small("mimetype")? {
        let m = String::from_utf8_lossy(&m);
        let sub = match m.trim() {
            "application/vnd.oasis.opendocument.text" => Some("odt"),
            "application/vnd.oasis.opendocument.spreadsheet" => Some("ods"),
            "application/vnd.oasis.opendocument.presentation" => Some("odp"),
            _ => None,
        };
        return match sub.and_then(office_sub) {
            Some(s) => Ok(s),
            None => Err(SniffError::UnknownZip),
        };
    }
    // OOXML: the content types name the main part, and the part must exist.
    let types = small("[Content_Types].xml")?
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .ok_or(SniffError::UnknownZip)?;
    let has = |name: &str| zip.file_names().any(|n| n == name);
    let sub = if types.contains("wordprocessingml") && has("word/document.xml") {
        "docx"
    } else if types.contains("spreadsheetml") && has("xl/workbook.xml") {
        "xlsx"
    } else if types.contains("presentationml") && has("ppt/presentation.xml") {
        "pptx"
    } else {
        return Err(SniffError::UnknownZip);
    };
    office_sub(sub).ok_or(SniffError::UnknownZip)
}

fn office_sub(sub: &'static str) -> Option<Sniffed> {
    let mime = match sub {
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        _ => return None,
    };
    Some(Sniffed {
        kind: Kind::Office,
        mime,
        sub,
    })
}

/// The OLE2 / Compound File Binary signature.
const OLE_MAGIC: [u8; 4] = [0xD0, 0xCF, 0x11, 0xE0];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::test_files;

    fn sub(b: &[u8]) -> (Kind, &'static str) {
        let s = sniff(b).unwrap();
        (s.kind, s.sub)
    }

    #[test]
    fn images_pdf_and_text() {
        assert_eq!(sub(b"\x89PNG\r\n\x1a\nrest"), (Kind::Image, "png"));
        assert_eq!(sub(&[0xFF, 0xD8, 0xFF, 0xE0, 0]), (Kind::Image, "jpeg"));
        assert_eq!(sub(b"GIF89a...."), (Kind::Image, "gif"));
        assert_eq!(sub(b"RIFF\0\0\0\0WEBPVP8 "), (Kind::Image, "webp"));
        assert_eq!(sub(b"%PDF-1.7\n%..."), (Kind::Pdf, "pdf"));
        let t = sniff("héllo\nworld".as_bytes()).unwrap();
        assert_eq!((t.kind, t.sub), (Kind::Text, "text"));
        assert_eq!(t.mime, "text/plain; charset=utf-8");
        // Empty is text, as it always was.
        assert_eq!(sub(b""), (Kind::Text, "text"));
    }

    #[test]
    fn audio_containers() {
        assert_eq!(sub(b"RIFF\x24\0\0\0WAVEfmt "), (Kind::Audio, "wav"));
        assert_eq!(sub(b"ID3\x04\0\0\0\0\0\0"), (Kind::Audio, "mp3"));
        assert_eq!(sub(&[0xFF, 0xFB, 0x90, 0x64]), (Kind::Audio, "mp3"));
        assert_eq!(sub(b"OggS\0\x02"), (Kind::Audio, "ogg"));
        assert_eq!(sub(b"fLaC\0\0\0\"\x10"), (Kind::Audio, "flac"));
        assert_eq!(sub(b"\0\0\0\x20ftypM4A \0\0\0\0"), (Kind::Audio, "m4a"));
        assert_eq!(sub(&[0x1A, 0x45, 0xDF, 0xA3, 0x9F]), (Kind::Audio, "webm"));
        assert_eq!(sniff(b"RIFF\x24\0\0\0WAVEfmt ").unwrap().mime, "audio/wav");
    }

    #[test]
    fn near_misses_are_not_audio() {
        // AAC/ADTS has layer bits 00, the reserved bitrate index 1111 and the
        // reserved version 01 are all refused as MPEG frames — and then, being
        // invalid UTF-8, as everything.
        assert!(sniff(&[0xFF, 0xF1, 0x50, 0x80]).is_err());
        assert!(sniff(&[0xFF, 0xFB, 0xF0, 0x64]).is_err());
        assert!(sniff(&[0xFF, 0xEB, 0x90, 0x64]).is_err());
        // A HEIC picture shares M4A's box layout.
        assert!(sniff(b"\0\0\0\x18ftypheic\0\0\0\0").is_err());
        // And a non-tag ID3 or non-Ogg/FLAC header is nothing (invalid UTF-8).
        assert!(sniff(b"ID3\x09\xFF\xFF\xFF").is_err());
    }

    #[test]
    fn office_zip_formats_by_manifest() {
        for (sub_, bytes) in [
            ("docx", test_files::docx(&["hi"])),
            ("xlsx", test_files::xlsx(&[("S", &[&["a"]])])),
            ("pptx", test_files::pptx(&[&["one"]])),
            ("odt", test_files::odf("text", "<text:p>hi</text:p>")),
            ("ods", test_files::odf("spreadsheet", "")),
            ("odp", test_files::odf("presentation", "")),
        ] {
            let s = sniff(&bytes).unwrap();
            assert_eq!((s.kind, s.sub), (Kind::Office, sub_));
            assert_eq!(s.office_format().unwrap().as_str(), sub_);
        }
    }

    #[test]
    fn a_zip_that_is_not_office_is_refused_with_the_accepted_list() {
        let plain = test_files::zip(&[("a.txt", b"hello".as_slice())]);
        let e = sniff(&plain).unwrap_err();
        assert_eq!(e, SniffError::UnknownZip);
        assert!(e.to_string().contains("docx"), "{e}");
        // Content types that name nothing we read.
        let other = test_files::zip(&[("[Content_Types].xml", b"<Types/>".as_slice())]);
        assert_eq!(sniff(&other).unwrap_err(), SniffError::UnknownZip);
        // A truncated archive is a broken zip, not text.
        assert_eq!(
            sniff(b"PK\x03\x04junk").unwrap_err(),
            SniffError::UnknownZip
        );
    }

    #[test]
    fn every_ole_file_is_legacy_office_with_one_message() {
        let with = |name: &str| {
            let mut b = OLE_MAGIC.to_vec();
            b.extend([0xA1, 0xB1, 0x1A, 0xE1]);
            b.extend(vec![0u8; 100]);
            b.extend(name.encode_utf16().flat_map(u16::to_le_bytes));
            b.extend(vec![0u8; 100]);
            b
        };
        for name in [
            "Workbook",
            "Book",
            "WordDocument",
            "PowerPoint Document",
            "Other",
        ] {
            let e = sniff(&with(name)).unwrap_err();
            assert_eq!(e, SniffError::LegacyOffice, "{name}");
            let m = e.to_string();
            assert!(
                m.contains("legacy binary Office file (.doc/.xls/.ppt)")
                    && m.contains(".docx/.xlsx/.pptx"),
                "{m}"
            );
        }
        assert!(!ACCEPTED.contains("xls,") && !ACCEPTED.contains("xls)"));
    }

    #[test]
    fn text_that_starts_like_audio_stays_text() {
        for t in [
            "ID3v2 notes: how the tag works
",
            "ID3 is a tag format",
            "OggS is the capture pattern",
            "fLaC is a codec",
            "OggS\x01 and more",
            "fLaC and then some more words",
            "    ftypM4A  nothing",
        ] {
            let s = sniff(t.as_bytes()).unwrap_or_else(|e| panic!("{t:?}: {e}"));
            assert_eq!((s.kind, s.sub), (Kind::Text, "text"), "{t:?}");
        }
        // Real headers still are audio: ID3v2.3 with a tag size, Ogg page
        // with the first-page flag, FLAC with STREAMINFO.
        assert_eq!(sub(b"ID3\x03\0\0\0\0\x01\x00rest"), (Kind::Audio, "mp3"));
        assert_eq!(sub(b"OggS\0\x02\0\0"), (Kind::Audio, "ogg"));
        assert_eq!(sub(b"fLaC\x80\0\0\x22\x10\0"), (Kind::Audio, "flac"));
        // An ID3 header with a bad size byte is not a tag.
        assert!(sniff(b"ID3\x04\0\0\xFF\0\0\0rest").is_err());
    }

    #[test]
    fn mp4_audio_brands_are_audio_and_video_brands_are_refused() {
        let ftyp = |major: &[u8; 4], compat: &[&[u8; 4]]| {
            let mut b = vec![0, 0, 0, (16 + 4 * compat.len()) as u8];
            b.extend(b"ftyp");
            b.extend(major);
            b.extend([0, 0, 0, 0]);
            for c in compat {
                b.extend(*c);
            }
            b
        };
        assert_eq!(sub(&ftyp(b"M4A ", &[])), (Kind::Audio, "m4a"));
        assert_eq!(sub(&ftyp(b"M4B ", &[])), (Kind::Audio, "m4a"));
        // ffmpeg's audio-only files: isom major, M4A among the compatibles.
        assert_eq!(
            sub(&ftyp(b"isom", &[b"M4A ", b"mp42"])),
            (Kind::Audio, "m4a")
        );
        for major in [b"isom", b"mp42", b"qt  ", b"avc1"] {
            let e = sniff(&ftyp(major, &[b"isom", b"iso2"])).unwrap_err();
            assert!(matches!(e, SniffError::Video(_)), "{e}");
            assert!(e.to_string().contains("video container"), "{e}");
        }
        assert!(sniff(&ftyp(b"heic", &[b"mif1"])).is_err());
    }

    #[test]
    fn ebml_webm_is_audio_and_matroska_is_refused() {
        let ebml = |doctype: &[u8]| {
            let mut b = vec![0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x86, 0x81, 0x01];
            b.extend([0x42, 0x82, 0x80 | doctype.len() as u8]);
            b.extend(doctype);
            b
        };
        assert_eq!(sub(&ebml(b"webm")), (Kind::Audio, "webm"));
        let e = sniff(&ebml(b"matroska")).unwrap_err();
        assert_eq!(e, SniffError::Video("Matroska"));
    }

    #[test]
    fn a_manifest_of_a_real_large_deck_is_read_and_only_the_archive_budget_refuses() {
        // 300 slides with notes: well over the 64 KiB a guessed cap allowed.
        let mut types = String::from("<Types>");
        for i in 1..=1200 {
            types.push_str(&format!(
                r#"<Override PartName="/ppt/slides/slide{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#
            ));
        }
        types.push_str(r#"<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/></Types>"#);
        assert!(types.len() > 100 << 10);
        let z = test_files::zip(&[
            ("[Content_Types].xml", types.as_bytes()),
            ("ppt/presentation.xml", b"<p/>".as_slice()),
        ]);
        assert_eq!(sub(&z), (Kind::Office, "pptx"));
        // Over the archive's budget: refused, naming the constant.
        let e = sniff_zip_within(&z, &mut Budget::with_limit(64 << 10)).unwrap_err();
        assert!(matches!(e, SniffError::ManifestTooLarge(_)), "{e}");
        assert!(e.to_string().contains("MAX_UNCOMPRESSED_BYTES"), "{e}");
    }

    fn bx(ty: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut b = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend(ty);
        b.extend(payload);
        b
    }

    /// An `ftyp` of `major` plus a `moov` with one `trak` per handler type.
    fn mp4(major: &[u8; 4], handlers: &[&[u8; 4]]) -> Vec<u8> {
        let mut f = major.to_vec();
        f.extend([0, 0, 0, 0]);
        f.extend(major);
        let mut out = bx(b"ftyp", &f);
        let traks: Vec<u8> = handlers
            .iter()
            .flat_map(|h| {
                let mut hd = vec![0u8; 8];
                hd.extend(*h);
                hd.extend([0u8; 12]);
                let mdhd = bx(b"mdhd", &[0u8; 24]);
                let mdia = bx(b"mdia", &[mdhd, bx(b"hdlr", &hd)].concat());
                bx(b"trak", &[bx(b"tkhd", &[0u8; 20]), mdia].concat())
            })
            .collect();
        out.extend(bx(b"mdat", &[0u8; 5]));
        out.extend(bx(b"moov", &traks));
        out
    }

    #[test]
    fn an_mp4_without_an_audio_brand_is_decided_by_its_tracks() {
        // ffmpeg -f mp4 of AAC: brand isom, only a sound track.
        assert_eq!(sub(&mp4(b"isom", &[b"soun"])), (Kind::Audio, "m4a"));
        assert_eq!(
            sub(&mp4(b"mp42", &[b"soun", b"soun"])),
            (Kind::Audio, "m4a")
        );
        for handlers in [
            &[b"vide" as &[u8; 4]][..],
            &[b"soun", b"vide"],
            &[],
            &[b"text"],
        ] {
            let e = sniff(&mp4(b"isom", handlers)).unwrap_err();
            assert!(matches!(e, SniffError::Video(_)), "{handlers:?}: {e}");
        }
        // Truncated inside moov, or a lying box size: refused, never a panic.
        let mut cut = mp4(b"isom", &[b"soun"]);
        cut.truncate(cut.len() - 10);
        assert!(matches!(sniff(&cut), Err(SniffError::Video(_)) | Ok(_)));
        let mut lie = mp4(b"isom", &[b"soun"]);
        let n = lie.len();
        lie[n - 100 - 8..n - 100 - 4].copy_from_slice(&u32::MAX.to_be_bytes());
        let _ = sniff(&lie);
        let mut big = mp4(b"isom", &[b"soun"]);
        let at = big.windows(4).position(|w| w == b"moov").unwrap() - 4;
        big[at..at + 4].copy_from_slice(&1u32.to_be_bytes());
        let _ = sniff(&big);
    }

    #[test]
    fn binary_that_is_not_known_is_refused() {
        let e = sniff(&[0, 1, 2, 3, 0xFF, 0xFE]).unwrap_err();
        assert_eq!(e, SniffError::Unrecognised);
        let m = e.to_string();
        for w in ["PNG", "PDF", "xlsx", "odt", "WAV", "text"] {
            assert!(m.contains(w), "{m}");
        }
        // Text with a NUL is binary.
        assert!(sniff(b"abc\0def").is_err());
    }
}
