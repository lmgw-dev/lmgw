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
//! 2. **the hold swap** for direct model names (gpu-hold design §4, along
//!    the chain: [`super::chain::walk_static`]) — a local route under the GPU
//!    hold goes on to its fallback, hop by hop, or `gpu_hold`;
//! 3. **the site's own route check** ([`RouteCheck`]) — made on the route the
//!    bytes will actually go to, before anything is started for it;
//! 4. **admission** ([`admit_or_fall_back`], stage 5): `vram::admit`, which
//!    queues, evicts and starts the container — or, when the VRAM that is
//!    short is VRAM lmgw cannot free, swaps to the model's fallback instead
//!    of queueing (§4.7, [`usable_fallback`]), once that fallback has passed
//!    the site's check ([`fallback_serves`]). A request whose origin is
//!    background (`x-lmgw-background: on`) is admitted as a guest instead:
//!    [`super::guest`] on a plain local model, a guest walk on a candidate
//!    alias (per-request background design §4–§5).
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
use super::chain::{self, Chain, Next, Walk};
use super::ladder::{RungTag, RUNG_HEADER};
use crate::candidates::FacetSet;
use crate::config::{FallbackRoute, HopKey, Route, Snapshot};
use crate::error::GatewayError;
use crate::runtime::registry::Origin;
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
    /// An in-process caller (MCP sampling, agent runs, the dashboard chats,
    /// a profile's Try) whose route choice is already a chat model's: only
    /// the decision guard, which reads the snapshot and nothing else, so a
    /// decision row a setting names is refused before its container starts
    /// ([`crate::proxy::refuse_decision_row`], decision-models design §2.2).
    None,
    /// A generation endpoint (`/v1/chat/completions`, `/v1/messages`,
    /// `/v1/responses`, realtime): an image or audio model is refused by name
    /// before its container starts ([`crate::proxy::refuse_media_route`]),
    /// and so is a local **decision** row, which cannot generate
    /// ([`crate::proxy::refuse_decision_row`], decision-models design §2.2),
    /// and a local embedding or rerank row
    /// ([`crate::proxy::refuse_encoder_for_generation`]).
    Text(&'static str),
    /// A token counter (`/tokenize`, `/v1/count_tokens`,
    /// `/v1/messages/count_tokens`): only the media guard. A decision row
    /// counts tokens like any llama-server, and `/tokenize` is how a client
    /// sizes a state against its ubatch (decision-models design §2.2).
    Count(&'static str),
    /// Legacy `/v1/completions`: [`Self::Text`] on an endpoint that also only
    /// speaks to an openai-protocol upstream, since the body is forwarded as
    /// it came.
    LegacyCompletions,
    /// `/v1/embeddings`: a text endpoint, and a local **rerank** or
    /// **decision** row is refused — llama-server answers a reranker with an
    /// all-zero vector ([`crate::proxy::refuse_reranker_for_embeddings`]).
    Embeddings,
    /// `/v1/rerank`: a text endpoint, and a local **embedding** or
    /// **decision** row is refused — neither has a head to score with
    /// ([`crate::proxy::refuse_embedder_for_rerank`]).
    Rerank,
    /// `/v1/systemone`: a local aux row of kind decision, or an
    /// `openai_compatible` or `llama_cpp` upstream whose catalog does not
    /// rule the model out; every
    /// other route is a `501` ([`crate::proxy::decision_route_guard`],
    /// decision-models design §2.2).
    Decision,
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

impl RouteCheck {
    /// Whether an exhausted upstream quota hands this request to the
    /// fallback (openai egress design §14.4): chat and Responses requests —
    /// the generation endpoints and the in-process chat callers — only yet.
    /// Embeddings, audio, images and legacy completions keep the quota
    /// error.
    pub fn quota_falls_back(self) -> bool {
        matches!(self, Self::None | Self::Text(_))
    }
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
    /// The candidate alias this request came through, and the candidates it
    /// already found could not take it — `None` for every other request.
    pub candidate: Option<Arc<CandidateCtx>>,
    /// The hops the request was handed to before this one (openai egress
    /// design §14.3), so a fallback chosen at the send goes on along the
    /// same chain, its visited set included.
    pub chain: Chain,
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
            Self::None => {
                crate::proxy::refuse_decision_row(snap, route, alias, None).map_err(on_route)?;
                crate::proxy::refuse_encoder_for_generation(snap, route, alias, "a chat turn")
                    .map_err(on_route)
            }
            Self::Text(endpoint) => {
                crate::proxy::refuse_media_route(route, alias, endpoint)?;
                crate::proxy::refuse_decision_row(snap, route, alias, Some(endpoint))
                    .map_err(on_route)?;
                crate::proxy::refuse_encoder_for_generation(snap, route, alias, endpoint)
                    .map_err(on_route)
            }
            Self::Count(endpoint) => Ok(crate::proxy::refuse_media_route(route, alias, endpoint)?),
            Self::LegacyCompletions => {
                crate::proxy::refuse_media_route(route, alias, "/v1/completions")?;
                crate::proxy::refuse_decision_row(snap, route, alias, Some("/v1/completions"))
                    .map_err(on_route)?;
                crate::proxy::refuse_encoder_for_generation(snap, route, alias, "/v1/completions")
                    .map_err(on_route)?;
                // llama-server serves /v1/completions as well.
                if route.upstream.protocol.speaks_openai_http() {
                    Ok(())
                } else {
                    Err(GatewayError::Unsupported(
                        "legacy /v1/completions requires an OpenAI-shaped upstream (openai, openai_compatible or llama_cpp)".into(),
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
            // A refusal about a local aux row names it, as the aux section
            // guards' do; one about any other route does not.
            Self::Decision => crate::proxy::decision_route_guard(state, snap, route, alias)
                .await
                .map_err(|error| Refusal {
                    error,
                    names_route: snap.aux_model_for(route).is_some(),
                }),
        }
    }
}

/// Why a fallback is answering instead of the requested model or alias —
/// `x-lmgw-fallback-reason` next to `x-lmgw-fallback` on the response, each
/// hop's in `x-lmgw-fallback-chain`, and the request log's `fallback_reason`
/// (candidate-aliases design §4.7, openai egress design §14.5).
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
    /// A cloud hop's upstream answered that its account's quota is
    /// exhausted (`insufficient_quota`), or is remembered so (openai egress
    /// design §14.4, §14.6).
    Quota,
    /// A cloud hop's upstream could not be connected to: nothing reached it
    /// and nothing reached the client (§14.4, decision 28).
    Unreachable,
    /// The name is a disabled row or alias, or an alias whose upstream row
    /// is disabled (§14.3, decision 28): decided at resolve.
    Disabled,
    /// A local model's start on the graphics card failed and the machine
    /// check names the card, its driver or what lets models use it
    /// (machine-check design §2.5, D23): handed on at admission. While that
    /// check still describes the machine, a GPU hop with a fallback is
    /// swapped at resolve for the causes it can tell are still so, like
    /// under the hold (`gate::machine_setup`, D95).
    GpuSetup,
    /// The engine image a local model runs in could not be downloaded
    /// before its start (machine-check design §5, D36): handed on at
    /// admission.
    EngineDownload,
}

impl FallbackReason {
    /// Every reason, in the order `x-lmgw-fallback-reason`'s enum lists
    /// them (a test holds the two together).
    pub const ALL: [Self; 10] = [
        Self::Hold,
        Self::ExternalVram,
        Self::Background,
        Self::Unavailable,
        Self::Benchmark,
        Self::Quota,
        Self::Unreachable,
        Self::Disabled,
        Self::GpuSetup,
        Self::EngineDownload,
    ];

    /// Why the fallback answers, as a clause a person reads after "because"
    /// — what a caller that refuses every fallback (a pinned one, an
    /// upscale) says, so it never names a hold that is not on.
    pub fn because(self) -> &'static str {
        match self {
            Self::Hold => "the GPU hold is on",
            Self::ExternalVram => "the graphics card's memory is in use outside lmgw",
            Self::Background => "background traffic does not disturb the owner's models",
            Self::Unavailable => "the model cannot serve this request",
            Self::Benchmark => "a benchmark run has the graphics card to itself",
            Self::Quota => "its provider's quota is used up",
            Self::Unreachable => "its provider could not be reached",
            Self::Disabled => "it is disabled",
            Self::GpuSetup => "it cannot start on the graphics card of the computer lmgw runs on",
            Self::EngineDownload => "its engine could not be downloaded",
        }
    }

    /// The header value and the log column's text.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::ExternalVram => "external_vram",
            Self::Background => "background",
            Self::Unavailable => "unavailable",
            Self::Benchmark => "benchmark",
            Self::Quota => "quota",
            Self::Unreachable => "unreachable",
            Self::Disabled => "disabled",
            Self::GpuSetup => "gpu_setup",
            Self::EngineDownload => "engine_download",
        }
    }
}

