//! The warm's load of a speech-to-text model (realtime design §9.1; live
//! run 3b, D3'; chat-voice design §4.2): a short silent clip on a claim the
//! warm already holds.
//!
//! audio.cpp loads a lazy row's weights on its first request: warmed by its
//! container alone, nemotron and qwen3-asr held no GPU memory, the first
//! barge's word check loaded the 3 GB model and missed its 500 ms, and the
//! first turn's transcript took 1.2 s instead of 0.1. audio.cpp's own load
//! route (`POST /v1/models/load`) is refused without `--ui-management`,
//! which lmgw keeps off, so the warm sends the smallest real request
//! instead — the multipart upload a turn's transcript is, through the audio
//! routes' own send ([`audio_send`]: a dead container is recovered the same
//! way), so the claim learns what any answered request teaches
//! (`LocalHold::note_sending`, `LocalHold::note_inference`). The claim stays
//! the caller's: an Admit warm keeps it until its group is up. It writes
//! **no request row**: nobody asked for it.

use bytes::Bytes;

use super::super::multipart::{reencode_multipart, MultipartField};
use crate::config::Route;
use crate::egress::apply_bearer_auth;
use crate::error::GatewayError;
use crate::state::SharedState;
use crate::vram::LocalHold;

use super::super::audio::audio_send;

/// Transcribe `wav` on `route`, a local speech-to-text row's container that
/// `hold` claims, to load its model (module doc). The transcript is not
/// wanted; the answer is read whole.
pub(crate) async fn load(
    state: &SharedState,
    route: &Route,
    hold: &LocalHold,
    wav: Bytes,
) -> Result<(), GatewayError> {
    let fields = [
        MultipartField::Text("model".into(), route.upstream_model.clone()),
        MultipartField::File(
            "file".into(),
            "warm.wav".into(),
            Some("audio/wav".into()),
            wav,
        ),
    ];
    hold.note_sending();
    let resp = audio_send(Some(hold), route, |r| {
        let url = format!("{}/audio/transcriptions", r.upstream.base());
        Ok(apply_bearer_auth(
            state
                .http
                .post(url)
                .multipart(reencode_multipart(&fields, r)),
            &r.upstream,
        ))
    })
    .await?;
    resp.bytes()
        .await
        .map_err(|e| GatewayError::Transport(e.to_string()))?;
    hold.note_inference();
    Ok(())
}
