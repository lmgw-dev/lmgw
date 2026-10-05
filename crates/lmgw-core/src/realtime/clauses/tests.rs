use super::*;

// ─── the earlier prototype's aggregator tests, verbatim ─────────────────────

fn all(agg: &mut ClauseAggregator, text: &str) -> Vec<String> {
    // Feed char-by-char to emulate token streaming, then flush.
    let mut out = Vec::new();
    for ch in text.chars() {
        out.extend(agg.push(&ch.to_string()));
    }
    out.extend(agg.flush());
    out
}

/// TTS batches, 2026-10-05: a clause is a sentence, never a part of one. A
/// cut at the first comma was spoken as two utterances with the engine's
/// silence between ("In Berlin," | "it's currently around 14 degrees ...").
#[test]
fn the_first_clause_runs_to_the_sentence_end() {
    let mut agg = ClauseAggregator::new();
    let out = all(
        &mut agg,
        "Sure, let me check the weather for you right now in Berlin today.",
    );
    assert_eq!(
        out,
        ["Sure, let me check the weather for you right now in Berlin today."]
    );
    // The live case: the comma after a short opening phrase.
    let mut agg = ClauseAggregator::new();
    let out = all(
        &mut agg,
        "In Berlin, it's currently around 14 degrees Celsius.",
    );
    assert_eq!(
        out,
        ["In Berlin, it's currently around 14 degrees Celsius."]
    );
    // Every comma of the sentence, and a push that ends at one.
    let mut agg = ClauseAggregator::new();
    assert!(agg.push("Well, ").is_empty());
    assert!(agg.push("yes, ").is_empty());
    assert!(agg.push("and so, ").is_empty());
    assert_eq!(agg.push("on. Next"), ["Well, yes, and so, on."]);
}

