//! Chat voice, the store's half (chat-voice design §2.2, §3): a thread's
//! voice overrides ([`ThreadVoice`], `chat_threads.voice`) and what a spoken
//! turn leaves on its message ([`MessageVoice`], `chat_messages.voice`). No
//! audio is stored anywhere; a spoken turn is kept as text.
//!
//! Both are JSON columns. **Input is strict** — an unknown key or a wrong
//! type is refused by name — and **what is stored is read tolerantly**: a
//! key this build cannot read (one a newer build wrote, or a value it
//! changed) is dropped on its own, so the rest of the object still applies.
//! That matters most where a [`ThreadVoice`] sits inside a folder's
//! [`ThreadDefaults`](super::ThreadDefaults): a failed nested read used to
//! wipe every default of the folder
//! ([`ThreadDefaults::from_stored`](super::ThreadDefaults::from_stored)).

mod spoken;
pub use spoken::{cut_chat_reply, set_chat_message_voice};

mod audio_input;
pub use audio_input::{AudioInputMode, InputPath};

mod languages;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// How realtime mode detects the end of a turn (§2.1, `chat_turn_detection`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnDetection {
    /// Smart Turn decides when the speaker is done (realtime's
    /// `semantic_vad`).
    #[default]
    SemanticVad,
    /// Silence windows only (realtime's `server_vad`).
    ServerVad,
    /// A turn is what is spoken while the key is held.
    PushToTalk,
}

impl TurnDetection {
    /// Every value, as the setting and the thread override spell it.
    pub const NAMES: [&'static str; 3] = ["semantic_vad", "server_vad", "push_to_talk"];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SemanticVad => "semantic_vad",
            Self::ServerVad => "server_vad",
            Self::PushToTalk => "push_to_talk",
        }
    }

    /// The setting's text (trimmed, any case); anything else is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "semantic_vad" => Some(Self::SemanticVad),
            "server_vad" => Some(Self::ServerVad),
            "push_to_talk" => Some(Self::PushToTalk),
            _ => None,
        }
    }
}

/// A thread's own voice settings (§2.2). Every field is optional: absent
/// inherits Settings → Chat → Voice, then realtime's settings
/// (`web::chat_voice::resolve`).
///
/// Input is strict (`deny_unknown_fields`); stored values go through
/// [`ThreadVoice::from_stored`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThreadVoice {
    /// The speech-to-text alias for dictation, realtime mode and this
    /// thread's audio attachments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_alias: Option<String>,
    /// The text-to-speech alias for read-aloud and realtime mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts_alias: Option<String>,
    /// A voice of that TTS model, resolved by realtime's chain (realtime
    /// §5.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// The language the user speaks, an ISO 639-1 code: what the
    /// speech-to-text model is told where it takes one, and what the prompt
    /// says the user speaks (chat-voice design §2.1, split 2026-10-05) — or
    /// `auto`, none whatever Settings say: the ASR detects
    /// ([`FOLLOW_USER`]). Replies follow it while no reply language is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// The language replies are in, an ISO 639-1 code: what the model is
    /// asked to answer in and the text-to-speech model speaks (§2.1, added
    /// 2026-10-05) — or `auto`: not Settings' reply language, the thread's
    /// spoken language as resolved instead. Empty inherits Settings → Chat
    /// → Voice, which falls back to the spoken language too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_language: Option<String>,
    /// Read every reply aloud as it streams.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_aloud: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_detection: Option<TurnDetection>,
    /// Whether a voice turn may go to the chat model as audio
    /// (voice-audio-input design §2.1): `off` or `on` (a stored `local`,
    /// its name before 2026-10-06, reads as `on`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_input: Option<AudioInputMode>,
    /// The TTS's speech instructions, as `session.lmgw.speech_instructions`.
    /// `Some("")` is "none for this thread", which differs from absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speech_style: Option<String>,
    /// The thread's TTS seed (§6.1), so one thread keeps one voice on a TTS
    /// that designs or draws its voice. Drawn and stored on first use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u32>,
}

impl ThreadVoice {
    /// Read `chat_threads.voice` tolerantly: text that is not a JSON object
    /// is no overrides at all, and a key this build cannot read is dropped
    /// on its own.
    pub fn from_stored(text: &str) -> Self {
        serde_json::from_str::<Value>(text)
            .ok()
            .and_then(Self::from_value)
            .unwrap_or_default()
    }

