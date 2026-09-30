//! The gate's **per-send half** (unified-KV design §3.3 steps 1–5, ladder
//! design §3.2–3.4): run immediately before each chat send to a local model,
//! after the site has its final [`Params`].
//!
//! **Why per send and not per request.** The count needs the *running* server
//! (`/apply-template` → `/tokenize`; ladder design §3.3 puts the fit check
//! "after acquire, before forwarding"), so it cannot run before admission.
//! And the in-process runners — `/v1/responses`, Admin Chat, MCP sampling,
//! agent batch runs, quickdoc — hold **one** admission across many turns
//! ([`LocalHold`]'s doc comment says why), while every turn is a different
//! prompt with its own reservation. A reservation that lived as long as the
//! hold would pin the pool for the length of a tool loop, with tool calls in
//! between that use no KV at all. So the stages run in the order
//! 1, 2, 5 ([`super::open`]) then 3, 4 (here), and each turn's lease is
//! released when that turn's response ends.
//!
//! For a row where nothing applies — not [`LlamaParams::pool_guarded`], and
//! not a ladder — every function here returns an empty [`TurnLease`]
//! **without a network call and without touching the body** (unified-KV
//! design §7 item 14, ladder design §3.1 "Row without a ladder: unchanged").
//! A cloud route, a GPU-hold fallback included, has no hold and is the same
//! case.
//!
//! **Two kinds of guarded row, two orders** (ladder design §12 entry 7):
//! - a **guarded shared pool** counts, then reserves, then sends: it has to
//!   hold its tokens before a byte goes to llama-server, since one overflow
//!   aborts every slot on the model;
//! - a **ladder** is clamped here and nothing more: its exact count runs
//!   *beside* the send ([`super::send::send_gated`]), and the answer is held
//!   until the count's verdict. Its slots are split (§4.3 rule 2), so an
//!   over-long send can only fail on its own slot, and it is dropped before
//!   any byte reaches the client.
//!
//! **Whether a row is guarded, and by how much, is the running container's
//! answer** ([`GateFacts`], second review, finding 1): the params it was started
//! with, read through the hold — not the row as it is now. A row edited while
//! its container is busy keeps being gated by the numbers the container
//! actually runs until it restarts.
//!
//! [`LlamaParams::pool_guarded`]: crate::config::LlamaParams::pool_guarded

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use super::clamp::clamp_max_tokens;
use super::count::{
    count_chat_prompt, count_text_prompt, image_token_bound, media_bound, media_parts, ImageBound,
    MediaParts,
};
use super::facts::GateFacts;
use super::ladder::{ladder_ready, RungTag};
use super::pool::{self, Ask, Need, PoolTicket};
use crate::config::Route;
use crate::error::GatewayError;
use crate::ir::{ChatRequest, Params};
use crate::runtime::registry::SendGuard;
use crate::runtime::Class;
use crate::state::SharedState;
use crate::vram::LocalHold;

/// One send's share of the gate: the pool reservation it holds, the
/// `max_tokens` the clamp lowered, if any, and — on a ladder row — the rung
/// it was judged on and its place in the climb's drain.
///
/// **Keep it alive until that send's response has ended** — held across the
/// unary body read, moved into the streaming relay task next to the hold,
/// and ended only once the upstream response itself is dropped (review
/// finding 6: a response still alive is a connection llama-server is still
/// generating on). On a ladder row that is also what a climb's drain waits
/// for (§12 entry 10): the send is counted until the lease ends.
///
/// **How it ends decides when the tokens come back** ([`super::pool`]'s
/// "release only once llama-server has let go"):
/// - [`Self::complete`] (or [`Self::end`] with `true`) — the response was
///   read to its normal end, so the server has finished the task: released
///   at once;
/// - a plain drop — anything else: a client disconnect, an lmgw-side timeout
///   or stall, an upstream error mid-stream, a send that failed. The
///   reservation stays, marked releasing, until the container's `/slots` shows
///   the abandoned slot let go.
///
/// Either way it is released exactly once, on every path.
#[must_use = "the lease holds this send's KV reservation — keep it until the response has ended"]
#[derive(Debug, Default)]
pub struct TurnLease {
    max_tokens_clamped: Option<u32>,
    reservation: Option<PoolTicket>,
    /// `Some` exactly on a ladder row: what [`super::send::send_gated`]
    /// counts and judges beside the send.
    pub(super) ladder: Option<LadderPlan>,
    /// The rung the send was judged on — then, once it is handed over, the
    /// rung that served it. `None` without a ladder, and when a fallback
    /// answered instead.
    pub(super) served: Option<RungTag>,
    /// The send's place in the drain count of the container that answered
    /// (ladder design §12 entry 10), released with the lease.
    pub(super) send: Option<SendGuard>,
    /// The fit found that the candidate alias's model this send is on cannot
    /// take it (candidate-aliases §12 entry 45, [`super::candidate::repicks`]):
    /// a guest over the pool's per-request limit, or a candidate that stopped
    /// under the count and may not be restarted for it. Not a refusal: the
    /// send ([`super::send::send_gated`]) picks again before anything is sent,
    /// so every site handles it where it already handles a reroute.
    pub(super) skip: Option<GatewayError>,
}

