//! `lmgw__settings`

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::realtime::RealtimeSettings;
use crate::{AdminLevel, DocsSearchDefaults, HoldSettingsDto};

/// `lmgw__settings`: the gateway's settings with every secret redacted: a
/// token is reported as `has_*`, never returned. It is a subset of `GET
/// /api/settings-full`'s `SettingsFull` (no currency or budget and no
/// `/v1/responses` limits) under the same names and types.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SettingsRead {
    pub bind_addr: String,
    pub auth_enabled: bool,
    pub retention_days: i64,
    pub retention_max_rows: i64,
    pub jobs_retention_days: i64,
    pub jobs_max_rows: i64,
    /// Largest request body on the JSON `/v1` routes, in MiB; `0` = unlimited.
    pub max_body_mb: u32,
    /// Archive a Chat thread this many days after its last activity; `0` = never.
    pub chat_archive_days: i64,
    /// Delete an archived, unpinned Chat thread this many days after it was
    /// archived; `0` = never.
    pub chat_purge_days: i64,
    /// Room reserved for one extraction reply, in tokens.
    pub docs_ingest_reply_tokens: u32,
    /// Texts per embedding call during ingest and re-embed.
    pub docs_embed_batch: u32,
    /// Politeness pause between two fetches to the same host, ms.
    pub docs_fetch_delay_ms: u64,
    /// The stage defaults a request that overrides nothing starts from.
    pub docs_search: DocsSearchDefaults,
    /// Alias the rerank stage calls; empty = the aux router's single enabled
    /// rerank model.
    pub docs_rerank_model: String,
    /// Alias that answers MCP `sampling/createMessage` for a server with none
    /// of its own; empty = none.
    pub sampling_alias: String,
    /// What the `lmgw__*` tools may do: `off | read_only | full`.
    pub self_admin: AdminLevel,
    pub update_check_enabled: bool,
    /// A Hugging Face token is stored.
    pub has_hf_token: bool,
    /// An update-feed token is stored.
    pub has_update_token: bool,
    /// Podman container name prefix.
    pub container_prefix: String,
    /// The DNS suffix a service agent's UI is served under.
    pub agent_origin_suffix: String,
    /// The stock image a manifest's `script` step runs in.
    pub agent_script_image: String,
    /// The GPU hold. `active` is reported here but never patchable through
    /// `lmgw__settings_set`: engaging it runs the sweep, a side effect a
    /// generic settings save must not grow (`hold_set` does that).
    pub hold: HoldSettingsDto,
    /// The only field of the GPU admission settings the tool reports: with the
    /// hold, it decides when a fallback answers instead of the local model.
    pub vram: VramRead,
    pub router: ClassRead,
    pub aux_router: AuxClassRead,
    pub audio: AudioRead,
    /// The image class has no engine fields at all: sd-server has no config
    /// file, and everything per-process is a flag in some row's `args`.
    pub image: ClassRead,
    /// Days the Chat change feed keeps its records; `0` = all.
    pub chat_feed_retention_days: i64,
    /// Seconds between the Chat feed's keep-alive comments.
    pub chat_feed_keepalive_s: u32,
    /// The longest keep-alive a feed may ask for itself, in seconds.
    pub chat_feed_keepalive_max_s: u32,
    /// Records the Chat feed reads per query while a client catches up.
    pub chat_feed_page_size: u32,
    /// Live Chat feed events held for a slow client.
    pub chat_feed_live_buffer: u32,
    /// The system prompt new Chat threads start with, as in force: the
    /// configured one, or the built-in text when none was written.
    pub chat_system_prompt: String,
    /// Whether `chat_system_prompt` is the built-in text (none written).
    pub chat_system_prompt_is_builtin: bool,
    /// The personality profile new threads start with, as a number, in the shape
    /// `lmgw__settings_set` takes back; `null` for none.
    pub chat_profile: Option<i64>,
    /// `text | images | ask`.
    pub chat_pdf_mode: String,
    /// The Chat's speech-to-text alias; empty = `realtime.asr_alias`.
    pub chat_stt_alias: String,
    /// The Chat's text-to-speech alias; empty = `realtime.tts_alias`.
    pub chat_tts_alias: String,
    pub chat_voice: String,
    pub chat_speech_style: String,
    /// ISO 639-1; empty = none.
    pub chat_voice_language: String,
    /// ISO 639-1; empty = `chat_voice_language`.
    pub chat_voice_reply_language: String,
    pub chat_read_aloud: bool,
    /// `semantic_vad | server_vad | push_to_talk`.
    pub chat_turn_detection: String,
    /// `off | on`.
    pub chat_voice_audio_input: String,
    /// Tokens of knowledge-base excerpts one Chat turn may carry.
    pub chat_kb_budget_tokens: u32,
    /// Where container builds keep their git mirrors, worktrees and logs, as
    /// configured; `null` = the default `builds_dir_effective` spells out.
    pub builds_dir: Option<String>,
    /// The directory builds actually use. "Unset" means a default that differs
    /// between prod and a dev instance, so both are shown.
    pub builds_dir_effective: String,
    /// Why builds would refuse to run in `builds_dir_effective` (it is on
    /// tmpfs, i.e. RAM); `null` when they would not.
    pub builds_dir_warning: Option<String>,
    /// Forge API tokens by host; the value is always `"<set>"`.
    pub forge_tokens: BTreeMap<String, String>,
    /// How often the build update check runs, in hours; `0` = off.
    pub build_update_check_hours: u32,
    /// Spoken conversations (`GET /v1/realtime`), in the shape `lmgw__settings_set
    /// realtime` takes back.
    pub realtime: RealtimeSettings,
}

