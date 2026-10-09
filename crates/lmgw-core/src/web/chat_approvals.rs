//! `POST /chat/api/threads/{id}/approvals` (client-apps design §6.3, L13):
//! decide the calls a gated turn stopped on, and resume it.
//!
//! - **Every waiting call needs a verdict**, as `/v1/responses` has it: a
//!   missing one is `400 approval_missing` naming each; an id no call of
//!   the thread waits under is `404 approval_not_found`.
//! - **The first decision wins** (one write, `store::chat_approvals`): a
//!   call decided already is `409 approval_decided`, naming who decided; a
//!   reply that is no longer the thread's last message is `409
//!   approval_moved_on`, checked in that write.
//! - **A device approves only within its own reach**: a call its key's
//!   tool scope does not admit, or one of lmgw's admin tools unless its own
//!   admin tools may do everything (`full`, capped by the gateway's level),
//!   is `403 approval_out_of_scope`, and nothing of the batch is decided.
//!   Declining is open to anyone who sees the thread. The owner approves
//!   anything the thread offers.
//! - **The resumed turn runs as its starter**, the principal whose key the
//!   reply stored: its scope, policy and attribution, whoever approved. A
//!   starter whose key is gone or disabled is `409
//!   approval_starter_unavailable` naming it, and nothing is decided. The
//!   starter's concurrency slot is taken before the decision too.
//! - **A turn decided but never started** (its starter gone after the
//!   write, or the turn refused or stopped before its tool loop) closes its
//!   calls as not run ([`unrun`]): the record and the feed say so.
//! - **It streams the frames a send streams**, to the approver's request;
//!   `speak` reads it aloud as a send's does.
//! - **The approver** is on each approved call's request row
//!   (`request_logs.approved_by`), in the feed's `approval.decided`, and in
//!   `_meta["lmgw/approval"]` of a call forwarded to a device
//!   (`agent::approval`).
//!
//! A session bound to the thread decides through [`approve`] too (§6.4),
//! with its own principal as approver; every decision wakes the bound
//! sessions ([`LiveTurns::approvals_decided`]), so one that showed the call
//! says it was decided elsewhere.
//!
//! [`LiveTurns::approvals_decided`]: super::chat_live::LiveTurns::approvals_decided

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use lmgw_api_types::chat_approvals::{code, ApprovalsRequest};

use super::chat::err_json;
use super::chat_caller::Caller;
use super::chat_extract::{ChatJson, ChatPath};
use super::chat_repo::ChatRepo;
use super::chat_turn::resume::Resume;
use super::chat_turn::{self, TurnAs, TurnMode};
use crate::agent::PendingCall;
use crate::ingress::ClientProto;
use crate::state::SharedState;
use crate::store::{ApprovalRefusal, ChatThread, PendingApprovals, Verdict};

/// A refusal of decisions, as the route answers it and a bound session
/// words its error.
#[derive(Debug, Clone)]
pub(crate) struct Refused {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl Refused {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub(crate) fn response(&self) -> Response {
        err_json(self.status, self.code, self.message.clone())
    }
}

/// Decisions taken: who the resumed turn runs as, and what it resumes.
pub(crate) struct Approved {
    pub starter: Caller,
    pub resume: Resume,
    /// The starter's concurrency slot, taken before anything was decided
    /// (`Caller::turn_slot`): the resumed turn holds it for its length.
    /// `None` for the owner, a key with no limit, or a request that holds
    /// the starter's slot already.
    pub slot: Option<crate::policy::ConcurrencyGuard>,
}

/// Where a decision comes from: the protocol a refused slot's row is
/// labelled with, and the key whose slot the deciding request already
/// holds (a bound session's own), whose turns take no second one.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Via {
    pub proto: ClientProto,
    pub holds: Option<i64>,
}

