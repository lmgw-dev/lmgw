//! The seam for the suite (`tests/it`): one heard response run to its end
//! by the bound responder, and what it sent its session.

use tokio::sync::mpsc;

use super::super::super::super::responder::Msg;
use super::{Launch, Spoken};
use crate::ir::ContentPart;
use crate::state::SharedState;
use crate::web::chat_voice::bound::UserRow;

/// What a heard response sent its session ([`heard_response_for_tests`]).
#[doc(hidden)]
#[derive(Debug)]
pub struct HeardForTests {
    /// The chat frames relayed (`lmgw.chat.frame`), in order.
    pub frames: Vec<(String, serde_json::Value)>,
    /// The notes that it goes as its transcript: why.
    pub notes: Vec<String>,
    /// What the session keeps (`Msg::Refused`): the model, the verdict's
    /// `why` for it from then on, and the note said with it, if any.
    pub remembered: Vec<(String, String, Option<String>)>,
    /// The reply's text, or the error's code.
    pub result: Result<String, String>,
}

/// Test-only (`tests/it`): one heard response of a session bound to thread
/// `thread_id` — text output, its new turn `spoken`, its user row written
/// as `row` (the journal's answer) — run to its end by the bound responder,
/// and what it sent the session. What lets the voice-audio-input suite
/// drive a managed server's refusal (`gpu_world`). `pub` (unconditional) to
/// match the other `*_for_tests` seams.
#[doc(hidden)]
pub async fn heard_response_for_tests(
    state: &SharedState,
    thread_id: i64,
    spoken: Vec<ContentPart>,
    row: i64,
) -> HeardForTests {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_stop, signal) = crate::proxy::stop_pair();
    let (user_tx, user) = tokio::sync::oneshot::channel();
    // A heard response's user entry answers no id (§3.3).
    let _ = user_tx.send(Ok(None));
    let (_row, user_row) = tokio::sync::watch::channel(Some(UserRow::Written(row)));
    super::super::run(super::super::Job {
        state: state.clone(),
        ctx: Default::default(),
        gen: 1,
        label: format!("realtime test of thread {thread_id}"),
        thread_id,
        tx,
        stop: signal,
        user,
        journal: None,
        speaking: None,
        hint: false,
        audio: Some(Launch {
            parts: spoken.into_iter().map(Spoken::Ready).collect(),
            user_row,
        }),
    })
    .await;
    let mut out = HeardForTests {
        frames: Vec::new(),
        notes: Vec::new(),
        remembered: Vec::new(),
        result: Err("no result".into()),
    };
    while let Ok((_, m)) = rx.try_recv() {
        match m {
            Msg::ChatFrame { event, data } => out.frames.push((event.to_string(), data)),
            Msg::Input { why, .. } => out.notes.push(why),
            Msg::Refused { refused, note } => {
                out.remembered.push((refused.model, refused.why, note))
            }
            Msg::Finished(r) => {
                out.result = match *r {
                    Ok(c) => Ok(c
                        .content
                        .iter()
                        .filter_map(|p| match p {
                            ContentPart::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect()),
                    Err(e) => Err(e.code().to_string()),
                }
            }
            _ => {}
        }
    }
    out
}
