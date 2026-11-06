//! Cancelling a running image job (chat-media-tools design §5.4).
//!
//! The job loop owns a [`JobGuard`], which owns the call's claim on the
//! image row. A call that ends — the picture made, the job failed — tells
//! the guard so ([`JobGuard::ended`]), and dropping the guard then only lets
//! the claim go. A call that lets its job go while it runs — a per-call
//! cancel or the turn's Stop (both raise the call's stop, and the call drops
//! its send), a disconnected `/mcp` client, the route's own timeout
//! ([`JobGuard::let_go`]), a job that stalled ([`pace`]) — drops the guard
//! with the job still in it, and the guard cancels the job, from `Drop`, on
//! a task of its own ([`follow`]):
//!
//! | Job | Build | Action | Container |
//! |---|---|---|---|
//! | queued | `cancel_queued` | `POST /sdcpp/v1/jobs/{id}/cancel` | stays |
//! | generating | `cancel_generating` | the same; followed to `cancelled` | stays |
//! | either, without its feature | this call the row's only request | the claim let go, the container [`stop_generation`](crate::runtime::registry::Registry::stop_generation)ped | stopped; the next call loads it again |
//! | either, without its feature | other requests on the row | followed to its end **holding the claim**, its picture thrown away | stays |
//!
//! Holding the claim in the last case keeps admission honest: the GPU is
//! busy until the job ends. The stop names the generation the claim was
//! admitted to, so a container restarted meanwhile is not hit, and it is not
//! forced: a request that joined the row in between keeps it, and the job
//! runs to its end under that request's claim.
//!
//! **A followed job** — after a native cancel, or abandoned — is followed
//! until it ends, fails, or its image server stops answering for it, or
//! until it stalls by its own pace ([`pace`]); never by a clock that started
//! with the call, so a call that timed out holds its claim while its job
//! still draws. A job that stalled while its call followed it is cancelled
//! as a stopped one is, except that no native cancel is asked of a server
//! that stopped working on it: the restart where it is the row's only
//! request, else the claim goes.
//!
//! A native cancel the server refuses (`409`: the job began generating
//! between the last poll and the cancel) falls back to the same rule. A job
//! whose submit was still on the wire has no id yet: there is nothing to
//! name in a cancel, and the claim simply goes.
//!
//! **A synchronous upscale** has no job ([`JobGuard::sync_request`]): it is
//! the restart or abandon row by the same rule, with no cancel of its own.
//! Its request runs on a task of its own, which the guard keeps: a restart
//! aborts it before the container is stopped, and an abandoned upscale is
//! followed to its end **holding the claim**, as an abandoned job is — while
//! the image server still answers its readiness route ([`answer`]).
//!
//! **A server that stops answering** — a status read (a poll, a native
//! cancel, a submit, an upscale's liveness read) unanswered within
//! [`STATUS_ANSWER`](super::answer::STATUS_ANSWER) — is a server that stopped working, as a
//! stalled job's is: the call lets its job go ([`LetGo::Unanswered`]), and
//! the follow of a job a call let go stops following it the same way. Either
//! way no cancel is asked of it: the restart where the call is the row's
//! only request, else the claim goes, with a log line naming the bound.
//!
//! What the guard chose is written into the call's [`Taken`] before the
//! task starts, so the call's result says which one happened.
//!
//! [`pace`]: super::pace
//! [`answer`]: super::answer

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_api_types::chat_media::{CancelKind, CANCELLED_BY_USER};
use serde_json::Value;
use tokio::time::Instant;

use crate::config::Route;
use crate::error::GatewayError;
use crate::runtime::Class;
use crate::state::SharedState;
use crate::vram::LocalHold;

use super::answer::Unanswered;
use super::pace::{Pace, Stall};

mod follow;
#[cfg(test)]
mod tests;

/// Where a dropped guard says what it did to the job.
pub(crate) type Taken = Arc<Mutex<Option<CancelKind>>>;

/// How far a followed job got, as its last poll said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JobStatus {
    /// Submitted, not polled yet.
    Submitted,
    Queued,
    Generating,
    /// A synchronous request (an upscale): no job to poll or cancel.
    Sync,
}

/// The job a guard follows.
#[derive(Debug)]
struct Job {
    root: String,
    id: String,
    status: JobStatus,
    /// What its polls showed of its progress.
    pace: Pace,
    /// A synchronous request's task (module doc): the guard aborts it for
    /// a restart and follows it to its end when abandoning.
    task: Option<tokio::task::JoinHandle<()>>,
}

/// Why a call let its running job go other than by dropping its guard
/// (module doc): its log line and its error say which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LetGo {
    /// The route's ceiling, `image.request_timeout_seconds`.
    Timeout(Duration),
    /// The job stopped advancing by its own pace.
    Stalled(Stall),
    /// The image server did not answer a status read (module doc).
    Unanswered,
}

