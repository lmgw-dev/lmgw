//! Expressive speech in a realtime session (WP10; realtime design §5.4,
//! §7.2, §8.2): the speech instructions a response's TTS is sent, the seed
//! that keeps a designed voice, and the hint that tells the chat model which
//! sounds its voice can make.
//!
//! - **The style is lmgw's own knob** (D1): GA has no speech-style field,
//!   and `session.instructions` is the chat model's prompt — long, often
//!   about tools, and on a voice-design row it would design a voice from
//!   it. So it is `session.lmgw.speech_instructions`, a response's
//!   `response.lmgw.speech_instructions`, and the owner's
//!   `realtime.speech_instructions`.
//! - **Precedence** ([`resolve_style`], D4): the response's, the session's,
//!   the setting's, then the TTS row's own description (its default request
//!   options' `instruction` or `instruct`, which its engine merges in). `""`
//!   at a level is "none": lmgw sends nothing, and a row's own description
//!   still holds. On a voice-design row that describes itself the owner's
//!   setting stands back — a style is not a voice. A text that is sent
//!   replaces the row's under both keys (`audio::shape`, R2).
//! - **Per mode** (D5) of the session's TTS ([`SpeechFacts`]): `style` and
//!   `passthrough` get the text with every clause; `voice_design` designs
//!   the voice from it, and with no description from any source an audio
//!   response is refused before it is created (`instructions_required`);
//!   `none` reads nothing — the text is shaped away on the way (`dropped`),
//!   said once per resolution, never per clause.
//! - **Seed** ([`seed_in_effect`], D6): one random seed per session, or the
//!   client's `speech_seed`, sent where the final row reads one
//!   (`proxy::synthesize::sends_seed`) — on its own to a row whose voice
//!   comes from it: one that designs it, or OmniVoice, which draws a
//!   speaker when it is named none (R4 M1).
//! - **Hint** ([`hint`], D7): a paragraph after the instructions in an audio
//!   response's prompt, naming the sounds the TTS renders, or, for a TTS
//!   that takes delivery cues instead (WP9b C6), how to ask for a delivery
//!   — [`lmgw_api_types::realtime::speech_hint_text`], the text the
//!   dashboard previews.
//! - **Cues** ([`SpeechFacts::takes_cues`], WP9b): a `style` or
//!   `passthrough` TTS that renders no tags gets a clause's leading tag as
//!   how to say it, after the style (`crate::audio::cues`) — not a cloud
//!   alias nobody described, whose mode is only assumed. Whether a
//!   clause gets its cue is decided on the route that answers it
//!   (`proxy::synthesize`, a fallback by its own rules); the echo's `cues`
//!   is the primary's.
//!
//! What the session knows about its TTS is gathered with its voice facts,
//! without a request to the model ([`facts`]: a local row's speech profile,
//! a remote alias's owner override), and its sync half is read again when
//! the settings change ([`refresh`]). The hint and the echo come from the
//! primary alias; a fallback that answers is shaped per clause by its own
//! rules (D12).

use crate::audio::cues::row_description;
use crate::audio::profile::{InstructionsMode, Unvoiced};
use crate::audio::shape::{Expressive, ShapeChange, ShapeReport};
use crate::audio::tags::TagMode;
use crate::audio::voices::{row_of_route, row_profile};
use crate::config::{AudioModel, RealtimeSettings, Snapshot};
use crate::proxy::synthesize::{sends_seed, SessionSeed};
use crate::state::SharedState;

use super::protocol::{Session, SpeechResolved};

/// What a session knows about how its TTS alias takes speech instructions
/// and inline tags — from the primary, never a fallback (D12).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SpeechFacts {
    /// What it does with instructions (`capabilities.speech.instructions`).
    pub mode: InstructionsMode,
    /// `mode` is assumed, not declared: a remote alias nobody described
    /// (`Expressive::assumed`). It takes no cues.
    pub assumed: bool,
    /// What it does with inline tags.
    pub tags: TagMode,
    /// The tags a fixed vocabulary renders, in its spelling.
    pub vocab: Vec<String>,
    /// Its engine takes a `seed` (an lmgw row only).
    pub reads_seed: bool,
    /// Its engine draws a speaker when it is named none (OmniVoice,
    /// `Unvoiced::DrawsSpeaker`, R4 M1).
    pub draws_speaker: bool,
    /// The row's own description — its default request options'
    /// `instruction` or `instruct` — which its engine merges into every
    /// request; only for a row that reads instructions.
    pub row_description: Option<String>,
    /// The row pins its own seed (`default_request_options.seed`).
    pub row_seed: bool,
}

impl SpeechFacts {
    /// The voice is designed from the description (D5).
    pub fn designs(&self) -> bool {
        self.mode == InstructionsMode::VoiceDesign
    }

