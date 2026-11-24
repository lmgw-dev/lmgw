//! What the page-reading model is asked (design §3 tier 1, D23): the page
//! prompts of the knowledge bases (`extract/vision_prompts.rs`, measured
//! 2026-09-24) with a conversion's output form. Their fidelity sentences
//! are kept word for word (reproduce exactly as printed; with the page's
//! text given, copy from it character for character and use the image for
//! the layout only). What changes is the form asked for: markdown the
//! block model reads (headings, list items, tables as markdown tables
//! instead of one line per cell, which a knowledge base's chunks want and a
//! converted document does not), and D23's formulas as LaTeX and code as
//! fenced blocks.
//!
//! Nothing about the request goes into a prompt: no file name, no caller.
//! The page's own text does (with [`IMAGE_AND_TEXT_PROMPT`]), as untrusted
//! as the answer: whatever it says, the answer only becomes text blocks.

use super::select::Ask;

/// The version of the prompts below, for whoever keeps readings: changing a
/// prompt means bumping it.
pub const PAGE_PROMPT_VERSION: &str = "1";

/// A page without text: its image alone.
pub const IMAGE_PROMPT: &str = "Transcribe this document page as Markdown. Reproduce every \
piece of text exactly as printed, in reading order, without translating, summarising or adding \
anything. Mark headings with #, list items with -, and write every table as a Markdown table \
with its header row. Write every formula as LaTeX between $$ lines, and every piece of program \
code in a fenced code block. Output only the transcription.";

/// A page with text, before the page's text is appended ([`prompt`]).
pub const IMAGE_AND_TEXT_PROMPT: &str = "Below is the exact text of this page, extracted from \
the PDF. Rewrite it as Markdown in reading order. Copy every word and number character for \
character from the extracted text — never from the image, never corrected, translated or \
summarised; use the image only to see the layout: which lines are headings, which row and \
column each table value belongs to, what is a formula and what is program code. Mark headings \
with #, list items with -, and write every table as a Markdown table with its header row. \
Write every formula as LaTeX between $$ lines, and every piece of program code in a fenced code \
block. Output only the rewritten page.\n\nExtracted text:\n";

/// What a DocTags model is asked: docling's own instruction, the one
/// granite-docling was measured with (design §14 WP0a). It reads the image
/// alone.
pub const DOCTAGS_PROMPT: &str = "Convert this page to docling.";

/// The prompt for a page asked as `ask`; `text` is the page's own text as
/// the tools read it (used for [`Ask::ImageAndText`] only).
pub fn prompt(ask: Ask, doctags: bool, text: &str) -> String {
    match (doctags, ask) {
        (true, _) => DOCTAGS_PROMPT.to_string(),
        (false, Ask::Image) => IMAGE_PROMPT.to_string(),
        (false, Ask::ImageAndText) => format!("{IMAGE_AND_TEXT_PROMPT}{text}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompts_ask_for_latex_and_fenced_code_and_carry_only_the_pages_text() {
        for p in [IMAGE_PROMPT, IMAGE_AND_TEXT_PROMPT] {
            assert!(p.contains("LaTeX between $$ lines"), "{p}");
            assert!(p.contains("fenced code block"), "{p}");
            assert!(p.contains("Markdown table"), "{p}");
        }
        let p = prompt(Ask::ImageAndText, false, "Rent   800");
        assert!(p.ends_with("Extracted text:\nRent   800"));
        assert_eq!(prompt(Ask::Image, false, "ignored"), IMAGE_PROMPT);
        assert_eq!(prompt(Ask::ImageAndText, true, "ignored"), DOCTAGS_PROMPT);
    }
}