impl LetGo {
    /// Whether the image server stopped working on the job: no cancel is
    /// asked of it then (module doc).
    fn server_stopped(self) -> bool {
        matches!(self, Self::Stalled(_) | Self::Unanswered)
    }
}

/// What a guard's call runs, for its error's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Work {
    /// A picture, as a job.
    Job,
    /// A synchronous upscale.
    Upscale,
}

impl Work {
    fn noun(self) -> &'static str {
        match self {
            Self::Job => "job",
            Self::Upscale => "upscale",
        }
    }

    fn made(self) -> &'static str {
        match self {
            Self::Job => "the picture",
            Self::Upscale => "the upscale",
        }
    }
}

/// The call's claim on the image row and the job it runs (module doc).
pub(super) struct JobGuard {
    state: SharedState,
    /// Shared with a synchronous request's task, which sends on it.
    hold: Option<Arc<LocalHold>>,
    /// The route as it is sent (its upstream's credentials).
    route: Route,
    model_id: String,
    cancel_queued: bool,
    cancel_generating: bool,
    /// The route's ceiling for the call: with none, the call's own follow
    /// judges its job's pace too ([`Self::own_pace`]).
    ceiling: Option<Duration>,
    job: Mutex<Option<Job>>,
    let_go: Mutex<Option<LetGo>>,
    taken: Taken,
}

impl JobGuard {
    /// `hold` on row `model_id`, whose capabilities say what it can cancel;
    /// `ceiling` the route's timeout for the whole call.
    pub(super) fn new(
        state: &SharedState,
        hold: LocalHold,
        route: &Route,
        model_id: &str,
        taken: Taken,
        ceiling: Option<Duration>,
    ) -> Self {
        let features = state
            .runtime()
            .image_capabilities(model_id)
            .map(|c| c.features)
            .unwrap_or_default();
        Self {
            state: state.clone(),
            hold: Some(Arc::new(hold)),
            route: route.clone(),
            model_id: model_id.to_string(),
            cancel_queued: features.cancel_queued,
            cancel_generating: features.cancel_generating,
            ceiling,
            job: Mutex::new(None),
            let_go: Mutex::new(None),
            taken,
        }
    }

    /// The call's claim.
    pub(super) fn hold(&self) -> &LocalHold {
        self.hold.as_ref().expect("held until the guard drops")
    }

    /// The call's claim, for a synchronous request's task.
    pub(super) fn shared_hold(&self) -> Arc<LocalHold> {
        self.hold.clone().expect("held until the guard drops")
    }

    /// Job `id` was submitted at `root`.
    pub(super) fn submitted(&self, root: &str, id: &str) {
        *self.lock() = Some(Job {
            root: root.to_string(),
            id: id.to_string(),
            status: JobStatus::Submitted,
            pace: Pace::new(Instant::now()),
            task: None,
        });
    }

    /// A submit at `root` did not answer ([`LetGo::Unanswered`]): the job
    /// may be on the server with no id the call knows, and the guard treats
    /// the server as one that stopped working (module doc).
    pub(super) fn submit_unanswered(&self, root: &str) {
        self.submitted(root, "");
        self.let_go(LetGo::Unanswered);
    }

    /// A poll of the followed job read `job`, its status `status`.
    pub(super) fn read(&self, status: JobStatus, job: &Value) {
        if let Some(j) = self.lock().as_mut() {
            j.status = status;
            j.pace.read(job, Instant::now());
        }
    }

    /// The followed job's pace, when the call's own follow judges it: only
    /// on a route with no ceiling, where nothing else bounds a job that
    /// stopped advancing. A route's ceiling is the owner's bound for a
    /// call, and a slow stage the pace did not foresee (a large picture's
    /// decode after fast steps) is not cut short below it.
    pub(super) fn own_pace(&self) -> Option<Pace> {
        if self.ceiling.is_some() {
            return None;
        }
        self.lock().as_ref().map(|j| j.pace.clone())
    }

    /// A synchronous request on `root` is on the wire, on `task` (module
    /// doc).
    pub(super) fn sync_request(&self, root: &str, task: tokio::task::JoinHandle<()>) {
        *self.lock() = Some(Job {
            root: root.to_string(),
            id: String::new(),
            status: JobStatus::Sync,
            pace: Pace::new(Instant::now()),
            task: Some(task),
        });
    }

    /// The call lets its running job go, for `why`: dropping the guard then
    /// cancels it as a stop does (module doc).
    pub(super) fn let_go(&self, why: LetGo) {
        *self.let_go.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
    }

