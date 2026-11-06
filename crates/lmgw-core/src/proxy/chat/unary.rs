//! The non-streaming send of [`super::handle_chat`].
//!
//! On a cloud row the send and the read of the answer run as a task of
//! their own (openai egress design §13.4, after the WP9 review). hyper drops
//! the handler when its client goes away, and with it everything the
//! handler awaits: a platform answer that took a minute to generate would
//! leave a `499` with no usage, and a client could take whole answers
//! budget-free by hanging up just before them. The task outlives that: it
//! reads the answer, and when nobody waits for it any more it writes the
//! `499` with the answer's usage itself, counted as answered. It borrows
//! the request's open row for that ([`Unanswered::lend`]) and gives it back
//! when the handler takes its answer. A local row reads its answer
//! inline, as before: a local model's slot is freed by closing the
//! connection.

use std::sync::Arc;

use axum::body::Bytes;
use axum::http::HeaderMap;
use tracing::Instrument;

use super::*;
use crate::proxy::unanswered::{Lent, LentRow};

/// What one send came to, before it is read as an answer. `#[allow]` like
/// [`Turn`]: there is one of these per send.
#[allow(clippy::large_enum_variant)]
enum Read {
    /// Nothing was sent; the gate sent the request elsewhere.
    Rerouted(Result<crate::gate::Opened, crate::gate::OpenFailed>),
    /// The upstream's answer, read whole.
    Answer {
        status: StatusCode,
        headers: HeaderMap,
        ttfb: i64,
        bytes: Bytes,
    },
}

/// Everything the send needs, owned, so it can run as a task of its own.
struct Unary {
    state: SharedState,
    proto: ClientProto,
    ctx: RequestCtx,
    ir: Arc<ChatRequest>,
    route: Route,
    hold: Option<crate::vram::LocalHold>,
    lease: crate::gate::TurnLease,
    egress: &'static dyn Egress,
    params: crate::ir::Params,
    fitted: super::super::reasoning_fit::Fitted,
    started: Instant,
    fallback: Option<crate::gate::FallbackReason>,
    /// What the request carries to be handed on at the send (an exhausted
    /// quota, an unreachable upstream).
    hand: super::super::reasoning_fit::Handover,
    /// The route is a local model's: free, nothing to bound and nothing to
    /// lose to a hang-up.
    local: bool,
    /// What the lease said once the send was judged, before the answer's
    /// read released it.
    facts: Facts,
}

/// What a send's lease said about it, for its row and headers.
#[derive(Default)]
struct Facts {
    rung: Option<crate::gate::RungTag>,
    max_tokens_clamped: Option<u32>,
    degraded: Option<String>,
}

/// An answer read whole, parsed and counted: done by the task before it
/// hands the answer over, so that a handler dropped while the count runs
/// (a blocking encode, on a long answer) cannot leave an answer that was
/// read in full to be written off as unanswered.
struct Parsed {
    ttfb: i64,
    completion: Result<crate::ir::Completion, GatewayError>,
    count: Option<crate::pricing::LocalCount>,
}

impl Unary {
    /// The send and the read of its whole answer. A body that could not be
    /// read to its end (a timeout, a reset) drops the lease unfinished:
    /// released once llama-server lets go of the slot.
    async fn run(&mut self) -> Result<Read, GatewayError> {
        let timeout = self.route.upstream.request_timeout();
        let (state, egress, ir) = (&self.state, self.egress, &*self.ir);
        let sent = super::super::reasoning_fit::send_chat(
            state,
            self.hold.as_ref(),
            &self.route,
            &mut self.lease,
            ir,
            &self.params,
            false,
            None,
            None,
            None,
            &mut self.fitted,
            super::super::reasoning_fit::RowAs::Public {
                proto: self.proto,
                ctx: &self.ctx,
                alias: &ir.model_alias,
                fallback: self.fallback,
            },
            Some(&self.hand),
            |r, p| {
                Ok(with_timeout(
                    egress.build_chat(&state.http, &r.upstream, &r.upstream_model, ir, p, false)?,
                    timeout,
                ))
            },
        )
        .await;
        self.facts = Facts {
            rung: self.lease.rung(),
            max_tokens_clamped: self.lease.max_tokens_clamped(),
            degraded: self.lease.degraded(),
        };
        let resp = match sent? {
            crate::gate::Sent::Upstream(resp) => resp,
            crate::gate::Sent::Rerouted(r) => return Ok(Read::Rerouted(r)),
        };
        let status = resp.status();
        let headers = resp.headers().clone();
        let ttfb = self.started.elapsed().as_millis() as i64;
        let bytes = resp.bytes().await.map_err(GatewayError::from)?;
        // The whole answer is read: the generation is over, and so is its
        // claim on the pool.
        std::mem::take(&mut self.lease).complete();
        Ok(Read::Answer {
            status,
            headers,
            ttfb,
            bytes,
        })
    }

