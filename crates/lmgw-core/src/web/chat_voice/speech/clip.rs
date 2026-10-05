//! A thread's voice as a library clip its TTS cannot clone without a
//! transcript (chat-voice design §2.3, §6.1; `crate::audio::transcript`):
//! the thread JSON's `problems` say so ([`problem`]), so the voice settings
//! warn and read-aloud and voice mode refuse before the press; and the
//! speech plan refuses it ([`refusal`]) — read-aloud's `speech_error`, a
//! voice-mode turn's error — before anything is warmed or started. The
//! code is `voice_needs_transcript` either way, and the message names the
//! clip and the fix. Never another voice instead: the owner chose this one.

use crate::audio::transcript;
use crate::config::Snapshot;
use crate::realtime::voice::{SpeakVoice, VoiceFacts, VoiceOutcome};
use crate::state::SharedState;

use super::super::resolve::{Problem, VoiceConfig};
use super::plan::{voice_of, Refusal};

/// The refusal of `voice` for `alias`, when it is a clip of the facts'
/// [`VoiceFacts::untranscribed`].
pub(crate) fn refusal(alias: &str, voice: &SpeakVoice, facts: &VoiceFacts) -> Option<Refusal> {
    let clip = voice
        .send
        .as_deref()
        .filter(|s| facts.untranscribed.iter().any(|c| c == s))?;
    Some(Refusal {
        code: transcript::CODE.to_string(),
        message: transcript::message(alias, clip),
    })
}

/// The `tts` problem of a thread whose voice resolves to such a clip. Only
/// a row whose engine refuses one is looked at further — its profile is
/// cached — so the thread JSON of every other thread lists no clips.
pub(crate) async fn problem(
    state: &SharedState,
    snap: &Snapshot,
    cfg: &VoiceConfig,
    alias: &str,
    label: &str,
) -> Option<Problem> {
    let route = snap.resolve(alias).ok()?;
    let row = crate::audio::voices::row_of_route(snap, &route)?;
    if !crate::audio::voices::row_profile(state, row)
        .await
        .requires_reference_text
    {
        return None;
    }
    let v = voice_of(state, snap, cfg, alias, label).await;
    let Ok(VoiceOutcome::Resolved(voice)) = &v.outcome else {
        return None;
    };
    let r = refusal(alias, voice, &v.facts)?;
    Some(Problem {
        stage: "tts",
        code: transcript::CODE,
        message: r.message,
    })
}
