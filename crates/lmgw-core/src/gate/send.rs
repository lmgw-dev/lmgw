//! The gate's send: every fitted chat send to a local model goes through
//! [`send_gated`] with the lease its fit returned (ladder design §3.3–3.4,
//! §12 entries 7–10).
//!
//! **A row without a ladder** is sent exactly as before this existed: one
//! check of the lease, then [`crate::vram::send_local`] — no count, no
//! `/apply-template` call, no send counter, no header (§3.1 "Row without a
//! ladder: unchanged").
//!
//! **A ladder row** is sent and counted at once (§12 entry 7). The exact count
//! (`/apply-template` → `/tokenize`, or `/tokenize` alone on the legacy path)
//! runs beside the forward to the running rung, and the upstream response is
//! **held until the count's verdict is in**:
//! - it fits: the response is handed over, and the send stays counted for a
//!   climb's drain until the lease ends (§12 entry 10);
//! - it does not fit: the send is dropped — no byte has reached the client —
//!   and the model climbs straight to the smallest rung that holds the
//!   request ([`crate::vram::climb`]); the request is then counted and sent
//!   again on the rung that runs by then;
//! - no rung holds it: `400 context_length_exceeded` against the top rung.
//!
//! Two more ways the pair ends, each at most once per send: llama-server's own
//! context refusal before any work (`exceed_context_size_error`, the
//! **backstop** — the count undercounted) climbs to fit its number and retries
//! once (§3.1); a dead container ends the count and the send together, and
//! the pair is recovered and run again as one unit, so a verdict is always
//! about the container that answered (§12 entry 20).
//!
//! **Why no byte can leave before the verdict:** this function returns the
//! upstream response only after a count that fits, taken on the same
//! container; every site relays only what this function returned.
//!
//! **A candidate that cannot take the request** (candidate-aliases design §12
//! entry 45) is found out here too, before any byte leaves, and the gate
//! picks again ([`super::candidate`]): a guest over the candidate's context
//! — the fit's pool limit (carried on the lease), llama-server's own refusal
//! before any work, which a guest's send holds back for this (on a ladder,
//! the second one, after the climb the first made), or a ladder's top rung —
//! a guest's denied climb, a guest's climb that failed (a rung no card
//! holds, a drain that timed out, a rung that would not start: §12 entry
//! 92), and, in both modes, a candidate that stopped under the request and
//! may not be restarted for it (`candidate_lost`). The answer is
//! [`Sent::Rerouted`]: the site serves the request again on what the walk
//! picked. Every other hold is unchanged.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::candidate;
use super::count::{count_chat_prompt, count_text_prompt};
use super::fit::{split_prompts, within, LadderPlan, Prompt, TurnLease};
use super::ladder::{climb_reason, target_rung, trained_contexts, RungTag};
use super::open::{GateHeaders, OpenFailed, Opened};
use crate::config::Route;
use crate::error::GatewayError;
use crate::ir::{ChatRequest, Params};
use crate::state::SharedState;
use crate::vram::{Climbed, LocalHold};

/// What a ladder row's count is taken on: the body the send carries.
pub enum CountInput<'a> {
    /// A chat send: counted as `chat_body(ir, params, stream)` renders it —
    /// exactly what egress sends.
    Chat {
        ir: &'a ChatRequest,
        params: &'a Params,
        stream: bool,
    },
    /// Legacy `/v1/completions`: the raw body, whose largest prompt is what
    /// one slot has to hold.
    Text { body: &'a Value },
}

/// What [`send_gated`] ended with.
///
/// `#[allow(large_enum_variant)]` like [`Climbed`]: the route is the point
/// of the rerouted answer, and there is one of these per send.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Sent {
    /// The upstream answered, and on a ladder row the count said it fits the
    /// rung that answered: relay it.
    Upstream(reqwest::Response),
    /// Nothing was sent anywhere yet, and the request goes elsewhere: the
    /// site serves it again on this admission — its route, its hold (keep it
    /// as long as that send's response, as for the first) and its headers,
    /// which replace the ones it had — exactly as if the gate had opened it
    /// there. `Err` is the refusal to answer with instead, under its own
    /// headers.
    ///
    /// - A climb could not happen, and the request's fallback answers (§12
    ///   entry 8, or the GPU hold): no hold, `x-lmgw-fallback` and its reason.
    /// - A candidate alias's model cannot take the request (module doc): the
    ///   walk's next pick — another loaded candidate, the alias fallback, or
    ///   the deferral.
    Rerouted(Result<Opened, OpenFailed>),
}

