//! What the reader is asked ([`Op`], its command line) and what it answers
//! (one frame on stdout): the gateway builds the command line and reads the
//! frame, the reader parses the command line and writes the frame, both from
//! this module, so the two cannot drift within one build.
//!
//! A frame is
//!
//! ```text
//! lmgw-doc-reader/<WIRE_VERSION>\n     the magic line
//! <u64, little endian>                 the header's length in bytes
//! <header>                             an [`Answer`] as JSON
//! <blobs>                              raw bytes back to back, each length named in the header
//! ```
//!
//! The blobs are what would be wasteful as JSON: an e-mail's attachments (in
//! the order of [`Mail::attachments`]), a page's grey pixels ([`Pixels`])
//! and a page's pictures cut out as PNGs ([`Answer::Crops`]). A failure the reader can name travels in the frame
//! ([`Answer::Failed`]) with exit status 0; anything else (no frame, another
//! status, a signal) is the reader stopping, which the gateway words itself.
//! Nothing the reader writes to stderr is shown or logged by the gateway: it
//! may quote the document.
//!
//! The gateway reads at most the output bound it gave the run, so the header
//! and the blobs are bounded before they are parsed, and [`read`] checks
//! every length against the bytes that are there.

use serde::{Deserialize, Serialize};

/// The frame's version: raised whenever [`Op`]'s command line or [`Answer`]
/// changes shape. A reader of another version is refused by name (an lmgw
/// updated under a running gateway), never half understood.
pub const WIRE_VERSION: u32 = 2;

/// What every frame of every version starts with.
const MAGIC_PREFIX: &str = "lmgw-doc-reader/";

/// How deep a page's elements may nest for the markdown writer: 256. htmd
/// writes the tree recursively; measured in a debug build (2026-11-19, htmd
/// 0.6.0), 500 nested `div`s fit a 2 MiB stack and 750 overflowed it, and the
/// reader converts on a stack of [`READER_STACK_BYTES`], so 256 is far inside
/// it for every handler chain (tables in cells, lists, block quotes: the
/// reader's own tests run each at the bound). Real pages nest 20 to 40 deep.
/// The depth is counted on the parsed tree, without recursion, never
/// estimated from the text.
pub const MAX_HTML_DEPTH: usize = 256;

/// The stack the reader converts on: 64 MiB of address space, committed only
/// as far as it is used.
pub const READER_STACK_BYTES: usize = 64 << 20;

/// What a page's pictures become (docling's placeholder).
pub const PICTURE: &str = "<!-- image -->";

/// The flag the reader takes its address-space limit after, before the
/// request: `lmgw-doc-reader --memory-limit <bytes> html`. The reader sets
/// the limit on itself (`RLIMIT_AS`) and closes every descriptor beyond
/// stdio before it reads a byte of the file, and refuses to go on if it
/// cannot. The gateway always gives it (`run.rs`, a tool that limits
/// itself); a person running the reader by hand may leave it out.
pub const MEMORY_LIMIT_FLAG: &str = "--memory-limit";

/// The most pictures one [`Op::Crop`] cuts: the gateway's
/// `PICTURES_PER_PAGE_MAX` (256), which bounds a page's crops before this
/// request is made; a command line with more is no request.
pub const MAX_CROPS: usize = 256;

/// One request: the reader's command line. The file is on stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// An HTML page as markdown ([`Answer::Page`]).
    Html,
    /// An EPUB's chapters ([`Answer::Book`]); `budget` bounds the bytes its
    /// ZIP entries may unpack to.
    Epub { budget: u64 },
    /// An e-mail message ([`Answer::Mail`]), its attachments as blobs.
    Email,
    /// A legacy Excel workbook's cells ([`Answer::Sheets`]).
    Xls,
    /// An image file's pages by their headers, no pixel decoded
    /// ([`Answer::Images`]): one for a PNG, JPEG, GIF or WebP, one per
    /// directory for a TIFF.
    ImageInfo,
    /// One page as grey pixels ([`Answer::Pixels`]): the image itself, or
    /// the TIFF page whose directory is at `ifd` (as [`Op::ImageInfo`] gave
    /// it). A page of more than `max_pixels` is refused from its header.
    ImagePage { max_pixels: u64, ifd: Option<u64> },
    /// A PDF page's pictures cut out of poppler's PNG render of the box
    /// that holds them all ([`Answer::Crops`], each crop a PNG blob): the
    /// render is at most `width × height` pixels (the box asked of poppler;
    /// a PNG that declares more is refused before it is decoded), each
    /// `[x, y, w, h]` of `boxes` is clipped to it, and crops past `budget`
    /// bytes of PNG in all are left out (`None`), at most [`MAX_CROPS`].
    Crop {
        width: u32,
        height: u32,
        budget: u64,
        boxes: Vec<[u32; 4]>,
    },
}

