//! What a running media call reports to its caller's hooks
//! (chat-media-tools design §5.1, §5.3): the stages of bringing its model
//! in, the image server's queue, and the steps and previews of the job.
//!
//! The job is polled at `agent::CANCEL_POLL`; a poll that says nothing new
//! reports nothing, and a preview is passed on only when its `(pass, step)`
//! changed since the last one — the image server keeps the latest frame
//! only, so a poll between two steps reads the same one again.

use lmgw_api_types::chat_media::{CancelKind, MediaPhase};
use serde_json::Value;

use crate::state::SharedState;
use crate::vram::stage_watch::WaitPhase;
use crate::vram::Target;

use super::Hooks;

/// Where a job's steps stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Steps {
    /// A new pass (a batch item, a high- or low-noise stage, a hires pass)
    /// starts its steps again.
    pub pass: u32,
    pub step: u32,
    pub total: u32,
}

/// One report of a running call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MediaProgress {
    /// Before admission: `model`'s engine image is being downloaded
    /// (machine-check design §5), `done` of `total` bytes (`None` while the
    /// size is not known).
    Downloading {
        model: String,
        image: String,
        done: u64,
        total: Option<u64>,
        /// Why `total` is not known, when the pull said.
        size_note: Option<String>,
    },
    /// Before the job: bringing `model` in (`loading`), another model going
    /// to make room (`unloading`, `model` the one going), waiting for room
    /// (`room`: needs, free) — or an audio call at work.
    ///
    /// An upscale at work is [`Self::Upscaling`], which can be cancelled.
    Stage {
        phase: MediaPhase,
        model: String,
        room: Option<(u64, u64)>,
    },
    /// The image server holds the job at `position` of its queue.
    Queued {
        model: String,
        position: u32,
        cancel: CancelKind,
    },
    /// An upscale on `model`'s image server: a synchronous request with no
    /// job, which a cancel stops by the rule for one (design §5.4).
    Upscaling { model: String, cancel: CancelKind },
    /// Drawing: its steps where the job reports them (a build or a mode
    /// without previews does not), and the latest preview, base64 PNG as
    /// the image server drew it.
    Generating {
        model: String,
        steps: Option<Steps>,
        preview: Option<String>,
        cancel: CancelKind,
    },
}

impl MediaProgress {
    /// Whether this says something `last` did not. A preview alone is no
    /// news: it changes with its step (module doc).
    pub fn is_news_after(&self, last: Option<&MediaProgress>) -> bool {
        let Some(last) = last else {
            return true;
        };
        match (self, last) {
            (
                Self::Generating {
                    model,
                    steps,
                    cancel,
                    ..
                },
                Self::Generating {
                    model: m,
                    steps: s,
                    cancel: c,
                    ..
                },
            ) => (model, steps, cancel) != (m, s, c),
            _ => self != last,
        }
    }

    /// What a cancel would do now; `None` before a job exists.
    pub fn cancel(&self) -> Option<CancelKind> {
        match self {
            Self::Stage { .. } | Self::Downloading { .. } => None,
            Self::Queued { cancel, .. }
            | Self::Generating { cancel, .. }
            | Self::Upscaling { cancel, .. } => Some(*cancel),
        }
    }
}

/// The report of the admission wait's `phase` for the call's own `model`.
/// Waiting for the gate and measuring are part of bringing the model in;
/// an eviction names the model it stops.
pub(crate) fn of_wait(phase: &WaitPhase, model: &str) -> MediaProgress {
    let stage = |phase, model: &str| MediaProgress::Stage {
        phase,
        model: model.to_string(),
        room: None,
    };
    match phase {
        WaitPhase::Downloading {
            image,
            done_bytes,
            total_bytes,
            size_note,
        } => MediaProgress::Downloading {
            model: model.to_string(),
            image: image.clone(),
            done: *done_bytes,
            total: *total_bytes,
            size_note: size_note.clone(),
        },
        WaitPhase::Gate | WaitPhase::Measuring | WaitPhase::Starting => {
            stage(MediaPhase::Loading, model)
        }
        WaitPhase::Evicting { victim } => stage(MediaPhase::Unloading, victim),
        WaitPhase::WaitingForRoom {
            needs_bytes,
            free_bytes,
        } => MediaProgress::Stage {
            phase: MediaPhase::WaitingForRoom,
            model: model.to_string(),
            room: Some((*needs_bytes, *free_bytes)),
        },
    }
}

