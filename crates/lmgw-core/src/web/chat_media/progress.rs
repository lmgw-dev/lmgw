//! A Chat turn's media calls while they run (chat-media-tools design §6.2,
//! §6.5): each call's progress as the turn's `tool` frames of event
//! `progress`, and its own stop, registered for the per-call cancel route.
//!
//! The frames are lossy: sent with `try_send`, so a reader that falls behind
//! drops previews rather than hold up a generation, and the next frame
//! supersedes the last anyway.

use std::sync::Arc;
use std::time::Instant;

use lmgw_api_types::chat_frames::ToolFrame;
use lmgw_api_types::chat_media::{MediaPhase, ToolProgressFrame};

use crate::mcp::exec::{LiveCall, LiveCalls};
use crate::mcp::media::Progress;
use crate::proxy::image_call::MediaProgress;
use crate::state::SharedState;

use super::super::chat_turn::{Events, TurnFrame};
use super::cancel_route::Registered;

/// The thread's side of [`LiveCalls`]: a turn's frames and the thread's
/// running calls.
pub(crate) struct ThreadLive {
    state: SharedState,
    thread_id: i64,
    tx: Events,
}

impl ThreadLive {
    pub fn new(state: SharedState, thread_id: i64, tx: Events) -> Arc<Self> {
        Arc::new(Self {
            state,
            thread_id,
            tx,
        })
    }
}

impl LiveCalls for ThreadLive {
    fn begin(&self, call_id: &str) -> LiveCall {
        let (handle, stop) = crate::proxy::stop_pair();
        let raise = Arc::new(handle);
        let running =
            self.state
                .media_calls
                .register(&self.state, (self.thread_id, call_id), raise.clone());
        LiveCall {
            progress: Box::new(CallFrames {
                tx: self.tx.clone(),
                call_id: call_id.to_string(),
                started: Instant::now(),
                running,
            }),
            stop,
            raise,
        }
    }
}

/// One call's frames; dropping it takes the call off the thread's running
/// calls.
struct CallFrames {
    tx: Events,
    call_id: String,
    started: Instant,
    running: Registered,
}

impl Progress for CallFrames {
    fn report(&self, p: &MediaProgress) {
        self.running.set_cancel(p.cancel());
        let frame = frame(&self.call_id, p, self.started.elapsed().as_millis() as u64);
        let _ = self
            .tx
            .try_send(TurnFrame::of("tool", &ToolFrame::Progress(frame)));
    }

    fn previews(&self) -> bool {
        true
    }
}

/// The frame of `p` for call `call_id`.
pub(crate) fn frame(call_id: &str, p: &MediaProgress, elapsed_ms: u64) -> ToolProgressFrame {
    let mut f = ToolProgressFrame {
        call_id: call_id.to_string(),
        elapsed_ms,
        cancel: p.cancel(),
        ..Default::default()
    };
    match p {
        MediaProgress::Downloading {
            model,
            image,
            done,
            total,
            size_note,
        } => {
            f.phase = Some(MediaPhase::Downloading);
            f.model = Some(model.clone());
            f.image = Some(image.clone());
            f.done_bytes = Some(*done);
            f.total_bytes = *total;
            f.size_note = size_note.clone();
        }
        MediaProgress::Stage { phase, model, room } => {
            f.phase = Some(*phase);
            f.model = Some(model.clone());
            if let Some((needs, free)) = room {
                f.needs_bytes = Some(*needs);
                f.free_bytes = Some(*free);
            }
        }
        MediaProgress::Upscaling { model, .. } => {
            f.phase = Some(MediaPhase::Upscaling);
            f.model = Some(model.clone());
        }
        MediaProgress::Queued {
            model, position, ..
        } => {
            f.phase = Some(MediaPhase::Queued);
            f.model = Some(model.clone());
            f.queue_position = Some(*position);
        }
        MediaProgress::Generating {
            model,
            steps,
            preview,
            ..
        } => {
            f.phase = Some(MediaPhase::Generating);
            f.model = Some(model.clone());
            if let Some(s) = steps {
                f.pass = Some(s.pass);
                f.step = Some(s.step);
                f.total_steps = Some(s.total);
            }
            f.preview = preview.clone();
        }
    }
    f
}
