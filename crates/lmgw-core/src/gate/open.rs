//! The gate's **per-request half** (unified-KV design §5 stages 1, 2 and 5):
//! from the name a client asked for to a route, and — for a local model — to
//! a container that is up and claimed.
//!
//! In order:
//! 1. **the candidate pick** ([`pick_candidate`], [`super::candidate`]): a
//!    candidate alias is not a model name, so it has to be answered *before*
//!    the name is resolved; it decides its own hold behaviour (§4.2–4.3),
//!    hands back a local chat route here, and picks the candidate at
//!    admission;
//! 2. **the hold swap** for direct model names
//!    ([`Snapshot::resolve_for_request`], gpu-hold design §4) — a local route
//!    under the GPU hold becomes its fallback's route, or `gpu_hold`;
//! 3. **the site's own route check** ([`RouteCheck`]) — made on the route the
//!    bytes will actually go to, before anything is started for it;
//! 4. **admission** ([`admit_or_fall_back`], stage 5): `vram::admit`, which
//!    queues, evicts and starts the container — or, when the VRAM that is
//!    short is VRAM lmgw cannot free, swaps to the model's fallback instead
//!    of queueing (§4.7, [`usable_fallback`]), once that fallback has passed
//!    the site's check ([`fallback_serves`]).
//!
//! The fit check, the ladder climb and the pool reservation (stages 3 and 4)
//! are deliberately *not* here: they need the running server to count on, and
//! they are per **send**, not per request — see [`super::fit`] for why.
//!
//! Two entry points, one pipeline. [`open`] runs it end to end, which is what
//! most sites want. [`resolve`] stops before admission and hands back a
//! [`Routed`] whose [`Routed::admit`] finishes it: for the sites that have
//! work between the two which must not start a container — `/v1/responses`
//! checks the key's budget and loads a stored chain, Admin Chat resolves its
//! tools, the dashboard chat starts its telemetry. **Between the two, a site
//! may read the route but must not act on it being local**: admission may
//! still swap it (§4.7). What a site sends to must come from [`Opened`].

use std::sync::Arc;

use axum::response::Response;

use super::candidate::{self, CandidateCtx, CandidateReq};
use super::ladder::{RungTag, RUNG_HEADER};
use crate::candidates::{Facet, FacetSet};
use crate::config::{FallbackRoute, Route, Snapshot};
use crate::error::GatewayError;
use crate::state::SharedState;
use crate::vram::LocalHold;

/// The site-specific check a route must pass before its container is
/// started — made on the route the gate settled on, so a fallback is judged
/// exactly as if the client had named it (gpu-hold design §4).
///
/// An enum rather than a closure so the gate can keep it and run it again:
/// phase 2 swaps to a fallback route *at admission*, and that route has to
/// pass the same check before anything is sent to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteCheck {
    /// No check: an in-process caller (MCP sampling, agent runs, the
    /// dashboard chats) whose route choice is already a chat model's.
    None,
    /// A text endpoint (`/v1/chat/completions`, `/v1/responses`,
    /// `/v1/count_tokens`, …): an image or audio model is refused by name
    /// before its container starts ([`crate::proxy::refuse_media_route`]).
    Text(&'static str),
    /// Legacy `/v1/completions`: a text endpoint that also only speaks to an
    /// openai-protocol upstream, since the body is forwarded as it came.
    LegacyCompletions,
    /// `/v1/embeddings`: a text endpoint, and a local **rerank** row is
    /// refused — llama-server answers it with an all-zero vector
    /// ([`crate::proxy::refuse_reranker_for_embeddings`]).
    Embeddings,
    /// `/v1/rerank`: a text endpoint, and a local **embedding** row is
    /// refused — it has no head to score with
    /// ([`crate::proxy::refuse_embedder_for_rerank`]).
    Rerank,
    /// `/v1/audio/*` and `/v1/tasks/*`: byte-level passthroughs of OpenAI's
    /// audio shape, so an openai-protocol upstream
    /// ([`crate::proxy::require_openai_audio`]).
    Audio,
    /// `GET /v1/audio/voices`: [`Self::Audio`], and a model that has voices
    /// ([`crate::proxy::refuse_non_audio_voices`]).
    AudioVoices,
    /// `/v1/images/*` (the endpoint named): an openai-protocol upstream, a
    /// local route only onto an image row (and an `edit` one for edits), a
    /// cloud route only when its catalog does not rule it out
    /// ([`crate::proxy::image_route_guard`]).
    Image(&'static str),
}

/// What admission knew about the request a hold was taken for, kept on the
/// hold ([`LocalHold::policy`]) so a later decision about that same request —
/// a ladder climb in the middle of it (ladder design §12 entry 8) — answers the
/// way admission would have, without every send site passing it again.
///
/// Present only when the request **may fall back** ([`Routed::admit`]);
/// pinned callers ([`Routed::admit_pinned`]) and direct `vram::admit` callers
/// (quickdoc's batch runners, `local_model_test`) get none, and a climb of
/// theirs never answers with a fallback. Whether the request is background
/// traffic is the hold's own fact ([`LocalHold::origin`]), not this.
///
/// `Clone`, not `Copy`: a candidate alias's request carries its
/// [`CandidateCtx`] here — what a send that finds the candidate unable to
/// take the request needs to pick again (candidate-aliases §12 entry 45).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionPolicy {
    /// The site's route check — what a fallback must pass to answer this
    /// request ([`fallback_serves`]).
    pub check: RouteCheck,
    /// The request carries image parts ([`Routed::carrying_images`]).
    pub images: bool,
    /// The candidate alias this request came through, and the candidates it
    /// already found could not take it — `None` for every other request.
    pub candidate: Option<Arc<CandidateCtx>>,
}

