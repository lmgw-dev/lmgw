//! An image call made inside the gateway, returning bytes
//! (chat-media-tools design §4.1): what the `image__*` tools run, through
//! the same gate every `/v1/images/*` request takes.
//!
//! In order: the caller's key policy for the alias, the gate's resolve (the
//! GPU hold's swap, the image route check), the caller's
//! [`Hooks::before_admit`] — where the Chat's tool loop lets its own GPU
//! claim go (§4.3) — then admission, and:
//! - on a **local** image row, sd-server's native job API ([`job`]):
//!   submit `POST /sdcpp/v1/img_gen`, poll `GET /sdcpp/v1/jobs/{id}` until
//!   the job ends, decode its pictures. Every local call takes it, with or
//!   without a preview: it is the path with a job time, and the one a cancel
//!   and previews build on (§5);
//! - on any other route (a hold fallback, a cloud alias), the OpenAI image
//!   route, synchronously ([`openai`]). What its body cannot say — seed,
//!   steps, cfg, negative prompt, strength — is reported as not applied,
//!   never smuggled into the prompt.
//!
//! One `request_logs` row of class `image` per call ([`log`]), charged to
//! the caller's context.
//!
//! **While it runs** (design §5): the stages of bringing the row in — the
//! model an eviction stops, the wait for room ([`vram::stage_watch`]) — and
//! the job's queue place, steps and previews go to the caller's
//! [`Hooks::progress`] ([`observe`]). **A stop** — the caller's, the route's
//! timeout, the call's future dropped, a job that stalled by its own pace
//! on a route with no timeout ([`pace`]) — cancels the job through its guard
//! ([`cancel`]): the image server's own cancel where the build has one for
//! the job's state, else the row's container stopped when the call is its
//! only request, else the job followed to its end holding the claim. The
//! call's error says which.
//!
//! [`vram::stage_watch`]: crate::vram::stage_watch

use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::capabilities::IMAGE_GENERATIONS_ENDPOINT;
use crate::config::Route;
use crate::erased::Erased;
use crate::error::GatewayError;
use crate::gate::{FallbackReason, GateHeaders, RouteCheck};
use crate::ingress::ClientProto;
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::{RequestCtx, StopSignal};

mod answer;
pub(crate) mod body;
mod cancel;
mod job;
mod log;
pub(crate) mod observe;
mod openai;
mod pace;
mod unchanged;
pub(crate) mod upscale;

pub(crate) use observe::{MediaProgress, Steps};

pub(crate) use body::parse_size;

/// A picture going in: an edit's or a variation's source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InputImage {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// What the call does.
#[derive(Debug, Clone)]
pub(crate) enum Action {
    /// A new picture from the prompt.
    Generate,
    /// `image` changed as the prompt says: an instruction edit
    /// (`ref_images`) on a row whose `edit` column is set, a cloud model's
    /// edits route. A local row without the column refuses it — never an
    /// img2img under the name of an edit.
    Edit { image: InputImage },
    /// A variation of `image` (img2img: `init_image` + `strength`), the
    /// prompt describing the whole result: the card's Vary on a local row.
    /// It keeps the picture's composition by construction — a low strength
    /// redraws only the last few of the schedule's steps — so it cannot add
    /// or remove things the way an edit does.
    Vary { image: InputImage },
}

impl Action {
    /// The picture an edit or a variation starts from.
    pub(crate) fn input(&self) -> Option<&InputImage> {
        match self {
            Self::Generate => None,
            Self::Edit { image } | Self::Vary { image } => Some(image),
        }
    }
}

/// The parameters a call sets. Every one but the prompt is optional, and an
/// unset one is left out of the request so the row's own default applies —
/// never an lmgw constant.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Params {
    pub prompt: String,
    pub negative_prompt: Option<String>,
    /// `(width, height)` in pixels.
    pub size: Option<(u32, u32)>,
    pub seed: Option<i64>,
    pub steps: Option<i64>,
    pub cfg: Option<f64>,
    /// A variation's distance from its source, 0 to 1.
    pub strength: Option<f64>,
}

/// One image call.
#[derive(Debug, Clone)]
pub(crate) struct ImageCall<'a> {
    /// The public name asked for (`image/z-image-turbo`, a cloud alias).
    pub alias: &'a str,
    pub action: Action,
    pub params: Params,
}

/// What a caller does at points of a call.
pub(crate) trait Hooks: Send + Sync {
    /// The gate settled on `route`, which admission takes next: the Chat's
    /// tool loop lets its GPU claim go here when `route` lands on the GPU
    /// (design §4.3).
    fn before_admit<'a>(&'a self, route: &'a Route) -> Erased<'a, ()>;

