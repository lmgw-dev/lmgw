//! OpenAI image endpoints (image-generation design §6): byte-level passthrough
//! to the resolved upstream, the audio family's shape applied to a second
//! engine. sd-server speaks `/v1/images/*` natively and a cloud image provider
//! speaks the same routes, so there is no IR here either — alias resolution,
//! the route guard, the model rewrite, auth and telemetry, and nothing else.
//!
//! lmgw adds **no** field of its own to the request and strips none: the escape
//! hatch for everything the OpenAI body cannot say is sd.cpp's own
//! `<sd_cpp_extra_args>` block inside the prompt, which is published in the
//! model's notes. `response_format: url`, the native async job routes and
//! `/sdapi/v1/*` are §13, not this.

use std::time::Instant;

use axum::response::Response;
use serde_json::Value;

use crate::capabilities::{IMAGE_EDITS_ENDPOINT, IMAGE_GENERATIONS_ENDPOINT};
use crate::config::Route;
use crate::egress::{apply_bearer_auth, for_protocol};
use crate::error::GatewayError;
use crate::gate::GateHeaders;
use crate::ingress::ClientProto;
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::*;

/// The upstream path of one of the two published image routes: the route
/// without the `/v1` that an upstream's own `base()` already carries. Derived
/// rather than declared a second time, so the advertised endpoint and the URL
/// actually posted to cannot drift apart.
fn image_upstream_path(endpoint: &str) -> &str {
    endpoint.trim_start_matches("/v1")
}

/// sd-server's two error shapes, normalized into the gateway's one (§2.3,
/// §12.6).
///
/// `400 {"error":"prompt required"}` carries the message in `error` itself;
/// `500 {"error":"server_error","message":…}` carries the *type* there and the
/// text in `message` — and a JSON parse error is a 500 on that server, not a
/// 400. Anything else — a cloud provider's `{"error":{"message":…}}`, an HTML
/// body from a proxy in between — goes through the shared OpenAI mapper. One
/// route must never answer in two error shapes, whichever kind of upstream is
/// behind it.
fn image_error(status: u16, body: &[u8]) -> GatewayError {
    let v: Value = serde_json::from_slice(body).unwrap_or_default();
    let Some(err) = v.get("error").and_then(Value::as_str) else {
        return for_protocol(crate::config::Protocol::Openai).map_error(status, body);
    };
    match v.get("message").and_then(Value::as_str) {
        Some(message) => GatewayError::Upstream {
            status,
            provider_type: Some(err.to_string()),
            message: message.to_string(),
        },
        None => GatewayError::Upstream {
            status,
            provider_type: None,
            message: err.to_string(),
        },
    }
}

/// The route check of the `/v1/images/*` routes
/// ([`crate::gate::RouteCheck::Image`]): refuse a route here if the endpoint
/// is not one the model serves.
///
/// Three guards, in the order the audio routes established (gpu-hold design
/// §4: the gate runs this on the route it settled on — after the hold swap,
/// and again on a fallback it takes at admission — so it judges the upstream
/// the bytes will actually reach):
///
/// 1. the upstream must speak the OpenAI protocol — an anthropic or gemini
///    base URL has neither the path nor the auth header;
/// 2. a **local** route must be an image row. Every class answers on an
///    openai-shaped port, so the protocol check cannot tell them apart, and a
///    generation body posted to llama-server or audio.cpp would come back as
///    that server's confusion;
/// 3. `/v1/images/edits` needs the row's `edit` column. This is a hard gate,
///    not a hint: sd-server does not refuse a reference-image request against
///    a pipeline that cannot take one, it **segfaults** (measured, §12.8), so
///    the refusal has to stand between the client and the container.
pub(crate) async fn image_route_guard(
    state: &SharedState,
    snap: &crate::config::Snapshot,
    route: &Route,
    alias: &str,
    endpoint: &str,
) -> Result<(), GatewayError> {
    if route.upstream.protocol != crate::config::Protocol::Openai {
        return Err(GatewayError::Unsupported(format!(
            "/v1/images/* requires an openai-protocol upstream; alias '{alias}' resolves to \
             '{}' ({})",
            route.upstream.name,
            route.upstream.protocol.as_str(),
        )));
    }
    match crate::vram::classify(route) {
        Some(target) if target.class == crate::runtime::Class::Image => {
            let row = snap
                .image_models
                .iter()
                .find(|m| m.model_id == target.model_id)
                .ok_or_else(|| GatewayError::UnknownAlias(alias.to_string()))?;
            if endpoint == IMAGE_EDITS_ENDPOINT && !row.edit {
                return Err(GatewayError::Unsupported(format!(
                    "model '{alias}' does not serve {IMAGE_EDITS_ENDPOINT}: its image row has \
                     edit = false, so the pipeline takes no reference images. sd-server does \
                     not refuse such a request — it crashes on it — so lmgw refuses it here. \
                     Set edit on the row only if this really is an edit pipeline (Kontext, \
                     Qwen-Image-Edit, Z-Image-Omni)."
                )));
            }
            Ok(())
        }
        Some(target) => Err(GatewayError::Unsupported(format!(
            "model '{alias}' is a {} model, and {endpoint} serves image models",
            target.class
        ))),
        None => cloud_image_guard(state, route, alias, endpoint).await,
    }
}