/// What a ladder row's send helper needs from the fit: the numbers the count
/// beside the send is judged with.
#[derive(Debug, Clone)]
pub(super) struct LadderPlan {
    /// The clamped max output — what a rung's per-slot context has to hold
    /// on top of the prompt (§3.3 step 4).
    pub(super) max_output: u64,
    pub(super) media: MediaParts,
    /// The per-image bound, when the request carries images (§3.3 step 3):
    /// the projector is row-level, so every rung shares it (§12 entry 15).
    pub(super) image_bound: Option<ImageBound>,
}

impl TurnLease {
    /// The lease of a send nothing gates: no reservation, nothing clamped.
    pub fn unguarded() -> Self {
        Self::default()
    }

    /// The rung this send was judged on or served from (ladder design §6):
    /// what `x-lmgw-rung` says, through [`super::GateHeaders::set_rung`].
    /// `None` on every row without a ladder, and when a fallback answered.
    pub fn rung(&self) -> Option<RungTag> {
        self.served.clone()
    }

    /// [`Self::rung`] as `request_logs.rung` records it: 1-based.
    pub fn rung_log(&self) -> Option<i64> {
        self.served.as_ref().map(RungTag::log)
    }

    /// The send's response was read to its normal end: release now.
    pub fn complete(mut self) {
        if let Some(t) = self.reservation.as_mut() {
            t.mark_complete();
        }
    }

    /// End the lease according to how the send ended: `fully_read` is
    /// [`Self::complete`], anything else a plain drop (deferred release).
    pub fn end(self, fully_read: bool) {
        if fully_read {
            self.complete();
        }
    }

    /// The send this lease was for goes elsewhere before anything reached the
    /// client — a candidate alias's re-pick (candidate-aliases §12 entry
    /// 45): the rung it named and its place in a climb's drain go, and so
    /// does its pool reservation, before the re-pick rather than after it
    /// (§12 entry 88) — the re-pick can start a model or wait for admission,
    /// and holding the old candidate's tokens meanwhile keeps other requests
    /// on that pool queued behind a request that has left. `nothing_ran`:
    /// llama-server refused the request before any work (or it was never
    /// sent), so the tokens come back at once; otherwise through the
    /// deferred release, once the container's slots show it let go.
    pub(super) fn reroute(&mut self, nothing_ran: bool) {
        self.served = None;
        self.send = None;
        if let Some(mut ticket) = self.reservation.take() {
            if nothing_ran {
                ticket.mark_complete();
            }
        }
    }

    /// The `max_tokens` a client-set value was lowered to — what
    /// `x-lmgw-max-tokens-clamped` and `request_logs.max_tokens_clamped`
    /// carry. `None` when nothing was lowered.
    pub fn max_tokens_clamped(&self) -> Option<u32> {
        self.max_tokens_clamped
    }
}

/// [`fit_chat`]/[`fit_text`]'s error: step 1 (the clamp) already ran by the
/// time the later steps (count, reserve, a ladder's media refusals) can still
/// refuse the request, and
/// every site that reports the clamp — the `x-lmgw-max-tokens-clamped`
/// header, the `request_logs.max_tokens_clamped` column — has to see it on
/// this refusal too, not only on success (review finding 7): a client whose
/// `max_tokens` lmgw lowered and then still refused otherwise sees neither
/// the number it lost nor why the request failed at that number.
///
/// A site that only wants the error itself can still use `?`:
/// `From<FitRefusal> for GatewayError` discards the clamp the same way
/// dropping it silently used to.
#[derive(Debug)]
pub struct FitRefusal {
    pub error: GatewayError,
    /// See [`TurnLease::max_tokens_clamped`] — the same value, carried out of
    /// a call that did not return a [`TurnLease`] to read it from.
    pub max_tokens_clamped: Option<u32>,
    /// See [`TurnLease::rung`]: the rung a ladder row's request was refused
    /// on, for its `x-lmgw-rung` and its log row.
    pub rung: Option<RungTag>,
}

