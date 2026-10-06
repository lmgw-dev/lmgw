//! The seam a realtime session bound to a thread works through (chat-voice
//! design §8): the thread as stored now, what its voice resolves to, the
//! stages its connect warm loads — the Chat's own code, reached from
//! `realtime::thread`, so nothing of it is copied there.
//!
//! Every function takes the thread's id and dispatches on its sign, as the
//! Chat's routes do (`ChatRepo::of`): a temporary thread is bound like a
//! stored one.
//!
//! The turn is the Chat's turn (`chat_turn::start_turn_into`) and the
//! speech is the read-aloud's plan; the history writes are the journal's
//! (§8.3): the user message as a send writes it, a reply annotated, cut or
//! deleted — the last two only under the thread's conditional write.

use crate::config::Snapshot;
use crate::realtime::warm::Warm;
use crate::state::SharedState;
use crate::store::{ChatMessageRow, ChatThread, MessageVoice, SendMessageOutcome};

use super::super::agentchat::ADMIN_KIND;
use super::super::chat_repo::ChatRepo;

pub(crate) use super::resolve::{Source, VoiceConfig};

/// Thread `id` as stored now; `None` when there is no such thread (or the
/// store failed to say).
pub(crate) async fn thread(state: &SharedState, id: i64) -> Option<ChatThread> {
    ChatRepo::of(id).thread(state, id).await.ok().flatten()
}

/// [`thread`], telling a store that failed (`Err`) from a thread that is
/// not there (`Ok(None)`) — for a history write, which reports them apart.
pub(crate) async fn thread_checked(
    state: &SharedState,
    id: i64,
) -> Result<Option<ChatThread>, String> {
    ChatRepo::of(id)
        .thread(state, id)
        .await
        .map_err(|e| e.to_string())
}

/// An Admin Chat thread: realtime mode is refused for it (§8.1, ruling 7).
pub(crate) fn is_admin(thread: &ChatThread) -> bool {
    thread.kind == ADMIN_KIND
}

/// `thread` as a bound session says it (`session.lmgw.resolved.chat_thread`,
/// §8.1): its id, title, whether it is temporary, and whether its turns may
/// dispatch lmgw's admin tools — as the settings are now.
pub(crate) fn thread_ref(
    snap: &Snapshot,
    thread: &ChatThread,
) -> crate::realtime::protocol::ChatThreadRef {
    crate::realtime::protocol::ChatThreadRef {
        id: thread.id,
        title: thread.title.clone(),
        temporary: thread.id < 0,
        admin_tools: super::resolve::admin_tools(snap, thread),
    }
}

/// What `thread`'s voice resolves to (§2.3): the one resolution every Chat
/// voice route uses.
pub(crate) fn voice(snap: &Snapshot, thread: &ChatThread) -> VoiceConfig {
    super::resolve::resolve(snap, thread)
}

/// The stages a bound session's connect warm loads (§4.1): the thread's
/// ASR, chat model and TTS — each the press's own stage.
pub(crate) async fn connect_stages(state: &SharedState, thread: &ChatThread) -> Vec<Warm> {
    let snap = state.snapshot();
    super::warm::thread_stages(state, &snap, thread).await
}

// -- the turn ---------------------------------------------------------------

#[cfg(test)]
pub(crate) use super::super::chat_turn::SentAs;
pub(crate) use super::super::chat_turn::{
    RowWatch, TurnFrame, TurnLanguage, TurnOpts, UserRow, VoiceTurn, AUDIO_NOT_HEARD,
};

/// The languages of a bound turn of `thread` (§8.5, changed 2026-10-04,
/// split 2026-10-05): the reply's, which it is asked to answer in, and the
/// one the user speaks; every bound turn — the user spoke — `spoken` when
/// the session has audio output.
pub(crate) fn turn_language(
    snap: &Snapshot,
    thread: &ChatThread,
    spoken: bool,
) -> Option<super::super::chat_turn::TurnLanguage> {
    super::resolve::turn_language(snap, thread, spoken)
}
pub(crate) use super::speech::{Plan, Refusal};

pub(crate) use super::audio_input::{AudioInput, Refusals, Shown};

/// Whether `thread`'s next voice turn goes to its chat model as audio, with
/// the setting it was judged under (voice-audio-input design §2.2): the
/// bound session's verdict, judged at the bind and again before turns.
pub(crate) async fn audio_input(state: &SharedState, thread: &ChatThread) -> Shown {
    super::audio_input::shown(state, thread).await
}