/// The one field of the GPU admission settings the tool reports.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VramRead {
    /// Fall back when VRAM outside lmgw's control is short.
    pub fallback_on_external: bool,
}

/// A model class as the tool reports it, for the chat (`router`), aux
/// (`aux_router`, with [`AuxClassRead`]) and image (`image`) classes alike: the image
/// every model of the class inherits, where its GGUFs live, the extra `podman
/// run` flags, and the prefix its models are exposed under. Container names
/// are derived and ports are dynamic, so neither is a setting.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClassRead {
    pub image: String,
    pub models_dir: String,
    /// One entry per flag.
    pub extra_run_args: Vec<String>,
    pub public_prefix: String,
}

/// The aux class as the tool reports it: the class fields, and the prefix its
/// decision rows are exposed under (`decide/<model id>` by default) beside
/// `public_prefix`, which names its embedders and rerankers
/// (decision-models design §3.10).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuxClassRead {
    #[serde(flatten)]
    pub base: ClassRead,
    pub decision_prefix: String,
}

/// The audio class as the tool reports it: the class fields, and the engine
/// fields that go into every audio model's `server.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioRead {
    pub image: String,
    pub models_dir: String,
    /// `cuda | cpu | vulkan | metal | hip`.
    pub backend: String,
    pub device: i64,
    pub threads: i64,
    pub lazy_load: bool,
    /// The four bounds and the voice library go into every model's
    /// `server.json`; `0` is "no bound" on each, and a reader that cannot see
    /// them cannot explain a `503 server_busy` or a reloaded model.
    pub busy_timeout_ms: i64,
    pub idle_unload_ms: i64,
    pub min_free_memory_mb: i64,
    pub max_request_body_mb: i64,
    pub voice_dir: String,
    /// One entry per flag.
    pub extra_run_args: Vec<String>,
    pub public_prefix: String,
    /// Which local model writes voice-clip transcripts; empty = none.
    pub voice_transcribe_alias: String,
    /// What an audio catalog download takes: `pinned | latest`.
    pub catalog_revision: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The 53 keys are all there on a default value, the optionals as `null`
    /// (a key that is there, never left out), the token map an empty object,
    /// the secrets as `has_*` flags.
    #[test]
    fn a_default_answer_sends_every_key_with_the_unset_ones_null() {
        let wire = serde_json::to_value(SettingsRead::default()).unwrap();
        let keys = wire.as_object().unwrap();
        assert_eq!(keys.len(), 53, "{wire}");
        for nullable in ["chat_profile", "builds_dir", "builds_dir_warning"] {
            assert!(keys[nullable].is_null(), "{nullable}: {wire}");
        }
        assert_eq!(wire["has_hf_token"], json!(false));
        assert_eq!(wire["has_update_token"], json!(false));
        assert!(wire.get("hf_token").is_none() && wire.get("update_token").is_none());
        assert!(wire["hold"]["fallback_alias"].is_null());
        assert_eq!(wire["forge_tokens"], json!({}));
        assert_eq!(wire["self_admin"], json!("off"));
    }

    /// A class's run arguments are a list, a class has four keys, the audio
    /// class fifteen, and the GPU settings one.
    #[test]
    fn the_class_sections_have_the_keys_the_tool_always_sent() {
        let class = serde_json::to_value(ClassRead::default()).unwrap();
        assert_eq!(
            class,
            json!({"image": "", "models_dir": "", "extra_run_args": [], "public_prefix": ""})
        );
        let audio = serde_json::to_value(AudioRead::default()).unwrap();
        assert_eq!(audio.as_object().unwrap().len(), 15, "{audio}");
        assert!(audio["device"].is_i64() && audio["lazy_load"] == json!(false));
        assert_eq!(
            serde_json::to_value(VramRead::default()).unwrap(),
            json!({"fallback_on_external": false})
        );
    }

    /// The tool's text reads back through the type unchanged, a float stays a
    /// float and a secret stays a flag.
    #[test]
    fn an_answer_reads_back_as_it_was_written() {
        let mut read = SettingsRead {
            has_hf_token: true,
            chat_profile: Some(4),
            ..Default::default()
        };
        read.forge_tokens
            .insert("github.com".into(), "<set>".into());
        read.docs_search.rrf_k = 60.0;
        let text = serde_json::to_string_pretty(&read).unwrap();
        assert!(text.contains("\"rrf_k\": 60.0"), "{text}");
        let back: SettingsRead = serde_json::from_str(&text).unwrap();
        assert_eq!(back, read);
    }
}
