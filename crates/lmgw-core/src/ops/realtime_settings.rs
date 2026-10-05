//! The `realtime` settings section's save (realtime design §12): its patch,
//! the checks a save makes, and the section as `/api/settings-full` and
//! `lmgw__settings` report it.
//!
//! One patch for both surfaces — the dashboard's `settings_set_full` and the
//! tool plane's `settings_set` carry it as `realtime` — so the two cannot
//! accept different things. A save refuses what no session could run (fix
//! package B6): a `semantic_vad` row that cannot run, a word-check alias that
//! is no ASR alias, an alias of the wrong task, a script the word check does
//! not know, and both WebSocket limits at 0. The session-start fallbacks stay
//! as a second line for a blob that was written some other way.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::config::{
    BargeInCheck, RealtimeSettings, SemanticVadEngine, SemanticVadRow, DEFAULT_VOICE_INSTRUCTIONS,
};
use crate::state::SharedState;
use lmgw_api_types::realtime as dto;

/// Sparse patch over `RealtimeSettings`: a field left out keeps its value.
/// The two maps and the two lists are replaced whole, like a class's
/// `extra_run_args`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RealtimeSettingsPatch {
    /// The chat alias a session starts on when the client names none, or
    /// names an OpenAI Realtime model; `""` = none. Must be a chat alias.
    pub default_model: Option<String>,
    /// Client model names mapped to lmgw aliases — `session.model` to a chat
    /// alias, a transcription model to an ASR alias. Replaces the whole map.
    pub model_map: Option<BTreeMap<String, String>>,
    /// The speech-to-text alias turns are transcribed with; `""` = the
    /// Chat's transcription model. Must be an alias whose task is `asr`.
    pub asr_alias: Option<String>,
    /// The text-to-speech alias a session speaks with; `""` = none. Must be
    /// an alias whose task is `tts`, or `vdes` (a voice designed from the
    /// speech instructions).
    pub tts_alias: Option<String>,
    /// The voice a session speaks with when it names none, or names an
    /// OpenAI built-in voice; `""` = the TTS row's default preset.
    pub default_voice: Option<String>,
    /// Client voice names mapped to voices of the TTS model. Replaces the
    /// whole map.
    pub voice_map: Option<BTreeMap<String, String>>,
    /// The speech instructions a session's TTS gets while the client sends
    /// none: a speaking style, or a voice-design row's description. `""` =
    /// none.
    pub speech_instructions: Option<String>,
    /// Whether an audio response's prompt names the sounds the TTS can make.
    pub tag_hint: Option<bool>,
    /// The instructions a session with audio output uses while the client
    /// gives none. The built-in text returns to the built-in default (and
    /// follows it from then on); `""` = none.
    pub default_instructions: Option<String>,
    /// `server_vad.threshold`: the speech probability that counts as voice,
    /// `0.0..=1.0`.
    pub threshold: Option<f64>,
    pub prefix_padding_ms: Option<u32>,
    pub silence_duration_ms: Option<u32>,
    /// `smart_turn` | `server_vad`.
    pub semantic_vad_engine: Option<String>,
    /// The Smart Turn rows by eagerness; a row or field left out keeps its
    /// value.
    pub semantic_vad: Option<SemanticVadTablePatch>,
    pub semantic_floor_window_ms: Option<u32>,
    pub post_interrupt_silence_ms: Option<u32>,
    pub barge_in_min_ms: Option<u32>,
    pub barge_in_guard_ms: Option<u32>,
    pub half_duplex: Option<bool>,
    pub echo_tail_ms: Option<u32>,
    /// `words` | `duration`.
    pub barge_in_check: Option<String>,
    /// Replaces the whole list.
    pub backchannel_words: Option<Vec<String>>,
    /// Unicode script names (Latin, Cyrillic, Han, …); `[]` = every script.
    /// Replaces the whole list.
    pub barge_in_check_scripts: Option<Vec<String>>,
    /// `0` = no bound.
    pub barge_in_check_timeout_ms: Option<u32>,
    /// The ASR alias the word check transcribes with; `""` = the session's
    /// own ASR alias. Must be an alias whose task is `asr`.
    pub barge_in_check_alias: Option<String>,
    pub output_lead_ms: Option<u32>,
    /// `0` = no bound.
    pub synthesis_ahead_s: Option<u32>,
    /// The longest silence in synthesized speech, in ms; `0` = keep the
    /// engine's silences.
    pub longest_pause_ms: Option<u32>,
    pub warm_on_connect: Option<bool>,
    /// In MiB; `0` = no bound. Not `0` together with `max_frame_mb`.
    pub max_message_mb: Option<u32>,
    /// In MiB; `0` = bounded by `max_message_mb`. Not `0` together with it.
    pub max_frame_mb: Option<u32>,
    /// `0` = no pings, and no liveness bound at all.
    pub ping_interval_s: Option<u32>,
}

