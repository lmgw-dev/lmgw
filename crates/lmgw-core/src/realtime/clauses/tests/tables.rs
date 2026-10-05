//! Tables are skipped like fenced code, and every skipped block is reported
//! where it begins (chat-voice design §6.2).

use super::super::{Block, ClauseAggregator, Item};
use super::all;

/// Every item of `text` streamed char by char, then flushed.
fn items(text: &str) -> Vec<Item> {
    let mut agg = ClauseAggregator::new();
    let mut out = Vec::new();
    for ch in text.chars() {
        out.extend(agg.push_items(&ch.to_string()));
    }
    out.extend(agg.flush_items());
    assert_eq!(agg.taken(), text.len(), "everything is taken: {text:?}");
    out
}

/// `items` as text: a clause's, or `<code rust>` / `<table>` for a block.
fn shown(text: &str) -> Vec<String> {
    items(text)
        .into_iter()
        .map(|i| match i {
            Item::Clause(c) => c.text,
            Item::Block(Block::Code { info: Some(i) }, _) => format!("<code {i}>"),
            Item::Block(Block::Code { info: None }, _) => "<code>".into(),
            Item::Block(Block::Table, _) => "<table>".into(),
        })
        .collect()
}

const TABLE: &str =
    "Vergleich:\n| Modell | Größe |\n|---|---|\n| klein | 1 GB |\n| groß | 9 GB |\nDanach weiter.";

#[test]
fn a_table_is_no_clause() {
    assert_eq!(
        all(&mut ClauseAggregator::new(), TABLE),
        ["Vergleich:", "Danach weiter."]
    );
    // Whole in one delta, the same.
    let mut agg = ClauseAggregator::new();
    let mut out = agg.push(TABLE);
    out.extend(agg.flush());
    assert_eq!(out, ["Vergleich:", "Danach weiter."]);
    // Reported once, at its first row, between the clauses around it.
    assert_eq!(shown(TABLE), ["Vergleich:", "<table>", "Danach weiter."]);
}

#[test]
fn the_history_keeps_the_rows() {
    let mut agg = ClauseAggregator::new();
    let mut placed = Vec::new();
    for ch in TABLE.chars() {
        placed.extend(agg.push_placed(&ch.to_string()));
    }
    placed.extend(agg.flush_placed());
    // What lies between the two clauses is the table, no clause's own.
    assert_eq!(
        &TABLE[placed[0].end..placed[1].start],
        "| Modell | Größe |\n|---|---|\n| klein | 1 GB |\n| groß | 9 GB |\n"
    );
}

#[test]
fn a_row_the_stream_ends_inside_is_a_row() {
    // The answer's last line is a row without its line end.
    assert_eq!(
        shown("Zahlen.\n| a | b |\n| 1 | 2 |"),
        ["Zahlen.", "<table>"]
    );
    // A table that is the whole answer, its only row unfinished.
    assert_eq!(shown("| a | b |"), ["<table>"]);
    // A row waits for its line end, then goes unsaid.
    let mut agg = ClauseAggregator::new();
    assert!(agg.push_items("| a | b").is_empty());
    assert_eq!(agg.push_items(" |\n"), [Item::Block(Block::Table, 0)]);
    assert!(agg.push_items("| 1 | 2 |\n").is_empty());
    assert_eq!(agg.flush_items(), []);
}

#[test]
fn a_new_table_is_reported_again() {
    // A blank line, text or a fence ends a table; the next one is new.
    assert_eq!(
        shown("| a |\n\n| b |\nText.\n| c |\n```\nx\n```\n| d |\n"),
        ["<table>", "<table>", "Text.", "<table>", "<code>", "<table>"]
    );
}