/// A [`RouteCheck`] refusal.
struct Refusal {
    error: GatewayError,
    /// The refusal is about the model the route names — the aux section
    /// guards — so the request log keeps the route's `upstream_*` columns, as
    /// it always has. Every other check refuses a route before it counts as
    /// settled, and its row has none.
    names_route: bool,
}

impl From<GatewayError> for Refusal {
    fn from(error: GatewayError) -> Self {
        Self {
            error,
            names_route: false,
        }
    }
}

impl RouteCheck {
    async fn run(
        self,
        state: &SharedState,
        snap: &Snapshot,
        route: &Route,
        alias: &str,
    ) -> Result<(), Refusal> {
        let on_route = |error| Refusal {
            error,
            names_route: true,
        };
        match self {
            Self::None => Ok(()),
            Self::Text(endpoint) => Ok(crate::proxy::refuse_media_route(route, alias, endpoint)?),
            Self::LegacyCompletions => {
                crate::proxy::refuse_media_route(route, alias, "/v1/completions")?;
                if route.upstream.protocol == crate::config::Protocol::Openai {
                    Ok(())
                } else {
                    Err(GatewayError::Unsupported(
                        "legacy /v1/completions requires an openai-protocol upstream".into(),
                    )
                    .into())
                }
            }
            Self::Embeddings => {
                crate::proxy::refuse_media_route(route, alias, "/v1/embeddings")?;
                crate::proxy::refuse_reranker_for_embeddings(snap, route, alias).map_err(on_route)
            }
            Self::Rerank => {
                crate::proxy::refuse_media_route(route, alias, "/v1/rerank")?;
                crate::proxy::refuse_embedder_for_rerank(snap, route, alias).map_err(on_route)
            }
            Self::Audio => Ok(crate::proxy::require_openai_audio(route, alias)?),
            Self::AudioVoices => {
                crate::proxy::require_openai_audio(route, alias)?;
                Ok(crate::proxy::refuse_non_audio_voices(route, alias)?)
            }
            Self::Image(endpoint) => {
                Ok(crate::proxy::image_route_guard(state, snap, route, alias, endpoint).await?)
            }
        }
    }
}

/// Why a fallback is answering instead of the requested local model —
/// `x-lmgw-fallback-reason` next to `x-lmgw-fallback` on the response, and the
/// request log's `fallback_reason` (candidate-aliases design §4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// The GPU hold is on (gpu-hold design §2): the swap at resolve time.
    Hold,
    /// VRAM outside lmgw's control is short (§4.7): the swap at admission.
    ExternalVram,
    /// A background candidate alias could not load its primary without
    /// disturbing the owner (§4.3). Phase 4's candidate pick sets it.
    Background,
    /// A candidate alias without background whose primary cannot be used at
    /// all — its row is missing, disabled, or lost a facet the alias
    /// enables — and no other candidate is loaded (§4.2, §12 entry 70):
    /// the owner's order ends "otherwise use fallback".
    Unavailable,
    /// A benchmark run holds the card (benchmark design §3.2): the swap at
    /// resolve time, exactly as under the hold.
    Benchmark,
}

