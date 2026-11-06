//! An ESRGAN upscale made inside the gateway (chat-media-tools design §2,
//! §3.3, §8.3): what `image__upscale` and the Chat card's Upscale run.
//!
//! sd-server's `POST /sdcpp/v1/upscale` is synchronous, needs no prompt and
//! no sampling, and runs on whichever pipeline the container has loaded —
//! but it needs a container, so it is a **local** call only: a cloud route
//! is refused, and so is a row the GPU hold swaps to its fallback (an
//! upscale has nothing a fallback could answer).
//!
//! In order:
//! 1. the row ([`choose`], design §9): a resident row whose capabilities
//!    report `upscale`, preferring the row that made the picture; with none
//!    resident, the row that made it; for a picture no row made, the
//!    enabled row with the smallest planning footprint — among the rows the
//!    caller may use;
//! 2. the caller's key policy for that row, the gate's resolve, the
//!    caller's [`Hooks::before_admit`] (the Chat loop's claim let go), and
//!    admission pinned to the row — never swapped to a fallback;
//! 3. the request through `vram::send_local`, so a container that died is
//!    recovered once, on a task of its own ([`request`]), bounded by the
//!    row's own ceiling (`image.request_timeout_seconds`, 0 = none) and by
//!    its image server answering its readiness route meanwhile;
//! 4. one `request_logs` row of class `image`, `images_out` 1, charged to
//!    the caller.
//!
//! Before admission, the picture it would make is checked against the
//! chosen row's probed `max_upscale_width` / `max_upscale_height`
//! ([`limit`]): a `422 too_large` naming the numbers, not a model loaded
//! for a request the server refuses.
//!
//! A row whose container last reported `upscale: false` is asked again
//! once it is held: sd-server rescans `upscalers/` on every capabilities
//! read (and every upscale), so a file that arrived after the container
//! started is found then, and the registry keeps the new answer. Only a
//! server that still reports none is refused, with the reason.
//!
//! **While it runs** it says `upscaling` to the caller's hooks once the
//! row is admitted (and `loading`, an eviction, the wait for room before
//! that, as a picture does), with what a cancel would do — and again when
//! that changes. **A stop** — the caller's, the route's timeout, the call's
//! future dropped, an image server that stopped answering — takes design
//! §5.4's rule for a request with no job (`cancel`): the row's container
//! is stopped when the upscale is its only request, else the upscale
//! finishes on the image server beside the others, holding its claim until
//! it does (a server that stopped answering: the claim goes at once). The
//! call's error says which.

use std::time::Instant;

use base64::Engine as _;
use serde_json::{json, Value};

use crate::capabilities::IMAGE_GENERATIONS_ENDPOINT;
use crate::config::Route;
use crate::egress::apply_bearer_auth;
use crate::error::GatewayError;
use crate::gate::{GateHeaders, RouteCheck};
use crate::ingress::ClientProto;
use crate::runtime::registry::RuntimeState;
use crate::runtime::Class;
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::super::{RequestCtx, StopSignal};
use super::{Hooks, InputImage, Picture};

mod choose;
mod limit;
mod request;
mod rescan;

pub(crate) use choose::Candidate;

/// One upscale.
#[derive(Debug, Clone)]
pub(crate) struct UpscaleCall<'a> {
    pub image: InputImage,
    /// The upscaler by name (a file stem); the server's first compatible
    /// one when `None`.
    pub upscaler: Option<&'a str>,
    /// 1 to 4; the server's 1 when `None`.
    pub repeats: Option<u32>,
    /// The public name of the model that made the picture, when one did.
    pub made_by: Option<&'a str>,
    /// The public names of the image rows the caller may use (its key's
    /// scope); `None` for every enabled one.
    pub rows: Option<&'a [String]>,
}

