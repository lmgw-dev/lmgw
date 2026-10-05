//! `GET /v1/audio/voices?model=<alias>[&probe=engine]` — a TTS model's
//! voice names, so a client can fill a voice picker instead of guessing
//! names like "alloy".
//!
//! **A local audio row is answered from lmgw's own catalog** — its presets,
//! the voices its package ships ([`crate::audio::profile`]), the
//! `embeddings/` files of its root and the voice library — without starting
//! anything: no admission, no claim, no `request_logs` row (metadata, like
//! `GET /v1/models`). That list is a superset of what audio.cpp's own
//! `GET /v1/audio/voices` answers, which lacks the shipped voices (so
//! `Jason`, `F1` or `ryan` were refused by clients that checked it), and it
//! cost a container start to read. `x-lmgw-voices-source: config`.
//!
//! **Under the GPU hold** (or a benchmark's lease) with no fallback for the
//! row, the list is still answered from lmgw's catalog — the hold is a zero
//! VRAM allowance, not "the model does not exist", and a config read needs
//! no VRAM — with `lmgw.held: true`, saying speech would be refused now.
//! With a fallback, the list is the fallback's: what would actually answer.
//!
//! `?probe=engine` asks the model's own server instead — the read this
//! route always did: admission starts the container when it is down, and the
//! answer is audio.cpp's. For a row whose `extra_run_args` mount another
//! voice directory than the library, that is the only list that shows its
//! clips. `x-lmgw-voices-source: engine`. A remote route is always asked
//! (it has no container to start), and says `engine` too.

use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use super::{error_response, voices_body, voices_response};
use crate::audio::voices::{row_of_route, row_speech};
use crate::config::AudioModel;
use crate::error::GatewayError;
use crate::ingress::ClientProto;
use crate::state::SharedState;

/// The response header naming where a voice list came from.
pub const VOICES_SOURCE_HEADER: &str = "x-lmgw-voices-source";

/// Where `GET /v1/audio/voices` reads a list from (module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoicesProbe {
    /// lmgw's catalog for a local row; the upstream for a remote one.
    Config,
    /// The model's own server, started if it is down.
    Engine,
}

impl VoicesProbe {
    /// The `probe` query value: absent or `config` is [`Self::Config`],
    /// `engine` is [`Self::Engine`]; anything else is the client's error.
    pub fn parse(v: Option<&str>) -> Result<Self, String> {
        match v.map(str::trim).filter(|v| !v.is_empty()) {
            None => Ok(Self::Config),
            Some(v) if v.eq_ignore_ascii_case("config") => Ok(Self::Config),
            Some(v) if v.eq_ignore_ascii_case("engine") => Ok(Self::Engine),
            Some(v) => Err(format!(
                "probe '{v}' is not one of config, engine (engine asks the model's own server, \
                 starting it if it is down)"
            )),
        }
    }
}

/// `GET /v1/audio/voices` (module doc).
pub async fn handle_audio_voices(state: SharedState, alias: &str, probe: VoicesProbe) -> Response {
    const PROTO: ClientProto = ClientProto::OpenaiChat;
    // Resolved like the inference routes it describes — the same gate and
    // checks ([`crate::gate::RouteCheck::AudioVoices`]), so "which model does
    // `audio/x` mean" has one answer here and on the speech route.
    let routed =
        match crate::gate::resolve(&state, alias, crate::gate::RouteCheck::AudioVoices).await {
            Ok(r) => r,
            Err(f) => {
                // The GPU hold (or a benchmark's lease) with no fallback:
                // the hold is a zero VRAM allowance, not a model that is
                // gone, and reading lmgw's config takes none — so a local
                // row's list is answered as usual, marked held (speech
                // would be refused now). A voice picker keeps working.
                let held = matches!(
                    f.error,
                    GatewayError::GpuHold { .. } | GatewayError::GpuBenchmark { .. }
                );
                if held && probe == VoicesProbe::Config {
                    if let Some(resp) = held_config(&state, alias).await {
                        return resp;
                    }
                }
                return f.headers.stamp(error_response(PROTO, &f.error));
            }
        };
    if probe == VoicesProbe::Config {
        let snap = state.snapshot();
        if let Some(row) = row_of_route(&snap, routed.resolved()) {
            let resp = config_answer(&state, row, false).await;
            return routed.headers().stamp(resp);
        }
    }
    // The model's own server: admission starts a local container that is
    // down, and the hold is held for the read; a remote route is just asked.
    let opened = match routed.admit(&state).await {
        Ok(o) => o,
        Err(f) => return f.headers.stamp(error_response(PROTO, &f.error)),
    };
    let resp = match voices_body(&state, opened.hold.as_ref(), &opened.route).await {
        Ok(bytes) => with_source(voices_response(bytes), "engine"),
        Err(e) => error_response(PROTO, &e),
    };
    opened.headers.stamp(resp)
}

/// A local row's list from lmgw's catalog (module doc); `held`: the GPU
/// hold (or a benchmark) refuses its speech right now.
async fn config_answer(state: &SharedState, row: &AudioModel, held: bool) -> Response {
    let speech = row_speech(state, row).await;
    let v = &speech.voices;
    let body = json!({
        "voices": v.names(),
        "lmgw": {
            "source": "config",
            "engine_asked": false,
            "held": held,
            "entries": v.entries,
            "default": v.default,
            "missing": v.missing,
            // The engine refuses a library clip without its transcript
            // (`crate::audio::transcript`): an entry with `transcript:
            // false` is refused until it has one.
            "needs_transcript":
                crate::audio::transcript::refuses_untranscribed(row, &speech.profile),
        },
    });
    with_source(Json(body).into_response(), "config")
}

/// [`config_answer`] for `alias` resolved past the hold, when it lands on a
/// local audio row and passes the route's own checks; `None` otherwise (the
/// hold's refusal stands).
async fn held_config(state: &SharedState, alias: &str) -> Option<Response> {
    let snap = state.snapshot();
    let route = snap.resolve(alias).ok()?;
    crate::proxy::require_openai_audio(&route, alias).ok()?;
    crate::proxy::refuse_non_audio_voices(&route, alias).ok()?;
    let row = row_of_route(&snap, &route)?;
    Some(config_answer(state, row, true).await)
}

fn with_source(mut resp: Response, source: &'static str) -> Response {
    resp.headers_mut()
        .insert(VOICES_SOURCE_HEADER, HeaderValue::from_static(source));
    resp
}
