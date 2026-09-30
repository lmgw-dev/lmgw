//! audio.cpp `server.json` rendering, per model (design §3.6, "Audio
//! config").
//!
//! audio.cpp has no CLI-flag surface to render into, unlike llama-server
//! (`argv.rs`): a mounted `server.json` *is* its configuration. Each audio
//! model gets its own config *directory* — never shared with another model —
//! so its `server.json` can be mounted `:ro` at `/config` independently of
//! every other audio model's container.
//!
//! The renderer itself moved here from `crate::audio` when §7 deleted the
//! shared audiocpp_server manager. It still takes a *slice* of models even
//! though every caller now passes exactly one: the JSON shape (and its golden
//! tests) is unchanged by per-model containers, and a "single-model" config
//! is nothing more than the same renderer called with a one-element slice.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::config::{AudioModel, AudioSettings};

/// The config file audio.cpp reads, inside the mounted `/config` dir.
pub const CONFIG_FILE_NAME: &str = "server.json";
/// audiocpp_server's port inside the container.
const CONTAINER_PORT: u16 = 8080;

/// Render `server.json` from the class settings and the models it should
/// serve. Model paths are container paths (`/models/<path>`); `load_options`
/// / `session_options` / `default_request_options` / the voice keys are
/// omitted when empty so audiocpp's defaults apply.
///
/// The engine keys this deliberately does **not** render, so the omissions are
/// a decision rather than an oversight:
///
/// - `max_loaded_models` — one model per container since §3.6, so a residency
///   limit has nothing to choose between.
/// - `live_ingest` — bounds for `/v1/audio/*/live`, which the gateway does not
///   proxy (the body is a duplex chunked stream).
/// - `cors_origins`, `ui`, `ui_management`, `frontend_listener` — the
///   container is reachable from lmgw and nothing else; its browser surface
///   stays off.
/// - `log_request_body` — needs `--log` on the argv as well, and puts prompt
///   text in the container log.
pub fn render_server_config(s: &AudioSettings, models: &[AudioModel]) -> String {
    let models_json: Vec<Value> = models
        .iter()
        .filter(|m| m.enabled)
        .map(|m| {
            let mut o = json!({
                "id": m.model_id,
                "family": m.family,
                "path": format!("/models/{}", m.path.trim_start_matches('/')),
                "task": m.task,
                "mode": m.mode,
            });
            // Every key below is omitted when the row says nothing, so a model
            // that configures none of them renders the same bytes it rendered
            // before these columns existed — which is what keeps adoption
            // (§3.4) from evicting every container on the first boot after the
            // upgrade.
            if let Some(lazy) = m.lazy {
                o["lazy"] = Value::Bool(lazy);
            }
            if let Some(ms) = m.busy_timeout_ms {
                o["busy_timeout_ms"] = json!(ms);
            }
            if let Some(spec) = m.model_spec_override.as_deref().map(str::trim) {
                if !spec.is_empty() {
                    o["model_spec_override"] =
                        Value::String(format!("/models/{}", spec.trim_start_matches('/')));
                }
            }
            // `config` / `weight`, not `config_id` / `weight_id`: the server
            // reads the short spelling, the columns carry the long one.
            for (key, value) in [("config", &m.config_id), ("weight", &m.weight_id)] {
                if let Some(id) = value.as_deref().map(str::trim) {
                    if !id.is_empty() {
                        o[key] = Value::String(id.to_string());
                    }
                }
            }
            if !m.load_options.is_empty() {
                o["load_options"] = Value::Object(m.load_options.clone());
            }
            if !m.session_options.is_empty() {
                o["session_options"] = Value::Object(m.session_options.clone());
            }
            if !m.default_request_options.is_empty() {
                o["default_request_options"] = Value::Object(m.default_request_options.clone());
            }
            // Voice config: audiocpp injects the default preset into any
            // request that names no voice, which is what pins the speaker.
            if !m.voice_presets.is_empty() {
                o["voice_presets"] = Value::Object(m.voice_presets.clone());
            }
            if let Some(d) = &m.default_voice_preset {
                o["default_voice_preset"] = d.clone();
            }
            o
        })
        .collect();
    let mut cfg = json!({
        "host": "0.0.0.0",
        "port": CONTAINER_PORT,
        "backend": s.backend,
        "device": s.device,
        "threads": s.threads,
        "lazy_load": s.lazy_load,
        // Stated rather than inherited: these three bound how long a request
        // waits for a busy model, when an idle model gives its VRAM back, and
        // how little free memory a load may leave behind. `0` is the engine's
        // own "no bound" on each, so a zero here is a decision the file
        // records, not a value it omits.
        "busy_timeout_ms": s.busy_timeout_ms.max(0),
        "idle_unload_ms": s.idle_unload_ms.max(0),
        "min_free_memory_mb": s.min_free_memory_mb.max(0),
        "models": models_json,
    });
    // 0 = leave the key out, so audiocpp_server's own default (2 GiB) applies.
    if s.max_request_body_mb > 0 {
        cfg["max_request_body_bytes"] = json!(s.max_request_body_mb * 1024 * 1024);
    }
    // The shared voice library: a request's `voice` that matches no preset
    // resolves to `<voice_dir>/<name>.wav`, with its `prompt_text` line as the
    // reference transcript.
    let voice_dir = s.voice_dir.trim();
    if !voice_dir.is_empty() {
        cfg["voice_dir"] = Value::String(voice_dir.to_string());
    }
    // Map keys are BTree-ordered, so the output is deterministic.
    format!(
        "{}\n",
        serde_json::to_string_pretty(&cfg).unwrap_or_default()
    )
}

/// The per-model config dir for one audio model (§3.6): `<data_dir>/audiocpp/
/// <slug(model_id)>-<hash6(model_id)>/`. This is the directory mounted `:ro`
/// at `/config` — `server.json` lives inside it, never the mount source
/// itself.
///
/// **The `hash6` suffix is the same one [`super::container_name`] carries, and
/// it is there for the same reason**: [`super::slug`] is lossy by design, so
/// `"org/Voice.v2"` and `"org-voice-v2"` name one directory between them.
/// Two audio models sharing a config dir would each overwrite the other's
/// `server.json` at every start — and since adoption (§3.4) compares the
/// mounted config against what the model renders now, the survivor would also
/// evict the other's container on every boot. A container name that is
/// injective over a config path that is not would be an odd place to stop.
///
/// Deliberately under `data_dir/audiocpp/` — where the shared multi-model
/// `server.json` used to sit, directly at `audiocpp/server.json`: a per-model
/// subdirectory cannot collide with that filename, so an upgraded install
/// needs no cleanup here.
pub fn config_dir(data_dir: &Path, model_id: &str) -> PathBuf {
    data_dir.join("audiocpp").join(format!(
        "{}-{}",
        super::slug(model_id),
        super::hash6(model_id)
    ))
}

/// Render `server.json` for exactly one model — [`render_server_config`]
/// called with a one-element slice, not a second implementation that could
/// drift from it (byte-identical for the same model, proven in
/// `tests/it/runtime_descriptor.rs`).
pub fn render_single_model_config(s: &AudioSettings, model: &AudioModel) -> String {
    render_server_config(s, std::slice::from_ref(model))
}

/// Write one model's `server.json` atomically (tmp + rename inside the
/// mounted directory, so the container never sees a half-written file).
/// Returns the directory (what [`super::argv::RenderSpec::config_mount`]
/// wants), not the file path.
pub fn write_config(dir: &Path, json: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{CONFIG_FILE_NAME}.tmp"));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, dir.join(CONFIG_FILE_NAME))?;
    Ok(dir.to_path_buf())
}
