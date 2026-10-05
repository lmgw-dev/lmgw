//! A thread's voice overrides in its settings patch, and a folder's voice
//! defaults (chat-voice design §2.2).
//!
//! `POST /chat/api/threads/{id}/settings` takes `voice` as a **whole
//! object**: what it names is the thread's voice from then on, a `null` (or
//! absent) field inherits, and `voice: null` clears every override. Input is
//! strict — an unknown key or a wrong type is a 400 naming it. One exception
//! keeps a form from erasing what it never showed: the **seed** is drawn by
//! the server on first use (§6.1), so an object without a `seed` key keeps
//! the stored one; `"seed": null` clears it, and a number sets it. "The
//! stored one" is the one stored when the write lands
//! ([`SeedWrite::Keep`]): a seed drawn while this request was checking its
//! aliases survives it.
//!
//! The aliases are checked as Settings → Chat → Voice checks its own (an ASR
//! alias of task `asr`, a TTS alias of task `tts` or `vdes`), but only when
//! they change: a model deleted since must not block saving the rest.

use serde_json::Value;

use crate::state::SharedState;
use crate::store::{ChatThread, SeedWrite, ThreadVoice};

/// Lay the settings patch's `voice` over `t` (module doc). `None`: the
/// patch did not name it. What the write must do with the seed comes back:
/// [`SeedWrite::AsGiven`] only when the patch named it. `Err` is the 400's
/// message; `t` is then as it was.
pub(crate) async fn apply_thread_voice(
    state: &SharedState,
    t: &mut ChatThread,
    voice: Option<Value>,
) -> Result<SeedWrite, String> {
    let Some(v) = voice else {
        return Ok(SeedWrite::Keep);
    };
    let (mut next, seed) = match v {
        Value::Null => (
            ThreadVoice {
                seed: t.voice.seed,
                ..Default::default()
            },
            SeedWrite::Keep,
        ),
        Value::Object(map) => {
            let names_seed = map.contains_key("seed");
            let mut next: ThreadVoice =
                serde_json::from_value(Value::Object(map)).map_err(|e| format!("voice: {e}"))?;
            if names_seed {
                (next, SeedWrite::AsGiven)
            } else {
                next.seed = t.voice.seed;
                (next, SeedWrite::Keep)
            }
        }
        other => return Err(format!("voice must be an object or null, not {other}")),
    };
    next.normalise()?;
    check_voice_aliases(state, &next, Some(&t.voice)).await?;
    t.voice = next;
    Ok(seed)
}

/// The aliases of `v` name models of their stage's task. With `before`, an
/// alias equal to the one there is not checked again.
pub(crate) async fn check_voice_aliases(
    state: &SharedState,
    v: &ThreadVoice,
    before: Option<&ThreadVoice>,
) -> Result<(), String> {
    let changed = |now: &Option<String>, was: Option<&Option<String>>| match now {
        Some(a) if was.is_none_or(|w| w.as_deref() != Some(a.as_str())) => Some(a.clone()),
        _ => None,
    };
    if let Some(a) = changed(&v.asr_alias, before.map(|b| &b.asr_alias)) {
        crate::ops::validate_stt_alias(state, &a)
            .await
            .map_err(|e| format!("voice.asr_alias: {e}"))?;
    }
    if let Some(a) = changed(&v.tts_alias, before.map(|b| &b.tts_alias)) {
        crate::ops::validate_tts_alias(state, &a)
            .await
            .map_err(|e| format!("voice.tts_alias: {e}"))?;
    }
    Ok(())
}
