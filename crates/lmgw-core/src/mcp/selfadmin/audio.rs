//! `lmgw__audio_catalog`, `lmgw__audio_model_set` and `lmgw__voice_transcribe`
//! dispatch: the flat tool arguments into what `ops::audio_catalog`,
//! `ops::audio_model_set` and `ops::voice_transcribe` take —
//! the same patch the dashboard posts, so a misspelt argument is refused by
//! name (`deny_unknown_fields`).
//!
//! What differs from the dashboard's shape, because every self-admin argument
//! is a flat scalar: the four option maps arrive as JSON strings, the
//! default voice as a preset name or a JSON string, and `extra_run_args` as
//! shell-quoted text. An empty one of these means "leave unchanged", as an
//! empty optional string does on every other tool; `{}` empties a map, and
//! `clear` resets the rest. And `model_id` alone selects the row to change.

use serde_json::{json, Map, Value};

use crate::ops;
use crate::runtime::argv;
use crate::state::SharedState;

use super::arg_str;

/// The maps that arrive as JSON strings.
const JSON_MAPS: [&str; 4] = [
    "load_options",
    "session_options",
    "default_request_options",
    "voice_presets",
];

pub(super) async fn run(
    state: &SharedState,
    name: &str,
    args: Option<Map<String, Value>>,
) -> Result<Value, String> {
    let mut a = args.unwrap_or_default();
    match name {
        "lmgw__audio_catalog" => {
            let action =
                arg_str(&a, "action")?.ok_or("action is required (list|refresh|download)")?;
            let family = arg_str(&a, "family")?;
            let mut v = ops::audio_catalog(
                state,
                action,
                family,
                arg_str(&a, "package")?,
                arg_str(&a, "search")?,
            )
            .await?;
            if action == "download" {
                v["next_step"] = json!(format!(
                    "follow the queued files with lmgw__hf_downloads until each says 'done', \
                     then lmgw__audio_model_set action=create with this package's \
                     suggested_model_id, suggested_path, suggested_task and suggested_mode — \
                     lmgw__audio_catalog action=list family={} shows them, and the options \
                     the family takes",
                    family.unwrap_or_default()
                ));
            }
            Ok(v)
        }
        "lmgw__audio_model_set" => {
            flatten(&mut a)?;
            select_by_model_id(state, &mut a);
            let created = a.get("action").and_then(Value::as_str) == Some("create");
            let mut v = ops::audio_model_set(state, ops::patch_from_args(Some(a.clone()))?).await?;
            if created {
                let model_id = a
                    .get("model_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let public = state.snapshot().audio_public_name(model_id);
                v["next_step"] = json!(format!(
                    "the container starts on the first request: POST /v1/audio/speech (task \
                     tts) or /v1/audio/transcriptions (task asr) with model '{public}' proves \
                     it loads; lmgw__local_model_get target=audio model_id={model_id} reads \
                     the row back, and lmgw__container model={model_id} action=logs shows a \
                     start that failed"
                ));
            }
            Ok(v)
        }
        "lmgw__voice_transcribe" => {
            ops::voice_transcribe(state, arg_str(&a, "clip")?, arg_str(&a, "alias")?).await
        }
        other => Err(format!("unhandled built-in tool '{other}'")),
    }
}

/// The flat projections back into the patch's own types.
fn flatten(a: &mut Map<String, Value>) -> Result<(), String> {
    for key in JSON_MAPS {
        let Some(Value::String(s)) = a.get(key) else {
            continue;
        };
        let s = s.trim();
        if s.is_empty() {
            a.remove(key);
            continue;
        }
        let parsed: Value =
            serde_json::from_str(s).map_err(|e| format!("{key}: not a JSON object ({e})"))?;
        if !parsed.is_object() {
            return Err(format!(
                "{key}: must be a JSON object, like '{{\"name\": value}}'"
            ));
        }
        a.insert(key.to_string(), parsed);
    }
    if let Some(Value::String(s)) = a.get("default_voice_preset") {
        let t = s.trim();
        if t.starts_with('{') {
            let parsed: Value = serde_json::from_str(t)
                .map_err(|e| format!("default_voice_preset: not a JSON object ({e})"))?;
            a.insert("default_voice_preset".into(), parsed);
        }
    }
    if let Some(Value::String(s)) = a.get("extra_run_args") {
        if s.trim().is_empty() {
            a.remove("extra_run_args");
        } else {
            let tokens = argv::parse_args_text(s).map_err(|e| format!("extra_run_args: {e}"))?;
            a.insert("extra_run_args".into(), json!(tokens));
        }
    }
    Ok(())
}

/// `model_id` without `id` selects the row an update, delete, enable or
/// disable is about — the way the image and aux tools find theirs. With `id`
/// it is the row's new name.
fn select_by_model_id(state: &SharedState, a: &mut Map<String, Value>) {
    let action = a.get("action").and_then(Value::as_str).unwrap_or_default();
    if action == "create" || a.get("id").is_some_and(|v| !v.is_null()) {
        return;
    }
    let Some(model_id) = a.get("model_id").and_then(Value::as_str) else {
        return;
    };
    let snap = state.snapshot();
    if let Some(row) = snap
        .audio_models
        .iter()
        .find(|m| m.model_id == model_id.trim())
    {
        a.insert("id".into(), json!(row.id));
        a.remove("model_id");
    }
}
