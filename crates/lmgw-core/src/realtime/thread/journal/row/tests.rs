//! A heard response's row (voice-audio-input design §3.3), the journal
//! driven directly: the row before the reply, the history write when the
//! history moved, the veto, the failed marker, a turn no model heard, a
//! row the store refused, and the title.

use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use super::super::{Ended, In, Journal, UserTurn};
use crate::realtime::protocol::ServerEvent;
use crate::realtime::thread::reply::Heard;
use crate::state::{AppState, SharedState};
use crate::store::{self, InputPath};
use crate::web::chat_voice::bound::UserRow;

struct Rig {
    state: SharedState,
    tid: i64,
    journal: Journal,
    events: mpsc::UnboundedReceiver<ServerEvent>,
}

async fn rig() -> Rig {
    let state = AppState::init_for_tests().await.unwrap();
    let tid = store::create_chat_thread(&state.db, "m", "chat")
        .await
        .unwrap();
    let (tx, events) = mpsc::unbounded_channel();
    let journal = Journal::spawn(
        state.clone(),
        tid,
        "realtime test".into(),
        tx,
        None,
        Default::default(),
    );
    Rig {
        state,
        tid,
        journal,
        events,
    }
}

fn turn(text: &str, error: Option<&str>) -> UserTurn {
    UserTurn {
        item_id: format!("item_{text}"),
        text: text.into(),
        asr: None,
        error: error.map(str::to_string),
        heard: true,
    }
}

impl Rig {
    /// Response `gen` launched hearing a turn: its barrier's answer, and its
    /// pre-save barrier.
    async fn launch(&self, gen: u64) -> (Option<i64>, watch::Receiver<Option<UserRow>>) {
        let (reply, user) = oneshot::channel();
        let (row, watch) = watch::channel(None);
        self.journal.send(In::Response {
            gen,
            response_id: format!("resp_{gen}"),
            turns: vec![turn("", None)],
            reply,
            row: Some(row),
        });
        let id = tokio::time::timeout(Duration::from_secs(5), user)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .map(|w| w.id);
        (id, watch)
    }

    /// The responder says response `gen`'s attempt began at `generation`
    /// and whether it `carried` the audio.
    fn began(&self, gen: u64, generation: Option<u64>, carried: bool) {
        self.journal.send(In::Began {
            gen,
            generation,
            carried,
        });
    }

    /// The barrier's answer, once it said.
    async fn row(&self, mut w: watch::Receiver<Option<UserRow>>) -> Option<UserRow> {
        let said = tokio::time::timeout(Duration::from_secs(5), w.wait_for(Option::is_some))
            .await
            .unwrap();
        said.ok().and_then(|r| *r)
    }

    /// The response's turn saved `reply` (or nothing) at `generation`, and
    /// was heard whole.
    async fn end(&self, gen: u64, reply: Option<&str>, generation: Option<u64>) {
        let message_id = match reply {
            Some(text) => Some(
                store::append_chat_message(
                    &self.state.db,
                    self.tid,
                    "assistant",
                    text,
                    "",
                    None,
                    None,
                    None,
                )
                .await
                .unwrap(),
            ),
            None => None,
        };
        self.journal.send(In::Saved {
            gen,
            message_id,
            generation,
            chat: None,
        });
        self.journal.send(In::Cut {
            gen,
            heard: Heard::Whole,
            ended: Some(Box::new(Ended::default())),
        });
    }

    async fn drain(mut self) -> (Vec<store::ChatMessageRow>, Vec<ServerEvent>) {
        self.journal.drain().await;
        let rows = store::list_chat_messages(&self.state.db, self.tid)
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Ok(e) = self.events.try_recv() {
            events.push(e);
        }
        (rows, events)
    }
}

