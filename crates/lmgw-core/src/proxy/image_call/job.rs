//! A local image row's call over sd-server's native job API
//! (chat-media-tools design §4.1, §5.1): submit `POST /sdcpp/v1/img_gen`,
//! poll `GET /sdcpp/v1/jobs/{id}` every [`CANCEL_POLL`] until the job ends,
//! decode its pictures.
//!
//! **A container that dies mid-job** answers the next poll with a dropped
//! connection: the hold's recovery restarts it and the job is submitted
//! again, once — the same rule `send_local` gives a synchronous request
//! (`vram::retry_dead_container`). **A container that restarted under the
//! job** answers it with a 404 or a 410 — sd-server keeps its jobs in
//! memory, and only lmgw submitted this one — and gets the same one
//! resubmit, without a recovery: it answers. A second loss is the error
//! ([`resubmit`]).
//!
//! The poll samples; it bounds nothing. The whole job is bounded by the
//! route's own ceiling, which the caller applies; on a route with none, by
//! the job's own pace ([`pace`](super::pace)): a job that stops advancing
//! is given up, and its guard cancels it as a stop does. Each submit and
//! each poll must be answered within [`answer::STATUS_ANSWER`]: a server
//! that accepts and then says nothing is given up the same way, whether or
//! not the job has a pace.
//!
//! **Progress** (design §5.1): every poll that says something new is
//! reported to the caller's hooks ([`observe`](super::observe)), with what a
//! cancel would do at that moment; the preview mode asked for depends on how
//! closely the caller follows ([`preview_for`]). **Cancel** (§5.4) is the
//! [`JobGuard`]'s: this loop tells it each job it submits and how far it
//! got, and the caller tells it when the call is done with the job.

use base64::Engine as _;
use lmgw_api_types::chat_media::PreviewMode;
use serde_json::Value;

use crate::agent::CANCEL_POLL;
use crate::config::Route;
use crate::egress::apply_bearer_auth;
use crate::error::GatewayError;
use crate::state::SharedState;
use crate::vram::LocalHold;

use super::cancel::{JobGuard, JobStatus, LetGo};
use super::observe::{self, MediaProgress};
use super::pace::within;
use super::{answer, body, native_root};
use super::{Action, Done, EditKind, Follows, Hooks, ImageCall, Picture};

mod resubmit;

/// Run `call` on the local image row `model_id`, through `guard`'s claim.
pub(super) async fn run(
    state: &SharedState,
    (guard, route, model_id): (&JobGuard, &Route, &str),
    call: &ImageCall<'_>,
    hooks: &dyn Hooks,
) -> Result<Done, GatewayError> {
    let hold: &LocalHold = guard.hold();
    let snap = state.snapshot();
    let row = snap
        .image_models
        .iter()
        .find(|m| m.model_id == model_id)
        .ok_or_else(|| GatewayError::UnknownAlias(call.alias.to_string()))?;
    let edit = match &call.action {
        Action::Generate => None,
        Action::Edit { image } if row.edit => Some((image, EditKind::Instruction)),
        // An edit asked of a row that cannot edit is refused, never sent as
        // an img2img under the edit's name: that comes back as the same
        // picture, slightly redrawn.
        Action::Edit { .. } => {
            return Err(GatewayError::Refused {
                status: 422,
                code: "not_an_edit_model",
                message: format!(
                    "{} cannot edit pictures: its image row is not an edit model, it only \
                     draws new pictures",
                    call.alias
                ),
            })
        }
        Action::Vary { image } => Some((image, EditKind::Variation)),
    };
    let seed = call.params.seed.unwrap_or_else(body::draw_seed);
    let preview = preview_for(
        row,
        state.runtime().image_capabilities(model_id).as_ref(),
        hooks.follows(),
    );
    let request = body::native(&call.params, edit, preview, seed);
    let ended = resubmit::once(hold, || async {
        let r = hold.on_attempt(route);
        let root = native_root(&r);
        let id = match answer::within(submit(state, &r, &root, &request)).await {
            Ok(id) => id?,
            Err(silent) => {
                tracing::warn!(
                    "image model '{model_id}': its image server {silent} when a job was \
                     submitted; the call gives it up"
                );
                guard.submit_unanswered(&root);
                return Err(GatewayError::Timeout);
            }
        };
        guard.submitted(&root, &id);
        poll(state, &r, (&root, &id), (guard, call.alias), hooks).await
    })
    .await?;
    // What the server used for what the call left unset: its probed request
    // defaults, read from the container that answered.
    let defaults = probed_defaults(state, model_id);
    let variation = matches!(edit, Some((_, EditKind::Variation)));
    Ok(Done {
        effective: body::effective(&call.params, seed, variation, defaults.as_ref()),
        images: ended.images,
        job_ms: ended.job_ms,
        not_applied: Vec::new(),
        edit: edit.map(|(_, k)| k),
    })
}