/// `realtime.semantic_vad`'s patch: the rows by eagerness.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticVadTablePatch {
    pub high: Option<SemanticVadRowPatch>,
    pub medium: Option<SemanticVadRowPatch>,
    pub low: Option<SemanticVadRowPatch>,
}

/// One row's patch.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticVadRowPatch {
    pub threshold: Option<f64>,
    pub floor: Option<f64>,
    pub max_wait_ms: Option<u32>,
    pub silence_duration_ms: Option<u32>,
}

impl SemanticVadRowPatch {
    fn apply(self, row: &mut SemanticVadRow) {
        if let Some(v) = self.threshold {
            row.threshold = v;
        }
        if let Some(v) = self.floor {
            row.floor = v;
        }
        if let Some(v) = self.max_wait_ms {
            row.max_wait_ms = v;
        }
        if let Some(v) = self.silence_duration_ms {
            row.silence_duration_ms = v;
        }
    }
}

/// A map's entries trimmed; an empty name or target is refused by the
/// setting's name, since a session could only ever miss it.
fn clean_map(
    field: &str,
    map: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for (k, v) in map {
        let (k, v) = (k.trim().to_string(), v.trim().to_string());
        if k.is_empty() {
            return Err(format!(
                "realtime.{field}: an entry has no name (it maps to '{v}')"
            ));
        }
        if v.is_empty() {
            return Err(format!("realtime.{field}: '{k}' maps to nothing"));
        }
        out.insert(k, v);
    }
    Ok(out)
}

/// A list's entries trimmed, the empty ones dropped.
fn clean_list(list: Vec<String>) -> Vec<String> {
    list.into_iter()
        .map(|w| w.trim().to_string())
        .filter(|w| !w.is_empty())
        .collect()
}