fn ids(list: &[String]) -> String {
    list.iter()
        .map(|id| format!("'{id}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn refused_by(e: &crate::error::GatewayError) -> Refused {
    Refused::new(e.http_status(), e.code(), e.to_string())
}

/// The refusal of a device approving calls beyond its own reach (module
/// doc): a call its key's tool scope does not admit, or one of lmgw's admin
/// tools while its admin tools may not do everything — the resumed turn
/// runs as its starter, whose reach may be wider, so the approval would be
/// the device's path to it. The owner approves anything the thread offers.
/// `None` when nothing approved is beyond it.
async fn beyond_reach(
    state: &SharedState,
    approver: &Caller,
    held: &[&PendingApprovals],
    verdicts: &[Verdict],
) -> Option<Refused> {
    if !approver.is_device() {
        return None;
    }
    let approved: Vec<&PendingCall> = verdicts
        .iter()
        .filter(|v| v.approve)
        .filter_map(|v| held.iter().find_map(|p| p.call(&v.approval_request_id)))
        .collect();
    if approved.is_empty() {
        return None;
    }
    let scope = approver.scope(state).await;
    let level = scope.admin_level(state.snapshot().settings.self_admin);
    let beyond: Vec<String> = approved
        .iter()
        .filter(|c| {
            !scope.admits(&c.name)
                || (crate::mcp::selfadmin::owns(&c.name) && !level.allows_write())
        })
        .map(|c| format!("'{}' ({})", c.name, c.approval_id))
        .collect();
    if beyond.is_empty() {
        return None;
    }
    Some(Refused::new(
        StatusCode::FORBIDDEN,
        code::APPROVAL_OUT_OF_SCOPE,
        format!(
            "{} may not approve {}: beyond its own reach (a tool its key's tool scope does not \
             admit, or one of lmgw's admin tools while its own may not do everything); it may \
             decline them, and nothing was decided",
            approver.named(),
            beyond.join(", ")
        ),
    ))
}

/// The ids of the gated calls `resume` decided (approved or declined).
fn decided_ids(resume: &Resume) -> Vec<String> {
    resume
        .decided
        .iter()
        .filter(|d| d.by.is_some())
        .map(|d| d.call.approval_id.clone())
        .collect()
}

/// The turn `resume` was decided for never started (a refusal after the
/// decision was written, or a stop before its tool loop): its calls are
/// closed as not run, and the feed says so (`store::close_unrun`). The
/// reply no longer holds MCP task results off then (`chat_tasks::deliver`):
/// what waits enters, unless a turn of the thread runs (it delivers when
/// it ends).
pub(in crate::web) async fn unrun(
    state: &crate::state::AppState,
    repo: ChatRepo,
    thread_id: i64,
    resume: &Resume,
) {
    let ids = decided_ids(resume);
    if ids.is_empty() {
        return;
    }
    if let Err(e) = repo
        .close_never_run(state, thread_id, resume.message_id, &ids)
        .await
    {
        tracing::warn!(thread_id, "closing calls whose turn never started: {e}");
    }
    if !repo.is_temp() {
        // Boxed, as every await this deep in a turn: its future stays off
        // the stack.
        Box::pin(super::chat_tasks::deliver::when_idle(state, thread_id)).await;
    }
}

/// Decide calls of `thread` with `verdicts` as `approver` (module doc):
/// the decider's reach and the starter checked and its slot taken, the
/// decisions written, the bound sessions woken.
pub(in crate::web) async fn approve(
    state: &SharedState,
    approver: &Caller,
    repo: ChatRepo,
    thread: &ChatThread,
    verdicts: &[Verdict],
    via: Via,
) -> Result<Approved, Refused> {
    let internal = |e: crate::error::GatewayError| {
        Refused::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
    };
    let messages = repo.messages(state, thread.id).await.map_err(internal)?;
    let held: Vec<&PendingApprovals> = messages
        .iter()
        .rev()
        .filter_map(|m| m.pending_approvals.as_ref())
        .collect();
    // The decider's own reach first: nothing is decided by a refused batch.
    if let Some(r) = beyond_reach(state, approver, &held, verdicts).await {
        return Err(r);
    }
    // Then the starter: a turn that cannot run is not decided, and its slot
    // is taken now, so a key at its limit refuses before anything is.
    let mut slot = None;
    let starter = held.iter().find(|p| {
        verdicts
            .iter()
            .any(|v| p.call(&v.approval_request_id).is_some())
    });
    if let Some(p) = starter.filter(|p| p.is_open()) {
        let starter =
            Caller::resumed(&state.snapshot(), p.key_id, p.key_name.as_deref()).map_err(|why| {
                Refused::new(
                    StatusCode::CONFLICT,
                    code::APPROVAL_STARTER_UNAVAILABLE,
                    why,
                )
            })?;
        if via.holds.is_none() || starter.key_id() != via.holds {
            slot = starter
                .turn_slot(state, via.proto, &thread.model_alias)
                .await
                .map_err(|e| refused_by(&e))?;
        }
    }
    let by = approver.decider();
    let claimed = repo
        .decide_approvals(state, thread.id, verdicts, &by)
        .await
        .map_err(|e| match e {
            crate::error::GatewayError::NotFound(_) => {
                Refused::new(StatusCode::NOT_FOUND, "not_found", "thread not found")
            }
            e => internal(e),
        })?;
    let claimed = claimed.map_err(|r| match r {
        ApprovalRefusal::Empty => Refused::new(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "'decisions' is empty: send a verdict for each call that waits",
        ),
        ApprovalRefusal::Decided { id, by } => Refused::new(
            StatusCode::CONFLICT,
            code::APPROVAL_DECIDED,
            format!("call '{id}' was already decided, by {by}"),
        ),
        ApprovalRefusal::Unknown(list) => Refused::new(
            StatusCode::NOT_FOUND,
            code::APPROVAL_NOT_FOUND,
            format!(
                "no call of this thread waits for an approval under {}",
                ids(&list)
            ),
        ),
        ApprovalRefusal::Missing(list) => Refused::new(
            StatusCode::BAD_REQUEST,
            code::APPROVAL_MISSING,
            format!(
                "the reply waits on approval for {} as well; send a decision for each \
                 (approve: false declines it)",
                ids(&list)
            ),
        ),
        ApprovalRefusal::MovedOn => Refused::new(
            StatusCode::CONFLICT,
            code::APPROVAL_MOVED_ON,
            super::chat_turn::resume::MOVED_ON_WHY,
        ),
    })?;
    state.chat_live.approvals_decided(thread.id);
    let resume = Resume {
        message_id: claimed.message_id,
        decided: claimed.decided,
    };
    // Read again after the write: the key may have gone meanwhile. Its
    // calls never run then, and are closed saying so.
    let starter = match Caller::resumed(
        &state.snapshot(),
        claimed.pending.key_id,
        claimed.pending.key_name.as_deref(),
    ) {
        Ok(starter) => starter,
        Err(why) => {
            unrun(state, repo, thread.id, &resume).await;
            return Err(Refused::new(
                StatusCode::CONFLICT,
                code::APPROVAL_STARTER_UNAVAILABLE,
                why,
            ));
        }
    };
    Ok(Approved {
        starter,
        resume,
        slot,
    })
}

/// `POST /chat/api/threads/{id}/approvals` (module doc).
pub(super) async fn decide(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
    ChatJson(req): ChatJson<ApprovalsRequest>,
) -> Response {
    let repo = ChatRepo::of(id);
    let thread = match repo.thread_as(&state, &caller, id).await {
        Ok(Some(t)) => t,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "not_found", "thread not found"),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    };
    let verdicts: Vec<Verdict> = req
        .decisions
        .into_iter()
        .map(|d| Verdict {
            approval_request_id: d.approval_request_id,
            approve: d.approve,
            reason: d.reason,
        })
        .collect();
    let via = Via {
        proto: ClientProto::Chat,
        holds: None,
    };
    let approved = match approve(&state, &caller, repo, &thread, &verdicts, via).await {
        Ok(a) => a,
        Err(r) => return r.response(),
    };
    let caps = super::chat_attach_gate::thread_caps(&state, repo, &thread).await;
    let mode = TurnMode::Resume {
        message_id: approved.resume.message_id,
    };
    let turn = TurnAs {
        caller: approved.starter,
        resume: Some(approved.resume),
        slot: Some(approved.slot),
        sent: None,
    };
    let speak = req.speak.then(super::chat_voice::ReadAloud::default);
    chat_turn::start_turn_as(&state, &caller, turn, repo, &thread, (mode, caps), speak).await
}

#[cfg(test)]
mod tests;