/// The preview mode to ask the job for (design §5.2): none for a row whose
/// mode is off; the quick sketch, where the build lists it, for a caller
/// that shows no previews ([`Follows::Steps`]) — sd-server reports steps
/// only beside a preview, and the steps are what the caller's progress and
/// the job's pace ([`pace`](super::pace)) are read from; for one that shows
/// previews, the row's own mode when the build lists it (and, for `tae`,
/// the row has a TAE file), else with no mode set the quick sketch where
/// the build lists it. A mode the build does not list is not sent.
pub(super) fn preview_for(
    row: &crate::config::ImageModel,
    caps: Option<&crate::runtime::image::ImageCapabilities>,
    follows: Follows,
) -> Option<PreviewMode> {
    let listed =
        |m: PreviewMode| caps.is_some_and(|c| c.preview_modes.iter().any(|p| p == m.as_str()));
    let mode = match (follows, row.preview_mode) {
        (_, Some(PreviewMode::None)) => return None,
        (Follows::Steps, _) | (Follows::Previews, None) => PreviewMode::Proj,
        (Follows::Previews, Some(PreviewMode::Tae)) if !row.files.contains_key("taesd") => {
            tracing::debug!(
                "image model '{}': its preview mode is the tiny decoder, but its files name no \
                 taesd file; no preview is asked for",
                row.model_id
            );
            return None;
        }
        (Follows::Previews, Some(m)) => m,
    };
    if !listed(mode) {
        if row.preview_mode.is_some() {
            tracing::debug!(
                "image model '{}': its build does not list the preview mode '{}'; no preview is \
                 asked for",
                row.model_id,
                mode.as_str()
            );
        }
        return None;
    }
    Some(mode)
}

/// The row's probed `defaults_by_mode.img_gen`, when its container reported
/// capabilities.
fn probed_defaults(state: &SharedState, model_id: &str) -> Option<serde_json::Map<String, Value>> {
    state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.class == crate::runtime::Class::Image && v.model_id == model_id)
        .and_then(|v| v.image_capabilities)
        .and_then(|c| c.defaults_by_mode.get("img_gen").cloned())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
}

/// What a job ended with.
struct Ended {
    images: Vec<Picture>,
    job_ms: Option<u64>,
}

/// One submit-and-follow: the job's end, or the server no longer knowing it
/// (the status it answered: 404 or 410) — a container that restarted.
enum Followed {
    Ended(Ended),
    Gone(u16),
}

async fn submit(
    state: &SharedState,
    route: &Route,
    root: &str,
    request: &Value,
) -> Result<String, GatewayError> {
    let rb = apply_bearer_auth(
        state
            .http
            .post(format!("{root}/sdcpp/v1/img_gen"))
            .json(request),
        &route.upstream,
    );
    let resp = rb.send().await.map_err(GatewayError::from)?;
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap_or_default();
    if status == 429 {
        let queue = probed_queue(state, route);
        return Err(GatewayError::Refused {
            status: 429,
            code: "image_queue_full",
            message: format!(
                "the image server's queue is full{}: try again once its pictures are done",
                queue
                    .map(|q| format!(" ({q} jobs, its limits.max_queue_size)"))
                    .unwrap_or_default()
            ),
        });
    }
    if !(200..300).contains(&status) {
        return Err(super::super::image::image_error(
            route.upstream.protocol,
            status,
            &bytes,
        ));
    }
    let v: Value = serde_json::from_slice(&bytes).unwrap_or_default();
    v.get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| GatewayError::Upstream {
            status: 502,
            provider_type: None,
            message: "the image server accepted the job but named no job id".into(),
        })
}

/// The job queue's depth the row's build states, for a `429`'s message.
fn probed_queue(state: &SharedState, route: &Route) -> Option<i64> {
    let target = crate::vram::classify(route)?;
    state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.class == target.class && v.model_id == target.model_id)
        .and_then(|v| v.image_capabilities)
        .and_then(|c| c.limits.max_queue_size)
}

