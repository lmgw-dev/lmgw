//! The session's voice (realtime design §5.3, §5.4, §10.2, §10.3): the TTS
//! alias and the voice it speaks with, resolved at the handshake and again
//! by every `session.update` that changes either.
//!
//! A changed TTS alias is checked against the session's key like the chat
//! and ASR aliases (`policy::check`). The voice facts are read afresh when
//! the alias or the voice changes (`voice::facts`: the row and the voice
//! library — no request to the model, so neither the handshake nor the core
//! ever waits on one, WP3 review M1), keeping a list a response already read
//! for the same alias. A changed voice is resolved through the chain; only
//! `voice_not_found` refuses the update — a voice that resolves to nothing
//! is recorded, and said by the first audio response, so a text session
//! never trips over it; one only the model's list could confirm is accepted
//! provisionally and checked at the first spoken clause.
//!
//! **Before each response** ([`Core::refresh_voice`], B2 review 5) the voice
//! is resolved again when the settings changed since it was — the owner
//! fixed `voice_map`, `default_voice` or the TTS row — and while it is
//! `NotFound` or `Missing` the list a response read is forgotten, so the
//! next audio response's first clause reads it again: a fix to the model's
//! voices is seen too. Without a new snapshot nothing is redone, and the
//! voice library (a directory listing, never read on the core) is read again
//! only by a `session.update`.
//!
//! **The list read stays seen** (R6): forgetting it does not make the
//! engine's voices unseen again ([`VoiceFacts::seen`]). When it did, a
//! `default_voice` a cloning or speaker-drawing TTS lacks went out
//! provisionally on every response — admitted, maybe cold-started, then
//! refused. It is dropped with the TTS alias, or when the owner changes
//! what it was read under (`voice::list_key`); like the library, a clip
//! added to a mounted dir meanwhile is not seen until then.
//!
//! **What the TTS does with speech instructions and inline tags**
//! ([`expressive::SpeechFacts`], WP10) is gathered with the voice facts —
//! a row's speech profile, a remote alias's override, no request either —
//! and its row defaults and override are read again with the presets when
//! the settings change. Whether it designs its voice goes into the voice
//! facts too: such a TTS is sent no voice where nobody named one (R2).

use std::sync::Arc;

use super::super::expressive::{self, SpeechFacts};
use super::super::protocol::{ErrorObject, Session, Voice};
use super::super::tts::{self, TtsResolution};
use super::super::voice::{self, VoiceFacts, VoiceOutcome};
use super::super::{merge, policy};
use super::Core;
use crate::error::GatewayError;
use crate::proxy::RequestCtx;
use crate::state::SharedState;
use crate::telemetry::RequestClass;

/// The TTS alias, what its model is known to speak, and the voice.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Speech {
    pub tts: TtsResolution,
    pub facts: VoiceFacts,
    pub voice: VoiceOutcome,
    /// What the TTS alias does with speech instructions and inline tags
    /// (WP10); the default with no alias.
    pub expressive: SpeechFacts,
}

/// The handshake's speech: the TTS setting and the voice a session starts
/// with (`merge::DEFAULT_VOICE`). Never refuses: a built-in name is never
/// `voice_not_found` — what the owner's settings name for it and the model
/// lacks is `Missing`, said by an audio response (`voice_not_configured`,
/// B2 review 4) — but kept fallible like the other resolutions.
pub(crate) async fn initial(state: &SharedState) -> Result<Speech, GatewayError> {
    let tts = tts::resolve_tts(state, None).await?;
    let (facts, expressive) = match &tts.alias {
        // At the handshake: no session, and so no id, yet.
        Some(a) => gather(state, a, None).await,
        None => Default::default(),
    };
    let requested = Voice::Name(merge::DEFAULT_VOICE.into());
    let settings = &state.snapshot().settings.realtime;
    let voice = voice::resolve(tts.alias.as_deref(), &requested, &facts, settings)
        .unwrap_or_else(|e| VoiceOutcome::Missing(e.message));
    Ok(Speech {
        tts,
        facts,
        voice,
        expressive,
    })
}

