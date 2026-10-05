//! The journal (chat-voice design §8.3): one FIFO task per bound session
//! that writes the spoken turns into the thread's history, in a fixed
//! order. Named so as not to be confused with `realtime::writer`, the
//! socket's writer.
//!
//! **Each response is two entries, queued together when it is launched**
//! ([`In::Response`]): the user message for its turns, then its reply slot.
//! - **User entry.** One user message for the response's turns that have
//!   none yet, with `voice.via = "realtime"`, written as a send writes one
//!   (a history write) and naming an untitled thread. Turn ids map to
//!   message ids, so turns owed again after a cut are never written twice.
//!   A response with no new words writes none. Its answer — the message's
//!   id — is the responder's barrier: the turn starts only once it came,
//!   so it comes after every earlier entry.
//! - **Reply slot.** It finalizes once both of its inputs are in: from the
//!   responder, what the turn saved ([`In::Saved`]: the reply's id and the
//!   generation the turn left behind — straight here, past the core's
//!   generation filter, so a cancelled response's save still lands); from
//!   the core, what was heard ([`In::Cut`], at drain or cancel, with the
//!   response's timing and served models).
//!
//! **A response that hears the user's audio** (voice-audio-input design
//! §3.3, `row`) has no user message before its turn: its turns are not
//! transcribed yet. Its user entry answers the barrier with no id once
//! every earlier entry is done, and its slot gains a first phase — the
//! **row**, written once the core said what was heard ([`In::Heard`]) and
//! the responder said its attempt's fate ([`In::Began`]: where it began,
//! and whether it carried the audio), before the reply is saved behind the
//! pre-save barrier. The inputs and how they land are `input`.
//!
//! **The order is fixed:** reply N saved → N finalized → user message N+1
//! → turn N+1 begins. A slot waiting for its inputs holds every entry
//! behind it, so N+1's history write can never refuse N's save.
//!
//! **Finalize** (`reply::decide`): nothing saved, no write; heard whole, the
//! reply's `voice` is annotated (no generation move); heard in part, cut;
//! heard none, deleted — never a reply with a tool record. A cut, a delete
//! and a re-cut are **conditional**: they proceed under the thread's lock
//! only while its generation is still the one the voice turn left behind
//! (`web::chat_live::LiveTurns::write_if`), and are skipped with a WARN line
//! and `lmgw.chat.reply {skipped}` otherwise — a voice finalize never
//! cancels a text turn of another window.
//!
//! **A late truncate** (the page's, a round trip after a barge-in) that
//! arrives after its slot finalized queues a **re-cut**, under the same
//! guard: cut from the reply as it was saved.
//!
//! **The drain** ([`Journal::drain`], §8.6): the session's core closes its
//! side when the session ends, and the task runs until every responder has
//! said what its turn saved and every entry is written.
//!
//! **A takeover** (§8.1, WP8 review m7): the session taken over drains
//! after the newer one is live, and may still write a user message. The
//! newer journal starts behind the older binding's fence, raised once that
//! session drained, so the two never interleave in one history.

use std::collections::{HashMap, VecDeque};

use tokio::sync::{mpsc, oneshot};

use super::super::protocol::{ErrorObject, ServerEvent};
use super::reply::Heard;
use crate::proxy::StopSignal;
use crate::state::SharedState;
use crate::store::{MessageVoice, ServedModel};

mod finalize;
mod input;
mod row;
mod user;

pub(crate) use input::{Ended, In, RowTx, UserTurn};

/// The journal's input: the core's and the responders'.
pub(crate) type Tx = mpsc::UnboundedSender<In>;

