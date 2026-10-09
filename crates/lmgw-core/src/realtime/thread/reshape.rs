//! A bound session's own voice following its thread's (personality-profiles
//! design D21): a profile switch (the thread's `profile_id`, a folder's
//! `defaults_patch`), a profile's voice edit, or a thread's voice chips
//! reach the session object from its next response, with no rebind.
//!
//! The bind shapes the session from the thread (`shape_session`); each
//! bound response re-reads the thread before it speaks, and its speech is
//! planned from that already (`turn`). What the re-read resolves for the
//! three thread-owned voice fields — `audio.output.voice`,
//! `lmgw.tts_model`, `lmgw.speech_instructions` — goes to the core with
//! the response's `Planned` message, and with a spoken response the speech
//! it planned. When that differs from the session object, the session
//! takes it through realtime's own merge, `session.lmgw.resolved` follows,
//! and the client is told with `session.updated` and the full session, as
//! after its own `session.update`. Said before the response's first audio
//! and never during another's: a response's turn is planned only once the
//! response before it has finished.
//!
//! A text-output response plans no speech, so `session.lmgw.resolved`'s
//! TTS and voice catch up at the next spoken one; the object's own three
//! fields follow at once. The rest of what the thread owns (the
//! transcription model and language) stays as the bind set it; turn
//! detection is the client's.

use serde_json::{json, Map, Value};

use super::super::protocol::ServerEvent;
use super::super::session::speech::Speech;
use super::super::session::Core;
use super::super::tts::{TtsResolution, TtsVia};
use super::super::voice::VoiceOutcome;
use crate::web::chat_voice::bound::{Plan, Source, VoiceConfig};

/// What a bound response's re-read thread says the session's voice is.
pub(crate) struct Reshape {
    /// The three fields as a partial `session.update` ([`voice_patch`]).
    pub patch: Map<String, Value>,
    /// The speech a spoken response planned, as the session resolves it:
    /// `session.lmgw.resolved`'s TTS, voice and speech facts. `None` for a
    /// text-output response.
    pub speech: Option<Speech>,
}

impl Reshape {
    /// The thread's voice as `cfg` resolves it, and `plan`'s speech.
    pub(crate) fn of(cfg: &VoiceConfig, plan: Option<&Plan>) -> Self {
        Self {
            patch: voice_patch(cfg),
            speech: plan.map(|p| Speech {
                tts: TtsResolution {
                    alias: Some(p.speech.alias.clone()),
                    via: TtsVia::Session,
                    missing: None,
                },
                facts: p.speech.facts.clone(),
                voice: VoiceOutcome::Resolved(p.speech.voice.clone()),
                expressive: p.expressive.clone(),
            }),
        }
    }
}

/// The thread-owned voice of a session object as `cfg` resolves it, as a
/// partial `session.update`: the voice (or `merge::DEFAULT_VOICE` when none
/// is named, as the Chat's speech plan asks), the TTS, and the thread's —
/// or its profile's — own speech style (design §2.3); an owner-wide style
/// is the setting's, not the session's.
pub(in crate::realtime) fn voice_patch(cfg: &VoiceConfig) -> Map<String, Value> {
    let style = match cfg.speech_style.source {
        Source::Thread | Source::Profile => json!(cfg.speech_style.text),
        _ => Value::Null,
    };
    let patch = json!({
        "type": "realtime",
        "audio": {
            "output": {
                "voice": cfg.voice.name.as_deref().unwrap_or(super::super::merge::DEFAULT_VOICE),
            },
        },
        "lmgw": {"tts_model": cfg.tts.alias, "speech_instructions": style},
    });
    match patch {
        Value::Object(m) => m,
        _ => unreachable!("a JSON object literal"),
    }
}

impl Core {
    /// The thread's voice as a response re-read it (module doc): taken, and
    /// said with `session.updated`, when it differs from the session's.
    pub(in crate::realtime) fn reshape(&mut self, r: Reshape) {
        let snap = self.state.snapshot();
        let settings = &snap.settings.realtime;
        let mut next = match super::super::merge::apply_update(&self.session, &r.patch, settings) {
            Ok(next) => next,
            // Only a stored value out of range could, as at the bind: the
            // session keeps what it had, and says so.
            Err(e) => {
                tracing::warn!(
                    "realtime {}: the chat thread's voice settings did not apply to the \
                     session: {}",
                    self.id(),
                    e.message
                );
                self.session.clone()
            }
        };
        // What `lmgw.resolved` says of the speech: its TTS and voice. The
        // facts themselves move with every voice list a response reads, and
        // are no change of the session's.
        let speech = r.speech.filter(|s| {
            s.tts.alias != self.speech.tts.alias || s.voice.name() != self.speech.voice.name()
        });
        if next == self.session && speech.is_none() {
            return;
        }
        if let Some(speech) = speech {
            self.speech = speech;
        }
        super::super::session::set_resolved(
            &mut next,
            &self.chat,
            &self.asr,
            &self.speech,
            (settings, self.seed),
        );
        tracing::info!(
            "realtime {}: the chat thread's voice changed; the session speaks with TTS {} and \
             voice {} from this response",
            self.id(),
            self.speech.tts.alias.as_deref().unwrap_or("(none)"),
            self.speech.voice.name().as_deref().unwrap_or("(none)"),
        );
        self.session = next;
        self.ob.send(ServerEvent::SessionUpdated {
            session: Box::new(self.session.clone()),
        });
    }
}
