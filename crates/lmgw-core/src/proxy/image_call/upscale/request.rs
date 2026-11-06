//! The upscale request on a task of its own (chat-media-tools design §5.4).
//!
//! `POST /sdcpp/v1/upscale` answers only when the picture is done, so the
//! request runs on a task that shares the call's claim, and the guard keeps
//! the task ([`JobGuard::sync_request`]): a stopped or timed-out call can
//! then restart the row (the task aborted first) or leave the upscale to
//! finish holding its claim — never let the claim go while the image server
//! still works. While it waits, the call:
//! - reports `upscaling` with what a cancel would do now, and again when
//!   that changes (a request joined or left the row);
//! - lets the upscale go at the route's ceiling (`LetGo::Timeout`);
//! - reads the image server's readiness route every [`CANCEL_POLL`], and
//!   lets the upscale go when a read goes unanswered
//!   ([`answer::STATUS_ANSWER`], `LetGo::Unanswered`): the request itself
//!   has no pace to judge, and a server that works answers that read
//!   beside it.

use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;

use crate::agent::CANCEL_POLL;
use crate::config::Route;
use crate::error::GatewayError;
use crate::state::SharedState;

use super::super::answer::{self, Unanswered};
use super::super::cancel::{JobGuard, JobStatus, LetGo};
use super::super::{native_root, Hooks, MediaProgress};
use super::UpscaleOutput;

/// Run the upscale of `body` under `guard` for `alias` (module doc).
pub(super) async fn run(
    state: &SharedState,
    (guard, route, alias): (&JobGuard, &Route, &str),
    body: Value,
    hooks: &dyn Hooks,
    ceiling: Option<Duration>,
) -> Result<UpscaleOutput, GatewayError> {
    let (tx, mut answer) = tokio::sync::oneshot::channel();
    let task = tokio::spawn({
        let (state, hold, route) = (state.clone(), guard.shared_hold(), route.clone());
        async move {
            let _ = tx.send(super::send(&state, &hold, &route, &body).await);
        }
    });
    guard.sync_request(&native_root(route), task);
    let deadline = ceiling.map(|d| (Instant::now() + d, d));
    let mut said = None;
    loop {
        let cancel = guard.kind_now(JobStatus::Sync);
        if said != Some(cancel) {
            hooks.progress(&MediaProgress::Upscaling {
                model: alias.to_string(),
                cancel,
            });
            said = Some(cancel);
        }
        let on = guard.hold().on_attempt(route);
        let root = native_root(&on);
        let alive = async {
            tokio::time::sleep(CANCEL_POLL).await;
            answer::alive(state, &on, &root).await
        };
        let ceiling_reached = async {
            match deadline {
                Some((at, _)) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            sent = &mut answer => {
                return sent.unwrap_or_else(|_| {
                    Err(GatewayError::Upstream {
                        status: 502,
                        provider_type: None,
                        message: format!("{alias}'s upscale request ended without an answer"),
                    })
                });
            }
            () = ceiling_reached => {
                if let Some((_, d)) = deadline {
                    guard.let_go(LetGo::Timeout(d));
                }
                return Err(GatewayError::Timeout);
            }
            r = alive => {
                if let Err(Unanswered) = r {
                    tracing::warn!(
                        "image model '{alias}': its image server {Unanswered} while an upscale \
                         ran; the call gives it up"
                    );
                    guard.let_go(LetGo::Unanswered);
                    return Err(GatewayError::Timeout);
                }
            }
        }
    }
}
