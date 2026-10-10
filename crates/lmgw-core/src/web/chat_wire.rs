//! The Chat API's bodies as typed DTOs (client-apps design §4.3): the
//! store's threads and folders turned into `lmgw-api-types::chat`'s
//! [`ThreadRow`], [`Thread`] and [`Folder`] — the types the API document is
//! generated from, so the document and the answers cannot drift.
//!
//! **Every field, by name.** Each conversion destructures the store's
//! struct without `..`: a column added to a thread, a folder or their
//! defaults does not compile until it is on the wire (or deliberately left
//! off it here).
//!
//! **The bytes the API always sent.** The Chat's bodies were built as
//! `serde_json::Value`s, whose maps keep their keys sorted; [`wire`] writes
//! a DTO the same way, so the answers are byte for byte what they were
//! (pinned in this module's tests).

use std::collections::HashMap;

use lmgw_api_types::chat as api;
use serde::Serialize;
use serde_json::Value;

use super::chat_folders::retention::PurgeDays;
use super::chat_repo::ChatRepo;
use crate::state::SharedState;
use crate::store::{self, ChatFolderListed, ChatThread};

mod messages;
pub(crate) use messages::{attachment, message, message_context};

#[cfg(test)]
mod tests;

/// A Chat DTO as the API writes it: through a `Value`, whose map keeps its
/// keys sorted at every depth (module doc).
pub(super) fn wire<T: Serialize>(dto: &T) -> Value {
    serde_json::to_value(dto).expect("a Chat DTO always serializes")
}

/// A local model's timings as the `stats` and `done` frames carry them.
pub(crate) fn timings(t: &crate::ir::Timings) -> lmgw_api_types::chat_frames::Timings {
    let crate::ir::Timings {
        prompt_n,
        prompt_ms,
        prompt_per_second,
        predicted_n,
        predicted_ms,
        predicted_per_second,
        cache_n,
        draft_n,
        draft_n_accepted,
    } = *t;
    lmgw_api_types::chat_frames::Timings {
        prompt_n,
        prompt_ms,
        prompt_per_second,
        predicted_n,
        predicted_ms,
        predicted_per_second,
        cache_n,
        draft_n,
        draft_n_accepted,
    }
}

/// When each of `threads`' newest message was written, in unix seconds
/// (`last_message_at`): one query for the stored ones, the messages held in
/// memory for the temporary ones. A thread without messages is not in the
/// map; a query that fails leaves every stored one out, with a log line.
pub(crate) async fn last_messages(
    state: &crate::state::AppState,
    threads: &[&ChatThread],
) -> HashMap<i64, i64> {
    let (temporary, stored): (Vec<&ChatThread>, Vec<&ChatThread>) =
        threads.iter().partition(|t| ChatRepo::of(t.id).is_temp());
    let ids: Vec<i64> = stored.iter().map(|t| t.id).collect();
    let mut out = match store::chat_last_message_at(&state.db, &ids).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("chat: the threads' newest messages could not be read: {e}");
            HashMap::new()
        }
    };
    for t in temporary {
        let newest = state
            .chat_temp
            .messages(t.id)
            .iter()
            .filter_map(|m| unix_seconds(&m.created_at))
            .max();
        if let Some(at) = newest {
            out.insert(t.id, at);
        }
    }
    out
}

/// `YYYY-MM-DD HH:MM:SS` (UTC, as the store writes it) in unix seconds.
fn unix_seconds(at: &str) -> Option<i64> {
    chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|t| t.and_utc().timestamp())
}

/// `t` as the thread list carries it: `purge_at` (when an archived thread
/// is deleted: `archived_at` plus its folder's purge days, else
/// `chat_purge_days`; computed here, so a changed setting reprices every
/// archived thread at once), `temporary`, and `last_message_at` (from
/// [`last_messages`]), beside its columns.
pub(crate) fn thread_row(
    t: &ChatThread,
    purge: &PurgeDays,
    last_message_at: Option<i64>,
) -> api::ThreadRow {
    let purge_days = purge.of(t);
    let purge_at = (!t.pinned && purge_days > 0)
        .then_some(t.archived_at.as_deref())
        .flatten()
        .and_then(|a| store::chat_thread_purge_at(a, purge_days));
    let ChatThread {
        id,
        title,
        model_alias,
        system_prompt,
        temperature,
        max_tokens,
        top_p,
        top_k,
        min_p,
        repeat_penalty,
        presence_penalty,
        frequency_penalty,
        seed,
        stop,
        kind,
        mcp_tools,
        reasoning_enabled,
        reasoning_effort,
        reasoning_budget,
        agent_id,
        pinned,
        archived_at,
        folder_id,
        profile_id,
        kb_ids,
        kb_mode,
        kb_budget_tokens,
        voice,
        created_at,
        updated_at,
        // Internal to the approval check (client-apps design §6.6).
        approval_floor: _,
    } = t.clone();
    api::ThreadRow {
        id,
        title,
        model_alias,
        system_prompt,
        temperature,
        max_tokens,
        top_p,
        top_k,
        min_p,
        repeat_penalty,
        presence_penalty,
        frequency_penalty,
        seed,
        stop,
        kind,
        mcp_tools: mcp_tools.iter().map(thread_mcp).collect(),
        reasoning_enabled,
        reasoning_effort,
        reasoning_budget,
        agent_id,
        pinned,
        archived_at,
        folder_id,
        profile_id,
        kb_ids,
        kb_mode: kb_mode_of(kb_mode),
        kb_budget_tokens,
        voice: thread_voice(&voice),
        created_at,
        updated_at,
        last_message_at,
        purge_at,
        temporary: ChatRepo::of(id).is_temp(),
    }
}

