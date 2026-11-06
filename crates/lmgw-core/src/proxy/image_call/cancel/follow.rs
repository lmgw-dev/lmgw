//! A cancel on its own task (module doc of `cancel`): the native cancel,
//! the restart, and the follow of a job or an upscale to its end, holding
//! the claim.

use std::sync::Arc;

use lmgw_api_types::chat_media::CancelKind;
use serde_json::Value;
use tokio::time::Instant;

use crate::agent::CANCEL_POLL;
use crate::config::Route;
use crate::egress::apply_bearer_auth;
use crate::runtime::registry::RuntimeError;
use crate::runtime::Class;
use crate::state::SharedState;
use crate::vram::LocalHold;

use super::super::answer::{self, Unanswered};
use super::super::pace::{within, Stall};
use super::{only_request, Job, JobStatus, LetGo};

/// How the follow of a let-go job or upscale ended.
enum Followed {
    /// It ended on the image server.
    Ended,
    /// The image server no longer knows it (a dropped connection, an error
    /// status): logged, and its claim goes.
    Lost,
    /// The image server stopped working on it (module doc of `cancel`).
    Stopped(Stopped),
}

/// Why the image server counts as no longer working on the job.
#[derive(Debug, Clone, Copy)]
enum Stopped {
    Stalled(Stall),
    Unanswered,
}

/// What a native cancel came to.
enum Native {
    /// Accepted, or the job is gone already.
    Accepted,
    /// Refused because the job is generating (`409`).
    Refused,
    /// Any other answer, or none on the wire.
    Failed(String),
    Stopped(Stopped),
}

/// What a log line calls the cancelled work.
fn what(job: &Job) -> String {
    match job.status {
        JobStatus::Sync => "an upscale".to_string(),
        _ => format!("job {}", job.id),
    }
}

/// Why the call let the job go, for a log line.
fn why_words(why: Option<LetGo>) -> String {
    match why {
        None => "its call was stopped".to_string(),
        Some(LetGo::Timeout(d)) => format!(
            "its call reached the route's timeout ({} s, image.request_timeout_seconds)",
            d.as_secs()
        ),
        Some(LetGo::Stalled(stall)) => format!("it {stall}"),
        Some(LetGo::Unanswered) => format!("its image server {Unanswered}"),
    }
}

/// A cancel on its own task, owning the claim until it is done.
pub(super) struct Cancelling {
    pub(super) state: SharedState,
    pub(super) hold: Arc<LocalHold>,
    pub(super) route: Route,
    pub(super) model_id: String,
    pub(super) job: Job,
    pub(super) why: Option<LetGo>,
}

impl Cancelling {
    pub(super) async fn run(mut self, chosen: CancelKind) {
        let chosen = match chosen {
            CancelKind::Native => match self.native().await {
                Native::Accepted => {
                    match self.follow_to_end().await {
                        Followed::Ended => tracing::info!(
                            "image model '{}': job {} cancelled on the image server",
                            self.model_id,
                            self.job.id
                        ),
                        Followed::Lost => {}
                        Followed::Stopped(why) => self.stopped(why).await,
                    }
                    return;
                }
                // It began generating before the cancel reached it, on a
                // build that cannot stop that: the rule without the feature.
                Native::Refused => {
                    if only_request(&self.state, &self.model_id, self.hold.generation()) {
                        CancelKind::Restart
                    } else {
                        CancelKind::Abandon
                    }
                }
                Native::Failed(e) => {
                    tracing::warn!(
                        "image model '{}': the cancel of job {} did not reach the image server \
                         ({e}); it is followed to its end instead",
                        self.model_id,
                        self.job.id
                    );
                    CancelKind::Abandon
                }
                Native::Stopped(why) => return self.stopped(why).await,
            },
            other => other,
        };
        match chosen {
            CancelKind::Restart => self.restart().await,
            // A server that stopped working on it is not followed.
            _ if self.why.is_some_and(LetGo::server_stopped) => self.let_go_stopped(),
            _ if self.job.status == JobStatus::Sync => match self.follow_sync().await {
                Followed::Stopped(why) => self.stopped(why).await,
                _ => tracing::info!(
                    "image model '{}': an upscale abandoned after {} has ended on the image \
                     server; its picture is thrown away and its claim goes",
                    self.model_id,
                    why_words(self.why)
                ),
            },
            _ => match self.follow_to_end().await {
                Followed::Ended => tracing::info!(
                    "image model '{}': job {} was abandoned ({}) and has ended; its picture is \
                     thrown away",
                    self.model_id,
                    self.job.id,
                    why_words(self.why)
                ),
                Followed::Lost => {}
                Followed::Stopped(why) => self.stopped(why).await,
            },
        }
    }

    /// The image server stopped working on the followed work (`why`): the
    /// restart where the claim is the row's only request, else the claim
    /// goes (module doc of `cancel`).
    async fn stopped(mut self, why: Stopped) {
        let only = only_request(&self.state, &self.model_id, self.hold.generation());
        let said = match why {
            Stopped::Stalled(stall) => format!("{} {stall}", what(&self.job)),
            Stopped::Unanswered => format!("its image server {Unanswered} for {}", what(&self.job)),
        };
        tracing::warn!(
            "image model '{}': {said}, after {}; {}",
            self.model_id,
            why_words(self.why),
            if only {
                "its container is stopped"
            } else {
                "another request uses the model, so its container is left running and the \
                 claim goes now"
            }
        );
        if only {
            self.restart().await;
        } else {
            self.abort_task().await;
        }
    }