/// Every response header the gate owes the client, carried from the moment
/// the route is chosen to the moment a response goes out — success or error.
///
/// That is `x-lmgw-fallback` and `x-lmgw-fallback-reason` (gpu-hold design
/// §4, candidate-aliases design §4.7), always together, and
/// `x-lmgw-fallback-chain` beside them (openai egress design §14.5);
/// `x-lmgw-rung` on a ladder row (ladder design §6); and `x-lmgw-candidate`
/// on a request a candidate alias's model answered (§4.1). One struct with
/// one method that stamps them all, so the sites never change when one is
/// added.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GateHeaders {
    /// The hops the request was handed to: the last answers.
    chain: Chain,
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

    /// Headers that say the request was handed along `chain`.
    pub(crate) fn along(chain: Chain) -> Self {
        Self {
            chain,
            ..Self::default()
        }
    }

    /// The hops the request was handed to (§14.3).
    pub fn chain(&self) -> &Chain {
        &self.chain
    }

    /// The request was handed on along `chain` after these headers were
    /// taken (a send found the model could not take it): the chain is that
    /// one now, and a rung or a candidate recorded before — the model's,
    /// judged before it handed the request on — no longer describes who
    /// answers, so they go.
    pub(crate) fn handed_on(&mut self, chain: Chain) {
        self.chain = chain;
        self.rung = None;
        self.candidate = None;
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

    /// The model or alias that is answering instead of the one requested,
    /// when the gate re-routed it: the last hop of the chain.
    pub fn fallback(&self) -> Option<&str> {
        self.chain.last().map(|(name, _)| name)
    }

    /// Why [`Self::fallback`] is answering: the last hop's reason.
    pub fn fallback_reason(&self) -> Option<FallbackReason> {
        self.chain.last().map(|(_, reason)| reason)
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
        use crate::proxy::{
            CANDIDATE_HEADER, FALLBACK_CHAIN_HEADER, FALLBACK_HEADER, FALLBACK_REASON_HEADER,
        };
        use axum::http::header::{HeaderName, HeaderValue};

        let Some((alias, reason)) = self.chain.last() else {
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
        // Every hop, on every answer a fallback served (§14.5): with one hop
        // it repeats the two above, so a client reads one header whatever
        // the chain's length.
        if let Some(v) = self.chain.header() {
            resp.headers_mut()
                .insert(HeaderName::from_static(FALLBACK_CHAIN_HEADER), v);
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
    /// The hop the route is for: the name asked for, or the fallback the
    /// static walk reached (§14.3) — what admission admits, and looks a
    /// further fallback up by.
    hop: String,
    /// Kept so admission can run it on the fallback it may swap to
    /// ([`fallback_serves`]): that route is judged exactly as if the client
    /// had named it.
    check: RouteCheck,
    /// `Some` when the name is a candidate alias: admission walks its list
    /// ([`super::candidate`]) instead of admitting [`Self::resolved`].
    candidate: Option<CandidateReq>,
    /// Whose traffic the request is (per-request background design §3):
    /// [`Origin::Background`] makes admission a guest's — a candidate
    /// alias's guest walk, or [`super::guest`] on a plain local model.
    origin: Origin,
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

    /// Say which capability facets the request uses
    /// ([`super::request_facets`], [`super::legacy_facets`]) — what a site
    /// that parses a body calls before admission.
    ///
    /// For a **candidate alias** it is also the contract check (candidate-
    /// aliases design §4.6 "Requests", §9): a facet the alias does not
    /// enable is refused here with a `400` naming it — before anything is
    /// started, and under the hold as much as without it. Every other name
    /// is never refused by it.
    pub fn using(self, uses: FacetSet) -> Result<Self, OpenFailed> {
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

    /// Refuse a structured output the model that would answer is known not
    /// to produce (structured-output design §7): `kind` is the JSON type the
    /// request asks for, `None` when it asks for none. The model judged is
    /// the hold's fallback when the hold swapped the route, else the name the
    /// client sent; its flag must say `false` — an absent flag lets the
    /// request go. Before admission, so nothing is started for it.
    pub async fn serves_format(
        self,
        state: &SharedState,
        kind: Option<crate::ir::FormatKind>,
    ) -> Result<Self, OpenFailed> {
        let Some(kind) = kind else {
            return Ok(self);
        };
        let judged = self.route.fallback.as_deref().unwrap_or(&self.alias);
        match super::structured::refusal(state, judged, kind).await {
            None => Ok(self),
            Some(error) => Err(OpenFailed {
                route: Some(Box::new(self.route)),
                headers: self.headers,
                error,
            }),
        }
    }

    /// A site's own refusal of its body for the route as resolved, before
    /// admission, so nothing is started for a body that route cannot take
    /// (structured-output design §11.22: llama.cpp's legacy completions).
    /// Where admission swaps the route, the body is judged again for the
    /// route it is sent on.
    pub fn takes_body(
        self,
        check: impl FnOnce(&Route) -> Result<(), GatewayError>,
    ) -> Result<Self, OpenFailed> {
        match check(&self.route) {
            Ok(()) => Ok(self),
            Err(error) => Err(OpenFailed {
                route: Some(Box::new(self.route)),
                headers: self.headers,
                error,
            }),
        }
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
    /// gpu-hold design §2 already refuses to let fall back. They resolve with
    /// [`resolve_pinned`]: the hold's swap still happens for them, and they
    /// refuse it themselves; the machine check's does not.
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
///
/// The hold's swap walks the chain (openai egress design §14.3,
/// [`chain::walk_static`]): a local route under the hold, or a benchmark's
/// lease, goes on to its fallback, and from a hop the snapshot stops too to
/// that hop's own, each hop judged by the site's check, until a hop can be
/// tried — which admission then admits, a local one included.
///
/// `origin` is whose traffic the request is (per-request background design
/// §3): an HTTP request's `RequestCtx::origin` — [`Origin::Background`] for
/// `x-lmgw-background: on` — and [`Origin::Owner`] for an in-process caller
/// that does not act for one. It changes nothing here, since the hold's swap
/// is the same for a guest (§4 step 1); admission reads it.
///
/// Erased ([`crate::erased`]), with admission ([`admit_or_fall_back`]): the
/// candidate pick, the route checks and VRAM admission behind one future,
/// which some twenty trait methods await.
pub fn resolve<'a>(
    state: &'a SharedState,
    alias: &'a str,
    check: RouteCheck,
    origin: Origin,
) -> crate::erased::Erased<'a, Result<Routed, OpenFailed>> {
    resolve_as(state, alias, check, origin, AtAdmission::MayFallBack)
}

/// [`resolve`] for a caller pinned to the model it named, which admits
/// with [`Routed::admit_pinned`]: the same, except that the model it named
/// is never swapped at resolve for the machine check's verdict
/// (machine-check design D97). Its start is tried, and a start this
/// computer cannot make answers it `gpu_setup` at admission — never a
/// fallback's answer it would only refuse, worded as a hold that is not on.
/// The hold's and a benchmark's swap still happen, and the caller refuses
/// those itself.
pub fn resolve_pinned<'a>(
    state: &'a SharedState,
    alias: &'a str,
    check: RouteCheck,
    origin: Origin,
) -> crate::erased::Erased<'a, Result<Routed, OpenFailed>> {
    resolve_as(state, alias, check, origin, AtAdmission::Pinned)
}

/// [`resolve`] for a caller that admits `at`.
fn resolve_as<'a>(
    state: &'a SharedState,
    alias: &'a str,
    check: RouteCheck,
    origin: Origin,
    at: AtAdmission,
) -> crate::erased::Erased<'a, Result<Routed, OpenFailed>> {
    futures::FutureExt::boxed(async move {
        let snap = state.snapshot();
        let refused = |headers: GateHeaders, error| OpenFailed {
            route: None,
            headers,
            error,
        };
        let picked = match pick_candidate(state, &snap, alias, check).await {
            Some(picked) => picked.map_err(|e| refused(GateHeaders::default(), e))?,
            None => {
                let mut chain = Chain::default();
                let route = match snap.resolve(alias) {
                    Ok(route) => route,
                    // A disabled model or alias (§14.3, decision 28): its
                    // own fallback answers, along the chain; without one,
                    // today's answer.
                    Err(e) => {
                        let Some(key) = snap.disabled_key(alias) else {
                            return Err(refused(GateHeaders::default(), e));
                        };
                        let next = snap.next_hop(&key);
                        if matches!(next, FallbackRoute::None) {
                            return Err(refused(GateHeaders::default(), e));
                        }
                        let stop = chain::Stop::disabled(e, key);
                        let (hop, route) = chain::walk_static(
                            state,
                            &snap,
                            check,
                            &mut chain,
                            alias.to_string(),
                            None,
                            stop,
                            next,
                        )
                        .await
                        .map_err(|e| refused(GateHeaders::along(chain.clone()), e))?;
                        return Ok(Routed {
                            route,
                            headers: GateHeaders::along(chain),
                            alias: alias.to_string(),
                            hop,
                            check,
                            candidate: None,
                            origin,
                        });
                    }
                };
                let key = snap.hop_key(alias);
                let stop = key
                    .as_ref()
                    .and_then(|k| chain::stop_at(state, &snap, &route, k, check, at));
                let (hop, route) = match stop {
                    None => (alias.to_string(), route),
                    Some(stop) => {
                        let next = snap.next_hop(&stop.key);
                        let here = stop.tries_hop().then(|| route.clone());
                        chain::walk_static(
                            state,
                            &snap,
                            check,
                            &mut chain,
                            alias.to_string(),
                            here,
                            stop,
                            next,
                        )
                        .await
                        .map_err(|e| refused(GateHeaders::along(chain.clone()), e))?
                    }
                };
                Picked {
                    route,
                    headers: GateHeaders::along(chain),
                    candidate: None,
                    hop,
                }
            }
        };
        // A hop the walk reached was judged by the check on the way.
        if picked.headers.fallback().is_none() {
            if let Err(r) = check.run(state, &snap, &picked.route, alias).await {
                return Err(if r.names_route {
                    OpenFailed {
                        route: Some(Box::new(picked.route)),
                        headers: picked.headers,
                        error: r.error,
                    }
                } else {
                    refused(GateHeaders::default(), r.error)
                });
            }
        }
        Ok(Routed {
            route: picked.route,
            headers: picked.headers,
            alias: alias.to_string(),
            hop: picked.hop,
            check,
            candidate: picked.candidate,
            origin,
        })
    })
}