#[tokio::test]
async fn the_row_is_written_once_heard_and_the_reply_follows_it() {
    let r = rig().await;
    let (id, w) = r.launch(1).await;
    assert_eq!(id, None, "a heard response's barrier answers no id");
    // The turn begins, as a text turn would.
    let ticket = r.state.chat_live.begin(r.tid).await;
    r.began(1, Some(ticket.generation()), true);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(*w.borrow(), None, "not before the transcript");
    r.journal.send(In::Heard {
        gen: 1,
        turns: vec![turn("Wie spät ist es?", None)],
        veto: false,
    });
    let Some(UserRow::Written(user_id)) = r.row(w).await else {
        panic!("no row");
    };
    // The insert moved no generation: the reply can still be saved.
    assert!(
        ticket.save_lock().await.is_some(),
        "the reply is still saved"
    );
    r.end(1, Some("Keine Ahnung."), Some(ticket.generation()))
        .await;
    drop(ticket);
    let (rows, events) = r.drain().await;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0].id, rows[0].content.as_str()),
        (user_id, "Wie spät ist es?")
    );
    assert!(rows[0].id < rows[1].id);
    let voice = rows[0].voice.as_ref().unwrap();
    assert_eq!(voice.input, Some(InputPath::Audio));
    assert_eq!(voice.transcript_error, None);
    let user = events.iter().find_map(|e| match e {
        ServerEvent::LmgwChatUser { response_id, .. } => Some(response_id.clone()),
        _ => None,
    });
    assert_eq!(user, Some(Some("resp_1".to_string())));
    // The untitled thread is named, and the session told.
    let named = events.iter().find_map(|e| match e {
        ServerEvent::LmgwChatThread { chat_thread } => Some(chat_thread.title.clone()),
        _ => None,
    });
    assert_eq!(named.as_deref(), Some("Wie spät ist es?"));
}

#[tokio::test]
async fn a_history_that_moved_meanwhile_gets_the_row_as_a_history_write() {
    let r = rig().await;
    let (_, w) = r.launch(1).await;
    let ticket = r.state.chat_live.begin(r.tid).await;
    r.began(1, Some(ticket.generation()), true);
    // Another window's edit moves the history.
    drop(r.state.chat_live.write(r.tid).await);
    r.journal.send(In::Heard {
        gen: 1,
        turns: vec![turn("Hallo", None)],
        veto: false,
    });
    assert!(matches!(r.row(w).await, Some(UserRow::Written(_))));
    assert!(ticket.save_lock().await.is_none(), "the reply is not saved");
    r.end(1, None, Some(ticket.generation())).await;
    drop(ticket);
    let (rows, _) = r.drain().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content, "Hallo");
}

#[tokio::test]
async fn an_attempt_that_never_began_gets_its_row_as_a_history_write() {
    // The skipped attempt (WP2 review): it says no turn began, then waits.
    let r = rig().await;
    let (_, w) = r.launch(1).await;
    r.began(1, None, false);
    r.journal.send(In::Heard {
        gen: 1,
        turns: vec![turn("Hallo", None)],
        veto: false,
    });
    assert!(matches!(r.row(w).await, Some(UserRow::Written(_))));
    r.end(1, None, None).await;
    let (rows, _) = r.drain().await;
    assert_eq!(rows.len(), 1);
    // A skipped attempt heard nothing: its row is a transcript turn's.
    assert_eq!(rows[0].voice.as_ref().unwrap().input, None);
}

#[tokio::test]
async fn a_veto_writes_nothing_and_says_nothing() {
    let r = rig().await;
    let (_, w) = r.launch(1).await;
    r.began(1, Some(1), true);
    r.journal.send(In::Heard {
        gen: 1,
        turns: vec![turn("", None)],
        veto: true,
    });
    assert_eq!(r.row(w).await, Some(UserRow::Veto));
    r.end(1, None, None).await;
    let (rows, events) = r.drain().await;
    assert!(rows.is_empty());
    assert!(events.is_empty(), "{events:?}");
}

/// The marked row is the insert under the turn's own generation, not the
/// history write (WP3 review #10): the reply is saved after it.
#[tokio::test]
async fn a_failed_transcription_writes_the_marked_row_and_no_words_is_no_row() {
    let r = rig().await;
    let (_, w) = r.launch(1).await;
    let ticket = r.state.chat_live.begin(r.tid).await;
    r.began(1, Some(ticket.generation()), true);
    r.journal.send(In::Heard {
        gen: 1,
        turns: vec![turn("", Some("engine fell over"))],
        veto: false,
    });
    assert_eq!(r.row(w).await, Some(UserRow::Failed));
    assert!(
        ticket.save_lock().await.is_some(),
        "an insert that moved no generation: the reply is still saved"
    );
    r.end(1, Some("Wie bitte?"), Some(ticket.generation()))
        .await;
    drop(ticket);
    // No words and no failure (a turn owed again had them): no row.
    let (_, w) = r.launch(2).await;
    let ticket = r.state.chat_live.begin(r.tid).await;
    r.began(2, Some(ticket.generation()), true);
    r.journal.send(In::Heard {
        gen: 2,
        turns: vec![turn("", None)],
        veto: false,
    });
    assert_eq!(r.row(w).await, Some(UserRow::NoRow));
    r.end(2, None, Some(ticket.generation())).await;
    drop(ticket);
    let (rows, _) = r.drain().await;
    let shape: Vec<(&str, &str)> = rows
        .iter()
        .map(|m| (m.role.as_str(), m.content.as_str()))
        .collect();
    assert_eq!(shape, [("user", ""), ("assistant", "Wie bitte?")]);
    let voice = rows[0].voice.as_ref().unwrap();
    assert_eq!(voice.transcript_error.as_deref(), Some("engine fell over"));
    assert_eq!(voice.input, Some(InputPath::Audio));
}