    /// Its voice comes from the seed: it designs one, or draws a speaker
    /// (`proxy::synthesize::seeds_voice`).
    pub fn seeds_voice(&self) -> bool {
        self.designs() || self.draws_speaker
    }

    /// A remote alias's: the owner's override, or what lmgw assumes of a
    /// TTS nobody described (`Expressive::remote`: passthrough, no tags).
    fn of_rules(rules: &Expressive) -> Self {
        Self {
            mode: rules.instructions,
            assumed: rules.assumed,
            tags: rules.tags,
            vocab: rules.vocab.clone(),
            ..Self::default()
        }
    }

    /// It takes delivery cues (WP9b C1): a leading tag of a clause is how
    /// to say it, sent as instructions (`crate::audio::cues`).
    pub fn takes_cues(&self) -> bool {
        lmgw_api_types::realtime::takes_cues(self.declared_mode(), self.tags.as_str(), &self.vocab)
    }

    /// The mode's word as the TTS declares it, `None` when it is only
    /// assumed.
    fn declared_mode(&self) -> Option<&'static str> {
        (!self.assumed).then(|| self.mode.as_str())
    }

    /// What the row's own defaults say, as they are now.
    fn read_row(&mut self, row: &AudioModel) {
        self.row_description = (self.mode != InstructionsMode::None)
            .then(|| row_description(row))
            .flatten();
        self.row_seed = row.default_request_options.contains_key("seed");
    }
}

/// Gather `alias`'s [`SpeechFacts`] without a request to the model: an lmgw
/// audio row's speech profile (read off its package, cached) and its
/// defaults, or a remote alias's owner override. An alias that does not
/// resolve has none.
pub(crate) async fn facts(state: &SharedState, alias: &str) -> SpeechFacts {
    let snap = state.snapshot();
    let Ok(route) = snap.resolve(alias) else {
        return SpeechFacts::default();
    };
    let Some(row) = row_of_route(&snap, &route).cloned() else {
        return SpeechFacts::of_rules(&crate::proxy::remote_rules(&snap, alias));
    };
    // The profile alone: the facts need no voice list.
    let p = &row_profile(state, &row).await;
    let mut f = SpeechFacts {
        mode: p.instructions,
        tags: p.inline_tags,
        vocab: if p.inline_tags == TagMode::Fixed {
            p.tags.clone()
        } else {
            Vec::new()
        },
        reads_seed: p.reads_seed,
        draws_speaker: p.unvoiced == Unvoiced::DrawsSpeaker,
        ..SpeechFacts::default()
    };
    f.read_row(&row);
    f
}

/// The settings changed (`session::speech::Core::refresh_voice`): what
/// `snap` says now of `alias` — a row's defaults, a remote alias's override.
/// A row's profile is the package's, and is read again when the session's
/// TTS alias changes.
pub(crate) fn refresh(snap: &Snapshot, alias: &str, facts: &mut SpeechFacts) {
    let Ok(route) = snap.resolve(alias) else {
        return;
    };
    match row_of_route(snap, &route) {
        Some(row) => facts.read_row(row),
        None => *facts = SpeechFacts::of_rules(&crate::proxy::remote_rules(snap, alias)),
    }
}

/// Where the speech instructions in effect come from (D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// `response.lmgw.speech_instructions`.
    Response,
    /// `session.lmgw.speech_instructions`.
    Session,
    /// `realtime.speech_instructions`.
    Setting,
    /// The TTS row's own default description.
    Row,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Response => "response",
            Self::Session => "session",
            Self::Setting => "setting",
            Self::Row => "row",
        }
    }
}

/// The speech instructions a response speaks with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Style {
    /// What every clause is sent; `None` sends nothing — a row's own
    /// description then still holds, applied by its engine.
    pub send: Option<String>,
    /// The text in effect: `send`, or the row's own description.
    pub text: Option<String>,
    pub source: Option<Source>,
    /// The TTS reads no instructions: `send` is shaped away on its way to
    /// it (a fallback that answers may still read it, D12).
    pub dropped: bool,
}

/// What a response's levels ask for (D2, D4), highest first. `None` at a
/// level is "not set" (absent or `null`), `Some("")` is "none".
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Asked<'a> {
    pub response: Option<&'a str>,
    pub session: Option<&'a str>,
    /// `realtime.speech_instructions`; `""` = none.
    pub setting: &'a str,
}

impl<'a> Asked<'a> {
    /// The session's own levels, with no response's on top.
    pub fn of_session(s: &'a Session, settings: &'a RealtimeSettings) -> Self {
        Self {
            response: None,
            session: s
                .lmgw
                .as_ref()
                .and_then(|l| l.speech_instructions.as_deref()),
            setting: &settings.speech_instructions,
        }
    }
}