    /// [`Self::from_stored`] for a value already parsed — the `voice` inside
    /// a folder's defaults. `None` for anything but an object (`null`
    /// included). An `audio_input` of `local`, the value's name before
    /// 2026-10-06, reads as `on`: the derive stays strict, so the API
    /// refuses `local` on input.
    pub fn from_value(v: Value) -> Option<Self> {
        match v {
            Value::Object(mut map) => {
                if map.get("audio_input").and_then(Value::as_str) == Some(audio_input::OLD_ON) {
                    map.insert("audio_input".into(), AudioInputMode::On.as_str().into());
                }
                Some(keep_parsable(map))
            }
            _ => None,
        }
    }

    /// The stored text.
    pub fn to_stored(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }

    /// No field set.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Lay the set fields of `over` onto `self` (a folder's defaults onto a
    /// new thread).
    pub fn overlay(&mut self, over: &ThreadVoice) {
        let o = over.clone();
        self.asr_alias = o.asr_alias.or(self.asr_alias.take());
        self.tts_alias = o.tts_alias.or(self.tts_alias.take());
        self.voice = o.voice.or(self.voice.take());
        self.language = o.language.or(self.language.take());
        self.reply_language = o.reply_language.or(self.reply_language.take());
        self.read_aloud = o.read_aloud.or(self.read_aloud);
        self.turn_detection = o.turn_detection.or(self.turn_detection);
        self.audio_input = o.audio_input.or(self.audio_input);
        self.speech_style = o.speech_style.or(self.speech_style.take());
        self.seed = o.seed.or(self.seed);
    }

    /// Normalise what was typed and refuse what can be judged without the
    /// gateway: names are trimmed, and an empty alias or voice is "inherit"
    /// (never refused); each language is an ISO 639-1 code (`^[a-z]{2}$`,
    /// case folded), `auto` ([`FOLLOW_USER`]: for the spoken language none
    /// whatever Settings say, for the reply's the spoken one) — or empty to
    /// inherit. The speech
    /// style is trimmed but an empty one stays: it is "none for this
    /// thread". Whether the aliases name models of the right task is
    /// `web::chat_voice`'s check.
    pub fn normalise(&mut self) -> Result<(), String> {
        fn name(v: &mut Option<String>) {
            *v = v
                .take()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
        }
        name(&mut self.asr_alias);
        name(&mut self.tts_alias);
        name(&mut self.voice);
        name(&mut self.language);
        name(&mut self.reply_language);
        languages::check(&mut self.language, "voice.language", languages::SPOKEN_AUTO)?;
        languages::check(
            &mut self.reply_language,
            "voice.reply_language",
            languages::REPLY_AUTO,
        )?;
        if let Some(s) = self.speech_style.as_mut() {
            *s = s.trim().to_string();
        }
        Ok(())
    }
}

/// What a thread's settings write does with the stored TTS seed (§2.2,
/// §6.1). The server draws the seed on first use, in between a settings
/// handler's read and its write, so a write whose patch did not name it
/// keeps whatever is stored when it lands — never the copy it read, which
/// would erase a seed drawn meanwhile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedWrite {
    /// The patch did not name the seed: the stored one stays.
    Keep,
    /// The patch named it (a number, or `null` to clear it): written as
    /// given.
    AsGiven,
}

/// `^[a-z]{2}$` — both voice languages' shape, the realtime session's own
/// rule — and `auto`, a thread's own "none" (shared with the dashboard).
pub use lmgw_api_types::chat_voice::{is_language_code, FOLLOW_USER};

/// `MessageVoice::via` of a dictated user message.
pub const VIA_DICTATION: &str = "dictation";
/// `MessageVoice::via` of a turn spoken in realtime mode, either side.
pub const VIA_REALTIME: &str = "realtime";