impl RealtimeSettingsPatch {
    /// Apply onto `r`, refusing what can be judged without the gateway: a
    /// value out of range, a name of an unknown engine, check or script, and
    /// both WebSocket limits at 0. Answers the settings it touched, as
    /// `realtime.<field>`.
    ///
    /// The `semantic_vad` rows are judged here too, but only when this patch
    /// touches the table or its floor window: a stored row a hand edit broke
    /// is the session start's to fall back from, not a reason to refuse an
    /// unrelated save.
    pub fn apply(self, r: &mut RealtimeSettings) -> Result<Vec<&'static str>, String> {
        let mut touched: Vec<&'static str> = Vec::new();
        macro_rules! set {
            ($field:ident) => {
                if let Some(v) = self.$field {
                    r.$field = v;
                    touched.push(concat!("realtime.", stringify!($field)));
                }
            };
            ($field:ident, trim) => {
                if let Some(v) = self.$field {
                    r.$field = v.trim().to_string();
                    touched.push(concat!("realtime.", stringify!($field)));
                }
            };
        }
        set!(default_model, trim);
        if let Some(m) = self.model_map {
            r.model_map = clean_map("model_map", m)?;
            touched.push("realtime.model_map");
        }
        set!(asr_alias, trim);
        set!(tts_alias, trim);
        set!(default_voice, trim);
        if let Some(m) = self.voice_map {
            r.voice_map = clean_map("voice_map", m)?;
            touched.push("realtime.voice_map");
        }
        // Types only: any text is a style or a description (WP10 D14).
        set!(speech_instructions, trim);
        set!(tag_hint);
        if let Some(v) = self.default_instructions {
            let v = v.trim();
            // The built-in text is not stored: an unset setting follows it
            // as releases improve it (§12), exactly as the Chat's default
            // system prompt does.
            r.default_instructions =
                (v != DEFAULT_VOICE_INSTRUCTIONS.trim()).then(|| v.to_string());
            touched.push("realtime.default_instructions");
        }
        if let Some(v) = self.threshold {
            if !(0.0..=1.0).contains(&v) {
                return Err(format!(
                    "realtime.threshold is a speech probability, 0.0 to 1.0, not {v}"
                ));
            }
            r.threshold = v;
            touched.push("realtime.threshold");
        }
        set!(prefix_padding_ms);
        set!(silence_duration_ms);
        if let Some(v) = self.semantic_vad_engine {
            r.semantic_vad_engine = match v.trim() {
                "smart_turn" => SemanticVadEngine::SmartTurn,
                "server_vad" => SemanticVadEngine::ServerVad,
                other => {
                    return Err(format!(
                        "realtime.semantic_vad_engine is 'smart_turn' or 'server_vad', not \
                         '{other}'"
                    ))
                }
            };
            touched.push("realtime.semantic_vad_engine");
        }
        let mut table_touched = false;
        if let Some(t) = self.semantic_vad {
            for (row, p) in [
                (&mut r.semantic_vad.high, t.high),
                (&mut r.semantic_vad.medium, t.medium),
                (&mut r.semantic_vad.low, t.low),
            ] {
                if let Some(p) = p {
                    p.apply(row);
                }
            }
            table_touched = true;
            touched.push("realtime.semantic_vad");
        }
        if let Some(v) = self.semantic_floor_window_ms {
            r.semantic_floor_window_ms = v;
            table_touched = true;
            touched.push("realtime.semantic_floor_window_ms");
        }
        if table_touched {
            let problems = r.semantic_vad.problems(r.semantic_floor_window_ms);
            if !problems.is_empty() {
                return Err(problems.join("; "));
            }
        }
        set!(post_interrupt_silence_ms);
        set!(barge_in_min_ms);
        set!(barge_in_guard_ms);
        set!(half_duplex);
        set!(echo_tail_ms);
        if let Some(v) = self.barge_in_check {
            r.barge_in_check = match v.trim() {
                "words" => BargeInCheck::Words,
                "duration" => BargeInCheck::Duration,
                other => {
                    return Err(format!(
                        "realtime.barge_in_check is 'words' or 'duration', not '{other}'"
                    ))
                }
            };
            touched.push("realtime.barge_in_check");
        }
        if let Some(v) = self.backchannel_words {
            r.backchannel_words = clean_list(v);
            touched.push("realtime.backchannel_words");
        }
        if let Some(v) = self.barge_in_check_scripts {
            let v = clean_list(v);
            // A name the table does not know matches no letter, so the word
            // check would hear nothing said in it — refused here as a
            // client's `session.update` refuses it.
            if let Some(name) = crate::realtime::turn::scripts::unknown(&v) {
                return Err(format!(
                    "realtime.barge_in_check_scripts names '{name}', a script lmgw does not \
                     know; known: {}",
                    crate::realtime::turn::scripts::known().join(", ")
                ));
            }
            r.barge_in_check_scripts = v;
            touched.push("realtime.barge_in_check_scripts");
        }
        set!(barge_in_check_timeout_ms);
        set!(barge_in_check_alias, trim);
        set!(output_lead_ms);
        set!(synthesis_ahead_s);
        set!(longest_pause_ms);
        set!(warm_on_connect);
        let limits = self.max_message_mb.is_some() || self.max_frame_mb.is_some();
        set!(max_message_mb);
        set!(max_frame_mb);
        if limits && r.max_message_mb == 0 && r.max_frame_mb == 0 {
            // tungstenite reserves a frame's declared length before it reads
            // it, so with neither bound one header could abort the process
            // (§10.4) — the loader would fall back to the defaults anyway.
            return Err(
                "realtime.max_message_mb and realtime.max_frame_mb cannot both be 0 — a frame \
                 would then have no bound at all; 0 on one of them leaves it to the other"
                    .into(),
            );
        }
        set!(ping_interval_s);
        Ok(touched)
    }
}

