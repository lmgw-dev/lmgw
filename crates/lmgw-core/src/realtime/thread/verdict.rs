//! A bound session's audio-input verdict (voice-audio-input design §2.2):
//! whether its next turn goes to the chat model as audio, kept as
//! `Bound.audio_input` for the commit to read.
//!
//! It is judged at the bind (`bind`), and judged again off the core —
//! `gate::resolve` and the capability lookup are async — at every
//! `speech_started`, beside the warm's re-read of the thread, and after
//! every response, so push-to-talk (which has no `speech_started`) follows
//! a chip changed mid-session too. The judgement comes back by the session
//! loop's own arm; one older than the last started is not taken.
//!
//! **A prediction, not a permission.** A turn committed under a verdict
//! that has gone stale still goes only where `fit_route` lets its audio go
//! (a route this lmgw runs); anything else is refused before a byte leaves,
//! and the turn goes again as its transcript.

use super::super::session::Core;
use crate::store::InputPath;
use crate::web::chat_voice::bound::{self, AudioInput, Shown};

impl Core {
    /// Judge the verdict again, off the core (module doc).
    pub(in crate::realtime) fn rejudge_audio_input(&mut self) {
        let state = self.state.clone();
        let Some(b) = self.bound.as_mut() else {
            return;
        };
        let Some(tx) = b.verdict_tx.clone() else {
            return;
        };
        b.verdicts += 1;
        let (seq, id) = (b.verdicts, b.thread_id);
        tokio::spawn(async move {
            let Some(thread) = bound::thread(&state, id).await else {
                return;
            };
            let shown = bound::audio_input(&state, &thread).await;
            let _ = tx.send((seq, shown));
        });
    }

    /// A verdict judged off the core came back (module doc).
    pub(in crate::realtime) fn audio_verdict(&mut self, seq: u64, shown: Shown) {
        let id = self.id().to_string();
        let Some(b) = self.bound.as_mut() else {
            return;
        };
        if seq < b.verdicts {
            return;
        }
        if b.audio_input.verdict != shown.verdict {
            tracing::debug!(
                "realtime {id}: the next voice turn goes to {} as its {}{}",
                shown.verdict.model,
                shown.verdict.path.as_str(),
                shown
                    .verdict
                    .why
                    .as_deref()
                    .map_or(String::new(), |w| format!(": {w}"))
            );
        }
        b.audio_input = shown;
    }

    /// The verdict a turn committed now goes by: the last judged, with the
    /// session's memory of a refusal on top (§3.5). `None` for an unbound
    /// session.
    pub(in crate::realtime) fn audio_now(&self) -> Option<AudioInput> {
        let b = self.bound.as_ref()?;
        Some(b.audio_input.verdict.clone().after_refusal(&b.refused))
    }

    /// Whether a turn committed now goes to the chat model as audio.
    pub(in crate::realtime) fn hears_next(&self) -> bool {
        self.audio_now().is_some_and(|a| a.path == InputPath::Audio)
    }

    /// Whether audio input is on for the thread (the setting resolves to
    /// `local`): the session then says how each response's turns went
    /// (`lmgw.chat.input`, the timing's fields). With it off, nothing new is
    /// sent or stored (§5).
    pub(in crate::realtime) fn audio_input_on(&self) -> bool {
        self.bound
            .as_ref()
            .is_some_and(|b| b.audio_input.value == crate::store::AudioInputMode::Local)
    }
}