/// How a turn was spoken (§3), on its message. `None` on the row is a typed
/// turn.
///
/// - On a **user message**: `via` (`dictation` | `realtime`), the ASR alias
///   that was asked, the one that answered when it was another (a
///   fallback), the transcription's time and the audio's length.
/// - On a **spoken reply**: `via` (`realtime`), the TTS alias, the one that
///   answered when it was another, the voice, the part that was never heard
///   and the turn's timing. The chat model's own fallback stays in the row's
///   `answered_by`.
///
/// Written by the server only, so read leniently: a key it does not know is
/// ignored, and one it cannot read is dropped ([`Self::from_stored`]). The
/// one client input — a dictated send — is strict
/// ([`Self::dictation_from_input`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessageVoice {
    /// [`VIA_DICTATION`] or [`VIA_REALTIME`].
    pub via: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_answered_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts_answered_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// The reply's text after the heard part (§8.4), shown greyed behind a
    /// "not heard" marker. Not in `content`, so the model never sees it and
    /// search never finds it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unheard: Option<String>,
    /// On a user message: `audio` when the chat model heard the turn
    /// (voice-audio-input design §3.3); absent on the transcript path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<InputPath>,
    /// On a user message the model heard: why its transcription failed. The
    /// row then has no words, and a later request carries a placeholder
    /// for it (voice-audio-input design §3.2, §3.4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_error: Option<String>,
    /// Read key by key as well: a key of it this build cannot read goes
    /// alone, not the whole timing.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "tolerant_opt"
    )]
    pub timing: Option<VoiceTiming>,
}

/// A spoken reply's timing (§8.7's `lmgw.response.timing`, as data): each
/// stage measured from the one before, `to_first_audio_ms` their sum from
/// the end of speech, `cold` the stages that loaded during the turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceTiming {
    pub response_id: Option<String>,
    pub message_id: Option<i64>,
    pub end_of_turn_ms: Option<u64>,
    pub asr_ms: Option<u64>,
    pub first_token_ms: Option<u64>,
    /// How long the chat model reasoned before its first token (part of
    /// `first_token_ms`); absent when it did not reason. Never spoken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_ms: Option<u64>,
    pub first_clause_ms: Option<u64>,
    pub first_audio_ms: Option<u64>,
    pub total_ms: Option<u64>,
    pub to_first_audio_ms: Option<u64>,
    pub cold: Vec<String>,
    /// `"announcement"` when the first clause said was an announcement of
    /// a skipped block (§8.4): `first_clause_ms` then measures it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_clause: Option<String>,
    #[serde(deserialize_with = "tolerant")]
    pub models: VoiceModels,
    /// How the turn reached the chat model (voice-audio-input design §5):
    /// `audio` or `transcript`. Absent with audio input off — the timing is
    /// then today's, byte for byte.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<InputPath>,
    /// Why the transcript went where audio input is on (the verdict's
    /// `why`, or the model's refusal of the audio).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_why: Option<String>,
    /// How long the first output was held for the turn's transcript: 0 when
    /// nothing waited; absent on the transcript path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_wait_ms: Option<u64>,
}

/// The models one spoken turn went through.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceModels {
    #[serde(deserialize_with = "tolerant_opt")]
    pub asr: Option<ServedModel>,
    #[serde(deserialize_with = "tolerant_opt")]
    pub chat: Option<ServedModel>,
    #[serde(deserialize_with = "tolerant_opt")]
    pub tts: Option<ServedModel>,
}

/// An alias a stage asked for, the one that answered when it was another,
/// and (for the TTS) the voice.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServedModel {
    pub alias: String,
    pub answered_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
}

/// What a dictated send may say about itself (§5): the transcribe answer's
/// facts, nothing a reply carries.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DictationInput {
    via: String,
    #[serde(default)]
    asr: Option<String>,
    #[serde(default)]
    asr_answered_by: Option<String>,
    #[serde(default)]
    asr_ms: Option<u64>,
    #[serde(default)]
    audio_ms: Option<u64>,
}

impl MessageVoice {
    /// Read `chat_messages.voice`: NULL (a typed turn) and text that is not
    /// a JSON object are `None`; a key this build cannot read is dropped on
    /// its own.
    pub fn from_stored(text: Option<String>) -> Option<Self> {
        match serde_json::from_str::<Value>(text.as_deref()?).ok()? {
            Value::Object(map) => Some(keep_parsable(map)),
            _ => None,
        }
    }

    /// The stored text.
    pub fn to_stored(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }

    /// The `voice` of a `send` (§5): strict, and only a dictation — a
    /// realtime turn is written by the bound session, never sent.
    pub fn dictation_from_input(v: Value) -> Result<Self, String> {
        let d: DictationInput = serde_json::from_value(v).map_err(|e| format!("voice: {e}"))?;
        if d.via != VIA_DICTATION {
            return Err(format!(
                "voice.via '{}' cannot be sent: a send carries only a dictation \
                 (via '{VIA_DICTATION}')",
                d.via
            ));
        }
        let name = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        Ok(Self {
            via: VIA_DICTATION.into(),
            asr: name(d.asr),
            asr_answered_by: name(d.asr_answered_by),
            asr_ms: d.asr_ms,
            audio_ms: d.audio_ms,
            ..Default::default()
        })
    }

    /// The voice an edited reply keeps (§3): its text is the owner's now, so
    /// what was not heard and the turn's timing no longer describe it.
    pub fn edited(mut self) -> Self {
        self.unheard = None;
        self.timing = None;
        self
    }

    /// The voice a continued reply keeps (§3): the continuation follows the
    /// heard text, so the unheard rest goes.
    pub fn continued(mut self) -> Self {
        self.unheard = None;
        self
    }
}

/// The JSON a `voice` column takes, `None` (SQL NULL) for a typed turn.
pub(super) fn message_voice_json(v: Option<&MessageVoice>) -> Option<String> {
    v.map(MessageVoice::to_stored)
}

/// A nested object of a stored [`MessageVoice`], read with
/// [`keep_parsable`]: anything but an object is its default. Never fails, so
/// the key holding it is never dropped for one bad key inside.
fn tolerant<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    Ok(match Value::deserialize(d)? {
        Value::Object(map) => keep_parsable(map),
        _ => T::default(),
    })
}

/// [`tolerant`] for an optional object: anything but an object is `None`.
fn tolerant_opt<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    Ok(match Value::deserialize(d)? {
        Value::Object(map) => Some(keep_parsable(map)),
        _ => None,
    })
}

/// Deserialize `map` keeping only the keys that read on their own: one this
/// build does not know (with `deny_unknown_fields`) or cannot read (a type or
/// a value a newer build changed) is dropped without failing the others.
/// Every field of `T` must default.
fn keep_parsable<T: DeserializeOwned + Default>(map: Map<String, Value>) -> T {
    let kept: Map<String, Value> = map
        .into_iter()
        .filter(|(k, v)| {
            let one = Value::Object(Map::from_iter([(k.clone(), v.clone())]));
            serde_json::from_value::<T>(one).is_ok()
        })
        .collect();
    serde_json::from_value(Value::Object(kept)).unwrap_or_default()
}