/// The journal's handle, the core's.
pub(crate) struct Journal {
    tx: Option<Tx>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Journal {
    /// Start the journal of a session bound to `thread_id`; its events go
    /// to `events`. `label`: what its log lines start with.
    /// `fence`: the session this one took over, raised once its journal
    /// drained — nothing is written before it (module doc).
    pub(crate) fn spawn(
        state: SharedState,
        thread_id: i64,
        label: String,
        events: mpsc::UnboundedSender<ServerEvent>,
        fence: Option<StopSignal>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = Task {
            state,
            thread_id,
            label,
            events,
            ops: VecDeque::new(),
            slots: HashMap::new(),
            done: HashMap::new(),
            written: HashMap::new(),
            unwritten: Vec::new(),
        };
        Self {
            tx: Some(tx),
            task: Some(tokio::spawn(task.run(rx, fence))),
        }
    }

    /// A sender for a responder (`In::Saved`).
    pub(crate) fn tx(&self) -> Option<Tx> {
        self.tx.clone()
    }

    pub(crate) fn send(&self, input: In) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(input);
        }
    }

    /// Close the core's side and wait until every entry is written (module
    /// doc).
    pub(crate) async fn drain(&mut self) {
        self.tx = None;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

/// What the head allows (`Task::step`).
enum Step {
    /// Slot `gen`'s deferred row.
    Row(u64),
    /// The head entry.
    Op,
}

/// What the task works through.
enum Op {
    User {
        turns: Vec<UserTurn>,
        /// The responder's barrier; `None` for the turns written as the
        /// session ends.
        reply: Option<oneshot::Sender<Result<Option<i64>, ErrorObject>>>,
        /// A heard response's: an empty barrier, its row deferred to its
        /// slot (module doc).
        deferred: bool,
    },
    Slot(u64),
    ReCut {
        gen: u64,
        heard: Heard,
    },
}

/// A reply slot's inputs, as they arrive.
struct Slot {
    response_id: String,
    saved: Option<(Option<i64>, Option<u64>)>,
    /// The chat model that answered, as the responder said it.
    chat: Option<ServedModel>,
    heard: Option<Heard>,
    ended: Option<Box<Ended>>,
    /// A heard response's deferred user row, until written (`row`).
    row: Option<row::Row>,
    /// Every turn it answered came back without words: nothing of it is
    /// written, and nothing said (`row`).
    vetoed: bool,
}

/// A finalized reply, for a re-cut.
struct Done {
    message_id: i64,
    generation: Option<u64>,
    /// The reply as the turn saved it.
    original: String,
    record: Option<String>,
    voice: MessageVoice,
    /// Deleted, or gone.
    gone: bool,
    /// A cut of it was skipped: another turn moved the thread on, and no
    /// re-cut can be written any more.
    settled: bool,
    /// A user message was written since: its generation moved, and its
    /// text was let go.
    moved: bool,
}

struct Task {
    state: SharedState,
    thread_id: i64,
    label: String,
    events: mpsc::UnboundedSender<ServerEvent>,
    ops: VecDeque<Op>,
    slots: HashMap<u64, Slot>,
    done: HashMap<u64, Done>,
    /// Turn id → the user message it was written in.
    written: HashMap<String, i64>,
    /// Turns whose user entry the store refused: they lead the next one.
    unwritten: Vec<UserTurn>,
}

impl Task {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<In>, fence: Option<StopSignal>) {
        if let Some(fence) = fence {
            // What arrives meanwhile waits in the channel, in order. Said at
            // INFO, both ends: the newer window's first turn waits for it
            // (WP11 binding review m1).
            if !fence.is_raised() {
                tracing::info!(
                    "{}: writes nothing to chat thread {} until the older window's session it \
                     took the thread over from has written what it holds (a reply's save and \
                     cut, its last transcripts)",
                    self.label,
                    self.thread_id
                );
                let waited = std::time::Instant::now();
                fence.raised().await;
                tracing::info!(
                    "{}: the older window's session is done with chat thread {} after {} ms",
                    self.label,
                    self.thread_id,
                    waited.elapsed().as_millis()
                );
            }
        }
        loop {
            while let Some(step) = self.step() {
                match step {
                    Step::Row(gen) => self.row(gen).await,
                    Step::Op => {
                        let op = self.ops.pop_front().expect("a step means queued");
                        self.process(op).await;
                    }
                }
            }
            match rx.recv().await {
                Some(input) => self.apply(input),
                None => break,
            }
        }
        // Every sender is gone — the core and every responder: what is
        // left is written with what it has.
        while let Some(op) = self.ops.pop_front() {
            self.process(op).await;
        }
        let lost: Vec<&str> = self
            .unwritten
            .iter()
            .map(|t| t.text.trim())
            .filter(|t| !t.is_empty())
            .collect();
        if !lost.is_empty() {
            tracing::warn!(
                "{}: the session ended with spoken words chat thread {} could not store: {:?}",
                self.label,
                self.thread_id,
                lost
            );
        }
    }

    /// What can be written now at the head, if anything: a heard slot's
    /// row (its first phase), or the head entry itself.
    fn step(&self) -> Option<Step> {
        match self.ops.front()? {
            Op::User { .. } | Op::ReCut { .. } => Some(Step::Op),
            Op::Slot(gen) => match self.slots.get(gen) {
                None => Some(Step::Op),
                Some(s) => match &s.row {
                    Some(r) => r.ready(s.saved.is_some()).then_some(Step::Row(*gen)),
                    None => (s.saved.is_some() && s.heard.is_some()).then_some(Step::Op),
                },
            },
        }
    }

    async fn process(&mut self, op: Op) {
        match op {
            Op::User {
                turns,
                reply,
                deferred,
            } => {
                // A heard response's barrier answers no id: its row is its
                // slot's (module doc).
                let answer = if deferred {
                    Ok(None)
                } else {
                    self.user(turns).await
                };
                if let Some(reply) = reply {
                    let _ = reply.send(answer);
                }
            }
            Op::Slot(gen) => {
                // At the drain a row may still be due: written with what is
                // known.
                if self.slots.get(&gen).is_some_and(|s| s.row.is_some()) {
                    self.row(gen).await;
                }
                if let Some(slot) = self.slots.remove(&gen) {
                    self.finalize(gen, slot).await;
                }
            }
            Op::ReCut { gen, heard } => self.recut(gen, heard).await,
        }
    }

    fn event(&self, ev: ServerEvent) {
        let _ = self.events.send(ev);
    }
}
