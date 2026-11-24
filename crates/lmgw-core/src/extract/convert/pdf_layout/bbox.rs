//! `pdftotext -bbox-layout` read into [`Page`]s (poppler 26.08 checked on
//! 2026-11-18): an XHTML document of
//! `<page width height>` → `<flow>` → `<block xMin yMin xMax yMax>` →
//! `<line …>` → `<word …>text</word>`, boxes in points from the page's
//! top-left corner, text XML-escaped. Control characters from the PDF come
//! through raw (seen in the WP0a corpus); they are kept as the text has them.
//! Right-to-left words come in visual order and are put back in reading
//! order ([`super::bidi`]).
//!
//! A word outside a line, or a line outside a block, gets an implicit
//! parent; an element poppler does not write is skipped. Nothing recurses,
//! so nesting depth costs nothing.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use super::super::doc::Rect;
use super::{finite, Line, Page, Role};

fn rect(e: &BytesStart<'_>) -> Rect {
    let mut r = Rect::default();
    for a in e.attributes().flatten() {
        let v = finite(&String::from_utf8_lossy(&a.value));
        match a.key.as_ref() {
            b"xMin" => r.x0 = v,
            b"yMin" => r.y0 = v,
            b"xMax" => r.x1 = v,
            b"yMax" => r.y1 = v,
            _ => {}
        }
    }
    // A box given backwards is read the right way round.
    if r.x1 < r.x0 {
        std::mem::swap(&mut r.x0, &mut r.x1);
    }
    if r.y1 < r.y0 {
        std::mem::swap(&mut r.y0, &mut r.y1);
    }
    r
}

fn size(e: &BytesStart<'_>) -> (f32, f32) {
    let (mut w, mut h) = (0.0, 0.0);
    for a in e.attributes().flatten() {
        let v = finite(&String::from_utf8_lossy(&a.value)).max(0.0);
        match a.key.as_ref() {
            b"width" => w = v,
            b"height" => h = v,
            _ => {}
        }
    }
    (w, h)
}

/// The line being read.
#[derive(Default)]
struct Open {
    rect: Option<Rect>,
    words: Vec<String>,
}

/// Where the reader is.
struct State {
    pages: Vec<Page>,
    block: u32,
    in_block: bool,
    line: Option<Open>,
    word: Option<String>,
}

impl State {
    fn page(&mut self) -> &mut Page {
        if self.pages.is_empty() {
            self.pages.push(Page::default());
        }
        let last = self.pages.len() - 1;
        &mut self.pages[last]
    }

    fn open_block(&mut self) {
        if self.in_block {
            return;
        }
        self.in_block = true;
        self.block = self.block.saturating_add(1);
    }

    fn close_line(&mut self) {
        let Some(open) = self.line.take() else {
            return;
        };
        let words: Vec<String> = open
            .words
            .iter()
            .map(|w| w.trim())
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect();
        // Right-to-left words come in visual order (`bidi`).
        let text = super::bidi::logical(words).join(" ");
        if text.is_empty() {
            return;
        }
        let block = self.block;
        let rect = open.rect.unwrap_or_default();
        self.page().lines.push(Line {
            rect,
            text,
            size: 0.0,
            bold: false,
            block,
            role: Role::Body,
        });
    }

    fn close_word(&mut self) {
        if let Some(w) = self.word.take() {
            if self.line.is_none() {
                self.open_block();
                self.line = Some(Open::default());
            }
            if let Some(l) = self.line.as_mut() {
                l.words.push(w);
            }
        }
    }
}

/// The pages of `xml`; the error names where poppler's output stopped being
/// XML.
pub(super) fn pages(xml: &[u8]) -> Result<Vec<Page>, String> {
    let mut reader = Reader::from_reader(xml);
    let mut s = State {
        pages: Vec::new(),
        block: 0,
        in_block: false,
        line: None,
        word: None,
    };
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(|e| {
            format!(
                "poppler's layout output is not readable XML at byte {}: {e}",
                reader.buffer_position()
            )
        })?;
        match ev {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                b"page" => {
                    s.close_word();
                    s.close_line();
                    s.in_block = false;
                    let (width, height) = size(&e);
                    s.pages.push(Page {
                        width,
                        height,
                        lines: Vec::new(),
                    });
                }
                b"block" => {
                    s.close_word();
                    s.close_line();
                    s.in_block = false;
                    s.open_block();
                }
                b"line" => {
                    s.close_word();
                    s.close_line();
                    s.open_block();
                    s.line = Some(Open {
                        rect: Some(rect(&e)),
                        words: Vec::new(),
                    });
                }
                b"word" => {
                    s.close_word();
                    if s.line.is_none() {
                        s.open_block();
                        s.line = Some(Open {
                            rect: Some(rect(&e)),
                            words: Vec::new(),
                        });
                    }
                    s.word = Some(String::new());
                }
                _ => {}
            },
            Event::End(e) => match e.local_name().as_ref() {
                b"word" => s.close_word(),
                b"line" => {
                    s.close_word();
                    s.close_line();
                }
                b"block" => {
                    s.close_word();
                    s.close_line();
                    s.in_block = false;
                }
                _ => {}
            },
            Event::Text(t) => {
                if let Some(w) = s.word.as_mut() {
                    w.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
            }
            Event::GeneralRef(r) => {
                if let Some(w) = s.word.as_mut() {
                    let name: &[u8] = &r;
                    let c = match name {
                        b"amp" => Some('&'),
                        b"lt" => Some('<'),
                        b"gt" => Some('>'),
                        b"apos" => Some('\''),
                        b"quot" => Some('"'),
                        _ => r.resolve_char_ref().ok().flatten(),
                    };
                    if let Some(c) = c {
                        w.push(c);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    s.close_word();
    s.close_line();
    Ok(s.pages)
}
