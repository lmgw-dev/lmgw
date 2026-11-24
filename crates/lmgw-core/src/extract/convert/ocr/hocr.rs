//! tesseract's hOCR (5.5.4, checked 2026-11-18) read into lines of words:
//! `div.ocr_page` (`bbox 0 0 W H; …; scan_res X Y`) → `div.ocr_carea` →
//! `p.ocr_par` → `span.ocr_line` (or `ocr_header`, `ocr_caption`,
//! `ocr_textfloat`; `bbox …; baseline …; x_size S; …`) → `span.ocrx_word`
//! (`bbox x0 y0 x1 y1; x_wconf N`).
//!
//! The text was recognised in an untrusted image, so the read is bounded:
//! the input's size by the run's output budget (`HOCR_OUTPUT_BYTES`), its
//! nesting by [`MAX_HOCR_DEPTH`] (deeper elements are counted, not kept,
//! and nothing recurses); every number is checked to be finite, a box is
//! put the right way round and clipped to the page, an element out of place
//! is attached to an implicit parent or skipped. Malformed XML ends the
//! read with what was read before it.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use super::super::doc::Rect;

/// How deep elements are followed: 32. tesseract writes 7 levels (html,
/// body, page, area, paragraph, line, word, with `strong`/`em` inside a
/// word when font facts are on); deeper elements' text still belongs to
/// the word they are in.
pub const MAX_HOCR_DEPTH: usize = 32;

/// One recognised word, in the image's pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub rect: Rect,
    pub text: String,
    /// `x_wconf`, 0 to 100.
    pub confidence: Option<f32>,
}

/// One line of words.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub rect: Rect,
    /// The paragraph it is in, counted on the page.
    pub block: u32,
    /// The size of the line's type in pixels, as tesseract estimates it
    /// (`x_size`): the same for a line of small letters and one with
    /// capitals and descenders, which the box's height is not.
    pub x_size: Option<f32>,
    pub words: Vec<Word>,
}

/// A page whose lines read as vertical is taken to be on its side when it
/// has at least this many lines of two or more words: 3. One or two
/// vertical lines are a margin note or a table's turned header.
pub const SIDEWAYS_MIN_LINES: usize = 3;

/// One page.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Page {
    /// The image's size in pixels.
    pub width: f32,
    pub height: f32,
    /// The resolution tesseract read it at (`scan_res`), if it said.
    pub dpi: Option<f32>,
    pub lines: Vec<Line>,
}

impl Page {
    /// The words' mean confidence; `None` without words.
    pub fn confidence(&self) -> Option<f32> {
        let (sum, n) = self
            .lines
            .iter()
            .flat_map(|l| &l.words)
            .filter_map(|w| w.confidence)
            .fold((0.0f64, 0u64), |(s, n), c| (s + f64::from(c), n + 1));
        (n > 0).then(|| (sum / n as f64) as f32)
    }

    /// Whether the page looks scanned on its side: most of its lines of
    /// two or more words are taller than wide (at least
    /// [`SIDEWAYS_MIN_LINES`] such lines). tesseract reads the lines of a
    /// page lying on its side as vertical text (measured 2026-11-19,
    /// tesseract 5.5.4, an A4 text page at 150 DPI: 25 of 25 lines taller
    /// than wide; the same page upright: 0 of 25), so their shape tells.
    ///
    /// Which side it lies on shows in the words' confidence
    /// ([`Page::confidence`]): tesseract reads vertical lines top to bottom,
    /// which is the right way for a page lying on its right side (turned 90°
    /// clockwise: mean confidence 96 on that page) and upside down for one
    /// lying on its left side (letters of noise, low confidence).
    pub fn sideways(&self) -> bool {
        let (mut lines, mut tall) = (0usize, 0usize);
        for l in self.lines.iter().filter(|l| l.words.len() >= 2) {
            lines += 1;
            if l.rect.height() > l.rect.width() {
                tall += 1;
            }
        }
        lines >= SIDEWAYS_MIN_LINES && tall * 2 > lines
    }
}

/// What an element opened.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Opened {
    Page,
    Paragraph,
    Line,
    Word,
    Other,
}

