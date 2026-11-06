//! The download before a model's first start (machine-check design §5,
//! D15).
//!
//! A model whose engine image is not on this machine pulls it **before
//! admission**: after the route is resolved and the row's runtime is known,
//! before the gate, the evictions and the reservation. Nothing is evicted
//! and nothing is reserved for a model whose engine is still downloading,
//! and the queue's time budget (`started`, `queue_timeout_seconds`) starts
//! after the download, so it measures only the wait for the card. The start
//! itself then runs with `--pull=never` (`registry/acquire.rs`), so the
//! bytes are never fetched inside the gate.
//!
//! The wait is a waiter entry of its own ([`Live::downloads`]): it holds no
//! place in the admission queue, the vram view lists it under
//! `downloading`, and a caller that follows its wait
//! ([`stage_watch::follow`]) hears [`WaitPhase::Downloading`] first, with
//! the bytes as they move. The pull is the `image_pull` job
//! ([`pull::pull_and_wait`]), so two starts of one image, or a start and an
//! Engines-page pull, share one download; a caller that hangs up stops
//! waiting, not the download.
//!
//! **While it waits** the download is no shelter from the rules every other
//! wait keeps: at the queue's poll interval it asks whether the GPU hold or
//! a benchmark's lease came on ([`Engine::Blocked`]) and — for a caller that
//! has a fallback — whether the room it needs has become VRAM outside lmgw's
//! control ([`Engine::External`]). Either ends the wait, never the download.
//! A wait that ran to its end is [`Engine::Downloaded`]: it can take
//! minutes, so the caller reads the snapshot and the row again before it
//! starts anything. A caller that follows its wait hears
//! [`WaitPhase::Starting`] then, whatever comes after it, so its line moves
//! on from the download even when admission says no phase of its own.
//!
//! **A guest never waits for it** ([`VramScheduler::engine_for_guest`]):
//! background traffic does not wait in ways that disturb others, so a
//! guest's start begins the download (or joins it) and is blocked at once,
//! and the gate's usual background answer — the fallback, or
//! `gpu_deferred` — answers. A later guest starts the model once the image
//! is here.
//!
//! [`Live::downloads`]: super::scheduler::Live

use std::time::Instant;

use crate::backends::pull::{self, Bytes, Presence, PullFailed};
use crate::bench::lease::GpuBlock;
use crate::config::Snapshot;
use crate::error::GatewayError;
use crate::runtime::descriptor::ModelRuntime;
use crate::runtime::registry::Origin;
use crate::runtime::Class;
use crate::state::SharedState;

use super::scheduler::{Waiter, POLL};
use super::stage_watch::{self, WaitPhase};
use super::{broadcast, ExternalFallback, ExternalShortfall, Target, VramScheduler};

/// What [`VramScheduler::engine_download`] came to, short of a failed
/// download (which is an `Err`).
pub(super) enum Engine {
    /// The image is on this machine, or podman cannot say: nothing waited.
    Here,
    /// It was downloaded just now. The snapshot and the row the caller read
    /// are from before the wait, which can take minutes: the owner may have
    /// edited or removed the row, or switched the hold on, meanwhile — the
    /// caller reads both again before it starts anything.
    Downloaded,
    /// The GPU hold or a benchmark's lease came on while it downloaded: the
    /// start is refused. The download goes on.
    Blocked(GpuBlock),
    /// While it downloaded, the room this model needs became VRAM outside
    /// lmgw's control, and the caller's fallback answers (candidate-aliases
    /// design §4.7). The download goes on.
    External(ExternalShortfall),
}

/// A download entry, removed (and the dashboard told) on drop — however the
/// wait ends, a caller hanging up included, as [`super::scheduler::Queued`].
struct Downloading {
    state: SharedState,
    id: u64,
}

impl Drop for Downloading {
    fn drop(&mut self) {
        self.state
            .vram
            .live
            .lock()
            .unwrap()
            .downloads
            .retain(|w| w.id != self.id);
        broadcast(&self.state);
    }
}

/// A download's bytes so far, in words: " (1.2 of 3.4 GiB)", " (5.0
/// MiB, size not known)", or nothing before the first byte of a size not known.
fn so_far(done: u64, total: Option<u64>) -> String {
    use lmgw_api_types::units::{fmt_bytes_of, fmt_bytes_so_far};
    match total {
        Some(t) if t > 0 => format!(" ({})", fmt_bytes_of(done, t)),
        _ if done > 0 => format!(" ({}, size not known)", fmt_bytes_so_far(done)),
        _ => String::new(),
    }
}

