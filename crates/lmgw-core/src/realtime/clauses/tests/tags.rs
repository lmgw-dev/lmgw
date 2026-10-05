//! Inline tags through the splitter and the speakable pass (WP10 D8).

use super::super::{speakable, ClauseAggregator};
use super::all;

fn words(n: usize) -> String {
    (1..=n)
        .map(|k| format!("w{k}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn a_tag_inside_a_long_sentence_stays_whole() {
    // The word cap once cut at the space inside "[clears throat]" (the 25th
    // word): both halves were read aloud. With no cap a sentence is one
    // clause, tag and all.
    let text = format!("Yes. {} [clears throat] then more.", words(24));
    let out = all(&mut ClauseAggregator::new(), &text);
    assert_eq!(
        out,
        ["Yes.", &format!("{} [clears throat] then more.", words(24))]
    );
    assert_eq!(out.join(" "), text, "nothing lost");
}

#[test]
fn a_tag_alone_on_a_line_is_no_clause() {
    let out = all(&mut ClauseAggregator::new(), "[laughs]\nOkay.");
    assert_eq!(out, ["[laughs]\nOkay."]);
    assert_eq!(speakable(&out[0]), "[laughs] Okay.");
    // Nor is one before a sentence end: its words are none, so the dot has
    // said nothing yet.
    let out = all(&mut ClauseAggregator::new(), "[laughs]. Okay. Gut.");
    assert_eq!(out, ["[laughs]. Okay.", "Gut."]);
    // However many words its name has.
    let out = all(&mut ClauseAggregator::new(), "[clears throat]\nOkay.");
    assert_eq!(out, ["[clears throat]\nOkay."]);
    // Brackets that are no tag are words as before.
    let out = all(&mut ClauseAggregator::new(), "[Hinweis]\nOkay.");
    assert_eq!(out, ["[Hinweis].", "Okay."]);
    let out = all(&mut ClauseAggregator::new(), "[see\nOkay.");
    assert_eq!(
        out,
        ["[see.", "Okay."],
        "a candidate that dies gives its words back"
    );
}

#[test]
fn speakable_keeps_every_tag_whole_and_canonical() {
    assert_eq!(
        speakable("Ha *laughs* that's (laughs) funny [laughs]."),
        "Ha [laughs] that's [laughs] funny [laughs]."
    );
    assert_eq!(speakable("**[sighs] fine**"), "[sighs] fine");
    assert_eq!(speakable("[quick_breath] Okay."), "[quick_breath] Okay.");
    assert_eq!(speakable("<Laughs> [Sighs] so."), "[laughs] [sighs] so.");
    // A link is a link, not a tag.
    assert_eq!(
        speakable("Read [see here](https://x.y) now."),
        "Read see here now."
    );
    // The rest of the pass still runs around a tag.
    assert_eq!(
        speakable("[laughs] z. B. am 3. Oktober"),
        "[laughs] zum Beispiel am dritten Oktober"
    );
    // Prose in parentheses stays prose.
    assert_eq!(
        speakable("Ja (wirklich) und (sighs) gut."),
        "Ja, wirklich, und [sighs] gut."
    );
}

#[test]
fn the_text_s_own_private_use_characters_are_dropped() {
    assert_eq!(
        speakable("a\u{E000}b [laughs] c\u{F8FF}."),
        "ab [laughs] c."
    );
}
