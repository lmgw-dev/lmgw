//! Which ASR alias transcribes a session's turns (realtime design §5.2).
//!
//! The cascade always transcribes — the chat model needs text — so a session
//! has an ASR alias whether or not it asked for transcription events;
//! `audio.input.transcription` only decides whether those events are sent.
//! Its `model` is resolved the way the chat model is (§5.1), against
//! ASR-capable aliases:
//! 1. a name in `realtime.model_map`, the mapped alias — the owner's explicit
//!    intent wins;
//! 2. an OpenAI transcription name (`gpt-4o*-transcribe*`, `whisper-1`, what
//!    the stock SDKs send — `@openai/agents` asks for
//!    `gpt-4o-mini-transcribe`): the configured default below, never the
//!    name itself, for the reason §5.1 gives for Realtime names;
//! 3. an ASR alias, used as is;
//! 4. anything else is an `error` on the `session.update` that named it —
//!    `not_an_asr_alias`, naming its task, for an alias of another task (a
//!    `model_map` target too; live run 3 D1), `unknown_alias` for a name
//!    that is none.
//!
//! With no name — or an OpenAI one — the default is `realtime.asr_alias`,
//! then the Chat's `chat_stt_alias`. With neither, the session still opens:
//! a text session never needs ASR, and an audio session is told at its first
//! commit (`asr_not_configured`), with the reason this resolution recorded.
//!
//! "ASR-capable" is what `chat_stt_alias` itself must be
//! (`ops::validate_stt_alias`): a name that resolves, whose capability task
//! is `asr` — for a cloud model whose catalog says nothing, its name's
//! (`openai/gpt-4o-mini-transcribe`, `capabilities::task::by_name`).
//! Resolving starts nothing and ignores the GPU hold — each ASR call routes
//! around the hold itself (§9.2). Every substitution is echoed in
//! `lmgw.resolved.asr` and logged.

use super::tts::{alias_task, wrong_task};
use crate::error::GatewayError;
use crate::state::SharedState;

/// The code of a transcription model that is an alias of another task (live
/// run 3 D1).
const NOT_ASR: &str = "not_an_asr_alias";

/// What a session's ASR alias resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsrResolution {
    /// The ASR alias, or `None`: nothing is configured, and the first commit
    /// says so.
    pub alias: Option<String>,
    /// Which rule produced it.
    pub via: AsrVia,
    /// Why `alias` is `None`, in a sentence the `asr_not_configured` error
    /// carries — or a broken default setting, named.
    pub missing: Option<String>,
}

/// The rule of §5.2 that resolved the transcription model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrVia {
    /// `realtime.model_map`.
    ModelMap,
    /// The name is itself an ASR alias.
    Alias,
    /// The default chain, for no name or an OpenAI transcription name: the
    /// setting that answered (`realtime.asr_alias`, then `chat_stt_alias`),
    /// or `None` when neither did.
    Default(Option<&'static str>),
}

impl AsrResolution {
    fn missing(why: String) -> Self {
        Self {
            alias: None,
            via: AsrVia::Default(None),
            missing: Some(why),
        }
    }
}

