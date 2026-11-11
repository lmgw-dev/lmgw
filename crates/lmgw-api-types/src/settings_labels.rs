//! The Settings page's names for the places a gateway message sends the
//! owner to: its categories, cards and setting labels, and the paths built
//! from them ([`SettingPath`], shown as "Settings → GPU → Admission → Load
//! timeout").
//!
//! One definition for both sides, so a message and the page cannot name a
//! setting differently: the dashboard's table (lmgw-ui `pages/settings`)
//! builds its rail, cards and rows from these names, and lmgw-core's
//! refusals and notes point at a setting by its [`SettingPath`], never by its
//! config key (`vram.load_timeout_seconds`), which the dashboard does not
//! show. A test of the table checks that every path in [`PATHS`] is one of
//! its rows, so moving a row to another card fails there too.

use std::fmt;

// The categories, as the Settings rail names them.

/// The listeners, the access settings and the device link.
pub const NETWORK: &str = "Network & access";
/// Admission control and the GPU hold.
pub const GPU: &str = "GPU";
/// The container defaults every model of a class inherits.
pub const RUNTIMES: &str = "Runtimes";
/// Container builds: forge tokens, the builds directory, the update check
/// (the category's slug and route stay `backends`).
pub const ENGINES: &str = "Engines";
/// The Chat's prompt, attachments, knowledge, voice and media.
pub const CHAT: &str = "Chat";
/// Spoken conversations on `/v1/realtime` and WebRTC calls.
pub const REALTIME: &str = "Realtime";

// The cards a path goes through.

/// Network & access → Gateway: the plain listener.
pub const GATEWAY: &str = "Gateway";
/// Realtime → WebRTC calls.
pub const WEBRTC_CALLS: &str = "WebRTC calls";
/// Network & access → TLS: the main listener's TLS and the public address.
pub const TLS: &str = "TLS";
/// Network & access → Accounts: who signs in (headless server design §5).
pub const ACCOUNTS: &str = "Accounts";
/// Network & access → Devices: the device listener and pairing.
pub const DEVICES: &str = "Devices";
/// GPU → Admission.
pub const ADMISSION: &str = "Admission";
/// Runtimes → the chat class (llama-server).
pub const CHAT_CLASS: &str = "Chat";
/// Runtimes → the aux class (embeddings, rerank, decisions).
pub const AUX_CLASS: &str = "Aux";
/// Runtimes → the audio class (audio.cpp).
pub const AUDIO_CLASS: &str = "Audio";
/// Runtimes → the image class (stable-diffusion.cpp).
pub const IMAGE_CLASS: &str = "Image";
/// Engines → Forge tokens: the card, and the one setting on it.
pub const FORGE_TOKENS: &str = "Forge tokens";
/// Engines → Builds.
pub const BUILDS: &str = "Builds";
/// Realtime → Voice cascade.
pub const VOICE_CASCADE: &str = "Voice cascade";
/// Chat → Voice.
pub const CHAT_VOICE: &str = "Voice";

// The settings.