/// Send one fitted request (module doc).
///
/// `build` renders the request against the route it is handed — the caller's
/// route, its base URL on the container the attempt goes to — exactly as for
/// [`crate::vram::send_local`]; it is called once per attempt. `timeout`
/// bounds each attempt's wait for response headers, as there. `deadline` is
/// an in-process caller's bound on the whole send (see
/// [`super::fit::fit_chat`]'s): on a ladder row the count, a climb and the
/// retry all end at it. `count` is what a ladder row's count is taken on,
/// and is read only there.
///
/// On a ladder row `lease` learns the rung each attempt was judged on — the
/// one that served, once this returns [`Sent::Upstream`], and none once a
/// fallback answers — and keeps the send's place in the drain count.
#[allow(clippy::too_many_arguments)]
pub async fn send_gated<F>(
    state: &SharedState,
    hold: Option<&LocalHold>,
    route: &Route,
    lease: &mut TurnLease,
    count: CountInput<'_>,
    deadline: Option<Instant>,
    timeout: Option<Duration>,
    build: F,
) -> Result<Sent, GatewayError>
where
    F: Fn(&Route) -> Result<reqwest::RequestBuilder, GatewayError>,
{
    send_gated_marked(
        state, hold, route, lease, count, deadline, timeout, build, None,
    )
    .await
}

/// [`send_gated`] for a caller that bills a stop by whether a prompt went
/// out: `build` sets `prompt_sent` as each attempt leaves, and this clears
/// it whenever the gate takes an attempt back (A2 review 2, B2 review 1):
/// - the request goes elsewhere ([`Sent::Rerouted`]) — an attempt held back
///   as a context refusal ran nothing, and a candidate that was lost or
///   re-picked is not working on it. The rerouted call keeps its own record;
/// - a ladder's count says the attempt does not fit, or llama-server refused
///   it as above its context (the backstop): the attempt is dropped and the
///   model climbs, and the retry's build sets the flag again;
/// - the container is found dead (a transport failure, on a ladder or not)
///   and is recovered: a dead container works on nothing.
///
/// A stop during any of these waits — the re-pick, the climb's restart, the
/// recovery — or a refusal in their place, then bills no prompt. A wait for
/// response headers that timed out clears nothing: that prompt went out.
#[allow(clippy::too_many_arguments)]
pub async fn send_gated_marked<F>(
    state: &SharedState,
    hold: Option<&LocalHold>,
    route: &Route,
    lease: &mut TurnLease,
    count: CountInput<'_>,
    deadline: Option<Instant>,
    timeout: Option<Duration>,
    build: F,
    prompt_sent: Option<&AtomicBool>,
) -> Result<Sent, GatewayError>
where
    F: Fn(&Route) -> Result<reqwest::RequestBuilder, GatewayError>,
{
    // The fit's frozen tool-image decision, on the route every attempt is
    // built from — and counted on, beside a ladder's send (llama egress
    // design §3.2). The held endpoint's single writer carries it unchanged
    // through a dead-container retry and a climb.
    let llama = lease.llama.clone();
    let decided = super::tool_images::on_route(route, llama.as_ref());
    let route: &Route = &decided;
    // The fit already found this candidate cannot take the send (a guest
    // over its pool's per-request limit, or the candidate lost at the count).
    if let Some(cause) = lease.skip.take() {
        let Some(hold) = hold else {
            return Err(cause);
        };
        return Ok(reroute(state, hold, lease, cause, deadline, prompt_sent).await);
    }
    let Some(plan) = lease.ladder.clone() else {
        let sent = Box::pin(crate::vram::send_local_marked(
            hold,
            route,
            timeout,
            build,
            prompt_sent,
        ))
        .await;
        return match (hold, sent) {
            (Some(h), Err(e)) if candidate::repicks(hold, &e) => {
                Ok(reroute(state, h, lease, e, deadline, prompt_sent).await)
            }
            (Some(h), Ok(resp)) if candidate::holds_back_refusals(hold) => {
                match Box::pin(context_refusal(h, resp)).await? {
                    Ok(resp) => Ok(Sent::Upstream(resp)),
                    Err(cause) => Ok(reroute(state, h, lease, cause, deadline, prompt_sent).await),
                }
            }
            (_, sent) => sent.map(Sent::Upstream),
        };
    };
    let Some(hold) = hold else {
        return Err(GatewayError::Internal(
            "a ladder row's send lost the claim it was fitted on".into(),
        ));
    };
    let ladder = Ladder {
        state,
        hold,
        route,
        plan: &plan,
        count: &count,
        timeout,
        build: &build,
        prompt_sent,
    };
    match within(deadline, ladder.send(lease)).await {
        Err(e) if candidate::repicks(Some(hold), &e) => {
            Ok(reroute(state, hold, lease, e, deadline, prompt_sent).await)
        }
        sent => sent,
    }
}