/// Admission (`admit`) of a call for `alias` on `target`, its stages passed
/// to `hooks` as they happen (design §5.3): `loading` first when the row
/// has no ready container, then the phases of its own wait for room — the
/// model an eviction stops, the wait, the start.
pub(super) async fn admit_followed<F: std::future::Future>(
    state: &SharedState,
    target: Option<Target>,
    alias: &str,
    hooks: &dyn Hooks,
    admit: F,
) -> F::Output {
    if let Some(t) = target {
        if state.runtime().ready_port(t.class, &t.model_id).is_none() {
            hooks.progress(&MediaProgress::Stage {
                phase: MediaPhase::Loading,
                model: alias.to_string(),
                room: None,
            });
        }
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let admit = crate::vram::stage_watch::follow(tx, admit);
    tokio::pin!(admit);
    loop {
        tokio::select! {
            biased;
            opened = &mut admit => return opened,
            Some(phase) = rx.recv() => hooks.progress(&of_wait(&phase, alias)),
        }
    }
}

/// A job poll's report, for `status` `queued` or `generating`; `cancel`
/// what a cancel would do now.
pub(crate) fn of_job(
    status: &str,
    job: &Value,
    model: &str,
    cancel: CancelKind,
) -> Option<MediaProgress> {
    let n = |v: Option<&Value>| v.and_then(Value::as_u64).map(|n| n as u32);
    match status {
        "queued" => Some(MediaProgress::Queued {
            model: model.to_string(),
            position: n(job.get("queue_position")).unwrap_or(0),
            cancel,
        }),
        "generating" => {
            let p = job.get("preview").filter(|p| p.is_object());
            let steps = p.and_then(|p| {
                Some(Steps {
                    pass: n(p.get("pass"))?,
                    step: n(p.get("step"))?,
                    total: n(p.get("total_steps"))?,
                })
            });
            let preview = p
                .and_then(|p| p.get("b64_json"))
                .and_then(Value::as_str)
                .filter(|b| !b.is_empty())
                .map(str::to_string);
            Some(MediaProgress::Generating {
                model: model.to_string(),
                steps,
                preview,
                cancel,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn generating(pass: u32, step: u32, b64: &str) -> MediaProgress {
        let job = json!({"status": "generating",
                         "preview": {"pass": pass, "step": step, "total_steps": 8, "b64_json": b64}});
        of_job("generating", &job, "image/z", CancelKind::Restart).unwrap()
    }

    #[test]
    fn a_preview_is_news_only_with_a_new_step_or_pass() {
        let a = generating(1, 3, "AAA");
        assert!(a.is_news_after(None));
        assert!(!generating(1, 3, "BBB").is_news_after(Some(&a)));
        assert!(generating(1, 4, "BBB").is_news_after(Some(&a)));
        // Pass 2 starts its steps again: step 3 of pass 2 is news.
        assert!(generating(2, 3, "AAA").is_news_after(Some(&a)));
        let MediaProgress::Generating { preview, steps, .. } = generating(2, 1, "") else {
            panic!()
        };
        assert_eq!(preview, None, "an empty frame is no preview");
        assert_eq!(steps.map(|s| (s.pass, s.step, s.total)), Some((2, 1, 8)));
    }

    #[test]
    fn a_job_without_a_preview_reports_no_steps() {
        let p = of_job(
            "generating",
            &json!({"status": "generating"}),
            "image/z",
            CancelKind::Native,
        );
        assert_eq!(
            p,
            Some(MediaProgress::Generating {
                model: "image/z".into(),
                steps: None,
                preview: None,
                cancel: CancelKind::Native,
            })
        );
    }

    #[test]
    fn an_eviction_names_its_victim() {
        let p = of_wait(
            &WaitPhase::Evicting {
                victim: "chat/qwen".into(),
            },
            "image/z",
        );
        assert_eq!(
            p,
            MediaProgress::Stage {
                phase: MediaPhase::Unloading,
                model: "chat/qwen".into(),
                room: None,
            }
        );
    }
}
