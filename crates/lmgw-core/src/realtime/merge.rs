//! The session a connection starts with, and `session.update` (realtime
//! design §2.2, §5.4).
//!
//! An update is a **partial** session merged into the one in effect, and the
//! answer is always the full session. The merge happens on JSON rather than on
//! the typed [`Session`], because only JSON can tell an absent key ("leave it")
//! from an explicit `null` ("clear it" — `turn_detection: null` is manual
//! turns) — and the typed session is rebuilt from the result, so every shape
//! rule of the protocol types still applies to what comes out.
//!
//! **An update is all or nothing.** A shape error, an unsupported value or a
//! strict-`lmgw` violation leaves the session exactly as it was and answers
//! one `error`, as OpenAI does; half an update would leave the client
//! believing in a configuration that is not the one in effect.

use serde_json::{Map, Value};

use super::protocol::{
    AudioConfig, AudioFormat, AudioInput, AudioOutput, Eagerness, ErrorObject, Inf, LmgwExt,
    MaxOutputTokens, Modality, Resolved, Session, SessionType, ToolChoice, ToolChoiceMode,
    TurnDetection, Voice, PCM_RATE,
};
use crate::config::RealtimeSettings;

/// The voice a session asks for when the client names none: an OpenAI
/// built-in name, so it resolves through `realtime.default_voice` and the TTS
/// row's default preset unless the model has a voice of that name (§5.3).
/// What speaks is `lmgw.resolved.voice`.
pub const DEFAULT_VOICE: &str = "marin";

/// The `object` of an echoed session.
pub const SESSION_OBJECT: &str = "realtime.session";

/// The session `session.created` announces: the protocol's defaults (§2.2),
/// `server_vad` from the settings (§6.2), and the owner's default
/// instructions.
pub fn initial_session(settings: &RealtimeSettings, id: String, model: Option<String>) -> Session {
    let mut s = Session {
        kind: SessionType::Realtime,
        object: None,
        id: Some(id),
        model,
        instructions: None,
        output_modalities: None,
        audio: Some(AudioConfig {
            input: Some(AudioInput {
                turn_detection: Some(TurnDetection::ServerVad {
                    threshold: None,
                    prefix_padding_ms: None,
                    silence_duration_ms: None,
                    create_response: None,
                    interrupt_response: None,
                    idle_timeout_ms: None,
                }),
                ..Default::default()
            }),
            output: None,
        }),
        tools: None,
        tool_choice: None,
        parallel_tool_calls: None,
        max_output_tokens: None,
        reasoning: None,
        tracing: None,
        truncation: None,
        prompt: None,
        include: None,
        lmgw: None,
    };
    normalize(&mut s, settings);
    s
}

/// Merge one `session.update` into `current`.
///
/// Model *resolution* is not done here: the caller compares `model` before
/// and after and re-resolves (and re-runs the policy check, §10.3) when it
/// changed, then writes `lmgw.resolved.chat` itself.
pub fn apply_update(
    current: &Session,
    update: &Map<String, Value>,
    settings: &RealtimeSettings,
) -> Result<Session, ErrorObject> {
    check_type(update)?;
    let mut patch = update.clone();
    // Server-owned keys: a client that sends back the session it was given
    // carries them, and they are not the client's to change.
    patch.remove("id");
    patch.remove("object");
    if let Some(lmgw) = patch.get_mut("lmgw") {
        strict_lmgw(lmgw)?;
    }

    let mut merged = serde_json::to_value(current).map_err(|e| ErrorObject {
        kind: "server_error".into(),
        ..ErrorObject::invalid("internal", format!("session did not serialize: {e}"))
    })?;
    deep_merge(&mut merged, &Value::Object(patch));

    let mut next: Session = serde_json::from_value(merged).map_err(|e| {
        ErrorObject::invalid("invalid_value", format!("session.update: {e}")).with_param("session")
    })?;
    validate(&next)?;
    // The server's own resolution survives the client's merge untouched.
    let resolved = current.lmgw.as_ref().and_then(|l| l.resolved.clone());
    next.lmgw.get_or_insert_with(LmgwExt::default).resolved = resolved;
    normalize(&mut next, settings);
    Ok(next)
}

