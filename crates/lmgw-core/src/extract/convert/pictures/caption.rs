//! Captions (design §5.3): the text block right below or above a picture
//! that names it as a figure ("Figure 2: …", "Fig. 3", "Abbildung 1 –",
//! "Chart:"). Where the layout model (WP8) read the page, it marks captions
//! itself and a marked one beside the figure is taken as it is
//! (`pdf/order.rs`); elsewhere the words at a block's start tell them apart.
//!
//! **The rule.** A caption word ([`WORDS`]), then a colon, or a number (or
//! roman numeral) that ends the label: followed by nothing, by punctuation
//! (`:`, `.`, `–`, `—`, `-`, `|`, `)`), or by a word that does not start
//! with a lower-case letter. So "Figure 3: Results" and "Fig. 3 The
//! example" are captions, and a sentence of the text that starts with
//! "Figure 3 shows …" or "Abbildung 4 zeigt …" is not.

/// The words a caption starts with, lower case, in the languages
/// covered so far (English, German).
const WORDS: &[&str] = &[
    "figure",
    "fig",
    "image",
    "picture",
    "photo",
    "chart",
    "graph",
    "diagram",
    "illustration",
    "plate",
    "exhibit",
    "map",
    "scheme",
    "abbildung",
    "abb",
    "bild",
    "grafik",
    "diagramm",
    "schaubild",
    "foto",
];

/// The words that make a numbered figure caption ("Figure 3", "Abb. 2"),
/// what a picture of a paper or a report has: a numbered caption with no
/// picture beside it is a figure lmgw did not find (a vector drawing).
const NUMBERED: &[&str] = &[
    "figure",
    "fig",
    "chart",
    "graph",
    "diagram",
    "abbildung",
    "abb",
    "diagramm",
];

/// The first word of `text` (lower case) and what follows it: right after
/// the word (`after`), and with `.`/`:` and spaces cut off (`rest`). Every
/// slice is cut at the word's own end in `text`, never at the length of
/// its lower-case form, which can differ (`K` U+212A is three bytes and
/// lowers to `k`, one).
struct Head<'a> {
    word: String,
    after: &'a str,
    rest: &'a str,
}

fn head(text: &str) -> Option<Head<'_>> {
    let t = text.trim_start();
    let end = t
        .find(|c: char| c.is_whitespace() || c == ':' || c == '.')
        .unwrap_or(t.len());
    if end == 0 || end > 16 {
        return None;
    }
    let after = &t[end..];
    let rest = after.trim_start_matches(['.', ':']).trim_start();
    Some(Head {
        word: t[..end].to_lowercase(),
        after,
        rest,
    })
}

/// A number or a roman numeral at the start of `rest` (`3`, `2.1`, `IV`,
/// `3a`) that ends the label (module doc).
fn numbered(rest: &str) -> bool {
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ':')
        .unwrap_or(rest.len());
    let word = &rest[..end];
    let number = word.chars().next().is_some_and(|c| c.is_ascii_digit())
        || (!word.is_empty()
            && word
                .trim_end_matches(['.', ')'])
                .chars()
                .all(|c| matches!(c, 'I' | 'V' | 'X')));
    if !number {
        return false;
    }
    // What follows the number: punctuation, nothing, or no lower-case word.
    let next = rest[end..].trim_start();
    match next.chars().next() {
        None => true,
        Some(':' | '.' | '–' | '—' | '-' | '|' | ')') => true,
        Some(c) => !c.is_lowercase() || word.ends_with(['.', ':', ')']),
    }
}

/// Whether `text` reads as a picture's caption: a caption word, then a
/// colon or a number that ends the label (`Figure 1`, `Chart: sales`).
pub fn is_caption(text: &str) -> bool {
    let Some(h) = head(text) else {
        return false;
    };
    if !WORDS.contains(&h.word.as_str()) {
        return false;
    }
    h.after.trim_start().starts_with(':') || numbered(h.rest)
}

/// The words a table's caption starts with: a caption the layout model
/// marked beside a figure that names a table is the table's, not the
/// figure's.
const TABLE_WORDS: &[&str] = &["table", "tab", "tbl", "tabelle"];

/// Whether `text` names a table ("Table 2: …", "Tab. 3", "Tabelle 1").
pub fn names_table(text: &str) -> bool {
    head(text).is_some_and(|h| TABLE_WORDS.contains(&h.word.as_str()))
}

/// Whether `text` is a numbered figure caption ("Figure 3", "Abb. 2").
pub fn is_numbered_figure(text: &str) -> bool {
    head(text).is_some_and(|h| NUMBERED.contains(&h.word.as_str()) && numbered(h.rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captions_start_with_a_figure_word_and_a_number_or_colon() {
        for yes in [
            "Figure 1: Example revenue",
            "Fig. 3 The example",
            "fig 2",
            "Abbildung 4 – Beispiel",
            "Abb. 2: Beispiel",
            "Chart: example sales",
            "Figure IV. Example",
            "Figure 3. shows the example",
            "Figure 2 (a) example",
            "Figure 12",
        ] {
            assert!(is_caption(yes), "{yes}");
        }
        for no in [
            "Figures show the example",
            "The figure 1 shows",
            "Image quality matters",
            "",
            "Figure",
            "Figure 3 shows the example results",
            "Abbildung 4 zeigt das Beispiel",
            "Fig. 2 and 3 compare the examples",
        ] {
            assert!(!is_caption(no), "{no}");
        }
        assert!(is_numbered_figure("Figure 2: Example"));
        assert!(is_numbered_figure("Abb. 3"));
        assert!(!is_numbered_figure("Figure 3 shows the example"));
        assert!(!is_numbered_figure("Chart: example"));
        assert!(!is_numbered_figure("Photo 1"));
        for yes in ["Table 2: Example", "Tab. 3", "tabelle 1 Beispiel", "TBL 4"] {
            assert!(names_table(yes), "{yes}");
        }
        for no in ["Tables show", "Figure 1: Example table", "", "The table 2"] {
            assert!(!names_table(no), "{no}");
        }
    }

    #[test]
    fn letters_whose_lower_case_has_another_length_never_cut_a_character() {
        // K (KELVIN SIGN, 3 bytes) lowers to k (1 byte): "grafiK" is
        // "grafik" in 8 bytes, not 6.
        assert!(is_caption("grafi\u{212A}: example"));
        assert!(is_caption("Grafi\u{212A} 2 Example"));
        // ẞ (3 bytes) lowers to ß (2), İ (2) to i̇ (3), final sigma.
        for text in [
            "\u{1E9E}\u{1E9E}\u{1E9E}: x",
            "\u{130}MAGE 1: x",
            "\u{130}\u{130}\u{130}\u{130}\u{130}\u{130}\u{130}\u{130}",
            "\u{3A3}\u{03A7}\u{0397}\u{039C}\u{0391}\u{3A3} 1: x",
            "\u{212A}",
            "Figure\u{212A}\u{212A}:",
            "Abbildung \u{212A}",
            "Figure 3\u{212A} example",
        ] {
            let _ = is_caption(text);
            let _ = is_numbered_figure(text);
        }
        assert!(!is_caption("\u{130}MAGE 1: x"));
    }
}