/// The cloud half of [`resolve_image`]'s route guard.
///
/// A provider's catalog is the only thing that knows, so the rule is "refuse
/// what is positively known not to fit": modalities the upstream *states*, and
/// that do not include an image. A catalog that says nothing about modalities
/// — or that cannot be reached at all — forwards the request: absent means
/// unknown is the whole contract of those lists (model-capabilities design
/// §2.1), and refusing on a silence would lock out every OpenAI-compatible
/// image provider that publishes no metadata.
async fn cloud_image_guard(
    state: &SharedState,
    route: &Route,
    alias: &str,
    endpoint: &str,
) -> Result<(), GatewayError> {
    let Some(info) = crate::catalog::upstream_models(state, &route.upstream)
        .await
        .ok()
        .and_then(|list| list.into_iter().find(|m| m.id == route.upstream_model))
    else {
        return Ok(());
    };
    if let Some(out) = info
        .output_modalities
        .as_ref()
        .filter(|out| !out.iter().any(|m| m == "image" || m == "video"))
    {
        return Err(GatewayError::Unsupported(format!(
            "model '{alias}' does not serve {endpoint}: the catalog of upstream '{}' says it \
             outputs {}, and an image route needs a model that outputs images",
            route.upstream.name,
            out.join(", ")
        )));
    }
    if endpoint == IMAGE_EDITS_ENDPOINT {
        if let Some(inp) = info
            .input_modalities
            .as_ref()
            .filter(|inp| !inp.iter().any(|m| m == "image"))
        {
            return Err(GatewayError::Unsupported(format!(
                "model '{alias}' does not serve {endpoint}: the catalog of upstream '{}' says it \
                 takes {} as input, and an edit sends an image",
                route.upstream.name,
                inp.join(", ")
            )));
        }
    }
    Ok(())
}

/// Send an image request, bounding only the wait for response *headers* — the
/// body then streams unbounded, because a generation is a synchronous render
/// that holds the connection open for as long as it takes. The ceiling is the
/// route's own ceiling (`image.request_timeout_seconds` on the synthetic image
/// upstream, 0 there meaning none at all); this adds none of its own.
///
/// A non-2xx is buffered and turned into a [`GatewayError`] carrying the
/// server's own message — error bodies are small, and the alternative is
/// logging a bare "HTTP 500" and relaying a foreign error shape.
async fn image_send<F>(
    hold: Option<&crate::vram::LocalHold>,
    route: &Route,
    build: F,
) -> Result<reqwest::Response, GatewayError>
where
    F: Fn(&Route) -> Result<reqwest::RequestBuilder, GatewayError>,
{
    // Headers only, through the shared dead-container retry (§3.2). This is
    // also the path that gives `vram::LocalHold::recover` its one chance: a
    // container that died mid-render answers with a dropped connection, which
    // arrives here as `GatewayError::Transport`, and `send_local` turns that
    // into a forced stop plus a fresh admit before retrying once.
    let resp =
        crate::vram::send_local(hold, route, route.upstream.request_timeout(), build).await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let bytes = resp.bytes().await.unwrap_or_default();
    Err(image_error(status.as_u16(), &bytes))
}

/// Resolve, admit, rewrite `model`, send — the JSON path behind
/// `/v1/images/generations`.
async fn image_json_call(
    state: &SharedState,
    endpoint: &'static str,
    body: &Value,
    started: Instant,
) -> Result<MediaOutcome, Failed> {
    let alias = body.get("model").and_then(Value::as_str).ok_or((
        None,
        GateHeaders::default(),
        GatewayError::BadRequest("missing 'model'".into()),
    ))?;
    // The gate's per-request half: the hold swap, the route guard
    // ([`crate::gate::RouteCheck::Image`]), then admission — a loaded
    // pipeline is 7–13 GiB of the card, so the image class takes it like
    // every other local model. The guard travels out with the outcome (see
    // [`MediaOutcome::admission`]), and a local route comes back on the port
    // its container answers on (§5).
    let crate::gate::Opened {
        route,
        hold: admission,
        headers,
    } = crate::gate::open(state, alias, crate::gate::RouteCheck::Image(endpoint))
        .await
        .map_err(|f| (f.route, f.headers, f.error))?;
    // The one edit lmgw makes. sd-server ignores `model` outright (§2.3), so
    // the rewrite costs nothing there and is what a cloud upstream needs;
    // every other field is forwarded exactly as it arrived.
    let mut out = body.clone();
    out["model"] = Value::String(route.upstream_model.clone());
    let path = image_upstream_path(endpoint);
    let sent = image_send(admission.as_ref(), &route, |r| {
        let url = format!("{}{path}", r.upstream.base());
        Ok(apply_bearer_auth(
            state.http.post(url).json(&out),
            &r.upstream,
        ))
    })
    .await;
    match sent {
        Ok(resp) => Ok(MediaOutcome {
            ttfb_ms: started.elapsed().as_millis() as i64,
            resp,
            route,
            headers,
            admission,
            chunked: false,
        }),
        Err(e) => Err((Some(Box::new(route)), headers, e)),
    }
}

