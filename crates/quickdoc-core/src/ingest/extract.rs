//! The verbatim-payload contract (§8, steps 2–4).
//!
//! The model never writes a payload. It emits **line ranges plus anchors** under
//! a JSON schema; code slices the original document by those ranges and checks
//! the anchors. A payload is therefore a pure slice of the source text by
//! construction, and the anchors are what prove the model was actually looking
//! at the lines it named rather than at a hallucinated position.
//!
//! Line ranges rather than byte offsets because the failure mode matters:
//! small models cannot count characters, so a byte-offset contract would reject
//! almost everything and teach nothing. They *can* copy a line back, and a
//! copied line that does not match is exactly the signal
//! [`validate`] needs.
//!
//! Context7's `use axum({` — a rewritten, corrupted code sample served as
//! documentation — is the failure this whole module exists to make impossible.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Name of the tool the extraction run is driven through. The tool's parameter
/// schema *is* the constrained-output schema: llama.cpp compiles it to the same
/// grammar `response_format: json_schema` would, and going through a tool keeps
/// the retry conversational — a rejected span comes back as a tool result the
/// model can correct.
pub const EMIT_TOOL: &str = "emit_extraction";

/// One section the model proposes, in the schema [`emit_schema`] constrains.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedSection {
    /// 1-based, inclusive, into the numbered document the model was shown.
    pub start_line: usize,
    /// 1-based, inclusive.
    pub end_line: usize,
    /// Verbatim copy of line `start_line` — the anchor code checks.
    #[serde(default)]
    pub first_line: String,
    /// Verbatim copy of line `end_line`.
    #[serde(default)]
    pub last_line: String,
    /// `Getting started > Routing > Handlers`.
    #[serde(default)]
    pub heading_path: String,
    /// Derived, stored apart from the payload and surfaced only as a label (§8).
    #[serde(default)]
    pub derived_title: String,
    #[serde(default)]
    pub derived_summary: String,
    /// Boilerplate the model wants dropped — nav, license footers, edit links.
    /// Validated like any other range, then discarded instead of stored.
    #[serde(default)]
    pub boilerplate: bool,
}

/// The `emit_extraction` payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Extraction {
    #[serde(default)]
    pub sections: Vec<ProposedSection>,
}

/// JSON schema of [`Extraction`], handed to the model as the tool's parameters.
pub fn emit_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sections": {
                "type": "array",
                "description":
                    "Every section of the document, in order. Ranges may not overlap.",
                "items": {
                    "type": "object",
                    "properties": {
                        "start_line": {
                            "type": "integer", "minimum": 1,
                            "description": "First line of the section (1-based, inclusive)."
                        },
                        "end_line": {
                            "type": "integer", "minimum": 1,
                            "description": "Last line of the section (1-based, inclusive)."
                        },
                        "first_line": {
                            "type": "string",
                            "description":
                                "Line start_line copied EXACTLY, without its number prefix."
                        },
                        "last_line": {
                            "type": "string",
                            "description":
                                "Line end_line copied EXACTLY, without its number prefix."
                        },
                        "heading_path": {
                            "type": "string",
                            "description": "Enclosing headings, outermost first, joined by ' > '."
                        },
                        "derived_title": {
                            "type": "string",
                            "description": "Short title you write for this section."
                        },
                        "derived_summary": {
                            "type": "string",
                            "description": "One or two sentences you write about this section."
                        },
                        "boilerplate": {
                            "type": "boolean",
                            "description":
                                "True if this range is navigation, badges, license or other \
                                 non-documentation text that should be dropped."
                        }
                    },
                    "required": ["start_line", "end_line", "first_line", "last_line"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["sections"],
        "additionalProperties": false
    })
}