/// A text-to-speech alias must resolve and its capability `task` must answer
/// `/v1/audio/speech` — `tts`, or `vdes` (the predicate a session goes by,
/// [`crate::realtime::is_tts_alias`]) — [`super::validate_stt_alias`]'s twin,
/// worded the same way.
pub async fn validate_tts_alias(state: &SharedState, alias: &str) -> Result<(), String> {
    state
        .snapshot()
        .resolve(alias)
        .map_err(|_| format!("TTS alias '{alias}' does not resolve"))?;
    let task = crate::capabilities::exposed::exposed_entry(state, alias)
        .await
        .and_then(|e| e.capabilities)
        .map(|c| c.task);
    match task.as_deref() {
        Some(t) if crate::capabilities::speech::speaks(t) => Ok(()),
        Some(other) => Err(format!(
            "TTS alias '{alias}' is a '{other}' model; it must be a text-to-speech model (task \
             tts, or vdes for a designed voice)"
        )),
        None => Err(format!(
            "TTS alias '{alias}' has no readable capabilities, so it cannot be confirmed as a \
             text-to-speech model (task tts or vdes)"
        )),
    }
}

/// A chat alias in the sense a realtime session takes one (§5.1,
/// `realtime::resolve::is_chat_alias`), refused in words that say which half
/// failed.
fn validate_chat_alias(state: &SharedState, alias: &str) -> Result<(), String> {
    let snap = state.snapshot();
    if crate::realtime::resolve::is_chat_alias(&snap, alias) {
        return Ok(());
    }
    match snap.resolve(alias) {
        Err(_) => Err(format!("'{alias}' does not resolve")),
        Ok(_) => Err(format!(
            "'{alias}' is an audio or image model, which cannot answer a conversation"
        )),
    }
}

/// The checks that need the gateway, for the settings `touched` names: each
/// alias names a model of the task its stage runs. Empty is always allowed —
/// each one's "none" has a meaning of its own (§12).
pub async fn validate_realtime(
    state: &SharedState,
    r: &RealtimeSettings,
    touched: &[&str],
) -> Result<(), String> {
    let has = |f: &str| touched.contains(&f);
    if has("realtime.default_model") && !r.default_model.is_empty() {
        validate_chat_alias(state, &r.default_model)
            .map_err(|e| format!("realtime.default_model: {e}"))?;
    }
    if has("realtime.asr_alias") && !r.asr_alias.is_empty() {
        super::validate_stt_alias(state, &r.asr_alias)
            .await
            .map_err(|e| format!("realtime.asr_alias: {e}"))?;
    }
    if has("realtime.tts_alias") && !r.tts_alias.is_empty() {
        validate_tts_alias(state, &r.tts_alias)
            .await
            .map_err(|e| format!("realtime.tts_alias: {e}"))?;
    }
    if has("realtime.barge_in_check_alias") && !r.barge_in_check_alias.is_empty() {
        // §6.4: the check transcribes what was said over the answer; a
        // model of any other task cannot (fix package B6).
        if !crate::realtime::asr::is_asr_alias(state, &r.barge_in_check_alias).await {
            let why = super::validate_stt_alias(state, &r.barge_in_check_alias)
                .await
                .err()
                .unwrap_or_default();
            return Err(format!(
                "realtime.barge_in_check_alias must be a speech-to-text (asr) alias, or empty \
                 for the session's own: {why}"
            ));
        }
    }
    if has("realtime.model_map") {
        let snap = state.snapshot();
        for (name, target) in &r.model_map {
            // A name maps a session's model to a chat alias (§5.1) or a
            // transcription model to an ASR alias (§5.2); either serves.
            if crate::realtime::resolve::is_chat_alias(&snap, target)
                || crate::realtime::asr::is_asr_alias(state, target).await
            {
                continue;
            }
            return Err(format!(
                "realtime.model_map[\"{name}\"]: '{target}' is neither a chat alias nor a \
                 speech-to-text (asr) alias this gateway serves"
            ));
        }
    }
    Ok(())
}

