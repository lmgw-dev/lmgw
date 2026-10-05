//! What a voice says of the chat stream (realtime design §8.1, §23 L7): line
//! ends and list items as clauses, rule lines silent, known abbreviations
//! never a sentence end and said in full, parentheses as commas — and the
//! spoken transcript is what was said.

use lmgw_core::realtime::clauses::{speakable, ClauseAggregator, ABBREVIATIONS};
use serde_json::{json, Value};

use crate::support::realtime_fakes::{events_until, send, user_text, Turn};
use crate::support::realtime_tts::{speech_gateway, spoken_session};

/// `text` streamed in `step`-character deltas, then flushed: the clauses.
fn clauses_by(text: &str, step: usize) -> Vec<String> {
    let mut agg = ClauseAggregator::new();
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    for delta in chars.chunks(step) {
        out.extend(agg.push(&delta.iter().collect::<String>()));
    }
    out.extend(agg.flush());
    out
}

/// What is spoken of `text`: each clause through the speakable pass, the
/// empty ones dropped — the responder's pipeline.
fn spoken(text: &str) -> Vec<String> {
    clauses_by(text, 1)
        .iter()
        .map(|c| speakable(c))
        .filter(|c| !c.is_empty())
        .collect()
}

#[test]
fn a_line_end_is_a_boundary_so_a_heading_speaks_at_once() {
    let mut agg = ClauseAggregator::new();
    assert!(agg.push("## Weather in Berlin").is_empty());
    // No punctuation at all: the line end alone makes it a clause.
    // Closed with a full stop, so it is spoken as finished (package B
    // review 4).
    assert_eq!(agg.push("\nIt is"), ["## Weather in Berlin."]);
    // Fenced code is not read out.
    assert_eq!(
        spoken("## Weather\nSunny and warm\n```rust\nlet x = 1;\n```\nDone."),
        ["Weather.", "Sunny and warm.", "Done."]
    );
    // A blank line between paragraphs is no clause of its own.
    assert_eq!(
        clauses_by("One here.\n\nTwo there.", 1),
        ["One here.", "Two there."]
    );
}

#[test]
fn list_items_are_clauses_and_their_numbers_are_not_glued_on() {
    // Live L7: "Apples 2." — the next item's number at the end of this one.
    assert_eq!(
        spoken("Here are three:\n1. Apples\n2. Pears\n3. Plums"),
        ["Here are three:", "Apples.", "Pears.", "Plums."]
    );
    assert_eq!(
        spoken("Steps:\n1. First step.\n2. Second step.\n  - nested item\n10) last"),
        [
            "Steps:",
            "First step.",
            "Second step.",
            "nested item.",
            "last."
        ]
    );
    // Mid-line numbers are text, and a lone number is an answer. A dot
    // before a lowercase word ends no sentence (B3 item 15), and is said as
    // written: only a German date is written out (fix package B6).
    assert_eq!(spoken("Pick 1. or 2. today"), ["Pick 1. or 2. today."]);
    assert_eq!(spoken("Pick 1. Or 2."), ["Pick 1.", "Or 2."]);
    assert_eq!(spoken("42."), ["42."]);
    // An ordinal or a year at a line start keeps its number (package B
    // review 3): `N.` is a marker only when it starts or continues a list.
    // A date is one clause (B3 item 15), its day written out for the
    // voice (live run 2 E2) — at the clause start "-ter" (fix package B6);
    // a year still ends a sentence.
    assert_eq!(
        spoken("Am Ende:\n3. Oktober ist frei\n2024. Ein Jahr"),
        [
            "Am Ende:",
            "Dritter Oktober ist frei.",
            "2024.",
            "Ein Jahr."
        ]
    );
}

#[test]
fn rule_lines_are_silent() {
    // Live L7: a `---` line became 1.48 s of silence.
    assert_eq!(
        spoken("Intro text.\n---\nMore text.\n***\n_ _ _\nEnd."),
        ["Intro text.", "More text.", "End."]
    );
    for rule in ["---", "***", "___", "- - -", "  ______"] {
        assert_eq!(speakable(rule), "", "{rule}");
    }
    // Two marks, or marks with text, are not a rule.
    assert_eq!(speakable("-- not a rule"), "-- not a rule");
}