/// Draw `thread_id`'s TTS seed on first use (chat-voice design §2.2, §6.1):
/// `drawn` is stored only where the thread holds none, in one conditional
/// write, so two first uses at once keep one seed — the one written first.
/// The seed in effect afterwards; `None` when there is no such thread.
///
/// A stored seed that is not a `u32` (a hand-edited row) is no seed:
/// [`ThreadVoice::from_stored`] drops it on read, so it is replaced here
/// too. A `voice` that is not a JSON object becomes one. The thread's
/// `updated_at` stays: the server drew this, the owner changed nothing. The
/// change feed hears of it (`thread.updated`, the gateway's own).
pub async fn draw_chat_thread_seed(
    pool: &sqlx::SqlitePool,
    thread_id: i64,
    drawn: u32,
) -> super::DbResult<Option<u32>> {
    // A `u32` seed is held; anything else is none (doc, `seed_held!`).
    let mut tx = super::begin_write(pool).await?;
    let written: Option<i64> = sqlx::query_scalar(concat!(
        "UPDATE chat_threads SET voice = json_set(
           CASE WHEN NOT json_valid(voice) THEN '{}'
                WHEN json_type(voice) = 'object' THEN voice ELSE '{}' END,
           '$.seed', ?2)
         WHERE id = ?1 AND NOT (",
        seed_held!(),
        ") RETURNING json_extract(voice, '$.seed')"
    ))
    .bind(thread_id)
    .bind(i64::from(drawn))
    .fetch_optional(&mut *tx)
    .await?;
    if written.is_some() {
        // The thread's `voice` as a client lists it changed: the gateway's
        // own write (client-apps design §2.2).
        super::feed::record_thread(&mut tx, super::feed::kind::THREAD_UPDATED, thread_id, None)
            .await?;
        tx.commit().await?;
        return Ok(Some(drawn));
    }
    drop(tx);
    let held: Option<i64> = sqlx::query_scalar(concat!(
        "SELECT json_extract(voice, '$.seed') FROM chat_threads WHERE id = ?1 AND ",
        seed_held!()
    ))
    .bind(thread_id)
    .fetch_optional(pool)
    .await?;
    Ok(held.and_then(|s| u32::try_from(s).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ChatThread, KbMode, ThreadDefaults};
    use serde_json::json;

    /// WP4 review m8: the conditional draw's SQL edges.
    #[tokio::test]
    async fn a_seed_is_drawn_only_where_no_u32_seed_is_held() {
        let pool = crate::store::open_in_memory().await.unwrap();
        let thread = async |voice: &str| {
            let id = crate::store::create_chat_thread(&pool, "m", "chat")
                .await
                .unwrap();
            sqlx::query("UPDATE chat_threads SET voice = ?1 WHERE id = ?2")
                .bind(voice)
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
            id
        };
        let stored = async |id: i64| -> Value {
            let raw: String = sqlx::query_scalar("SELECT voice FROM chat_threads WHERE id = ?1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
            serde_json::from_str(&raw).unwrap()
        };
        // No `seed` key (`IFNULL`): drawn and stored, the other keys kept.
        let id = thread(r#"{"language":"de"}"#).await;
        assert_eq!(draw_chat_thread_seed(&pool, id, 7).await.unwrap(), Some(7));
        assert_eq!(stored(id).await, json!({ "language": "de", "seed": 7 }));
        // Held: kept, and the later draw is not.
        assert_eq!(draw_chat_thread_seed(&pool, id, 9).await.unwrap(), Some(7));
        assert_eq!(stored(id).await["seed"], 7);
        // `u32::MAX` is a seed; anything that is no `u32` is none, and
        // replaced: negative, too large, a real, a string.
        let id = thread(r#"{"seed":4294967295}"#).await;
        assert_eq!(
            draw_chat_thread_seed(&pool, id, 3).await.unwrap(),
            Some(u32::MAX)
        );
        for bad in [
            r#"{"seed":-1}"#,
            r#"{"seed":4294967296}"#,
            r#"{"seed":1.5}"#,
            r#"{"seed":"7"}"#,
            r#"{"seed":null}"#,
        ] {
            let id = thread(bad).await;
            assert_eq!(
                draw_chat_thread_seed(&pool, id, 11).await.unwrap(),
                Some(11),
                "{bad}"
            );
            assert_eq!(stored(id).await, json!({ "seed": 11 }), "{bad}");
        }
        // A `voice` that is no JSON object becomes one.
        for bad in ["[]", "\"x\"", "null", "not json"] {
            let id = thread(bad).await;
            assert_eq!(
                draw_chat_thread_seed(&pool, id, 5).await.unwrap(),
                Some(5),
                "{bad}"
            );
            assert_eq!(stored(id).await, json!({ "seed": 5 }), "{bad}");
        }
        // No such thread.
        assert_eq!(draw_chat_thread_seed(&pool, 9_999, 1).await.unwrap(), None);
        // A settings save over a malformed stored `voice` keeps no seed and
        // fails nothing.
        let id = thread("not json").await;
        let t = crate::store::get_chat_thread(&pool, id)
            .await
            .unwrap()
            .unwrap();
        let saved = crate::store::update_chat_thread_settings(
            &pool,
            &t,
            crate::store::SeedWrite::Keep,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(saved.seed, None);
        // The settings save judges a stored seed as the draw does (WP11
        // server review n7): a `u32` is kept, an out-of-range integer is
        // none, in the row too.
        for (stored_voice, kept) in [
            (r#"{"seed":4294967295}"#, Some(json!(4294967295u64))),
            (r#"{"seed":-1}"#, None),
            (r#"{"seed":4294967296}"#, None),
        ] {
            let id = thread(stored_voice).await;
            let t = crate::store::get_chat_thread(&pool, id)
                .await
                .unwrap()
                .unwrap();
            crate::store::update_chat_thread_settings(
                &pool,
                &t,
                crate::store::SeedWrite::Keep,
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                stored(id).await.get("seed").cloned(),
                kept,
                "{stored_voice}"
            );
        }
    }

    #[test]
    fn thread_voice_input_is_strict() {
        let ok: ThreadVoice = serde_json::from_value(json!({
            "asr_alias": "asr", "turn_detection": "push_to_talk", "seed": 7, "speech_style": ""
        }))
        .unwrap();
        assert_eq!(ok.turn_detection, Some(TurnDetection::PushToTalk));
        assert_eq!(ok.speech_style.as_deref(), Some(""));
        // A typo, a wrong type, an unknown value: each refused by name.
        for (bad, word) in [
            (json!({ "tts": "x" }), "tts"),
            (json!({ "read_aloud": "yes" }), "boolean"),
            (json!({ "turn_detection": "auto" }), "auto"),
            (json!({ "seed": -1 }), "-1"),
        ] {
            let e = serde_json::from_value::<ThreadVoice>(bad.clone()).unwrap_err();
            assert!(e.to_string().contains(word), "{bad}: {e}");
        }
        // `null` is "unset", as absent is.
        let unset: ThreadVoice = serde_json::from_value(json!({ "voice": null })).unwrap();
        assert!(unset.is_empty());
    }

    #[test]
    fn a_stored_thread_voice_drops_only_what_it_cannot_read() {
        let v = ThreadVoice::from_stored(
            r#"{"asr_alias":"asr","future_key":1,"turn_detection":"auto","read_aloud":true}"#,
        );
        assert_eq!(
            v,
            ThreadVoice {
                asr_alias: Some("asr".into()),
                read_aloud: Some(true),
                ..Default::default()
            }
        );
        assert!(ThreadVoice::from_stored("not json").is_empty());
        assert!(ThreadVoice::from_stored("[1]").is_empty());
        assert_eq!(ThreadVoice::from_value(Value::Null), None);
    }

    #[test]
    fn a_newer_nested_voice_key_keeps_every_folder_default() {
        let d = ThreadDefaults::from_stored(
            r#"{"model_alias":"m","temperature":0.3,"kb_mode":"tool",
                "voice":{"tts_alias":"tts","voice":"alba","added_later":{"x":1}}}"#,
        );
        assert_eq!(d.model_alias.as_deref(), Some("m"));
        assert_eq!(d.temperature, Some(0.3));
        assert_eq!(d.kb_mode, Some(KbMode::Tool));
        assert_eq!(
            d.voice,
            Some(ThreadVoice {
                tts_alias: Some("tts".into()),
                voice: Some("alba".into()),
                ..Default::default()
            })
        );
        // A voice that is not an object is no voice defaults, and the rest
        // still reads.
        let d = ThreadDefaults::from_stored(r#"{"model_alias":"m","voice":"loud"}"#);
        assert_eq!((d.model_alias.as_deref(), d.voice), (Some("m"), None));
    }

    #[test]
    fn folder_voice_defaults_lay_over_a_new_thread() {
        let mut t = ChatThread::default();
        let d = ThreadDefaults {
            voice: Some(ThreadVoice {
                tts_alias: Some("tts".into()),
                read_aloud: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        d.apply(&mut t);
        assert_eq!(t.voice.tts_alias.as_deref(), Some("tts"));
        assert_eq!(t.voice.read_aloud, Some(true));
        assert_eq!(t.voice.asr_alias, None);
    }

    #[test]
    fn normalise_trims_folds_and_refuses_a_bad_language() {
        let mut v = ThreadVoice {
            asr_alias: Some("  ".into()),
            tts_alias: Some(" tts ".into()),
            language: Some(" DE ".into()),
            speech_style: Some("  ".into()),
            ..Default::default()
        };
        v.normalise().unwrap();
        assert_eq!(v.asr_alias, None, "an empty alias inherits");
        assert_eq!(v.tts_alias.as_deref(), Some("tts"));
        assert_eq!(v.language.as_deref(), Some("de"));
        assert_eq!(v.speech_style.as_deref(), Some(""), "none for this thread");
        for bad in ["deu", "d", "d1", "de-AT", "automatic"] {
            let mut v = ThreadVoice {
                language: Some(bad.into()),
                ..Default::default()
            };
            assert!(v.normalise().is_err(), "{bad}");
        }
        // `auto`: none here, whatever Settings say.
        let mut v = ThreadVoice {
            language: Some(" AUTO ".into()),
            ..Default::default()
        };
        v.normalise().unwrap();
        assert_eq!(v.language.as_deref(), Some(FOLLOW_USER));
    }

    #[test]
    fn message_voice_reads_leniently_and_round_trips() {
        let v = MessageVoice {
            via: VIA_REALTIME.into(),
            tts: Some("tts".into()),
            unheard: Some("the rest".into()),
            timing: Some(VoiceTiming {
                asr_ms: Some(31),
                cold: vec!["tts".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(MessageVoice::from_stored(Some(v.to_stored())), Some(v));
        assert_eq!(MessageVoice::from_stored(None), None);
        assert_eq!(MessageVoice::from_stored(Some("3".into())), None);
        // A key from a newer build is ignored; one whose type changed goes
        // alone.
        let v = MessageVoice::from_stored(Some(
            r#"{"via":"dictation","asr":"a","asr_ms":"fast","new":true}"#.into(),
        ))
        .unwrap();
        assert_eq!(
            (v.via.as_str(), v.asr.as_deref(), v.asr_ms),
            ("dictation", Some("a"), None)
        );
        // Inside the timing too: one changed key goes, the rest stays, down
        // to a served model's own keys.
        let v = MessageVoice::from_stored(Some(
            r#"{"via":"realtime","timing":{"asr_ms":31,"total_ms":"slow","cold":"tts",
                "models":{"tts":{"alias":"t","answered_by":7},"chat":"plain"}}}"#
                .into(),
        ))
        .unwrap();
        let t = v.timing.unwrap();
        assert_eq!((t.asr_ms, t.total_ms), (Some(31), None));
        assert!(t.cold.is_empty());
        assert_eq!(
            t.models.tts,
            Some(ServedModel {
                alias: "t".into(),
                ..Default::default()
            })
        );
        assert_eq!(t.models.chat, None);
    }

    #[test]
    fn a_send_carries_only_a_dictation() {
        let v = MessageVoice::dictation_from_input(json!({
            "via": "dictation", "asr": "nemo", "asr_ms": 98, "audio_ms": 2800
        }))
        .unwrap();
        assert_eq!(
            (v.asr.as_deref(), v.asr_ms, v.audio_ms),
            (Some("nemo"), Some(98), Some(2800))
        );
        assert!(MessageVoice::dictation_from_input(json!({ "via": "realtime" })).is_err());
        let e = MessageVoice::dictation_from_input(json!({ "via": "dictation", "unheard": "x" }))
            .unwrap_err();
        assert!(e.contains("unheard"), "{e}");
    }

    #[test]
    fn editing_clears_unheard_and_timing_continuing_only_unheard() {
        let v = MessageVoice {
            via: VIA_REALTIME.into(),
            tts: Some("tts".into()),
            unheard: Some("rest".into()),
            timing: Some(VoiceTiming::default()),
            ..Default::default()
        };
        let e = v.clone().edited();
        assert_eq!(
            (e.unheard, e.timing, e.tts.as_deref()),
            (None, None, Some("tts"))
        );
        let c = v.continued();
        assert_eq!(c.unheard, None);
        assert!(c.timing.is_some());
    }

    #[test]
    fn turn_detection_names_parse_back() {
        for n in TurnDetection::NAMES {
            assert_eq!(TurnDetection::parse(n).unwrap().as_str(), n);
        }
        // The dashboard's list names the same values, in the same order.
        let shared: Vec<&str> = lmgw_api_types::chat_voice::TURN_DETECTIONS
            .iter()
            .map(|(n, _)| *n)
            .collect();
        assert_eq!(shared, TurnDetection::NAMES);
        assert_eq!(
            TurnDetection::parse(" Server_VAD "),
            Some(TurnDetection::ServerVad)
        );
        assert_eq!(TurnDetection::parse("auto"), None);
    }
}