impl From<FitRefusal> for GatewayError {
    fn from(f: FitRefusal) -> Self {
        f.error
    }
}

/// What gates a container, and by how much.
struct Guard {
    /// The facts it was started with.
    facts: Arc<GateFacts>,
    /// Its `n_predict`: the ceiling every send is clamped to, which a guarded
    /// row's validation already requires to be positive.
    n_predict: i64,
    kind: Kind,
}

/// Which of the two guards a container runs under (module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A guarded shared KV pool: count, reserve, then send.
    Pool,
    /// A ladder rung: clamp here, count beside the send.
    Ladder,
}

/// The facts of the container a held chat route is on, when something gates
/// it — the container's as it was started, not the row's as it is now
/// (second review, finding 1; [`GateFacts`]).
///
/// A ladder start is gated whether or not its row would count as a guarded
/// pool — ladder design §3.1 — and one that could not be gated safely is
/// refused, never sent unguarded ([`ladder_ready`]).
fn guarded(hold: Option<&LocalHold>) -> Result<Option<(&LocalHold, Guard)>, GatewayError> {
    let Some(hold) = hold.filter(|h| h.class() == Class::Chat) else {
        return Ok(None);
    };
    let Some(facts) = hold.gate_facts() else {
        return Ok(None);
    };
    if facts.rung.is_some() {
        let n_predict = ladder_ready(&facts)?;
        let kind = Kind::Ladder;
        return Ok(Some((
            hold,
            Guard {
                facts,
                n_predict,
                kind,
            },
        )));
    }
    if !facts.pool_guarded() {
        return Ok(None);
    }
    let Some(n_predict) = facts.n_predict() else {
        return Ok(None);
    };
    let kind = Kind::Pool;
    Ok(Some((
        hold,
        Guard {
            facts,
            n_predict,
            kind,
        },
    )))
}

/// The lease of a send whose candidate cannot take it ([`TurnLease::skip`]):
/// the clamp it already ran, and why.
fn skipped(max_tokens_clamped: Option<u32>, error: GatewayError) -> TurnLease {
    TurnLease {
        max_tokens_clamped,
        skip: Some(error),
        ..TurnLease::default()
    }
}

/// A refusal of the gate itself, before the clamp could run.
fn unfit(hold: Option<&LocalHold>, error: GatewayError) -> FitRefusal {
    FitRefusal {
        error,
        max_tokens_clamped: None,
        rung: hold
            .and_then(LocalHold::gate_facts)
            .and_then(|f| RungTag::of(&f)),
    }
}

/// Run one step of the gate within an in-process caller's own deadline
/// (second review, finding 7): past it, [`GatewayError::Timeout`] — the same answer
/// the send it precedes would have given.
pub(super) async fn within<T>(
    deadline: Option<Instant>,
    step: impl std::future::Future<Output = Result<T, GatewayError>>,
) -> Result<T, GatewayError> {
    match deadline {
        Some(d) => tokio::time::timeout_at(tokio::time::Instant::from_std(d), step)
            .await
            .map_err(|_| GatewayError::Timeout)?,
        None => step.await,
    }
}