#[test]
fn known_abbreviations_are_no_sentence_end_and_are_said_in_full() {
    // Live L7: "e.g." was spoken "egg".
    assert_eq!(
        spoken("Fruit, e.g. apples, is good. Next one."),
        ["Fruit, for example apples, is good.", "Next one."]
    );
    // The inner dot of "z. B." waits for the rest, streamed one character at
    // a time.
    assert_eq!(
        spoken("Das sind z. B. Äpfel. Und d.h. mehr."),
        ["Das sind zum Beispiel Äpfel.", "Und das heißt mehr."]
    );
    for (written, said) in ABBREVIATIONS {
        assert_eq!(speakable(&format!("A {written} b")), format!("A {said} b"));
    }
    // One that ends the clause keeps its full stop.
    assert_eq!(
        speakable("Äpfel, Birnen usw."),
        "Äpfel, Birnen und so weiter."
    );
    // Case-sensitive, word-boundary safe, and decimals are never touched.
    for same in [
        "Ca. ist ein Element.",
        "Africa. Then",
        "The etc.x file",
        "Pi is 3.14 vs.2",
    ] {
        assert_eq!(speakable(same), same);
    }
    assert_eq!(speakable("ca. 3.5 kg"), "circa 3.5 kg");
    assert_eq!(
        clauses_by("In Africa. Then more.", 1),
        ["In Africa.", "Then more."]
    );
    // One that can end a sentence does, before a capital, and is said with
    // its stop (package B review 2); a title never does, and stays as is.
    assert_eq!(
        spoken("Obst, Gemüse usw. Dann Dr. Weber."),
        ["Obst, Gemüse und so weiter.", "Dann Dr. Weber."]
    );
}

#[test]
fn parentheses_become_commas() {
    // Live L7: ten seconds of audio and a word lost.
    let german = "Sie können die Uhrzeit von Geräten (Computer, Smartphone etc.) ablesen.";
    assert_eq!(
        spoken(german).join(" "),
        "Sie können die Uhrzeit von Geräten, Computer, Smartphone et cetera, ablesen."
    );
    assert_eq!(speakable("(Hinweis) Text folgt"), "Hinweis, Text folgt");
    assert_eq!(speakable("Hello (world)."), "Hello, world.");
    assert_eq!(speakable("Fine! (really)"), "Fine! really");
}

#[test]
fn the_scan_is_incremental_and_cuts_the_same_however_the_text_arrives() {
    // Each text, and how many of its words are list markers or code — no
    // text, and left out.
    let texts = [
        (
            "Sure, let me check the weather for you right now in Berlin today.",
            0,
        ),
        (
            "## Title\n1. One item\n2. Two, e.g. this. More z. B. here.\n---\nThe end",
            2,
        ),
        ("Pi is about 3.14 which is useful. Das macht 3,14 Euro.", 0),
        (
            "Ein langer Satz ohne Satzzeichen der einfach immer weiter geht und weiter und \
             weiter bis die Wortgrenze greift und dann noch ein wenig mehr Text folgt hier",
            0,
        ),
        (
            "Code:\n```sh\nls -la\n```\nObst usw. Dann Dr. Weber\n3. Oktober\n- item",
            5,
        ),
    ];
    for (text, no_text) in texts {
        let one = clauses_by(text, 1);
        for step in [2, 3, 7, 1000] {
            assert_eq!(clauses_by(text, step), one, "{text:?} in steps of {step}");
        }
        let words = |t: &str| t.split_whitespace().count();
        assert_eq!(
            words(&one.join(" ")) + no_text,
            words(text),
            "nothing lost: {one:?}"
        );
    }
}

#[tokio::test]
async fn the_spoken_transcript_is_what_the_voice_said() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    chat.push(Turn::text(&[
        "## Weather\n",
        "- sunny, e.g. ",
        "warm\n---\n",
        "Done (mostly).",
    ]));
    let (mut ws, _) = spoken_session(&addr, &[], 60_000, json!({})).await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = events_until(&mut ws, "response.done").await;
    let said: Vec<String> = (0..tts.seen.count())
        .map(|n| tts.seen.body(n)["input"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        said,
        ["Weather.", "sunny, for example warm.", "Done, mostly."]
    );
    let deltas: String = events
        .iter()
        .filter(|e| e["type"] == "response.output_audio_transcript.delta")
        .map(|e| e["delta"].as_str().unwrap())
        .collect();
    assert_eq!(deltas, said.join(" "));
    let done: &Value = &events.last().unwrap()["response"];
    assert_eq!(
        done["output"][0]["content"][0]["transcript"],
        said.join(" ")
    );
}