/// `bind_addr`.
pub const BIND_ADDRESS: &str = "Bind address";
/// `realtime.webrtc_port`.
pub const UDP_PORT: &str = "UDP port";
/// `realtime.webrtc_tls_port`.
pub const TLS_UDP_PORT: &str = "TLS UDP port";
/// `tls.cert_path`.
pub const TLS_CERTIFICATE: &str = "Certificate";
/// `tls.key_path`.
pub const TLS_PRIVATE_KEY: &str = "Private key";
/// `tls.listen`.
pub const TLS_ADDRESS: &str = "TLS address";
/// `tls.handshake_timeout_s`.
pub const TLS_HANDSHAKE_TIMEOUT: &str = "Handshake timeout";
/// `public_url`.
pub const PUBLIC_URL: &str = "Public URL";
/// The accounts' switch and card (headless server design §5.6, §5.11).
pub const SIGN_IN_ACCOUNTS: &str = "Sign-in accounts";
/// `accounts.session_days`.
pub const SESSION_LIFETIME: &str = "Session lifetime";
/// `accounts.audit_days`.
pub const AUDIT_KEEP: &str = "Keep audit entries";
/// The audit log's list on the Accounts card (§5.8).
pub const AUDIT_LOG: &str = "Audit log";
/// `trusted_proxies`.
pub const TRUSTED_PROXIES: &str = "Trusted proxies";
/// `devices.listener`, `devices.listen` and `devices.port`: one row.
pub const DEVICE_LISTENER: &str = "Device listener";
/// `vram.load_timeout_seconds`.
pub const LOAD_TIMEOUT: &str = "Load timeout";
/// `vram.unload_timeout_seconds`.
pub const UNLOAD_TIMEOUT: &str = "Unload timeout";
/// `vram.queue_timeout_seconds`.
pub const QUEUE_TIMEOUT: &str = "Queue timeout";
/// `vram.enabled`.
pub const ARBITRATE_GPU_MEMORY: &str = "Arbitrate GPU memory";
/// `vram.budget_mb`.
pub const BUDGET: &str = "Budget";
/// `vram.fallback_on_external`.
pub const FALLBACK_ON_EXTERNAL: &str = "Fall back when VRAM outside lmgw's control is short";
/// Each class's `models_dir`.
pub const MODELS_DIR: &str = "Models dir";
/// Each class's `image`: the container image its models run.
pub const IMAGE: &str = "Image";
/// `audio.catalog_revision`.
pub const CATALOG_DOWNLOADS: &str = "Catalog downloads";
/// `audio.voice_transcribe_alias`.
pub const CLIP_TRANSCRIPTS: &str = "Clip transcripts";
/// `builds_dir`.
pub const BUILDS_DIR: &str = "Builds directory";
/// `realtime.default_model`.
pub const CHAT_MODEL: &str = "Chat model";
/// `realtime.asr_alias`, and the Chat's `chat_stt_alias`.
pub const SPEECH_TO_TEXT: &str = "Speech to text";
/// `realtime.tts_alias`, and the Chat's `chat_tts_alias`.
pub const TEXT_TO_SPEECH: &str = "Text to speech";
/// Each class's `extra_run_args`.
pub const EXTRA_RUN_ARGS: &str = "Extra podman run args";
/// `audio.backend`: the engine's backend, which must match its image.
pub const BACKEND: &str = "Backend";
/// A model row's own `image`, on its page (not a Settings row).
pub const IMAGE_OVERRIDE: &str = "Image override";
/// A model row's own `extra_run_args`, on its page (not a Settings row).
pub const RUN_ARGS_OVERRIDE: &str = "Extra podman run args override";

/// Where a setting is on the Settings page: its category, its card and,
/// when the card holds more than that one setting, its label — shown as
/// "Settings → GPU → Admission → Load timeout".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingPath(&'static [&'static str]);

impl SettingPath {
    /// The parts after "Settings": the category, the card, and the label
    /// unless the card is the setting.
    pub const fn parts(self) -> &'static [&'static str] {
        self.0
    }
}

impl fmt::Display for SettingPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Settings")?;
        for part in self.0 {
            write!(f, " → {part}")?;
        }
        Ok(())
    }
}

