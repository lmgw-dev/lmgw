//! The Chat API's thread and folder shapes: what `GET /chat/api/threads`,
//! `GET /chat/api/folders`, a folder create and
//! `POST /chat/api/folders/{id}/current` answer, and what the change feed's
//! `thread.*` and `folder.*` events carry; and a thread's MCP tasks
//! (a late result's row [`MessageTask`], the open tasks [`ThreadTask`],
//! `answer` and the cancel's [`TaskCancelled`]).
//!
//! The gateway serializes these types, and the API document is generated from
//! them, so the two cannot drift. Every body is written with its object keys
//! in alphabetical order at every depth ([`wire_order`]): the order the Chat
//! API has always sent.

use serde::{Deserialize, Serialize, Serializer};

use crate::chat_folders::{FolderOngoing, OngoingInput};

mod tasks;
pub use tasks::*;

/// Serializes `value` with its object keys in alphabetical order at every
/// depth, through a `serde_json::Value` (whose map keeps its keys sorted).
/// The Chat API's bodies were built as values from the start, so this is the
/// byte order its clients have always received.
pub fn wire_order<T: Serialize, S: Serializer>(value: &T, s: S) -> Result<S::Ok, S::Error> {
    serde_json::to_value(value)
        .map_err(serde::ser::Error::custom)?
        .serialize(s)
}

/// `Some(value)` for a field that was sent, `null` included: a bare
/// `Option<Option<T>>` would read `null` as absent, and a patch could never
/// clear a setting. Pair it with `#[serde(default)]`.
pub fn present<'de, T, D>(de: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(de).map(Some)
}

pub use crate::ack::Ack;

/// A tool server attached to a thread: the label it is reached by, and
/// optionally the tools of it the thread uses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadMcp {
    /// A registered tool server's tool prefix (or its name when it has
    /// none), or a built-in toolset such as `kb`.
    pub server_label: String,
    /// The tools of that server the thread uses; `null` for all of them.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Which of them wait for an approval before they run, in OpenAI's
    /// shapes: `"never"` (the default, also when absent), `"always"`, or
    /// `{"always": {"tool_names": […]}, "never": {"tool_names": […]}}`. A
    /// name may be the tool's own or its prefixed one. `read_only` is
    /// refused. A gated call stops the turn; `POST
    /// /chat/api/threads/{id}/approvals` decides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_approval: Option<serde_json::Value>,
}

// Forward compatibility (review W6-2): every enum a client reads gets a
// fallback for a value a newer gateway may send. These three are also
// written back (a folder's defaults, a thread's voice), so the fallback
// keeps the value as sent: `#[serde(untagged)]` on the variant reads any
// other string into it and writes it out unchanged. The document lists the
// known values only (`schemars(skip)`).

/// How a thread's knowledge bases reach the model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum KbMode {
    /// Search before every turn and send the excerpts with the user's
    /// message.
    #[default]
    Auto,
    /// Give the model the knowledge search tools; it searches when it
    /// decides to.
    Tool,
    /// A value this build does not know (a newer gateway's), as sent: it
    /// writes back unchanged.
    #[serde(untagged)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unknown(String),
}

/// How voice mode detects the end of the user's turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum TurnDetectionMode {
    /// A turn model decides when the speaker is done.
    SemanticVad,
    /// Silence windows only.
    ServerVad,
    /// A turn is what is spoken while the talk key is held.
    PushToTalk,
    /// A value this build does not know (a newer gateway's), as sent: it
    /// writes back unchanged.
    #[serde(untagged)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unknown(String),
}

/// Whether a voice turn may reach the chat model as audio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum AudioInputMode {
    /// The chat model reads the transcript.
    Off,
    /// A chat model that takes audio hears the turn; the transcript is
    /// made beside it.
    On,
    /// A value this build does not know (a newer gateway's), as sent: it
    /// writes back unchanged.
    #[serde(untagged)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unknown(String),
}