/// What an upscale came to.
#[derive(Debug, Clone)]
pub(crate) struct UpscaleOutput {
    pub picture: Picture,
    /// The image row that ran it, by its public name.
    pub model: String,
    /// The upscaler the server used.
    pub upscaler: Option<String>,
    /// Its scale factor per run.
    pub scale: Option<u32>,
    pub repeats: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// The upscale request's own time.
    pub upscale_ms: u64,
    /// The call's whole time, admission and loading included.
    pub wall_ms: u64,
}

/// What the send hands its row writer.
type Ended = (
    Option<Box<Route>>,
    GateHeaders,
    Result<UpscaleOutput, GatewayError>,
);

/// Run one upscale for `ctx` (module doc), its row labelled `proto`. `stop`
/// ends the wait at its next await; the row says `canceled`.
pub(crate) async fn run(
    state: &SharedState,
    ctx: &RequestCtx,
    proto: ClientProto,
    call: UpscaleCall<'_>,
    hooks: &dyn Hooks,
    stop: Option<&StopSignal>,
) -> Result<UpscaleOutput, GatewayError> {
    let started = Instant::now();
    let (alias, model_id) = row_for(state, &call).await?;
    // What the row's last answer says it can make, before anything waits.
    if let Some(caps) = state.runtime().image_capabilities(&model_id) {
        if let Some(refusal) = limit::too_large(state, &alias, &caps, &call) {
            return Err(refusal);
        }
    }
    super::super::policy_checked_call(state, proto, ctx, &alias, RequestClass::Image).await?;
    let in_flight = super::super::count::InFlight::start(&state.telemetry);
    let taken: super::cancel::Taken = Default::default();
    let mut send = Box::pin(open_and_send(
        state,
        &alias,
        &call,
        hooks,
        taken.clone(),
        ctx.origin(),
    ));
    let ended: Option<Ended> = tokio::select! {
        biased;
        () = super::super::stopped(stop) => None,
        e = &mut send => Some(e),
    };
    let ended: Ended = match ended {
        Some(e) => e,
        None => {
            // Dropped here, so its guard has stopped the row (or let the
            // upscale finish) and said which before the error is worded.
            drop(send);
            let how = *taken.lock().unwrap_or_else(|e| e.into_inner());
            (
                None,
                GateHeaders::default(),
                Err(super::super::canceled(&super::cancel::cancelled_message(
                    &alias, how,
                ))),
            )
        }
    };
    let (route, headers, result) = ended;
    super::log::write_counted(
        state,
        (ctx, proto, &alias),
        started,
        (route.as_deref(), &headers),
        result.as_ref().map(|_| 1),
    )
    .await;
    in_flight.finished();
    let mut out = result?;
    out.wall_ms = started.elapsed().as_millis() as u64;
    Ok(out)
}

/// The row that runs it, by its public name and its model id (module doc,
/// step 1).
async fn row_for(
    state: &SharedState,
    call: &UpscaleCall<'_>,
) -> Result<(String, String), GatewayError> {
    let snap = state.snapshot();
    let views = state.runtime().list();
    let mut candidates = Vec::new();
    for row in snap
        .image_models
        .iter()
        .filter(|m| m.enabled && m.modes().iter().any(|m| m == "img_gen"))
    {
        let name = snap.image_public_name(&row.model_id);
        if call.rows.is_some_and(|rows| !rows.contains(&name)) {
            continue;
        }
        let view = views.iter().find(|v| {
            v.class == Class::Image && v.model_id == row.model_id && v.state == RuntimeState::Ready
        });
        candidates.push(Candidate {
            model_id: row.model_id.clone(),
            resident: view.is_some(),
            upscale: view
                .and_then(|v| v.image_capabilities.as_ref())
                .map(|c| c.upscale),
            footprint: state
                .vram
                .footprint(&snap, Class::Image, &row.model_id)
                .await
                .map(|f| f.total_bytes)
                .filter(|b| *b > 0),
        });
    }
    let made_by = call.made_by.and_then(|m| {
        snap.image_models
            .iter()
            .find(|r| snap.image_public_name(&r.model_id) == m)
            .map(|r| r.model_id.as_str())
    });
    choose::choose(&candidates, made_by)
        .map(|id| (snap.image_public_name(id), id.to_string()))
        .ok_or_else(|| GatewayError::Refused {
            status: 422,
            code: "no_image_model",
            message: "upscaling needs a local image model, and no enabled image model can \
                      run it — add one under Models → Image"
                .into(),
        })
}