#[test]
fn only_a_line_starting_with_a_pipe_is_a_row() {
    // Up to three spaces may come first; four make it indented code in
    // CommonMark, and it is read as text, as indented code always was.
    assert_eq!(shown("   | a | b |\nok"), ["<table>", "ok."]);
    let four = shown("    | a | b |\nok");
    assert_eq!(four.len(), 2, "{four:?}");
    assert!(four[0].contains("a | b"), "{four:?}");
    assert_eq!(shown("\t| a |\nok").len(), 2, "a tab is four columns");
    // A pipe inside a line is text.
    assert_eq!(shown("Use a | b here."), ["Use a | b here."]);
    // A table without leading pipes is not detected (module doc of
    // `blocks`): it is read as text (the delimiter line, with no words of
    // its own, joins the line after it).
    assert_eq!(shown("a | b\n--|--\n1 | 2\n"), ["a | b.", "--|--\n1 | 2."]);
    // Inside fenced code a pipe is code, and no table.
    assert_eq!(shown("```\n| x |\n```\nok"), ["<code>", "ok."]);
}

#[test]
fn a_code_block_is_reported_with_its_info_string() {
    assert_eq!(
        shown("Hier:\n```rust\nfn main() {}\n```\nFertig.\n~~~\nraw\n~~~\n"),
        ["Hier:", "<code rust>", "Fertig.", "<code>"]
    );
    // An unclosed fence is reported as it opens.
    assert_eq!(shown("So:\n```sh\nls"), ["So:", "<code sh>"]);
}

#[test]
fn a_pipe_line_that_is_no_row_is_text() {
    // Prose that opens with a pipe (review m1): no closing pipe, no
    // delimiter row after it — read, not announced as a table.
    assert_eq!(
        shown("|x| ist der Betrag von x.\nMehr nicht."),
        ["|x| ist der Betrag von x.", "Mehr nicht."]
    );
    assert_eq!(shown("|| true\n"), ["|| true."]);
    // At the end of the stream, with or without its line end.
    assert_eq!(shown("|v| = 5"), ["|v| = 5."]);
    assert_eq!(shown("|x| ist gut\n| a | b |"), ["|x| ist gut.", "<table>"]);
    // A table still continues on any line that starts with a pipe.
    assert_eq!(shown("| a | b |\n| x\nText.\n"), ["<table>", "Text."]);
}

#[test]
fn a_header_without_its_closing_pipe_is_one_when_a_delimiter_row_follows() {
    // GFM's header row needs no closing pipe; its delimiter row says it is
    // one — with or without a leading pipe.
    for table in [
        "Vergleich:\n| a | b\n|---|---|\n| 1 | 2\nDanach.",
        "Vergleich:\n| a | b\n--- | ---\n| 1 | 2 |\nDanach.",
    ] {
        assert_eq!(
            shown(table),
            ["Vergleich:", "<table>", "Danach."],
            "{table:?}"
        );
    }
    // The line after it decides, once it is whole: it waits for that line
    // only, and is spoken as soon as it is known to be text.
    let mut agg = ClauseAggregator::new();
    assert!(agg.push_items("|x| ist gut\n").is_empty());
    assert!(agg.push_items("Und ").is_empty());
    let said: Vec<String> = agg
        .push_items("so.\n")
        .into_iter()
        .map(|i| match i {
            Item::Clause(c) => c.text,
            Item::Block(b, _) => format!("{b:?}"),
        })
        .collect();
    assert_eq!(said, ["|x| ist gut.", "Und so."]);
    // The rows of a header that waited are still the history's.
    let mut agg = ClauseAggregator::new();
    let text = "Hier:\n| a | b\n|---|---|\nFertig.";
    let mut placed = Vec::new();
    for ch in text.chars() {
        placed.extend(agg.push_placed(&ch.to_string()));
    }
    placed.extend(agg.flush_placed());
    assert_eq!(
        &text[placed[0].end..placed[1].start],
        "| a | b\n|---|---|\n"
    );
}

#[test]
fn a_block_is_reported_at_its_offset_in_the_stream() {
    let text = "Hier:\n```rust\nfn main() {}\n```\nDann:\n| a | b\n|---|---|\nFertig.\n| x |";
    let at: Vec<usize> = items(text)
        .into_iter()
        .filter_map(|i| match i {
            Item::Block(_, at) => Some(at),
            Item::Clause(_) => None,
        })
        .collect();
    assert_eq!(
        at,
        [
            text.find("```").unwrap(),
            text.find("| a").unwrap(),
            text.find("| x").unwrap()
        ]
    );
}
