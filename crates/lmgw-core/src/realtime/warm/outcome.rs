//! What a warm came to, and how it is said (chat-voice design §4.3).
//!
//! Each stage a warm takes on ends in one [`WarmOutcome`] — with one
//! exception: a group that does not fit together says `skipped:
//! does_not_fit` for each of its GPU stages first and then warms them in
//! Background, so each of those stages is said twice, `does_not_fit` and
//! then its Background outcome (`skipped: full`, or `loading` → `ready`, or
//! any other). A reader takes a stage's last frame as its state, and only
//! the stream's end (`done`) as the end of the warm. Every surface
//! shows it in one shape, the [`ModelState`] frame — the `state` event of
//! the Chat's SSE streams (`voice/warm`, and a chat turn's own admission),
//! and later `lmgw.model.state` in a bound realtime session:
//!
//! ```json
//! {"stage": "asr", "alias": "audio/parakeet", "state": "loading", "ms": null}
//! ```
//!
//! | `state` | carries |
//! |---|---|
//! | `loading` | — (sent before an admission or a load that will take time) |
//! | `ready` | `ms`: the load time; `null` when nothing had to load — the model was up already, or it is served off this machine |
//! | `held` | `cause`: `gpu_hold` or `benchmark`, and `message` |
//! | `fallback` | `answered_by`: the alias the GPU hold or admission answers with; nothing is started for it |
//! | `skipped` | `reason`: `full` (a Background warm that would have to evict), `does_not_fit` (a group, with `needed_bytes` and `capacity_bytes` — not a stage's last frame when the group check said it, see above), `cannot_speak`; and `message` |
//! | `failed` | `message` |
//!
//! A warm the caller abandoned ([`WarmOutcome::Aborted`]: the press ended
//! before admission, so the wait was dropped; [`WarmOutcome::UpNotLoaded`]:
//! it ended while the start was in flight) is said to nobody — the reader
//! is gone; it is one log line.
//!
//! The frames travel through a [`Reporter`]: the caller's channel, or none.
//! Its reader going away is also what tells a warm that the press ended
//! ([`Reporter::gone`]).

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::bench::lease::GpuBlock;
use crate::error::GatewayError;
use crate::proxy::StopSignal;

/// Why a stage is held: the owner's GPU hold, or a benchmark run's lease —
/// read from the `gpu_block` that refused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HeldCause {
    GpuHold,
    Benchmark,
}

impl HeldCause {
    pub(crate) fn of(block: &GpuBlock) -> Self {
        match block {
            GpuBlock::Hold => Self::GpuHold,
            GpuBlock::Benchmark(_) => Self::Benchmark,
        }
    }
}

/// Why a stage was not warmed although it could serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SkipReason {
    /// A Background warm that would have had to evict (it never does).
    Full,
    /// A group whose models do not fit on the card together: warmed one by
    /// one without evicting, so they cannot evict each other. Also a stage
    /// of a group that fit at the check but cannot get room beside the
    /// claims its siblings keep (chat-voice design §4.2): its wait is given
    /// up — its last frame.
    DoesNotFit,
    /// A TTS row that can never speak these answers
    /// (`proxy::synthesize::refuse_route`).
    CannotSpeak,
}

/// The sizes a `does_not_fit` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GroupSizes {
    /// The group's models together, plus `vram.headroom_mb`.
    pub needed_bytes: u64,
    /// What they could have together: at the group check, what lmgw may use
    /// (`vram.budget_mb`, or the devices' total) less what is held outside
    /// lmgw on the card when that can be told; for a stage crowded out
    /// later, the free memory plus every model it could evict plus what its
    /// kept siblings hold.
    pub capacity_bytes: u64,
}

/// What warming one stage came to (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WarmOutcome {
    /// Up, and loaded when it is a lazy audio row. `ms`: how long it took
    /// from its `loading` frame; `None` when nothing had to load.
    Ready {
        ms: Option<u64>,
    },
    /// The GPU hold or a benchmark's lease refuses it, and no usable
    /// fallback stands in.
    Held {
        cause: HeldCause,
        message: String,
    },
    /// A fallback answers instead; a warm never starts a fallback's model.
    Fallback {
        answered_by: String,
    },
    Skipped {
        reason: SkipReason,
        message: String,
        sizes: Option<GroupSizes>,
    },
    Failed {
        message: String,
    },
    /// The caller went away before admission: the wait was dropped.
    Aborted,
    /// The caller went away while its start was in flight: the start
    /// finished, and the lazy audio row it would have loaded was not.
    /// `ms`: how long it took to come up.
    UpNotLoaded {
        ms: Option<u64>,
    },
}