impl Op {
    /// The command line after the program's name.
    pub fn args(&self) -> Vec<String> {
        match self {
            Self::Html => vec!["html".into()],
            Self::Epub { budget } => vec!["epub".into(), budget.to_string()],
            Self::Email => vec!["email".into()],
            Self::Xls => vec!["xls".into()],
            Self::ImageInfo => vec!["image-info".into()],
            Self::ImagePage { max_pixels, ifd } => {
                let mut a = vec!["image-page".to_string(), max_pixels.to_string()];
                a.extend(ifd.map(|o| o.to_string()));
                a
            }
            Self::Crop {
                width,
                height,
                budget,
                boxes,
            } => {
                let mut a = vec![
                    "crop".to_string(),
                    width.to_string(),
                    height.to_string(),
                    budget.to_string(),
                ];
                a.extend(boxes.iter().map(|[x, y, w, h]| format!("{x},{y},{w},{h}")));
                a
            }
        }
    }

    /// The memory limit `args` start with ([`MEMORY_LIMIT_FLAG`]), if any,
    /// and the request after it.
    pub fn parse_limited(args: &[String]) -> Option<(Option<u64>, Self)> {
        match args {
            [flag, bytes, rest @ ..] if flag == MEMORY_LIMIT_FLAG => {
                Some((Some(bytes.parse().ok()?), Self::parse(rest)?))
            }
            _ => Some((None, Self::parse(args)?)),
        }
    }

    /// The request `args` (the command line after the program's name) is.
    pub fn parse(args: &[String]) -> Option<Self> {
        let num = |i: usize| args.get(i).and_then(|s| s.parse::<u64>().ok());
        if args.first()? == "crop" {
            return Self::parse_crop(args);
        }
        let op = match (args.first()?.as_str(), args.len()) {
            ("html", 1) => Self::Html,
            ("epub", 2) => Self::Epub { budget: num(1)? },
            ("email", 1) => Self::Email,
            ("xls", 1) => Self::Xls,
            ("image-info", 1) => Self::ImageInfo,
            ("image-page", 2) => Self::ImagePage {
                max_pixels: num(1)?,
                ifd: None,
            },
            ("image-page", 3) => Self::ImagePage {
                max_pixels: num(1)?,
                ifd: Some(num(2)?),
            },
            _ => return None,
        };
        Some(op)
    }

    fn parse_crop(args: &[String]) -> Option<Self> {
        let [_, width, height, budget, boxes @ ..] = args else {
            return None;
        };
        if boxes.len() > MAX_CROPS {
            return None;
        }
        let boxes = boxes
            .iter()
            .map(|b| {
                let mut it = b.split(',').map(|n| n.parse::<u32>().ok());
                let r = [it.next()??, it.next()??, it.next()??, it.next()??];
                it.next().is_none().then_some(r)
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self::Crop {
            width: width.parse().ok()?,
            height: height.parse().ok()?,
            budget: budget.parse().ok()?,
            boxes,
        })
    }
}

/// One picture cut out of a page ([`Answer::Crops`]): its size in pixels
/// and its PNG's length, the blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crop {
    pub width: u32,
    pub height: u32,
    pub len: u64,
}

/// An HTML page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    /// Its markdown; empty when [`Page::too_deep`] is set.
    pub markdown: String,
    /// Its elements nest this deep, past [`MAX_HTML_DEPTH`]: it was not
    /// written as markdown.
    pub too_deep: Option<u64>,
}

/// An EPUB.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Book {
    /// Its chapters' markdown, in spine order.
    pub chapters: Vec<String>,
    /// What could not be read, in words: one note per kind of gap, with how
    /// many chapters it is about.
    pub notes: Vec<String>,
}