/// Why `thread`'s heard turn goes as its transcript after all, by the
/// rows of the audio-input verdict the thread decides alone — the setting,
/// knowledge bases in auto mode — as they stand now (voice-audio-input
/// design §3.5); `None` when both pass.
pub(crate) fn audio_input_rows(snap: &Snapshot, thread: &ChatThread) -> Option<String> {
    super::audio_input::thread_rows(snap, thread)
}

/// How `thread` speaks (§6.1): the read-aloud's own plan — its TTS, voice,
/// style, language, announcements and seed (drawn on first use) — or why
/// it cannot.
pub(crate) async fn plan_speech(state: &SharedState, thread: &ChatThread) -> Result<Plan, Refusal> {
    super::speech::plan(state, ChatRepo::of(thread.id), thread).await
}

/// What a bound session's transcription goes by now (§2.3, §8.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AsrNow {
    /// The thread's ASR alias; `None` when it names none at any level.
    pub alias: Option<String>,
    /// The language the thread's user speaks; `None` for none (the ASR
    /// detects). The reply language is not the ASR's (§2.1, 2026-10-05).
    pub language: Option<String>,
}

/// The thread's ASR alias and language now, read afresh: a bound session's
/// turn is transcribed with what the thread names when it is committed, so
/// a chip changed since the bind applies from the next turn — and so does a
/// spoken language changed since, as the reply and the voice re-read the
/// reply language each turn (changed 2026-10-04). `None` when the thread is gone.
pub(crate) async fn asr_now(state: &SharedState, id: i64) -> Option<AsrNow> {
    let thread = thread(state, id).await?;
    let snap = state.snapshot();
    Some(AsrNow {
        alias: super::resolve::asr_alias(&snap, &thread),
        language: super::resolve::language(&snap.settings, &thread.voice).value,
    })
}

/// Whether opening `alias` has to start or load its model now (§4.3): a
/// local model on this machine, not up and loaded, and not held off it —
/// what a turn's `cold` stages name.
pub(crate) fn cold(state: &SharedState, alias: &str) -> bool {
    super::speech::cold(state, alias)
}

/// Start a voice turn of `thread` (§8.2): the Chat's own turn
/// (`start_turn_into`), answering the history as it stands — the user
/// message `user_message_id` the session just wrote, when it wrote one —
/// with the thread's capabilities computed for this turn, as a send does.
/// `Err`: why it was refused before it started.
pub(crate) async fn start_turn(
    state: &SharedState,
    thread: &ChatThread,
    user_message_id: Option<i64>,
    out: tokio::sync::mpsc::Sender<TurnFrame>,
    opts: TurnOpts,
) -> Result<(), String> {
    let repo = ChatRepo::of(thread.id);
    let caps = super::super::chat_attach_gate::thread_caps(state, repo, thread).await;
    let mode = super::super::chat_turn::TurnMode::Fresh { user_message_id };
    super::super::chat_turn::start_turn_into(state, repo, thread, mode, caps, out, opts)
        .await
        .map_err(|refused| {
            format!(
                "the chat turn was refused before it started ({})",
                refused.status()
            )
        })
}

// -- the history ------------------------------------------------------------

/// A user turn spoken in voice mode (§8.3): written as a send writes one —
/// a history write, which moves the thread's generation — with how it was
/// spoken; an untitled thread is named from it, as a send names one.
pub(crate) async fn write_user(
    state: &SharedState,
    thread: &ChatThread,
    content: &str,
    voice: &MessageVoice,
) -> Result<i64, String> {
    let repo = ChatRepo::of(thread.id);
    let id = match repo
        .append_user_message(state, thread.id, content, &[], &[], Some(voice))
        .await
    {
        Ok(SendMessageOutcome::Sent(id)) => id,
        Ok(SendMessageOutcome::AttachmentNotDraft) => {
            return Err("the user message was refused".into())
        }
        Err(e) => return Err(e.to_string()),
    };
    if thread.title == "New chat" || thread.title.trim().is_empty() {
        let title = super::super::chat::derive_title(content);
        let _ = repo.set_title(state, thread.id, &title).await;
    }
    Ok(id)
}