/// Network & access → Gateway → Bind address.
pub const BIND_ADDRESS_AT: SettingPath = SettingPath(&[NETWORK, GATEWAY, BIND_ADDRESS]);
/// Realtime → WebRTC calls → UDP port.
pub const WEBRTC_PORT_AT: SettingPath = SettingPath(&[REALTIME, WEBRTC_CALLS, UDP_PORT]);
/// Realtime → WebRTC calls → TLS UDP port.
pub const WEBRTC_TLS_PORT_AT: SettingPath = SettingPath(&[REALTIME, WEBRTC_CALLS, TLS_UDP_PORT]);
/// Network & access → TLS → Certificate.
pub const TLS_CERT_AT: SettingPath = SettingPath(&[NETWORK, TLS, TLS_CERTIFICATE]);
/// Network & access → TLS → Private key.
pub const TLS_KEY_AT: SettingPath = SettingPath(&[NETWORK, TLS, TLS_PRIVATE_KEY]);
/// Network & access → TLS → TLS address.
pub const TLS_LISTEN_AT: SettingPath = SettingPath(&[NETWORK, TLS, TLS_ADDRESS]);
/// Network & access → TLS → Handshake timeout.
pub const TLS_HANDSHAKE_TIMEOUT_AT: SettingPath =
    SettingPath(&[NETWORK, TLS, TLS_HANDSHAKE_TIMEOUT]);
/// Network & access → TLS → Public URL.
pub const PUBLIC_URL_AT: SettingPath = SettingPath(&[NETWORK, TLS, PUBLIC_URL]);
/// Network & access → TLS → Trusted proxies.
pub const TRUSTED_PROXIES_AT: SettingPath = SettingPath(&[NETWORK, TLS, TRUSTED_PROXIES]);
/// Network & access → Accounts → Sign-in accounts.
pub const SIGN_IN_ACCOUNTS_AT: SettingPath = SettingPath(&[NETWORK, ACCOUNTS, SIGN_IN_ACCOUNTS]);
/// Network & access → Accounts → Session lifetime.
pub const SESSION_LIFETIME_AT: SettingPath = SettingPath(&[NETWORK, ACCOUNTS, SESSION_LIFETIME]);
/// Network & access → Accounts → Keep audit entries.
pub const AUDIT_KEEP_AT: SettingPath = SettingPath(&[NETWORK, ACCOUNTS, AUDIT_KEEP]);
/// Network & access → Devices → Device listener.
pub const DEVICE_LISTENER_AT: SettingPath = SettingPath(&[NETWORK, DEVICES, DEVICE_LISTENER]);
/// GPU → Admission → Load timeout.
pub const LOAD_TIMEOUT_AT: SettingPath = SettingPath(&[GPU, ADMISSION, LOAD_TIMEOUT]);
/// GPU → Admission → Unload timeout.
pub const UNLOAD_TIMEOUT_AT: SettingPath = SettingPath(&[GPU, ADMISSION, UNLOAD_TIMEOUT]);
/// GPU → Admission → Queue timeout.
pub const QUEUE_TIMEOUT_AT: SettingPath = SettingPath(&[GPU, ADMISSION, QUEUE_TIMEOUT]);
/// GPU → Admission → Arbitrate GPU memory.
pub const ADMISSION_AT: SettingPath = SettingPath(&[GPU, ADMISSION, ARBITRATE_GPU_MEMORY]);
/// GPU → Admission → Budget.
pub const BUDGET_AT: SettingPath = SettingPath(&[GPU, ADMISSION, BUDGET]);
/// GPU → Admission → Fall back when VRAM outside lmgw's control is short.
pub const FALLBACK_ON_EXTERNAL_AT: SettingPath =
    SettingPath(&[GPU, ADMISSION, FALLBACK_ON_EXTERNAL]);