/// The candidate `hold` is on cannot take this request (`cause`): pick again
/// ([`candidate::repick`]) — within an in-process caller's deadline, like
/// the rest of the send. The lease no longer names a rung, holds a send or
/// holds its pool reservation ([`TurnLease::reroute`]): a context refusal
/// ran nothing on the container, so its tokens come back at once — and no
/// prompt of this send is being worked on any more (`prompt_sent`,
/// [`send_gated_marked`]).
async fn reroute(
    state: &SharedState,
    hold: &LocalHold,
    lease: &mut TurnLease,
    cause: GatewayError,
    deadline: Option<Instant>,
    prompt_sent: Option<&AtomicBool>,
) -> Sent {
    // Boxed: kept out of `send_gated_marked`'s debug-build poll frame (143 KB inline),
    // which sits on the turn -> gate -> climb chain sharing a 2 MiB stack.
    Box::pin(repicked(state, hold, lease, cause, deadline, prompt_sent)).await
}

async fn repicked(
    state: &SharedState,
    hold: &LocalHold,
    lease: &mut TurnLease,
    cause: GatewayError,
    deadline: Option<Instant>,
    prompt_sent: Option<&AtomicBool>,
) -> Sent {
    if let Some(sent) = prompt_sent {
        sent.store(false, Ordering::Relaxed);
    }
    lease.reroute(matches!(cause, GatewayError::ContextExceeded { .. }));
    let picked = match deadline {
        Some(d) => tokio::time::timeout_at(
            tokio::time::Instant::from_std(d),
            candidate::repick(state, hold, cause),
        )
        .await
        .unwrap_or_else(|_| {
            Err(OpenFailed {
                route: None,
                headers: GateHeaders::default(),
                error: GatewayError::Timeout,
            })
        }),
        None => candidate::repick(state, hold, cause).await,
    };
    Sent::Rerouted(picked)
}

/// A guest candidate's answer, held back if it is llama-server's context
/// refusal before any work (`exceed_context_size_error`, §12 entry 12's
/// backstop): `Err` is that refusal as [`GatewayError::ContextExceeded`], so
/// the gate can pick again (§12 entry 45). Any other answer is handed on as
/// the container gave it — a `400` rebuilt from the bytes already read.
async fn context_refusal(
    hold: &LocalHold,
    resp: reqwest::Response,
) -> Result<Result<reqwest::Response, GatewayError>, GatewayError> {
    if resp.status() != reqwest::StatusCode::BAD_REQUEST {
        return Ok(Ok(resp));
    }
    let (status, version, headers) = (resp.status(), resp.version(), resp.headers().clone());
    let bytes = resp.bytes().await.map_err(GatewayError::from)?;
    if let Some(ec) = crate::egress::llama_cpp::parse_exceed_context(&bytes) {
        return Ok(Err(GatewayError::ContextExceeded {
            model: hold.model_id().to_string(),
            prompt_tokens: ec.n_prompt_tokens,
            max_output: None,
            limit: ec.n_ctx,
            top_rung: None,
        }));
    }
    Ok(Ok(rebuild(status, version, headers, bytes)))
}