impl WarmOutcome {
    /// An admission's (or a start's) refusal: `held` for the GPU hold and a
    /// benchmark's lease, `failed` for anything else.
    pub(crate) fn refused(e: &GatewayError) -> Self {
        match e {
            GatewayError::GpuHold { .. } => Self::Held {
                cause: HeldCause::GpuHold,
                message: e.to_string(),
            },
            GatewayError::GpuBenchmark { .. } => Self::Held {
                cause: HeldCause::Benchmark,
                message: e.to_string(),
            },
            _ => Self::Failed {
                message: e.to_string(),
            },
        }
    }

    pub(crate) fn failed(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
        }
    }

    /// One sentence for the log line.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Ready { ms: Some(ms) } => format!("ready after {ms} ms"),
            Self::Ready { ms: None } => "ready, nothing had to load".into(),
            Self::Held { message, .. } => format!("held: {message}"),
            Self::Fallback { answered_by } => {
                format!("its fallback '{answered_by}' answers; nothing is started for it")
            }
            Self::Skipped { message, .. } => format!("skipped: {message}"),
            Self::Failed { message } => format!("failed: {message}"),
            Self::Aborted => "dropped: the caller went away before admission".into(),
            Self::UpNotLoaded { ms } => format!(
                "up{}; not loaded, the caller went away before its load",
                ms.map(|ms| format!(" after {ms} ms")).unwrap_or_default()
            ),
        }
    }
}

/// One `state` frame (module doc).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ModelState {
    pub stage: &'static str,
    pub alias: String,
    pub state: &'static str,
    pub ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<HeldCause>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<SkipReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needed_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity_bytes: Option<u64>,
}

impl ModelState {
    fn bare(stage: &'static str, alias: &str, state: &'static str) -> Self {
        Self {
            stage,
            alias: alias.to_string(),
            state,
            ms: None,
            cause: None,
            answered_by: None,
            reason: None,
            message: None,
            needed_bytes: None,
            capacity_bytes: None,
        }
    }

    /// `loading`: an admission or a load that takes time is under way.
    pub(crate) fn loading(stage: &'static str, alias: &str) -> Self {
        Self::bare(stage, alias, "loading")
    }

    /// The frame `outcome` is said as; `None` for [`WarmOutcome::Aborted`]
    /// and [`WarmOutcome::UpNotLoaded`], which have no reader.
    pub(crate) fn of(stage: &'static str, alias: &str, outcome: &WarmOutcome) -> Option<Self> {
        Some(match outcome {
            WarmOutcome::Ready { ms } => Self {
                ms: *ms,
                ..Self::bare(stage, alias, "ready")
            },
            WarmOutcome::Held { cause, message } => Self {
                cause: Some(*cause),
                message: Some(message.clone()),
                ..Self::bare(stage, alias, "held")
            },
            WarmOutcome::Fallback { answered_by } => Self {
                answered_by: Some(answered_by.clone()),
                ..Self::bare(stage, alias, "fallback")
            },
            WarmOutcome::Skipped {
                reason,
                message,
                sizes,
            } => Self {
                reason: Some(*reason),
                message: Some(message.clone()),
                ..Self::bare(stage, alias, "skipped")
            }
            .with_sizes(*sizes),
            WarmOutcome::Failed { message } => Self {
                message: Some(message.clone()),
                ..Self::bare(stage, alias, "failed")
            },
            WarmOutcome::Aborted | WarmOutcome::UpNotLoaded { .. } => return None,
        })
    }

    fn with_sizes(mut self, sizes: Option<GroupSizes>) -> Self {
        if let Some(s) = sizes {
            self.needed_bytes = Some(s.needed_bytes);
            self.capacity_bytes = Some(s.capacity_bytes);
        }
        self
    }
}