impl FallbackReason {
    /// The header value and the log column's text.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::ExternalVram => "external_vram",
            Self::Background => "background",
            Self::Unavailable => "unavailable",
            Self::Benchmark => "benchmark",
        }
    }
}

/// Every response header the gate owes the client, carried from the moment
/// the route is chosen to the moment a response goes out — success or error.
///
/// That is `x-lmgw-fallback` and `x-lmgw-fallback-reason` (gpu-hold design
/// §4, candidate-aliases design §4.7), always together; `x-lmgw-rung` on a
/// ladder row (ladder design §6); and `x-lmgw-candidate` on a request a
/// candidate alias's model answered (§4.1). One struct with one method that
/// stamps them all, so the sites never change when one is added.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GateHeaders {
    fallback: Option<(String, FallbackReason)>,
    /// The rung the send was judged on, and — when it was handed over — the
    /// rung that served it ([`crate::gate::TurnLease::rung`]).
    rung: Option<RungTag>,
    /// The candidate alias's model the request was given to (§4.1): the
    /// model id, as `x-lmgw-candidate` says it. Gone once a fallback answers.
    candidate: Option<String>,
}

impl GateHeaders {
    /// Record the candidate a candidate alias's walk gave the request to
    /// ([`super::candidate`]).
    pub(crate) fn chose(&mut self, model_id: String) {
        self.candidate = Some(model_id);
    }

    /// The candidate [`Self::chose`] recorded — `None` for every request
    /// that did not come through a candidate alias, and once a fallback
    /// answered.
    pub fn candidate(&self) -> Option<&str> {
        self.candidate.as_deref()
    }

    /// Headers that say `alias` answers instead, and why — a fallback that
    /// answers a request before any of its sends reached a model.
    pub(crate) fn falling_back(alias: String, reason: FallbackReason) -> Self {
        let mut h = Self::default();
        h.fall_back(alias, reason);
        h
    }

    /// Record the rung a ladder row's send was judged on or served from — a
    /// site calls this with its lease's [`crate::gate::TurnLease::rung`] once
    /// the send helper has answered, success or error, and on a refusal by
    /// the fit. `None` (every row without a ladder) leaves the header off.
    pub fn set_rung(&mut self, rung: Option<RungTag>) {
        self.rung = rung;
    }

    /// The rung [`Self::set_rung`] recorded.
    pub fn rung(&self) -> Option<&RungTag> {
        self.rung.as_ref()
    }

    /// The alias that is answering instead of the requested model, when the
    /// gate re-routed it.
    pub fn fallback(&self) -> Option<&str> {
        self.fallback.as_ref().map(|(alias, _)| alias.as_str())
    }

    /// Why [`Self::fallback`] is answering.
    pub fn fallback_reason(&self) -> Option<FallbackReason> {
        self.fallback.as_ref().map(|(_, reason)| *reason)
    }

    /// Record that `alias` answers instead, and why. A rung or a candidate
    /// recorded before — the local model's, judged before a climb handed the
    /// request to the fallback — no longer describes who answers, so they go.
    pub(crate) fn fall_back(&mut self, alias: String, reason: FallbackReason) {
        self.fallback = Some((alias, reason));
        self.rung = None;
        self.candidate = None;
    }