    /// Why the call let its job go, if it did ([`Self::let_go`]).
    pub(super) fn let_go_why(&self) -> Option<LetGo> {
        *self.let_go.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The call is done with its job: ended, or no longer followed — unless
    /// it let the job go running ([`Self::let_go`]), which stays the
    /// guard's to cancel.
    pub(super) fn ended(&self) {
        if self.let_go_why().is_none() {
            *self.lock() = None;
        }
    }

    /// What a cancel would do now (module doc's table), for status
    /// `status`.
    pub(super) fn kind_now(&self, status: JobStatus) -> CancelKind {
        kind(
            status,
            (self.cancel_queued, self.cancel_generating),
            self.only_request(),
        )
    }

    /// Whether this call's claim is the only request on the container it
    /// was admitted to.
    fn only_request(&self) -> bool {
        let Some(hold) = self.hold.as_ref() else {
            return false;
        };
        only_request(&self.state, &self.model_id, hold.generation())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Job>> {
        self.job.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The table's choice.
fn kind(
    status: JobStatus,
    (cancel_queued, cancel_generating): (bool, bool),
    only: bool,
) -> CancelKind {
    let native = match status {
        JobStatus::Submitted | JobStatus::Queued => cancel_queued,
        JobStatus::Generating => cancel_generating,
        JobStatus::Sync => false,
    };
    match (native, only) {
        (true, _) => CancelKind::Native,
        (false, true) => CancelKind::Restart,
        (false, false) => CancelKind::Abandon,
    }
}

/// Whether row `model_id`'s container `generation` has one request in
/// flight — the caller's own.
fn only_request(state: &SharedState, model_id: &str, generation: u64) -> bool {
    state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.class == Class::Image && v.model_id == model_id)
        .is_some_and(|v| v.generation == generation && v.in_flight <= 1)
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        let job = self.job.get_mut().unwrap_or_else(|e| e.into_inner()).take();
        let (Some(job), Some(hold)) = (job, self.hold.take()) else {
            // No job running: the claim goes with the guard.
            return;
        };
        let why = *self.let_go.get_mut().unwrap_or_else(|e| e.into_inner());
        let only = only_request(&self.state, &self.model_id, hold.generation());
        let chosen = match why {
            // A server that stopped working on the job is asked nothing
            // (module doc).
            Some(w) if w.server_stopped() && only => CancelKind::Restart,
            Some(w) if w.server_stopped() => CancelKind::Abandon,
            _ => kind(
                job.status,
                (self.cancel_queued, self.cancel_generating),
                only,
            ),
        };
        *self.taken.lock().unwrap_or_else(|e| e.into_inner()) = Some(chosen);
        // A drop during runtime teardown has nothing to cancel with; the
        // claim goes, as everything else does then.
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let c = follow::Cancelling {
            state: self.state.clone(),
            hold,
            route: self.route.clone(),
            model_id: self.model_id.clone(),
            job,
            why,
        };
        rt.spawn(c.run(chosen));
    }
}

/// What a stopped call's result says (design §5.4): the model reads it, so
/// it ends by telling it not to try again on its own.
pub(super) fn cancelled_message(alias: &str, how: Option<CancelKind>) -> String {
    let what = match how {
        Some(CancelKind::Native) => " (the image server dropped the job)".to_string(),
        Some(CancelKind::Restart) => format!(
            " ({alias} could not stop mid-picture, so lmgw stopped its container; the next \
            picture loads it again)"
        ),
        Some(CancelKind::Abandon) => format!(
            " (another request is using {alias}, so the picture finishes in the background and \
            is thrown away)"
        ),
        None => String::new(),
    };
    format!("{CANCELLED_BY_USER}; the picture was not made{what}. Do not try again unless asked.")
}

/// The error of a call that let its running `work` go (`why`), once its
/// guard has chosen `how` (module doc): what happened, and what became of
/// the work.
pub(super) fn let_go_error(
    alias: &str,
    why: LetGo,
    how: Option<CancelKind>,
    work: Work,
) -> GatewayError {
    let noun = work.noun();
    let what = match why {
        LetGo::Timeout(d) => format!(
            "{alias} did not finish {} within the route's timeout ({} s, \
             image.request_timeout_seconds)",
            work.made(),
            d.as_secs()
        ),
        LetGo::Stalled(stall) => {
            format!("{alias}'s {noun} {stall}, and the route sets no timeout: lmgw gave up on it")
        }
        LetGo::Unanswered => format!(
            "{alias}'s image server {} while it ran the {noun}: lmgw gave up on it",
            Unanswered
        ),
    };
    let job = match how {
        Some(CancelKind::Native) => "; the image server dropped the job".to_string(),
        Some(CancelKind::Restart) => format!(
            "; lmgw stopped {alias}'s container to end the {noun}, and the next call loads it \
             again"
        ),
        Some(CancelKind::Abandon) if why.server_stopped() => format!(
            "; another request is using {alias}, so its container was left running and the \
             {noun}'s claim goes"
        ),
        Some(CancelKind::Abandon) => format!(
            "; another request is using {alias}, so the {noun} runs to its end holding its \
             claim, and its picture is thrown away"
        ),
        None => String::new(),
    };
    GatewayError::Upstream {
        status: 504,
        provider_type: None,
        message: format!("{what}{job}"),
    }
}
