//! Skipped blocks through the speech splitter (chat-voice design §6.2): a
//! stock session skips code and tables silently; the Chat's callers hear
//! one short clause where each begins, which changes nothing the model's
//! history keeps.

use super::super::*;
use crate::realtime::clauses::Announce;

const ANSWER: &str =
    "Vergleich:\n```rust\nfn main() {}\n```\n| a | b |\n|---|---|\n| 1 | 2 |\nFertig.";

/// What the splitter hands the speaker for `ANSWER`, streamed char by char:
/// each clause as `(said, before, own)`, then what nothing says after the
/// last one.
fn split(announce: Option<Announce>) -> (Vec<(String, String, String)>, String) {
    let (clauses, unspoken) = split_written(announce);
    let clauses = clauses
        .into_iter()
        .map(|(said, w)| (said, w.before, w.own))
        .collect();
    (clauses, unspoken)
}

/// [`split`], each clause with its whole [`Written`].
fn split_written(announce: Option<Announce>) -> (Vec<(String, Written)>, String) {
    let (tx, _core) = mpsc::unbounded_channel();
    let (work, mut queue) = mpsc::unbounded_channel();
    let (_stop, signal) = crate::proxy::stop_pair();
    let mut sink = Splitter::new(1, &tx, work, signal, "chat thread 1", announce);
    for ch in ANSWER.chars() {
        sink.on_delta(&StreamDelta::TextDelta(ch.to_string()));
    }
    sink.finish();
    drop(sink);
    let (mut clauses, mut unspoken) = (Vec::new(), String::new());
    while let Ok(w) = queue.try_recv() {
        match w {
            Work::Clause { said, written, .. } => clauses.push((said, written)),
            Work::Unspoken(raw) => unspoken.push_str(&raw),
            Work::Pass(_) => panic!("no delta was passed"),
            Work::Break => {}
        }
    }
    (clauses, unspoken)
}

/// What the model's history keeps: every clause's text as written, and
/// what nothing said.
fn history(clauses: &[(String, String, String)], unspoken: &str) -> String {
    let mut h: String = clauses.iter().map(|(_, b, o)| format!("{b}{o}")).collect();
    h.push_str(unspoken);
    h
}

#[test]
fn a_stock_session_skips_code_and_tables_and_announces_nothing() {
    let (clauses, unspoken) = split(None);
    let said: Vec<&str> = clauses.iter().map(|c| c.0.as_str()).collect();
    assert_eq!(said, ["Vergleich:", "Fertig."]);
    // The code and the table are the second clause's text before it.
    assert_eq!(
        clauses[1].1,
        "```rust\nfn main() {}\n```\n| a | b |\n|---|---|\n| 1 | 2 |\n"
    );
    assert_eq!(history(&clauses, &unspoken), ANSWER);
}

#[test]
fn the_chat_hears_where_a_block_was_skipped() {
    let (clauses, unspoken) = split(Some(Announce::for_language(Some("de"))));
    let said: Vec<&str> = clauses.iter().map(|c| c.0.as_str()).collect();
    assert_eq!(
        said,
        ["Vergleich:", "Codeblock, rust.", "Tabelle.", "Fertig."]
    );
    // An announcement is nothing the model wrote: it carries what came
    // before its block, the clause after it the block — so the history is
    // the same, and a cut can tell which block was announced (§8.4).
    assert_eq!((clauses[1].1.as_str(), clauses[1].2.as_str()), ("", ""));
    assert_eq!(
        (clauses[2].1.as_str(), clauses[2].2.as_str()),
        ("```rust\nfn main() {}\n```\n", "")
    );
    assert_eq!(clauses[3].1, "| a | b |\n|---|---|\n| 1 | 2 |\n");
    assert_eq!(history(&clauses, &unspoken), ANSWER);
    let (written, _) = split_written(Some(Announce::for_language(Some("de"))));
    let marked: Vec<bool> = written.iter().map(|(_, w)| w.announcement).collect();
    assert_eq!(marked, [false, true, true, false]);
    let (stock, _) = split_written(None);
    assert!(stock.iter().all(|(_, w)| !w.announcement));
    // English where the hint is none.
    let (clauses, _) = split(Some(Announce::for_language(None)));
    let said: Vec<&str> = clauses.iter().map(|c| c.0.as_str()).collect();
    assert_eq!(
        said,
        ["Vergleich:", "Code block, rust.", "Table.", "Fertig."]
    );
}

#[test]
fn a_block_inside_a_clause_that_began_before_it_is_announced_once() {
    // A line with no words, then a block: the clause after the block began
    // before it. Every byte is still one clause's, once.
    for text in ["**\n```sh\nls\n```\nweiter so.", "—\n| a |\nweiter so."] {
        let (tx, _core) = mpsc::unbounded_channel();
        let (work, mut queue) = mpsc::unbounded_channel();
        let (_stop, signal) = crate::proxy::stop_pair();
        let announce = Some(Announce::for_language(None));
        let mut sink = Splitter::new(1, &tx, work, signal, "chat thread 1", announce);
        for ch in text.chars() {
            sink.on_delta(&StreamDelta::TextDelta(ch.to_string()));
        }
        sink.finish();
        drop(sink);
        let mut history = String::new();
        let mut announced = 0;
        while let Ok(w) = queue.try_recv() {
            match w {
                Work::Clause { written, .. } => {
                    announced += usize::from(written.announcement);
                    history.push_str(&written.before);
                    history.push_str(&written.own);
                }
                Work::Unspoken(raw) => history.push_str(&raw),
                Work::Pass(_) | Work::Break => {}
            }
        }
        assert_eq!(announced, 1, "{text:?}");
        assert_eq!(history, text);
    }
}