/// One attachment of a message; its bytes are a blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Its file name, or what it is when it has none.
    pub name: String,
    /// Its declared content type (`application/pdf`).
    pub content_type: String,
    /// Its decoded length in bytes.
    pub len: u64,
}

/// An e-mail message.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Mail {
    /// The subject, if it has one.
    pub subject: Option<String>,
    /// `From: …`, `To: …`, `Cc: …`, `Date: …`, each a line.
    pub fields: Vec<String>,
    /// The body as markdown (an HTML body converted), or the text body.
    pub body: String,
    /// The body is plain text (not markdown).
    pub plain: bool,
    pub attachments: Vec<Attachment>,
    /// What could not be read, in words.
    pub notes: Vec<String>,
}

/// One sheet of a workbook: its cells that hold something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sheet {
    pub name: String,
    /// `(row, column, text)`, both 0-based.
    pub cells: Vec<(u32, u32, String)>,
}

/// One page of an image file, from its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePage {
    pub width: u32,
    pub height: u32,
    /// Where a TIFF page's directory is in the file.
    pub ifd: Option<u64>,
}

/// One page as grey pixels: `width × height` bytes, row by row, the blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    /// The page's own resolution, when the file says.
    pub dpi: Option<u32>,
}

/// A failure the reader names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Failure {
    /// The file is not what it should be: why, in words (they may quote the
    /// file).
    Unreadable(String),
    /// Reading `what` passed the budget the request gave.
    OverBudget { what: String },
    /// The page has more pixels than the request allows.
    TooManyPixels { width: u64, height: u64 },
}

/// The reader's answer: the frame's header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answer {
    Page(Page),
    Book(Book),
    Mail(Mail),
    Sheets(Vec<Sheet>),
    Images(Vec<ImagePage>),
    Pixels(Pixels),
    /// [`Op::Crop`]'s pictures in the order of its boxes; `None` past its
    /// budget.
    Crops(Vec<Option<Crop>>),
    Failed(Failure),
}

/// The magic line of this build's frames.
pub fn magic() -> String {
    format!("{MAGIC_PREFIX}{WIRE_VERSION}\n")
}

/// Write one frame: `answer`, then `blobs` in order.
pub fn write<W: std::io::Write>(
    out: &mut W,
    answer: &Answer,
    blobs: &[&[u8]],
) -> std::io::Result<()> {
    let header = serde_json::to_vec(answer).map_err(std::io::Error::other)?;
    out.write_all(magic().as_bytes())?;
    out.write_all(&(header.len() as u64).to_le_bytes())?;
    out.write_all(&header)?;
    for b in blobs {
        out.write_all(b)?;
    }
    out.flush()
}

/// Why a frame could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// A frame of another [`WIRE_VERSION`]: the reader is another build's.
    OtherVersion(String),
    /// Not a frame: what is wrong with it (never its content).
    Malformed(&'static str),
}

/// A frame's answer and where its blobs start in `bytes`.
pub fn read(bytes: &[u8]) -> Result<(Answer, usize), FrameError> {
    let magic = magic();
    if !bytes.starts_with(magic.as_bytes()) {
        if let Some(rest) = bytes.strip_prefix(MAGIC_PREFIX.as_bytes()) {
            let version: String = rest
                .iter()
                .take(10)
                .take_while(|b| b.is_ascii_digit())
                .map(|b| *b as char)
                .collect();
            return Err(FrameError::OtherVersion(version));
        }
        return Err(FrameError::Malformed(
            "it does not start with the reader's magic line",
        ));
    }
    let at = magic.len();
    let len: [u8; 8] = bytes
        .get(at..at + 8)
        .and_then(|b| b.try_into().ok())
        .ok_or(FrameError::Malformed("it ends before the header's length"))?;
    let len = usize::try_from(u64::from_le_bytes(len))
        .map_err(|_| FrameError::Malformed("its header's length is not a size"))?;
    let start = at + 8;
    let end = start
        .checked_add(len)
        .filter(|e| *e <= bytes.len())
        .ok_or(FrameError::Malformed("it ends before its header does"))?;
    let answer = serde_json::from_slice(&bytes[start..end])
        .map_err(|_| FrameError::Malformed("its header is not an answer"))?;
    Ok((answer, end))
}

