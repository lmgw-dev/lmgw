//! Where a continued reply's read-aloud starts (chat-voice design §6.4;
//! WP4 review m5).
//!
//! A continue streams the rest of a reply that broke off, mostly at the
//! output limit and mid-sentence: "…der Ofen heizt scho" + "n vor. Danach
//! …". Read on its own, the continuation would open with "n vor." So the
//! read-aloud is fed the stored reply's **unfinished clause** first — what
//! the clause splitter has not cut off by the reply's end — and the first
//! clause it says is whole: "der Ofen heizt schon vor." The clauses before
//! it are not said again. A reply that broke off right after a sentence end
//! is not known to be finished until the next character comes (a decimal,
//! an abbreviation), so its last sentence is said again; one that broke off
//! inside a code block or a table starts at the block, so the block is
//! skipped (and announced) as it would have been. A continuation read this
//! way says a little more than its own `delta`s: the `speech` frames carry
//! the clause as said.

use crate::realtime::clauses::ClauseAggregator;

/// The tail of `content`, a stored reply, that its read-aloud's
/// continuation starts with (module doc): from the start of its unfinished
/// clause; `""` when it ends at a clause boundary.
pub(crate) fn lead(content: &str) -> &str {
    let mut clauses = ClauseAggregator::new();
    clauses.push_items(content);
    let rest = &content[clauses.taken().min(content.len())..];
    if rest.trim().is_empty() {
        ""
    } else {
        rest
    }
}

#[cfg(test)]
mod tests {
    use super::lead;

    #[test]
    fn a_continuation_starts_at_the_clause_it_finishes() {
        assert_eq!(
            lead("Erst das. Dann heizt der Ofen scho"),
            " Dann heizt der Ofen scho"
        );
        assert_eq!(lead("Fertig.\n"), "", "ended at a boundary");
        assert_eq!(lead(""), "");
        // A sentence end at the very end is not known to be one yet.
        assert_eq!(lead("Erst das. Das kostet 3."), " Das kostet 3.");
        // A reply that broke off inside a code block starts at it.
        assert_eq!(lead("Hier:\n```sh\nls -la\nrm"), "```sh\nls -la\nrm");
        // An abbreviation is no sentence end.
        assert_eq!(lead("Klar. Nimm z. B. den gro"), " Nimm z. B. den gro");
    }

    /// No clause is cut inside a sentence (2026-10-05): the lead is the whole
    /// unfinished sentence, whatever commas and however many words it has so
    /// far — the first comma of a reply, or a word count, once left only its
    /// tail.
    #[test]
    fn a_continuation_starts_at_the_sentence_it_finishes() {
        assert_eq!(
            lead("Klar, dann heizt der Ofen scho"),
            "Klar, dann heizt der Ofen scho"
        );
        assert_eq!(
            lead("Erst das. Klar, dann heizt der Ofen scho"),
            " Klar, dann heizt der Ofen scho"
        );
        let long = format!("Erst das. {}scho", "und dann noch ein Wort ".repeat(10));
        assert_eq!(lead(&long), &long["Erst das.".len()..]);
    }
}