    /// An answer read whole, as the completion it is, or the error it says.
    fn completion(
        &self,
        status: StatusCode,
        headers: &HeaderMap,
        bytes: &Bytes,
    ) -> Result<crate::ir::Completion, GatewayError> {
        if !status.is_success() {
            return Err(crate::gate::attribute(
                crate::egress::chat_error(
                    self.egress,
                    status.as_u16(),
                    headers,
                    bytes,
                    self.ir.format_ask(),
                ),
                &self.route,
            ));
        }
        self.egress
            .parse_completion_from(&self.route.upstream, bytes)
    }

    /// The budgets' bound for an answer that reported no usage on a cloud
    /// row: counted on an `openai` row (§13.4), estimated elsewhere
    /// ([`floor_count`]).
    async fn count(&self, c: &crate::ir::Completion) -> Option<crate::pricing::LocalCount> {
        if c.usage.reported() {
            return None;
        }
        let output = crate::egress::openai::completion_text(c);
        floor_count(&self.route, self.local, &self.ir, output).await
    }

    /// The budgets' bound for a failed whole Response that reported no
    /// usage (decision 22): its prompt, counted as an answer's is, and no
    /// output. `None` for every other error, which bills nothing.
    async fn failed_count(&self, e: &GatewayError) -> Option<crate::pricing::LocalCount> {
        match e.failed_answer() {
            Some(f) if !f.usage.reported() => {
                floor_count(&self.route, self.local, &self.ir, String::new()).await
            }
            _ => None,
        }
    }

    /// [`Parsed`] of a read that is an answer.
    async fn parse(&self, read: &Result<Read, GatewayError>) -> Option<Parsed> {
        let Ok(Read::Answer {
            status,
            headers,
            ttfb,
            bytes,
        }) = read
        else {
            return None;
        };
        let completion = self.completion(*status, headers, bytes);
        // Recorded before the answer is handed over (`answer_items`).
        if let Some(items) = completion
            .as_ref()
            .ok()
            .and_then(|c| c.upstream_items.as_ref())
        {
            crate::proxy::answer_items::remember(
                &self.state,
                &self.route,
                self.ctx.principal.key_id(),
                &self.ir.model_alias,
                items,
            )
            .await;
        }
        let count = match &completion {
            Ok(c) => self.count(c).await,
            Err(e) => self.failed_count(e).await,
        };
        Some(Parsed {
            ttfb: *ttfb,
            completion,
            count,
        })
    }