/// JSON merge with one rule beyond objects-merge-and-everything-else-replaces:
/// **two objects whose `type` differs are a different variant of a tagged
/// union, and the newer one replaces the older whole.** Without it, a switch
/// from `server_vad` to `semantic_vad` would carry `threshold` and
/// `silence_duration_ms` into an object that has no such fields, and
/// `audio/pcm` → `audio/pcmu` would keep a `rate` G.711 does not have.
/// Arrays replace (an explicit `tools: []` clears the tools, §2.2).
pub fn deep_merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) if !retyped(b, p) => {
            for (k, v) in p {
                match b.get_mut(k) {
                    Some(slot) => deep_merge(slot, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

fn retyped(base: &Map<String, Value>, patch: &Map<String, Value>) -> bool {
    matches!(
        (base.get("type"), patch.get("type")),
        (Some(a), Some(b)) if a != b
    )
}

/// `session.type` must be present and must be `realtime` (§2.2).
fn check_type(update: &Map<String, Value>) -> Result<(), ErrorObject> {
    match update.get("type") {
        None => Err(ErrorObject::invalid(
            "missing_required_parameter",
            "session.update must carry session.type (\"realtime\")",
        )
        .with_param("session.type")),
        Some(Value::String(t)) if t == "realtime" => Ok(()),
        Some(Value::String(t)) if t == "transcription" => Err(ErrorObject::invalid(
            "invalid_value",
            "transcription sessions are not served by lmgw yet; use session.type \"realtime\"",
        )
        .with_param("session.type")),
        Some(other) => Err(ErrorObject::invalid(
            "invalid_value",
            format!("session.type must be \"realtime\", got {other}"),
        )
        .with_param("session.type")),
    }
}

/// `session.lmgw` is strict (§5.4): unknown keys are an error. `resolved` is
/// accepted and dropped — it is the server's to write.
fn strict_lmgw(lmgw: &mut Value) -> Result<(), ErrorObject> {
    match lmgw {
        // `null` resets the knobs to the settings' defaults.
        Value::Null => Ok(()),
        Value::Object(m) => {
            m.remove("resolved");
            let ext = serde_json::from_value::<LmgwExt>(Value::Object(m.clone())).map_err(|e| {
                ErrorObject::invalid("invalid_value", format!("session.lmgw: {e}"))
                    .with_param("session.lmgw")
            })?;
            // Judged on what the client sent, not on the merged session: a
            // name in the owner's setting the table does not know is the
            // owner's to fix (a WARN at session start), not a reason to
            // refuse the client's update.
            let scripts = ext.barge_in_check_scripts.unwrap_or_default();
            match crate::realtime::turn::scripts::unknown(&scripts) {
                Some(name) => Err(ErrorObject::invalid(
                    "invalid_value",
                    format!(
                        "barge_in_check_scripts names '{name}', a script lmgw does not know; \
                         known: {}",
                        crate::realtime::turn::scripts::known().join(", ")
                    ),
                )
                .with_param("session.lmgw.barge_in_check_scripts")),
                None => Ok(()),
            }
        }
        other => Err(ErrorObject::invalid(
            "invalid_value",
            format!("session.lmgw must be an object, got {other}"),
        )
        .with_param("session.lmgw")),
    }
}

/// What the cascade can serve, checked after the merge so the error names
/// the final value rather than a fragment of the update.
fn validate(s: &Session) -> Result<(), ErrorObject> {
    if s.kind != SessionType::Realtime {
        return Err(ErrorObject::invalid(
            "invalid_value",
            "transcription sessions are not served by lmgw yet",
        )
        .with_param("session.type"));
    }
    let audio = s.audio.as_ref();
    for (param, format) in [
        (
            "session.audio.input.format",
            audio.and_then(|a| a.input.as_ref()).and_then(|i| i.format),
        ),
        (
            "session.audio.output.format",
            audio.and_then(|a| a.output.as_ref()).and_then(|o| o.format),
        ),
    ] {
        match format {
            None | Some(AudioFormat::Pcm { rate: PCM_RATE }) => {}
            Some(AudioFormat::Pcm { rate }) => {
                return Err(ErrorObject::invalid(
                    "invalid_value",
                    format!("audio/pcm is served at {PCM_RATE} Hz only, not {rate}"),
                )
                .with_param(param))
            }
            Some(_) => {
                return Err(ErrorObject::invalid(
                    "invalid_value",
                    "lmgw serves audio/pcm only; G.711 (audio/pcmu, audio/pcma) is not supported",
                )
                .with_param(param))
            }
        }
    }
    if let Some(m) = &s.output_modalities {
        // "It is not possible to request both text and audio at the same
        // time" — the GA reference's own rule.
        if m.len() != 1 {
            return Err(ErrorObject::invalid(
                "invalid_value",
                "output_modalities must be exactly one of [\"audio\"] or [\"text\"]",
            )
            .with_param("session.output_modalities"));
        }
    }
    if let Some(TurnDetection::ServerVad {
        threshold: Some(t), ..
    }) = audio
        .and_then(|a| a.input.as_ref())
        .and_then(|i| i.turn_detection.as_ref())
    {
        if !(0.0..=1.0).contains(t) {
            return Err(ErrorObject::invalid(
                "invalid_value",
                format!("turn_detection.threshold is a speech probability, 0.0 to 1.0, not {t}"),
            )
            .with_param("session.audio.input.turn_detection.threshold"));
        }
    }
    check_speed(
        audio.and_then(|a| a.output.as_ref()).and_then(|o| o.speed),
        "session.audio.output.speed",
    )
}

/// The GA range of `audio.output.speed` (package B review 10).
pub const SPEED: std::ops::RangeInclusive<f64> = 0.25..=1.5;

/// `audio.output.speed`, if given, inside [`SPEED`] — the session's or a
/// `response.create`'s, named by `param`.
pub fn check_speed(speed: Option<f64>, param: &str) -> Result<(), ErrorObject> {
    match speed {
        Some(s) if !SPEED.contains(&s) => Err(ErrorObject::invalid(
            "invalid_value",
            format!(
                "audio.output.speed is {} to {}, not {s}",
                SPEED.start(),
                SPEED.end()
            ),
        )
        .with_param(param)),
        _ => Ok(()),
    }
}

/// Fill every default, so the echoed session is the configuration in effect.
pub fn normalize(s: &mut Session, settings: &RealtimeSettings) {
    s.object = Some(SESSION_OBJECT.into());
    let speaks = s
        .output_modalities
        .get_or_insert_with(|| vec![Modality::Audio])
        .contains(&Modality::Audio);
    // The owner's default stands in while the client says nothing — absent
    // or empty — in a session that speaks: it asks for answers made to be
    // heard, which a text session does not want (package B review 8). The
    // echo shows what is in effect (§2.2, §23 L8). Instructions equal to
    // that default count as it, wherever they came from (B2 review 6): the
    // session's own echo, or a client sending back the session it was
    // given — so a switch to text output drops them, and back gets them.
    let default = settings.voice_instructions();
    if s.instructions
        .as_deref()
        .is_none_or(|i| i.trim().is_empty() || Some(i) == default)
    {
        s.instructions = Some(default.filter(|_| speaks).unwrap_or_default().to_string());
    }
    s.tools.get_or_insert_with(Vec::new);
    s.tool_choice
        .get_or_insert(ToolChoice::Mode(ToolChoiceMode::Auto));
    s.max_output_tokens
        .get_or_insert(MaxOutputTokens::Inf(Inf::Inf));

    let pcm = AudioFormat::Pcm { rate: PCM_RATE };
    let audio = s.audio.get_or_insert_with(Default::default);
    let input = audio.input.get_or_insert_with(Default::default);
    input.format.get_or_insert(pcm);
    match &mut input.turn_detection {
        Some(TurnDetection::ServerVad {
            threshold,
            prefix_padding_ms,
            silence_duration_ms,
            create_response,
            interrupt_response,
            ..
        }) => {
            threshold.get_or_insert(settings.threshold);
            prefix_padding_ms.get_or_insert(settings.prefix_padding_ms);
            silence_duration_ms.get_or_insert(settings.silence_duration_ms);
            create_response.get_or_insert(true);
            interrupt_response.get_or_insert(true);
        }
        Some(TurnDetection::SemanticVad {
            eagerness,
            create_response,
            interrupt_response,
        }) => {
            eagerness.get_or_insert(Eagerness::Auto);
            create_response.get_or_insert(true);
            interrupt_response.get_or_insert(true);
        }
        None => {}
    }
    let output: &mut AudioOutput = audio.output.get_or_insert_with(Default::default);
    output.format.get_or_insert(pcm);
    output
        .voice
        .get_or_insert_with(|| Voice::Name(DEFAULT_VOICE.into()));
    output.speed.get_or_insert(1.0);

    // `semantic_vad` runs Smart Turn unless the owner chose the escape
    // hatch, and the echo says which (§6.3).
    let semantic = super::input::semantic_rule(s, settings);
    let detector = s
        .audio
        .as_ref()
        .and_then(|a| a.input.as_ref())
        .and_then(|i| i.turn_detection.as_ref())
        .map(|td| match td {
            TurnDetection::SemanticVad { .. } if semantic.is_some() => "semantic_vad".to_string(),
            _ => "server_vad".to_string(),
        });
    let lmgw = s.lmgw.get_or_insert_with(LmgwExt::default);
    // Every knob echoes the default in effect.
    lmgw.barge_in_min_ms.get_or_insert(settings.barge_in_min_ms);
    lmgw.barge_in_guard_ms
        .get_or_insert(settings.barge_in_guard_ms);
    lmgw.half_duplex.get_or_insert(settings.half_duplex);
    lmgw.echo_tail_ms.get_or_insert(settings.echo_tail_ms);
    lmgw.barge_in_check.get_or_insert(settings.barge_in_check);
    lmgw.barge_in_check_timeout_ms
        .get_or_insert(settings.barge_in_check_timeout_ms);
    lmgw.barge_in_check_scripts
        .get_or_insert_with(|| settings.barge_in_check_scripts.clone());
    lmgw.barge_in_check_alias
        .get_or_insert_with(|| settings.barge_in_check_alias.clone());
    lmgw.post_interrupt_silence_ms
        .get_or_insert(settings.post_interrupt_silence_ms);
    lmgw.output_lead_ms.get_or_insert(settings.output_lead_ms);
    lmgw.synthesis_ahead_s
        .get_or_insert(settings.synthesis_ahead_s);
    lmgw.longest_pause_ms
        .get_or_insert(settings.longest_pause_ms);
    // Like `half_duplex`: the setting in effect. `speech_instructions` and
    // `speech_seed` are echoed only as the client set them, as `tts_model`
    // is — what is in effect is `resolved.speech` (WP10 D3).
    lmgw.tag_hint.get_or_insert(settings.tag_hint);
    let resolved = lmgw.resolved.get_or_insert_with(Resolved::default);
    resolved.turn_detection = detector;
    resolved.semantic_vad = semantic;
    resolved.truncation = Some("disabled".into());
}