/// Where a warm's frames go: the caller's channel, or nowhere (the realtime
/// session's own warm, which only logs) — and how the warm learns that its
/// caller went away.
#[derive(Clone, Default)]
pub(crate) struct Reporter {
    tx: Option<UnboundedSender<ModelState>>,
    /// The caller's life, for frames that go nowhere: raised when it ends.
    life: Option<StopSignal>,
}

impl Reporter {
    /// Frames into `tx`; its receiver dropped means the caller went away.
    pub(crate) fn to(tx: UnboundedSender<ModelState>) -> Self {
        Self {
            tx: Some(tx),
            life: None,
        }
    }

    /// Frames to nobody, and a caller that goes away when `life` is raised
    /// (a realtime session's warmer, dropped with the session).
    pub(crate) fn until(life: StopSignal) -> Self {
        Self {
            tx: None,
            life: Some(life),
        }
    }

    /// Frames into `tx`, and a caller that goes away when `life` is raised
    /// or `tx`'s receiver is dropped: a bound realtime session's connect
    /// warm, said as `lmgw.model.state` (chat-voice design §8.7).
    pub(crate) fn to_until(tx: UnboundedSender<ModelState>, life: StopSignal) -> Self {
        Self {
            tx: Some(tx),
            life: Some(life),
        }
    }