    /// Stamp every gate header on a response — every handler's return path,
    /// errors and streams included.
    ///
    /// The response body's `model` stays the alias the client *asked* for —
    /// that is what `serialize_completion(&ir.model_alias, …)` has always
    /// done, and changing it would break clients that match the echoed name
    /// against their own request. So these headers are the only place the
    /// substitution is visible, and they have to be set at every handler's
    /// return, streams included: the headers of an SSE response go out before
    /// the first event, so a stream carries them exactly like a unary answer.
    ///
    /// An alias that cannot be spelled as a header value (a non-ASCII name) is
    /// logged rather than turned into a failed response: the client got a real
    /// answer, and losing the annotation is not worth losing the answer. The
    /// reason is then left off too — it explains a header that is not there.
    ///
    /// `x-lmgw-rung` goes on a ladder row's every terminal response, errors
    /// and streams included, and never on one a fallback answered. So does
    /// `x-lmgw-candidate` (candidate-aliases §4.1): every response a
    /// candidate alias's model answered or refused — an admission error
    /// about its primary included — and none a fallback answered or a
    /// deferral refused.
    pub fn stamp(&self, mut resp: Response) -> Response {
        use crate::proxy::{CANDIDATE_HEADER, FALLBACK_HEADER, FALLBACK_REASON_HEADER};
        use axum::http::header::{HeaderName, HeaderValue};

        let Some((alias, reason)) = &self.fallback else {
            if let Some(rung) = &self.rung {
                resp.headers_mut()
                    .insert(HeaderName::from_static(RUNG_HEADER), rung.header_value());
            }
            if let Some(model) = &self.candidate {
                match HeaderValue::from_str(model) {
                    Ok(v) => {
                        resp.headers_mut()
                            .insert(HeaderName::from_static(CANDIDATE_HEADER), v);
                    }
                    Err(e) => tracing::warn!(
                        "cannot report the candidate '{model}' in {CANDIDATE_HEADER}: {e}"
                    ),
                }
            }
            return resp;
        };
        match HeaderValue::from_str(alias) {
            Ok(v) => {
                let h = resp.headers_mut();
                h.insert(HeaderName::from_static(FALLBACK_HEADER), v);
                h.insert(
                    HeaderName::from_static(FALLBACK_REASON_HEADER),
                    HeaderValue::from_static(reason.as_str()),
                );
            }
            Err(e) => {
                tracing::warn!("cannot report the fallback '{alias}' in {FALLBACK_HEADER}: {e}")
            }
        }
        resp
    }
}

/// A request that has a route and has not been admitted yet — the half-way
/// point of [`open`] (see the module doc for who stops here and why).
#[derive(Debug)]
pub struct Routed {
    route: Route,
    headers: GateHeaders,
    alias: String,
    /// Kept so admission can run it on the fallback it may swap to
    /// ([`fallback_serves`]): that route is judged exactly as if the client
    /// had named it.
    check: RouteCheck,
    /// The request carries image parts ([`Self::carrying_images`]).
    images: bool,
    /// `Some` when the name is a candidate alias: admission walks its list
    /// ([`super::candidate`]) instead of admitting [`Self::resolved`].
    candidate: Option<CandidateReq>,
}

impl Routed {
    /// The route as resolved. Not yet what the request is sent to —
    /// admission may still swap it to the fallback (§4.7); send to
    /// [`Opened::route`].
    ///
    /// Not named after the field: `tests/it/route_walk.rs` reads every call of a
    /// method by that name in the source as a router registration.
    pub fn resolved(&self) -> &Route {
        &self.route
    }

    pub fn headers(&self) -> &GateHeaders {
        &self.headers
    }

    /// Say that the request carries image parts (a chat site, from
    /// [`crate::gate::media_parts`]). A fallback whose exposed capabilities
    /// say it cannot see images (`vision: false`) is then not one the
    /// outside-VRAM swap may answer with (review finding 7): today's path
    /// would have waited for the local model instead of sending the images to
    /// a model that answers them with a 400. Unknown (`None`) stays usable.
    /// The hold's swap in [`resolve`] is not affected — under the hold there
    /// is no local model to wait for.
    pub fn carrying_images(mut self, images: bool) -> Self {
        self.images = images;
        self
    }

    /// Say which capability facets the request uses
    /// ([`super::request_facets`], [`super::legacy_facets`]) — what a site
    /// that parses a body calls before admission, in place of
    /// [`Self::carrying_images`] (it sets that too, from `vision`).
    ///
    /// For a **candidate alias** it is also the contract check (candidate-
    /// aliases design §4.6 "Requests", §9): a facet the alias does not
    /// enable is refused here with a `400` naming it — before anything is
    /// started, and under the hold as much as without it. Every other name
    /// is never refused by it.
    pub fn using(mut self, uses: FacetSet) -> Result<Self, OpenFailed> {
        self.images = uses.contains(Facet::Vision);
        if let Some(req) = &self.candidate {
            let missing = uses.minus(req.enabled);
            if !missing.is_empty() {
                return Err(OpenFailed {
                    route: None,
                    headers: GateHeaders::default(),
                    error: candidate::facet_refusal(&self.alias, missing),
                });
            }
        }
        Ok(self)
    }

