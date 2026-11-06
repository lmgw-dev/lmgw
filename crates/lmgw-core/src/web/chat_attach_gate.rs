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
    /// The model takes an audio part: the voice turn's predicate
    /// (`capabilities::hears`) — its egress has one, its capabilities list
    /// audio, its llama-server did not say otherwise.
    pub audio: Option<bool>,
    /// The answering egress takes FLAC as audio (Gemini does; the OpenAI
    /// wire takes `wav` and `mp3` only). `false` while unknown.
    pub flac: bool,
}

/// The `audio/*` type an OpenAI `input_audio` part can carry for `mime`
/// (its format is the subtype), for the three containers that go natively.
/// The table is the egress's (`openai_wire::audio_format`).
pub fn native_audio_mime(mime: &str) -> Option<&'static str> {
    match crate::egress::openai_wire::audio_format(mime)? {
        "wav" => Some("audio/wav"),
        "mp3" => Some("audio/mp3"),
        _ => Some("audio/flac"),
    }
}

/// The one native-audio decision: the `audio/*` type this attachment goes as
/// when the model takes audio *and* its container is one the answering
/// egress takes (WAV and MP3 everywhere; FLAC only where `caps.flac` says,
/// since OpenAI's `input_audio.format` takes just `wav` and `mp3`); `None` =
/// it takes the transcript path. Upload, the send gate and the render all
/// ask this, so they cannot disagree (an Ogg or M4A file on an audio-capable
/// model is transcribed, at upload).
pub fn native_audio(caps: Caps, mime: &str) -> Option<&'static str> {
    if caps.audio != Some(true) {
        return None;
    }
    native_audio_mime(mime).filter(|m| *m != "audio/flac" || caps.flac)
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

/// Why `att`, a draft, cannot go to `model` — empty when it can. `stt_set`:
/// the thread resolves a speech-to-text alias (`chat_voice::asr_alias`).
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
                         (Settings → Chat → Voice, or this thread's voice settings)",
                        att.name
                    ));
                }
            }
        }
        _ => {}
    }
    out
}

/// Fill `blockers` on the drafts of `atts` for the thread's current model,
/// and `hints` where a draft that goes would reach a fallback that cannot
/// see ([`blind_hint`]).
pub(super) async fn annotate_drafts(
    state: &SharedState,
    thread: &ChatThread,
    atts: &mut [ChatAttachmentMeta],
) {
    if !atts.iter().any(|a| a.message_id.is_none()) {
        return;
    }
    let caps = caps_for(state, thread, atts).await;
    let stt_set = super::chat_voice::asr_alias(&state.snapshot(), thread).is_some();
    let seen = |a: &ChatAttachmentMeta| a.kind == "image" || paged(a);
    let blind = match atts.iter().any(|a| a.message_id.is_none() && seen(a)) {
        true => blind_hint(state, thread).await,
        false => None,
    };
    for a in atts.iter_mut().filter(|a| a.message_id.is_none()) {
        let blocked = blockers(a, &thread.model_alias, caps, stt_set);
        a.hints = match &blind {
            Some(lead) if blocked.is_empty() && a.kind == "image" => {
                Some(vec![format!("{lead}: it gets a placeholder instead")])
            }
            Some(lead) if blocked.is_empty() && paged(a) => Some(vec![format!(
                "{lead}: it gets the PDF's text instead of its page images"
            )]),
            _ => None,
        };
        a.blockers = Some(blocked);
    }
}

/// A PDF that goes as page images to a model that sees: one sent as Pages,
/// or one with pages without text.
fn paged(a: &ChatAttachmentMeta) -> bool {
    let textless = a
        .meta
        .get("textless")
        .and_then(|t| t.as_array())
        .is_some_and(|t| !t.is_empty());
    a.kind == "pdf" && (a.mode.as_deref() == Some("images") || textless)
}

/// The lead of a draft's hint (review I2) when the GPU hold or a benchmark
/// run hands the thread's turns to a fallback that cannot see — "under the
/// GPU hold this goes to openai/gpt, which cannot see images", worded as the
/// voice turn's hint is — and `None` otherwise. Nothing is refused for it:
/// the fallback answers, with the images as placeholders and a PDF's pages
/// as its text (`chat_turn::blind`, the owner's ruling of 2026-10-06).
async fn blind_hint(state: &SharedState, thread: &ChatThread) -> Option<String> {
    let snap = state.snapshot();
    let resolved = snap.resolve_for_request(&thread.model_alias).ok()?;
    let fallback = resolved.fallback?;
    let route = &resolved.route;
    crate::gate::fallback_images::blind_fallback(state, route, &thread.model_alias).await?;
    let lead = match snap.gpu_block() {
        Some(crate::bench::lease::GpuBlock::Benchmark(_)) => "while a benchmark run holds the GPU",
        _ => "under the GPU hold",
    };
    Some(format!(
        "{lead} this goes to {fallback}, which cannot see images"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEARS: Caps = Caps {
        vision: None,
        audio: Some(true),
        flac: false,
    };

    #[test]
    fn flac_goes_natively_only_where_the_egress_takes_it() {
        assert_eq!(native_audio(HEARS, "audio/flac"), None);
        assert_eq!(native_audio(HEARS, "audio/x-flac"), None);
        let gemini = Caps {
            flac: true,
            ..HEARS
        };
        assert_eq!(native_audio(gemini, "audio/flac"), Some("audio/flac"));
    }

    #[test]
    fn wav_and_mp3_go_natively_in_any_spelling_and_case() {
        for m in ["audio/WAV", "audio/x-wav", "audio/wave", "audio/vnd.wave"] {
            assert_eq!(native_audio(HEARS, m), Some("audio/wav"), "{m}");
        }
        for m in ["audio/MPEG", "audio/mp3", "audio/x-mp3", "audio/x-mpeg"] {
            assert_eq!(native_audio(HEARS, m), Some("audio/mp3"), "{m}");
        }
        assert_eq!(native_audio(HEARS, "audio/ogg"), None);
        let deaf = Caps {
            audio: Some(false),
            ..HEARS
        };
        assert_eq!(native_audio(deaf, "audio/wav"), None);
    }
}
