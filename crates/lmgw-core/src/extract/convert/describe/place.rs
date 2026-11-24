//! Where a picture's description is written (design §5.3): after the
//! picture item, as docling's serializers write an item's meta
//! (docling-core 2.104.0, `_meta_position` "after"). docling's picture item
//! holds its caption; lmgw keeps the caption as the block (or the reader's
//! paragraph) that WP10 placed beside the picture. So the description goes
//! after the caption when the caption follows the picture, and right after
//! the picture otherwise (a caption above it, or none): image, caption,
//! description, in markdown and in text alike.

use std::collections::HashMap;

use super::super::doc::{BlockKind, Doc};
use super::super::pictures::Found;
use super::paragraph;

/// Whether `text` (a block's or a reader paragraph's) is `caption`, the
/// words the picture's caption lookup took: equal once markdown's escapes
/// and runs of white space are set aside.
pub(crate) fn is_caption_of(text: &str, caption: &str) -> bool {
    fn plain(s: &str) -> impl Iterator<Item = &str> {
        s.split_whitespace()
    }
    let unescaped = text.replace('\\', "");
    let caption = caption.replace('\\', "");
    !caption.trim().is_empty() && plain(&unescaped).eq(plain(&caption))
}

/// Each described figure's description, keyed by the address of the block
/// it is written after in `doc` (the figure's caption block when that
/// follows the figure, else the figure itself): as a markdown paragraph, or
/// as plain text when `text`. Valid while `doc` is not changed.
pub(crate) fn after_blocks(doc: &Doc, found: &Found, text: bool) -> HashMap<usize, String> {
    let mut out = HashMap::new();
    for part in &doc.parts {
        for (k, b) in part.blocks.iter().enumerate() {
            if b.kind != BlockKind::Figure {
                continue;
            }
            let Some(p) = b
                .figure
                .as_ref()
                .and_then(|f| found.pictures.get(f.index as usize))
            else {
                continue;
            };
            let Some(d) = p.description.as_deref() else {
                continue;
            };
            let written = if text {
                d.trim().to_string()
            } else {
                paragraph(d)
            };
            let caption_next = part.blocks.get(k + 1).filter(|n| {
                matches!(n.kind, BlockKind::Caption | BlockKind::Paragraph)
                    && p.caption
                        .as_deref()
                        .is_some_and(|c| is_caption_of(&n.text, c))
            });
            let at = caption_next.unwrap_or(b);
            out.insert(std::ptr::from_ref(at) as usize, written);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caption_is_matched_by_its_words() {
        assert!(is_caption_of(
            "Figure 1:  Example\nchart",
            " Figure 1: Example chart"
        ));
        assert!(is_caption_of("Figure 1\\* a", "Figure 1* a"));
        assert!(!is_caption_of(
            "Figure 2: Example chart",
            "Figure 1: Example chart"
        ));
        assert!(!is_caption_of("", ""));
    }
}
