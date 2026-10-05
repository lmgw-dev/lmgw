//! Sentence ends of other scripts (`stops`; TTS batches, 2026-10-05): with
//! no word cap, a text whose sentences end with `।` or `。` got no cut until
//! a line end.

use super::super::{speakable, ClauseAggregator};
use super::{all, placed};

/// Every clause of `text` pushed whole, then flushed.
fn at_once(text: &str) -> Vec<String> {
    let mut agg = ClauseAggregator::new();
    let mut out = agg.push(text);
    out.extend(agg.flush());
    out
}

/// `text` cut into `expected`, whether streamed char by char or pushed
/// whole.
fn cuts(text: &str, expected: &[&str]) {
    assert_eq!(all(&mut ClauseAggregator::new(), text), expected, "{text}");
    assert_eq!(at_once(text), expected, "{text}, at once");
    // Placed: each clause's own bytes, on char boundaries, all of the text.
    let own: Vec<&str> = placed(text).iter().map(|p| &text[p.start..p.end]).collect();
    assert_eq!(own.len(), expected.len(), "{text}, placed");
    let bare = |s: &str| s.replace(char::is_whitespace, "");
    assert_eq!(bare(&own.concat()), bare(text), "{text}, placed");
}

#[test]
fn a_danda_ends_a_sentence() {
    cuts(
        "यह पहला वाक्य है। यह दूसरा है।",
        &["यह पहला वाक्य है।", "यह दूसरा है।"],
    );
    // The double danda closes a verse.
    cuts("राम गया॥ श्याम आया॥", &["राम गया॥", "श्याम आया॥"]);
    // With no space after it there is no sentence end, as with a `!`.
    cuts("एक।दो। तीन।", &["एक।दो।", "तीन।"]);
}

#[test]
fn the_other_spaced_stops_end_a_sentence() {
    // Arabic: the question mark, then a full stop.
    cuts("هل أنت هنا؟ نعم.", &["هل أنت هنا؟", "نعم."]);
    // Armenian and Ethiopic full stops.
    cuts(
        "Ես տանն եմ։ Դու որտեղ ես։",
        &["Ես տանն եմ։", "Դու որտեղ ես։"],
    );
    cuts("ሰላም ነው። እንዴት ነህ።", &["ሰላም ነው።", "እንዴት ነህ።"]);
}

#[test]
fn a_spaced_stop_waits_for_what_follows_it() {
    let mut agg = ClauseAggregator::new();
    assert!(agg.push("यह पहला वाक्य है।").is_empty());
    assert_eq!(agg.push(" यह"), ["यह पहला वाक्य है।"]);
    assert_eq!(agg.flush().as_deref(), Some("यह."));
}

#[test]
fn a_cjk_stop_ends_a_sentence_without_a_space() {
    cuts(
        "今日は晴れです。明日は雨です。",
        &["今日は晴れです。", "明日は雨です。"],
    );
    cuts(
        "你好！你好吗？我很好。",
        &["你好！", "你好吗？", "我很好。"],
    );
    // The half-width stop.
    cuts("今日｡明日｡", &["今日｡", "明日｡"]);
    // A space after it is just a space.
    cuts(
        "今日は晴れです。 明日は雨です。",
        &["今日は晴れです。", "明日は雨です。"],
    );
    // Latin text, a wide stop: the same.
    cuts("Hello！World？", &["Hello！", "World？"]);
}

#[test]
fn a_closing_quote_stays_with_its_sentence() {
    // The cut is after the closer, not before it.
    cuts(
        "彼は「行く。」と言った。次。",
        &["彼は「行く。」", "と言った。", "次。"],
    );
    cuts(
        "彼は（行く。）と言った。",
        &["彼は（行く。）", "と言った。"],
    );
    // Several closers, and a stop in the run.
    cuts(
        "彼は『「行く。」』と言った。",
        &["彼は『「行く。」』", "と言った。"],
    );
    cuts("えっ？！本当？", &["えっ？！", "本当？"]);
    cuts("他说“好。”然后走了。", &["他说“好。”", "然后走了。"]);
    cuts("他说“好。”\n然后走了。", &["他说“好。”", "然后走了。"]);
}

#[test]
fn a_cjk_stop_waits_for_the_next_character() {
    let mut agg = ClauseAggregator::new();
    assert!(agg.push("今日は晴れです。").is_empty());
    assert_eq!(agg.push("明"), ["今日は晴れです。"]);
    assert_eq!(agg.flush().as_deref(), Some("明."));
    // A closer may follow, and another after it.
    let mut agg = ClauseAggregator::new();
    assert!(agg.push("彼は「行く。").is_empty());
    assert!(agg.push("」").is_empty());
    assert!(agg.push("』").is_empty());
    assert_eq!(agg.push("と"), ["彼は「行く。」』"]);
    // The end of the stream settles it: nothing is added after a closer.
    let mut agg = ClauseAggregator::new();
    assert!(agg.push("彼は「行く。」").is_empty());
    assert_eq!(agg.flush().as_deref(), Some("彼は「行く。」"));
}

#[test]
fn a_run_of_characters_is_something_said() {
    // One run of Han characters is a word: the stop after it ends the
    // sentence.
    cuts("一二三四。五六。", &["一二三四。", "五六。"]);
    // A stop before anything was said is no sentence end, as a "." is not.
    cuts("。次。明日。", &["。次。", "明日。"]);
    cuts("। अगला। फिर।", &["। अगला।", "फिर।"]);
    // A tag is no words: "[laughs]。" has said nothing yet.
    cuts("[laughs]。次。明日。", &["[laughs]。次。", "明日。"]);
}

#[test]
fn a_clause_is_closed_only_when_it_has_no_stop_of_its_own() {
    // The last clause, flushed: no full stop on top of a script's own.
    for text in [
        "यह दूसरा है।",
        "هل أنت هنا؟",
        "今日は晴れです。",
        "彼は「行く。」",
        "今日は（晴れです。）",
    ] {
        assert_eq!(all(&mut ClauseAggregator::new(), text), [text]);
    }
    // One without, in any script, still gets one.
    assert_eq!(
        all(&mut ClauseAggregator::new(), "今日は晴れです"),
        ["今日は晴れです."]
    );
    assert_eq!(
        all(&mut ClauseAggregator::new(), "यह दूसरा है\nफिर"),
        ["यह दूसरा है.", "फिर."]
    );
}

#[test]
fn the_speakable_pass_leaves_the_stops_alone() {
    for text in [
        "यह पहला वाक्य है।",
        "هل أنت هنا؟",
        "彼は「行く。」と言った。",
        "彼は（行く。）と言った。",
    ] {
        assert_eq!(speakable(text), text);
    }
    // An ASCII parenthesis closing before a stop sets no comma there, as
    // before a ".".
    assert_eq!(speakable("彼は(行く)。"), "彼は, 行く。");
    assert_eq!(speakable("(はい)。"), "はい。");
}
