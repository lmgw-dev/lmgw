//! Which errors of a heard response's audio attempt go again as the
//! transcript (voice-audio-input design §3.5), and which are its server
//! going away under the audio.

use crate::error::GatewayError;
use crate::web::chat_voice::bound::AUDIO_NOT_HEARD;

/// What a refused audio attempt was refused by (§3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// lmgw's own: the model that answers cannot take audio input, lmgw
    /// cannot tell whether it can, or its upstream's API has no audio part
    /// (`audio_input_unsupported`, `spoken::may_hear`; the Anthropic
    /// egress's own refusal too, should one ever get past that check).
    Unheard,
    /// lmgw's own: the model picked for it guards its context, which
    /// cannot bound audio.
    Guard,
    /// The model's server: an audio part it cannot take, a body too large,
    /// or a context exceeded (the transcript is 7–10× smaller).
    Server,
    /// The connection to a model that is no llama-server dropped under the
    /// audio, before an answer ([`dropped`]): this turn goes again as its
    /// transcript, and nothing is kept (review V4).
    Dropped,
}

impl Refusal {
    /// The note's `why`. lmgw's own refusals sent nothing.
    pub(super) fn why(self, e: &GatewayError) -> String {
        match self {
            // The predicate's own sentence, which names the model, the
            // upstream and that nothing was sent.
            Self::Unheard => e.to_string(),
            Self::Guard => "the model picked for it guards its context, which cannot bound audio, \
                            so nothing was sent"
                .into(),
            Self::Server => format!("the model's server refused the audio: {e}"),
            Self::Dropped => format!(
                "the connection to the model dropped under the audio ({e}), so this turn goes \
                 again as its transcript"
            ),
        }
    }
}

/// llama-server's refusal of an audio part it cannot take: a server error,
/// a 500 (`oaicompat_chat_params_parse`).
const AUDIO_UNSUPPORTED: &str = "audio input is not supported";
/// …and of a media part it could not decode (`handle_media`), an image's
/// as well as an audio part's: about the audio only when the request
/// carried no image.
const MEDIA_UNLOADABLE: &str = "Failed to load image or audio file";

/// Whether `e`, ending an audio attempt before anything was said, is a
/// refusal of the audio that the transcript may not meet (§3.5) — and
/// whose. `images`: the request carried an image. Not one: a crash, an OOM
/// or any other 5xx, a dropped connection, a timeout, the GPU hold or a
/// benchmark, a queue timeout, a model too large, a stop, a policy refusal.
pub(crate) fn refusal(e: &GatewayError, images: bool) -> Option<Refusal> {
    match e {
        GatewayError::InvalidRequest { code, .. } if *code == AUDIO_NOT_HEARD => {
            Some(Refusal::Unheard)
        }
        GatewayError::Unsupported(m) if m == crate::egress::anthropic::NO_AUDIO_BLOCK => {
            Some(Refusal::Unheard)
        }
        GatewayError::Unsupported(m) if m == crate::gate::count::AUDIO_UNBOUNDED => {
            Some(Refusal::Guard)
        }
        GatewayError::ContextExceeded { .. } => Some(Refusal::Server),
        GatewayError::Upstream {
            status: 400 | 413 | 415 | 422,
            ..
        } => Some(Refusal::Server),
        GatewayError::Upstream {
            status: 500,
            message,
            ..
        } if message.contains(AUDIO_UNSUPPORTED)
            || (!images && message.contains(MEDIA_UNLOADABLE)) =>
        {
            Some(Refusal::Server)
        }
        _ => None,
    }
}

/// Whether a server's refusal names the audio: then it is about the audio
/// whatever the transcript would meet, and kept at once (`super`'s module
/// doc).
pub(super) fn names_audio(e: &GatewayError) -> bool {
    matches!(e, GatewayError::Upstream { message, .. } if message.to_lowercase().contains("audio"))
}

/// Whether `e`, ending an audio attempt before anything was said, is the
/// connection to a model that is no llama-server dropping under the audio
/// (`llama_server`: `SentAs::llama_server`; review V4). Retried once with
/// the transcript, and never kept: a provider or a proxy that closes the
/// connection on a large body would otherwise fail every heard turn of the
/// session, and a network blip says nothing about the audio — the next
/// turn tries the audio again. A llama-server's drop is [`crashed`].
pub(crate) fn dropped(e: &GatewayError, llama_server: bool) -> bool {
    !llama_server && matches!(e, GatewayError::Transport(_))
}

/// Whether `e`, ending an audio attempt before anything was said, is the
/// model's server going away under the audio: the connection dropped (after
/// the dead-container path restarted it once and it went again), or the
/// restart failed. Not retried — the response's error — but kept (`super`'s
/// module doc).
///
/// Only when the attempt went to a llama-server (`llama_server`,
/// `SentAs::llama_server`; decision D5, 2026-10-06). The memory is about the
/// quality of the evidence, not where the model runs: llama-server aborts
/// on an audio part its projector cannot take (a non-causal encoder above
/// its ubatch, a bad decode) and takes the connection with it, so a drop
/// right under the audio says something about the audio. Through a cloud
/// API, a proxy or any other server a dropped connection is a network or
/// provider fault that says nothing about the audio: that turn goes again
/// as its transcript ([`dropped`]), and the model hears the next turn
/// again.
pub(crate) fn crashed(e: &GatewayError, llama_server: bool) -> bool {
    if !llama_server {
        return false;
    }
    match e {
        GatewayError::Transport(_) => true,
        GatewayError::Upstream {
            status: 502,
            message,
            ..
        } => message.contains("stopped answering"),
        _ => false,
    }
}