/// The gate, the claim hook, admission and the send — as `origin`'s traffic,
/// the caller's request's (per-request background design §7).
async fn open_and_send(
    state: &SharedState,
    alias: &str,
    call: &UpscaleCall<'_>,
    hooks: &dyn Hooks,
    taken: super::cancel::Taken,
    origin: crate::runtime::registry::Origin,
) -> Ended {
    let check = RouteCheck::Image(IMAGE_GENERATIONS_ENDPOINT);
    // Pinned: an upscale needs the model itself, so the machine check's
    // verdict does not swap it (machine-check design D97); its start is
    // tried, and a failure answers `gpu_setup`.
    let routed = match crate::gate::resolve_pinned(state, alias, check, origin).await {
        Ok(r) => r,
        Err(f) => return (f.route, f.headers, Err(f.error)),
    };
    if let Some((fb, reason)) = routed.headers().chain().hops().first() {
        // The hold's swap, or another of the walk's: a fallback cannot
        // upscale. Said for the reason it answers, never a hold that is not
        // on.
        return (
            None,
            GateHeaders::default(),
            Err(GatewayError::Refused {
                status: 503,
                code: "upscale_needs_local",
                message: format!(
                    "upscaling needs {alias} itself, and {fb} answers for it right now, because \
                     {}: an upscale has no fallback",
                    reason.because()
                ),
            }),
        );
    }
    hooks.before_admit(routed.resolved()).await;
    let target = crate::vram::classify(routed.resolved());
    let admit = routed.admit_pinned(state);
    let opened = match super::observe::admit_followed(state, target, alias, hooks, admit).await {
        Ok(o) => o,
        Err(f) => return (f.route, f.headers, Err(f.error)),
    };
    let crate::gate::Opened {
        route,
        hold,
        headers,
    } = opened;
    let local = crate::vram::classify(&route).filter(|t| t.class == Class::Image);
    let (Some(target), Some(hold)) = (local, hold) else {
        return (
            Some(Box::new(route)),
            headers,
            Err(GatewayError::Refused {
                status: 422,
                code: "upscale_needs_local",
                message: format!("upscaling needs a local image model, and {alias} is not one"),
            }),
        );
    };
    if let Some(refusal) = rescan::no_upscaler(state, alias, &target.model_id, &hold, &route).await
    {
        return (Some(Box::new(route)), headers, Err(refusal));
    }
    // A row first probed by this start is checked now.
    let caps = state.runtime().image_capabilities(&target.model_id);
    if let Some(refusal) = caps.and_then(|c| limit::too_large(state, alias, &c, call)) {
        return (Some(Box::new(route)), headers, Err(refusal));
    }
    // The row's own ceiling (`image.request_timeout_seconds`, 0 = none).
    let ceiling = route.upstream.request_timeout();
    let guard = super::cancel::JobGuard::new(
        state,
        hold,
        &route,
        &target.model_id,
        taken.clone(),
        ceiling,
    );
    let sent = request::run(
        state,
        (&guard, &route, alias),
        body_of(call),
        hooks,
        ceiling,
    )
    .await;
    // Answered: nothing is left to stop — unless the call let the upscale
    // go running (its timeout, a server that stopped answering), which the
    // guard then ends by the rule a stop takes.
    guard.ended();
    let why = guard.let_go_why();
    drop(guard);
    let out = match why {
        Some(why) => {
            let how = *taken.lock().unwrap_or_else(|e| e.into_inner());
            Err(super::cancel::let_go_error(
                alias,
                why,
                how,
                super::cancel::Work::Upscale,
            ))
        }
        None => sent.map(|mut o| {
            o.model = alias.to_string();
            o
        }),
    };
    (Some(Box::new(route)), headers, out)
}