    /// Finish the pipeline: stage 5, admission — which may answer with the
    /// fallback when VRAM outside lmgw's control is short (§4.7).
    pub async fn admit(self, state: &SharedState) -> Result<Opened, OpenFailed> {
        admit_or_fall_back(state, self, AtAdmission::MayFallBack).await
    }

    /// Admission for a caller **pinned** to the model it named: never swapped
    /// to a fallback at admission — it waits for room, exactly as before §4.7.
    ///
    /// quickdoc's in-process embedder and reranker (a corpus pins the model its
    /// vectors come from, a trace names the reranker) and ingest's token count
    /// (it sizes windows with the model's own tokenizer) — the callers the
    /// gpu-hold design §2 already refuses to let fall back. The hold's swap
    /// in [`resolve`] still happens for them, and they refuse it themselves.
    ///
    /// A **candidate alias** is refused here (`400`): it answers each
    /// request from whichever candidate is loaded, so a caller that needs
    /// one model's tokenizer or vectors has to name that model.
    pub async fn admit_pinned(self, state: &SharedState) -> Result<Opened, OpenFailed> {
        admit_or_fall_back(state, self, AtAdmission::Pinned).await
    }
}

/// A request the gate let through: where it goes, and — for a local model —
/// the claim that keeps its container up for as long as the caller holds it.
#[derive(Debug)]
pub struct Opened {
    /// The route to send to. For a local model its `base_url` is already the
    /// container `acquire` came up on (per-model-containers §5), so every
    /// caller forwards to the held port without doing the swap itself.
    pub route: Route,
    /// `Some` exactly when a local container is in this request's future.
    /// Hold it until the response has ended (streams: move it into the relay
    /// task) — see [`LocalHold`] for why loops keep one across turns.
    pub hold: Option<LocalHold>,
    pub headers: GateHeaders,
}

/// A request the gate refused, carrying just enough for each site to log and
/// answer exactly as it did before the gate existed.
#[derive(Debug)]
pub struct OpenFailed {
    /// `None` when no route was settled on (unknown alias, the hold with no
    /// fallback, the site's route check), `Some` when the refusal came from
    /// admission or from an aux section guard (a check about the model the
    /// route names) — the request log's `upstream_*` columns follow this, as
    /// they always have.
    pub route: Option<Box<Route>>,
    /// Empty unless a fallback was already answering when admission refused.
    /// A route-check refusal carries none, as before the gate: the check runs
    /// before anything was sent anywhere.
    pub headers: GateHeaders,
    pub error: GatewayError,
}

/// Stages 1–3 of the per-request half: the candidate pick, the hold swap and
/// the site's route check. Nothing is started here.
pub async fn resolve(
    state: &SharedState,
    alias: &str,
    check: RouteCheck,
) -> Result<Routed, OpenFailed> {
    let snap = state.snapshot();
    let refused = |error| OpenFailed {
        route: None,
        headers: GateHeaders::default(),
        error,
    };
    let picked = match pick_candidate(state, &snap, alias).await {
        Some(picked) => picked.map_err(refused)?,
        None => {
            let mut r = snap.resolve_for_request(alias).map_err(refused)?;
            // A row switched to the CPU is not held, but a container of its
            // still running on the GPU is: its fallback answers, as it did
            // before the switch.
            if r.fallback.is_none() {
                if let Some((fallback, route)) =
                    crate::vram::held_container_fallback(state, &snap, &r.route)
                {
                    r.route = route;
                    r.fallback = Some(fallback);
                }
            }
            let mut headers = GateHeaders::default();
            if let Some(fallback) = r.fallback {
                // The same snapshot answered the swap, so it names the reason:
                // the hold, or a benchmark's lease (benchmark design §3.2).
                let reason = snap
                    .gpu_block()
                    .map_or(FallbackReason::Hold, |b| b.fallback_reason());
                headers.fall_back(fallback, reason);
            }
            Picked {
                route: r.route,
                headers,
                candidate: None,
            }
        }
    };
    if let Err(r) = check.run(state, &snap, &picked.route, alias).await {
        return Err(if r.names_route {
            OpenFailed {
                route: Some(Box::new(picked.route)),
                headers: picked.headers,
                error: r.error,
            }
        } else {
            refused(r.error)
        });
    }
    Ok(Routed {
        route: picked.route,
        headers: picked.headers,
        alias: alias.to_string(),
        check,
        images: false,
        candidate: picked.candidate,
    })
}