/// The speech a session bound to a chat thread starts with (chat-voice
/// design §8.1): the thread's TTS alias — `None` with the reason the
/// thread's resolution gives — and the voice it names, or none named
/// (`merge::DEFAULT_VOICE`, which realtime's chain resolves as the Chat's
/// speech plan asks for it). Never refuses: what cannot speak is said by
/// the response that tries (as [`initial`]'s).
pub(crate) async fn for_thread(
    state: &SharedState,
    alias: Option<&str>,
    missing: Option<String>,
    voice_name: Option<&str>,
) -> Speech {
    let tts = TtsResolution {
        alias: alias.map(str::to_string),
        via: tts::TtsVia::Session,
        missing: alias.is_none().then(|| {
            missing.unwrap_or_else(|| "the chat thread names no text-to-speech model".into())
        }),
    };
    let (facts, expressive) = match alias {
        Some(a) => gather(state, a, None).await,
        None => Default::default(),
    };
    let requested = Voice::Name(voice_name.unwrap_or(merge::DEFAULT_VOICE).to_string());
    let settings = &state.snapshot().settings.realtime;
    let voice = voice::resolve(alias, &requested, &facts, settings)
        .unwrap_or_else(|e| VoiceOutcome::NotFound(e.message));
    Speech {
        tts,
        facts,
        voice,
        expressive,
    }
}

/// What `alias`'s model is known to speak and how it takes speech
/// instructions, read without a request — the voice facts told whether it
/// designs its voice ([`VoiceFacts::designs`]). `sid`: the session's id,
/// for the log.
async fn gather(state: &SharedState, alias: &str, sid: Option<&str>) -> (VoiceFacts, SpeechFacts) {
    let label = sid.map(|sid| format!("realtime {sid}"));
    let mut facts = voice::facts(state, alias, label.as_deref()).await;
    let expressive = expressive::facts(state, alias).await;
    facts.designs = expressive.designs();
    (facts, expressive)
}

/// The log lines of a resolution (§5.1).
pub(crate) fn log(session_id: &str, s: &Speech, requested: &Voice) {
    tts::log_resolution(session_id, &s.tts);
    if s.tts.alias.is_some() {
        voice::log_resolution(session_id, requested, &s.voice);
    }
}

/// `session.lmgw.tts_model`, as the session names it.
fn tts_model(s: &Session) -> Option<&str> {
    s.lmgw.as_ref().and_then(|l| l.tts_model.as_deref())
}

/// `audio.output.voice`, as the session names it — always set once
/// normalized.
pub(crate) fn requested_voice(s: &Session) -> Voice {
    s.audio
        .as_ref()
        .and_then(|a| a.output.as_ref())
        .and_then(|o| o.voice.clone())
        .unwrap_or_else(|| Voice::Name(merge::DEFAULT_VOICE.into()))
}

impl Core {
    /// The speech of `next`, a merged `session.update` — or the error that
    /// refuses the update. What did not change is kept as resolved.
    pub(super) async fn update_speech(&mut self, next: &Session) -> Result<Speech, ErrorObject> {
        // Fields, not the core, across the awaits: the core is `Send` but
        // not `Sync` (the resampler is not).
        update(&self.state, &self.ctx, &self.session, &self.speech, next).await
    }

    /// Before a response is created or started (module doc): the voice
    /// resolved again if the settings snapshot changed since it was, and —
    /// while it is `NotFound` or `Missing` — against no list, so the
    /// response's first clause reads the model's list again; the list seen
    /// still decides rule 3's checks before it starts (R6).
    pub(in crate::realtime) fn refresh_voice(&mut self) {
        let Some(alias) = self.speech.tts.alias.clone() else {
            return;
        };
        let snap = self.state.snapshot();
        let changed = !std::sync::Weak::ptr_eq(&self.voice_seen, &Arc::downgrade(&snap));
        let unresolved = !matches!(self.speech.voice, VoiceOutcome::Resolved(_));
        if !changed && !unresolved {
            return;
        }
        self.voice_seen = Arc::downgrade(&snap);
        if changed {
            // The row's presets and default preset as they are now; the
            // voice library is what was read (module doc).
            let (row, _) = voice::row_facts(&snap, &alias);
            self.speech.facts.presets = row.presets;
            self.speech.facts.clip_presets = row.clip_presets;
            self.speech.facts.default_preset = row.default_preset;
            // Its default description and seed, or the alias's override.
            expressive::refresh(&snap, &alias, &mut self.speech.expressive);
            self.speech.facts.designs = self.speech.expressive.designs();
            // The owner changed what the list was read under: read afresh.
            if voice::list_key(&snap, &alias) != self.list_key {
                self.speech.facts.seen = None;
            }
        }
        if unresolved {
            // Seen it stays (module doc, R6).
            self.speech.facts.listed = None;
        }
        let requested = requested_voice(&self.session);
        let voice = voice::resolve(
            Some(&alias),
            &requested,
            &self.speech.facts,
            &snap.settings.realtime,
        )
        .unwrap_or_else(|e| VoiceOutcome::NotFound(e.message));
        if voice != self.speech.voice {
            voice::log_resolution(self.id(), &requested, &voice);
            self.speech.voice = voice;
        }
    }

