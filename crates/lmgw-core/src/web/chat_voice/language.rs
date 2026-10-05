//! What the two languages do at each speech stage (chat-voice design §2.1,
//! changed 2026-10-04, split 2026-10-05): the language the user speaks
//! drives speech recognition where the model takes a language; the reply
//! language drives the reply ([`super::prompt`]) and the voice's
//! pronunciation (`audio::language::tts_fit`). Where a stage's model cannot
//! take its language, [`notes`] says so beside the resolved voice — the
//! thread JSON's `voice_resolved.language_notes`, Settings' own
//! `chat_voice_language_notes` — and a speech plan logs it ([`tts_note`]).
//! While the two differ, each note opens by naming which language it is
//! about; with one language (the reply following the spoken one) the notes
//! read as before the split.
//!
//! Only an lmgw audio row is judged: lmgw knows what its engine does with a
//! language. A cloud alias is sent nothing for the TTS (OpenAI's speech has
//! no language) and the code as it is for the ASR, and gets no note.

use std::sync::Arc;

use crate::audio::language::{asr_fit, tts_fit};
use crate::audio::profile::{SpeechProfile, Unvoiced};
use crate::config::Snapshot;
use crate::state::SharedState;

use super::resolve::VoiceConfig;

/// One stage whose model the language does not reach as set — the API's
/// own shape (`SettingsFull::chat_voice_language_notes`).
pub(crate) use lmgw_api_types::chat_voice::LanguageNote;

/// The speech profile of the lmgw audio row `alias` resolves to; `None`
/// for any other alias, or one that does not resolve.
async fn profile_of(
    state: &SharedState,
    snap: &Snapshot,
    alias: &str,
) -> Option<Arc<SpeechProfile>> {
    let route = snap.resolve(alias).ok()?;
    let row = crate::audio::voices::row_of_route(snap, &route)?;
    Some(crate::audio::voices::row_profile(state, row).await)
}

/// The voice a row speaks when `voice` names none: the spec's default, or
/// the engine's own (Kokoro's `af_heart`).
fn speaks<'a>(profile: &'a SpeechProfile, voice: Option<&'a str>) -> Option<&'a str> {
    voice
        .or(profile.default_voice.as_deref())
        .or(match profile.unvoiced {
            Unvoiced::EngineDefault { voice } => voice,
            _ => None,
        })
}

/// Log a speech plan's TTS note (`note` on `alias` for `code`) once per
/// distinct note: every read-aloud and bound turn plans its speech, and the
/// page shows the note beside the language anyway. The set holds one entry
/// per distinct (alias, language, note) the process has seen — a handful.
pub(crate) fn log_once(label: &str, alias: &str, code: &str, note: &str) {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let key = format!("{alias}\u{0}{code}\u{0}{note}");
    let first = SEEN
        .get_or_init(Default::default)
        .lock()
        .map(|mut seen| seen.insert(key))
        .unwrap_or(true);
    if first {
        tracing::info!("{label}: the language '{code}': text-to-speech model '{alias}' {note}");
    } else {
        tracing::debug!("{label}: the language '{code}': text-to-speech model '{alias}' {note}");
    }
}

/// Why `code` does not reach the text-to-speech model `alias` as set when
/// it speaks `voice` — what a speech plan logs; `None` when it does, or for
/// an alias that is no lmgw audio row.
pub(crate) async fn tts_note(
    state: &SharedState,
    snap: &Snapshot,
    alias: &str,
    code: &str,
    voice: Option<&str>,
) -> Option<String> {
    let profile = profile_of(state, snap, alias).await?;
    tts_fit(code, &profile, speaks(&profile, voice)).note
}

/// The notes of Settings → Chat → Voice as saved: the Chat's own
/// resolution, a thread that overrides nothing (`settings_full`'s
/// `chat_voice_language_notes`).
pub(crate) async fn settings_notes(state: &SharedState) -> Vec<LanguageNote> {
    let snap = state.snapshot();
    let cfg = super::resolve::resolve(&snap, &crate::store::ChatThread::default());
    notes(state, &snap, &cfg).await
}

/// The notes for `cfg`'s languages (module doc): the ASR judged on the
/// spoken one, the TTS on the reply's; empty without them, or when both
/// stages take theirs.
pub(crate) async fn notes(
    state: &SharedState,
    snap: &Snapshot,
    cfg: &VoiceConfig,
) -> Vec<LanguageNote> {
    let spoken = cfg.language.value.as_deref();
    let reply = cfg.reply_language.value.as_deref();
    // Which language a note is about, said only while there are two.
    let split = matches!((spoken, reply), (Some(s), Some(r)) if s != r);
    let about = |what: &str, code: &str| {
        if split {
            format!("{what}, {}: ", crate::audio::language::display_name(code))
        } else {
            String::new()
        }
    };
    let mut out = Vec::new();
    if let (Some(code), Some(alias)) = (spoken, cfg.asr.alias.as_deref()) {
        if let Some(note) = profile_of(state, snap, alias)
            .await
            .and_then(|p| asr_fit(code, &p).note)
        {
            out.push(LanguageNote {
                stage: "asr".into(),
                alias: alias.to_string(),
                message: format!(
                    "{}speech-to-text model '{alias}' {note}",
                    about("the language you speak", code)
                ),
            });
        }
    }
    if let (Some(code), Some(alias)) = (reply, cfg.tts.alias.as_deref()) {
        let voice = cfg.voice.name.as_deref().or(cfg.voice.inherits.as_deref());
        if let Some(note) = tts_note(state, snap, alias, code, voice).await {
            out.push(LanguageNote {
                stage: "tts".into(),
                alias: alias.to_string(),
                message: format!(
                    "{}text-to-speech model '{alias}' {note}",
                    about("the reply language", code)
                ),
            });
        }
    }
    out
}