/// Byte offsets of every line of a document, so a line range converts to a span
/// without re-scanning the text.
///
/// Lines are split on `\n` and the terminator belongs to the line before it —
/// so a span's `end` is the offset just past a line's `\n`, and concatenating
/// consecutive spans reproduces the document exactly.
#[derive(Debug, Clone)]
pub struct LineIndex {
    /// `starts[i]` is the byte offset of line `i+1`; the last entry is the
    /// document length, so line `n` is `starts[n-1]..starts[n]`.
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0usize];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        // A document not ending in a newline still has a final line; one that
        // does must not gain an empty one.
        if *starts.last().unwrap_or(&0) < text.len() || starts.len() == 1 {
            starts.push(text.len());
        }
        Self { starts }
    }

    /// Number of lines.
    pub fn len(&self) -> usize {
        self.starts.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Byte span of lines `first..=last` (1-based), or `None` when out of range.
    pub fn span(&self, first: usize, last: usize) -> Option<(usize, usize)> {
        if first == 0 || last < first || last > self.len() {
            return None;
        }
        Some((self.starts[first - 1], self.starts[last]))
    }

    /// Line `n` (1-based) without its terminator.
    pub fn line<'a>(&self, text: &'a str, n: usize) -> Option<&'a str> {
        let (a, b) = self.span(n, n)?;
        Some(text[a..b].trim_end_matches('\n').trim_end_matches('\r'))
    }

    /// The document as the model sees it: `   12| text`, one line per line, for
    /// lines `first..=last`. The numbers are the contract's coordinate system,
    /// so they are absolute over the whole document even when only a window of
    /// it is shown.
    pub fn numbered(&self, text: &str, first: usize, last: usize) -> String {
        let width = last.to_string().len();
        let mut out = String::new();
        for n in first..=last.min(self.len()) {
            let line = self.line(text, n).unwrap_or("");
            out.push_str(&format!("{n:>width$}| {line}\n"));
        }
        out
    }
}

/// A section that passed the contract: a pure slice of the source document.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedSection {
    pub span: (usize, usize),
    /// `document[span.0..span.1]` — never model text.
    pub payload: String,
    pub heading_path: String,
    pub derived_title: String,
    pub derived_summary: String,
}

/// A section that failed it, with the reason the model is told.
#[derive(Debug, Clone, PartialEq)]
pub struct Rejection {
    /// Position in the emitted `sections` array, so the model knows which one.
    pub index: usize,
    pub reason: String,
}

/// Outcome of one `emit_extraction` call.
#[derive(Debug, Clone, Default)]
pub struct Verdict {
    pub accepted: Vec<AcceptedSection>,
    pub rejected: Vec<Rejection>,
    /// Ranges the model marked as boilerplate; validated, then dropped.
    pub dropped: usize,
}

impl Verdict {
    pub fn is_clean(&self) -> bool {
        self.rejected.is_empty()
    }
}

/// Slice `document` by every proposed range and check the anchors.
///
/// A failure is **never** repaired: the section is rejected with a reason
/// naming what the line actually says, and the caller feeds that back so the
/// model can re-emit. Nothing that fails here can reach a corpus.
pub fn validate(document: &str, idx: &LineIndex, ex: &Extraction) -> Verdict {
    let mut v = Verdict::default();
    for (i, s) in ex.sections.iter().enumerate() {
        let reject = |reason: String| Rejection { index: i, reason };
        let Some(span) = idx.span(s.start_line, s.end_line) else {
            v.rejected.push(reject(format!(
                "lines {}–{} are not a range inside this document (it has {} lines)",
                s.start_line,
                s.end_line,
                idx.len()
            )));
            continue;
        };
        // Anchors: trailing whitespace is not a rewrite, anything else is.
        for (label, n, quoted) in [
            ("first_line", s.start_line, &s.first_line),
            ("last_line", s.end_line, &s.last_line),
        ] {
            let actual = idx.line(document, n).unwrap_or("");
            if actual.trim_end() != quoted.trim_end() {
                v.rejected.push(reject(format!(
                    "{label} does not match line {n}: you wrote {quoted:?}, \
                     the document has {actual:?}"
                )));
                break;
            }
        }
        if v.rejected.last().is_some_and(|r| r.index == i) {
            continue;
        }
        let payload = &document[span.0..span.1];
        // Belt and braces: the payload is a slice, so this holds by
        // construction — and the whole corpus rests on it holding, so it is
        // asserted rather than assumed.
        debug_assert!(is_verbatim(document, span, payload));
        if payload.trim().is_empty() {
            v.rejected.push(reject(format!(
                "lines {}–{} are blank — a payload has to carry text",
                s.start_line, s.end_line
            )));
            continue;
        }
        if s.boilerplate {
            v.dropped += 1;
            continue;
        }
        v.accepted.push(AcceptedSection {
            span,
            payload: payload.to_string(),
            heading_path: s.heading_path.trim().to_string(),
            derived_title: s.derived_title.trim().to_string(),
            derived_summary: s.derived_summary.trim().to_string(),
        });
    }
    v
}

/// The guarantee itself, in one place: `payload` is exactly the source text at
/// `span`. Used by [`validate`] and available to anything that wants to re-check
/// a stored chunk against its document.
pub fn is_verbatim(document: &str, span: (usize, usize), payload: &str) -> bool {
    document.get(span.0..span.1) == Some(payload)
}