/// A response rebuilt from its parts and a body already read whole.
pub(crate) fn rebuild(
    status: reqwest::StatusCode,
    version: reqwest::Version,
    headers: reqwest::header::HeaderMap,
    bytes: bytes::Bytes,
) -> reqwest::Response {
    let mut rebuilt = axum::http::Response::new(bytes);
    *rebuilt.status_mut() = status;
    *rebuilt.version_mut() = version;
    *rebuilt.headers_mut() = headers;
    reqwest::Response::from(rebuilt)
}

/// One ladder send's fixed inputs.
struct Ladder<'s, F> {
    state: &'s SharedState,
    hold: &'s LocalHold,
    route: &'s Route,
    plan: &'s LadderPlan,
    count: &'s CountInput<'s>,
    timeout: Option<Duration>,
    build: &'s F,
    /// The caller's record of a prompt that went out ([`send_gated_marked`]).
    prompt_sent: Option<&'s AtomicBool>,
}

/// How one count-and-send pair ended.
enum Verdict {
    /// Hand this over: the count fits the rung that answered, or the
    /// container refused the request for a reason no climb changes.
    Answer(reqwest::Response),
    /// The container is gone (a transport failure on either call).
    Dead(String),
    /// It does not fit: a prompt of `prompt` tokens, counted — or, from the
    /// `backstop`, llama-server's own number.
    Climb { prompt: u64, backstop: bool },
}

/// What one answer of the send says before the count is in.
enum Judged {
    /// An answer the count's verdict decides about: held until then.
    Hold(reqwest::Response),
    /// An answer that decides the pair by itself.
    Now(Verdict),
}