/// Runtimes → Chat → Models dir.
pub const CHAT_MODELS_DIR_AT: SettingPath = SettingPath(&[RUNTIMES, CHAT_CLASS, MODELS_DIR]);
/// Runtimes → Aux → Models dir.
pub const AUX_MODELS_DIR_AT: SettingPath = SettingPath(&[RUNTIMES, AUX_CLASS, MODELS_DIR]);
/// Runtimes → Audio → Models dir.
pub const AUDIO_MODELS_DIR_AT: SettingPath = SettingPath(&[RUNTIMES, AUDIO_CLASS, MODELS_DIR]);
/// Runtimes → Image → Models dir.
pub const IMAGE_MODELS_DIR_AT: SettingPath = SettingPath(&[RUNTIMES, IMAGE_CLASS, MODELS_DIR]);
/// Runtimes → Chat → Image.
pub const CHAT_IMAGE_AT: SettingPath = SettingPath(&[RUNTIMES, CHAT_CLASS, IMAGE]);
/// Runtimes → Aux → Image.
pub const AUX_IMAGE_AT: SettingPath = SettingPath(&[RUNTIMES, AUX_CLASS, IMAGE]);
/// Runtimes → Audio → Image.
pub const AUDIO_IMAGE_AT: SettingPath = SettingPath(&[RUNTIMES, AUDIO_CLASS, IMAGE]);
/// Runtimes → Image → Image.
pub const IMAGE_IMAGE_AT: SettingPath = SettingPath(&[RUNTIMES, IMAGE_CLASS, IMAGE]);
/// Runtimes → Audio → Catalog downloads.
pub const CATALOG_DOWNLOADS_AT: SettingPath =
    SettingPath(&[RUNTIMES, AUDIO_CLASS, CATALOG_DOWNLOADS]);
/// Runtimes → Audio → Clip transcripts.
pub const CLIP_TRANSCRIPTS_AT: SettingPath =
    SettingPath(&[RUNTIMES, AUDIO_CLASS, CLIP_TRANSCRIPTS]);
/// Engines → Forge tokens.
pub const FORGE_TOKENS_AT: SettingPath = SettingPath(&[ENGINES, FORGE_TOKENS]);
/// Engines → Builds → Builds directory.
pub const BUILDS_DIR_AT: SettingPath = SettingPath(&[ENGINES, BUILDS, BUILDS_DIR]);
/// Realtime → Voice cascade → Chat model.
pub const REALTIME_CHAT_MODEL_AT: SettingPath = SettingPath(&[REALTIME, VOICE_CASCADE, CHAT_MODEL]);
/// Realtime → Voice cascade → Speech to text.
pub const REALTIME_ASR_AT: SettingPath = SettingPath(&[REALTIME, VOICE_CASCADE, SPEECH_TO_TEXT]);
/// Realtime → Voice cascade → Text to speech.
pub const REALTIME_TTS_AT: SettingPath = SettingPath(&[REALTIME, VOICE_CASCADE, TEXT_TO_SPEECH]);
/// Chat → Voice → Speech to text.
pub const CHAT_STT_AT: SettingPath = SettingPath(&[CHAT, CHAT_VOICE, SPEECH_TO_TEXT]);
/// Runtimes → Chat → Extra podman run args.
pub const CHAT_RUN_ARGS_AT: SettingPath = SettingPath(&[RUNTIMES, CHAT_CLASS, EXTRA_RUN_ARGS]);
/// Runtimes → Aux → Extra podman run args.
pub const AUX_RUN_ARGS_AT: SettingPath = SettingPath(&[RUNTIMES, AUX_CLASS, EXTRA_RUN_ARGS]);
/// Runtimes → Audio → Extra podman run args.
pub const AUDIO_RUN_ARGS_AT: SettingPath = SettingPath(&[RUNTIMES, AUDIO_CLASS, EXTRA_RUN_ARGS]);
/// Runtimes → Image → Extra podman run args.
pub const IMAGE_RUN_ARGS_AT: SettingPath = SettingPath(&[RUNTIMES, IMAGE_CLASS, EXTRA_RUN_ARGS]);
/// Runtimes → Audio → Backend.
pub const AUDIO_BACKEND_AT: SettingPath = SettingPath(&[RUNTIMES, AUDIO_CLASS, BACKEND]);

