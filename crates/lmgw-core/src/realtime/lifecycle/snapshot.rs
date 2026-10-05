//! One response's snapshot of the session config (realtime design §4.2
//! step 4, §7.2, §8.3, §19): what `response.create` asked for on top of the
//! session, or why this response cannot be made.

use std::time::Duration;

use serde_json::Value;

use super::super::expressive::{self, Asked, Style};
use super::super::protocol::{
    AudioConfig, AudioFormat, AudioOutput, ErrorObject, Inf, MaxOutputTokens, Modality,
    ResponseCreateParams, Tool, ToolChoice, Voice, PCM_RATE,
};
use super::super::session::speech::requested_voice;
use super::super::voice::{self, SpeakVoice, VoiceFacts, VoiceOutcome, VoiceVia};
use super::Core;
use crate::proxy::synthesize::SessionSeed;

/// How one response speaks (§5.3, §8.2), snapshotted with the rest.
#[derive(Debug, Clone)]
pub(crate) struct Speaking {
    /// The TTS alias.
    pub tts: String,
    /// The voice asked for: the session's, or the `response.create`'s own.
    pub requested: Voice,
    pub voice: SpeakVoice,
    /// What the session knows the TTS alias speaks, for the first clause's
    /// check (§5.3).
    pub facts: VoiceFacts,
    /// What `response.audio.output.voice` echoes: the voice the session
    /// asked for, as a string.
    pub echo: String,
    /// `output_lead_ms`.
    pub lead: Duration,
    /// `synthesis_ahead_s`; `None` for no bound (§8.2).
    pub ahead: Option<Duration>,
    /// `longest_pause_ms`; 0 keeps the engine's silences (§8.2).
    pub longest_pause_ms: u32,
    /// `audio.output.speed`.
    pub speed: Option<f64>,
    /// The parameter that named the voice — the `response.create`'s own,
    /// or the session's — for an error about it at the first clause
    /// (package B review 6).
    pub voice_param: &'static str,
    /// The session's language (`audio.input.transcription.language`): what
    /// the clauses are synthesized in, where the TTS row declares languages
    /// — a hint (`crate::audio::language::hint_language`).
    pub language: Option<crate::audio::language::SpeechLanguage>,
    /// The speech instructions every clause carries (WP10 D4, D5).
    pub style: Style,
    /// The session's seed, sent where the row reads one (WP10 D6).
    pub seed: SessionSeed,
    /// The paragraph about the TTS's sounds the prompt gets (WP10 D7).
    pub hint: Option<String>,
}

/// One response's snapshot of the session config (§4.2 step 4): every
/// field a response renders or speaks with lives here, taken when it is
/// created, so a `session.update` while it waits for transcripts or runs
/// changes only the next one.
pub(super) struct Snapshot {
    pub alias: String,
    pub instructions: String,
    pub tools: Vec<Tool>,
    pub tool_choice: Option<ToolChoice>,
    pub parallel_tool_calls: Option<bool>,
    /// The session's `reasoning` object, as the client sent it (§7.6).
    pub reasoning: Option<Value>,
    pub modalities: Vec<Modality>,
    pub max_output_tokens: MaxOutputTokens,
    pub metadata: Option<Value>,
    /// `None` for text output (§8.3).
    pub speaking: Option<Speaking>,
}

