//! The pull job's API path (machine-check design §5): Podman's compat-mode
//! answer ([`super::compat`]), tallied into bytes against the sizes the
//! registry's manifest lists, which are read beside the pull.

use std::time::Duration;

use lmgw_api_types::builds::ImagePullJobDetail;
use serde_json::Value;
use tokio::sync::mpsc;

use super::compat::{self, Event, Step, Tally};
use crate::backends::oci::{Blobs, Platform, PullRef};
use crate::jobs::{JobCtx, JobProgress};
use crate::state::SharedState;

/// How the API path ended.
pub(super) enum Ended {
    /// The answer ended without an error: the log of what came down.
    Pulled(Vec<String>),
    Canceled,
    /// Podman's error, or the answer breaking off.
    Failed(String),
}

/// The manifest's blobs for `image` on this machine's platform.
async fn manifest(state: &SharedState, image: &str) -> Result<Blobs, String> {
    let at = PullRef::parse(image).ok_or_else(|| format!("{image} names no registry manifest"))?;
    state
        .builds
        .registry_client()?
        .platform_blobs(&at, Platform::host())
        .await
}

async fn publish(ctx: &JobCtx, tally: &Tally, detail: &mut ImagePullJobDetail) {
    detail.done_bytes = tally.done_bytes();
    detail.total_bytes = tally.total_bytes();
    detail.size_note = tally.size_note();
    ctx.progress(JobProgress {
        done: detail.done_bytes,
        total: detail.total_bytes,
        stage: detail.last_line.clone(),
        detail: serde_json::to_value(&*detail).unwrap_or(Value::Null),
    })
    .await;
}

/// Read `lines` until the answer ends, publishing the bytes as they move.
/// `cancel_poll` is how often the cancel flag is looked at.
pub(super) async fn run(
    ctx: &JobCtx,
    image: &str,
    old_id: Option<String>,
    mut lines: mpsc::Receiver<Result<String, String>>,
    cancel_poll: Duration,
) -> Ended {
    let mut detail = ImagePullJobDetail {
        image: image.to_string(),
        old_id,
        last_line: format!("pulling {image} through Podman's API"),
        ..Default::default()
    };
    let mut tally = Tally::default();
    let mut log: Vec<String> = Vec::new();
    publish(ctx, &tally, &mut detail).await;
    let sizes = manifest(&ctx.state, image);
    tokio::pin!(sizes);
    let mut sizing = true;
    let mut tick = tokio::time::interval(cancel_poll);
    loop {
        tokio::select! {
            read = &mut sizes, if sizing => {
                sizing = false;
                match read {
                    Ok(blobs) => tally.set_manifest(blobs),
                    Err(why) => {
                        tracing::info!("the size of {image} is not known: {why}");
                        tally.manifest_failed(why);
                    }
                }
                publish(ctx, &tally, &mut detail).await;
            }
            line = lines.recv() => match line {
                Some(Ok(line)) => match compat::parse(&line) {
                    Some(Event::Blob { id, step, words }) => {
                        tally.note(&id, &step);
                        // A blob's changes go in the log, its byte counts
                        // only on the progress.
                        if !matches!(step, Step::Downloading { .. })
                            && log.last() != Some(&words)
                        {
                            log.push(words.clone());
                        }
                        detail.last_line = words;
                        publish(ctx, &tally, &mut detail).await;
                    }
                    Some(Event::Failed(message)) => {
                        log.push(message.clone());
                        return Ended::Failed(message);
                    }
                    Some(Event::Note(words)) => {
                        log.push(words.clone());
                        detail.last_line = words;
                        publish(ctx, &tally, &mut detail).await;
                    }
                    None => {}
                },
                Some(Err(broke)) => return Ended::Failed(broke),
                None => break,
            },
            _ = tick.tick() => {
                if ctx.canceled() {
                    // Dropping `lines` drops the connection, which ends
                    // the pull on Podman's side.
                    return Ended::Canceled;
                }
            }
        }
    }
    log.push(tally.summary());
    detail.last_line = tally.summary();
    publish(ctx, &tally, &mut detail).await;
    Ended::Pulled(log)
}