/// Resolve `requested`, the session's `audio.input.transcription.model`
/// (`None` when it names none). An `Err` is a name the client sent that
/// cannot be used; a missing default is not an error (module doc).
pub async fn resolve_asr(
    state: &SharedState,
    requested: Option<&str>,
) -> Result<AsrResolution, GatewayError> {
    let name = requested.map(str::trim).filter(|n| !n.is_empty());
    let snap = state.snapshot();
    let rt = &snap.settings.realtime;
    if let Some(name) = name {
        if let Some(mapped) = rt.model_map.get(name) {
            if !is_asr_alias(state, mapped).await {
                return Err(match alias_task(state, mapped).await {
                    None => GatewayError::UnknownAlias(format!(
                        "{mapped} (named by the setting realtime.model_map[\"{name}\"], and not \
                         a speech-to-text alias this gateway serves)"
                    )),
                    Some(task) => wrong_task(
                        NOT_ASR,
                        &format!(
                            "'{mapped}' (named by the setting realtime.model_map[\"{name}\"])"
                        ),
                        task.as_deref(),
                        "a speech-to-text model",
                        "fix the setting realtime.model_map",
                    ),
                });
            }
            return Ok(AsrResolution {
                alias: Some(mapped.clone()),
                via: AsrVia::ModelMap,
                missing: None,
            });
        }
        if !is_openai_transcription_name(name) {
            if is_asr_alias(state, name).await {
                return Ok(AsrResolution {
                    alias: Some(name.to_string()),
                    via: AsrVia::Alias,
                    missing: None,
                });
            }
            const RULE: &str = "audio.input.transcription.model must name a speech-to-text \
                                alias (task asr), a realtime.model_map entry, or an OpenAI \
                                transcription model";
            return Err(match alias_task(state, name).await {
                None => GatewayError::UnknownAlias(format!("{name} ({RULE})")),
                Some(task) => wrong_task(
                    NOT_ASR,
                    &format!("'{name}'"),
                    task.as_deref(),
                    "a speech-to-text model",
                    RULE,
                ),
            });
        }
    }
    Ok(default_chain(state).await)
}

/// `realtime.asr_alias`, then `chat_stt_alias`. A setting that names
/// something unusable is reported, not skipped: falling through to the next
/// one would quietly answer with a model the owner did not pick for voice.
async fn default_chain(state: &SharedState) -> AsrResolution {
    let snap = state.snapshot();
    for (setting, value) in [
        (
            "realtime.asr_alias",
            snap.settings.realtime.asr_alias.trim(),
        ),
        ("chat_stt_alias", snap.settings.chat_stt_alias.trim()),
    ] {
        if value.is_empty() {
            continue;
        }
        if is_asr_alias(state, value).await {
            return AsrResolution {
                alias: Some(value.to_string()),
                via: AsrVia::Default(Some(setting)),
                missing: None,
            };
        }
        return AsrResolution::missing(format!(
            "the setting {setting} names '{value}', which is not a speech-to-text (asr) alias \
             this gateway serves"
        ));
    }
    AsrResolution::missing(
        "no speech-to-text alias is configured: set realtime.asr_alias (or the Chat's \
         speech-to-text alias), or name one in audio.input.transcription.model"
            .into(),
    )
}

/// `gpt-4o*-transcribe*` and `whisper-1` (§5.2).
pub fn is_openai_transcription_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "whisper-1" || (n.starts_with("gpt-4o") && n.contains("-transcribe"))
}

/// A name that resolves and whose capability task is `asr` — what
/// `chat_stt_alias` must be, and the route `/v1/audio/transcriptions` serves.
pub async fn is_asr_alias(state: &SharedState, name: &str) -> bool {
    crate::ops::validate_stt_alias(state, name).await.is_ok()
}

/// The log line for a resolution (§5.1: every substitution is logged).
pub fn log_resolution(session_id: &str, requested: Option<&str>, r: &AsrResolution) {
    let Some(a) = &r.alias else {
        return tracing::info!(
            "realtime {session_id}: no ASR alias — {}; audio turns will fail at their commit",
            r.missing.as_deref().unwrap_or("none configured")
        );
    };
    let setting = match r.via {
        AsrVia::Default(Some(s)) => s,
        _ => "?",
    };
    match (r.via, requested) {
        (AsrVia::Alias, _) => tracing::info!("realtime {session_id}: ASR alias '{a}'"),
        (AsrVia::ModelMap, name) => tracing::info!(
            "realtime {session_id}: transcription model '{}' → '{a}' (realtime.model_map)",
            name.unwrap_or_default()
        ),
        (_, Some(name)) => tracing::info!(
            "realtime {session_id}: transcription model '{name}' is an OpenAI name → '{a}' \
             ({setting})"
        ),
        (_, None) => tracing::info!(
            "realtime {session_id}: no transcription model named → '{a}' ({setting})"
        ),
    }
}