/// The words the view shows for a download's phase.
pub(super) fn words(image: &str, done: u64, total: Option<u64>) -> String {
    format!("downloading {image}{}", so_far(done, total))
}

/// The setting a start's engine comes from: the class's Image setting, or
/// the model's own.
fn image_source(snap: &Snapshot, class: Class, image: &str) -> String {
    let class_image = match class {
        Class::Chat => &snap.settings.router.image,
        Class::Aux => &snap.settings.aux_router.image,
        Class::Audio => &snap.settings.audio.image,
        Class::Image => &snap.settings.image.image,
    };
    if class_image.trim() == image {
        format!(
            "The engine is chosen in {}.",
            lmgw_api_types::settings_labels::image_at(class.as_str())
        )
    } else {
        "It is the model's own engine, chosen on the model.".to_string()
    }
}

/// A start whose engine could not be downloaded: a plain 502 naming the
/// model, why (the job's or Podman's own words after the colon), the
/// image's reference and the setting the engine is chosen in — code
/// `engine_download`, which a configured fallback answers (WP5, D69).
fn could_not_start(
    state: &SharedState,
    target: &Target,
    image: &str,
    failed: PullFailed,
) -> GatewayError {
    let snap = state.snapshot();
    GatewayError::EngineDownload {
        message: format!(
            "The engine {} model '{}' runs in could not be downloaded, so the model could not \
             start: {failed} (engine {image}). {}",
            target.class.as_str(),
            target.model_id,
            image_source(&snap, target.class, image)
        ),
    }
}

impl VramScheduler {
    /// Make sure `runtime`'s engine image is on this machine before
    /// `target` is admitted (module doc): [`Engine::Here`] at once when it
    /// is there (or podman cannot say — the start then goes ahead as it
    /// would have), else the download's end, or what ended the wait for it
    /// first. `fallback`: the caller's, when it has one still usable; the
    /// outside-VRAM verdict is taken again while it is ([`Self::recheck`]).
    /// Answers a failed download with a plain error that names the image
    /// and the setting it comes from.
    pub(super) async fn engine_download(
        &self,
        state: &SharedState,
        target: &Target,
        runtime: &ModelRuntime,
        alias: &str,
        fallback: &mut Option<&mut ExternalFallback<'_>>,
    ) -> Result<Engine, GatewayError> {
        let image = runtime.image.trim();
        if image.is_empty() {
            return Ok(Engine::Here);
        }
        match pull::presence(state, image).await {
            Presence::Present => return Ok(Engine::Here),
            Presence::Unknown(why) => {
                tracing::debug!(
                    "whether {image} is on this machine is not known ({why}); starting {}/{} \
                     as it is",
                    target.class.as_str(),
                    target.model_id
                );
                return Ok(Engine::Here);
            }
            Presence::Missing => {}
        }
        let id = self.next_id();
        let first = WaitPhase::Downloading {
            image: image.to_string(),
            done_bytes: 0,
            total_bytes: None,
            size_note: None,
        };
        let watch = stage_watch::current();
        if let Some(w) = &watch {
            w.send(&first);
        }
        self.live.lock().unwrap().downloads.push(Waiter {
            id,
            alias: alias.to_string(),
            model: target.model_id.clone(),
            class: target.class,
            needs: 0,
            since: Instant::now(),
            stage: words(image, 0, None),
            phase: Some(first),
            watch,
            origin: Origin::Owner,
        });
        // Dropped however this returns: the entry leaves the view.
        let _entry = Downloading {
            state: state.clone(),
            id,
        };
        broadcast(state);
        tracing::info!(
            "{}/{} needs {image}, which is not on this machine: downloading it before admission",
            target.class.as_str(),
            target.model_id
        );
        let mut told = Instant::now();
        let pulled = pull::pull_and_wait(state, image, |b| {
            self.set_download(id, image, b);
            if told.elapsed() >= crate::jobs::PUBLISH_INTERVAL {
                told = Instant::now();
                broadcast(state);
            }
        });
        tokio::pin!(pulled);
        // By the placement of the descriptor this start renders, as the
        // admission's own checks: a row on the CPU is not held.
        let placement = runtime.placement();
        let ended = loop {
            tokio::select! {
                ended = &mut pulled => break ended,
                () = tokio::time::sleep(POLL) => {}
            }
            if let Some(block) = state.snapshot().gpu_block_at(placement) {
                tracing::info!(
                    "{}/{}: {} came on while {image} downloaded — not started; the download \
                     goes on",
                    target.class.as_str(),
                    target.model_id,
                    block.who()
                );
                return Ok(Engine::Blocked(block));
            }
            if let Some(short) = self.recheck(state, target, alias, fallback).await {
                return Ok(Engine::External(short));
            }
        };
        match ended {
            // Asked once more: a caller that reads its row again looks
            // again, and an image the pull stored under a spelling
            // `podman image exists` does not resolve would be downloaded
            // again on every look. Said instead, once.
            Ok(()) => match pull::presence(state, image).await {
                Presence::Missing => Err(could_not_start(
                    state,
                    target,
                    image,
                    PullFailed::Ended(
                        "ended, but podman still does not find it under that name".into(),
                    ),
                )),
                Presence::Present | Presence::Unknown(_) => {
                    // The caller's next phase, said now: what follows may
                    // say none (admission switched off, nothing to measure,
                    // a CPU start, a model another request brought up), and
                    // its line would sit at a full bar through the load.
                    if let Some(w) = stage_watch::current() {
                        w.send(&WaitPhase::Starting);
                    }
                    Ok(Engine::Downloaded)
                }
            },
            Err(failed) => Err(could_not_start(state, target, image, failed)),
        }
    }