/// WP3 review #3: a turn whose attempt did not carry the audio — refused,
/// or ended before the model said anything — was heard by no model. Its
/// row is a transcript turn's: no `input`, and a failed transcription is no
/// "heard, not transcribed" row.
#[tokio::test]
async fn a_turn_no_model_heard_is_no_heard_row() {
    let r = rig().await;
    let (_, w) = r.launch(1).await;
    let ticket = r.state.chat_live.begin(r.tid).await;
    r.began(1, Some(ticket.generation()), false);
    r.journal.send(In::Heard {
        gen: 1,
        turns: vec![turn("Hallo", None)],
        veto: false,
    });
    assert!(matches!(r.row(w).await, Some(UserRow::Written(_))));
    r.end(1, None, Some(ticket.generation())).await;
    drop(ticket);
    let (_, w) = r.launch(2).await;
    r.began(2, None, false);
    r.journal.send(In::Heard {
        gen: 2,
        turns: vec![turn("", Some("engine fell over"))],
        veto: false,
    });
    assert_eq!(r.row(w).await, Some(UserRow::NoRow), "no heard failure");
    r.end(2, None, None).await;
    let (rows, _) = r.drain().await;
    assert_eq!(rows.len(), 1);
    let voice = rows[0].voice.as_ref().unwrap();
    assert_eq!(
        (voice.input, voice.transcript_error.as_deref()),
        (None, None)
    );
}

/// WP3 review #8: a row the store refused is said as such — the barrier's
/// `unwritten`, not a journal gone — and its words lead the next entry; a
/// thread that went is `unwritten` too.
#[tokio::test]
async fn a_row_the_store_refused_is_unwritten_and_its_words_are_kept() {
    let r = rig().await;
    sqlx::query(
        "CREATE TRIGGER refuse_user BEFORE INSERT ON chat_messages WHEN NEW.role = 'user' \
         BEGIN SELECT RAISE(ABORT, 'the disk is full'); END",
    )
    .execute(&r.state.db)
    .await
    .unwrap();
    let (_, w) = r.launch(1).await;
    let ticket = r.state.chat_live.begin(r.tid).await;
    r.began(1, Some(ticket.generation()), true);
    r.journal.send(In::Heard {
        gen: 1,
        turns: vec![turn("Erste Frage.", None)],
        veto: false,
    });
    assert_eq!(r.row(w).await, Some(UserRow::Unwritten));
    r.end(1, None, Some(ticket.generation())).await;
    drop(ticket);
    sqlx::query("DROP TRIGGER refuse_user")
        .execute(&r.state.db)
        .await
        .unwrap();
    // The next heard row leads with the words the store refused.
    let (_, w) = r.launch(2).await;
    let ticket = r.state.chat_live.begin(r.tid).await;
    r.began(2, Some(ticket.generation()), true);
    r.journal.send(In::Heard {
        gen: 2,
        turns: vec![turn("Zweite Frage.", None)],
        veto: false,
    });
    assert!(matches!(r.row(w).await, Some(UserRow::Written(_))));
    r.end(2, None, Some(ticket.generation())).await;
    drop(ticket);
    let rows = store::list_chat_messages(&r.state.db, r.tid).await.unwrap();
    let texts: Vec<&str> = rows.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(texts, ["Erste Frage.\nZweite Frage."]);
    // A thread that went.
    store::delete_chat_thread(&r.state.db, r.tid, None)
        .await
        .unwrap();
    let (_, w) = r.launch(3).await;
    r.began(3, None, true);
    r.journal.send(In::Heard {
        gen: 3,
        turns: vec![turn("Dritte Frage.", None)],
        veto: false,
    });
    assert_eq!(r.row(w).await, Some(UserRow::Unwritten));
    r.end(3, None, None).await;
    r.drain().await;
}
