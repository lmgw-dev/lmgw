//! A turn's start and end as the session core takes them (realtime design
//! §2.3, §4.3, §6.4, §6.5).
//!
//! **`speech_started` goes out first**, before anything it causes: clients
//! stop playback on it, and `@openai/agents` interrupts only while it still
//! counts the response's item as playing — up to that item's
//! `output_audio.done`, which a barge-in's cancel sends right behind it
//! (§2.3). Then the owed response is deferred (the user is not done), the
//! models are warmed (§9.1), and the active response, if any, is cut when
//! `interrupt_response` says so (`lifecycle::interrupt`) — or when it is a
//! held response nobody heard (voice-audio-input design §3.1). With
//! `interrupt_response: false` the turn runs beside the response — the
//! client stops playback on the event itself, and `@openai/agents` sends
//! `response.cancel` — and is answered after it (`lifecycle::pending`).
//!
//! Every turn that starts while a response is in progress, and every turn
//! the barge-in gate started, says at INFO what it did; speech in the
//! playing window that was deliberately no turn says so at DEBUG.

use tokio::time::Instant;

use super::super::audio_in::Detected;
use super::super::lifecycle::Interruption;
use super::super::protocol::ServerEvent;
use super::super::session::Core;
use super::super::turn::arbiter::{DropReason, Dropped};
use super::super::turn::server_vad::TurnEvent;
use super::{create_response, interrupt_response, OpenTurn};

impl Core {
    /// A turn event, judged at the capture instant of its deciding frame.
    #[cfg(test)]
    pub(in crate::realtime) fn on_judged(&mut self, d: Detected) {
        self.on_judged_noted(d, None, None);
    }

    /// [`Self::on_judged`]; `note`: what the barge-in word check heard, for
    /// the start line of the turn it let through (`word_check`), and
    /// `heard_by` its alias when it heard words (N3).
    pub(in crate::realtime) fn on_judged_noted(
        &mut self,
        d: Detected,
        note: Option<&str>,
        heard_by: Option<&str>,
    ) {
        match d.event {
            TurnEvent::SpeechStarted {
                audio_start_ms,
                onset_ms,
            } => self.speech_started(audio_start_ms, onset_ms, d.at, d.barge_in, note, heard_by),
            TurnEvent::SpeechStopped {
                audio_end_ms,
                segment,
            } => {
                let Some(turn) = self.turn.take() else {
                    return;
                };
                self.ob.send(ServerEvent::SpeechStopped {
                    audio_end_ms,
                    item_id: turn.item_id.clone(),
                });
                if self.asr.alias.is_none() {
                    self.asr_missing(None);
                    // The turn will never commit: what its speech deferred
                    // — an owed response, a held create — decides now.
                    return self.pending_undefer();
                }
                let auto = create_response(&self.session);
                self.commit_turn(turn, segment.samples, auto, None, d.speech_end, d.end);
            }
        }
    }

    fn speech_started(
        &mut self,
        audio_start_ms: u64,
        onset_ms: u64,
        at: Instant,
        gate: bool,
        note: Option<&str>,
        heard_by: Option<&str>,
    ) {
        let item_id = self.conversation.fresh_item_id(&self.ids);
        self.ob.send(ServerEvent::SpeechStarted {
            audio_start_ms,
            item_id: item_id.clone(),
        });
        self.turn = Some(OpenTurn {
            item_id,
            heard_by: heard_by.map(str::to_string),
        });
        // Before anything that could end a response and start the next
        // (WP1c review H5).
        self.pending_defer();
        // The user is talking: get the models up (§9.1) — and judge again
        // whether the turn goes to the chat model as audio
        // (voice-audio-input design §2.2).
        self.warm();
        self.rejudge_audio_input();
        let how = self.interruption(at);
        // A held response nobody heard is cut whatever `interrupt_response`
        // says: it would answer half the sentence (voice-audio-input §3.1,
        // WP3 review #6).
        let cut = matches!(how, Interruption::Cuts { .. })
            && (interrupt_response(&self.session) || self.holding());
        if let Some(line) = turn_start_line(onset_ms, gate, &how, cut) {
            match note {
                Some(note) => tracing::info!("realtime {}: {line} — {note}", self.id()),
                None => tracing::info!("realtime {}: {line}", self.id()),
            }
        }
        if cut {
            self.interrupt();
        }
    }