/// `POST /v1/images/generations` — OpenAI image shape: JSON in, one JSON
/// document with base64 images out.
pub async fn handle_image_generation(state: SharedState, ctx: RequestCtx, body: Value) -> Response {
    let started = Instant::now();
    state.telemetry.request_started();
    let alias = body_alias(&body);
    if let Some(r) = policy_or_refuse(
        &state,
        ClientProto::OpenaiChat,
        &ctx,
        &alias,
        started,
        RequestClass::Image,
    )
    .await
    {
        return r;
    }
    let result = image_json_call(&state, IMAGE_GENERATIONS_ENDPOINT, &body, started).await;
    finish_image(&state, &ctx, alias, started, result).await
}

/// `POST /v1/images/edits` — the multipart twin: `model`, `prompt`, `image`
/// (and optionally `mask`, `n`, `size`, `output_format`), relayed with `model`
/// rewritten exactly as the transcription route relays an upload.
///
/// Multipart only. OpenAI's own edits route takes nothing else, sd-server
/// reads nothing else, and accepting a JSON body here would mean inventing a
/// shape for the image bytes that no upstream would understand.
pub async fn handle_image_edit(
    state: SharedState,
    ctx: RequestCtx,
    req: axum::extract::Request,
) -> Response {
    let started = Instant::now();
    state.telemetry.request_started();
    let is_multipart = is_multipart_content_type(req.headers());
    if !is_multipart {
        let err = GatewayError::BadRequest(format!(
            "{IMAGE_EDITS_ENDPOINT} takes multipart/form-data with the fields model, prompt and \
             image"
        ));
        return finish_image(
            &state,
            &ctx,
            "?".into(),
            started,
            Err((None, GateHeaders::default(), err)),
        )
        .await;
    }

    let (fields, alias) = match buffer_multipart(&state, req).await {
        Ok(v) => v,
        Err((alias, err)) => {
            return finish_image(
                &state,
                &ctx,
                alias,
                started,
                Err((None, GateHeaders::default(), err)),
            )
            .await
        }
    };
    if let Some(r) = policy_or_refuse(
        &state,
        ClientProto::OpenaiChat,
        &ctx,
        &alias,
        started,
        RequestClass::Image,
    )
    .await
    {
        return r;
    }
    // The route guard runs before admission, not just before the send: a row
    // that cannot take a reference image must not have its container started
    // for a request that would kill it. The claim is held until the last byte
    // of the answer has been relayed — see [`MediaOutcome::admission`].
    let check = crate::gate::RouteCheck::Image(IMAGE_EDITS_ENDPOINT);
    let result = match crate::gate::open(&state, &alias, check).await {
        Ok(crate::gate::Opened {
            route,
            hold: admission,
            headers,
        }) => {
            let path = image_upstream_path(IMAGE_EDITS_ENDPOINT);
            let encode = |r: &Route| {
                let url = format!("{}{path}", r.upstream.base());
                Ok(apply_bearer_auth(
                    state
                        .http
                        .post(url)
                        .multipart(reencode_multipart(&fields, r)),
                    &r.upstream,
                ))
            };
            match image_send(admission.as_ref(), &route, encode).await {
                Ok(resp) => Ok(MediaOutcome {
                    ttfb_ms: started.elapsed().as_millis() as i64,
                    resp,
                    route,
                    headers,
                    admission,
                    chunked: false,
                }),
                Err(e) => Err((Some(Box::new(route)), headers, e)),
            }
        }
        Err(f) => Err((f.route, f.headers, f.error)),
    };
    finish_image(&state, &ctx, alias, started, result).await
}

/// `finish_audio` (`proxy/audio.rs`) for the image routes (image-generation design §6): the
/// same relay, the same "log when the last byte is out", the same moment the
/// admission guard is dropped — with the row's own [`RequestClass`], because
/// a generation is not audio traffic and must not average into it.
pub(super) async fn finish_image(
    state: &SharedState,
    ctx: &RequestCtx,
    alias: String,
    started: Instant,
    result: Result<MediaOutcome, Failed>,
) -> Response {
    finish_media(state, ctx, alias, started, RequestClass::Image, result).await
}
