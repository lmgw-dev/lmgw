//! A skipped attempt — the thread's setting went off since the verdict —
//! whose row ends the response (WP2 review #6): it relays how it ended as a
//! retry does — a failed transcription's `error` and `done {aborted}`, a
//! row the journal could not write as `chat_history_write_failed` (WP3
//! review #8), a veto's or the stop's `done {aborted}` alone — and nothing
//! is sent.

use serde_json::json;

use super::{bodies, events, respond, respond_until, settles, world, Ending};
use crate::web::chat_voice::bound::UserRow;

#[tokio::test]
async fn a_skipped_attempt_relays_how_its_row_ended_the_response() {
    let audio = || {
        vec![crate::ir::ContentPart::Audio {
            mime: "audio/wav".into(),
            data: "UklGRg==".into(),
        }]
    };
    // A failed transcription: no words to send.
    let (state, mock, tid) = world("off").await;
    let said = respond(&state, tid, audio(), settles(UserRow::Failed)).await;
    assert_eq!(events(&said), ["error", "done"], "{:?}", said.frames);
    assert_eq!(said.frames[0].1["code"], "transcription_failed");
    assert_eq!(said.frames[1].1, json!({"aborted": true}));
    let e = said.result.unwrap().unwrap_err();
    assert_eq!(e.code(), "transcription_failed", "{e}");
    assert!(bodies(&mock).await.is_empty(), "nothing was sent");

    // A row the store refused: said as such, not as a journal gone.
    let said = respond(&state, tid, audio(), settles(UserRow::Unwritten)).await;
    assert_eq!(events(&said), ["error", "done"], "{:?}", said.frames);
    assert_eq!(said.frames[0].1["code"], "chat_history_write_failed");
    let e = said.result.unwrap().unwrap_err();
    assert_eq!(e.code(), "chat_history_write_failed", "{e}");

    // A veto: quiet.
    let said = respond(&state, tid, audio(), settles(UserRow::Veto)).await;
    assert_eq!(events(&said), ["done"], "no error: {:?}", said.frames);
    assert!(crate::proxy::is_canceled(
        &said.result.unwrap().unwrap_err()
    ));

    // The response's stop while it waits: quiet too.
    let said = respond_until(&state, tid, audio(), Ending::Stop).await;
    assert_eq!(events(&said), ["done"], "no error: {:?}", said.frames);
    assert!(crate::proxy::is_canceled(
        &said.result.unwrap().unwrap_err()
    ));
    assert!(bodies(&mock).await.is_empty());
    let rows = crate::store::list_chat_messages(&state.db, tid)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "nothing written");
}
