//! Transcription that never leaves this machine (audio-class gap 5): a voice
//! library clip — the owner's own voice — is transcribed by a local
//! speech-to-text row and nothing else.
//!
//! So the gate is opened more narrowly than for a client's request:
//! - the GPU hold's swap is refused, not taken: under the hold a local row
//!   with a cloud fallback would answer from the cloud, and without one the
//!   hold's own `gpu_hold` refusal stands;
//! - the route must land on lmgw's own audio container for a row whose task
//!   is `asr` — an alias onto a cloud or remote upstream, a chat model or a
//!   TTS row is refused before anything is sent. It is judged on the route
//!   the gate resolved, before admission starts anything, and again on the
//!   route admission hands back to send to — never on a fresh resolve of
//!   the name, which a configuration change in between could answer
//!   differently;
//! - admission is pinned ([`crate::gate::Routed::admit_pinned`]): it waits
//!   for room rather than answering with the outside-VRAM fallback.
//!
//! The send and the request row are the in-process transcription's own
//! ([`super::transcribe_labelled`]).

use bytes::Bytes;

use crate::config::{AudioModel, Route, Snapshot};
use crate::error::GatewayError;
use crate::gate::{OpenFailed, Opened, RouteCheck};
use crate::ingress::ClientProto;
use crate::state::SharedState;

use super::{transcribe_labelled, RequestCtx, Upload};

/// Transcribe `bytes` with `alias`, which must be a local speech-to-text row
/// served now (module doc). Writes one `request_logs` row, like
/// [`super::transcribe`].
pub async fn transcribe_local_only(
    state: &SharedState,
    alias: &str,
    bytes: Bytes,
    filename: &str,
    mime: &str,
) -> Result<String, GatewayError> {
    let upload = Upload {
        alias,
        bytes,
        filename,
        mime,
        stop: None,
        language: None,
        local_only: true,
    };
    transcribe_labelled(
        state,
        &RequestCtx::default(),
        ClientProto::OpenaiChat,
        upload,
    )
    .await
    .map(|t| t.text)
    .map_err(|u| u.error)
}

/// The local speech-to-text row `alias` names, or why it is not one — the
/// rule [`transcribe_local_only`] holds a request to, and what a setting
/// naming such an alias is checked against when saved. The hold plays no
/// part here (a setting outlives it).
pub fn local_asr_row<'a>(snap: &'a Snapshot, alias: &str) -> Result<&'a AudioModel, String> {
    let route = snap
        .resolve(alias)
        .map_err(|e| format!("'{alias}' does not resolve ({e})"))?;
    asr_row_of(snap, &route, alias)
}

/// The rule itself, on a route already resolved: the local speech-to-text
/// row `route` lands on, or why it lands on none. Only lmgw's own audio
/// upstream counts ([`crate::vram::classify`] goes by its sentinel id,
/// which no stored upstream can hold); `alias` is for the message.
fn asr_row_of<'a>(
    snap: &'a Snapshot,
    route: &Route,
    alias: &str,
) -> Result<&'a AudioModel, String> {
    let row = crate::vram::classify(route)
        .filter(|t| t.class == crate::runtime::Class::Audio)
        .and_then(|t| snap.audio_models.iter().find(|m| m.model_id == t.model_id));
    match row {
        Some(m) if m.task == "asr" => Ok(m),
        Some(m) => Err(format!(
            "'{alias}' is a local audio model of task '{}', not speech-to-text (asr)",
            m.task
        )),
        None => Err(format!(
            "'{alias}' is not one of this machine's audio models — a voice clip is transcribed \
             only by a local speech-to-text (asr) model, so the voice never leaves this machine"
        )),
    }
}

/// The gate for [`transcribe_local_only`] (module doc).
pub(super) async fn open(state: &SharedState, alias: &str) -> Result<Opened, OpenFailed> {
    let refused = |error| OpenFailed {
        route: None,
        headers: Default::default(),
        error,
    };
    let routed = crate::gate::resolve(state, alias, RouteCheck::Audio).await?;
    if let Some(fallback) = routed.headers().fallback() {
        return Err(refused(GatewayError::Refused {
            status: 503,
            code: "gpu_hold",
            message: format!(
                "'{alias}' is a local model and lmgw is holding the GPU — its fallback \
                 '{fallback}' would answer, and a voice clip is transcribed only on this \
                 machine; release the hold and try again"
            ),
        }));
    }
    let local = |route: &Route| {
        asr_row_of(&state.snapshot(), route, alias)
            .map(|_| ())
            .map_err(|message| GatewayError::InvalidRequest {
                code: "local_asr_required",
                message,
            })
    };
    // The route the gate resolved — what admission starts — not the name
    // resolved again: nothing is started for a route that may not hear it.
    local(routed.resolved()).map_err(refused)?;
    let opened = routed.admit_pinned(state).await?;
    // And the route the clip is sent to. Pinned admission never swaps it
    // for a fallback; this holds the rule on what is actually sent should
    // that ever change.
    if let Err(error) = local(&opened.route) {
        let Opened { route, headers, .. } = opened;
        return Err(OpenFailed {
            route: Some(Box::new(route)),
            headers,
            error,
        });
    }
    Ok(opened)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::{UpstreamKind, AUDIO_UPSTREAM_ID};

    fn row(model_id: &str, task: &str) -> AudioModel {
        serde_json::from_value(json!({
            "id": 1, "model_id": model_id, "family": "qwen3_asr", "path": model_id,
            "task": task, "mode": "offline", "load_options": {}, "session_options": {},
            "default_request_options": {}, "voice_presets": {}, "default_voice_preset": null,
            "enabled": true, "image": null, "extra_run_args": null, "warm_start": false
        }))
        .unwrap()
    }

    fn snap() -> Snapshot {
        Snapshot {
            audio_models: vec![row("asr", "asr"), row("tts", "tts")],
            ..Default::default()
        }
    }

    fn audio(snap: &Snapshot, model: &str) -> Route {
        Route {
            upstream: snap.audio_upstream(),
            upstream_model: model.into(),
            param_defaults: Default::default(),
        }
    }

    /// The rule is held on the route it is given — the one the gate
    /// resolved and the one sent to — whatever the name would resolve to
    /// now: a cloud route is refused even when it carries the local row's
    /// model name.
    #[test]
    fn the_route_given_is_judged_not_the_name() {
        let s = snap();
        assert_eq!(
            asr_row_of(&s, &audio(&s, "asr"), "audio/asr").map(|m| m.model_id.as_str()),
            Ok("asr")
        );
        let tts = asr_row_of(&s, &audio(&s, "tts"), "audio/tts").unwrap_err();
        assert!(tts.contains("not speech-to-text"), "{tts}");

        let mut cloud = audio(&s, "asr");
        cloud.upstream.id = 7;
        cloud.upstream.kind = UpstreamKind::Generic;
        cloud.upstream.base_url = "https://speech.example/v1".into();
        let e = asr_row_of(&s, &cloud, "audio/asr").unwrap_err();
        assert!(e.contains("not one of this machine's audio models"), "{e}");

        let mut chat = audio(&s, "asr");
        chat.upstream = s.router_upstream();
        assert_ne!(chat.upstream.id, AUDIO_UPSTREAM_ID);
        assert!(asr_row_of(&s, &chat, "asr").is_err());
    }
}