/// The models dir of download target (or class) `target` — `chat`, `aux`,
/// `audio`, `image`; anything else is chat's, as the gateway's models-dir
/// lookup reads it.
pub fn models_dir_at(target: &str) -> SettingPath {
    match target {
        "aux" => AUX_MODELS_DIR_AT,
        "audio" => AUDIO_MODELS_DIR_AT,
        "image" => IMAGE_MODELS_DIR_AT,
        _ => CHAT_MODELS_DIR_AT,
    }
}

/// The image setting of class `class` — `chat`, `aux`, `audio`, `image`;
/// anything else is chat's, as [`models_dir_at`] reads it.
pub fn image_at(class: &str) -> SettingPath {
    match class {
        "aux" => AUX_IMAGE_AT,
        "audio" => AUDIO_IMAGE_AT,
        "image" => IMAGE_IMAGE_AT,
        _ => CHAT_IMAGE_AT,
    }
}

/// The run arguments of class `class`, as [`image_at`] reads it.
pub fn run_args_at(class: &str) -> SettingPath {
    match class {
        "aux" => AUX_RUN_ARGS_AT,
        "audio" => AUDIO_RUN_ARGS_AT,
        "image" => IMAGE_RUN_ARGS_AT,
        _ => CHAT_RUN_ARGS_AT,
    }
}

/// Every path above, for the dashboard's check that each is a row it has.
pub const PATHS: &[SettingPath] = &[
    BIND_ADDRESS_AT,
    WEBRTC_PORT_AT,
    WEBRTC_TLS_PORT_AT,
    TLS_CERT_AT,
    TLS_KEY_AT,
    TLS_LISTEN_AT,
    TLS_HANDSHAKE_TIMEOUT_AT,
    PUBLIC_URL_AT,
    TRUSTED_PROXIES_AT,
    SIGN_IN_ACCOUNTS_AT,
    SESSION_LIFETIME_AT,
    AUDIT_KEEP_AT,
    DEVICE_LISTENER_AT,
    LOAD_TIMEOUT_AT,
    UNLOAD_TIMEOUT_AT,
    QUEUE_TIMEOUT_AT,
    ADMISSION_AT,
    BUDGET_AT,
    FALLBACK_ON_EXTERNAL_AT,
    CHAT_MODELS_DIR_AT,
    AUX_MODELS_DIR_AT,
    AUDIO_MODELS_DIR_AT,
    IMAGE_MODELS_DIR_AT,
    CHAT_IMAGE_AT,
    AUX_IMAGE_AT,
    AUDIO_IMAGE_AT,
    IMAGE_IMAGE_AT,
    CATALOG_DOWNLOADS_AT,
    CLIP_TRANSCRIPTS_AT,
    FORGE_TOKENS_AT,
    BUILDS_DIR_AT,
    REALTIME_CHAT_MODEL_AT,
    REALTIME_ASR_AT,
    REALTIME_TTS_AT,
    CHAT_STT_AT,
    CHAT_RUN_ARGS_AT,
    AUX_RUN_ARGS_AT,
    AUDIO_RUN_ARGS_AT,
    IMAGE_RUN_ARGS_AT,
    AUDIO_BACKEND_AT,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_reads_from_settings_down() {
        assert_eq!(
            LOAD_TIMEOUT_AT.to_string(),
            "Settings → GPU → Admission → Load timeout"
        );
        assert_eq!(
            FORGE_TOKENS_AT.to_string(),
            "Settings → Engines → Forge tokens"
        );
        assert_eq!(
            models_dir_at("audio").to_string(),
            "Settings → Runtimes → Audio → Models dir"
        );
        assert_eq!(models_dir_at("other"), CHAT_MODELS_DIR_AT);
        assert_eq!(
            image_at("audio").to_string(),
            "Settings → Runtimes → Audio → Image"
        );
        assert_eq!(image_at("other"), CHAT_IMAGE_AT);
        assert_eq!(
            run_args_at("image").to_string(),
            "Settings → Runtimes → Image → Extra podman run args"
        );
        assert_eq!(run_args_at("other"), CHAT_RUN_ARGS_AT);
    }
}