/// The per-send half for a chat send (`/v1/chat/completions`, `/v1/messages`,
/// every in-process turn, the dashboard chat, the load test's probe).
///
/// On a guarded pool, in order:
/// 1. **clamp** — a passthrough `n_predict` is bound first, then
///    `max_tokens` is clamped to the row's `n_predict` ([`bind_max_output`]);
/// 2. **count** — `/apply-template` on the exact body egress will send
///    (`chat_body` of the returned request and `params`), `/tokenize` on the
///    result, plus the per-image bound; audio and unknown image bounds are
///    refused as [`super::count`] defines. A dead container is recovered once
///    through the hold, as for the forward itself;
/// 3. **reserve** `prompt + max output` in the model's pool, or refuse with
///    `400 context_length_exceeded` above the per-request limit.
///
/// On a ladder rung: step 1, and the media refusals of step 2 that need no
/// server (audio, an image without a bound). The count itself, the fit and
/// the climb run beside the send, in [`super::send::send_gated`], which every
/// caller uses to send with this lease.
///
/// Returns the lease and the request to send: borrowed unchanged unless a
/// passthrough `n_predict` had to be bound. `params` is clamped in place; the
/// caller builds its egress body from both.
///
/// `deadline` is an in-process caller's own bound on this whole send (MCP
/// sampling's `SAMPLING_DEADLINE`, an agent loop's remaining wall clock —
/// second review, finding 7): the count and the pool wait both end at it, so the gate
/// can never stretch a "fails fast" bound. `None` for a client request, whose
/// bounds are the route's and the queue's own.
pub async fn fit_chat<'a>(
    state: &SharedState,
    hold: Option<&LocalHold>,
    route: &Route,
    ir: &'a ChatRequest,
    params: &mut Params,
    stream: bool,
    deadline: Option<Instant>,
) -> Result<(TurnLease, Cow<'a, ChatRequest>), FitRefusal> {
    let (hold, guard) = match guarded(hold) {
        Ok(Some(g)) => g,
        Ok(None) => return Ok((TurnLease::unguarded(), Cow::Borrowed(ir))),
        Err(e) => return Err(unfit(hold, e)),
    };

    // 1. Clamp. The raw `n_predict` is taken out of the passthrough (it is
    // the one field that would otherwise carry a limit past the clamp — see
    // `clamp_max_tokens`'s doc comment) and folded into `max_tokens`.
    let mut ir = Cow::Borrowed(ir);
    let raw_n_predict = if ir.passthrough.contains_key("n_predict") {
        ir.to_mut().passthrough.remove("n_predict")
    } else {
        None
    };
    let clamped = bind_max_output(raw_n_predict.as_ref(), params, guard.n_predict);
    let max_output = u64::from(params.max_tokens.unwrap_or(0));
    let rung = RungTag::of(&guard.facts);

    // The per-image bound: both kinds count images against it.
    let media = media_parts(&ir);
    let image_bound = async {
        if media.images == 0 {
            return Ok(None);
        }
        let facts = &guard.facts;
        image_token_bound(&facts.models_dir, facts.projector_row())
            .await
            .map_err(GatewayError::BadRequest)
    };

    if guard.kind == Kind::Ladder {
        // The ladder's share ends here: what needs no server is refused now,
        // before anything is sent; the rest is the send helper's.
        let planned = async {
            let image_bound = image_bound.await?;
            media_bound(media, image_bound.as_ref())?;
            Ok(LadderPlan {
                max_output,
                media,
                image_bound,
            })
        }
        .await;
        return match planned {
            Ok(plan) => Ok((
                TurnLease {
                    max_tokens_clamped: clamped,
                    ladder: Some(plan),
                    served: rung,
                    ..TurnLease::default()
                },
                ir,
            )),
            Err(error) => Err(FitRefusal {
                error,
                max_tokens_clamped: clamped,
                rung,
            }),
        };
    }

    // Steps 2-3 can still refuse the request, but the clamp above already
    // happened — run them behind one `Result` so every refusal from here on
    // is re-attached to `clamped` in one place (review finding 7) rather than
    // at each of their own `?`.
    let reserved: Result<Option<PoolTicket>, GatewayError> = async {
        // 2. Count, on the running server, the body egress is about to send.
        let body = crate::egress::openai::chat_body(
            &ir,
            &route.upstream_model,
            params,
            stream,
            route.upstream.kind,
        );
        let image_bound = image_bound.await?;
        let http = &state.http;
        let (body, image_bound) = (&body, &image_bound);
        let count = within(
            deadline,
            on_running_server(route, hold, move |root| async move {
                count_chat_prompt(http, &root, body, media, image_bound.clone()).await
            }),
        )
        .await?;

        // 3. Reserve.
        let completions =
            completions_per_prompt(ir.passthrough.get("n_cmpl"), ir.passthrough.get("n"));
        let need = Need {
            prompt: count.total,
            max_output: max_output.saturating_mul(completions),
            slots: completions,
        };
        let limits = Limits {
            alias: &ir.model_alias,
            largest_prompt: count.total,
            max_output,
            deadline,
            route,
        };
        reserve(state, hold, &guard.facts, limits, need).await
    }
    .await;

    let ticket = match reserved {
        Ok(ticket) => ticket,
        // A candidate that cannot take this send: the send picks again.
        Err(error) if super::candidate::repicks(Some(hold), &error) => {
            return Ok((skipped(clamped, error), ir));
        }
        Err(error) => {
            return Err(FitRefusal {
                error,
                max_tokens_clamped: clamped,
                rung: None,
            })
        }
    };
    Ok((
        TurnLease {
            max_tokens_clamped: clamped,
            reservation: ticket,
            ..TurnLease::default()
        },
        ir,
    ))
}

