//! The substitution itself: every audio part and the spoken texts go, the
//! user row's words come, and a trailing user row's text a spoken text was
//! joined onto keeps its own words.

use super::*;
use crate::ir::{Message, Role};

fn audio() -> ContentPart {
    ContentPart::Audio {
        mime: "audio/wav".into(),
        data: "UklGRg==".into(),
    }
}

fn request(spoken: Vec<ContentPart>) -> ChatRequest {
    ChatRequest {
        model_alias: "m".into(),
        messages: vec![
            Message::text(Role::System, "Be brief."),
            Message {
                role: Role::User,
                content: spoken,
            },
            Message::text(Role::Assistant, "calling a tool"),
        ],
        params: Default::default(),
        tools: vec![],
        tool_choice: None,
        stream: true,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    }
}

#[test]
fn the_audio_gives_way_to_the_rows_words() {
    let ir = request(vec![audio()]);
    let out = substitute(&ir, 1, &[], Some("Wie spät ist es?"));
    assert_eq!(
        out.messages[1],
        Message::text(Role::User, "Wie spät ist es?")
    );
    assert_eq!(out.messages[2], ir.messages[2], "the loop's record stays");
    assert_eq!(out.messages[0], ir.messages[0]);
}

#[test]
fn a_spoken_text_joined_onto_an_owed_row_is_taken_off_its_end() {
    // `merge` joined the transcript turn onto the trailing user row's text.
    let ir = request(vec![
        ContentPart::text("Morgen früh muss ich\n\nzum Bäcker."),
        audio(),
    ]);
    let texts = ["zum Bäcker.".to_string()];
    let out = substitute(&ir, 1, &texts, Some("zum Bäcker. Und dann?"));
    assert_eq!(
        out.messages[1].content,
        vec![
            ContentPart::text("Morgen früh muss ich"),
            ContentPart::text("zum Bäcker. Und dann?"),
        ]
    );
    // A spoken text of its own goes whole.
    let ir = request(vec![ContentPart::text("zum Bäcker."), audio()]);
    let out = substitute(&ir, 1, &texts, Some("zum Bäcker. Und dann?"));
    assert_eq!(
        out.messages[1].content,
        vec![ContentPart::text("zum Bäcker. Und dann?")]
    );
}

#[test]
fn no_row_drops_the_spoken_parts_and_an_empty_message_says_so() {
    let ir = request(vec![ContentPart::text("Morgen früh muss ich"), audio()]);
    let out = substitute(&ir, 1, &[], None);
    assert_eq!(
        out.messages[1].content,
        vec![ContentPart::text("Morgen früh muss ich")]
    );
    let out = substitute(&request(vec![audio()]), 1, &[], None);
    assert_eq!(
        out.messages[1].content,
        vec![ContentPart::text(NOT_TRANSCRIBED)]
    );
}
