//! The Chat API's thread routes: creating a thread, reading one whole (its
//! messages, drafts and tasks), changing its settings, and the actions on a
//! thread and its messages — pin, archive, move, keep, delete, edit — and
//! the full-text search.
//!
//! The gateway builds its answers from these types, and the API document is
//! generated from them, so the two cannot drift. A reader of
//! `GET /chat/api/threads/{id}` deserializes [`ThreadDetail`]: it reads
//! leniently (a field a newer gateway adds is ignored), which is why the
//! gateway's own test compares what it sends with what this type writes
//! back, key by key.

use serde::{Deserialize, Serialize};

use crate::chat::{present, ContinueState, Thread, ThreadMcp, ThreadVoice};
use crate::chat::{MessageTask, ThreadTask};
use crate::chat_approvals::ApprovalRequest;

/// `POST /chat/api/threads`'s body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadCreate {
    /// The alias the thread's turns go to. In a folder, the folder's
    /// defaults may name the model instead.
    pub model_alias: String,
    /// `chat` (the default) or `admin`, a thread that drives the gateway's
    /// own configuration (not for a device key: 403 `forbidden`).
    pub kind: String,
    /// A temporary chat: kept in memory only, with a negative id, and gone
    /// at the gateway's next start unless it is kept
    /// (`POST /chat/api/threads/{id}/persist`). Always a `chat` thread,
    /// whatever `kind` says, and not with `folder_id`.
    pub temporary: bool,
    /// Create the thread in this folder, starting from the folder's
    /// defaults laid over the Chat's own.
    pub folder_id: Option<i64>,
}

/// `POST /chat/api/threads/{id}/pin`'s body.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PinRequest {
    /// Pin (`true`) or unpin the thread. Pinning an archived thread also
    /// restores it.
    pub pinned: bool,
}

/// `POST /chat/api/threads/{id}/archive`'s body.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArchiveRequest {
    /// Archive (`true`) the thread by hand, or restore it.
    pub archived: bool,
}

/// `POST /chat/api/threads/{id}/move`'s body.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MoveRequest {
    /// The target folder; `null` takes the thread out of any folder.
    pub folder_id: Option<i64>,
}

/// The `voice` of a settings patch in the document: the thread's own voice
/// settings, or `null` to clear them all.
#[cfg(feature = "schema")]
type VoicePatch = Option<Option<ThreadVoice>>;

/// `POST /chat/api/threads/{id}/settings`'s body: a **patch**. An absent
/// field leaves that setting alone; for the settings that can be unset,
/// `null` clears it back to the route's default. The thread's title is not
/// here: it names itself on the first send.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SettingsPatch {
    /// The alias the thread's turns go to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Sampling overrides sent with every turn: a number sets one, `null`
    /// clears it.
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub temperature: Option<Option<f64>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<Option<i64>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<Option<f64>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub top_k: Option<Option<i64>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub min_p: Option<Option<f64>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub repeat_penalty: Option<Option<f64>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<Option<f64>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<Option<f64>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub seed: Option<Option<i64>>,
    /// The whole list of stop sequences; `[]` clears it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    /// The tool servers the thread uses, as a whole list (`[]` detaches
    /// all). A device key's labels pass its tool scope first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_tools: Option<Vec<ThreadMcp>>,
    /// Reasoning overrides sent with every turn; `null` clears one.
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub reasoning_enabled: Option<Option<bool>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<Option<String>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub reasoning_budget: Option<Option<i64>>,
    /// The thread's knowledge bases, as a whole selection (`[]` clears it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kb_ids: Option<Vec<i64>>,
    /// `auto` or `tool`: how the bases reach the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kb_mode: Option<String>,
    /// The retrieval budget of `auto` mode in tokens; `null` takes the
    /// Chat's setting.
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub kb_budget_tokens: Option<Option<i64>>,
    /// The thread's voice overrides as a whole object; `null` clears them.
    /// An unknown key or value is a 400 `bad_request`.
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(with = "VoicePatch"))]
    pub voice: Option<serde_json::Value>,
    /// The personality profile, an id from `GET /chat/api/profiles`; `null`
    /// for none. An unknown id is a 400 `unknown_profile`.
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<Option<i64>>,
}