/// The per-send half for legacy `/v1/completions`, whose body is forwarded
/// as the client sent it: the same four steps as [`fit_chat`], on the raw
/// JSON.
///
/// - **Clamp:** llama-server reads `n_predict`, then its aliases
///   `max_completion_tokens` and `max_tokens` (`server-schema.cpp`: the
///   primary name wins over its aliases). On a guarded row all three are
///   folded into one `max_tokens`, clamped to the row's `n_predict`.
/// - **Count:** `/tokenize` alone — there is no chat template. `prompt` is
///   split the way the completion path splits it (`tokenize_input_prompts`:
///   an array with no token id in it is several prompts, anything else is
///   one); a pure token-id array is counted by its length. A prompt that
///   carries media cannot be bounded and is refused, like audio on the chat
///   path.
/// - Every prompt runs in its own slot, so the per-request limit applies to
///   the largest one and the reservation sums them all.
pub async fn fit_text(
    state: &SharedState,
    hold: Option<&LocalHold>,
    route: &Route,
    alias: &str,
    body: &mut Value,
) -> Result<TurnLease, FitRefusal> {
    let (hold, guard) = match guarded(hold) {
        Ok(Some(g)) => g,
        Ok(None) => return Ok(TurnLease::unguarded()),
        Err(e) => return Err(unfit(hold, e)),
    };

    // 1. Clamp. The first spelling that holds a number is the one
    // llama-server would honour; a non-numeric one is dropped with the rest.
    let raw = ["n_predict", "max_completion_tokens", "max_tokens"]
        .into_iter()
        .find_map(|k| body.get(k).filter(|v| v.is_number()).cloned());
    let mut params = Params::default();
    let clamped = bind_max_output(raw.as_ref(), &mut params, guard.n_predict);
    let max_output = params.max_tokens.unwrap_or(0);
    if let Some(obj) = body.as_object_mut() {
        obj.remove("n_predict");
        obj.remove("max_completion_tokens");
        obj.insert("max_tokens".into(), Value::from(max_output));
    }
    let max_output = u64::from(max_output);

    if guard.kind == Kind::Ladder {
        // A prompt that cannot be counted at all (media, a malformed shape)
        // is refused now, before anything is sent; the count itself runs
        // beside the send ([`super::send::send_gated`]).
        let rung = RungTag::of(&guard.facts);
        return match split_prompts(body.get("prompt")) {
            Ok(_) => Ok(TurnLease {
                max_tokens_clamped: clamped,
                ladder: Some(LadderPlan {
                    max_output,
                    media: MediaParts::default(),
                    image_bound: None,
                }),
                served: rung,
                ..TurnLease::default()
            }),
            Err(error) => Err(FitRefusal {
                error,
                max_tokens_clamped: clamped,
                rung,
            }),
        };
    }

    // Steps 2-3 can still refuse the request — see `fit_chat`'s identical
    // reasoning (review finding 7).
    let reserved: Result<Option<PoolTicket>, GatewayError> = async {
        // 2. Count.
        let prompts = split_prompts(body.get("prompt"))?;
        let http = &state.http;
        let mut counts = Vec::with_capacity(prompts.len());
        for p in &prompts {
            let n = match p {
                Prompt::Tokens(n) => *n,
                Prompt::Text(content) => {
                    on_running_server(route, hold, move |root| async move {
                        count_text_prompt(http, &root, content).await
                    })
                    .await?
                }
            };
            counts.push(n);
        }
        let largest = counts.iter().copied().max().unwrap_or(0);

        // 3. Reserve.
        let completions = completions_per_prompt(body.get("n_cmpl"), body.get("n"));
        let slots = completions.saturating_mul(counts.len() as u64);
        let need = Need {
            prompt: saturating_sum(&counts),
            max_output: max_output.saturating_mul(slots),
            slots,
        };
        let limits = Limits {
            alias,
            largest_prompt: largest,
            max_output,
            deadline: None,
            route,
        };
        reserve(state, hold, &guard.facts, limits, need).await
    }
    .await;

    let ticket = match reserved {
        Ok(ticket) => ticket,
        // A candidate that cannot take this send: the send picks again.
        Err(error) if super::candidate::repicks(Some(hold), &error) => {
            return Ok(skipped(clamped, error));
        }
        Err(error) => {
            return Err(FitRefusal {
                error,
                max_tokens_clamped: clamped,
                rung: None,
            })
        }
    };
    Ok(TurnLease {
        max_tokens_clamped: clamped,
        reservation: ticket,
        ..TurnLease::default()
    })
}

