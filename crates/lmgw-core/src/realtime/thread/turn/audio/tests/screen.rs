//! What the relay sees of an armed attempt (WP2 review #3–#5): an MCP
//! server's error frame does not disarm it, a crash on the audio is kept
//! with a note, and a refusal is kept for the model that refused. And
//! whether it carried the audio, said once (WP3 review #3).

use super::super::*;
use super::{armed, events_of, frame, upstream};

/// An MCP server that did not answer says so in an `error` frame with no
/// gateway error, before the model is asked (WP2 review #5): it does not
/// disarm the attempt. It is held back with what follows it — dropped when
/// the audio is refused (the retry says it again), relayed in order once
/// the model speaks, or when the turn ends for another reason.
#[test]
fn an_mcp_servers_error_frame_does_not_disarm_the_attempt() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mcp = || TurnFrame::new("error", r#"{"message":"MCP server 'kb' : down"}"#.into());
    let unheard = GatewayError::InvalidRequest {
        code: crate::web::chat_voice::bound::AUDIO_NOT_HEARD,
        message: "gpt does not take audio input".into(),
    };
    let mut a = armed(&tx);
    assert!(a.screen(mcp()).is_empty(), "held back");
    assert_eq!(events_of(&a.screen(frame("state"))), ["state"]);
    assert!(a.screen(TurnFrame::error(&unheard)).is_empty());
    assert!(a.refused.is_some(), "still armed: the refusal is retried");
    assert!(a.screen(frame("done")).is_empty());

    let mut a = armed(&tx);
    assert!(a.screen(mcp()).is_empty());
    assert!(a.screen(frame("usage")).is_empty(), "behind the held frame");
    assert_eq!(
        events_of(&a.screen(frame("delta"))),
        ["error", "usage", "delta"],
        "in order once the model speaks"
    );
    assert_eq!(a.screen(TurnFrame::error(&unheard)).len(), 1, "disarmed");
}

/// A llama-server going away under the audio (WP2 review #4): the turn's
/// own error, kept for the model that went away with a note; elsewhere a
/// dropped connection says nothing about the audio (decision D5): that turn
/// goes again as its transcript, and nothing is kept (review V4); and a
/// refusal is kept for the model the turn names as having answered.
#[tokio::test]
async fn a_crash_on_the_audio_is_kept_for_the_model_with_a_note() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let gone = GatewayError::Transport("connection reset by peer".into());
    // Through a cloud API: held for the transcript retry, and nothing kept.
    let mut a = armed(&tx);
    let cloud = crate::web::chat_voice::bound::SentAs::default();
    assert!(a.screen(TurnFrame::error_sent(&gone, cloud)).is_empty());
    assert_eq!(a.refused.as_ref().map(|r| r.kind), Some(Refusal::Dropped));
    assert!(Refusal::Dropped
        .why(&gone)
        .starts_with("the connection to the model dropped under the audio"));
    assert!(rx.try_recv().is_err(), "a cloud route's drop is not kept");
    let mut a = armed(&tx);
    let llama = crate::web::chat_voice::bound::SentAs {
        llama_server: true,
        ..Default::default()
    };
    let out = a.screen(TurnFrame::error_sent(&gone, llama));
    assert_eq!(events_of(&out), ["error"], "the response's error");
    match rx.try_recv() {
        Ok((1, Msg::Refused { refused, note })) => {
            assert_eq!(refused.model, "gemma");
            assert!(
                refused
                    .why
                    .starts_with("its server failed on the audio this session"),
                "{}",
                refused.why
            );
            assert!(note.unwrap().contains("later turns of this session"));
        }
        _ => panic!("kept"),
    }
    // A candidate alias's pick refused: kept for the pick, by its name.
    let mut a = armed(&tx);
    let sent = crate::web::chat_voice::bound::SentAs {
        answered_by: Some("gemma-b".into()),
        ..Default::default()
    };
    let refusal = upstream(400, "invalid audio part");
    assert!(a.screen(TurnFrame::error_sent(&refusal, sent)).is_empty());
    assert_eq!(a.refused.as_ref().unwrap().model, "gemma-b");
    // The thread's model when nothing answered in its place.
    let mut a = armed(&tx);
    assert!(a.screen(TurnFrame::error(&refusal)).is_empty());
    assert_eq!(a.refused.as_ref().unwrap().model, "gemma");
    // An image in the request: "Failed to load image or audio file" is no
    // refusal of the audio.
    let mut a = armed(&tx);
    let sent = crate::web::chat_voice::bound::SentAs {
        images: true,
        ..Default::default()
    };
    let media = upstream(500, "Failed to load image or audio file");
    assert_eq!(a.screen(TurnFrame::error_sent(&media, sent)).len(), 1);
    assert!(a.refused.is_none());
}

/// WP3 review #3: the attempt says once — to the journal with the
/// generation it began at, and to the core — whether it carried the audio:
/// at the model's first frame, or `false` when it was stopped (or refused,
/// or ended) before the model said anything.
#[test]
fn the_attempt_says_once_whether_it_carried_the_audio() {
    let said = |carried: bool| {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (jtx, mut jrx) = tokio::sync::mpsc::unbounded_channel();
        let (_row, watch) = tokio::sync::watch::channel(None);
        let mut a = armed(&tx);
        a.journal = Some(jtx);
        a.row = Some(watch);
        a.generation = Some(7);
        assert_eq!(events_of(&a.screen(frame("state"))), ["state"]);
        assert!(jrx.try_recv().is_err(), "a model state is no answer");
        if carried {
            a.screen(frame("delta"));
        } else {
            a.halted();
        }
        let began = match jrx.try_recv() {
            Ok(In::Began {
                gen: 1,
                generation: Some(7),
                carried,
            }) => carried,
            _ => panic!("the journal is told"),
        };
        assert!(matches!(rx.try_recv(), Ok((1, Msg::Carried(c))) if c == began));
        // Said once.
        a.halted();
        a.screen(frame("delta"));
        assert!(jrx.try_recv().is_err() && rx.try_recv().is_err());
        began
    };
    assert!(said(true), "the model answered from the audio");
    assert!(!said(false), "stopped before it said anything");
    // A response nothing was heard for says nothing.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut a = Attempt::new(None, 1, &tx, None);
    a.screen(frame("delta"));
    a.halted();
    assert!(rx.try_recv().is_err());
}