impl<F> Ladder<'_, F>
where
    F: Fn(&Route) -> Result<reqwest::RequestBuilder, GatewayError>,
{
    /// The loop: judge on the rung that runs, count and send, then hand over,
    /// climb, or recover.
    ///
    /// **Why it ends** (review finding 19). There is no iteration cap, on
    /// purpose. No number of passes is right that is not invented: another
    /// request's failed climb can legitimately send this one back to the
    /// base, which is progress lost to someone else, not a loop. Instead,
    /// every pass that does not return does one of these:
    /// - **climbs strictly higher on its own.** Its target is above the rung
    ///   it was judged on (`target_rung` searches `running+1..=top`), and its
    ///   own climb that succeeds leaves that rung or a higher one running. The
    ///   ladder has `of` rungs, and the top one ends in a return
    ///   (`context_length_exceeded`);
    /// - **spends its one recovery or its one backstop** (`recovered`,
    ///   `backstop_left`);
    /// - **follows another actor's change**: another trigger's climb it
    ///   joined or waited for, a stop, an edit, another user of a shared hold
    ///   moving the claim. That is the only way a pass ends lower or level
    ///   (`sync`, `begin_send`'s refusal, a `Done` that settled someone
    ///   else's climb, a failed rung that was a joiner's). A joiner whose own
    ///   rung failed gets that 502, not another pass (§12 entry 52).
    ///
    /// Every wait in a pass is bounded by a real setting, not by this loop:
    /// the climb's budget (`vram.queue_timeout_seconds`, where 0 is the
    /// owner's own "no limit"), the load and unload timeouts, the route's
    /// request timeout for the count and the send, and an in-process caller's
    /// deadline around the whole send.
    async fn send(&self, lease: &mut TurnLease) -> Result<Sent, GatewayError> {
        // The largest prompt this request is known to have — its count, or
        // llama-server's number from a backstop — so a send that climbed is
        // judged against the new rung before it is sent again.
        let mut known: Option<u64> = None;
        let mut recovered = false;
        let mut backstop_left = true;
        loop {
            // A climb in progress is waited for, a claim a climb moved
            // follows it, and a claim whose model is gone is re-admitted at
            // the base (`LocalHold::sync`).
            self.hold.sync().await?;
            // The container, and the facts it is judged by, in one read.
            let (attempt, facts) = self.hold.attempt_with_facts();
            let Some(tag) = facts.as_deref().and_then(RungTag::of) else {
                // The model came back without a ladder (the row was edited,
                // and a recovery restarted it): judged like any such row.
                lease.served = None;
                // Boxed like the climb below: this future sits on the turn ->
                // gate chain that shares a 2 MiB stack in a debug build.
                return Box::pin(crate::vram::send_local_marked(
                    Some(self.hold),
                    self.route,
                    self.timeout,
                    self.build,
                    self.prompt_sent,
                ))
                .await
                .map(Sent::Upstream);
            };
            lease.served = Some(tag.clone());
            if let Some(prompt) =
                known.filter(|&p| p.saturating_add(self.plan.max_output) > tag.per_slot)
            {
                // Boxed: the climb's poll frame is 111 KB in a debug build.
                match Box::pin(self.climb(&tag, prompt)).await? {
                    Some(fallback) => return Ok(fallen_back(lease, fallback)),
                    None => continue,
                }
            }

            let Ok(guard) = self.hold.begin_send() else {
                // A climb marked the model, or moved the claim, since the
                // sync: sync again.
                continue;
            };
            if self.hold.attempt() != attempt {
                // The claim moved between the read and the count (another
                // user of a shared hold recovered it): the rung judged is not
                // the one the send would reach. Judge again.
                continue;
            }
            match self
                .pair(attempt, facts.as_deref(), tag.per_slot, backstop_left)
                .await?
            {
                Verdict::Answer(resp) => {
                    lease.send = Some(guard);
                    return Ok(Sent::Upstream(resp));
                }
                Verdict::Dead(why) => {
                    drop(guard);
                    // A dead container works on nothing: a stop during the
                    // recovery bills no prompt.
                    self.took_back();
                    if recovered {
                        // Twice in a row, on two containers: whatever is
                        // wrong is not this container (`retry_dead_container`).
                        self.hold.mark_failed();
                        return Err(GatewayError::Transport(why));
                    }
                    recovered = true;
                    self.hold
                        .recover(attempt)
                        .await
                        .map_err(|e| restart_failed(self.hold, &why, e))?;
                }
                Verdict::Climb { prompt, backstop } => {
                    // Released before the climb: its drain waits for every
                    // send counted on the running rung, this one included
                    // (§12 entry 30).
                    drop(guard);
                    // The attempt was dropped (or refused before any work):
                    // a stop during the climb bills no prompt.
                    self.took_back();
                    backstop_left &= !backstop;
                    let prompt = known.map_or(prompt, |k| k.max(prompt));
                    known = Some(prompt);
                    // Boxed: the climb's poll frame is 111 KB in a debug build (chain of
                    // frames from the turn to the climb, 2 MiB test stack).
                    if let Some(fallback) = Box::pin(self.climb(&tag, prompt)).await? {
                        return Ok(fallen_back(lease, fallback));
                    }
                }
            }
        }
    }

    /// The attempt just made is not being worked on any more: the caller's
    /// record of a sent prompt is cleared ([`send_gated_marked`]).
    fn took_back(&self) {
        if let Some(sent) = self.prompt_sent {
            sent.store(false, Ordering::Relaxed);
        }
    }

    /// Count and send together on the container `attempt` names (its port
    /// and generation, judged with `started`, its start facts), and decide.
    /// The response is never handed over before the count's verdict on it.
    ///
    /// Both go out with the send's tool-image decision rechecked against
    /// that container (`tool_images::recheck`): a rung climbed to, or a
    /// container recovered onto, that does not see gets placeholders in the
    /// count and in the send alike.
    async fn pair(
        &self,
        attempt: (u16, u64),
        started: Option<&super::facts::GateFacts>,
        per_slot: u64,
        backstop_left: bool,
    ) -> Result<Verdict, GatewayError> {
        let (port, generation) = attempt;
        let root = format!("http://127.0.0.1:{port}");
        let mut on = self.route.clone();
        crate::vram::LocalHold::point_at(&mut on, port);
        super::tool_images::recheck(&mut on, self.hold, generation, started);
        let sent = crate::vram::send_built((self.build)(&on)?, self.timeout);
        let counted = self.count_on(&root, &on.upstream);
        tokio::pin!(sent, counted);
        // A guest's second backstop refusal is held back for a re-pick.
        let guest = candidate::holds_back_refusals(Some(self.hold)).then(|| self.hold.model_id());

        let mut held = None;
        let count = loop {
            tokio::select! {
                n = &mut counted => break n,
                r = &mut sent, if held.is_none() => match judge(r, backstop_left, guest).await? {
                    Judged::Hold(resp) => held = Some(resp),
                    Judged::Now(verdict) => return Ok(verdict),
                },
            }
        };
        let prompt = match count {
            Ok(n) => n,
            Err(GatewayError::Transport(why)) => return Ok(Verdict::Dead(why)),
            // Phase 1's rule: the container refusing the count (a 400 from
            // `/apply-template`) is the request's own answer.
            Err(e) => return Err(e),
        };
        if prompt.saturating_add(self.plan.max_output) > per_slot {
            return Ok(Verdict::Climb {
                prompt,
                backstop: false,
            });
        }
        let resp = match held {
            Some(resp) => resp,
            None => match judge(sent.await, backstop_left, guest).await? {
                Judged::Hold(resp) => resp,
                Judged::Now(verdict) => return Ok(verdict),
            },
        };
        Ok(Verdict::Answer(resp))
    }

    /// The exact count of this request on the server at `root`: the chat
    /// body's prompt with its media bound, or the legacy body's largest
    /// prompt. Bounded by the route's own request timeout, like the count of
    /// a guarded pool. The chat body is rendered for `upstream`, the route
    /// this attempt sends on.
    async fn count_on(
        &self,
        root: &str,
        upstream: &crate::config::Upstream,
    ) -> Result<u64, GatewayError> {
        let http = &self.state.http;
        let counted = async {
            match self.count {
                CountInput::Chat { ir, params, stream } => {
                    let body = crate::egress::llama_cpp::chat_body(
                        ir,
                        &self.route.upstream_model,
                        params,
                        *stream,
                        upstream,
                    );
                    let bound = self.plan.image_bound.clone();
                    count_chat_prompt(http, root, &body, self.plan.media, bound)
                        .await
                        .map(|c| c.total)
                }
                CountInput::Text { body } => {
                    let mut largest = 0;
                    for prompt in split_prompts(body.get("prompt"))? {
                        let n = match prompt {
                            Prompt::Tokens(n) => n,
                            Prompt::Text(content) => count_text_prompt(http, root, content).await?,
                        };
                        largest = largest.max(n);
                    }
                    Ok(largest)
                }
            }
        };
        match self.route.upstream.request_timeout() {
            Some(d) => tokio::time::timeout(d, counted)
                .await
                .map_err(|_| GatewayError::Timeout)?,
            None => counted.await,
        }
    }

    /// Climb to the smallest rung above `running` that holds `prompt` plus
    /// the max output. `Ok(None)`: judge again on whatever runs now — a
    /// climb's `Done` does not promise the rung it asked for (another
    /// trigger's climb, a failed one). `Ok(Some)`: a fallback answers.
    async fn climb(&self, running: &RungTag, prompt: u64) -> Result<Option<Sent>, GatewayError> {
        let max_output = self.plan.max_output;
        let model = self.hold.model_id();
        let to = {
            let snap = self.state.snapshot();
            let row = snap
                .local_models
                .iter()
                .find(|m| m.model_id == model)
                .ok_or_else(|| GatewayError::Upstream {
                    status: 502,
                    provider_type: None,
                    message: format!(
                        "'{model}' needs a higher rung for this request, and it is no longer \
                         configured"
                    ),
                })?;
            let trained = trained_contexts(self.state, &snap.settings.router.models_dir, row).await;
            let need = prompt.saturating_add(max_output);
            target_rung(row, &trained, running.index, need).map_err(|top| {
                GatewayError::ContextExceeded {
                    model: model.to_string(),
                    prompt_tokens: prompt,
                    max_output: Some(max_output),
                    limit: top.per_slot,
                    top_rung: Some(top.label()),
                }
            })?
        };
        debug_assert!(to > running.index, "a climb only goes up");
        let reason = climb_reason(prompt, max_output, running.per_slot);
        let need = prompt.saturating_add(max_output);
        // Boxed: 78 KB poll frame in a debug build, on the turn -> gate -> climb chain.
        let climbed = match Box::pin(crate::vram::climb_for(
            self.state, self.hold, to, need, &reason,
        ))
        .await
        {
            Ok(climbed) => climbed,
            // A guest's climb that failed (§12 entry 92): nothing was sent
            // yet, and the walk goes on without this model, as for a denial —
            // ending in the climb's own error when nothing else answers.
            Err(e) if candidate::repicks_climb(self.hold, &e) => {
                return Ok(Some(Sent::Rerouted(
                    candidate::repick_failed_climb(self.state, self.hold, e).await,
                )));
            }
            Err(e) => return Err(e),
        };
        match climbed {
            Climbed::Done => Ok(None),
            Climbed::Fallback {
                alias,
                route,
                reason,
            } => Ok(Some(Sent::Rerouted(Ok(Opened {
                route,
                hold: None,
                headers: GateHeaders::falling_back(alias, reason),
            })))),
            // A guest may not make this climb (§9): nothing was sent yet, and
            // the walk picks again without this model (§12 entry 45). Only a
            // candidate alias's guest is ever denied; without its context the
            // deferral is the answer.
            Climbed::Denied { why } => {
                let cause = GatewayError::GpuDeferred {
                    model: self.hold.alias().to_string(),
                    detail: why,
                };
                Ok(Some(Sent::Rerouted(
                    candidate::repick(self.state, self.hold, cause).await,
                )))
            }
        }
    }
}