    /// A running call's progress (design §5): sent as it happens, and never
    /// waited on — a caller that cannot keep up drops what it cannot show.
    fn progress(&self, _p: &MediaProgress) {}

    /// How closely the caller follows a job: the preview mode the job is
    /// asked for depends on it (design §5.2).
    fn follows(&self) -> Follows {
        Follows::Steps
    }
}

/// How closely a caller follows a running job ([`Hooks::follows`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Follows {
    /// Its steps at most (`/mcp` progress, or nothing at all): sd-server
    /// reports steps only beside a preview, so the quick sketch is asked
    /// for — the cheapest mode, and free on the measured build — and its
    /// frames are dropped. The steps also give the job its pace (`pace`).
    Steps,
    /// Its steps and previews (the Chat): the row's preview mode.
    Previews,
}

/// No hooks: a caller with no claim to let go.
pub(crate) struct NoHooks;

impl Hooks for NoHooks {
    fn before_admit<'a>(&'a self, _route: &'a Route) -> Erased<'a, ()> {
        Box::pin(async {})
    }
}

/// One picture that came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Picture {
    pub mime: String,
    pub bytes: Vec<u8>,
}

impl Picture {
    /// `(width, height)` from a PNG's header; `None` for any other format.
    pub fn png_size(&self) -> Option<(u32, u32)> {
        body::png_size(&self.bytes)
    }
}

/// The values a picture was made with: the request's, else the server's
/// own defaults as its probed capabilities state them; `None` where neither
/// says (shown as unknown, never guessed).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Effective {
    pub seed: Option<i64>,
    pub steps: Option<i64>,
    pub cfg: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub strength: Option<f64>,
}

/// How an edit went out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditKind {
    /// `ref_images` on an `edit` row: the prompt says what to change.
    Instruction,
    /// `init_image` with `strength`: the prompt describes the result.
    Variation,
    /// The OpenAI edits route of a cloud model.
    Cloud,
}

/// What a call came to (design §4.1).
#[derive(Debug, Clone)]
pub(crate) struct ImageOutput {
    pub images: Vec<Picture>,
    pub effective: Effective,
    /// The job's own time, `completed − started`; `None` off the job API.
    pub job_ms: Option<u64>,
    /// The call's whole time, admission and loading included.
    pub wall_ms: u64,
    /// The alias that answered instead of the one asked for.
    pub answered_by: Option<String>,
    pub fallback: Option<FallbackReason>,
    /// The parameters the call set that the answering route does not take.
    pub not_applied: Vec<&'static str>,
    /// An edit's form; `None` for a generation.
    pub edit: Option<EditKind>,
}

/// What a finished call hands its row writer: the route it went to, the
/// gate's headers, and the outcome.
type Ended = (Option<Box<Route>>, GateHeaders, Result<Done, GatewayError>);

/// A successful send, before the row.
struct Done {
    images: Vec<Picture>,
    effective: Effective,
    job_ms: Option<u64>,
    not_applied: Vec<&'static str>,
    edit: Option<EditKind>,
}

/// Run one image call for `ctx` (design §4.1), its row labelled `proto`.
/// `stop` ends the call at its next await and cancels its job (module doc);
/// the row says `canceled`.
pub(crate) async fn run(
    state: &SharedState,
    ctx: &RequestCtx,
    proto: ClientProto,
    call: ImageCall<'_>,
    hooks: &dyn Hooks,
    stop: Option<&StopSignal>,
) -> Result<ImageOutput, GatewayError> {
    let started = Instant::now();
    // The key's policy for the alias, before anything is resolved or
    // started: a refusal writes its own row.
    super::policy_checked_call(state, proto, ctx, call.alias, RequestClass::Image).await?;
    let in_flight = super::count::InFlight::start(&state.telemetry);
    let taken: cancel::Taken = Arc::new(Mutex::new(None));
    let mut send = Box::pin(open_and_send(
        state,
        &call,
        hooks,
        taken.clone(),
        ctx.origin(),
    ));
    let ended: Option<Ended> = tokio::select! {
        biased;
        () = super::stopped(stop) => None,
        e = &mut send => Some(e),
    };
    let ended: Ended = match ended {
        Some(e) => e,
        None => {
            // Dropped here, so a running job's guard has cancelled it and
            // said how before the error is worded.
            drop(send);
            let how = *taken.lock().unwrap_or_else(|e| e.into_inner());
            (
                None,
                GateHeaders::default(),
                Err(super::canceled(&cancel::cancelled_message(call.alias, how))),
            )
        }
    };
    let (route, headers, result) = ended;
    log::write(
        state,
        (ctx, proto, call.alias),
        started,
        (route.as_deref(), &headers),
        &result,
    )
    .await;
    in_flight.finished();
    let done = result?;
    Ok(ImageOutput {
        images: done.images,
        effective: done.effective,
        job_ms: done.job_ms,
        wall_ms: started.elapsed().as_millis() as u64,
        answered_by: headers.fallback().map(str::to_string),
        fallback: headers.fallback_reason(),
        not_applied: done.not_applied,
        edit: done.edit,
    })
}