/// A thread's own voice settings. Every field is optional and absent when
/// unset: an absent field takes the Chat's voice settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadVoice {
    /// The speech-to-text alias for dictation, voice mode and audio
    /// attachments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_alias: Option<String>,
    /// The text-to-speech alias for read-aloud and voice mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts_alias: Option<String>,
    /// A voice of that text-to-speech model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// The language the user speaks, a two-letter ISO 639-1 code, or `auto`
    /// to let the speech-to-text model detect it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// The language replies are in, a two-letter code, or `auto` to follow
    /// the language the user speaks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_language: Option<String>,
    /// Read every reply aloud as it streams.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_aloud: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_detection: Option<TurnDetectionMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_input: Option<AudioInputMode>,
    /// The speaking style the text-to-speech model is given; an empty text
    /// is none for this thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speech_style: Option<String>,
    /// The thread's text-to-speech seed, so one thread keeps one voice on a
    /// model that draws its voice. Set on first use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u32>,
}

/// A Chat thread as `GET /chat/api/threads` lists it, and as the change
/// feed's `thread.created` and `thread.updated` carry it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadRow {
    /// Negative for a temporary thread.
    pub id: i64,
    pub title: String,
    /// The alias the thread's turns go to.
    pub model_alias: String,
    pub system_prompt: String,
    /// Sampling overrides sent with every turn; `null` leaves the route's
    /// default.
    pub temperature: Option<f64>,
    pub max_tokens: Option<i64>,
    pub top_p: Option<f64>,
    pub top_k: Option<i64>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub seed: Option<i64>,
    /// Stop sequences; empty leaves the route's default.
    pub stop: Vec<String>,
    /// `chat`, or `admin` for a thread that drives the gateway's own
    /// configuration (never listed for a device key).
    pub kind: String,
    /// The tool servers the thread's turns use; empty for a plain chat. A
    /// thread with `lmgw` (the self-admin toolset) among them is listed for a
    /// device key only when the device may use lmgw's admin tools.
    pub mcp_tools: Vec<ThreadMcp>,
    /// Reasoning overrides sent with every turn; `null` leaves the route's
    /// default.
    pub reasoning_enabled: Option<bool>,
    pub reasoning_effort: Option<String>,
    pub reasoning_budget: Option<i64>,
    /// The agent the thread was opened from, if any.
    pub agent_id: Option<String>,
    /// Pinned threads are listed first and are never archived or deleted
    /// by the sweep.
    pub pinned: bool,
    /// When the thread was archived; `null` while it is active.
    pub archived_at: Option<String>,
    /// The folder the thread is in; `null` for none.
    pub folder_id: Option<i64>,
    /// The personality profile the thread talks with
    /// (`GET /chat/api/profiles`); `null` for none ("Default").
    pub profile_id: Option<i64>,
    /// Knowledge bases the thread searches, by id.
    pub kb_ids: Vec<i64>,
    pub kb_mode: KbMode,
    /// The retrieval budget of `auto` mode in tokens; `null` for the Chat's
    /// setting.
    pub kb_budget_tokens: Option<i64>,
    pub voice: ThreadVoice,
    pub created_at: String,
    /// Moves with every change to the thread and every new message.
    pub updated_at: String,
    /// When its newest message was written, in unix seconds; `null` for a
    /// thread without messages. An ongoing folder's idle rollover is
    /// measured from it: a client that shows when the conversation moves
    /// on reads it here rather than timing the turns it saw.
    pub last_message_at: Option<i64>,
    /// When an archived thread will be deleted; `null` while it is active,
    /// pinned, or when deleting is switched off.
    pub purge_at: Option<String>,
    /// The thread lives in memory only and is gone at the gateway's next
    /// start unless it is kept.
    pub temporary: bool,
}

/// Whether a thread's last reply can be continued, and why not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContinueState {
    pub ok: bool,
    pub reason: Option<String>,
}

