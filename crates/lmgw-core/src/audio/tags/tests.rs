use super::*;

fn vocab(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

fn omnivoice() -> Vec<String> {
    vocab(&["laughter", "sigh", "question-en", "surprise-ah"])
}

fn shaped(input: &str, mode: TagMode, v: &[String]) -> (String, TagCounts) {
    shape_tags(input, mode, v).expect("tags found")
}

#[test]
fn a_family_without_tags_has_them_stripped_and_the_text_rejoined() {
    let none = TagMode::None;
    assert_eq!(shaped("Hello [laughs] world.", none, &[]).0, "Hello world.");
    assert_eq!(shaped("Hello [laughs].", none, &[]).0, "Hello.");
    assert_eq!(shaped("[sighs] Fine, then.", none, &[]).0, "Fine, then.");
    assert_eq!(shaped("Fine, then. [sighs]", none, &[]).0, "Fine, then.");
    assert_eq!(
        shaped("Oh (laughs) really *sighs* yes <laugh> no.", none, &[]),
        (
            "Oh really yes no.".to_string(),
            TagCounts {
                stripped: 3,
                ..Default::default()
            }
        )
    );
}

#[test]
fn a_fixed_vocabulary_keeps_its_own_and_maps_stage_directions() {
    let (out, n) = shaped(
        "Ha [laughter] and (laughs) and [Laughing] and [cough].",
        TagMode::Fixed,
        &omnivoice(),
    );
    assert_eq!(out, "Ha [laughter] and [laughter] and [laughter] and.");
    assert_eq!(
        n,
        TagCounts {
            kept: 1,
            mapped: 2,
            stripped: 1
        }
    );
    // The family's own tags pass whatever they are named.
    assert_eq!(
        shaped("Really [question-en]", TagMode::Fixed, &omnivoice()).0,
        "Really [question-en]"
    );
}

#[test]
fn a_free_family_keeps_every_tag_in_the_bracket_form() {
    let (out, n) = shaped("Well [whispers] fine (sighs) ok.", TagMode::Free, &[]);
    assert_eq!(out, "Well [whispers] fine [sighs] ok.");
    assert_eq!(n.kept, 2);
}

#[test]
fn prose_markup_and_speaker_labels_are_not_tags() {
    for text in [
        "Hello (see above) world.",
        "A *really* good idea.",
        "<|speaker:0|>Hi there.",
        "[S1] Hello. [S2] Hi.",
        "Numbers [1] and [2a].",
        "- [x] done, [a] first, [b] second.",
        "No tags here.",
    ] {
        assert_eq!(shape_tags(text, TagMode::None, &[]), None, "{text}");
        assert!(!only_tags(text), "{text}");
    }
}

#[test]
fn nothing_but_tags_is_nothing_to_say() {
    assert!(only_tags("[laughs]"));
    assert!(only_tags("  [laughter] (sighs) . "));
    assert!(!only_tags("[laughs] Ha."));
    assert!(!only_tags("Ha!"), "no tags at all is not this check's");
    assert!(!only_tags(""));
}