/// The gate, the claim hook, admission and the send — as `origin`'s traffic,
/// the caller's request's (per-request background design §7).
async fn open_and_send(
    state: &SharedState,
    call: &ImageCall<'_>,
    hooks: &dyn Hooks,
    taken: cancel::Taken,
    origin: crate::runtime::registry::Origin,
) -> Ended {
    // Every action resolves as a generation: a variation runs on any image
    // row, an edit only on an `edit` row, which the row admission settles
    // on is checked for (`job`). A cloud route is checked for the edits
    // route where it is sent (`openai`).
    let check = RouteCheck::Image(IMAGE_GENERATIONS_ENDPOINT);
    let routed = match crate::gate::resolve(state, call.alias, check, origin).await {
        Ok(r) => r,
        Err(f) => return (f.route, f.headers, Err(f.error)),
    };
    hooks.before_admit(routed.resolved()).await;
    let target = crate::vram::classify(routed.resolved());
    let admit = routed.admit(state);
    let opened = match observe::admit_followed(state, target, call.alias, hooks, admit).await {
        Ok(o) => o,
        Err(f) => return (f.route, f.headers, Err(f.error)),
    };
    let crate::gate::Opened {
        route,
        hold,
        headers,
    } = opened;
    let local = crate::vram::classify(&route)
        .filter(|t| t.class == crate::runtime::Class::Image)
        .map(|t| t.model_id);
    // The route's own ceiling bounds the call, submit to its end
    // (`image.request_timeout_seconds`, 0 = none).
    let ceiling = route.upstream.request_timeout();
    let result = match (&local, hold) {
        (Some(model_id), Some(hold)) => {
            let guard =
                cancel::JobGuard::new(state, hold, &route, model_id, taken.clone(), ceiling);
            let done = local_job(state, (&guard, &route, model_id), call, hooks, ceiling).await;
            let why = guard.let_go_why();
            // Dropped here, so a job let go running is cancelled, and its
            // guard says how before the error is worded.
            drop(guard);
            match why {
                Some(why) => {
                    let how = *taken.lock().unwrap_or_else(|e| e.into_inner());
                    Err(cancel::let_go_error(
                        call.alias,
                        why,
                        how,
                        cancel::Work::Job,
                    ))
                }
                None => done,
            }
        }
        _ => match ceiling {
            Some(d) => tokio::time::timeout(d, openai::run(state, &route, call))
                .await
                .unwrap_or(Err(GatewayError::Timeout)),
            None => openai::run(state, &route, call).await,
        },
    };
    // A picture that comes back byte for byte as it went in is no new
    // picture: said as such, and nothing is stored ([`unchanged`]).
    let result = result.and_then(|d| unchanged::check(call, d));
    (Some(Box::new(route)), headers, result)
}

/// A local row's API root: the route's base without the `/v1` the OpenAI
/// routes carry — sd-server's native routes sit beside them.
fn native_root(route: &Route) -> String {
    let base = route.upstream.base();
    base.trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_string()
}

/// A local row's job under `guard`, within `ceiling`. A call that lets its
/// job go running — the ceiling reached, the job stalled — tells the guard
/// why ([`cancel::JobGuard::let_go`]): its caller drops it, which cancels
/// the job by the rule a stop takes, and words the error from what that
/// did.
async fn local_job(
    state: &SharedState,
    (guard, route, model_id): (&cancel::JobGuard, &Route, &str),
    call: &ImageCall<'_>,
    hooks: &dyn Hooks,
    ceiling: Option<std::time::Duration>,
) -> Result<Done, GatewayError> {
    let run = job::run(state, (guard, route, model_id), call, hooks);
    let done = match ceiling {
        Some(d) => match tokio::time::timeout(d, run).await {
            Ok(done) => done,
            Err(_) => {
                guard.let_go(cancel::LetGo::Timeout(d));
                Err(GatewayError::Timeout)
            }
        },
        None => run.await,
    };
    // Ended, or no longer followed: the claim simply goes — unless the
    // call let the job go running.
    guard.ended();
    done
}