/// A fallback answers: the lease no longer names a rung, and holds no send.
fn fallen_back(lease: &mut TurnLease, fallback: Sent) -> Sent {
    lease.served = None;
    lease.send = None;
    fallback
}

/// Read what the send's answer says on its own. A `400` is llama-server
/// refusing before any work — one small JSON body, read whole here to see
/// whether it is the context refusal (§2.1 fact 3); every other answer waits
/// for the count.
///
/// `guest` is the model a guest's candidate send is on
/// ([`candidate::holds_back_refusals`]): the backstop's second context
/// refusal is then [`GatewayError::ContextExceeded`], which the gate answers
/// by picking again (candidate-aliases §12 entry 92), rather than the `400`
/// the owner gets.
async fn judge(
    sent: Result<reqwest::Response, GatewayError>,
    backstop_left: bool,
    guest: Option<&str>,
) -> Result<Judged, GatewayError> {
    let resp = match sent {
        Ok(resp) => resp,
        Err(GatewayError::Transport(why)) => return Ok(Judged::Now(Verdict::Dead(why))),
        Err(e) => return Err(e),
    };
    if resp.status() != reqwest::StatusCode::BAD_REQUEST {
        return Ok(Judged::Hold(resp));
    }
    let (status, version, headers) = (resp.status(), resp.version(), resp.headers().clone());
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return match GatewayError::from(e) {
                GatewayError::Transport(why) => Ok(Judged::Now(Verdict::Dead(why))),
                other => Err(other),
            }
        }
    };
    if let Some(ec) = crate::egress::llama_cpp::parse_exceed_context(&bytes) {
        if backstop_left {
            return Ok(Judged::Now(Verdict::Climb {
                prompt: ec.n_prompt_tokens,
                backstop: true,
            }));
        }
        if let Some(model) = guest {
            return Err(GatewayError::ContextExceeded {
                model: model.to_string(),
                prompt_tokens: ec.n_prompt_tokens,
                max_output: None,
                limit: ec.n_ctx,
                top_rung: None,
            });
        }
    }
    // Any other refusal — or the backstop's second one — is the answer, as
    // the container gave it: rebuilt from the bytes already read.
    Ok(Judged::Now(Verdict::Answer(rebuild(
        status, version, headers, bytes,
    ))))
}

/// [`crate::vram::retry_dead_container`]'s wording for a recovery that could
/// not start the model again.
fn restart_failed(hold: &LocalHold, why: &str, e: GatewayError) -> GatewayError {
    // A candidate the request may not bring back is the gate's to answer —
    // by picking again — not a restart that failed (candidate-aliases §12
    // entry 46).
    if matches!(e, GatewayError::CandidateLost { .. }) {
        return e;
    }
    GatewayError::Upstream {
        status: 502,
        provider_type: None,
        message: format!(
            "{} model '{}' stopped answering ({why}) and could not be restarted: {e}",
            hold.class(),
            hold.model_id()
        ),
    }
}