    /// A call that let its work go because the server stopped working on
    /// it, on a row other requests use: the container is left running and
    /// the claim goes.
    fn let_go_stopped(&mut self) {
        if let Some(task) = self.job.task.take() {
            task.abort();
        }
        tracing::info!(
            "image model '{}': {} was let go after {}; another request uses the model, so its \
             container is left running and the claim goes now",
            self.model_id,
            what(&self.job),
            why_words(self.why)
        );
    }

    /// A synchronous request's task ended, aborted if it still ran, so its
    /// share of the claim is gone.
    async fn abort_task(&mut self) {
        if let Some(task) = self.job.task.take() {
            task.abort();
            let _ = task.await;
        }
    }

    /// `POST …/jobs/{id}/cancel`, its answer bounded by the job's pace and
    /// by [`answer::STATUS_ANSWER`].
    async fn native(&self) -> Native {
        let url = format!("{}/sdcpp/v1/jobs/{}/cancel", self.job.root, self.job.id);
        let rb = apply_bearer_auth(self.state.http.post(url), &self.route.upstream);
        let ask = answer::within(async {
            let resp = rb.send().await.map_err(|e| e.to_string())?;
            Ok::<_, String>(resp.status().as_u16())
        });
        match within(&self.job.pace, ask).await {
            Err(stall) => Native::Stopped(Stopped::Stalled(stall)),
            Ok(Err(Unanswered)) => Native::Stopped(Stopped::Unanswered),
            Ok(Ok(Err(e))) => Native::Failed(e),
            Ok(Ok(Ok(s))) => match s {
                s if (200..300).contains(&s) => Native::Accepted,
                409 => Native::Refused,
                // The job is gone already: nothing is left to cancel.
                404 | 410 => Native::Accepted,
                s => Native::Failed(format!("it answered {s}")),
            },
        }
    }

    /// Stop the container the claim was admitted to, the claim let go first
    /// (module doc).
    async fn restart(mut self) {
        // A synchronous request's task shares the claim: it ends first.
        self.abort_task().await;
        let Cancelling {
            state,
            hold,
            model_id,
            job,
            why,
            ..
        } = self;
        let generation = hold.generation();
        drop(hold);
        let registry = state.runtime();
        match registry
            .stop_generation(Class::Image, &model_id, generation, false)
            .await
        {
            Ok(()) => tracing::info!(
                "image model '{model_id}': its container was stopped to cancel {} after {} (the \
                 build cannot stop it running); the next call loads it again",
                what(&job),
                why_words(why)
            ),
            Err(RuntimeError::Busy { in_flight, .. }) => tracing::info!(
                "image model '{model_id}': {in_flight} request(s) joined it before its container \
                 could be stopped to cancel {}; it runs to its end under their claim",
                what(&job)
            ),
            Err(e) => tracing::info!(
                "image model '{model_id}': not stopped to cancel {}: {e}",
                what(&job)
            ),
        }
        crate::vram::broadcast(&state);
    }

    /// Poll the job until it ends, the image server no longer answers for
    /// it, or it stops working on it — the job stalls by its own pace, or a
    /// poll goes unanswered ([`answer::STATUS_ANSWER`]); the claim is held
    /// all the while (module doc). A lost job is logged here.
    async fn follow_to_end(&mut self) -> Followed {
        if !self.job.pace.judged() {
            tracing::info!(
                "image model '{}': job {} has not reported steps enough to judge a stall by its \
                 pace: it is followed, holding its claim, while the image server answers its \
                 polls (each within {} s)",
                self.model_id,
                self.job.id,
                answer::STATUS_ANSWER.as_secs()
            );
        }
        let url = format!("{}/sdcpp/v1/jobs/{}", self.job.root, self.job.id);
        loop {
            let rb = apply_bearer_auth(self.state.http.get(&url), &self.route.upstream);
            let read = answer::within(async {
                let resp = rb.send().await.map_err(|e| e.to_string())?;
                if !resp.status().is_success() {
                    return Err(format!("it answered {}", resp.status().as_u16()));
                }
                Ok(resp.json::<Value>().await.unwrap_or_default())
            });
            let job = match within(&self.job.pace, read).await {
                Ok(Ok(Ok(job))) => job,
                Ok(Ok(Err(why))) => {
                    tracing::info!(
                        "image model '{}': the image server no longer answers for job {} \
                         ({why}); its claim goes now",
                        self.model_id,
                        self.job.id
                    );
                    return Followed::Lost;
                }
                Ok(Err(Unanswered)) => return Followed::Stopped(Stopped::Unanswered),
                Err(stall) => return Followed::Stopped(Stopped::Stalled(stall)),
            };
            match job.get("status").and_then(Value::as_str) {
                Some("queued" | "generating") => self.job.pace.read(&job, Instant::now()),
                _ => return Followed::Ended,
            }
            tokio::time::sleep(CANCEL_POLL).await;
        }
    }

    /// Wait for an abandoned upscale's request to end, holding the claim,
    /// while the image server answers its readiness route every
    /// [`CANCEL_POLL`] (module doc of `cancel`).
    async fn follow_sync(&mut self) -> Followed {
        let Some(task) = self.job.task.as_mut() else {
            return Followed::Ended;
        };
        loop {
            let route = self.hold.on_attempt(&self.route);
            let root = super::super::native_root(&route);
            let alive = async {
                tokio::time::sleep(CANCEL_POLL).await;
                answer::alive(&self.state, &route, &root).await
            };
            let ended = tokio::select! {
                _ = &mut *task => true,
                r = alive => match r {
                    Ok(()) => false,
                    Err(Unanswered) => return Followed::Stopped(Stopped::Unanswered),
                },
            };
            if ended {
                self.job.task = None;
                return Followed::Ended;
            }
        }
    }
}
