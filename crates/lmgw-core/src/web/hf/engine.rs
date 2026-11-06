//! Getting a model also gets what its models run in (machine-check design
//! §5, D16).
//!
//! A model download — `hf_add`, an image recipe, an audio catalog package —
//! asks whether the image its class's models run in is on this machine, and
//! when it is not, starts its pull through the `image_pull` job a start of
//! the model would use (`pull::begin`): keyed by the reference, so the first
//! request, or an Engines-page pull of the same image, joins it instead of
//! downloading it again. Nothing waits for it and nothing is started — the
//! 2026-10-13 ruling holds, pulling an image is not loading a model — so
//! the first message then only loads the model, instead of sitting through
//! a multi-gigabyte download that a client's own timeout may end first.
//!
//! The class's image, not a row's own: the download creates no row yet, and
//! the row a download becomes (the wizard's plan, a recipe's prefilled row,
//! an audio row) runs in the class's image unless its owner names another.
//! An image that is here is left alone; one no registry can give (a
//! `localhost/` build) is said, with why. One podman cannot say anything
//! about is left alone too, as a start leaves it, and said, with why: the
//! question is bounded ([`PRESENCE_BOUND`]), since a podman waiting on its
//! storage lock would otherwise keep the download's answer — its files
//! already queued — from ever arriving.

use std::future::Future;
use std::time::Duration;

use lmgw_api_types::{EngineDownload, EngineFetch};

use crate::backends::pull::{self, Presence};
use crate::state::SharedState;

/// How long `podman image exists` may take here before the image counts as
/// not known: the bound a pull gives Podman's service to answer
/// ([`pull::service::SERVICE_START`]), for the same reason — the question
/// takes no time, and one that takes this long is wedged. Said in the
/// answer's note and the log.
pub(crate) const PRESENCE_BOUND: Duration = pull::service::SERVICE_START;

/// `presence`, or not known once `bound` ran out (module doc). Dropping the
/// question ends its podman (`kill_on_drop`).
pub(crate) async fn within(
    bound: Duration,
    image: &str,
    presence: impl Future<Output = Presence>,
) -> Presence {
    match tokio::time::timeout(bound, presence).await {
        Ok(p) => p,
        Err(_) => Presence::Unknown(format!(
            "podman did not answer within {} s whether {image} is on this machine",
            bound.as_secs()
        )),
    }
}

/// The image `target`'s models run in by default: its class's Image
/// setting; `None` for a target no class has.
fn class_image(state: &SharedState, target: &str) -> Option<String> {
    let snap = state.snapshot();
    let s = &snap.settings;
    let image = match target {
        "chat" => &s.router.image,
        "aux" => &s.aux_router.image,
        "audio" => &s.audio.image,
        "image" => &s.image.image,
        _ => return None,
    };
    Some(image.trim().to_string()).filter(|i| !i.is_empty())
}

/// Start (or join) the pull of `target`'s image when it is not on this
/// machine (module doc): what was done about it, or `None` when the image
/// is here (or `target` has no class).
pub(crate) async fn fetch(state: &SharedState, target: &str) -> Option<EngineDownload> {
    let image = class_image(state, target)?;
    match within(PRESENCE_BOUND, &image, pull::presence(state, &image)).await {
        Presence::Present => return None,
        Presence::Unknown(why) => {
            let note = format!(
                "whether {image} is on this machine is not known ({why}), so it was not \
                 fetched with the model; the model's first start looks again"
            );
            tracing::warn!("a {target} model download: {note}");
            return Some(EngineDownload {
                image,
                state: EngineFetch::NotKnown,
                job_id: None,
                note: Some(note),
            });
        }
        Presence::Missing => {}
    }
    Some(match pull::begin(state, &image).await {
        Ok(job) => {
            tracing::info!(
                "a {target} model download: {image}, which its models run in, is not on this \
                 machine — downloading it too (job {job}); nothing is started"
            );
            EngineDownload {
                image,
                state: EngineFetch::Downloading,
                job_id: Some(job),
                note: None,
            }
        }
        Err(failed) => {
            let note = format!("{image} is not on this machine, and {failed}");
            tracing::warn!("a {target} model download: {note}");
            EngineDownload {
                image,
                state: EngineFetch::Refused,
                job_id: None,
                note: Some(note),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A podman that never answers whether the image is here is not known
    /// once the bound runs out, and the words say how long it was given.
    #[tokio::test(start_paused = true)]
    async fn a_presence_that_never_answers_is_not_known_at_the_bound() {
        let p = within(
            PRESENCE_BOUND,
            "registry.example/engine:cuda",
            std::future::pending(),
        )
        .await;
        let Presence::Unknown(why) = p else {
            panic!("{p:?}");
        };
        assert_eq!(
            why,
            "podman did not answer within 30 s whether registry.example/engine:cuda is on this \
             machine"
        );
        assert_eq!(
            within(PRESENCE_BOUND, "x", async { Presence::Missing }).await,
            Presence::Missing
        );
    }
}