async fn poll(
    state: &SharedState,
    route: &Route,
    (root, id): (&str, &str),
    (guard, model): (&JobGuard, &str),
    hooks: &dyn Hooks,
) -> Result<Followed, GatewayError> {
    let url = format!("{root}/sdcpp/v1/jobs/{id}");
    let mut last: Option<MediaProgress> = None;
    loop {
        let rb = apply_bearer_auth(state.http.get(&url), &route.upstream);
        let read = async {
            let resp = rb.send().await.map_err(GatewayError::from)?;
            let status = resp.status().as_u16();
            let bytes = resp.bytes().await.map_err(GatewayError::from)?;
            Ok::<_, GatewayError>((status, bytes))
        };
        // A poll that does not answer counts toward a stall like one that
        // reads no progress, and is given up after `STATUS_ANSWER` whatever
        // the pace.
        let read = answer::within(read);
        let read = match guard.own_pace() {
            Some(pace) => within(&pace, read).await,
            None => Ok(read.await),
        };
        let (status, bytes) = match read {
            Ok(Ok(r)) => r?,
            Ok(Err(silent)) => {
                tracing::warn!(
                    "image model '{model}': its image server {silent} for job {id}; the call \
                     gives it up"
                );
                guard.let_go(LetGo::Unanswered);
                return Err(GatewayError::Timeout);
            }
            Err(stall) => {
                tracing::warn!(
                    "image model '{}': job {id} {stall}, and its route sets no timeout; the \
                     call gives it up",
                    model
                );
                guard.let_go(LetGo::Stalled(stall));
                return Err(GatewayError::Timeout);
            }
        };
        if status == 404 || status == 410 {
            return Ok(Followed::Gone(status));
        }
        if !(200..300).contains(&status) {
            return Err(super::super::image::image_error(
                route.upstream.protocol,
                status,
                &bytes,
            ));
        }
        let job: Value = serde_json::from_slice(&bytes).unwrap_or_default();
        match job.get("status").and_then(Value::as_str).unwrap_or("") {
            "completed" => return completed(&job).map(Followed::Ended),
            "failed" => return Err(failed(&job)),
            "cancelled" => {
                return Err(super::super::canceled(
                    "the job was cancelled on the image server",
                ))
            }
            other => {
                let status = match other {
                    "queued" => JobStatus::Queued,
                    _ => JobStatus::Generating,
                };
                guard.read(status, &job);
                let cancel = guard.kind_now(status);
                if let Some(p) = observe::of_job(other, &job, model, cancel)
                    .filter(|p| p.is_news_after(last.as_ref()))
                {
                    hooks.progress(&p);
                    last = Some(p);
                }
            }
        }
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

fn completed(job: &Value) -> Result<Ended, GatewayError> {
    let mime = match job
        .pointer("/result/output_format")
        .and_then(Value::as_str)
        .unwrap_or("png")
    {
        "jpeg" | "jpg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    };
    let mut images = Vec::new();
    for img in job
        .pointer("/result/images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(b64) = img.get("b64_json").and_then(Value::as_str) else {
            continue;
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| GatewayError::Upstream {
                status: 502,
                provider_type: None,
                message: format!("the image server's picture is not base64: {e}"),
            })?;
        images.push(Picture {
            mime: mime.to_string(),
            bytes,
        });
    }
    if images.is_empty() {
        return Err(GatewayError::Upstream {
            status: 502,
            provider_type: None,
            message: "the image server finished the job with no picture".into(),
        });
    }
    let at = |k: &str| job.get(k).and_then(Value::as_u64);
    let job_ms = at("completed")
        .zip(at("started"))
        .map(|(c, s)| c.saturating_sub(s));
    Ok(Ended { images, job_ms })
}

fn failed(job: &Value) -> GatewayError {
    let err = job.get("error");
    let code = err
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("the job failed without a message");
    GatewayError::Upstream {
        status: 502,
        provider_type: code,
        message: format!("the image server could not make the picture: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(mode: Option<&str>, files: Value) -> crate::config::ImageModel {
        serde_json::from_value(json!({
            "id": 1, "model_id": "z", "files": files, "enabled": true, "image": null,
            "extra_run_args": null, "warm_start": false, "idle_seconds": 300,
            "preview_mode": mode,
        }))
        .unwrap()
    }

    fn caps(modes: &[&str]) -> crate::runtime::image::ImageCapabilities {
        crate::runtime::image::ImageCapabilities {
            preview_modes: modes.iter().map(|m| m.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn the_preview_asked_for_follows_the_caller_the_row_and_the_build() {
        let files = json!({"diffusion_model": "z.gguf"});
        let all = caps(&["none", "proj", "tae", "vae"]);
        let unset = row(None, files.clone());
        assert_eq!(
            preview_for(&unset, Some(&all), Follows::Steps),
            Some(PreviewMode::Proj)
        );
        assert_eq!(
            preview_for(&unset, Some(&all), Follows::Previews),
            Some(PreviewMode::Proj)
        );
        // The row's own mode for previews; the cheapest for steps alone.
        let vae = row(Some("vae"), files.clone());
        assert_eq!(
            preview_for(&vae, Some(&all), Follows::Previews),
            Some(PreviewMode::Vae)
        );
        assert_eq!(
            preview_for(&vae, Some(&all), Follows::Steps),
            Some(PreviewMode::Proj)
        );
        // Off is off, for every caller.
        let off = row(Some("none"), files.clone());
        assert_eq!(preview_for(&off, Some(&all), Follows::Steps), None);
        // The tiny decoder needs its file.
        let tae = row(Some("tae"), files);
        assert_eq!(preview_for(&tae, Some(&all), Follows::Previews), None);
        let tae = row(
            Some("tae"),
            json!({"diffusion_model": "z.gguf", "taesd": "t.safetensors"}),
        );
        assert_eq!(
            preview_for(&tae, Some(&all), Follows::Previews),
            Some(PreviewMode::Tae)
        );
        // A build that lists no previews (c678dfe), or none probed yet.
        assert_eq!(
            preview_for(&unset, Some(&caps(&[])), Follows::Previews),
            None
        );
        assert_eq!(preview_for(&unset, None, Follows::Steps), None);
    }
}