/// A heard voice turn's user row (voice-audio-input design §3.3): an
/// insert that moves no generation — as an annotation is an update that
/// moves none — under the thread's conditional write at `generation`, the
/// one the turn that heard it began at, so its reply can still be saved
/// after it; an untitled thread is named from it, as a send names one.
/// `Ok(None)`: the history moved since (another turn, an edit), and nothing
/// was written.
pub(crate) async fn append_spoken_user(
    state: &SharedState,
    thread: &ChatThread,
    generation: u64,
    content: &str,
    voice: &MessageVoice,
) -> Result<Option<i64>, String> {
    let Some(proof) = state.chat_live.write_if(thread.id, generation).await else {
        return Ok(None);
    };
    let repo = ChatRepo::of(thread.id);
    let id = match repo
        .append_spoken_user(state, &proof, thread.id, content, voice)
        .await
    {
        Ok(SendMessageOutcome::Sent(id)) => id,
        Ok(SendMessageOutcome::AttachmentNotDraft) => {
            return Err("the user message was refused".into())
        }
        Err(e) => return Err(e.to_string()),
    };
    drop(proof);
    if !content.is_empty() && (thread.title == "New chat" || thread.title.trim().is_empty()) {
        let title = super::super::chat::derive_title(content);
        let _ = repo.set_title(state, thread.id, &title).await;
    }
    Ok(Some(id))
}

/// Message `id` of thread `thread_id`, as stored now.
pub(crate) async fn message(
    state: &SharedState,
    thread_id: i64,
    id: i64,
) -> Result<Option<ChatMessageRow>, String> {
    ChatRepo::of(thread_id)
        .message(state, thread_id, id)
        .await
        .map_err(|e| e.to_string())
}

/// The text a reply's tool record holds — `None` for a reply with no tool
/// record (`chat_turn::has_tool_record`).
pub(crate) fn record_text(row: &ChatMessageRow) -> Option<String> {
    if !super::super::chat_turn::has_tool_record(row) {
        return None;
    }
    let record = row
        .ir_messages
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<crate::ir::Message>>(raw).ok())
        .unwrap_or_default();
    Some(super::super::chat_turn::record_said(&record))
}

/// Annotate a spoken reply heard whole (§8.3): its `voice` alone, no
/// generation move. `false` when it is gone.
pub(crate) async fn annotate(
    state: &SharedState,
    thread_id: i64,
    id: i64,
    voice: &MessageVoice,
) -> bool {
    ChatRepo::of(thread_id)
        .set_message_voice(state, thread_id, id, voice)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(
                thread = thread_id,
                "chat voice: the reply's voice was not written: {e}"
            );
            false
        })
}

/// What a conditional write (§8.3) came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Guarded {
    /// Written; `false` when the row was gone.
    Written(bool),
    /// Another turn started, or the history was rewritten, since the voice
    /// turn saved: nothing was written.
    Moved,
}

/// Cut a spoken reply to `content`, its `voice` with it, while the thread's
/// generation is still `generation` (`LiveTurns::write_if`).
pub(crate) async fn cut_if(
    state: &SharedState,
    thread_id: i64,
    generation: u64,
    id: i64,
    content: &str,
    voice: &MessageVoice,
) -> Guarded {
    let Some(proof) = state.chat_live.write_if(thread_id, generation).await else {
        return Guarded::Moved;
    };
    let done = ChatRepo::of(thread_id)
        .cut_reply(state, &proof, thread_id, id, content, voice)
        .await;
    Guarded::Written(done.unwrap_or_else(|e| {
        tracing::warn!(thread = thread_id, "chat voice: the reply was not cut: {e}");
        false
    }))
}

/// Delete a spoken reply nobody heard, while the thread's generation is
/// still `generation`.
pub(crate) async fn delete_if(
    state: &SharedState,
    thread_id: i64,
    generation: u64,
    id: i64,
) -> Guarded {
    let Some(proof) = state.chat_live.write_if(thread_id, generation).await else {
        return Guarded::Moved;
    };
    let done = ChatRepo::of(thread_id)
        .delete_unheard(state, &proof, thread_id, id)
        .await;
    Guarded::Written(done.unwrap_or_else(|e| {
        tracing::warn!(
            thread = thread_id,
            "chat voice: the reply was not deleted: {e}"
        );
        false
    }))
}