/// Apply `p` onto `r` and check what it touched — the one call both save
/// paths make. `r` is left as it was when the patch is refused.
pub async fn apply_realtime(
    state: &SharedState,
    r: &mut RealtimeSettings,
    p: RealtimeSettingsPatch,
) -> Result<Vec<&'static str>, String> {
    let mut next = r.clone();
    let touched = p.apply(&mut next)?;
    validate_realtime(state, &next, &touched).await?;
    *r = next;
    Ok(touched)
}

fn row_view(r: &SemanticVadRow) -> dto::SemanticVadRow {
    dto::SemanticVadRow {
        threshold: r.threshold,
        floor: r.floor,
        max_wait_ms: r.max_wait_ms,
        silence_duration_ms: r.silence_duration_ms,
    }
}

/// The section as the read surfaces report it.
pub fn realtime_view(r: &RealtimeSettings) -> dto::RealtimeSettings {
    dto::RealtimeSettings {
        default_model: r.default_model.clone(),
        model_map: r.model_map.clone(),
        asr_alias: r.asr_alias.clone(),
        tts_alias: r.tts_alias.clone(),
        default_voice: r.default_voice.clone(),
        voice_map: r.voice_map.clone(),
        speech_instructions: r.speech_instructions.clone(),
        tag_hint: r.tag_hint,
        default_instructions: r
            .default_instructions
            .clone()
            .unwrap_or_else(|| DEFAULT_VOICE_INSTRUCTIONS.to_string()),
        default_instructions_builtin: DEFAULT_VOICE_INSTRUCTIONS.to_string(),
        default_instructions_is_builtin: r.default_instructions.is_none(),
        threshold: r.threshold,
        prefix_padding_ms: r.prefix_padding_ms,
        silence_duration_ms: r.silence_duration_ms,
        semantic_vad_engine: match r.semantic_vad_engine {
            SemanticVadEngine::SmartTurn => "smart_turn",
            SemanticVadEngine::ServerVad => "server_vad",
        }
        .to_string(),
        semantic_vad: dto::SemanticVadTable {
            high: row_view(&r.semantic_vad.high),
            medium: row_view(&r.semantic_vad.medium),
            low: row_view(&r.semantic_vad.low),
        },
        semantic_floor_window_ms: r.semantic_floor_window_ms,
        post_interrupt_silence_ms: r.post_interrupt_silence_ms,
        barge_in_min_ms: r.barge_in_min_ms,
        barge_in_guard_ms: r.barge_in_guard_ms,
        half_duplex: r.half_duplex,
        echo_tail_ms: r.echo_tail_ms,
        barge_in_check: match r.barge_in_check {
            BargeInCheck::Words => "words",
            BargeInCheck::Duration => "duration",
        }
        .to_string(),
        backchannel_words: r.backchannel_words.clone(),
        barge_in_check_scripts: r.barge_in_check_scripts.clone(),
        barge_in_check_timeout_ms: r.barge_in_check_timeout_ms,
        barge_in_check_alias: r.barge_in_check_alias.clone(),
        output_lead_ms: r.output_lead_ms,
        synthesis_ahead_s: r.synthesis_ahead_s,
        longest_pause_ms: r.longest_pause_ms,
        warm_on_connect: r.warm_on_connect,
        max_message_mb: r.max_message_mb,
        max_frame_mb: r.max_frame_mb,
        ping_interval_s: r.ping_interval_s,
    }
}

#[cfg(test)]
mod tests;