/// `POST /chat/api/threads/{id}/settings`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SettingsAck {
    /// Always `true`.
    pub ok: bool,
    /// Whether the last reply can be continued under the new settings (a
    /// model switch or a reasoning toggle changes it).
    #[serde(rename = "continue")]
    pub continue_state: ContinueState,
    /// The thread's own voice settings as stored now.
    pub voice: ThreadVoice,
    /// What the voice resolves to now, as `voice_resolved` of
    /// `GET /chat/api/threads/{id}`: an object with `asr` and `tts` (alias,
    /// whether it runs on this machine, what answers for it while the GPU
    /// is held), `voice`, `speech_style`, `language`, `reply_language`,
    /// `language_notes`, `read_aloud`, `turn_detection`, `audio_input`,
    /// `seed`, the `problems` that block a voice feature now, and
    /// `realtime`.
    #[cfg_attr(feature = "schema", schemars(extend("type" = "object")))]
    pub voice_resolved: serde_json::Value,
}

/// `POST /chat/api/threads/{id}/speech/stop`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SpeechStopped {
    /// Always `true`.
    pub ok: bool,
    /// How many read-alouds of the thread were running and are stopped; `0`
    /// when none ran.
    pub stopped: u64,
}

/// `POST /chat/api/threads/{id}/persist`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadKept {
    /// The stored thread's new, positive id.
    pub id: i64,
    /// The stored thread.
    pub thread: Thread,
}

/// `POST /chat/api/threads/{id}/messages/{mid}/edit`'s body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MessageEdit {
    /// The new text.
    pub content: String,
    /// A user message's knowledge bases for itself (`#`), replacing the
    /// ones it had; absent keeps them. Ignored for a reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kb_refs: Option<Vec<i64>>,
    /// Read the new answer aloud as it streams. Ignored for a reply, which
    /// is not answered again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub speak: bool,
}

/// `POST /chat/api/threads/{id}/messages/{mid}/edit`'s JSON answer, for a
/// reply: rewritten in place, nothing sent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReplyEdited {
    /// Always `true`.
    pub ok: bool,
    /// The reply as `GET /chat/api/threads/{id}` lists it.
    pub message: Message,
}

// ---------------------------------------------------------------------------
// The open thread
// ---------------------------------------------------------------------------

/// `GET /chat/api/threads/{id}`'s answer: the thread with its whole
/// history.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadDetail {
    /// The thread with `voice_resolved` and `continue`: whether its last
    /// reply can be continued.
    pub thread: Thread,
    /// Every message, oldest first.
    pub messages: Vec<Message>,
    /// Files attached to the next message, not sent yet.
    pub draft_attachments: Vec<AttachmentMeta>,
    /// The thread's MCP tasks still running, and the results waiting to
    /// enter it.
    pub tasks: Vec<ThreadTask>,
}

/// One stored message of a thread.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Message {
    pub id: i64,
    pub thread_id: i64,
    /// `user`, `assistant`, or `tool` (a late MCP task result).
    pub role: String,
    pub content: String,
    /// A reasoning model's thinking trace; empty otherwise.
    pub reasoning: String,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    /// An agentic turn's tool calls and results as JSON text; `null` for a
    /// plain chat turn.
    pub ir_messages: Option<String>,
    /// Knowledge bases picked with `#` for this user message only.
    pub kb_refs: Vec<i64>,
    /// The retrieval that ran for this user message; `null` when none ran.
    pub context: Option<MessageContext>,
    /// A reply's model: the alias its turn asked for. `null` on user
    /// messages and on replies saved before it was recorded.
    pub model: Option<String>,
    /// The alias that actually answered, when it was not `model` (a
    /// fallback); `null` when `model` itself answered.
    pub answered_by: Option<String>,
    /// For a reply that a fallback without sight answered: who, for which
    /// model, and what went to it in the images' place. Absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images_note: Option<String>,
    /// How the turn was spoken; `null` for a typed turn.
    pub voice: Option<MessageVoice>,
    /// On a late MCP task result (role `tool`): the task's facts. Absent
    /// otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<MessageTask>,
    /// When the message was written (UTC, `YYYY-MM-DD HH:MM:SS`).
    pub created_at: String,
    /// The files sent with this message.
    pub attachments: Vec<AttachmentMeta>,
    /// The calls a gated turn's reply still waits on
    /// (`POST /chat/api/threads/{id}/approvals` decides them). Absent when
    /// none wait.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_approvals: Option<Vec<ApprovalRequest>>,
}