    /// The engine image for a guest's start ([`Origin::Background`]), which
    /// never waits for a download (module doc): when `runtime`'s image is
    /// not on this machine, start its download — or join the one under way
    /// — and say so at once. `Ok(None)` when it is here, or podman cannot
    /// say. The words read after "GPU in use by ", like every
    /// [`super::BackgroundStart::Blocked`]. A download that cannot even
    /// begin (a `localhost/` build) is the owner's plain 502.
    pub(super) async fn engine_for_guest(
        &self,
        state: &SharedState,
        target: &Target,
        runtime: &ModelRuntime,
    ) -> Result<Option<String>, GatewayError> {
        let image = runtime.image.trim();
        if image.is_empty() || pull::presence(state, image).await != Presence::Missing {
            return Ok(None);
        }
        let job = pull::begin(state, image)
            .await
            .map_err(|failed| could_not_start(state, target, image, failed))?;
        let Bytes { done, total, .. } = pull::job_bytes(state, job).unwrap_or_default();
        tracing::info!(
            "{}/{} needs {image}, which is not on this machine: downloading it (job {job}); \
             background traffic does not wait for the download",
            target.class.as_str(),
            target.model_id
        );
        Ok(Some(format!(
            "nothing yet — its engine image {image} is still downloading{}, and background \
             traffic does not wait for a download",
            so_far(done, total)
        )))
    }

    /// Download entry `id` moved to `b`.
    fn set_download(&self, id: u64, image: &str, b: Bytes) {
        let mut live = self.live.lock().unwrap();
        let Some(w) = live.downloads.iter_mut().find(|w| w.id == id) else {
            return;
        };
        let phase = WaitPhase::Downloading {
            image: image.to_string(),
            done_bytes: b.done,
            total_bytes: b.total,
            size_note: b.size_note,
        };
        w.stage = words(image, b.done, b.total);
        if let Some(watch) = &w.watch {
            watch.send(&phase);
        }
        w.phase = Some(phase);
    }

    /// The download entries, oldest first, for the view: position 0 (no
    /// place in the queue), needs 0 (the footprint is read at admission).
    pub(super) fn downloads_view(&self) -> Vec<super::WaiterView> {
        self.live
            .lock()
            .unwrap()
            .downloads
            .iter()
            .map(|w| super::WaiterView {
                position: 0,
                alias: w.alias.clone(),
                model: w.model.clone(),
                container: w.class,
                needs_bytes: 0,
                waiting_ms: w.since.elapsed().as_millis() as u64,
                stage: w.stage.clone(),
                phase: w.phase.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_downloads_words_say_its_bytes_where_known() {
        assert_eq!(words("ghcr.io/o/r:t", 0, None), "downloading ghcr.io/o/r:t");
        assert!(words("x", 1_200_000_000, Some(3_400_000_000)).contains(" of "));
        assert!(words("x", 5, None).contains("size not known"));
    }
}
