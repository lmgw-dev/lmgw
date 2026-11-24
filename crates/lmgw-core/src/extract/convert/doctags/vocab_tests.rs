//! Every token of docling-core's vocabulary through the reader (review
//! finding S12): none may come out as literal text, wherever it stands,
//! and no word around it may be lost.

use super::vocab::{CODE_LANGUAGES, PICTURE_CLASSES};
use super::*;

/// docling-core 2.104.0's `DocumentToken` values (each with a closing
/// tag), as `types/doc/tokens.py` lists them.
const DOCUMENT_TOKENS: &[&str] = &[
    "doctag",
    "otsl",
    "chart",
    "ordered_list",
    "unordered_list",
    "page_break",
    "smiles",
    "inline",
    "caption",
    "footnote",
    "formula",
    "list_item",
    "page_footer",
    "page_header",
    "picture",
    "table",
    "text",
    "title",
    "document_index",
    "code",
    "checkbox_selected",
    "checkbox_unselected",
    "form",
    "key_value_region",
    "paragraph",
    "reference",
    "handwritten_text",
];

/// `TableToken`'s values.
const TABLE_TOKENS: &[&str] = &[
    "column_header",
    "row_header",
    "shed",
    "data",
    "ecel",
    "fcel",
    "lcel",
    "ucel",
    "xcel",
    "nl",
    "ched",
    "rhed",
    "srow",
];

/// Every token of the vocabulary, with its angle brackets.
fn vocabulary() -> Vec<String> {
    let mut all = Vec::new();
    for name in DOCUMENT_TOKENS {
        all.push(format!("<{name}>"));
        all.push(format!("</{name}>"));
    }
    for level in 0..6 {
        all.push(format!("<section_header_level_{level}>"));
        all.push(format!("</section_header_level_{level}>"));
    }
    all.extend(PICTURE_CLASSES.iter().map(|c| format!("<{c}>")));
    for lang in CODE_LANGUAGES {
        all.push(format!("<_{lang}_>"));
        all.push(format!("<{lang}/>"));
    }
    all.extend(TABLE_TOKENS.iter().map(|t| format!("<{t}>")));
    for n in [0, 1, 250, 499, 500] {
        all.push(format!("<loc_{n}>"));
        all.push(format!("<loc_{n}/>"));
    }
    // Locations no grid has.
    all.extend(["<loc_501>", "<loc_99999>", "<loc_abc>", "<loc_>"].map(String::from));
    // The key-value serializer's cells and links.
    for label in ["key", "value", "checkbox", "unspecified"] {
        all.push(format!("<{label}_7>"));
        all.push(format!("</{label}_7>"));
    }
    all.push("<link_7>".to_string());
    // A model's own control tokens.
    all.extend(["<|end_of_text|>", "<|im_end|>"].map(String::from));
    all
}

/// All the text a read page holds, its tables' cells included.
fn all_text(r: &Read) -> String {
    let mut all = String::new();
    for b in &r.blocks {
        all.push_str(&b.text);
        all.push(' ');
        if let BlockKind::Table(t) = &b.kind {
            for c in &t.cells {
                all.push_str(&c.text);
                all.push(' ');
            }
        }
    }
    all
}

#[test]
fn the_lists_are_docling_core_s() {
    // 2.104.0: 45 picture classes, 57 code languages.
    assert_eq!(PICTURE_CLASSES.len(), 45);
    assert_eq!(CODE_LANGUAGES.len(), 57);
    let longest = vocabulary().iter().map(String::len).max().unwrap();
    assert!(longest <= MAX_TAG_BYTES + 1, "{longest}");
}

#[test]
fn no_token_of_the_vocabulary_leaks_as_text_and_no_word_beside_it_is_lost() {
    let contexts: &[(&str, &str)] = &[
        ("<doctag>alpha ", " omega</doctag>"),
        ("<doctag><text>alpha ", " omega</text></doctag>"),
        ("<doctag><picture>alpha ", " omega</picture></doctag>"),
        ("<doctag><otsl><fcel>alpha ", " omega<nl></otsl></doctag>"),
        ("<doctag><otsl>alpha ", " omega<fcel>x<nl></otsl></doctag>"),
        ("<doctag><code>alpha ", " omega</code></doctag>"),
        (
            "<doctag><text><inline>alpha ",
            " omega</inline></text></doctag>",
        ),
        ("alpha ", " omega"),
    ];
    for token in vocabulary() {
        for (before, after) in contexts {
            let answer = format!("{before}{token}{after}");
            let r = read(&answer, Some((500.0, 500.0)));
            let text = all_text(&r);
            assert!(
                !text.contains('<') && !text.contains('>'),
                "{token} leaked in {answer:?}: {text:?}"
            );
            assert!(
                text.contains("alpha") && text.contains("omega"),
                "a word was lost around {token} in {answer:?}: {text:?}"
            );
        }
    }
}

/// A tag that only looks like one of the vocabulary's is text.
#[test]
fn near_misses_stay_text() {
    for not_a_token in [
        "<Text>",
        "<loc 5>",
        "<key_>",
        "<key_x>",
        "<link_>",
        "<_NotALanguage_>",
        "<NotALanguage/>",
        "<section_header_level_x>",
        "<section_header_level_+1>",
        "<bar chart>",
        "<|two words|>",
    ] {
        let r = read(
            &format!("<doctag><text>a {not_a_token} b</text></doctag>"),
            None,
        );
        assert_eq!(r.blocks.len(), 1, "{not_a_token}");
        assert_eq!(r.blocks[0].text, format!("a {not_a_token} b"));
        assert!(!r.issues.any(), "{not_a_token}: {:?}", r.issues);
    }
}
