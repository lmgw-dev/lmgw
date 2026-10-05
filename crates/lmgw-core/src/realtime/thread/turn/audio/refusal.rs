//! Which errors of a heard response's audio attempt go again as the
//! transcript (voice-audio-input design §3.5), and which are its server
//! going away under the audio.

use crate::error::GatewayError;
use crate::web::chat_voice::bound::AUDIO_NOT_LOCAL;

/// What a refused audio attempt was refused by (§3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// lmgw's own: the route is not one it runs (`audio_not_local`).
    NotLocal,
    /// lmgw's own: the model picked for it guards its context, which
    /// cannot bound audio.
    Guard,
    /// The model's server: an audio part it cannot take, a body too large,
    /// or a context exceeded (the transcript is 7–10× smaller).
    Server,
}

impl Refusal {
    /// The note's `why`. lmgw's own refusals sent nothing.
    pub(super) fn why(self, e: &GatewayError) -> String {
        match self {
            Self::NotLocal => "its route is a model lmgw does not run, so nothing was sent: your \
                               voice stays on this machine"
                .into(),
            Self::Guard => "the model picked for it guards its context, which cannot bound audio, \
                            so nothing was sent"
                .into(),
            Self::Server => format!("the model's server refused the audio: {e}"),
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
        GatewayError::InvalidRequest { code, .. } if *code == AUDIO_NOT_LOCAL => {
            Some(Refusal::NotLocal)
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
/// model's server going away under the audio: the connection dropped (after
/// the dead-container path restarted it once and it went again), or the
/// restart failed. Not retried — the response's error — but kept (`super`'s
/// module doc).
pub(crate) fn crashed(e: &GatewayError) -> bool {
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