    /// The row of a send whose handler is gone: the `499` with the answer's
    /// usage, counted as answered, or the error it ended with.
    async fn record_gone(
        self,
        read: Result<Read, GatewayError>,
        parsed: Option<Parsed>,
        lent: Option<Lent>,
    ) {
        let Some(lent) = lent else {
            return;
        };
        let row = LentRow {
            max_tokens_clamped: self.facts.max_tokens_clamped,
            rung: self.facts.rung.as_ref().map(crate::gate::RungTag::log),
            degraded: self.facts.degraded.clone(),
            ..Default::default()
        };
        let answer = match (read, parsed) {
            (Ok(Read::Answer { .. }), Some(p)) => match p.completion {
                Ok(c) => Ok((p.ttfb, c, p.count)),
                Err(e) => Err((e, Some(p.ttfb), p.count)),
            },
            (Ok(Read::Rerouted(_)), _) => {
                let why = "the client disconnected while waiting for the model's answer; \
                           nothing was answered";
                lent.record(
                    CLIENT_CLOSED_STATUS,
                    Usage::default(),
                    Some((CLIENT_DISCONNECTED, why.to_string())),
                    row,
                    None,
                )
                .await;
                return;
            }
            // A hand-over the read decided is the handler's to make, and
            // the handler is gone: the attempt's error stands, saying so.
            (Err(e), _) => Err((
                match self.hand.take_deferred() {
                    Some(h) => e.with_clause(&format!(
                        "(the client left before it was handed to {}'s fallback)",
                        h.hop()
                    )),
                    None => e,
                },
                None,
                None,
            )),
            (Ok(Read::Answer { .. }), None) => {
                unreachable!("an answer is parsed before it is handed over")
            }
        };
        match answer {
            Ok((ttfb, c, local_count)) => {
                let row = LentRow {
                    ttfb_ms: Some(ttfb),
                    timings: c.timings,
                    quantities: crate::pricing::Quantities::answered(),
                    service_tier: self.ir.served_tier(&self.route, c.service_tier.clone()),
                    ..row
                };
                lent.record(
                    CLIENT_CLOSED_STATUS,
                    c.usage,
                    Some((
                        CLIENT_DISCONNECTED,
                        "client disconnected before the answer was sent".to_string(),
                    )),
                    row,
                    local_count,
                )
                .await;
            }
            // A failed whole Response keeps what it reported, or its local
            // floor (decision 22); every other error bills nothing.
            Err((e, ttfb, local_count)) => {
                let failed = e.failed_answer();
                let row = LentRow {
                    ttfb_ms: ttfb,
                    service_tier: failed.map_or_else(Default::default, |f| {
                        self.ir.served_tier(&self.route, f.service_tier.clone())
                    }),
                    ..row
                };
                lent.record(
                    e.http_status().as_u16(),
                    failed.map_or_else(Usage::default, |f| f.usage),
                    Some((e.kind(), e.log_text())),
                    row,
                    local_count,
                )
                .await;
            }
        }
    }
}