/// No word cap, in the first clause or a later one (TTS batches,
/// 2026-10-05): a long sentence stays whole — "... a mix of" | "sun and
/// clouds." was spoken as two. An engine that needs shorter input chunks it
/// itself.
#[test]
fn a_long_sentence_stays_whole() {
    // A comma-less first sentence is the first clause, whole.
    let mut agg = ClauseAggregator::new();
    let out = all(
        &mut agg,
        "It is currently around 23 degrees with partly cloudy conditions in Berlin.",
    );
    assert_eq!(
        out,
        ["It is currently around 23 degrees with partly cloudy conditions in Berlin."]
    );
    // A later one, far over the old 24-word cap, with and without commas.
    let numbered = format!(
        "{}.",
        (1..=120)
            .map(|k| format!("w{k}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    for sentence in [
        "The clouds are slowly making way for some sunshine and a mix of sun and clouds \
         through the afternoon, although it is noted as being mostly gray later in the \
         day, with a light breeze from the west and the temperature falling to around \
         nine degrees by the evening.",
        numbered.as_str(),
    ] {
        let mut agg = ClauseAggregator::new();
        let out = all(&mut agg, &format!("Yes. {sentence} Done."));
        assert_eq!(out, ["Yes.", sentence, "Done."]);
    }
}

#[test]
fn the_first_clause_still_ends_at_a_sentence_end() {
    // A sentence end is a boundary in the first clause and in every other.
    let mut agg = ClauseAggregator::new();
    let out = all(
        &mut agg,
        "Yes. It is around 23 degrees with partly cloudy skies in Berlin right now.",
    );
    assert_eq!(
        out,
        [
            "Yes.",
            "It is around 23 degrees with partly cloudy skies in Berlin right now."
        ]
    );
}

#[test]
fn sentences_split_on_terminal_punctuation() {
    let mut agg = ClauseAggregator::new();
    let out = all(&mut agg, "Yes. It is sunny and warm today. Enjoy it!");
    assert_eq!(out[0], "Yes.");
    assert!(out.iter().any(|c| c == "It is sunny and warm today."));
    assert!(out.iter().any(|c| c == "Enjoy it!"));
}

#[test]
fn decimals_and_numbers_do_not_split() {
    let mut agg = ClauseAggregator::new();
    let out = all(&mut agg, "Pi is about 3.14 which is useful.");
    // The "3.14" must stay intact in whichever clause carries it.
    assert!(out.iter().any(|c| c.contains("3.14")));
    assert_eq!(out.join(" "), "Pi is about 3.14 which is useful.");
}

#[test]
fn german_decimal_comma_does_not_split() {
    let mut agg = ClauseAggregator::new();
    let out = all(&mut agg, "Das macht 3,14 Euro.");
    assert!(
        out.iter().any(|c| c.contains("3,14")),
        "DE decimal comma kept intact"
    );
    assert_eq!(out.join(" "), "Das macht 3,14 Euro.");
}

// ─── speakable ──────────────────────────────────────────────────────────────

#[test]
fn emphasis_markers_go() {
    assert_eq!(
        speakable("**Bold** and *italic*, __strong__ and _em_; ~~gone~~ too."),
        "Bold and italic, strong and em; gone too."
    );
    assert_eq!(speakable("**Grüße** an _Jürgen_!"), "Grüße an Jürgen!");
    // CommonMark reads `__init__` as strong emphasis too.
    assert_eq!(speakable("Call __init__ first."), "Call init first.");
    // Split across clauses, each half still flanks text.
    assert_eq!(speakable("**This is bold."), "This is bold.");
    assert_eq!(speakable("And more.**"), "And more.");
}

#[test]
fn arithmetic_identifiers_and_punctuation_stay() {
    for s in [
        "5 * 3 = 15, 2*4 and 3.14 or 3,14.",
        "Call snake_case_name or my_var_2? Not really...",
        "Hello, world! - no; ok: fine - \"quoted\" and 'single'.",
        "About ~5 minutes, 50% off, C# and #1 hit, a [note] in brackets.",
        "Um 14:30 Uhr, am 3.10.2026.",
        "42.",
    ] {
        assert_eq!(speakable(s), s);
    }
    // A German ordinal is written out (live run 2 E2, `ordinal`).
    assert_eq!(
        speakable("Am 3. Oktober, um 14:30 Uhr."),
        "Am dritten Oktober, um 14:30 Uhr."
    );
}

#[test]
fn headings_fences_and_backticks() {
    assert_eq!(
        speakable("# Weather\n## Today: sunny"),
        "Weather Today: sunny"
    );
    // Fenced code is not read out (`blocks`).
    assert_eq!(
        speakable("Run this:\n```rust\nlet x = 1;\n```\nDone."),
        "Run this: Done."
    );
    assert_eq!(speakable("Title\n===\nSub\n--\n-"), "Title Sub");
    assert_eq!(speakable("Use `cargo build` now."), "Use cargo build now.");
}

#[test]
fn links_keep_their_text() {
    assert_eq!(
        speakable("See [the docs](https://x.y/a_(b)) and ![a cat](cat.png)."),
        "See the docs and a cat."
    );
    // Not a link: its parenthesis reads as any other, a comma.
    assert_eq!(speakable("[broken](no end"), "[broken], no end");
    // Nested brackets are not taken apart; the text is left as written.
    assert_eq!(speakable("[a [nested] b](x)"), "[a [nested] b], x");
}

#[test]
fn list_markers_at_line_start() {
    assert_eq!(
        speakable("- one\n* two\n  + three\n1. four\n10) five\n• six"),
        "one two three four five six"
    );
    // Mid-line dashes and numbers are text, an ordinal's dot and all (B6).
    assert_eq!(
        speakable("Pick 1. or 2. - both work"),
        "Pick 1. or 2. - both work"
    );
}

#[test]
fn speakable_after_the_aggregator() {
    // The pipeline order: clauses first, then each is cleaned.
    let mut agg = ClauseAggregator::new();
    let mut spoken: Vec<String> = all(
        &mut agg,
        "**Yes**, the *answer* is `42`. See [here](http://a.b).",
    )
    .iter()
    .map(|c| speakable(c))
    .collect();
    spoken.retain(|c| !c.is_empty());
    assert_eq!(spoken, ["Yes, the answer is 42.", "See here."]);
    assert_eq!(speakable(""), "");
    assert_eq!(speakable("```"), "");
}

#[test]
fn the_scan_resumes_where_it_stopped() {
    // Each delta is judged once (§23 L7: the whole buffer was rescanned per
    // delta, quadratic in a long punctuation-free first chunk).
    let mut agg = ClauseAggregator::new();
    assert!(agg.push("Hello world without").is_empty());
    assert_eq!(agg.scan.pos, agg.buf.len(), "everything judged");
    assert!(agg.push(" any punctuation 3.").is_empty());
    // The dot at the end waits for the next character.
    assert_eq!(agg.scan.pos, agg.buf.len() - 1);
    assert_eq!(
        agg.push("14 more. And"),
        ["Hello world without any punctuation 3.14 more."]
    );
    assert_eq!((agg.buf.as_str(), agg.scan.pos), (" And", 4));
}

// ─── package B review: what the splitter gets right since ───────────────────

#[test]
fn an_abbreviation_that_ends_a_sentence_ends_it() {
    let mut agg = ClauseAggregator::new();
    assert_eq!(
        all(
            &mut agg,
            "Äpfel, Birnen usw. Dann kommt mehr. Und etc. ist klein."
        ),
        [
            "Äpfel, Birnen usw.",
            "Dann kommt mehr.",
            "Und etc. ist klein."
        ]
    );
    // The space alone does not tell yet.
    let mut agg = ClauseAggregator::new();
    assert!(agg.push("Yes, and so on etc. ").is_empty());
    assert_eq!(agg.push("Then"), ["Yes, and so on etc."]);
    // Spoken, it keeps its full stop.
    assert_eq!(
        speakable("Apples etc. Then more"),
        "Apples et cetera. Then more"
    );
    assert_eq!(
        speakable("Äpfel usw. und mehr"),
        "Äpfel und so weiter und mehr"
    );
    // bzw. and ca. introduce what follows, capital or not.
    let mut agg = ClauseAggregator::new();
    assert_eq!(
        all(&mut agg, "Ja, Äpfel bzw. Birnen kosten ca. Zehn Euro."),
        ["Ja, Äpfel bzw. Birnen kosten ca. Zehn Euro."]
    );
}

#[test]
fn titles_are_no_sentence_end_and_stay_as_written() {
    let mut agg = ClauseAggregator::new();
    assert_eq!(
        all(
            &mut agg,
            "Yes. Dr. Müller and Prof. Weber met Mr. and Mrs. Smith at St. Ives, Nr. 5."
        ),
        [
            "Yes.",
            "Dr. Müller and Prof. Weber met Mr. and Mrs. Smith at St. Ives, Nr. 5."
        ]
    );
    assert_eq!(speakable("Dr. Müller, Nr. 5"), "Dr. Müller, Nr. 5");
}

#[test]
fn a_number_at_a_line_start_is_a_marker_only_in_a_list() {
    let mut agg = ClauseAggregator::new();
    assert_eq!(
        all(
            &mut agg,
            "Termine:\n3. Oktober frei\n1. Eins\n2. Zwei\n4. Vier"
        ),
        // "3. Oktober" is a date (B3 item 15). "4." neither continues the
        // numbering nor is a nested item: at the line start a number that
        // skips is text (B3 review L3) — the cost of keeping "42. Minute".
        [
            "Termine:",
            "3. Oktober frei.",
            "Eins.",
            "Zwei.",
            "4.",
            "Vier."
        ]
    );
    assert_eq!(speakable("3. Oktober frei"), "Dritter Oktober frei");
    assert_eq!(speakable("1. Eins\n2. Zwei\n3. Drei"), "Eins Zwei Drei");
    assert_eq!(speakable("1. Eins\n2. Zwei\n4. Vier"), "Eins Zwei 4. Vier");
    // After a list, a line-start number that does not continue it keeps
    // its number, blank line or not (B3 review L3). Its dot before a
    // capital is a sentence end, as anywhere ("Das kostet 30. Dann").
    for (text, want) in [
        (
            "1. Eins\n2. Zwei\n\n42. Minute: Tor für Bayern.",
            vec!["Eins.", "Zwei.", "42.", "Minute: Tor für Bayern."],
        ),
        (
            "1. Eins\n2. Zwei\n100. Geburtstag, sagt sie.",
            vec!["Eins.", "Zwei.", "100.", "Geburtstag, sagt sie."],
        ),
        (
            "1. Eins\n2. Zwei\n\n2026. war gut.",
            vec!["Eins.", "Zwei.", "2026.", "war gut."],
        ),
        // Indented, it is a nested list, whatever its number.
        (
            "1. Eins\n   5. Fünf\n2. Zwei",
            vec!["Eins.", "Fünf.", "Zwei."],
        ),
    ] {
        let mut agg = ClauseAggregator::new();
        assert_eq!(all(&mut agg, text), want, "{text}");
    }
    // After a line of text, a number that does not continue is text again.
    let mut agg = ClauseAggregator::new();
    assert_eq!(
        all(&mut agg, "1. Eins\nDazwischen Text\n4. Vier"),
        ["Eins.", "Dazwischen Text.", "4.", "Vier."]
    );
    assert_eq!(
        speakable("1. Eins\nDazwischen\n4. Vier"),
        "Eins Dazwischen 4. Vier"
    );
}

#[test]
fn a_nested_list_s_numbering_is_its_own() {
    // B2 review B: the outer "2." after "  2. b" was spoken as a number.
    let mut agg = ClauseAggregator::new();
    assert_eq!(
        all(&mut agg, "1. A\n  1. a\n  2. b\n2. B"),
        ["A.", "a.", "b.", "B."]
    );
    assert_eq!(speakable("1. A\n  1. a\n  2. b\n2. B"), "A a b B");
    // A blank line inside the list keeps it going.
    let mut agg = ClauseAggregator::new();
    assert_eq!(all(&mut agg, "1. A\n\n2. B"), ["A.", "B."]);
}

#[test]
fn a_german_ordinal_or_date_is_no_sentence_end() {
    // B3 item 15: cut after "3.", a voice pauses and reads "drei".
    for (text, want) in [
        (
            "Wir treffen uns am 3. Oktober um zehn.",
            vec!["Wir treffen uns am 3. Oktober um zehn."],
        ),
        (
            "Der 1. Mai ist frei, der 2. nicht.",
            vec!["Der 1. Mai ist frei, der 2. nicht."],
        ),
        ("Die 2. Auflage ist da.", vec!["Die 2. Auflage ist da."]),
        (
            "Bis 24. December. Then we rest.",
            vec!["Bis 24. December.", "Then we rest."],
        ),
        // Real sentence ends stay.
        (
            "Das kostet 30. Dann geht es los.",
            vec!["Das kostet 30.", "Dann geht es los."],
        ),
        (
            "Im Jahr 2026. Danach mehr.",
            vec!["Im Jahr 2026.", "Danach mehr."],
        ),
    ] {
        let mut agg = ClauseAggregator::new();
        assert_eq!(all(&mut agg, text), want, "{text}");
    }
    // A date at a line start is no list item either (B2 review C).
    let mut agg = ClauseAggregator::new();
    assert_eq!(
        all(&mut agg, "1. Mai ist Feiertag.\n2. Juni nicht."),
        ["1. Mai ist Feiertag.", "2. Juni nicht."]
    );
    assert_eq!(
        speakable("1. Mai ist Feiertag."),
        "Erster Mai ist Feiertag."
    );
}

/// Every clause of `text` streamed char by char, placed.
fn placed(text: &str) -> Vec<Placed> {
    let mut agg = ClauseAggregator::new();
    let mut out = Vec::new();
    for ch in text.chars() {
        out.extend(agg.push_placed(&ch.to_string()));
    }
    out.extend(agg.flush_placed());
    out
}

/// What each clause of `text` says — the TTS text and the transcript,
/// which are one text (`Spoken`, tags aside, and an inline list's dot,
/// R5 F2).
fn said(text: &str) -> Vec<String> {
    placed(text)
        .iter()
        .map(|p| {
            let s = Spoken::of(p);
            let heard = match p.inline_item {
                true => unstopped_number(&s.said),
                false => s.said.clone(),
            };
            assert_eq!(s.tts, heard, "the transcript is what was spoken");
            s.said
        })
        .collect()
}

/// Each clause of `text`: what the TTS is sent, and the transcript.
fn sent(text: &str) -> Vec<(String, String)> {
    placed(text)
        .iter()
        .map(|p| {
            let s = Spoken::of(p);
            (s.tts, s.said)
        })
        .collect()
}

/// R5 F2 (live run 3c): R4 kept an inline list's "2." in the clause it
/// opens, dot and all, and Pocket TTS read the dot as a sentence end —
/// "zwei", 450 to 650 ms of nothing, "Brot kaufen", longer than the gap
/// between clauses. The TTS gets the number without its dot; the transcript
/// keeps it. Dates, ordinals and line-start items are untouched.
#[test]
fn an_inline_list_s_number_goes_to_the_voice_without_its_dot() {
    let pair = |tts: &str, said: &str| (tts.to_string(), said.to_string());
    assert_eq!(
        sent("Heute ist viel zu tun. 1. Wasser holen. 2. Brot kaufen. 3. Nach Hause gehen."),
        [
            pair("Heute ist viel zu tun.", "Heute ist viel zu tun."),
            pair("1 Wasser holen.", "1. Wasser holen."),
            pair("2 Brot kaufen.", "2. Brot kaufen."),
            pair("3 Nach Hause gehen.", "3. Nach Hause gehen."),
        ]
    );
    assert_eq!(
        sent("Here is the plan. 1. Get water. 2. **Buy** bread. Then rest."),
        [
            pair("Here is the plan.", "Here is the plan."),
            pair("1 Get water.", "1. Get water."),
            pair("2 Buy bread.", "2. Buy bread."),
            pair("Then rest.", "Then rest."),
        ]
    );
    // A real sentence's number: "eins" straight on, as R4 said it, and in
    // the transcript as written.
    assert_eq!(
        sent("Das ist klar. 1. FC Köln ist abgestiegen.")[1],
        pair("1 FC Köln ist abgestiegen.", "1. FC Köln ist abgestiegen.")
    );
    // Nothing else: dates and ordinals, a number inside a clause or after
    // a comma, a line-start item (its marker is dropped), a number that
    // does not continue the numbering.
    for text in [
        "Gut. Am 3. Oktober ist Feiertag.",
        "Gut. 3. Oktober ist frei.",
        "Gut. 1. und 2. Platz.",
        "Er ist in der 3. Klasse.",
        "Plan:\n1. Wasser holen.\n2. Brot kaufen.",
        "Das kostet 30. Dann geht es los.",
        "Gut. 42. Minute: Tor.",
    ] {
        for (tts, said) in sent(text) {
            assert_eq!(tts, said, "{text}");
        }
    }
    // A long item is one clause, whole: the number opens it.
    let long = format!("Los. 1. Wasser {}holen.", "und Brot ".repeat(15));
    let parts = sent(&long);
    assert_eq!(parts.len(), 2, "{parts:?}");
    assert!(parts[1].0.starts_with("1 Wasser"), "{parts:?}");
    assert!(parts[1].1.starts_with("1. Wasser"), "{parts:?}");
    assert!(parts[1].0.ends_with("Brot holen."), "{parts:?}");
    // After the tags of a clause that said nothing: sent first, not said.
    let p = &placed("Los. 1. Wasser holen.")[1];
    let s = Spoken::after("[sighs]", p);
    assert_eq!(
        (s.tts.as_str(), s.said.as_str()),
        ("[sighs] 1 Wasser holen.", "1. Wasser holen.")
    );
    assert_eq!(unstopped_number("12. Mal."), "12 Mal.");
    assert_eq!(unstopped_number("1.5 Liter."), "1.5 Liter.");
    assert_eq!(unstopped_number("Erster Mai."), "Erster Mai.");
}

/// Live run 3, D4: "… weiter. 1. Wasser holen. 2. Brot kaufen." made "1.",
/// "2.", "3." clauses of their own — the bare number after a sentence end,
/// before a capital, is a sentence by the ordinal rule — spoken "Eins." with
/// a gap. R4 M2: D4 left the number out of the item it opened, and that
/// deleted a real sentence's "1." — German nouns are capitalised, so "Das
/// ist klar. 1. FC Köln ist abgestiegen." lost it in audio and transcript.
/// The number now goes with the clause it opens: no clause of its own, no
/// gap, and said.
#[test]
fn a_bare_number_after_a_sentence_end_opens_the_next_clause() {
    for (text, want) in [
        (
            "Dann geht es weiter. 1. Wasser holen. 2. Brot kaufen. 3. Nach Hause gehen.",
            vec![
                "Dann geht es weiter.",
                "1. Wasser holen.",
                "2. Brot kaufen.",
                "3. Nach Hause gehen.",
            ],
        ),
        (
            "Here is the plan. 1. Get water. 2. Buy bread! 3. Go home?",
            vec![
                "Here is the plan.",
                "1. Get water.",
                "2. Buy bread!",
                "3. Go home?",
            ],
        ),
        (
            "Gut! 1. Wasser holen. Dann 2. Brot kaufen.",
            vec!["Gut!", "1. Wasser holen.", "Dann 2.", "Brot kaufen."],
        ),
        // A number that neither starts nor continues the numbering is text,
        // as at a line start ("42. Minute").
        (
            "Das Spiel lief. 42. Minute: Tor.",
            vec!["Das Spiel lief.", "42.", "Minute: Tor."],
        ),
        // Dates and ordinals stay what they were (B6).
        (
            "Wir treffen uns. 3. Oktober ist frei.",
            vec!["Wir treffen uns.", "3. Oktober ist frei."],
        ),
        (
            "Gut. 1. Mai ist Feiertag.",
            vec!["Gut.", "1. Mai ist Feiertag."],
        ),
        (
            "Das kostet 30. Dann geht es los.",
            vec!["Das kostet 30.", "Dann geht es los."],
        ),
        ("Der 1. und 2. Platz.", vec!["Der 1. und 2.", "Platz."]),
    ] {
        let mut agg = ClauseAggregator::new();
        assert_eq!(all(&mut agg, text), want, "{text}");
    }
    // Run 3b's inline list: no clause is a bare number.
    let list = "Am 3. Oktober ist Feiertag. Dann geht es weiter. 1. Wasser holen. 2. Brot \
                kaufen. 3. Nach Hause gehen.";
    let spoken = said(list);
    assert_eq!(
        spoken,
        [
            "Am dritten Oktober ist Feiertag.",
            "Dann geht es weiter.",
            "1. Wasser holen.",
            "2. Brot kaufen.",
            "3. Nach Hause gehen."
        ]
    );
    assert!(
        !spoken
            .iter()
            .any(|c| c.trim_end_matches('.').chars().all(|c| c.is_ascii_digit())),
        "{spoken:?}"
    );
    // A real sentence's number is said, in audio and transcript.
    for (text, want) in [
        (
            "Das ist klar. 1. FC Köln ist abgestiegen.",
            ["Das ist klar.", "1. FC Köln ist abgestiegen."],
        ),
        (
            "Nächste Saison. 1. Bundesliga ist das Ziel.",
            ["Nächste Saison.", "1. Bundesliga ist das Ziel."],
        ),
        (
            "Bald ist es so weit. 1. Advent ist am Sonntag.",
            ["Bald ist es so weit.", "1. Advent ist am Sonntag."],
        ),
        (
            "Wie viele? 1. Das reicht.",
            ["Wie viele?", "1. Das reicht."],
        ),
    ] {
        assert_eq!(said(text), want, "{text}");
    }
    // The number is the clause's own text (B2 review 2: the history keeps
    // it), and the clause begins mid-line.
    let text = "Los. 1. Wasser holen.";
    let p = placed(text);
    let own: Vec<&str> = p.iter().map(|p| &text[p.start..p.end]).collect();
    assert_eq!(own, ["Los.", "1. Wasser holen."]);
    assert_eq!(&text[p[0].end..p[1].start], " ");
    assert_eq!((p[0].line_start, p[1].line_start), (true, false));
}

/// R4 M2: a clause begins at a line start when only spaces stand before it
/// on its line — after a line end, a fence, or at the stream's start; a
/// line-start marker the splitter dropped is not nothing. Mid-line, a
/// number first in a clause is text for the speakable pass too.
#[test]
fn a_clause_knows_whether_it_begins_a_line() {
    let starts = |text: &str| -> Vec<(String, bool)> {
        placed(text)
            .into_iter()
            .map(|p| (p.text, p.line_start))
            .collect()
    };
    assert_eq!(
        starts("Kopf\n  Zeile. Satz.\n- Punkt\n```\ncode\n```\nDanach"),
        [
            ("Kopf.".to_string(), true),
            ("Zeile.".to_string(), true),
            ("Satz.".to_string(), false),
            ("Punkt.".to_string(), false),
            ("Danach.".to_string(), true),
        ]
    );
    assert_eq!(speakable_at("1. FC Köln", false), "1. FC Köln");
    assert_eq!(speakable_at("1. FC Köln", true), "FC Köln");
    // Only the first line is mid-line (no list began there, so a "2."
    // after it is text too); a bullet is never a word.
    assert_eq!(speakable_at("1. Eins\n1. Zwei", false), "1. Eins Zwei");
    assert_eq!(speakable_at("1. Eins\n2. Zwei", false), "1. Eins 2. Zwei");
    assert_eq!(speakable_at("- Nein.", false), "Nein.");
}

/// Every clause ends a sentence or a line (TTS batches, 2026-10-05), so each
/// ends with a stop — its own, or the full stop a line end or the stream's
/// closes it with — and none is cut inside a sentence: not at a comma, not at
/// a word count, not at an abbreviation's, an ordinal's or an inline list's
/// dot, which cut nothing.
#[test]
fn a_clause_ends_at_a_sentence_end_a_line_end_or_the_stream_s() {
    let texts = |text: &str| -> Vec<String> { placed(text).into_iter().map(|p| p.text).collect() };
    assert_eq!(
        texts("Oh no, that is funny. Really? Yes! A heading\nAm 3. Oktober usw. Dann so"),
        [
            "Oh no, that is funny.",
            "Really?",
            "Yes!",
            "A heading.",
            "Am 3. Oktober usw.",
            "Dann so.",
        ]
    );
    // A sentence of thirty words is one clause.
    let long = (1..=30)
        .map(|k| format!("w{k}"))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        texts(&format!("Hi. {long}.")),
        ["Hi.".to_string(), format!("{long}.")]
    );
    for text in [
        "Oh no, that is funny. Really? Yes! A heading\nAm 3. Oktober usw. Dann so",
        &format!("Hi, there. {long}, and more"),
    ] {
        for p in placed(text) {
            assert!(p.text.ends_with(['.', '!', '?']), "{:?}", p.text);
        }
    }
}

#[test]
fn only_a_german_date_is_written_out_for_the_voice() {
    // B5 review: a bare "ab"/"bis" made cardinals ordinals and ate the
    // sentence's dot; "der"/"die" guessed a case. Fix package B6: a number
    // is written out only before a German month, everything else is said as
    // written — splitter and speakable pass together.
    for (text, want) in [
        (
            "Der Film ist ab 18. Danach gehen wir essen.",
            vec!["Der Film ist ab 18. Danach gehen wir essen."],
        ),
        ("Der Film ist ab 18.", vec!["Der Film ist ab 18."]),
        (
            "Wir haben von 9 bis 17. Danach ist zu.",
            vec!["Wir haben von 9 bis 17. Danach ist zu."],
        ),
        ("Er ist in der 3. Klasse.", vec!["Er ist in der 3. Klasse."]),
        // The splitter's own rule, unchanged: a dot before a capital that
        // no "der"/"am"/… stands before ends the sentence.
        (
            "Der 1. und 2. Platz gewinnen.",
            vec!["Der 1. und 2.", "Platz gewinnen."],
        ),
        (
            "Ende der 1. Woche geht es los.",
            vec!["Ende der 1. Woche geht es los."],
        ),
        (
            "Lies Seite 3. Dann reden wir.",
            vec!["Lies Seite 3.", "Dann reden wir."],
        ),
        (
            "Wir treffen uns am 3. und 4. Oktober.",
            vec!["Wir treffen uns am dritten und vierten Oktober."],
        ),
        ("1. Mai ist Feiertag.", vec!["Erster Mai ist Feiertag."]),
        ("Step 3. May I help?", vec!["Step 3. May I help?"]),
    ] {
        let mut agg = ClauseAggregator::new();
        let spoken: Vec<String> = all(&mut agg, text).iter().map(|c| speakable(c)).collect();
        assert_eq!(spoken, want, "{text}");
    }
}

#[test]
fn inline_triple_backticks_are_no_fence() {
    // B2 review A: taken as a fence, the rest of the answer went silent.
    let mut agg = ClauseAggregator::new();
    assert_eq!(
        all(
            &mut agg,
            "```npm i``` installiert das Paket.\nDanach geht es weiter."
        ),
        [
            "```npm i``` installiert das Paket.",
            "Danach geht es weiter."
        ]
    );
    assert_eq!(
        speakable("```npm i``` installiert das Paket."),
        "npm i installiert das Paket."
    );
    // A real fence is still silent, its info string and all.
    let mut agg = ClauseAggregator::new();
    assert_eq!(
        all(&mut agg, "So:\n```bash\nnpm i\n```\nFertig."),
        ["So:", "Fertig."]
    );
    // An unclosed one runs to the end, as CommonMark has it.
    let mut agg = ClauseAggregator::new();
    assert_eq!(all(&mut agg, "So:\n```\nnpm i\nmore"), ["So:"]);
}

#[test]
fn an_atx_heading_s_closing_hashes_are_not_spoken() {
    assert_eq!(speakable("## Titel ##"), "Titel");
    assert_eq!(speakable("# Titel #####   "), "Titel");
    assert_eq!(speakable("# C#"), "C#");
    assert_eq!(speakable("## ##"), "");
}

#[test]
fn a_line_waiting_for_its_end_is_not_searched_again_from_its_start() {
    // B2 review D: a long backtick line streamed char by char — each delta
    // judged once, and the clause comes whole at the line end.
    let mut agg = ClauseAggregator::new();
    let long = format!("```{}``` danach.\nWeiter.", "x".repeat(5000));
    let out = all(&mut agg, &long);
    assert_eq!(
        out.len(),
        2,
        "{:?}",
        out.iter().map(String::len).collect::<Vec<_>>()
    );
    assert!(out[0].ends_with("``` danach."));
}

#[test]
fn a_clause_cut_at_a_line_end_is_closed() {
    let mut agg = ClauseAggregator::new();
    assert_eq!(
        all(
            &mut agg,
            "## Weather\nSunny, warm:\n**Bold note**\n(aside)\nNo stop"
        ),
        [
            "## Weather.",
            "Sunny, warm:",
            "**Bold note**.",
            "(aside).",
            "No stop."
        ]
    );
    // A long sentence with no stop at the stream's end is one clause, closed.
    let mut agg = ClauseAggregator::new();
    let long = "Yes. one two three four five six seven eight nine ten eleven twelve \
                thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty \
                twentyone twentytwo twentythree twentyfour twentyfive end";
    let out = all(&mut agg, long);
    assert_eq!(out.len(), 2, "{out:?}");
    assert!(
        out[1].starts_with("one two") && out[1].ends_with("twentyfive end."),
        "{out:?}"
    );
    // Nothing to say, nothing closed.
    assert_eq!(closed("---"), "---");
}

#[test]
fn fenced_code_is_no_clause() {
    let mut agg = ClauseAggregator::new();
    assert_eq!(
        all(
            &mut agg,
            "Run this:\n```rust\nlet x = 1;\nfn main() {}\n```\nDone.\n~~~\nopen"
        ),
        ["Run this:", "Done."]
    );
    // A fence the stream never closes takes the rest with it.
    let mut agg = ClauseAggregator::new();
    assert!(agg.push("```\ncode line\n").is_empty());
    assert_eq!(agg.unclosed_fence(), Some(14));
    assert_eq!(agg.flush(), None);
    assert_eq!(agg.unclosed_fence(), None);
    // What it leaves unspoken is the fence's own bytes — not a list
    // marker before it that is skipped as well (B3 review NIT).
    let mut agg = ClauseAggregator::new();
    assert!(agg.push("- \n```\ncode\n").is_empty());
    assert_eq!(agg.unclosed_fence(), Some(9));
    // Inline code is text.
    let mut agg = ClauseAggregator::new();
    assert_eq!(all(&mut agg, "`x` is one"), ["`x` is one."]);
}

#[test]
fn clauses_know_where_they_sit_in_the_stream() {
    // B2 review 2: the model's history keeps what the voice leaves out, so
    // each clause says where its own text starts and ends; what lies between
    // two clauses — a list marker, fenced code — is no clause's own.
    let text = "Hier:\n```sh\nls -la\n```\n1. **Erstens** das.\n```\nrest";
    let mut agg = ClauseAggregator::new();
    let mut placed = Vec::new();
    for ch in text.chars() {
        placed.extend(agg.push_placed(&ch.to_string()));
    }
    placed.extend(agg.flush_placed());
    assert_eq!(agg.taken(), text.len(), "everything is taken");
    let own: Vec<&str> = placed.iter().map(|p| &text[p.start..p.end]).collect();
    assert_eq!(own, ["Hier:\n", "**Erstens** das."]);
    assert_eq!(placed[1].text, "**Erstens** das.");
    // Between them: the fenced code and the marker.
    assert_eq!(
        &text[placed[0].end..placed[1].start],
        "```sh\nls -la\n```\n1. "
    );
    // A flush with nothing to say still takes the unclosed fence.
    let mut agg = ClauseAggregator::new();
    assert!(agg.push_placed("```\ncode").is_empty());
    assert_eq!(agg.flush_placed(), None);
    assert_eq!(agg.taken(), "```\ncode".len());
}

mod scripts;
mod tables;
mod tags;