/// What `x-lmgw-max-tokens-clamped` will say for a request whose turns all
/// send the same `params` — `/v1/responses`, whose headers leave before its
/// first turn runs. The same arithmetic as [`fit_chat`]'s step 1, on copies;
/// every turn's own lease still logs its own value.
pub fn planned_clamp(hold: Option<&LocalHold>, ir: &ChatRequest, params: &Params) -> Option<u32> {
    let (_, guard) = guarded(hold).ok().flatten()?;
    bind_max_output(
        ir.passthrough.get("n_predict"),
        &mut params.clone(),
        guard.n_predict,
    )
}

/// What `x-lmgw-rung` will say for `/v1/responses`, whose headers leave
/// before its first turn runs: the rung the model runs at open (ladder design
/// §6). Every turn's own lease logs the rung that served it — a turn that
/// climbs is logged on the rung it climbed to.
pub fn planned_rung(hold: Option<&LocalHold>) -> Option<RungTag> {
    let (_, guard) = guarded(hold).ok().flatten()?;
    (guard.kind == Kind::Ladder)
        .then(|| RungTag::of(&guard.facts))
        .flatten()
}

/// Name the model on a [`GatewayError::ContextExceeded`] that came back from
/// llama-server itself (`egress::openai::map_error`'s backstop, which cannot
/// know it). Every other error passes through untouched.
pub fn attribute(e: GatewayError, route: &Route) -> GatewayError {
    match e {
        GatewayError::ContextExceeded {
            model,
            prompt_tokens,
            max_output,
            limit,
            top_rung,
        } if model.is_empty() => GatewayError::ContextExceeded {
            model: route.upstream_model.clone(),
            prompt_tokens,
            max_output,
            limit,
            top_rung,
        },
        other => other,
    }
}

/// Step 1, shared by every entry point: bind a raw `n_predict` (or, on the
/// legacy path, whichever of its spellings the body used) into
/// `params.max_tokens`, then [`clamp_max_tokens`] to the row's ceiling.
///
/// llama-server reads `n_predict` in preference to `max_tokens` (the primary
/// field wins over its aliases, `server-schema.cpp`; `-1..=INT32_MAX`), so the
/// value it would have honoured is the one the clamp judges. A negative value
/// — `-1`, "until the model stops" — is an unbounded ask, and clamping it is
/// a lowering like any other, so it is reported. A value that is not a number
/// is one llama-server would ignore in favour of `max_tokens`, and so does
/// this.
fn bind_max_output(raw: Option<&Value>, params: &mut Params, n_predict: i64) -> Option<u32> {
    if let Some(v) = raw {
        match v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)) {
            Some(n) if n >= 0 => params.max_tokens = Some(u32::try_from(n).unwrap_or(u32::MAX)),
            Some(_) => params.max_tokens = Some(u32::MAX),
            None => {}
        }
    }
    clamp_max_tokens(params, n_predict)
}

/// How many completions each prompt asks for: `n_cmpl`, or its alias `n`
/// (`server-schema.cpp`, `field_num("n_cmpl")->add_alias("n")`). Each is a
/// slot generating its own output from a shared prompt, so the reservation
/// charges the output once per completion.
fn completions_per_prompt(n_cmpl: Option<&Value>, n: Option<&Value>) -> u64 {
    n_cmpl
        .or(n)
        .and_then(Value::as_u64)
        .filter(|&c| c >= 1)
        .unwrap_or(1)
}

/// Σ of every legacy prompt's own count (review finding 5): saturating, like
/// every other reservation arithmetic here, so a request with an absurd
/// number of prompts (each itself a client-controlled token count) yields a
/// clean refusal against the per-request limit rather than wrapping past it
/// or panicking under overflow checks.
fn saturating_sum(counts: &[u64]) -> u64 {
    counts.iter().fold(0u64, |acc, &n| acc.saturating_add(n))
}

/// One legacy prompt, as the completion path will tokenize it.
pub(super) enum Prompt<'b> {
    /// A string or a mixed array of strings and token ids — `/tokenize` it.
    Text(&'b Value),
    /// A pure token-id array: its length is its count.
    Tokens(u64),
}