impl Core {
    /// The session config for one response, with its overrides — or why
    /// this response cannot be made.
    pub(super) fn snapshot(
        &self,
        p: Option<&ResponseCreateParams>,
    ) -> Result<Snapshot, ErrorObject> {
        if let Some(c) = p.and_then(|p| p.conversation.as_ref()) {
            if c.as_str() != Some("auto") {
                return Err(ErrorObject::invalid(
                    "unsupported",
                    format!(
                        "response.conversation {c} (an out-of-band response) is not supported: \
                         lmgw runs one response at a time, in the conversation"
                    ),
                )
                .with_param("response.conversation"));
            }
        }
        if p.is_some_and(|p| p.input.is_some()) {
            return Err(ErrorObject::invalid(
                "unsupported",
                "response.input (an out-of-band response over its own items) is not supported: \
                 lmgw runs one response at a time, in the conversation",
            )
            .with_param("response.input"));
        }
        let s = &self.session;
        let audio = audio_override(p)?;
        let modalities = p
            .and_then(|p| p.output_modalities.clone())
            .or_else(|| s.output_modalities.clone())
            .unwrap_or_default();
        let own_style = p
            .and_then(|p| p.lmgw.as_ref())
            .and_then(|l| l.speech_instructions.as_deref());
        let speaking = match modalities.as_slice() {
            [Modality::Text] => None,
            [Modality::Audio] => Some(self.speaking(audio.as_ref(), own_style)?),
            _ => {
                return Err(ErrorObject::invalid(
                    "invalid_value",
                    "output_modalities must be exactly one of [\"audio\"] or [\"text\"]",
                )
                .with_param("response.output_modalities"))
            }
        };
        let Some(alias) = self.chat.alias.clone() else {
            return Err(ErrorObject::invalid(
                "chat_not_configured",
                "this session has no chat model: name one with session.update (session.model), \
                 or set realtime.default_model",
            )
            .with_param("session.model"));
        };
        Ok(Snapshot {
            alias,
            instructions: p
                .and_then(|p| p.instructions.clone())
                .or_else(|| s.instructions.clone())
                .unwrap_or_default(),
            tools: p
                .and_then(|p| p.tools.clone())
                .or_else(|| s.tools.clone())
                .unwrap_or_default(),
            tool_choice: p
                .and_then(|p| p.tool_choice.clone())
                .or_else(|| s.tool_choice.clone()),
            parallel_tool_calls: s.parallel_tool_calls,
            reasoning: s.reasoning.clone(),
            modalities,
            max_output_tokens: p
                .and_then(|p| p.max_output_tokens)
                .or(s.max_output_tokens)
                .unwrap_or(MaxOutputTokens::Inf(Inf::Inf)),
            metadata: p.and_then(|p| p.metadata.clone()),
            speaking,
        })
    }