/// What the model is told after an `emit_extraction` call — the retry channel.
/// Accepted sections are acknowledged so it does not re-send them; every
/// rejection is quoted so it can correct that one specifically.
pub fn verdict_report(v: &Verdict) -> String {
    let mut s = format!(
        "accepted {} section(s), dropped {} as boilerplate, rejected {}.",
        v.accepted.len(),
        v.dropped,
        v.rejected.len()
    );
    if v.rejected.is_empty() {
        s.push_str(" Nothing left to correct — reply with a short summary of what you extracted.");
        return s;
    }
    for r in &v.rejected {
        s.push_str(&format!("\n- section {}: {}", r.index, r.reason));
    }
    s.push_str(
        "\nCall emit_extraction again with ONLY the corrected sections. \
         Copy first_line and last_line character for character from the numbered \
         document — do not retype or reformat them.",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str =
        "# Routing\n\nUse `Router::new()`:\n\n```rust\nlet app = Router::new()\n```\n";

    fn section(start: usize, end: usize, first: &str, last: &str) -> ProposedSection {
        ProposedSection {
            start_line: start,
            end_line: end,
            first_line: first.into(),
            last_line: last.into(),
            heading_path: "Routing".into(),
            derived_title: "Creating a router".into(),
            derived_summary: "How to build one.".into(),
            boilerplate: false,
        }
    }

    #[test]
    fn line_index_spans_reassemble_the_document() {
        let idx = LineIndex::new(DOC);
        assert_eq!(idx.len(), 7);
        assert_eq!(idx.line(DOC, 1), Some("# Routing"));
        assert_eq!(idx.line(DOC, 7), Some("```"));
        let (a, b) = idx.span(1, 7).unwrap();
        assert_eq!(&DOC[a..b], DOC);
    }

    #[test]
    fn a_document_without_a_trailing_newline_keeps_its_last_line() {
        let idx = LineIndex::new("a\nb");
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.line("a\nb", 2), Some("b"));
    }

    #[test]
    fn a_matching_anchor_accepts_a_verbatim_slice() {
        let idx = LineIndex::new(DOC);
        let v = validate(
            DOC,
            &idx,
            &Extraction {
                sections: vec![section(3, 7, "Use `Router::new()`:", "```")],
            },
        );
        assert!(v.is_clean(), "{:?}", v.rejected);
        let a = &v.accepted[0];
        assert_eq!(
            a.payload,
            "Use `Router::new()`:\n\n```rust\nlet app = Router::new()\n```\n"
        );
        assert!(is_verbatim(DOC, a.span, &a.payload));
        assert_eq!(a.derived_title, "Creating a router");
    }

    /// The context7 failure: the model *rewrote* the code it claimed to select.
    /// The anchor catches it, and nothing is stored.
    #[test]
    fn a_rewritten_anchor_is_rejected_never_repaired() {
        let idx = LineIndex::new(DOC);
        let v = validate(
            DOC,
            &idx,
            &Extraction {
                sections: vec![section(
                    6,
                    6,
                    "let app = Router::new({",
                    "let app = Router::new({",
                )],
            },
        );
        assert!(v.accepted.is_empty(), "a rewritten span must not be stored");
        assert_eq!(v.rejected.len(), 1);
        assert!(
            v.rejected[0]
                .reason
                .contains("first_line does not match line 6"),
            "{}",
            v.rejected[0].reason
        );
        assert!(verdict_report(&v).contains("emit_extraction again"));
    }

    #[test]
    fn out_of_range_and_blank_ranges_are_rejected() {
        let idx = LineIndex::new(DOC);
        let v = validate(
            DOC,
            &idx,
            &Extraction {
                sections: vec![section(1, 99, "# Routing", "x"), section(2, 2, "", "")],
            },
        );
        assert!(v.accepted.is_empty());
        assert_eq!(v.rejected.len(), 2);
        assert!(v.rejected[0].reason.contains("7 lines"));
        assert!(v.rejected[1].reason.contains("blank"));
    }

    #[test]
    fn boilerplate_is_validated_then_dropped() {
        let idx = LineIndex::new(DOC);
        let mut s = section(1, 1, "# Routing", "# Routing");
        s.boilerplate = true;
        let v = validate(DOC, &idx, &Extraction { sections: vec![s] });
        assert!(v.is_clean());
        assert_eq!(v.dropped, 1);
        assert!(v.accepted.is_empty());
    }

    #[test]
    fn numbering_is_absolute_over_the_whole_document() {
        let idx = LineIndex::new(DOC);
        let window = idx.numbered(DOC, 5, 7);
        assert!(window.starts_with("5| ```rust\n"), "{window}");
        assert!(window.contains("\n7| ```\n"), "{window}");
    }
}