/// Split a legacy `prompt` the way `tokenize_input_prompts` does, refusing the
/// shapes that cannot be counted.
pub(super) fn split_prompts(prompt: Option<&Value>) -> Result<Vec<Prompt<'_>>, GatewayError> {
    let Some(prompt) = prompt else {
        return Err(GatewayError::BadRequest("\"prompt\" is required".into()));
    };
    fn one(p: &Value) -> Result<Prompt<'_>, GatewayError> {
        match p {
            Value::String(_) => Ok(Prompt::Text(p)),
            Value::Array(items) if items.iter().all(Value::is_number) => {
                Ok(Prompt::Tokens(items.len() as u64))
            }
            Value::Array(items) if items.iter().all(|i| i.is_number() || i.is_string()) => {
                Ok(Prompt::Text(p))
            }
            Value::Object(o) if o.contains_key("multimodal_data") => Err(GatewayError::BadRequest(
                "this prompt carries multimodal data, which lmgw cannot count on this model's \
                     context guard (a guarded shared KV pool) — send images through \
                     /v1/chat/completions, or use a model without a context guard"
                    .into(),
            )),
            Value::Object(o) => match o.get("prompt_string") {
                Some(s) if s.is_string() => Ok(Prompt::Text(s)),
                _ => Err(unreadable()),
            },
            _ => Err(unreadable()),
        }
    }
    match prompt {
        Value::Array(items) if !items.iter().any(Value::is_number) => {
            if items.is_empty() {
                return Err(GatewayError::BadRequest(
                    "\"prompt\" must not be empty".into(),
                ));
            }
            items.iter().map(one).collect()
        }
        p => Ok(vec![one(p)?]),
    }
}

fn unreadable() -> GatewayError {
    GatewayError::BadRequest(
        "\"prompt\" elements must be a string, a list of tokens, a JSON object containing a \
         prompt string, or a list of mixed strings & tokens"
            .into(),
    )
}

/// Run one count against the container `hold` is on, with the forward's own
/// dead-container policy ([`crate::vram::retry_dead_container`]) and the
/// route's own request timeout — the configured bound on waiting for this
/// container, `None` meaning none, exactly as for the send that follows.
///
/// `pub(crate)` for `POST /v1/messages/count_tokens` (api-docs design §5.2),
/// which counts a local chat row's rendered template the same way and must
/// meet a dead container the same way.
pub(crate) async fn on_running_server<T, F, Fut>(
    route: &Route,
    hold: &LocalHold,
    call: F,
) -> Result<T, GatewayError>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<T, GatewayError>>,
{
    let timeout = route.upstream.request_timeout();
    let call = &call;
    crate::vram::retry_dead_container(hold, move || {
        // Read on every attempt: a recovery moves the hold to a new port.
        let fut = call(format!("http://127.0.0.1:{}", hold.port()));
        async move {
            match timeout {
                Some(d) => tokio::time::timeout(d, fut)
                    .await
                    .map_err(|_| GatewayError::Timeout)?,
                None => fut.await,
            }
        }
    })
    .await
}

/// What step 3 judges a request by, beside its [`Need`].
struct Limits<'a> {
    /// Only for the queue view.
    alias: &'a str,
    /// The largest single prompt — what one slot holds at its peak with
    /// `max_output`.
    largest_prompt: u64,
    max_output: u64,
    /// See [`fit_chat`]'s `deadline`.
    deadline: Option<Instant>,
    route: &'a Route,
}