    /// A response read the voice list of TTS alias `alias` (§5.3): kept for
    /// the session's later updates while it still speaks with that alias —
    /// and as the list seen, with what it was read under (R6) — and the
    /// session's voice resolved again against it (package B review 5) —
    /// verified if the list shows it, and [`VoiceOutcome::NotFound`] if it
    /// does not, so later audio responses are refused before they start.
    pub(in crate::realtime) fn voices_read(&mut self, alias: &str, names: Vec<String>) {
        if self.speech.tts.alias.as_deref() != Some(alias) {
            return;
        }
        let snap = self.state.snapshot();
        self.list_key = voice::list_key(&snap, alias);
        self.speech.facts.seen = Some(names.clone());
        self.speech.facts.listed = Some(names);
        let requested = requested_voice(&self.session);
        let settings = &snap.settings.realtime;
        let voice = voice::resolve(Some(alias), &requested, &self.speech.facts, settings)
            .unwrap_or_else(|e| VoiceOutcome::NotFound(e.message));
        if voice != self.speech.voice {
            voice::log_resolution(self.id(), &requested, &voice);
            self.speech.voice = voice;
        }
    }
}

/// [`Core::update_speech`], over the fields it needs.
async fn update(
    state: &SharedState,
    ctx: &RequestCtx,
    session: &Session,
    current: &Speech,
    next: &Session,
) -> Result<Speech, ErrorObject> {
    let tts_changed = tts_model(next) != tts_model(session);
    let requested = requested_voice(next);
    let voice_changed = requested != requested_voice(session);
    if !tts_changed && !voice_changed {
        return Ok(current.clone());
    }
    let sid = session.id.clone().unwrap_or_default();
    let gateway = |e: &GatewayError, param: &str| ErrorObject::from_gateway(e).with_param(param);
    let mut next_speech = current.clone();
    if tts_changed {
        const PARAM: &str = "session.lmgw.tts_model";
        let tts = tts::resolve_tts(state, tts_model(next))
            .await
            .map_err(|e| gateway(&e, PARAM))?;
        if let Some(alias) = tts
            .alias
            .as_deref()
            .filter(|_| tts.alias != current.tts.alias)
        {
            policy::check(state, ctx, alias, RequestClass::Audio)
                .await
                .map_err(|e| gateway(&e, PARAM))?;
        }
        next_speech.tts = tts;
    }
    // Afresh, so a preset or a clip added meanwhile is seen (review m9) —
    // keeping a list a response read for this very alias.
    (next_speech.facts, next_speech.expressive) = match next_speech.tts.alias.as_deref() {
        Some(alias) => {
            let (mut facts, expressive) = gather(state, alias, Some(&sid)).await;
            if next_speech.tts.alias == current.tts.alias {
                facts.listed = current.facts.listed.clone();
                facts.seen = current.facts.seen.clone();
            }
            (facts, expressive)
        }
        None => Default::default(),
    };
    let settings = &state.snapshot().settings.realtime;
    next_speech.voice = voice::resolve(
        next_speech.tts.alias.as_deref(),
        &requested,
        &next_speech.facts,
        settings,
    )?;
    log(&sid, &next_speech, &requested);
    Ok(next_speech)
}
