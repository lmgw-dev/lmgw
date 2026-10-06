//! The request path's two invalidations of an external row's facts (llama
//! egress design §4.2): a transport failure on the row, and its media
//! refusals.

use crate::config::Route;
use crate::error::GatewayError;
use crate::gate::Sent;
use crate::state::SharedState;
use crate::vram::LocalHold;

use super::is_external_llama;

/// What llama-server says when a request carries a medium its projector does
/// not take: `oaicompat_chat_params_parse` throws "image input is not
/// supported - hint: if this is unexpected, you may need to provide the
/// mmproj", and the same for audio and video (`server-common.cpp`; read at
/// b062ba735, where it is a `runtime_error` and so a 500; the design cites
/// the image and audio lines at 0c6a6a7). Matched on the phrase alone,
/// whatever the status, case-insensitively.
pub const MEDIA_REFUSALS: [&str; 3] = [
    "image input is not supported",
    "audio input is not supported",
    "video input is not supported",
];

/// The medium a llama-server error body refuses, if it is one of
/// [`MEDIA_REFUSALS`].
pub fn media_refusal(body: &[u8]) -> Option<&'static str> {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    MEDIA_REFUSALS
        .iter()
        .find(|phrase| text.contains(*phrase))
        .map(|phrase| phrase.split(' ').next().unwrap_or(phrase))
}

/// Watch one chat send's outcome on an external `llama_cpp` row and drop the
/// row's facts when it says they may be stale (§4.2): a transport failure
/// (the server went away, and may come back as another build or with another
/// projector), or a media refusal (it does not take what its facts said it
/// does). The outcome is handed back as it came: an error answer read here is
/// rebuilt from its bytes, so the caller relays the same status, headers and
/// body — and one whose body could not be read whole is rebuilt around that
/// failure, unchecked (`failing`), so each caller meets it where it would
/// have.
///
/// Nothing is read for a managed row, a row of another protocol, or a row
/// the cache knows nothing about — there is nothing to drop then.
pub async fn observe(
    state: &SharedState,
    hold: Option<&LocalHold>,
    route: &Route,
    sent: Result<Sent, GatewayError>,
) -> Result<Sent, GatewayError> {
    let up = &route.upstream;
    if hold.is_some() || !is_external_llama(up) || !state.llama_facts.knows(up.id) {
        return sent;
    }
    let resp = match sent {
        Err(GatewayError::Transport(why)) => {
            state.llama_facts.invalidate(up.id);
            tracing::info!(
                upstream = %up.name,
                "llama-server unreachable ({why}): its /props facts are read again on next use"
            );
            return Err(GatewayError::Transport(why));
        }
        Ok(Sent::Upstream(resp))
            if resp.status().is_client_error() || resp.status().is_server_error() =>
        {
            resp
        }
        other => return other,
    };
    let (status, version, headers) = (resp.status(), resp.version(), resp.headers().clone());
    let bytes = match resp.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => return Ok(Sent::Upstream(failing(status, version, headers, e))),
    };
    if let Some(medium) = media_refusal(&bytes) {
        state.llama_facts.invalidate(up.id);
        tracing::info!(
            upstream = %up.name,
            model = %route.upstream_model,
            "llama-server refused {medium} input: its /props facts are read again on next use"
        );
    }
    Ok(Sent::Upstream(crate::gate::rebuild(
        status, version, headers, bytes,
    )))
}

/// An error answer whose body broke off, rebuilt with a body that fails as
/// it did: what a caller does with an unreadable error body — a streaming
/// send maps the status with no body, a buffered one fails the request — it
/// does here too, as if nothing had read it first. Only the response's URL
/// is lost, as on every rebuilt response ([`crate::gate::rebuild`]).
fn failing(
    status: reqwest::StatusCode,
    version: reqwest::Version,
    headers: reqwest::header::HeaderMap,
    failed: reqwest::Error,
) -> reqwest::Response {
    let body = reqwest::Body::wrap_stream(futures::stream::once(async move {
        Err::<bytes::Bytes, _>(failed)
    }));
    let mut rebuilt = axum::http::Response::new(body);
    *rebuilt.status_mut() = status;
    *rebuilt.version_mut() = version;
    *rebuilt.headers_mut() = headers;
    reqwest::Response::from(rebuilt)
}