    /// Speech in the playing window that was deliberately no turn (§6.4).
    pub(super) fn log_dropped(&self, d: Dropped) {
        match d.why {
            DropReason::Backchannel => tracing::debug!(
                "realtime {}: speech at {} ms of the input, during playback, stayed below \
                 barge_in_min_ms ({} ms of voice) — a backchannel, not a turn",
                self.id(),
                d.onset_ms,
                d.evidence_ms
            ),
            DropReason::HalfDuplex => tracing::debug!(
                "realtime {}: speech at {} ms of the input is ignored — input during playback \
                 is not listened to (session.lmgw.half_duplex)",
                self.id(),
                d.onset_ms
            ),
            DropReason::Checked => tracing::debug!(
                "realtime {}: speech at {} ms of the input, during playback, passed the barge-in \
                 gate ({} ms of voice) but the word check heard no words that interrupt — not a \
                 turn, and the answer played on",
                self.id(),
                d.onset_ms,
                d.evidence_ms
            ),
        }
    }
}

/// The INFO line for a turn that started during a response, or that the
/// barge-in gate started (module doc); `None` for a plain turn.
fn turn_start_line(onset_ms: u64, gate: bool, how: &Interruption, cut: bool) -> Option<String> {
    let by = if gate { " (the barge-in gate)" } else { "" };
    let what = match how {
        Interruption::Idle if gate => {
            return Some(format!(
                "speech from {onset_ms} ms of the input{by}, captured while a response played, \
                 is a normal turn: that response had already ended"
            ))
        }
        Interruption::Idle => return None,
        Interruption::Cuts { id, into_ms } => {
            let into = into_ms.map_or(String::new(), |ms| format!(", {ms} ms into its playback"));
            if cut {
                format!("during response {id}{into} — the response is cancelled (turn_detected)")
            } else {
                format!(
                    "during response {id}{into} — not cancelled (interrupt_response is false); \
                     the turn is answered after it"
                )
            }
        }
        Interruption::Generated { id } => format!(
            "during response {id} — not cancelled: its output was complete, and it finishes as \
             generated"
        ),
        Interruption::PlayedOut { id } => {
            format!("during response {id} — not cancelled: its audio had already played out")
        }
        Interruption::ToolsRun { id } => format!(
            "during response {id} — not cancelled: only its MCP calls run and nothing of it \
             plays, so this is a turn of its own, answered after it"
        ),
    };
    Some(format!("speech from {onset_ms} ms of the input{by} {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_during_a_response_says_what_it_did() {
        let cuts = Interruption::Cuts {
            id: "resp_1".into(),
            into_ms: Some(1200),
        };
        assert_eq!(
            turn_start_line(4000, true, &cuts, true).unwrap(),
            "speech from 4000 ms of the input (the barge-in gate) during response resp_1, 1200 \
             ms into its playback — the response is cancelled (turn_detected)"
        );
        assert!(turn_start_line(4000, true, &cuts, false)
            .unwrap()
            .contains("interrupt_response is false"));
        let late = turn_start_line(9000, true, &Interruption::Idle, false).unwrap();
        assert!(late.contains("already ended"), "{late}");
        assert_eq!(
            turn_start_line(9000, false, &Interruption::Idle, false),
            None
        );
        let done = Interruption::PlayedOut {
            id: "resp_2".into(),
        };
        assert!(turn_start_line(1, false, &done, false)
            .unwrap()
            .contains("played out"));
    }
}
