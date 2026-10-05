//! The turn's half of a heard response (voice-audio-input design §3.4,
//! §7): the request with spoken parts (merge, placeholder, `None` =
//! today's), the local-only refusal, and the pre-save barrier.

use std::collections::HashMap;
use std::time::Duration;

use super::super::build_messages;
use super::*;
use crate::ir::{Message, Role};
use crate::state::AppState;
use crate::store::{ChatThread, MessageVoice, VIA_REALTIME};

fn audio() -> ContentPart {
    ContentPart::Audio {
        mime: "audio/wav".into(),
        data: "UklGRg==".into(),
    }
}

fn row(id: i64, role: &str, content: &str, voice: Option<MessageVoice>) -> ChatMessageRow {
    ChatMessageRow {
        id,
        role: role.into(),
        content: content.into(),
        voice,
        ..Default::default()
    }
}

fn spoken_row(transcript_error: Option<&str>) -> Option<MessageVoice> {
    Some(MessageVoice {
        via: VIA_REALTIME.into(),
        transcript_error: transcript_error.map(str::to_string),
        ..Default::default()
    })
}

fn built(history: &[ChatMessageRow], spoken: Option<&[ContentPart]>) -> Vec<Message> {
    let thread = ChatThread {
        system_prompt: "Be brief.".into(),
        ..Default::default()
    };
    build_messages(&thread, "m", (history, spoken), &HashMap::new(), None)
}

#[test]
fn the_spoken_parts_follow_the_history_and_earlier_spoken_turns_go_as_text() {
    let history = [
        row(1, "user", "Wie spät ist es?", spoken_row(None)),
        row(2, "assistant", "Das weiß ich nicht.", None),
    ];
    let out = built(&history, Some(&[audio()]));
    assert_eq!(
        out,
        vec![
            Message::text(Role::System, "Be brief."),
            Message::text(Role::User, "Wie spät ist es?"),
            Message::text(Role::Assistant, "Das weiß ich nicht."),
            Message {
                role: Role::User,
                content: vec![audio()]
            },
        ],
        "the new turn alone is audio"
    );
}

#[test]
fn a_turn_owed_again_merges_with_the_new_audio_in_order() {
    let history = [
        row(1, "assistant", "Hallo!", None),
        row(2, "user", "Morgen früh muss ich", spoken_row(None)),
    ];
    // A transcript turn of the same response, then the heard one.
    let spoken = [ContentPart::text("zum Bäcker."), audio()];
    let out = built(&history, Some(&spoken));
    assert_eq!(out.len(), 3, "{out:?}");
    assert_eq!(
        out[2].content,
        vec![
            ContentPart::text("Morgen früh muss ich\n\nzum Bäcker."),
            audio()
        ]
    );
}

#[test]
fn a_spoken_row_with_no_words_and_a_failed_transcript_goes_as_the_placeholder() {
    let history = [
        row(1, "user", "", spoken_row(Some("the ASR timed out"))),
        row(2, "assistant", "Gern.", None),
        // Words, or no failure: as stored.
        row(3, "user", "Danke", spoken_row(Some("partly"))),
        row(4, "assistant", "", None),
        row(5, "user", "", spoken_row(None)),
    ];
    let out = built(&history, None);
    let texts: Vec<_> = out
        .iter()
        .map(|m| match &m.content[..] {
            [ContentPart::Text { text }] => text.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        texts,
        ["Be brief.", NOT_TRANSCRIBED, "Gern.", "Danke", "", ""],
        "{out:?}"
    );
    assert_eq!(
        row_text(&row(6, "assistant", "", spoken_row(Some("x")))),
        ""
    );
}

#[test]
fn without_spoken_parts_the_messages_are_the_historys() {
    let history = [
        row(1, "user", "hi", None),
        row(2, "assistant", "Hallo!", None),
        row(3, "user", "Und morgen?", spoken_row(None)),
    ];
    assert_eq!(
        built(&history, None),
        vec![
            Message::text(Role::System, "Be brief."),
            Message::text(Role::User, "hi"),
            Message::text(Role::Assistant, "Hallo!"),
            Message::text(Role::User, "Und morgen?"),
        ]
    );
    assert!(!hears(None));
    assert!(!hears(Some(&[ContentPart::text("zum Bäcker.")])));
    assert!(hears(Some(&[ContentPart::text("a"), audio()])));
}

#[tokio::test]
async fn audio_goes_only_to_a_route_this_lmgw_runs() {
    let state = AppState::init_for_tests().await.unwrap();
    let snap = state.snapshot();
    assert!(local_only(&snap.chat_local_route("gemma")).is_ok());
    let mut cloud = snap.chat_local_route("gpt");
    cloud.upstream.id = 7;
    cloud.upstream.name = "openai".into();
    let e = local_only(&cloud).unwrap_err();
    assert_eq!(e.code(), AUDIO_NOT_LOCAL);
    assert_eq!(e.http_status().as_u16(), 400);
    let msg = e.to_string();
    assert!(msg.contains("'gpt' (upstream 'openai')"), "{msg}");
    // An aux, audio or image route lmgw runs is local too; the gate never
    // sends a chat request to one.
    assert!(local_only(&snap.aux_local_route("embed")).is_ok());
}

/// The barrier, as the journal settles it.
async fn settled(answer: Option<UserRow>) -> Barrier {
    let (tx, rx) = watch::channel(None);
    let waiting = tokio::spawn(barrier(rx, std::future::pending()));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiting.is_finished(), "it waits for the journal");
    match answer {
        Some(a) => {
            tx.send_replace(Some(a));
        }
        None => drop(tx),
    }
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("settled")
        .unwrap()
}

#[tokio::test]
async fn the_barrier_passes_once_the_row_is_settled_and_never_on_a_veto() {
    assert_eq!(settled(Some(UserRow::Written(7))).await, Barrier::Passed);
    assert_eq!(settled(Some(UserRow::Failed)).await, Barrier::Passed);
    assert_eq!(settled(Some(UserRow::NoRow)).await, Barrier::Passed);
    assert_eq!(settled(Some(UserRow::Veto)).await, Barrier::Veto);
    assert_eq!(settled(None).await, Barrier::Lost, "the journal went");
    // Said before the turn asked: no wait.
    let (_tx, rx) = watch::channel(Some(UserRow::NoRow));
    assert_eq!(barrier(rx, std::future::pending()).await, Barrier::Passed);
    // A turn superseded meanwhile stops waiting; its own check refuses it.
    let (_tx, rx) = watch::channel(None);
    assert_eq!(barrier(rx, async {}).await, Barrier::Passed);
}