/// The whole per-request half: [`resolve`], then admission.
pub async fn open(
    state: &SharedState,
    alias: &str,
    check: RouteCheck,
) -> Result<Opened, OpenFailed> {
    resolve(state, alias, check).await?.admit(state).await
}

/// [`open`] for a caller pinned to the model it named
/// ([`Routed::admit_pinned`]).
pub async fn open_pinned(
    state: &SharedState,
    alias: &str,
    check: RouteCheck,
) -> Result<Opened, OpenFailed> {
    resolve(state, alias, check)
        .await?
        .admit_pinned(state)
        .await
}

/// What the candidate pick settled on.
struct Picked {
    route: Route,
    headers: GateHeaders,
    candidate: Option<CandidateReq>,
}

/// **The candidate pick's resolve half** (candidate-aliases design §4.2–4.3,
/// [`super::candidate`]). `None` for every name that is not an enabled
/// candidate alias, which goes on to [`Snapshot::resolve_for_request`]
/// exactly as before — this runs first because a candidate alias is not a
/// model name and would not resolve.
///
/// For a candidate alias: under the hold, its own fallback straight away
/// (never a candidate's row fallback, §4.1), or `gpu_hold`; otherwise a local
/// chat route, and admission ([`Routed::admit`]) walks the list. Nothing is
/// claimed or started here.
async fn pick_candidate(
    state: &SharedState,
    snap: &Arc<Snapshot>,
    alias: &str,
) -> Option<Result<Picked, GatewayError>> {
    let ca = snap.candidate_alias(alias)?;
    Some(
        candidate::at_resolve(state, snap, ca)
            .await
            .map(|r| Picked {
                route: r.route,
                headers: r.headers,
                candidate: Some(r.req),
            }),
    )
}

/// Whether admission may answer with a fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AtAdmission {
    /// A request: §4.7's verdict may send it to its usable fallback.
    MayFallBack,
    /// A caller pinned to the model it named ([`Routed::admit_pinned`]).
    Pinned,
}

/// **Stage 5, admission, with §4.7's outside-VRAM fallback**
/// (candidate-aliases design §4.7, §5 stage 5).
///
/// A local route with a fallback ([`usable_fallback`]) is admitted through
/// `vram::admit_or_external`, whose verdict comes *before* anything queues:
/// when the VRAM that is short is VRAM lmgw cannot free (measured outside
/// use, not lmgw's own evictable or busy models), and the fallback passes the
/// site's `check` — run only then, on the fallback's route, so it is judged
/// exactly as if the client had named it ([`fallback_serves`]) — the request
/// swaps to the fallback's route and the headers say `x-lmgw-fallback:
/// <alias>` plus `x-lmgw-fallback-reason: external_vram`. Nothing is held: no
/// container was started or claimed. Every other verdict, a fallback that
/// fails the check, and every route without a fallback, is today's admission
/// (`vram::admit`): evict, queue, start, `vram_queue_timeout`,
/// `vram_too_large`. No site changes either way: they send to
/// [`Opened::route`] and stamp [`Opened::headers`].
///
/// Policy is untouched by the swap, exactly as under the hold: scope and
/// budget were checked on the requested alias before the gate, and the
/// served route is what the request is priced on.
///
/// A **candidate alias** walks its list instead ([`super::candidate`]),
/// unless the hold's swap in `resolve` already answered; a pinned caller is
/// refused ([`Routed::admit_pinned`]).
async fn admit_or_fall_back(
    state: &SharedState,
    routed: Routed,
    at: AtAdmission,
) -> Result<Opened, OpenFailed> {
    let Routed {
        route,
        headers,
        alias,
        check,
        images,
        candidate,
    } = routed;
    if candidate.is_some() {
        if at == AtAdmission::Pinned {
            return Err(OpenFailed {
                route: None,
                headers: GateHeaders::default(),
                error: GatewayError::Unsupported(format!(
                    "'{alias}' is a candidate alias, which answers each request from whichever \
                     of its models is loaded — this caller sizes or embeds with one model's own \
                     tokenizer, so it has to name that model"
                )),
            });
        }
        if headers.fallback().is_some() {
            return Ok(Opened {
                route,
                hold: None,
                headers,
            });
        }
        return candidate::walk(state, &alias, None, check, images, Vec::new(), None).await;
    }
    admit_route(state, route, headers, &alias, check, images, at, None).await
}

