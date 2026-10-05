//! Which TTS alias speaks a session's answers (realtime design §5.3, §5.4).
//!
//! The protocol has a voice field but no TTS-model field, so the alias is
//! lmgw's: `session.lmgw.tts_model`, else the setting `realtime.tts_alias`.
//! Either must name an alias whose capability task answers
//! `/v1/audio/speech` ([`is_tts_alias`]): `tts`, or `vdes` — a voice designed
//! from the session's speech instructions (WP10). A client's name that is an
//! alias of another task is refused `not_a_tts_alias`, naming its task (live
//! run 3 D1); one that is no alias, `unknown_alias`. Resolving it starts
//! nothing and ignores the GPU hold: each response opens its route itself
//! (§9.1, §9.2).
//!
//! With neither, the session still opens — a text session never speaks — and
//! an audio response fails before `response.created` with
//! `tts_not_configured`, carrying the reason recorded here. A *broken*
//! setting (one naming no TTS model) is reported in that reason rather than
//! skipped, as for ASR (§5.2). Every resolution is echoed in
//! `lmgw.resolved.tts` and logged.

use crate::capabilities::speech::speaks;
use crate::error::GatewayError;
use crate::state::SharedState;

/// What a session's TTS alias resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtsResolution {
    /// The TTS alias, or `None`: an audio response says why (`missing`).
    pub alias: Option<String>,
    /// Which knob named it: `session.lmgw.tts_model` or the setting.
    pub via: TtsVia,
    /// Why `alias` is `None`, in a sentence the `tts_not_configured` error
    /// carries.
    pub missing: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtsVia {
    /// `session.lmgw.tts_model`.
    Session,
    /// `realtime.tts_alias` — or nothing, when `alias` is `None`.
    Setting,
}

impl TtsResolution {
    fn missing(why: String) -> Self {
        Self {
            alias: None,
            via: TtsVia::Setting,
            missing: Some(why),
        }
    }
}

/// Resolve `requested`, the session's `lmgw.tts_model` (`None` when it names
/// none). An `Err` is a name the client sent that is not a TTS alias; a
/// missing setting is not an error (module doc).
pub async fn resolve_tts(
    state: &SharedState,
    requested: Option<&str>,
) -> Result<TtsResolution, GatewayError> {
    if let Some(name) = requested.map(str::trim).filter(|n| !n.is_empty()) {
        match alias_task(state, name).await {
            None => {
                return Err(GatewayError::UnknownAlias(format!(
                    "{name} (session.lmgw.tts_model must name a text-to-speech alias this \
                     gateway serves: task tts, or vdes for a designed voice)"
                )))
            }
            Some(task) if !task.as_deref().is_some_and(speaks) => {
                return Err(wrong_task(
                    "not_a_tts_alias",
                    &format!("'{name}'"),
                    task.as_deref(),
                    "a text-to-speech model",
                    "session.lmgw.tts_model must name an alias of task tts, or vdes for a \
                     designed voice",
                ))
            }
            Some(_) => {}
        }
        return Ok(TtsResolution {
            alias: Some(name.to_string()),
            via: TtsVia::Session,
            missing: None,
        });
    }
    let setting = state
        .snapshot()
        .settings
        .realtime
        .tts_alias
        .trim()
        .to_string();
    if setting.is_empty() {
        return Ok(TtsResolution::missing(
            "no text-to-speech alias is configured: set realtime.tts_alias, or name one in \
             session.lmgw.tts_model — or ask for output_modalities [\"text\"]"
                .into(),
        ));
    }
    if !is_tts_alias(state, &setting).await {
        return Ok(TtsResolution::missing(format!(
            "the setting realtime.tts_alias names '{setting}', which is not a text-to-speech \
             alias this gateway serves (task tts, or vdes for a designed voice)"
        )));
    }
    Ok(TtsResolution {
        alias: Some(setting),
        via: TtsVia::Setting,
        missing: None,
    })
}

/// A name that resolves and whose capability task answers
/// `/v1/audio/speech` ([`crate::capabilities::speech::speaks`]: `tts` or
/// `vdes`) — the one predicate for a session's TTS alias, the setting's
/// save-time check and `/v1/models`' realtime advertising. A cloud model's
/// task is its name's when its catalog says nothing
/// ([`crate::capabilities::task::by_name`]).
pub async fn is_tts_alias(state: &SharedState, name: &str) -> bool {
    alias_task(state, name)
        .await
        .flatten()
        .as_deref()
        .is_some_and(speaks)
}

/// `name`'s capability task: `None` when the name is no model this gateway
/// serves (it does not resolve, or is not exposed — a passthrough name its
/// upstream's catalog does not list), `Some(None)` when it is one but its
/// capabilities cannot be read.
pub(super) async fn alias_task(state: &SharedState, name: &str) -> Option<Option<String>> {
    state.snapshot().resolve(name).ok()?;
    let entry = crate::capabilities::exposed::exposed_entry(state, name).await?;
    Some(entry.capabilities.map(|c| c.task))
}

/// The refusal of `who`, an alias that exists with the wrong task (live run
/// 3 D1): its own `code`, and the task it has — `unknown_alias` said the
/// alias did not exist. `want` is what it must be, `rule` the knob's rule.
pub(super) fn wrong_task(
    code: &'static str,
    who: &str,
    task: Option<&str>,
    want: &str,
    rule: &str,
) -> GatewayError {
    let message = match task {
        Some(task) => format!("{who} is a '{task}' model, not {want}: {rule}"),
        None => format!(
            "{who} has no readable capabilities, so it cannot be confirmed as {want}: {rule}"
        ),
    };
    GatewayError::InvalidRequest { code, message }
}

/// The log line for a resolution (§5.1: every substitution is logged).
pub fn log_resolution(session_id: &str, r: &TtsResolution) {
    match (&r.alias, r.via) {
        (None, _) => tracing::info!(
            "realtime {session_id}: no TTS alias — {}; audio responses will fail before they \
             start",
            r.missing.as_deref().unwrap_or("none configured")
        ),
        (Some(a), TtsVia::Session) => {
            tracing::info!("realtime {session_id}: TTS alias '{a}' (session.lmgw.tts_model)")
        }
        (Some(a), TtsVia::Setting) => {
            tracing::info!("realtime {session_id}: TTS alias '{a}' (realtime.tts_alias)")
        }
    }
}
