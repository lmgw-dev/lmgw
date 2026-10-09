//! The profile tier of voice resolution (personality-profiles design D8,
//! §2.3): where a thread's profile sits between the thread and the Chat's
//! own settings.

use super::{chat_tts, first, Source, VoiceName};
use crate::config::{ChatProfile, Settings};

/// The profile's text-to-speech alias, as a level of the TTS chain.
pub(super) fn tts_level(p: Option<&ChatProfile>) -> (Option<&str>, Source) {
    (
        p.and_then(|p| p.voice.tts_alias.as_deref()),
        Source::Profile,
    )
}

/// The profile's speech style: a text, `""` included (it is none, and a
/// value, as a thread's own is), else unset.
pub(super) fn style(p: Option<&ChatProfile>) -> Option<&str> {
    p.and_then(|p| p.voice.speech_style.as_deref())
}

/// What the profile's voice does for a thread whose TTS alias is `tts`
/// (M1: a voice belongs to one model). It belongs to the profile's TTS
/// alias, or to the Chat's own when the profile names none. `Ok`: the voice
/// applies. `Err(Some(note))`: it was chosen for another model, in the
/// page's words. `Err(None)`: the profile names no voice.
pub(super) fn voice(
    p: &ChatProfile,
    s: &Settings,
    tts: Option<(&str, Source)>,
) -> Result<VoiceName, Option<String>> {
    let Some((name, _)) = first([(p.voice.voice.as_deref(), Source::Profile)]) else {
        return Err(None);
    };
    let chosen_for = first([(p.voice.tts_alias.as_deref(), Source::Profile)])
        .or_else(|| chat_tts(s))
        .map(|(a, _)| a);
    if chosen_for == tts.map(|(a, _)| a) {
        return Ok(VoiceName {
            name: Some(name.to_string()),
            source: Some(Source::Profile),
            inherits: None,
            note: None,
        });
    }
    Err(Some(format!(
        "the voice '{name}' set in the personality profile '{}' was chosen for {}; this \
         thread's text-to-speech model does not use it",
        p.name,
        chosen_for.map_or("another text-to-speech model".to_string(), |m| format!(
            "the text-to-speech model '{m}'"
        )),
    )))
}