/// What extraction found in an attachment (`AttachmentMeta::meta`): which
/// keys are there depends on the kind. A key a gateway version adds is kept
/// beside these, so a reader never loses it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AttachmentFacts {
    /// The extracted text's token estimate (text, PDF, office files and
    /// transcribed audio).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// A PDF's page count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
    /// A PDF's pages without a text layer, numbered from 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub textless: Option<Vec<u32>>,
    /// A PDF's class: `text`, `scanned` or `hybrid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(extend("enum" = ["text", "scanned", "hybrid"]))
    )]
    pub class: Option<String>,
    /// An office file's or audio clip's format, as sniffed (`docx`, `wav`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// How many parts an office file has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parts: Option<u64>,
    /// A spreadsheet's sheet names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheets: Option<Vec<String>>,
    /// The speech-to-text alias that wrote an audio clip's transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_alias: Option<String>,
    /// Why an audio clip's transcription failed, until a retry succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_error: Option<String>,
    /// Any other key.
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

/// One attachment's metadata, never its bytes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AttachmentMeta {
    pub id: i64,
    /// `image`, `pdf`, `text`, `audio`, ... as the upload was classified.
    pub kind: String,
    pub name: String,
    pub mime: String,
    /// Size in bytes.
    pub size: i64,
    /// A text-class PDF's choice, `text` or `images`; `null` when it does
    /// not apply or is not chosen yet.
    pub mode: Option<String>,
    /// What extraction found: pages, text-less pages, class, sheets,
    /// transcript alias or error, tokens. `{}` for an image.
    #[cfg_attr(feature = "schema", schemars(with = "AttachmentFacts"))]
    pub meta: serde_json::Value,
    /// The extracted text's token estimate; `null` when there is none.
    pub extracted_tokens: Option<i64>,
    /// For a draft: why it cannot be sent to the thread's current model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blockers: Option<Vec<String>>,
    /// For a draft: what it becomes on the way, sent all the same (an image
    /// as a placeholder, a PDF's pages as its text) under a fallback that
    /// cannot see.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hints: Option<Vec<String>>,
}

/// The retrieval that ran for one user message. Excerpt `n` in the
/// `<context>` block (and `[n]` in the answer) is `excerpts[n - 1]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MessageContext {
    pub excerpts: Vec<ContextExcerpt>,
    /// Tokens the excerpts use, heading paths included.
    pub tokens: u64,
    /// Excerpts that ranked but did not fit the budget.
    pub dropped: u64,
    /// The budget the retrieval ran with.
    pub budget_tokens: u64,
    /// Why something was not searched, or not fully: one sentence each.
    pub notes: Vec<String>,
    /// The bases that were searched, by name.
    pub searched: Vec<String>,
    /// The bases asked for: the thread's and the message's, as they were
    /// then.
    pub kb_ids: Vec<i64>,
    /// What was searched for.
    pub query: String,
    /// How long the retrieval took.
    pub ms: f64,
}

/// One retrieved excerpt, as a user message stores it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContextExcerpt {
    pub kb_id: i64,
    /// The base's name when it was retrieved.
    pub kb: String,
    pub file_id: i64,
    /// The file's name when it was retrieved.
    pub file: String,
    /// 1-based PDF page; `null` for other files.
    pub page: Option<i64>,
    pub chunk_id: String,
    pub heading_path: String,
    /// The chunk's text, verbatim.
    pub text: String,
    pub score: f32,
    pub tokens: u64,
    /// Byte range in the file's extracted text.
    pub span_start: i64,
    pub span_end: i64,
    /// The sha256 of the file version the chunk was cut from; empty for a
    /// context stored before it was kept.
    pub file_sha: String,
}

