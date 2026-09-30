//! What a model can take of the attachments (chat-complete design §8): its
//! capabilities as far as attachments care, and the reasons a draft cannot be
//! sent to it — the one predicate the send gate, the draft chips'
//! `blockers` and (for history) the render's notes all read.

use crate::state::SharedState;
use crate::store::{self, ChatAttachmentMeta, ChatThread};

use super::chat_repo::ChatRepo;
use super::chat_turn;

/// The answering model's capabilities that attachments depend on; `None` =
/// unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    pub vision: Option<bool>,
    /// `input_modalities` contains `audio`.
    pub audio: Option<bool>,
}

/// The `audio/*` type an OpenAI `input_audio` part can carry for `mime`
/// (its format is the subtype), for the three containers that go natively.
pub fn native_audio_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "audio/wav" | "audio/x-wav" | "audio/wave" => Some("audio/wav"),
        "audio/mpeg" | "audio/mp3" => Some("audio/mp3"),
        "audio/flac" | "audio/x-flac" => Some("audio/flac"),
        _ => None,
    }
}

/// The one native-audio decision: the `audio/*` type this attachment goes as
/// when the model takes audio *and* its container is one of the native ones;
/// `None` = it takes the transcript path. Upload, the send gate and the render
/// all ask this, so they cannot disagree (an Ogg or M4A file on an
/// audio-capable model is transcribed, at upload).
pub fn native_audio(caps: Caps, mime: &str) -> Option<&'static str> {
    if caps.audio == Some(true) {
        native_audio_mime(mime)
    } else {
        None
    }
}

/// Whether a lookup of the model's capabilities can change anything for these
/// attachments — a thread of text and office files has nothing for it to
/// decide, and the lookup is not free.
fn needs_caps(atts: &[ChatAttachmentMeta]) -> bool {
    atts.iter()
        .any(|a| matches!(a.kind.as_str(), "image" | "pdf" | "audio"))
}

/// [`Caps`] of the thread's current model, looked up only when one of its
/// attachments could depend on them.
pub(super) async fn thread_caps(state: &SharedState, repo: ChatRepo, thread: &ChatThread) -> Caps {
    let atts = repo
        .attachments_meta(state, thread.id)
        .await
        .unwrap_or_default();
    caps_for(state, thread, &atts).await
}

/// [`thread_caps`] for a list already read.
pub(super) async fn caps_for(
    state: &SharedState,
    thread: &ChatThread,
    atts: &[ChatAttachmentMeta],
) -> Caps {
    if needs_caps(atts) {
        chat_turn::model_caps(state, &thread.model_alias).await
    } else {
        Caps::default()
    }
}

/// Why `att`, a draft, cannot go to `model` — empty when it can.
pub fn blockers(att: &ChatAttachmentMeta, model: &str, caps: Caps, stt_set: bool) -> Vec<String> {
    let mut out = Vec::new();
    match att.kind.as_str() {
        "image" if caps.vision == Some(false) => out.push(format!(
            "'{model}' does not accept images — remove the image or switch models"
        )),
        "pdf" if store::is_text_pdf(&att.kind, &att.meta) => match att.mode.as_deref() {
            None => out.push(format!("choose Text or Pages for {}", att.name)),
            Some("images") if caps.vision == Some(false) => out.push(format!(
                "'{model}' does not accept images — send {} as Text or switch models",
                att.name
            )),
            _ => {}
        },
        "audio" => {
            let native = native_audio(caps, &att.mime).is_some();
            let transcribed = att.meta.get("transcript_alias").is_some();
            if !native && !transcribed {
                if let Some(err) = att.meta.get("transcript_error").and_then(|e| e.as_str()) {
                    out.push(format!(
                        "transcription of {} failed: {err}; retry the transcription or remove \
                         the file",
                        att.name
                    ));
                } else if !stt_set {
                    out.push(format!(
                        "'{model}' cannot take {} as audio and no speech-to-text model is set \
                         (Settings → Chat)",
                        att.name
                    ));
                }
            }
        }
        _ => {}
    }
    out
}

/// Fill `blockers` on the drafts of `atts` for the thread's current model.
pub(super) async fn annotate_drafts(
    state: &SharedState,
    thread: &ChatThread,
    atts: &mut [ChatAttachmentMeta],
) {
    if !atts.iter().any(|a| a.message_id.is_none()) {
        return;
    }
    let caps = caps_for(state, thread, atts).await;
    let stt_set = !state.snapshot().settings.chat_stt_alias.is_empty();
    for a in atts.iter_mut().filter(|a| a.message_id.is_none()) {
        a.blockers = Some(blockers(a, &thread.model_alias, caps, stt_set));
    }
}