/// Step 3: the per-request limit, then the pool reservation.
///
/// `largest_prompt + max_output` is what one slot will hold at its peak, which
/// is what the per-request limit bounds (design §3.2: the unified per-request
/// context is the minimum of the pool, the per-slot cap and the trained
/// context). `need` is the whole request's reservation.
///
/// Both numbers come from the facts of the container the send goes to —
/// re-read from the hold here, after the count, because a dead-container
/// recovery during the count (`on_running_server`) moves the hold onto a new
/// start with facts of its own. `Ok(None)` when that new start is not guarded
/// at all: nothing to reserve against.
async fn reserve(
    state: &SharedState,
    hold: &LocalHold,
    counted_on: &Arc<GateFacts>,
    limits: Limits<'_>,
    need: Need,
) -> Result<Option<PoolTicket>, GatewayError> {
    let facts = match hold.gate_facts() {
        Some(now) if Arc::ptr_eq(&now, counted_on) => now,
        Some(now) if now.pool_guarded() => now,
        _ => return Ok(None),
    };
    // Review finding 2: the trained context is no longer a fallback for the
    // pool's size (`--fit` can shrink an unset `ctx_size` to whatever memory
    // allows), so this is reachable only for a container started from a row
    // saved before `ops::validate_kv_unified` started requiring one of the two
    // fields — `validate_kv_unified` refuses a new save that would hit it.
    let unknown_pool = || {
        GatewayError::Internal(format!(
            "'{}' shares one KV pool across its slots, and lmgw cannot tell how large it is: the \
             row its container was started with sets neither ctx_size nor kv_unified_per_slot — \
             set one of them on the row, and restart the model",
            facts.model_id
        ))
    };
    let limit = facts.per_request_ctx().ok_or_else(unknown_pool)?;
    if limits.largest_prompt.saturating_add(limits.max_output) > limit {
        return Err(GatewayError::ContextExceeded {
            model: facts.model_id.clone(),
            prompt_tokens: limits.largest_prompt,
            max_output: Some(limits.max_output),
            limit,
            top_rung: None,
        });
    }
    let capacity = facts.pool_tokens().ok_or_else(unknown_pool)?;
    let ask = Ask {
        model: &facts.model_id,
        alias: limits.alias,
        capacity,
        need,
        deadline: limits.deadline,
        route_timeout: limits.route.upstream.request_timeout(),
    };
    pool::reserve(state, ask).await.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_raw_n_predict_is_what_the_clamp_judges() {
        // Above the ceiling: lowered, and reported.
        let mut p = Params {
            max_tokens: Some(10),
            ..Default::default()
        };
        assert_eq!(bind_max_output(Some(&json!(1000)), &mut p, 64), Some(64));
        assert_eq!(p.max_tokens, Some(64));

        // Below: it wins over max_tokens, as it does in llama-server.
        let mut p = Params {
            max_tokens: Some(50),
            ..Default::default()
        };
        assert_eq!(bind_max_output(Some(&json!(20)), &mut p, 64), None);
        assert_eq!(p.max_tokens, Some(20));

        // Unbounded (-1): a lowering like any other.
        let mut p = Params::default();
        assert_eq!(bind_max_output(Some(&json!(-1)), &mut p, 64), Some(64));
        assert_eq!(p.max_tokens, Some(64));

        // Not a number: ignored, as llama-server would; max_tokens governs.
        let mut p = Params {
            max_tokens: Some(30),
            ..Default::default()
        };
        assert_eq!(bind_max_output(Some(&json!("lots")), &mut p, 64), None);
        assert_eq!(p.max_tokens, Some(30));
    }

    #[test]
    fn legacy_prompts_split_the_way_the_completion_path_splits_them() {
        let one = json!("a b c");
        assert!(matches!(
            split_prompts(Some(&one)).unwrap()[..],
            [Prompt::Text(_)]
        ));

        let tokens = json!([1, 2, 3, 4]);
        assert!(matches!(
            split_prompts(Some(&tokens)).unwrap()[..],
            [Prompt::Tokens(4)]
        ));

        // Any token id in the array makes it one mixed prompt.
        let mixed = json!(["a", 5, "b"]);
        assert!(matches!(
            split_prompts(Some(&mixed)).unwrap()[..],
            [Prompt::Text(_)]
        ));

        // No token id: several prompts.
        let many = json!(["a", [1, 2], {"prompt_string": "c"}]);
        assert!(matches!(
            split_prompts(Some(&many)).unwrap()[..],
            [Prompt::Text(_), Prompt::Tokens(2), Prompt::Text(_)]
        ));

        let media = json!({"prompt_string": "x", "multimodal_data": ["AAAA"]});
        assert!(split_prompts(Some(&media)).is_err());
        assert!(split_prompts(None).is_err());
        assert!(split_prompts(Some(&json!([]))).is_err());
    }

    #[test]
    fn completions_multiply_only_when_asked_for() {
        assert_eq!(completions_per_prompt(None, None), 1);
        assert_eq!(completions_per_prompt(None, Some(&json!(3))), 3);
        assert_eq!(
            completions_per_prompt(Some(&json!(2)), Some(&json!(3))),
            2,
            "n_cmpl is the primary name"
        );
        assert_eq!(completions_per_prompt(Some(&json!(0)), None), 1);
    }

    #[test]
    fn saturating_sum_clamps_instead_of_wrapping() {
        // Review finding 5: several prompts each near `u64::MAX` (client-
        // controlled token counts) must saturate, never wrap or panic under
        // overflow checks.
        assert_eq!(saturating_sum(&[u64::MAX, u64::MAX, 1]), u64::MAX);
        assert_eq!(saturating_sum(&[1, 2, 3]), 6);
        assert_eq!(saturating_sum(&[]), 0);
    }
}