/// How a turn was spoken, on its message: `null` on the message for a typed
/// turn. On a user message: how it was dictated or spoken in realtime mode,
/// the speech-to-text alias that was asked and the one that answered, and
/// the audio's length. On a spoken reply: the text-to-speech alias, the
/// voice, the part that was never heard and the turn's timing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MessageVoice {
    /// `dictation` or `realtime`.
    pub via: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_answered_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts_answered_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// The reply's text after the heard part, shown behind a "not heard"
    /// marker. Not in `content`, so the model never sees it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unheard: Option<String>,
    /// On a user message: `audio` when the chat model heard the turn;
    /// absent on the transcript path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<InputPath>,
    /// On a user message the model heard: why its transcription failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing: Option<VoiceTiming>,
}

/// How a voice turn's words reached the chat model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum InputPath {
    /// The model heard the turn's audio.
    Audio,
    /// The model read the speech-to-text transcript.
    Transcript,
    /// A value this build does not know (a newer gateway's), as sent.
    #[serde(untagged)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unknown(String),
}

/// A spoken reply's timing: each stage measured from the one before,
/// `to_first_audio_ms` their sum from the end of speech, `cold` the stages
/// that loaded during the turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceTiming {
    pub response_id: Option<String>,
    pub message_id: Option<i64>,
    pub end_of_turn_ms: Option<u64>,
    pub asr_ms: Option<u64>,
    pub first_token_ms: Option<u64>,
    /// How long the chat model reasoned before its first token (part of
    /// `first_token_ms`); absent when it did not reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_ms: Option<u64>,
    pub first_clause_ms: Option<u64>,
    pub first_audio_ms: Option<u64>,
    pub total_ms: Option<u64>,
    pub to_first_audio_ms: Option<u64>,
    /// The stages that loaded during the turn.
    pub cold: Vec<String>,
    /// `announcement` when the first clause said was an announcement of a
    /// skipped block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_clause: Option<String>,
    pub models: VoiceModels,
    /// How the turn reached the chat model; absent with audio input off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<InputPath>,
    /// Why the transcript went where audio input is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_why: Option<String>,
    /// How long the first output was held for the turn's transcript; absent
    /// on the transcript path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_wait_ms: Option<u64>,
}

/// The models one spoken turn went through.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceModels {
    pub asr: Option<ServedModel>,
    pub chat: Option<ServedModel>,
    pub tts: Option<ServedModel>,
}

/// An alias a stage asked for, the one that answered when it was another,
/// and (for the text-to-speech stage) the voice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ServedModel {
    pub alias: String,
    pub answered_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// `GET /chat/api/search`'s answer: one page of matching threads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SearchPage {
    /// How many threads match in all.
    pub total_threads: i64,
    /// This page's threads, best match first.
    pub threads: Vec<SearchThread>,
    /// The `offset` of the next page; `null` on the last.
    pub next_offset: Option<i64>,
}

/// A thread with matches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SearchThread {
    pub thread_id: i64,
    pub title: String,
    /// `null` for a thread in no folder, and for one in a folder the
    /// caller cannot see.
    pub folder_id: Option<i64>,
    pub archived: bool,
    pub updated_at: String,
    /// How many places in the thread match.
    pub match_count: i64,
    /// The best matches, up to three.
    pub hits: Vec<SearchHit>,
}

/// One place that matches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SearchHit {
    /// `t` the title, `m` a message, `a` an attachment's name.
    pub kind: String,
    /// The message, for kind `m`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<i64>,
    /// The message's role, for kind `m`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The text around the match, with the match between U+E000 and U+E001
    /// (private-use characters; escape the text before turning them into
    /// markup).
    pub snippet: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_settings_patch_tells_absent_from_null() {
        let p: SettingsPatch =
            serde_json::from_value(json!({"temperature": null, "top_k": 4, "voice": null}))
                .unwrap();
        assert_eq!(p.temperature, Some(None));
        assert_eq!(p.top_k, Some(Some(4)));
        assert_eq!(p.voice, Some(serde_json::Value::Null));
        assert_eq!(p.seed, None);
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            json!({"temperature": null, "top_k": 4, "voice": null})
        );
    }

    #[test]
    fn an_unknown_input_path_is_kept_as_sent() {
        let v: MessageVoice =
            serde_json::from_value(json!({"via": "realtime", "input": "x"})).unwrap();
        assert_eq!(v.input, Some(InputPath::Unknown("x".into())));
        assert_eq!(serde_json::to_value(&v).unwrap()["input"], "x");
    }
}