/// The blobs of `lens` bytes each, from `bytes[at..]`: `None` when the frame
/// holds fewer bytes than its header names.
pub fn blobs(bytes: &[u8], mut at: usize, lens: &[u64]) -> Option<Vec<std::ops::Range<usize>>> {
    let mut out = Vec::with_capacity(lens.len());
    for len in lens {
        let end = at.checked_add(usize::try_from(*len).ok()?)?;
        if end > bytes.len() {
            return None;
        }
        out.push(at..end);
        at = end;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_request_round_trips_through_its_command_line() {
        for op in [
            Op::Html,
            Op::Epub { budget: 1 << 30 },
            Op::Email,
            Op::Xls,
            Op::ImageInfo,
            Op::ImagePage {
                max_pixels: 100_000_000,
                ifd: None,
            },
            Op::ImagePage {
                max_pixels: 7,
                ifd: Some(4096),
            },
            Op::Crop {
                width: 20,
                height: 10,
                budget: 1 << 30,
                boxes: vec![[0, 0, 5, 5], [15, 8, 50, 50]],
            },
            Op::Crop {
                width: 1,
                height: 1,
                budget: 0,
                boxes: Vec::new(),
            },
        ] {
            assert_eq!(Op::parse(&op.args()), Some(op.clone()), "{op:?}");
        }
        let many = Op::Crop {
            width: 1,
            height: 1,
            budget: 1,
            boxes: vec![[0, 0, 1, 1]; MAX_CROPS + 1],
        };
        assert_eq!(Op::parse(&many.args()), None);
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let limited = s(&["--memory-limit", "4096", "email"]);
        assert_eq!(Op::parse_limited(&limited), Some((Some(4096), Op::Email)));
        assert_eq!(Op::parse_limited(&s(&["xls"])), Some((None, Op::Xls)));
        assert_eq!(
            Op::parse_limited(&s(&["--memory-limit", "lots", "xls"])),
            None
        );
        assert_eq!(Op::parse_limited(&s(&["--memory-limit", "4096"])), None);
        for bad in [
            s(&[]),
            s(&["pdf"]),
            s(&["html", "x"]),
            s(&["epub"]),
            s(&["epub", "-1"]),
            s(&["image-page", "x"]),
            s(&["crop", "1", "1"]),
            s(&["crop", "1", "1", "1", "1,2,3"]),
            s(&["crop", "1", "1", "1", "1,2,3,4,5"]),
            s(&["crop", "1", "1", "1", "1,2,-3,4"]),
        ] {
            assert_eq!(Op::parse(&bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_frame_round_trips_with_its_blobs_and_a_cut_one_is_refused() {
        let mail = Answer::Mail(Mail {
            subject: Some("Example".into()),
            attachments: vec![
                Attachment {
                    name: "a.txt".into(),
                    content_type: "text/plain".into(),
                    len: 3,
                },
                Attachment {
                    name: "b.bin".into(),
                    content_type: "application/octet-stream".into(),
                    len: 2,
                },
            ],
            ..Default::default()
        });
        let mut frame = Vec::new();
        write(&mut frame, &mail, &[b"abc", b"\0\xff"]).unwrap();
        let (answer, at) = read(&frame).unwrap();
        assert_eq!(answer, mail);
        let ranges = blobs(&frame, at, &[3, 2]).unwrap();
        assert_eq!(&frame[ranges[0].clone()], b"abc");
        assert_eq!(&frame[ranges[1].clone()], b"\0\xff");
        // A header that names more than is there.
        assert_eq!(blobs(&frame, at, &[3, 3]), None);
        assert_eq!(blobs(&frame, at, &[u64::MAX, 1]), None);
        for cut in [0, 5, magic().len() + 3, magic().len() + 12] {
            assert!(
                matches!(read(&frame[..cut]), Err(FrameError::Malformed(_))),
                "{cut}"
            );
        }
        let mut other = frame.clone();
        other[MAGIC_PREFIX.len()] = b'9';
        assert_eq!(read(&other), Err(FrameError::OtherVersion("9".into())));
        // A length that claims the whole address space.
        let mut huge = magic().into_bytes();
        huge.extend_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(read(&huge), Err(FrameError::Malformed(_))));
    }
}