/// The whole per-request half: [`resolve`], then admission.
pub async fn open(
    state: &SharedState,
    alias: &str,
    check: RouteCheck,
    origin: Origin,
) -> Result<Opened, OpenFailed> {
    resolve(state, alias, check, origin)
        .await?
        .admit(state)
        .await
}

/// [`open`] for a caller pinned to the model it named
/// ([`resolve_pinned`], [`Routed::admit_pinned`]).
pub async fn open_pinned(
    state: &SharedState,
    alias: &str,
    check: RouteCheck,
    origin: Origin,
) -> Result<Opened, OpenFailed> {
    resolve_pinned(state, alias, check, origin)
        .await?
        .admit_pinned(state)
        .await
}

/// What the candidate pick settled on.
struct Picked {
    route: Route,
    headers: GateHeaders,
    candidate: Option<CandidateReq>,
    /// The hop `route` is for (the requested name, or the fallback the
    /// static walk reached).
    hop: String,
}

/// **The candidate pick's resolve half** (candidate-aliases design §4.2–4.3,
/// [`super::candidate`]). `None` for every name that is not an enabled
/// candidate alias, which goes on to the hold's swap — this runs first
/// because a candidate alias is not a model name and would not resolve.
///
/// For a candidate alias: under the hold, its own fallback straight away
/// (never a candidate's row fallback, §4.1), along its chain, or
/// `gpu_hold`; otherwise a local chat route, and admission
/// ([`Routed::admit`]) walks the list. Nothing is claimed or started here.
async fn pick_candidate(
    state: &SharedState,
    snap: &Arc<Snapshot>,
    alias: &str,
    check: RouteCheck,
) -> Option<Result<Picked, GatewayError>> {
    let ca = snap.candidate_alias(alias)?;
    Some(
        candidate::at_resolve(state, snap, ca, check)
            .await
            .map(|r| Picked {
                route: r.route,
                headers: r.headers,
                candidate: Some(r.req),
                hop: r.hop,
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
/// goes on to the fallback (openai egress design §14.3): a cloud hop answers
/// at once (`x-lmgw-fallback: <alias>`, `x-lmgw-fallback-reason:
/// external_vram`), a local one is admitted in turn. Every other verdict, a
/// fallback that fails the check, and every route without a fallback, is
/// today's admission (`vram::admit`): evict, queue, start,
/// `vram_queue_timeout`, `vram_too_large`. A GPU hold or benchmark lease
/// that admission meets is answered by the hop's fallback, as the swap in
/// [`resolve`] would have answered it ([`held`]); so is a start this
/// computer could not make, or whose engine image could not be downloaded
/// (`gpu_setup`, `engine_download`: [`super::machine_setup`]). No site
/// changes either way: they send to [`Opened::route`] and stamp
/// [`Opened::headers`].
///
/// A hop the static walk at resolve reached is admitted the same way, as a
/// request naming it would be: a cloud hop answers as it is, a local one
/// (a row on the CPU under the hold) is admitted.
///
/// Policy is untouched by the swap, exactly as under the hold: scope and
/// budget were checked on the requested alias before the gate, and the
/// served route is what the request is priced on.
///
/// A **candidate alias** walks its list instead ([`super::candidate`]),
/// unless the hold's swap in `resolve` already answered; a pinned caller is
/// refused ([`Routed::admit_pinned`]). A **background request** for a plain
/// local model is a guest's admission ([`super::guest`], per-request
/// background design §4), and one for a candidate alias walks as a guest
/// (§5).
///
/// Erased ([`crate::erased`]), for the reason [`resolve`] is.
fn admit_or_fall_back(
    state: &SharedState,
    routed: Routed,
    at: AtAdmission,
) -> crate::erased::Erased<'_, Result<Opened, OpenFailed>> {
    futures::FutureExt::boxed(async move {
        let Routed {
            route,
            headers,
            alias,
            hop,
            check,
            candidate,
            origin,
        } = routed;
        if candidate.is_some() && at == AtAdmission::Pinned {
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
        // A hop the hold's swap reached: a cloud hop answers as it is, a
        // local one is admitted as a request naming it would be.
        if headers.fallback().is_some() {
            if crate::vram::classify(&route).is_none() {
                return Ok(Opened {
                    route,
                    hold: None,
                    headers,
                });
            }
            if origin == Origin::Background {
                return super::guest::admit(state, route, headers, &hop, check, at).await;
            }
            return admit_route(state, route, headers, &hop, check, at, None).await;
        }
        if candidate.is_some() {
            return candidate::walk(state, &alias, None, check, Vec::new(), None, origin).await;
        }
        if origin == Origin::Background {
            return super::guest::admit(state, route, headers, &alias, check, at).await;
        }
        admit_route(state, route, headers, &alias, check, at, None).await
    })
}

/// [`admit_or_fall_back`] for one route: stage 5 as its doc comment says —
/// also a candidate alias's primary, loaded through normal admission (§4.2
/// step 3), with `candidate` its context for the hold's policy, and a local
/// hop of a chain, `alias` then that hop's name. The fallback answers the
/// outside-VRAM verdict there because [`usable_fallback`] looks it up by
/// `alias`: a candidate alias's own, else the hop's.
#[allow(clippy::too_many_arguments)]
pub(super) async fn admit_route(
    state: &SharedState,
    mut route: Route,
    headers: GateHeaders,
    alias: &str,
    check: RouteCheck,
    at: AtAdmission,
    candidate: Option<Arc<CandidateCtx>>,
) -> Result<Opened, OpenFailed> {
    let alias = alias.to_string();
    let fallback = match at {
        AtAdmission::MayFallBack => {
            usable_fallback_for(state, &route, &alias, candidate.as_deref(), headers.chain())
        }
        AtAdmission::Pinned => None,
    };
    let admitted = match fallback {
        Some(fb) => {
            let serves = fallback_serves(state, &fb.name, &fb.route, &alias, check);
            let admission = crate::vram::admit_or_external(
                state,
                &route,
                &alias,
                Some(crate::vram::ExternalFallback::confirmed_by(serves)),
            )
            .await;
            match admission {
                Ok(crate::vram::Admission::External(_)) => {
                    let walk = Walk {
                        check,
                        origin: Origin::Owner,
                    };
                    let mut chain = headers.chain().clone();
                    chain.leave(&alias, fb.from.clone());
                    let mut reason = FallbackReason::ExternalVram;
                    for (name, key) in fb.passed {
                        chain.push(name, key, reason);
                        reason = FallbackReason::Disabled;
                    }
                    return match chain::take(state, walk, chain, fb.name, fb.key, fb.route, reason)
                        .await
                    {
                        Next::Opened(o) => Ok(o),
                        Next::Failed(f) => Err(f),
                        // `take` ends only through a hop past this one,
                        // which fails with its own error.
                        Next::Ended(clause) => Err(OpenFailed {
                            route: Some(Box::new(route)),
                            headers,
                            error: GatewayError::Internal(format!(
                                "the fallback of '{alias}' could not be tried{clause}"
                            )),
                        }),
                    };
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
                h.point(&mut route);
                if at == AtAdmission::MayFallBack {
                    h.set_policy(AdmissionPolicy {
                        check,
                        candidate,
                        chain: headers.chain().clone(),
                    });
                }
            }
            Ok(Opened {
                route,
                hold,
                headers,
            })
        }
        // The hold (or a benchmark's lease) came on after `resolve`, while
        // this request was being admitted: answered as the swap would have
        // answered it, the hop's fallback first (per-request background
        // design §11.9, *changed 2026-10-22*: a configured fallback always
        // applies). Not for a pinned caller, which never falls back, nor a
        // candidate alias's primary, whose walk answers with the alias's
        // own fallback (`Walk::held`).
        Err(error @ (GatewayError::GpuHold { .. } | GatewayError::GpuBenchmark { .. }))
            if at == AtAdmission::MayFallBack && candidate.is_none() =>
        {
            match crate::vram::classify(&route) {
                Some(target) => {
                    held(
                        state,
                        &target,
                        route,
                        &alias,
                        check,
                        error,
                        headers.chain().clone(),
                        Origin::Owner,
                    )
                    .await
                }
                None => Err(OpenFailed {
                    route: Some(Box::new(route)),
                    headers,
                    error,
                }),
            }
        }
        // The start failed on the computer (machine-check design §2.5) or
        // its engine image could not be downloaded (§5, D36): nothing has
        // reached the client, and a configured fallback always applies —
        // the hop's own, or a candidate alias's (WP5). Not for a pinned
        // caller, which never falls back.
        Err(error) if at == AtAdmission::MayFallBack && super::machine_setup::hands_on(&error) => {
            match crate::vram::classify(&route) {
                Some(target) => {
                    super::machine_setup::hand_on(
                        state,
                        super::machine_setup::Failed {
                            target: &target,
                            route,
                            headers,
                            alias: &alias,
                            check,
                            origin: Origin::Owner,
                            candidate: candidate.as_deref(),
                        },
                        error,
                    )
                    .await
                }
                None => Err(OpenFailed {
                    route: Some(Box::new(route)),
                    headers,
                    error,
                }),
            }
        }
        Err(error) => Err(OpenFailed {
            route: Some(Box::new(route)),
            headers,
            error,
        }),
    }
}

/// The GPU hold or a benchmark's lease came on while a request for `target`
/// (the hop `alias`) was being admitted (`refusal` is what admission said)
/// — an owner's request ([`admit_route`]) or a plain model's guest
/// ([`super::guest`]): answered as the resolve-time swap answers a request
/// under it — the hop's fallback along the chain, stamped with the block's
/// reason, when one can take this request; else the block's refusal under
/// the hop's name, saying why a configured fallback did not answer. The
/// alias guest walk does the same with its alias's fallback (`Walk::held`).
#[allow(clippy::too_many_arguments)]
pub(super) async fn held(
    state: &SharedState,
    target: &crate::vram::Target,
    route: Route,
    alias: &str,
    check: RouteCheck,
    refusal: GatewayError,
    chain: Chain,
    origin: Origin,
) -> Result<Opened, OpenFailed> {
    let snap = state.snapshot();
    let failed = |headers: GateHeaders, error| OpenFailed {
        route: Some(Box::new(route.clone())),
        headers,
        error,
    };
    // Lifted again already: what admission said stands.
    let Some(block) = snap.gpu_block_for(target.class, &target.model_id) else {
        return Err(failed(GateHeaders::along(chain), refusal));
    };
    let key = HopKey::Local(target.class, target.model_id.clone());
    let mut tried = chain.clone();
    tried.leave(alias, key.clone());
    let walk = Walk { check, origin };
    match chain::fall_back(
        state,
        walk,
        chain,
        (alias, key.clone()),
        snap.next_hop(&key),
        block.fallback_reason(),
    )
    .await
    {
        Next::Opened(o) => Ok(o),
        Next::Failed(f) => Err(f),
        Next::Ended(clause) => Err(failed(
            GateHeaders::along(tried.clone()),
            block.refusal(alias, format!("{clause}{}", tried.tried())),
        )),
    }
}

/// The fallback §4.7's outside-VRAM verdict may answer a route with
/// ([`usable_fallback`]): the hop that would fall back (`from`), the
/// disabled hops on the way (`passed`, [`Snapshot::past_disabled`]), and the
/// fallback's name, identity and route.
pub struct ExternalFallbackHop {
    pub from: HopKey,
    pub passed: Vec<(String, HopKey)>,
    pub name: String,
    pub key: HopKey,
    pub route: Route,
}

/// The fallback §4.7's outside-VRAM verdict may answer `route` with: the
/// request's usable fallback ([`Snapshot::request_fallback`] — its hop's, or
/// a candidate alias's own). Cheap: a snapshot read and the registry's
/// in-memory list, nothing that probes or fetches. Whether the fallback may
/// take *this* request is [`fallback_serves`], asked only once a verdict
/// says it could answer.
///
/// `None` means admission is today's, whatever the verdict would say:
/// - the verdict cannot change this admission (a cloud route, a model that is
///   up, the switch or admission off, the hold on —
///   [`crate::vram::external_armed`]); nothing is looked up then;
/// - the hop has no fallback;
/// - the fallback does not resolve, or is a candidate alias: a
///   misconfiguration, logged as a warning — here the local model may still
///   serve the request, so it waits as it always has;
/// - the fallback was tried already in this request (a loop the save check
///   could not see): it waits rather than loop.
///
/// A non-background candidate alias's primary answers with the alias's own
/// fallback, never its row fallback (§4.7 "applies to", §4.1): `alias` is
/// the requested name, and [`Snapshot::request_fallback`] reads it. Every
/// other name reads the hop's own.
pub fn usable_fallback(
    state: &SharedState,
    route: &Route,
    alias: &str,
) -> Option<ExternalFallbackHop> {
    usable_fallback_for(state, route, alias, None, &Chain::default())
}

/// [`usable_fallback`] for a request that already carries its candidate
/// alias ([`CandidateCtx`]): that alias's fallback, read from the alias as
/// the request was picked when it has since been disabled or deleted
/// (§12 entry 86); and the chain it carries.
fn usable_fallback_for(
    state: &SharedState,
    route: &Route,
    alias: &str,
    candidate: Option<&CandidateCtx>,
    chain: &Chain,
) -> Option<ExternalFallbackHop> {
    let snap = state.snapshot();
    let target = crate::vram::external_armed(state, &snap, route)?;
    let (from, fallback) = match candidate {
        Some(c) => (
            HopKey::Candidate(c.row.id),
            snap.fallback_named(c.alias_row(&snap).fallback.as_deref()),
        ),
        None => (
            HopKey::Local(target.class, target.model_id.clone()),
            snap.request_fallback(alias, &target),
        ),
    };
    let (passed, fallback) = snap.past_disabled(fallback, |k| chain.seen(k));
    match fallback {
        FallbackRoute::None => None,
        FallbackRoute::Unusable { alias: fb, why, .. } => {
            tracing::warn!(
                "'{alias}' ({}/{}) is not loaded, and its fallback '{fb}' {why} — it cannot \
                 answer when VRAM outside lmgw's control is short, so the request waits for \
                 room instead",
                target.class.as_str(),
                target.model_id
            );
            None
        }
        FallbackRoute::Usable { key, .. }
            if chain.seen(&key) || passed.iter().any(|(_, k)| *k == key) =>
        {
            None
        }
        FallbackRoute::Usable {
            alias: fb,
            key,
            route: fb_route,
        } => Some(ExternalFallbackHop {
            from,
            passed,
            name: fb,
            key,
            route: fb_route,
        }),
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
///
/// What the request carries plays no part, and neither do the fallback's
/// capabilities (the owner's ruling, 2026-10-06: a configured fallback is
/// always used; only capability shapes what it is sent). *Changed
/// 2026-10-06:* a request with images used to count a fallback whose
/// exposed capabilities say `vision: false` as none and wait for the local
/// model (review finding 7), and a candidate alias's fallback that did not
/// positively support every facet the alias enables counted as none
/// (§4.6). Both now answer; what such a fallback cannot take goes to it
/// degraded — images as placeholders ([`super::fallback_images`]), a Chat
/// PDF's pages as its text, a voice turn as its transcript — and its
/// request row says so (`request_logs.degraded`).
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
) -> bool {
    fallback_refusal(state, fb, fb_route, alias, check)
        .await
        .is_none()
}

/// Why the endpoint's check refuses the fallback `fb` of `alias` — the
/// refusal a client naming `fb` would get — or `None` when it may answer.
pub(crate) async fn fallback_refusal(
    state: &SharedState,
    fb: &str,
    fb_route: &Route,
    alias: &str,
    check: RouteCheck,
) -> Option<String> {
    let snap = state.snapshot();
    let r = check.run(state, &snap, fb_route, fb).await.err()?;
    tracing::debug!(
        "fallback '{fb}' of '{alias}' is not one this endpoint can use ({})",
        r.error
    );
    Some(r.error.to_string())
}
