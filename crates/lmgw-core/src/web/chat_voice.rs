//! Chat voice, the server's half (chat-voice design): which speech models,
//! voice and style a thread resolves to ([`resolve`]), the checks a
//! thread's or a folder's voice overrides go through ([`settings`]), the
//! press's warm ([`warm`]), dictation ([`transcribe`]), a chat turn's
//! `state` frames ([`turn_state`]), and read-aloud ([`speech`]): of a
//! stored reply ([`speak`]) and of a turn as it streams ([`tee`]).
//!
//! The store's half — the types and their tolerant reads — is
//! [`crate::store::ThreadVoice`] / [`crate::store::MessageVoice`].
//!
//! A thread's speech and dictation rows carry the label its model turns
//! carry ([`speech_proto`]): `chat`, or `admin` for an Admin Chat thread.

use crate::ingress::ClientProto;
use crate::store::ChatThread;

use super::agentchat::ADMIN_KIND;

/// The label of `thread`'s speech and dictation rows (module doc): the one
/// its model turns carry (`agentchat`'s `ChatRunner`), so its voice traffic
/// is charged to the same internal identity — `internal:chat`, or
/// `internal:admin-chat`.
pub(super) fn speech_proto(thread: &ChatThread) -> ClientProto {
    if thread.kind == ADMIN_KIND {
        ClientProto::AdminChat
    } else {
        ClientProto::Chat
    }
}

/// Thread → Settings → Chat → Voice → realtime's settings, field by field,
/// with the facts the page shows beside each choice.
mod resolve;
pub(super) use resolve::{asr_alias, turn_language};

/// A thread's `voice` in its settings patch and a folder's `voice` default:
/// parsed strictly, normalised, the aliases checked.
mod settings;
pub(super) use settings::{apply_thread_voice, check_voice_aliases};

/// `POST /chat/api/threads/{id}/voice/warm`: the press warms the thread's
/// speech models through the request admission (§4.2), answered as `state`
/// frames.
mod warm;
pub(super) use warm::warm;

/// `POST /chat/api/threads/{id}/transcribe`: dictation (§5), bounded by
/// `max_body_mb` rather than axum's default.
mod transcribe;
pub(super) use transcribe::transcribe;

/// A chat turn's `state` frames around an admission that starts its model
/// (§4.3).
mod turn_state;
pub(super) use turn_state::{admit_reporting, held_at_resolve};

/// Read-aloud's engine (§6.1): realtime's speech pipeline without a writer,
/// for a thread's voice, answered as the Chat's speech frames.
mod speech;
pub(super) use speech::resolve_shown;

/// `POST /chat/api/threads/{id}/messages/{mid}/speak` and `POST
/// /chat/api/threads/{id}/speech/stop` (§6.3, §6.4).
mod speak;
pub(super) use speak::{speak, stop_speech};

/// The speech tee of a turn sent with `speak: true` (§6.4).
mod tee;
pub(super) use tee::{speaking_turn, ReadAloud};

/// What the two languages do at each stage, and the notes where a stage's
/// model does not take its language (§2.1).
mod language;
pub(super) use language::settings_notes as language_notes;

/// Whether a voice turn goes to the chat model as audio
/// (voice-audio-input design §2): the setting and the verdict.
mod audio_input;

/// A voice turn's system prompt (§8.5): the thread's, then the spoken-style
/// block and the tag hint.
mod prompt;
pub(super) use prompt::{turn_block, with_block};

/// The seam a realtime session bound to a thread works through (§8): the
/// thread, its voice, its turn and its history writes — the Chat's own
/// code, reached from `realtime::thread`.
pub(crate) mod bound;