/// The style a response speaks with (module doc: D4, D5).
pub(crate) fn resolve_style(facts: &SpeechFacts, asked: &Asked<'_>) -> Style {
    // A voice-design row that describes itself: the owner's style stands
    // back, a style is not a voice. A client's text still replaces the
    // row's description: shaping writes it under the row's key as well
    // (`audio::shape`, R2), since audio.cpp merges the row's default in
    // beside it and an engine that reads both keys refuses two that differ.
    let setting = (!(facts.designs() && facts.row_description.is_some())).then_some(asked.setting);
    let chosen = [
        (asked.response, Source::Response),
        (asked.session, Source::Session),
        (setting, Source::Setting),
    ]
    .into_iter()
    .find_map(|(text, source)| text.map(|t| (t.trim(), source)));
    match chosen.filter(|(t, _)| !t.is_empty()) {
        Some((text, source)) => Style {
            send: Some(text.to_string()),
            text: Some(text.to_string()),
            source: Some(source),
            dropped: facts.mode == InstructionsMode::None,
        },
        None => Style {
            text: facts.row_description.clone(),
            source: facts.row_description.as_ref().map(|_| Source::Row),
            ..Style::default()
        },
    }
}

/// The seed a session's clauses carry: the client's `speech_seed`, else the
/// one lmgw drew for the session.
pub(crate) fn session_seed(s: &Session, drawn: u32) -> SessionSeed {
    match s.lmgw.as_ref().and_then(|l| l.speech_seed) {
        Some(value) => SessionSeed {
            value,
            pinned: true,
        },
        None => SessionSeed {
            value: drawn,
            pinned: false,
        },
    }
}

/// The seed the primary is sent (D6), `None` when it gets none.
pub(crate) fn seed_in_effect(facts: &SpeechFacts, seed: SessionSeed) -> Option<u32> {
    sends_seed(
        facts.reads_seed,
        facts.seeds_voice(),
        facts.row_seed,
        seed.pinned,
    )
    .then_some(seed.value)
}

/// The hint an audio response's prompt gets (D7, WP9b C6): the sounds the
/// TTS renders, else the delivery cues it takes —
/// [`lmgw_api_types::realtime::speech_hint_text`]. `on` is the session's
/// `tag_hint`; `None` when it is off or the TTS does neither.
pub(crate) fn hint(facts: &SpeechFacts, on: bool) -> Option<String> {
    on.then(|| {
        lmgw_api_types::realtime::speech_hint_text(
            facts.declared_mode(),
            facts.tags.as_str(),
            &facts.vocab,
        )
    })
    .flatten()
}

/// Whether the session asks for the hint: its own `tag_hint`, else the
/// setting.
pub(crate) fn hint_on(s: &Session, settings: &RealtimeSettings) -> bool {
    s.lmgw
        .as_ref()
        .and_then(|l| l.tag_hint)
        .unwrap_or(settings.tag_hint)
}

/// `session.lmgw.resolved.speech` (D3): what the session's next audio
/// response speaks with, before any `response.lmgw`.
pub(crate) fn resolved(
    facts: &SpeechFacts,
    s: &Session,
    settings: &RealtimeSettings,
    drawn: u32,
) -> SpeechResolved {
    let style = resolve_style(facts, &Asked::of_session(s, settings));
    SpeechResolved {
        instructions: facts.mode.as_str().to_string(),
        text: style.text,
        source: style.source.map(|s| s.as_str().to_string()),
        dropped: style.dropped,
        tags: facts.tags.as_str().to_string(),
        cues: facts.takes_cues(),
        tag_hint: hint(facts, hint_on(s, settings)),
        seed: seed_in_effect(facts, session_seed(s, drawn)),
    }
}

/// The resolution's log line (§5.1: every substitution is logged) — once per
/// resolution, never per clause. Only a style the TTS cannot use is worth
/// a line (`dropped`, with the `source` it came from): the rest is in the
/// echo.
pub(crate) fn log_resolution(session_id: &str, alias: &str, dropped: bool, source: Option<&str>) {
    if let (true, Some(source)) = (dropped, source) {
        tracing::info!(
            "realtime {session_id}: TTS '{alias}' reads no speech instructions; the {source} \
             style is not sent to it"
        );
    }
}

/// What a clause's shaping dropped or stripped, for the one line a response
/// logs about it (D12): a dropped style is left out when the resolution has
/// said it already (`expected_drop`).
pub(crate) fn shaping_losses(report: &ShapeReport, expected_drop: bool) -> Vec<String> {
    report
        .changes
        .iter()
        .filter_map(|c| match c {
            ShapeChange::InstructionsDropped if !expected_drop => {
                Some("its speech instructions were dropped (the model reads none)".to_string())
            }
            ShapeChange::Tags { stripped, .. } if *stripped > 0 => Some(format!(
                "{stripped} inline tag(s) were stripped (the model does not render them)"
            )),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests;