/// [`admit_or_fall_back`] for one route: stage 5 as its doc comment says —
/// also a candidate alias's primary, loaded through normal admission (§4.2
/// step 3), with `candidate` its context for the hold's policy. The alias
/// fallback answers the outside-VRAM verdict there because
/// [`usable_fallback`] looks the fallback up by the requested name.
#[allow(clippy::too_many_arguments)]
pub(super) async fn admit_route(
    state: &SharedState,
    mut route: Route,
    mut headers: GateHeaders,
    alias: &str,
    check: RouteCheck,
    images: bool,
    at: AtAdmission,
    candidate: Option<Arc<CandidateCtx>>,
) -> Result<Opened, OpenFailed> {
    let alias = alias.to_string();
    // A route a fallback already answers (the hold's swap in `resolve`) is
    // not local: nothing to admit, nothing to swap again.
    let fallback = match at {
        AtAdmission::MayFallBack if headers.fallback().is_none() => {
            usable_fallback_for(state, &route, &alias, candidate.as_deref())
        }
        _ => None,
    };
    let admitted = match fallback {
        Some((fb, fb_route)) => {
            let facets = candidate.as_deref().map(|c| c.enabled(&state.snapshot()));
            let serves = fallback_serves(state, &fb, &fb_route, &alias, check, images, facets);
            let admission = crate::vram::admit_or_external(
                state,
                &route,
                &alias,
                Some(crate::vram::ExternalFallback::confirmed_by(serves)),
            )
            .await;
            match admission {
                Ok(crate::vram::Admission::External(_)) => {
                    headers.fall_back(fb, FallbackReason::ExternalVram);
                    return Ok(Opened {
                        route: fb_route,
                        hold: None,
                        headers,
                    });
                }
                Ok(crate::vram::Admission::Admitted(hold)) => Ok(hold),
                Err(e) => Err(e),
            }
        }
        // Exactly `admit_or_external(.., None)`: no probe, no inspect.
        None => crate::vram::admit(state, &route, &alias).await,
    };
    match admitted {
        Ok(mut hold) => {
            // A local model answers on the port `acquire` just started, not on
            // the class-wide router port the route was resolved against (§5).
            if let Some(h) = &mut hold {
                route.upstream.base_url = h.endpoint();
                if at == AtAdmission::MayFallBack {
                    h.set_policy(AdmissionPolicy {
                        check,
                        images,
                        candidate,
                    });
                }
            }
            Ok(Opened {
                route,
                hold,
                headers,
            })
        }
        Err(error) => Err(OpenFailed {
            route: Some(Box::new(route)),
            headers,
            error,
        }),
    }
}

/// The fallback §4.7's outside-VRAM verdict may answer `route` with: the
/// request's usable fallback ([`Snapshot::request_fallback`] — its row's, or
/// a candidate alias's own) — the alias and its route. Cheap: a snapshot
/// read and the registry's in-memory list, nothing that probes or fetches.
/// Whether the fallback may take *this* request is [`fallback_serves`],
/// asked only once a verdict says it could answer.
///
/// `None` means admission is today's, whatever the verdict would say:
/// - the verdict cannot change this admission (a cloud route, a model that is
///   up, the switch or admission off, the hold on —
///   [`crate::vram::external_armed`]); nothing is looked up then;
/// - the row has no fallback;
/// - the fallback does not resolve, or is itself local: a misconfiguration,
///   logged as a warning. Under the hold it is a 503 naming it; here the
///   local model may still serve the request, so it waits as it always has.
///
/// A non-background candidate alias's primary answers with the alias's own
/// fallback, never its row fallback (§4.7 "applies to", §4.1): `alias` is
/// the requested name, and [`Snapshot::request_fallback`] reads it. Every
/// other name reads exactly [`Snapshot::fallback_route`], as before.
pub fn usable_fallback(state: &SharedState, route: &Route, alias: &str) -> Option<(String, Route)> {
    usable_fallback_for(state, route, alias, None)
}