    /// How an audio response speaks — or, before it is created, why it
    /// cannot: no TTS alias (`tts_not_configured`), no voice to speak with
    /// (`voice_not_configured`): an engine is never called without one
    /// (§5.3) — or a voice-design TTS with nothing to design the voice from
    /// (`instructions_required`, WP10 D5). Text output works either way.
    /// `over` is the `response.create`'s own `audio.output`: its voice goes
    /// through the same chain as the session's — provisional if only the
    /// model's list could confirm it, and checked at the first clause — and
    /// its speed replaces the session's, for this response only (WP3 review
    /// m6). `own_style` is its `lmgw.speech_instructions`, over the
    /// session's for this response only.
    ///
    /// **A bound session** speaks with the thread's TTS and voice as each
    /// turn re-reads them (chat-voice §8.2): the turn's speech plan judges
    /// them after `response.created`, so a chip fixed mid-session applies
    /// from the next turn. The session's own TTS and voice are the bind's,
    /// and are not refused here (WP8 review m4) — nor is a voice-design TTS
    /// without a description: the thread's style chain is the Chat's.
    fn speaking(
        &self,
        over: Option<&AudioOutput>,
        own_style: Option<&str>,
    ) -> Result<Speaking, ErrorObject> {
        let bound = self.bound.is_some();
        let tts = match self.speech.tts.alias.clone() {
            Some(tts) => tts,
            None if bound => String::new(),
            None => {
                let why = self.speech.tts.missing.as_deref().unwrap_or("no TTS alias");
                return Err(ErrorObject::invalid(
                    "tts_not_configured",
                    format!("this response cannot be spoken: {why}"),
                )
                .with_param("session.lmgw.tts_model"));
            }
        };
        let s = &self.session;
        let snap = self.state.snapshot();
        let settings = &snap.settings.realtime;
        let (requested, outcome, param) = match over.and_then(|o| o.voice.clone()) {
            Some(v) => {
                const PARAM: &str = "response.audio.output.voice";
                let o = voice::resolve(Some(&tts), &v, &self.speech.facts, settings)
                    .map_err(|e| e.with_param(PARAM))?;
                voice::log_resolution(self.id(), &v, &o);
                (v, o, PARAM)
            }
            None => (
                requested_voice(s),
                self.speech.voice.clone(),
                "session.audio.output.voice",
            ),
        };
        let unspoken = |code: &str, why: String| {
            ErrorObject::invalid(code, format!("this response cannot be spoken: {why}"))
                .with_param(param)
        };
        let voice = match outcome {
            VoiceOutcome::Resolved(v) => v,
            // The turn's plan resolves the thread's voice (doc above); this
            // one is never sent.
            _ if bound => SpeakVoice {
                send: None,
                name: voice::echo(&requested),
                via: VoiceVia::Model,
                verified: false,
            },
            VoiceOutcome::Missing(why) => return Err(unspoken("voice_not_configured", why)),
            // A list a response read showed the model lacks it (§5.3).
            VoiceOutcome::NotFound(why) => return Err(unspoken("voice_not_found", why)),
        };
        let facts = &self.speech.expressive;
        let style = expressive::resolve_style(
            facts,
            &Asked {
                response: own_style,
                ..Asked::of_session(s, settings)
            },
        );
        // A bound session's speech style is the thread's, which the turn's
        // speech plan checks (chat-voice §8.2): the session's own chain
        // would judge it by realtime's setting instead.
        if facts.designs() && style.text.is_none() && !bound {
            return Err(ErrorObject::invalid(
                "instructions_required",
                format!(
                    "this response cannot be spoken: TTS model '{tts}' designs its voice from a \
                     description, and none is set — send session.lmgw.speech_instructions (e.g. \
                     \"a warm, calm female voice in her forties\"), set \
                     realtime.speech_instructions, or give the row a default under its default \
                     request options (`instruct`)"
                ),
            )
            .with_param("session.lmgw.speech_instructions"));
        }
        if own_style.is_some() {
            expressive::log_resolution(
                self.id(),
                &tts,
                style.dropped,
                style.source.map(|s| s.as_str()),
            );
        }
        let output = s.audio.as_ref().and_then(|a| a.output.as_ref());
        let lmgw = s.lmgw.as_ref();
        let lead_ms = lmgw
            .and_then(|l| l.output_lead_ms)
            .unwrap_or(settings.output_lead_ms);
        let ahead_s = lmgw
            .and_then(|l| l.synthesis_ahead_s)
            .unwrap_or(settings.synthesis_ahead_s);
        let longest_pause_ms = lmgw
            .and_then(|l| l.longest_pause_ms)
            .unwrap_or(settings.longest_pause_ms);
        Ok(Speaking {
            tts,
            echo: voice::echo(&requested),
            requested,
            voice,
            facts: self.speech.facts.clone(),
            lead: Duration::from_millis(u64::from(lead_ms)),
            ahead: (ahead_s > 0).then(|| Duration::from_secs(u64::from(ahead_s))),
            longest_pause_ms,
            speed: over
                .and_then(|o| o.speed)
                .or_else(|| output.and_then(|o| o.speed)),
            voice_param: param,
            language: super::super::input::session_language(s)
                .map(crate::audio::language::SpeechLanguage::Hint),
            hint: expressive::hint(facts, expressive::hint_on(s, settings)),
            seed: expressive::session_seed(s, self.seed),
            style,
        })
    }
}

/// `response.create`'s `audio.output` (§2.3), checked: a shape error, or a
/// format other than the session's — PCM16 at 24 kHz, the one this cascade
/// serves — is the response's error rather than silently ignored (WP3
/// review m6).
fn audio_override(p: Option<&ResponseCreateParams>) -> Result<Option<AudioOutput>, ErrorObject> {
    let Some(v) = p.and_then(|p| p.audio.as_ref()).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let audio: AudioConfig = serde_json::from_value(v.clone()).map_err(|e| {
        ErrorObject::invalid("invalid_value", format!("response.audio: {e}"))
            .with_param("response.audio")
    })?;
    let Some(out) = audio.output else {
        return Ok(None);
    };
    super::super::merge::check_speed(out.speed, "response.audio.output.speed")?;
    match out.format {
        None | Some(AudioFormat::Pcm { rate: PCM_RATE }) => Ok(Some(out)),
        Some(f) => {
            let asked = match f {
                AudioFormat::Pcm { rate } => format!("audio/pcm at {rate} Hz"),
                AudioFormat::Pcmu => "audio/pcmu".into(),
                AudioFormat::Pcma => "audio/pcma".into(),
            };
            Err(ErrorObject::invalid(
                "unsupported",
                format!(
                    "response.audio.output.format {asked} is not supported: a response speaks \
                     the session's format, audio/pcm at {PCM_RATE} Hz"
                ),
            )
            .with_param("response.audio.output.format"))
        }
    }
}