/// The `title` property `name`'s numbers (`bbox 1 2 3 4` → `[1, 2, 3, 4]`),
/// each finite.
fn prop(title: &str, name: &str) -> Option<Vec<f32>> {
    title.split(';').find_map(|p| {
        let mut it = p.split_whitespace();
        (it.next() == Some(name)).then(|| {
            it.take(4)
                .map(|v| {
                    v.parse::<f32>()
                        .ok()
                        .filter(|f| f.is_finite())
                        .unwrap_or(0.0)
                })
                .collect()
        })
    })
}

fn bbox(title: &str) -> Option<Rect> {
    let v = prop(title, "bbox")?;
    let [x0, y0, x1, y1] = <[f32; 4]>::try_from(v).ok()?;
    let (x0, x1) = (x0.min(x1), x0.max(x1));
    let (y0, y1) = (y0.min(y1), y0.max(y1));
    Some(Rect { x0, y0, x1, y1 })
}

/// `class` and `title` of an element.
fn attrs(e: &BytesStart<'_>) -> (String, String) {
    let (mut class, mut title) = (String::new(), String::new());
    for a in e.attributes().flatten() {
        let v = String::from_utf8_lossy(&a.value).into_owned();
        match a.key.as_ref() {
            b"class" => class = v,
            b"title" => title = v,
            _ => {}
        }
    }
    (class, title)
}

struct State {
    page: Page,
    stack: Vec<Opened>,
    /// Elements past [`MAX_HOCR_DEPTH`] that are open.
    deeper: usize,
    block: u32,
    line: Option<Line>,
    word: Option<Word>,
}

impl State {
    fn clip(&self, r: Rect) -> Rect {
        let (w, h) = (self.page.width, self.page.height);
        if w <= 0.0 || h <= 0.0 {
            return r;
        }
        Rect {
            x0: r.x0.clamp(0.0, w),
            y0: r.y0.clamp(0.0, h),
            x1: r.x1.clamp(0.0, w),
            y1: r.y1.clamp(0.0, h),
        }
    }

    fn open(&mut self, e: &BytesStart<'_>) -> Opened {
        let (class, title) = attrs(e);
        let classes: Vec<&str> = class.split_whitespace().collect();
        let has = |c: &str| classes.contains(&c);
        if has("ocr_page") {
            if let Some(r) = bbox(&title) {
                self.page.width = r.x1.max(0.0);
                self.page.height = r.y1.max(0.0);
            }
            self.page.dpi = prop(&title, "scan_res")
                .and_then(|v| v.first().copied())
                .filter(|d| *d > 0.0);
            return Opened::Page;
        }
        if has("ocr_par") || has("ocr_carea") {
            self.close_line();
            self.block = self.block.saturating_add(1);
            return Opened::Paragraph;
        }
        if ["ocr_line", "ocr_header", "ocr_caption", "ocr_textfloat"]
            .iter()
            .any(|c| has(c))
        {
            self.close_line();
            let rect = bbox(&title).map(|r| self.clip(r)).unwrap_or_default();
            let x_size = prop(&title, "x_size")
                .and_then(|v| v.first().copied())
                .filter(|s| *s > 0.0);
            self.line = Some(Line {
                rect,
                block: self.block,
                x_size,
                words: Vec::new(),
            });
            return Opened::Line;
        }
        if has("ocrx_word") {
            self.close_word();
            let rect = bbox(&title).map(|r| self.clip(r)).unwrap_or_default();
            let confidence = prop(&title, "x_wconf")
                .and_then(|v| v.first().copied())
                .map(|c| c.clamp(0.0, 100.0));
            self.word = Some(Word {
                rect,
                text: String::new(),
                confidence,
            });
            return Opened::Word;
        }
        Opened::Other
    }

    fn close_word(&mut self) {
        let Some(mut w) = self.word.take() else {
            return;
        };
        let t = w.text.trim();
        if t.is_empty() {
            return;
        }
        w.text = t.to_string();
        // A word outside a line gets an implicit one.
        let block = self.block;
        let line = self.line.get_or_insert_with(|| Line {
            rect: w.rect,
            block,
            x_size: None,
            words: Vec::new(),
        });
        line.words.push(w);
    }

