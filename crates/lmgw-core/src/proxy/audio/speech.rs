//! `POST /v1/audio/speech`: resolve → preflight → admit → shape → send.
//!
//! The other audio routes are passthroughs ([`super::audio_json_call`]);
//! speech is too, except that its body is checked before admission
//! ([`crate::audio::preflight`]: nothing is started for a request the
//! engine would refuse) and shaped on the way ([`crate::audio::shape`]) —
//! on the route admission settled on, so a fallback that answers instead
//! gets the client's body shaped for itself: a local audio.cpp row in full,
//! any other route its instructions and inline tags only.

use std::time::Instant;

use axum::response::Response;
use serde_json::Value;

use crate::audio::engine_errors::explain_speech;
use crate::audio::preflight::{refuse_speech, wants_stream};
use crate::audio::rates::SAMPLE_RATE_HEADER;
use crate::audio::shape::{shape_remote, shape_speech, Expressive, ShapeReport};
use crate::audio::voices::{row_of_route, row_speech, RowSpeech};
use crate::config::{AudioModel, Route, Snapshot};
use crate::egress::apply_bearer_auth;
use crate::error::GatewayError;
use crate::gate::GateHeaders;
use crate::ingress::ClientProto;
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::{audio_send, body_alias, finish_audio, note_buffered_answer, MediaOutcome};
use crate::proxy::{policy_or_refuse, Failed, RequestCtx};

/// `POST /v1/audio/speech` — OpenAI TTS shape (JSON in, audio/JSON/SSE out).
pub async fn handle_audio_speech(state: SharedState, ctx: RequestCtx, body: Value) -> Response {
    let started = Instant::now();
    state.telemetry.request_started();
    let alias = body_alias(&body);
    if let Some(r) = policy_or_refuse(
        &state,
        ClientProto::OpenaiChat,
        &ctx,
        &alias,
        started,
        RequestClass::Audio,
    )
    .await
    {
        return r;
    }
    let spoken = speech_call(&state, &body, started).await;
    let mut resp = finish_audio(&state, &ctx, alias, started, spoken.result).await;
    if let Some(v) = spoken.report.header_value() {
        resp.headers_mut().insert(SPEECH_HEADER, v);
    }
    if let Some(rate) = spoken.sample_rate.filter(|_| resp.status().is_success()) {
        resp.headers_mut()
            .insert(SAMPLE_RATE_HEADER, axum::http::HeaderValue::from(rate));
    }
    resp
}

/// What [`speech_call`] produced besides its outcome.
struct Spoken {
    result: Result<MediaOutcome, Failed>,
    report: ShapeReport,
    /// For a streamed request to an lmgw audio row: the row's sample rate,
    /// when its last WAV through lmgw said it (`crate::audio::rates`).
    sample_rate: Option<u32>,
}

/// The response header saying what shaping changed in the request
/// ([`ShapeReport::header_value`]); absent when the body went as it came.
pub const SPEECH_HEADER: &str = "x-lmgw-speech";

