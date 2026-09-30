//! The ingestion prompt, keyed by version (§4, §8).
//!
//! A corpus is a function of two models — the embedder and the ingest model —
//! *and* of the instructions the ingest model was given. Pinning
//! `ingest_prompt_version` on the corpus row is what makes the third one
//! visible: editing the prompt below without minting a new version would
//! silently change what a re-ingest produces while every corpus kept claiming
//! it was built the old way.
//!
//! **The rule for changing a prompt:** add a new `const` and a new arm in
//! [`for_version`], leave the old text alone. Old corpora keep resolving their
//! own version; new ingests take [`CURRENT`]. Removing a version is a breaking
//! change to every corpus that names it, which is why nothing is ever edited in
//! place.

use crate::error::{QuickdocError, Result};

use super::extract::EMIT_TOOL;
use super::source::SourceKind;

/// The version a new corpus pins.
pub const CURRENT: &str = "v1";

/// Every version this build can run, newest first.
pub const AVAILABLE: [&str; 1] = [CURRENT];

/// System prompt for a version, or an error naming what this build has.
pub fn for_version(version: &str) -> Result<&'static str> {
    match version {
        "v1" => Ok(V1),
        other => Err(QuickdocError::Invalid(format!(
            "unknown ingest prompt version {other:?} — this build has {}",
            AVAILABLE.join(", ")
        ))),
    }
}

/// The per-document turn: what this page is, and the numbered window to work on.
///
/// `first_line`/`last_line` are the absolute line numbers of the window inside
/// the document, so a document split across several windows keeps one
/// coordinate system and the spans of every window index the same text.
pub fn document_turn(
    url: &str,
    kind: SourceKind,
    window: &str,
    first_line: usize,
    last_line: usize,
    total_lines: usize,
) -> String {
    let scope = if first_line == 1 && last_line >= total_lines {
        format!("All {total_lines} lines are below.")
    } else {
        format!(
            "This is lines {first_line}–{last_line} of {total_lines}; \
             the rest of the document is handled separately, so do not \
             reference lines outside this range."
        )
    };
    format!(
        "Document: {url}\nFormat: {kind}\n{scope}\n\n\
         Call {EMIT_TOOL} with the sections of the text below.\n\n{window}"
    )
}

const V1: &str = "\
You split documentation into retrievable sections. You are one stage of an \
indexing pipeline, not a writer: the text you select is stored exactly as it \
appears, and anything you retype instead of select is a corruption of the \
documentation.

You are shown a document with every line numbered as `N| text`. The numbers are \
not part of the text.

Call `emit_extraction` with one entry per section, in document order:

- `start_line` / `end_line`: the section's first and last line, inclusive. \
Ranges must not overlap, and together they should cover the useful text of the \
document.
- `first_line` / `last_line`: those two lines copied character for character, \
WITHOUT the `N| ` prefix. These are checked against the document. If a copy \
does not match, the section is rejected and nothing about it is stored.
- `heading_path`: the headings the section sits under, outermost first, joined \
by ' > '.
- `derived_title` and `derived_summary`: your own words. They are stored as \
labels next to the section, never as its text.
- `boilerplate`: true for navigation, badge rows, edit links, license footers \
and similar — those ranges are dropped.

How to cut:

- One coherent topic per section. A prose explanation and the code sample that \
demonstrates it belong together; keep a fenced code block whole, including its \
opening and closing fence.
- Prose sections matter as much as code sections. Do not emit only the code.
- Aim for sections a reader could answer a question from on their own. A single \
line is usually too small; a whole page is usually too large.
- If part of the document is not documentation at all, mark it boilerplate \
rather than leaving a gap you cannot explain.

When every section has been accepted, reply with one short sentence describing \
what you extracted. Do not call the tool again after that.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_resolve_and_unknown_ones_name_what_exists() {
        assert!(for_version(CURRENT).is_ok());
        let err = for_version("v99").unwrap_err().to_string();
        assert!(err.contains("v99") && err.contains("v1"), "{err}");
    }

    #[test]
    fn a_windowed_document_says_so() {
        let whole = document_turn("u", SourceKind::Markdown, "1| x\n", 1, 1, 1);
        assert!(whole.contains("All 1 lines are below."), "{whole}");
        let part = document_turn("u", SourceKind::Markdown, "5| x\n", 5, 9, 20);
        assert!(part.contains("lines 5–9 of 20"), "{part}");
    }
}