/// `t` as an open thread shows it: its row and `voice_resolved`, what its
/// voice resolves to now (chat-voice design §2.3). `continue` is the
/// caller's to add where the thread is read whole.
pub(crate) async fn thread(state: &SharedState, t: &ChatThread) -> api::Thread {
    let last = last_messages(state, &[t]).await.get(&t.id).copied();
    api::Thread {
        row: thread_row(t, &PurgeDays::load(state).await, last),
        voice_resolved: serde_json::to_value(super::chat_voice::resolve_shown(state, t).await)
            .expect("a resolved voice always serializes"),
        continue_state: None,
    }
}

/// A listed folder as the API carries it.
pub(crate) fn folder(f: &ChatFolderListed) -> api::Folder {
    let ChatFolderListed {
        folder,
        threads_active,
        threads_archived,
    } = f;
    let store::ChatFolder {
        id,
        name,
        sort,
        defaults,
        ongoing,
        archive_days,
        purge_days,
        created_at,
        updated_at,
        // The owner's list says it (review F-7); a device never sees such a
        // folder.
        devices_hidden,
        // Internal to the approval check (client-apps design §6.6).
        approval_floor: _,
    } = folder.clone();
    api::Folder {
        id,
        name,
        sort,
        defaults: thread_defaults(&defaults),
        ongoing,
        archive_days,
        purge_days,
        devices_hidden,
        created_at,
        updated_at,
        threads_active: *threads_active,
        threads_archived: *threads_archived,
    }
}

fn thread_mcp(m: &store::ThreadMcp) -> api::ThreadMcp {
    let store::ThreadMcp {
        server_label,
        allowed_tools,
        require_approval,
    } = m.clone();
    api::ThreadMcp {
        server_label,
        allowed_tools,
        require_approval,
    }
}

/// A thread's tool server as a request wrote it, in the store's form: the
/// label read trimmed, as the store reads it from a stored row.
pub(crate) fn store_thread_mcp(m: api::ThreadMcp) -> store::ThreadMcp {
    let api::ThreadMcp {
        server_label,
        allowed_tools,
        require_approval,
    } = m;
    store::ThreadMcp {
        server_label: server_label.trim().to_string(),
        allowed_tools,
        require_approval,
    }
}

fn kb_mode_of(m: store::KbMode) -> api::KbMode {
    match m {
        store::KbMode::Auto => api::KbMode::Auto,
        store::KbMode::Tool => api::KbMode::Tool,
    }
}

fn turn_detection_of(t: store::TurnDetection) -> api::TurnDetectionMode {
    match t {
        store::TurnDetection::SemanticVad => api::TurnDetectionMode::SemanticVad,
        store::TurnDetection::ServerVad => api::TurnDetectionMode::ServerVad,
        store::TurnDetection::PushToTalk => api::TurnDetectionMode::PushToTalk,
    }
}

fn audio_input_of(m: store::AudioInputMode) -> api::AudioInputMode {
    match m {
        store::AudioInputMode::Off => api::AudioInputMode::Off,
        store::AudioInputMode::On => api::AudioInputMode::On,
    }
}

pub(crate) fn thread_voice(v: &store::ThreadVoice) -> api::ThreadVoice {
    let store::ThreadVoice {
        asr_alias,
        tts_alias,
        voice,
        language,
        reply_language,
        read_aloud,
        turn_detection,
        audio_input,
        speech_style,
        seed,
    } = v.clone();
    api::ThreadVoice {
        asr_alias,
        tts_alias,
        voice,
        language,
        reply_language,
        read_aloud,
        turn_detection: turn_detection.map(turn_detection_of),
        audio_input: audio_input.map(audio_input_of),
        speech_style,
        seed,
    }
}

fn thread_defaults(d: &store::ThreadDefaults) -> api::ThreadDefaults {
    let store::ThreadDefaults {
        model_alias,
        system_prompt,
        temperature,
        max_tokens,
        top_p,
        top_k,
        min_p,
        repeat_penalty,
        presence_penalty,
        frequency_penalty,
        seed,
        stop,
        reasoning_enabled,
        reasoning_effort,
        reasoning_budget,
        mcp_tools,
        kb_ids,
        kb_mode,
        kb_budget_tokens,
        voice,
        profile_id,
    } = d.clone();
    api::ThreadDefaults {
        model_alias,
        system_prompt,
        temperature,
        max_tokens,
        top_p,
        top_k,
        min_p,
        repeat_penalty,
        presence_penalty,
        frequency_penalty,
        seed,
        stop,
        reasoning_enabled,
        reasoning_effort,
        reasoning_budget,
        mcp_tools: mcp_tools.map(|m| m.iter().map(thread_mcp).collect()),
        kb_ids,
        kb_mode: kb_mode.map(kb_mode_of),
        kb_budget_tokens,
        voice: voice.as_ref().map(thread_voice),
        profile_id,
    }
}