    pub(crate) fn send(&self, s: ModelState) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(s);
        }
    }

    /// Say `outcome` for `stage`.
    pub(crate) fn outcome(&self, stage: &'static str, alias: &str, outcome: &WarmOutcome) {
        if let Some(s) = ModelState::of(stage, alias, outcome) {
            self.send(s);
        }
    }

    /// Resolves once the caller went away: its receiver was dropped, or its
    /// life ended — whichever comes first. Never, for a reporter with
    /// neither (`default`).
    pub(crate) async fn gone(&self) {
        let closed = async {
            match &self.tx {
                Some(tx) => tx.closed().await,
                None => std::future::pending().await,
            }
        };
        let ended = async {
            match &self.life {
                Some(life) => life.raised().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = closed => {}
            () = ended => {}
        }
    }

    /// Whether the caller has gone away.
    pub(crate) fn is_gone(&self) -> bool {
        self.tx.as_ref().is_some_and(UnboundedSender::is_closed)
            || self.life.as_ref().is_some_and(StopSignal::is_raised)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Review W6-9: every frame the gateway writes reads into the clients'
    /// type (`lmgw-api-types::chat_voice::ModelState`) field for field — the
    /// two are separate structs, and this is what keeps them in step.
    #[test]
    fn every_frame_reads_as_the_clients_type() {
        let outcomes = [
            WarmOutcome::Ready { ms: Some(12) },
            WarmOutcome::Ready { ms: None },
            WarmOutcome::Held {
                cause: HeldCause::GpuHold,
                message: "held".into(),
            },
            WarmOutcome::Held {
                cause: HeldCause::Benchmark,
                message: "bench".into(),
            },
            WarmOutcome::Fallback {
                answered_by: "cloud".into(),
            },
            WarmOutcome::Skipped {
                reason: SkipReason::DoesNotFit,
                message: "no room".into(),
                sizes: Some(GroupSizes {
                    needed_bytes: 9,
                    capacity_bytes: 4,
                }),
            },
            WarmOutcome::Skipped {
                reason: SkipReason::Full,
                message: "full".into(),
                sizes: None,
            },
            WarmOutcome::Skipped {
                reason: SkipReason::CannotSpeak,
                message: "mute".into(),
                sizes: None,
            },
            WarmOutcome::Failed {
                message: "boom".into(),
            },
        ];
        let frames = outcomes
            .iter()
            .filter_map(|o| ModelState::of("tts", "speak", o))
            .chain([ModelState::loading("asr", "hear")]);
        for ours in frames {
            let wire = serde_json::to_value(&ours).unwrap();
            let theirs: lmgw_api_types::chat_voice::ModelState =
                serde_json::from_value(wire.clone()).unwrap();
            let word = |v: serde_json::Value| v.as_str().map(str::to_string);
            assert_eq!(theirs.stage, ours.stage, "{wire}");
            assert_eq!(theirs.alias, ours.alias, "{wire}");
            assert_eq!(theirs.state, ours.state, "{wire}");
            assert_eq!(theirs.ms, ours.ms, "{wire}");
            assert_eq!(
                theirs.cause,
                ours.cause.and_then(|c| word(json!(c))),
                "{wire}"
            );
            assert_eq!(theirs.answered_by, ours.answered_by, "{wire}");
            assert_eq!(
                theirs.reason,
                ours.reason.and_then(|r| word(json!(r))),
                "{wire}"
            );
            assert_eq!(theirs.message, ours.message, "{wire}");
            assert_eq!(theirs.needed_bytes, ours.needed_bytes, "{wire}");
            assert_eq!(theirs.capacity_bytes, ours.capacity_bytes, "{wire}");
            // And nothing more is written than the clients' type holds
            // (review F-14): a field added here and not there fails, rather
            // than being dropped by every client. A `null` is no field.
            let present = |v: &serde_json::Value| -> serde_json::Map<String, serde_json::Value> {
                v.as_object()
                    .expect("a frame is an object")
                    .iter()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            };
            assert_eq!(
                present(&serde_json::to_value(&theirs).unwrap()),
                present(&wire),
                "the clients' type round-trips the frame whole"
            );
        }
    }

    #[test]
    fn each_outcome_is_said_in_its_frame_shape() {
        let f = |o: &WarmOutcome| serde_json::to_value(ModelState::of("asr", "a", o)).unwrap();
        assert_eq!(
            serde_json::to_value(ModelState::loading("asr", "a")).unwrap(),
            json!({"stage": "asr", "alias": "a", "state": "loading", "ms": null})
        );
        assert_eq!(
            f(&WarmOutcome::Ready { ms: Some(12) }),
            json!({"stage": "asr", "alias": "a", "state": "ready", "ms": 12})
        );
        assert_eq!(
            f(&WarmOutcome::Held {
                cause: HeldCause::Benchmark,
                message: "m".into()
            }),
            json!({"stage": "asr", "alias": "a", "state": "held", "ms": null,
                   "cause": "benchmark", "message": "m"})
        );
        assert_eq!(
            f(&WarmOutcome::Fallback {
                answered_by: "cloud".into()
            }),
            json!({"stage": "asr", "alias": "a", "state": "fallback", "ms": null,
                   "answered_by": "cloud"})
        );
        assert_eq!(
            f(&WarmOutcome::Skipped {
                reason: SkipReason::DoesNotFit,
                message: "m".into(),
                sizes: Some(GroupSizes {
                    needed_bytes: 2,
                    capacity_bytes: 1
                })
            }),
            json!({"stage": "asr", "alias": "a", "state": "skipped", "ms": null,
                   "reason": "does_not_fit", "message": "m",
                   "needed_bytes": 2, "capacity_bytes": 1})
        );
        assert!(ModelState::of("asr", "a", &WarmOutcome::Aborted).is_none());
        assert!(ModelState::of("asr", "a", &WarmOutcome::UpNotLoaded { ms: Some(3) }).is_none());
    }

    /// The caller going away is seen through either half: the frames'
    /// receiver dropped (a press's stream), or the life raised (a realtime
    /// session's warmer dropped with the session, WP8's connect warm).
    #[tokio::test]
    async fn a_reporter_goes_with_its_caller() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let r = Reporter::to(tx);
        assert!(!r.is_gone());
        drop(rx);
        assert!(r.is_gone());
        r.gone().await;

        let (handle, life) = crate::proxy::stop_pair();
        let r = Reporter::until(life);
        assert!(!r.is_gone());
        drop(handle);
        assert!(r.is_gone());
        r.gone().await;

        let never = Reporter::default();
        let waited = tokio::time::timeout(std::time::Duration::from_millis(20), never.gone()).await;
        assert!(waited.is_err() && !never.is_gone());
    }

    #[test]
    fn a_hold_or_a_lease_is_held_and_anything_else_failed() {
        let hold = GatewayError::GpuHold {
            model: "m".into(),
            detail: String::new(),
        };
        assert!(matches!(
            WarmOutcome::refused(&hold),
            WarmOutcome::Held {
                cause: HeldCause::GpuHold,
                ..
            }
        ));
        let other = GatewayError::Timeout;
        assert!(matches!(
            WarmOutcome::refused(&other),
            WarmOutcome::Failed { .. }
        ));
    }
}