/// The JSON body of `POST /sdcpp/v1/upscale` for `call`.
fn body_of(call: &UpscaleCall<'_>) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD.encode(&call.image.bytes);
    let mut body = json!({
        "image": format!("data:{};base64,{b64}", call.image.mime),
        "output_format": "png",
    });
    if let Some(u) = call.upscaler {
        body["upscaler"] = json!(u);
    }
    if let Some(r) = call.repeats {
        body["repeats"] = json!(r);
    }
    body
}

/// `POST /sdcpp/v1/upscale` with `body` on the held container, unbounded:
/// the caller bounds it ([`request`]).
async fn send(
    state: &SharedState,
    hold: &crate::vram::LocalHold,
    route: &Route,
    body: &Value,
) -> Result<UpscaleOutput, GatewayError> {
    let started = Instant::now();
    let resp = crate::vram::send_local(Some(hold), route, None, |r| {
        let root = super::native_root(r);
        Ok(apply_bearer_auth(
            state
                .http
                .post(format!("{root}/sdcpp/v1/upscale"))
                .json(body),
            &r.upstream,
        ))
    })
    .await?;
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.map_err(GatewayError::from)?;
    if !(200..300).contains(&status) {
        return Err(super::super::image::image_error(
            route.upstream.protocol,
            status,
            &bytes,
        ));
    }
    let upscale_ms = started.elapsed().as_millis() as u64;
    answered(&bytes, upscale_ms)
}

/// The server's answer read (`images[0].b64_json`, `upscaler`, `scale`,
/// `repeats`, `width`, `height`, `output_format`).
fn answered(bytes: &[u8], upscale_ms: u64) -> Result<UpscaleOutput, GatewayError> {
    let v: Value = serde_json::from_slice(bytes).unwrap_or_default();
    let bad = |m: &str| GatewayError::Upstream {
        status: 502,
        provider_type: None,
        message: format!("the image server's upscale answer {m}"),
    };
    let b64 = v
        .pointer("/images/0/b64_json")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("holds no picture"))?;
    let data = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| bad(&format!("is not base64: {e}")))?;
    let mime = match v.get("output_format").and_then(Value::as_str) {
        Some("jpeg" | "jpg") => "image/jpeg",
        Some("webp") => "image/webp",
        _ => "image/png",
    };
    let n = |k: &str| v.get(k).and_then(Value::as_u64).map(|n| n as u32);
    Ok(UpscaleOutput {
        picture: Picture {
            mime: mime.to_string(),
            bytes: data,
        },
        model: String::new(),
        upscaler: v
            .get("upscaler")
            .and_then(Value::as_str)
            .map(str::to_string),
        scale: n("scale"),
        repeats: n("repeats"),
        width: n("width"),
        height: n("height"),
        upscale_ms,
        wall_ms: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_reads_its_picture_and_facts() {
        let body = json!({
            "images": [{"index": 0, "b64_json": "AQID"}],
            "upscaler": "RealESRGAN_x4plus", "scale": 4, "repeats": 1,
            "width": 256, "height": 256, "output_format": "png",
        });
        let o = answered(body.to_string().as_bytes(), 120).unwrap();
        assert_eq!(o.picture.bytes, vec![1, 2, 3]);
        assert_eq!(o.picture.mime, "image/png");
        assert_eq!(o.upscaler.as_deref(), Some("RealESRGAN_x4plus"));
        assert_eq!(
            (o.scale, o.width, o.height),
            (Some(4), Some(256), Some(256))
        );
        assert!(answered(b"{}", 1).is_err());
    }
}