/// One Chat thread with what an open thread shows: its list row, what its
/// voice resolves to now, and (where it is read whole) whether its last
/// reply can be continued.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Thread {
    #[serde(flatten)]
    pub row: ThreadRow,
    /// What the thread's voice resolves to now, field by field with the
    /// level each value comes from (the thread, the Chat's settings, the
    /// realtime settings): the speech-to-text and text-to-speech stages
    /// (`asr`, `tts`: alias, whether it runs on this machine, what answers
    /// for it while the GPU is held), `voice`, `speech_style`, `language`,
    /// `reply_language`, `read_aloud`, `turn_detection`, `audio_input`,
    /// `seed`, the `problems` that block a voice feature now, and
    /// `realtime`: whether voice mode can bind the thread.
    #[cfg_attr(feature = "schema", schemars(extend("type" = "object")))]
    pub voice_resolved: serde_json::Value,
    /// Whether the last reply can be continued. Present where the thread
    /// is read whole, as `POST /chat/api/folders/{id}/current` answers it.
    #[serde(rename = "continue", default, skip_serializing_if = "Option::is_none")]
    pub continue_state: Option<ContinueState>,
}

/// What a new thread in a folder starts with. Every field is optional;
/// `null` is the Chat's own default. A copy is taken when a thread is
/// created, except that an ongoing conversation's changed defaults also
/// reach its current thread.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadDefaults {
    /// The model of a new thread; an ongoing conversation needs one.
    pub model_alias: Option<String>,
    pub system_prompt: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i64>,
    pub top_p: Option<f64>,
    pub top_k: Option<i64>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub seed: Option<i64>,
    pub stop: Option<Vec<String>>,
    pub reasoning_enabled: Option<bool>,
    pub reasoning_effort: Option<String>,
    pub reasoning_budget: Option<i64>,
    pub mcp_tools: Option<Vec<ThreadMcp>>,
    pub kb_ids: Option<Vec<i64>>,
    pub kb_mode: Option<KbMode>,
    pub kb_budget_tokens: Option<i64>,
    /// Voice settings, laid field by field over a new thread's.
    pub voice: Option<ThreadVoice>,
    /// The personality profile of a new thread; `null` for the Chat's own
    /// default (Settings → Chat).
    pub profile_id: Option<i64>,
}

/// A Chat folder as `GET /chat/api/folders` lists it, with its thread
/// counts as the caller may see them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Folder {
    pub id: i64,
    /// Folder names need not be unique.
    pub name: String,
    /// The folder's place in the list.
    pub sort: i64,
    pub defaults: ThreadDefaults,
    /// `null` for a folder that is not one ongoing conversation.
    pub ongoing: Option<FolderOngoing>,
    /// Days without activity before one of its threads is archived; `null`
    /// for the Chat's setting, `0` never. Set from the dashboard: a device
    /// key cannot change it.
    pub archive_days: Option<i64>,
    /// Days after archiving before one of its threads is deleted; `null`
    /// for the Chat's setting, `0` never. Set from the dashboard: a device
    /// key cannot change it.
    pub purge_days: Option<i64>,
    /// Set when a device deleted this folder while it held threads that
    /// device could not see: the folder stays for them, hidden from every
    /// device, until it is shown to devices again from the dashboard. Always
    /// `false` for a device key, which never sees such a folder.
    #[serde(default)]
    pub devices_hidden: bool,
    pub created_at: String,
    pub updated_at: String,
    pub threads_active: i64,
    pub threads_archived: i64,
}

/// `GET /chat/api/threads`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadList {
    /// Stored threads: pinned first, then the most recently active; with
    /// `?archived=1` the archived ones, the most recently archived first.
    pub threads: Vec<ThreadRow>,
    /// How many threads are archived, whichever list was asked for.
    pub archived_count: i64,
    /// Temporary threads, the most recently active first.
    pub temporary: Vec<ThreadRow>,
    /// Every folder, as `GET /chat/api/folders` lists them.
    pub folders: Vec<Folder>,
}

/// `GET /chat/api/threads/rows`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadRows {
    /// The rows found, each as `GET /chat/api/threads` lists it, in the
    /// active list's order.
    pub threads: Vec<ThreadRow>,
}

/// `GET /chat/api/folders`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FolderList {
    /// In the list's order.
    pub folders: Vec<Folder>,
}