/// [`usable_fallback`] for a request that already carries its candidate
/// alias ([`CandidateCtx`]): that alias's fallback, read from the alias as
/// the request was picked when it has since been disabled or deleted
/// (§12 entry 86).
fn usable_fallback_for(
    state: &SharedState,
    route: &Route,
    alias: &str,
    candidate: Option<&CandidateCtx>,
) -> Option<(String, Route)> {
    let snap = state.snapshot();
    let target = crate::vram::external_armed(state, &snap, route)?;
    let fallback = match candidate {
        Some(c) => snap.alias_fallback(c.alias_row(&snap)),
        None => snap.request_fallback(alias, &target),
    };
    match fallback {
        FallbackRoute::None => None,
        FallbackRoute::Unusable { alias: fb, why } => {
            tracing::warn!(
                "'{alias}' ({}/{}) is not loaded, and its fallback '{fb}' {why} — it cannot \
                 answer when VRAM outside lmgw's control is short, so the request waits for \
                 room instead",
                target.class.as_str(),
                target.model_id
            );
            None
        }
        FallbackRoute::Usable {
            alias: fb,
            route: fb_route,
        } => Some((fb, fb_route)),
    }
}

/// May the fallback [`usable_fallback`] found take *this* request? Asked only
/// once §4.7's verdict says `External` (review finding 6): for the image
/// routes the check reads the fallback upstream's catalog, which a request
/// that fits must not pay for.
///
/// - The site's `check` on the fallback's route, judged as if the client had
///   named it. A fallback this endpoint could not use (an anthropic-protocol
///   fallback for legacy `/v1/completions`, a chat alias for an image route)
///   counts as none, rather than turning a request that would have waited
///   into an error (§12 entry 28).
/// - A request that carries images ([`Routed::carrying_images`]) and a
///   fallback whose exposed capabilities say `vision: false` (review finding
///   7): the local model may still take the images, so the request waits for
///   it. Unknown vision stays usable — absent means unknown.
/// - the request came through a **candidate alias** and the fallback does
///   not positively support every facet it enables (§4.6: "the fallback is
///   treated as none") — `facets`, the enabled set the caller carries
///   ([`CandidateCtx::enabled`], so an alias disabled or deleted mid-flight
///   still holds its fallback to it, §12 entry 86), or else the enabled set
///   of the candidate alias `alias` names. Every path a candidate alias's
///   fallback answers on comes through here — the outside-VRAM swap, a
///   climb's, the hold's during a climb, the background walk's — so the rule
///   is one place. Other names are unchanged. The fallback's own
///   capabilities are read live here, from the catalog cache, on every
///   call: nothing about them is cached with the candidate pick.
///
/// `false` means today's path: the request queues for the local model.
///
/// Reads the snapshot current when it runs, which can be well into the wait
/// (the verdict is taken again while a request waits, §12 entry 44).
pub(crate) async fn fallback_serves(
    state: &SharedState,
    fb: &str,
    fb_route: &Route,
    alias: &str,
    check: RouteCheck,
    images: bool,
    facets: Option<FacetSet>,
) -> bool {
    let snap = state.snapshot();
    if let Err(r) = check.run(state, &snap, fb_route, alias).await {
        tracing::debug!(
            "fallback '{fb}' of '{alias}' is not one this endpoint can use ({}) — admission \
             waits for room instead",
            r.error
        );
        return false;
    }
    let enabled = facets.or_else(|| {
        snap.candidate_alias(alias)
            .map(|ca| FacetSet::from_names(&ca.capabilities_enabled).unwrap_or_default())
    });
    if let Some(enabled) = enabled {
        if let Err(f) = crate::candidates::derive::fallback_supports(state, fb, enabled).await {
            tracing::warn!(
                "fallback '{fb}' of candidate alias '{alias}' lacks {}, which the alias enables \
                 — it counts as none",
                f.as_str()
            );
            return false;
        }
    }
    if images {
        let vision = crate::capabilities::exposed::exposed_entry(state, fb)
            .await
            .and_then(|e| e.capabilities)
            .and_then(|c| c.vision);
        if vision == Some(false) {
            tracing::debug!(
                "fallback '{fb}' of '{alias}' cannot see images and this request carries some \
                 — admission waits for room instead"
            );
            return false;
        }
    }
    true
}