/// The speech call's gate, shaping and send.
async fn speech_call(state: &SharedState, body: &Value, started: Instant) -> Spoken {
    let refused = |failed: Failed| Spoken {
        result: Err(failed),
        report: ShapeReport::default(),
        sample_rate: None,
    };
    let Some(alias) = body.get("model").and_then(Value::as_str) else {
        let e = GatewayError::BadRequest("missing 'model'".into());
        return refused((None, GateHeaders::default(), e));
    };
    // Resolve: the hold swap and the audio routes' protocol check.
    let routed = match crate::gate::resolve(state, alias, crate::gate::RouteCheck::Audio).await {
        Ok(r) => r,
        Err(f) => return refused((f.route, f.headers, f.error)),
    };
    // Preflight: what the engine of the resolved route would refuse is
    // refused here, before anything is started for it.
    let resolved = local_speech(state, routed.resolved()).await;
    let checked =
        match body.as_object() {
            Some(obj) => refuse_speech(obj, resolved.as_ref().map(|(r, s)| (r, &*s.profile)))
                .and_then(|()| match &resolved {
                    // A library clip its engine cannot clone without a
                    // transcript (`crate::audio::transcript`).
                    Some((row, s)) => crate::audio::transcript::refuse_body(row, s, obj),
                    None => Ok(()),
                }),
            None => Ok(()),
        };
    if let Err(e) = checked {
        return refused((
            Some(Box::new(routed.resolved().clone())),
            routed.headers().clone(),
            e,
        ));
    }
    // Admit: the GPU claim travels out with the outcome.
    let crate::gate::Opened {
        route,
        hold: admission,
        headers,
    } = match routed.admit(state).await {
        Ok(o) => o,
        Err(f) => return refused((f.route, f.headers, f.error)),
    };
    let mut out = body.clone();
    out["model"] = Value::String(route.upstream_model.clone());
    // The row's facts are read again only when admission answered with
    // another route than the one resolved.
    let snap = state.snapshot();
    let same = |(row, _): &(AudioModel, RowSpeech)| {
        row_of_route(&snap, &route).is_some_and(|r| r.model_id == row.model_id)
    };
    let speech = match resolved.filter(same) {
        Some(s) => Some(s),
        None => local_speech(state, &route).await,
    };
    let answering = headers.fallback().unwrap_or(alias);
    let report = shape_on(&snap, speech.as_ref(), answering, &mut out);
    let sample_rate = speech
        .as_ref()
        .filter(|_| body.as_object().is_some_and(wants_stream))
        .and_then(|(row, _)| state.audio_rates.get(&row.model_id));
    // `stream_format: audio` answers raw PCM, chunked: streamed, though no
    // event stream (audio-class gap 8).
    let chunked = body.get("stream_format").and_then(Value::as_str) == Some("audio");
    if let Some(hold) = admission.as_ref() {
        hold.note_sending();
    }
    let sent = audio_send(admission.as_ref(), &route, |r| {
        let url = format!("{}/audio/speech", r.upstream.base());
        Ok(apply_bearer_auth(
            state.http.post(url).json(&out),
            &r.upstream,
        ))
    })
    .await;
    let result = match sent {
        Ok(resp) => {
            note_buffered_answer(admission.as_ref(), &resp, chunked);
            Ok(MediaOutcome {
                ttfb_ms: started.elapsed().as_millis() as i64,
                resp,
                route,
                headers,
                admission,
                chunked,
            })
        }
        // audio.cpp's own refusal of a clip without its transcript, or of
        // an image without eSpeak NG, said as lmgw's
        // (`crate::audio::engine_errors`).
        Err(e) => {
            let voice = out.get("voice").and_then(Value::as_str);
            let e = explain_speech(e, answering, voice);
            Err((Some(Box::new(route)), headers, e))
        }
    };
    Spoken {
        result,
        report,
        sample_rate,
    }
}

/// The lmgw audio row `route` lands on, with what it speaks; `None` for any
/// other route.
pub(crate) async fn local_speech(
    state: &SharedState,
    route: &Route,
) -> Option<(AudioModel, RowSpeech)> {
    let row = row_of_route(&state.snapshot(), route)?.clone();
    let speech = row_speech(state, &row).await;
    Some((row, speech))
}

/// Shape `body` for the route that answers: an lmgw audio row's `speech`
/// in full, any other route by what the owner's override on `alias` (the
/// name answering — the fallback's, when one does) says it takes.
pub(crate) fn shape_on(
    snap: &Snapshot,
    speech: Option<&(AudioModel, RowSpeech)>,
    alias: &str,
    body: &mut Value,
) -> ShapeReport {
    let Some(obj) = body.as_object_mut() else {
        return ShapeReport::default();
    };
    match speech {
        Some((row, s)) => shape_speech(&s.profile, &s.voices, row, obj),
        None => shape_remote(&rules_on(snap, None, alias), obj),
    }
}

/// What the route that answers does with instructions and inline tags: an
/// lmgw audio row's `speech` profile, else what the owner's override on
/// `alias` (the name answering) says ([`remote_rules`]). Shaping goes by it,
/// and so does whether a realtime clause's delivery cue is sent
/// (`crate::audio::cues`, WP9b).
pub(crate) fn rules_on(
    snap: &Snapshot,
    speech: Option<&(AudioModel, RowSpeech)>,
    alias: &str,
) -> Expressive {
    match speech {
        Some((_, s)) => Expressive::of(&s.profile),
        None => remote_rules(snap, alias),
    }
}

/// What a remote alias takes: the `capabilities.speech` of the owner's
/// `capabilities_override` on it, else [`Expressive::remote`].
pub(crate) fn remote_rules(snap: &Snapshot, alias: &str) -> Expressive {
    let speech = snap
        .aliases
        .get(alias)
        .and_then(|a| a.capabilities_override.as_ref())
        .and_then(|o| o.get("capabilities"))
        .and_then(|c| c.get("speech"));
    Expressive::from_override(speech)
}