/// `POST /chat/api/folders`'s body.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FolderCreate {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<ThreadDefaults>,
    /// Marks the folder as one ongoing conversation; its defaults must then
    /// name a model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ongoing: Option<OngoingInput>,
    /// Days without activity before one of its threads is archived; absent
    /// for the Chat's setting, `0` never. A device key cannot set it (403
    /// forbidden): a folder's own retention is set from the dashboard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_days: Option<i64>,
    /// Days after archiving before one of its threads is deleted; absent
    /// for the Chat's setting, `0` never. A device key cannot set it, as
    /// `archive_days`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purge_days: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_thread_is_its_row_with_the_open_thread_s_fields() {
        let t = Thread {
            row: ThreadRow {
                id: 7,
                title: "T".into(),
                ..Default::default()
            },
            voice_resolved: json!({"seed": null}),
            continue_state: Some(ContinueState {
                ok: false,
                reason: Some("no reply".into()),
            }),
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["continue"], json!({"ok": false, "reason": "no reply"}));
        assert_eq!(v["voice_resolved"], json!({"seed": null}));
        let back: Thread = serde_json::from_value(v).unwrap();
        assert_eq!(back, t);
        // Without `continue` where a thread is not read whole.
        let v = serde_json::to_value(Thread {
            continue_state: None,
            ..t
        })
        .unwrap();
        assert!(v.get("continue").is_none());
    }

    #[test]
    fn a_voice_leaves_out_what_it_does_not_set() {
        let v = ThreadVoice {
            tts_alias: Some("tts".into()),
            turn_detection: Some(TurnDetectionMode::PushToTalk),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"tts_alias":"tts","turn_detection":"push_to_talk"}"#
        );
    }

    #[test]
    fn a_newer_gateway_s_values_read_and_write_back_as_sent() {
        let row: ThreadRow = serde_json::from_value(json!({
            "id": 3, "kb_mode": "hybrid",
            "voice": {"turn_detection": "lookahead", "audio_input": "auto"}
        }))
        .unwrap();
        assert_eq!(row.kb_mode, KbMode::Unknown("hybrid".into()));
        assert_eq!(
            row.voice.turn_detection,
            Some(TurnDetectionMode::Unknown("lookahead".into()))
        );
        assert_eq!(
            row.voice.audio_input,
            Some(AudioInputMode::Unknown("auto".into()))
        );
        // A folder's defaults a client copies keep the value it read.
        let d: ThreadDefaults = serde_json::from_value(json!({"kb_mode": "hybrid"})).unwrap();
        assert_eq!(
            serde_json::to_value(&d).unwrap()["kb_mode"],
            json!("hybrid")
        );
        // The known values are still the known variants.
        let row: ThreadRow = serde_json::from_value(json!({
            "kb_mode": "tool", "voice": {"turn_detection": "push_to_talk", "audio_input": "on"}
        }))
        .unwrap();
        assert_eq!(row.kb_mode, KbMode::Tool);
        assert_eq!(
            row.voice.turn_detection,
            Some(TurnDetectionMode::PushToTalk)
        );
        assert_eq!(row.voice.audio_input, Some(AudioInputMode::On));
    }

    #[cfg(feature = "schema")]
    #[test]
    fn the_document_lists_the_known_values_only() {
        fn values(s: serde_json::Value) -> Vec<serde_json::Value> {
            let list = s.get("oneOf").or(s.get("anyOf")).expect("a list of values");
            list.as_array()
                .unwrap()
                .iter()
                .map(|v| v["const"].clone())
                .collect()
        }
        let s = serde_json::to_value(schemars::schema_for!(KbMode)).unwrap();
        assert_eq!(values(s), [json!("auto"), json!("tool")]);
        let s = serde_json::to_value(schemars::schema_for!(AudioInputMode)).unwrap();
        assert_eq!(values(s), [json!("off"), json!("on")]);
    }

    #[test]
    fn the_wire_order_is_alphabetical_at_every_depth() {
        #[derive(Serialize)]
        struct Outer {
            #[serde(serialize_with = "wire_order")]
            z: Inner,
            a: u8,
        }
        #[derive(Serialize)]
        struct Inner {
            y: u8,
            b: u8,
        }
        let s = serde_json::to_string(&Outer {
            z: Inner { y: 1, b: 2 },
            a: 3,
        })
        .unwrap();
        assert_eq!(s, r#"{"z":{"b":2,"y":1},"a":3}"#);
    }
}