    fn close_line(&mut self) {
        self.close_word();
        if let Some(mut l) = self.line.take() {
            if l.words.is_empty() {
                return;
            }
            if l.rect.width() <= 0.0 || l.rect.height() <= 0.0 {
                l.rect = Rect::union_all(l.words.iter().map(|w| &w.rect)).unwrap_or_default();
            }
            self.page.lines.push(l);
        }
    }

    fn close(&mut self) {
        if self.deeper > 0 {
            self.deeper -= 1;
            return;
        }
        match self.stack.pop() {
            Some(Opened::Word) => self.close_word(),
            Some(Opened::Line | Opened::Paragraph | Opened::Page) => self.close_line(),
            _ => {}
        }
    }

    fn start(&mut self, e: &BytesStart<'_>, empty: bool) {
        if self.stack.len() >= MAX_HOCR_DEPTH {
            if !empty {
                self.deeper += 1;
            }
            return;
        }
        let o = self.open(e);
        if empty {
            self.stack.push(o);
            self.close();
        } else {
            self.stack.push(o);
        }
    }
}

/// The page of `hocr`. Never fails: what could be read is returned.
pub fn parse(hocr: &[u8]) -> Page {
    let mut r = Reader::from_reader(hocr);
    let cfg = r.config_mut();
    cfg.check_end_names = false;
    cfg.trim_text(false);
    let mut s = State {
        page: Page::default(),
        stack: Vec::new(),
        deeper: 0,
        block: 0,
        line: None,
        word: None,
    };
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => s.start(&e, false),
            Ok(Event::Empty(e)) => s.start(&e, true),
            Ok(Event::End(_)) => s.close(),
            Ok(Event::Text(t)) => {
                if let Some(w) = s.word.as_mut() {
                    w.text.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
            }
            Ok(Event::GeneralRef(g)) => {
                if let Some(w) = s.word.as_mut() {
                    let name: &[u8] = &g;
                    let c = match name {
                        b"amp" => Some('&'),
                        b"lt" => Some('<'),
                        b"gt" => Some('>'),
                        b"apos" => Some('\''),
                        b"quot" => Some('"'),
                        _ => g.resolve_char_ref().ok().flatten(),
                    };
                    if let Some(c) = c {
                        w.text.push(c);
                    }
                }
            }
            Ok(Event::CData(t)) => {
                if let Some(w) = s.word.as_mut() {
                    w.text.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            Ok(_) => {}
        }
        buf.clear();
    }
    s.close_line();
    s.page
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body>
  <div class='ocr_page' id='page_1' title='image "stdin"; bbox 0 0 2481 3509; ppageno 0; scan_res 300 300'>
   <div class='ocr_carea' id='block_1_1' title="bbox 302 325 2098 627">
    <p class='ocr_par' id='par_1_1' lang='eng' title="bbox 302 325 2098 627">
     <span class='ocr_line' id='line_1_1' title="bbox 308 325 2098 403; baseline 0.001 -17; x_size 79">
      <span class='ocrx_word' id='word_1_1' title='bbox 308 325 601 403; x_wconf 92'>Docling</span>
      <span class='ocrx_word' id='word_1_2' title='bbox 628 326 936 387; x_wconf 96'>&amp;bundles</span>
     </span>
     <span class='ocr_line' id='line_1_2' title="bbox 302 437 2097 514">
      <span class='ocrx_word' id='word_1_7' title='bbox 302 437 507 499; x_wconf 90'>JSON</span>
     </span>
    </p>
   </div>
  </div>
 </body>
</html>"#;

    #[test]
    fn tesseracts_hocr_reads_into_lines_of_words() {
        let p = parse(PAGE.as_bytes());
        assert_eq!((p.width, p.height, p.dpi), (2481.0, 3509.0, Some(300.0)));
        assert_eq!(p.lines.len(), 2);
        let words: Vec<&str> = p.lines[0].words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(words, ["Docling", "&bundles"]);
        assert_eq!(p.lines[0].block, p.lines[1].block);
        assert_eq!(p.lines[0].words[0].rect.x1, 601.0);
        assert!((p.confidence().unwrap() - 92.666).abs() < 0.01);
        assert_eq!((p.lines[0].x_size, p.lines[1].x_size), (Some(79.0), None));
    }

    /// A page read on its side: its lines are taller than wide. Three such
    /// lines of several words say so; one line, or one-word lines, do not.
    #[test]
    fn lines_taller_than_wide_say_the_page_is_on_its_side() {
        let page = |lines: &[(&str, usize)]| {
            let mut s = String::from("<div class='ocr_page' title='bbox 0 0 1000 1000'>");
            for (bbox, words) in lines {
                s.push_str(&format!("<span class='ocr_line' title='bbox {bbox}'>"));
                for _ in 0..*words {
                    s.push_str("<span class='ocrx_word' title='bbox 0 0 5 5'>w</span>");
                }
                s.push_str("</span>");
            }
            parse(s.as_bytes())
        };
        let tall = "100 100 130 900";
        let wide = "100 100 900 130";
        assert!(page(&[(tall, 4), (tall, 3), (tall, 2)]).sideways());
        assert!(page(&[(tall, 4), (tall, 3), (wide, 5), (tall, 2)]).sideways());
        assert!(!page(&[(wide, 4), (wide, 3), (wide, 2), (tall, 2)]).sideways());
        assert!(!page(&[(tall, 4), (tall, 3)]).sideways());
        assert!(!page(&[(tall, 1), (tall, 1), (tall, 1), (tall, 1)]).sideways());
        assert!(!page(&[]).sideways());
    }

    #[test]
    fn hostile_hocr_is_bounded_and_never_panics() {
        // NaN, negative, backwards and past-the-page boxes; a word outside
        // any line; deep nesting; a broken end.
        let mut s = String::from(
            "<div class='ocr_page' title='bbox 0 0 100 100'>\
             <span class='ocrx_word' title='bbox NaN -5 50 inf; x_wconf -3'>lost</span>\
             <span class='ocr_line' title='bbox 90 90 10 10'>\
             <span class='ocrx_word' title='bbox 500 500 900 900; x_wconf 400'>far</span>",
        );
        for _ in 0..10_000 {
            s.push_str("<b>");
        }
        s.push_str("<span class='ocrx_word' title='bbox 1 1 2 2'>deep</span>");
        for _ in 0..10_000 {
            s.push_str("</b>");
        }
        s.push_str("</span></i></div><unclosed");
        let p = parse(s.as_bytes());
        let all: Vec<&Word> = p.lines.iter().flat_map(|l| &l.words).collect();
        assert_eq!(all[0].text, "lost");
        assert_eq!(
            all[0].rect,
            Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 50.0,
                y1: 0.0
            }
        );
        assert_eq!(all[1].confidence, Some(100.0));
        assert_eq!(
            all[1].rect,
            Rect {
                x0: 100.0,
                y0: 100.0,
                x1: 100.0,
                y1: 100.0
            }
        );
        // The word past the depth bound is no word of its own: its text is
        // the open word's, and there is none, so it is dropped.
        assert!(all.iter().all(|w| w.text != "deep"));
        assert!(parse(b"").lines.is_empty());
        assert!(parse(b"\xff\xfe<<<>>>&&&").lines.is_empty());
    }

    #[test]
    fn a_page_of_many_words_is_read_in_linear_time() {
        let mut s = String::from("<div class='ocr_page' title='bbox 0 0 1000 1000'>");
        for i in 0..200_000 {
            if i % 10 == 0 {
                s.push_str("<span class='ocr_line' title='bbox 0 0 10 10'>");
            }
            s.push_str("<span class='ocrx_word' title='bbox 0 0 5 5; x_wconf 90'>w</span>");
            if i % 10 == 9 {
                s.push_str("</span>");
            }
        }
        let t = std::time::Instant::now();
        let p = parse(s.as_bytes());
        assert_eq!(p.lines.len(), 20_000);
        assert!(t.elapsed().as_secs() < 20, "{:?}", t.elapsed());
    }
}