/// `headers` learn the rung the send was judged on or served from, whatever
/// the outcome (ladder design §6).
#[allow(clippy::too_many_arguments)]
pub(super) async fn unary_chat(
    state: &SharedState,
    proto: ClientProto,
    ctx: &RequestCtx,
    ir: &Arc<ChatRequest>,
    route: &Route,
    headers: &mut GateHeaders,
    hold: Option<crate::vram::LocalHold>,
    lease: crate::gate::TurnLease,
    egress: &'static dyn Egress,
    params: &crate::ir::Params,
    fitted: &mut super::super::reasoning_fit::Fitted,
    started: Instant,
    unanswered: &mut Unanswered,
) -> Result<Turn<Response>, GatewayError> {
    let local = state.snapshot().is_local_upstream(route.upstream.id);
    let mut send = Unary {
        state: state.clone(),
        proto,
        ctx: ctx.clone(),
        ir: ir.clone(),
        route: route.clone(),
        hold,
        lease,
        egress,
        params: params.clone(),
        fitted: std::mem::take(fitted),
        started,
        fallback: headers.fallback_reason(),
        hand: super::super::reasoning_fit::Handover {
            check: crate::gate::RouteCheck::Text(super::endpoint(proto)),
            origin: ctx.origin(),
            chain: headers.chain().clone(),
            // The cloud read runs detached (module doc): a hand-over it
            // decides is made here, in the handler, never by the task.
            deferred: (!local).then(super::super::reasoning_fit::Deferred::default),
        },
        local,
        facts: Facts::default(),
    };
    let read = if !local {
        // A task of its own (module doc), which writes the row itself when
        // nobody waits for its answer any more.
        let lent = unanswered.lend();
        let (mut tx, rx) = tokio::sync::oneshot::channel();
        // `timeout_ms = 0` on the upstream is the owner's choice of no bound
        // on the read; once the client is gone the read goes on regardless,
        // which is said (nothing else would show a task waiting on a hung
        // upstream).
        let unbounded = route.upstream.request_timeout().is_none();
        let (upstream, alias) = (route.upstream.name.clone(), ir.model_alias.clone());
        tokio::spawn(
            async move {
                let read = {
                    let run = send.run();
                    tokio::pin!(run);
                    if unbounded {
                        tokio::select! {
                            r = &mut run => r,
                            () = tx.closed() => {
                                tracing::warn!(
                                    %upstream, %alias,
                                    "the client left a request whose upstream has no timeout \
                                     (timeout_ms = 0): its answer is still read, without a \
                                     bound, until the upstream ends it"
                                );
                                run.await
                            }
                        }
                    } else {
                        run.await
                    }
                };
                let parsed = send.parse(&read).await;
                if let Err((read, parsed, send, lent)) = tx.send((read, parsed, send, lent)) {
                    send.record_gone(read, parsed, lent).await;
                }
            }
            .in_current_span(),
        );
        let Ok((read, parsed, back, lent)) = rx.await else {
            return Err(GatewayError::Internal(
                "the platform send's task failed".to_string(),
            ));
        };
        unanswered.restore(lent);
        send = back;
        (read, parsed)
    } else {
        let read = send.run().await;
        let parsed = send.parse(&read).await;
        (read, parsed)
    };
    let (read, parsed) = read;
    *fitted = std::mem::take(&mut send.fitted);
    let Facts {
        rung: rung_tag,
        max_tokens_clamped,
        degraded,
    } = std::mem::take(&mut send.facts);
    let rung = rung_tag.as_ref().map(crate::gate::RungTag::log);
    headers.set_rung(rung_tag);
    // A hand-over the detached read decided (an exhausted quota, an
    // unreachable upstream): made here, where a hang-up cancels it, so no
    // fallback is admitted or started for a client that left.
    if let Some(h) = send.hand.take_deferred() {
        drop(send);
        let rows = super::super::reasoning_fit::RowAs::Public {
            proto,
            ctx,
            alias: &ir.model_alias,
            fallback: headers.fallback_reason(),
        };
        let hand = super::super::reasoning_fit::Handover {
            check: crate::gate::RouteCheck::Text(super::endpoint(proto)),
            origin: ctx.origin(),
            chain: headers.chain().clone(),
            deferred: None,
        };
        return match super::super::reasoning_fit::open_deferred(state, &hand, h, rows).await? {
            crate::gate::Sent::Rerouted(r) => Ok(Turn::Rerouted(r)),
            crate::gate::Sent::Upstream(_) => Err(GatewayError::Internal(
                "a hand-over answered with the hop's own send".into(),
            )),
        };
    }
    if let Read::Rerouted(r) = read? {
        return Ok(Turn::Rerouted(r));
    }
    let Some(Parsed {
        ttfb,
        completion,
        count: local_count,
    }) = parsed
    else {
        unreachable!("an answer is parsed before it is handed over");
    };
    let completion = match completion {
        Ok(c) => c,
        // A failed whole Response: its row counts what it reported, or its
        // local floor (decision 22), and its client gets the error.
        Err(e) if e.failed_answer().is_some() => {
            let resp = error_response(proto, &e);
            let failed = e.failed_answer();
            unanswered.logging();
            record_counted(
                LogParams {
                    state,
                    proto,
                    ctx,
                    alias: ir.model_alias.clone(),
                    route: Some(route),
                    started,
                    streamed: false,
                    class: RequestClass::Chat,
                    timings: None,
                    max_tokens_clamped,
                    fallback: headers.fallback_reason(),
                    rung,
                    degraded,
                    quantities: Default::default(),
                    service_tier: ir
                        .served_tier(route, failed.and_then(|f| f.service_tier.clone())),
                },
                resp.status().as_u16(),
                Some(ttfb),
                failed.map_or_else(Usage::default, |f| f.usage),
                Some((e.kind(), e.log_text())),
                local_count,
            )
            .await;
            return Ok(Turn::Answered(resp));
        }
        Err(e) => return Err(e),
    };
    let body = proto.serialize_completion(&ir.model_alias, &completion);
    // A platform answer that reported no usage: what was sent and answered,
    // counted locally, bounds what the budgets see of it (§13.4).
    unanswered.logging();
    record_counted(
        LogParams {
            state,
            proto,
            ctx,
            alias: ir.model_alias.clone(),
            route: Some(route),
            started,
            streamed: false,
            class: RequestClass::Chat,
            timings: completion.timings,
            max_tokens_clamped,
            fallback: headers.fallback_reason(),
            rung,
            degraded,
            quantities: Default::default(),
            service_tier: ir.served_tier(route, completion.service_tier.clone()),
        },
        StatusCode::OK.as_u16(),
        Some(ttfb),
        completion.usage,
        None,
        local_count,
    )
    .await;
    // A block the client's wire has no place for is named, not dropped unseen.
    Ok(Turn::Answered(name_answer_blocks(
        axum::Json(body).into_response(),
        proto,
        &completion.content,
    )))
}
