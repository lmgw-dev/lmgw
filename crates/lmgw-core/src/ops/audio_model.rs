//! Audio model CRUD (`audio_model_set`), shared by `/api/op/audio_model_set`
//! and `lmgw__audio_model_set` — moved out of `web/api.rs` so the tool plane
//! posts the same patch at the same function, the way the image class's
//! [`super::image_model_set`] has been since it was built.

use serde::Deserialize;
use serde_json::Value;

use crate::config::HoldFallbackMode;
use crate::runtime::{lifecycle, Class};
use crate::state::SharedState;
use crate::web::audio;

mod engine;

/// Audio model CRUD: a sparse patch (unknown fields are rejected) over the wider
/// audio.cpp field set, like the aux model patch. A save does not restart the
/// model's container; apply does. A save is validated: a task from the fixed
/// 13-value list, every voice preset naming a `voice_id`/`voice_ref`, a bare
/// `default_voice_preset` resolving to a declared preset, and every `voice_ref`
/// under `/models/...` existing on disk — audiocpp_server opens reference clips
/// at startup and exits if one is missing.
// Validation itself lives in `crate::web::audio`, next to the container it protects.
#[derive(Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AudioPatch {
    action: String,
    id: Option<i64>,
    model_id: Option<String>,
    family: Option<String>,
    path: Option<String>,
    task: Option<String>,
    mode: Option<String>,
    load_options: Option<serde_json::Map<String, Value>>,
    session_options: Option<serde_json::Map<String, Value>>,
    /// Request-option defaults for every call to this model (audio.cpp's
    /// `default_request_options`); a request that names one still wins.
    default_request_options: Option<serde_json::Map<String, Value>>,
    /// Per-model override of the class's `lazy_load`. `None` keeps the current
    /// value; name `lazy` in `clear` to go back to inheriting the class.
    lazy: Option<bool>,
    /// Per-model `busy_timeout_ms` (and the ceiling a request's own value is
    /// clamped to), in ms; `0` means "wait forever". Same clear convention as
    /// `lazy`.
    busy_timeout_ms: Option<i64>,
    /// Where this row runs: `cpu` runs it on the CPU — no VRAM charged,
    /// never evicted, served while the GPU hold is on, no residency learned.
    /// Blank (or `backend` in `clear`) inherits the class's backend. No other
    /// value is accepted: another GPU backend is the class setting.
    backend: Option<String>,
    /// The row's thread count, 1 or more (no upper cap; above this machine's
    /// CPUs the save says so). `threads` in `clear` inherits: this machine's
    /// physical cores for a row switched to the CPU itself, the class's
    /// count for every other row (one inheriting a class backend of `cpu`
    /// included).
    threads: Option<i64>,
    /// A `<family>.json` spec, or a directory of them, relative to the audio
    /// models dir — empty clears back to the image's own catalog.
    model_spec_override: Option<String>,
    /// Named config/weights asset ids for a model directory holding several;
    /// empty clears.
    config_id: Option<String>,
    weight_id: Option<String>,
    voice_presets: Option<serde_json::Map<String, Value>>,
    /// `None` = not supplied (update keeps the current value); an explicit
    /// blank string clears it back to `None`, the same "empty clears"
    /// convention the aux patch's `pooling` uses for a plain string field.
    default_voice_preset: Option<Value>,
    enabled: Option<bool>,
    /// Per-model container image override; empty clears back to inheriting the audio class settings' image.
    image: Option<String>,
    /// `podman run` args override; replaces the current value
    /// wholesale when supplied. `None` keeps the current value; an empty list
    /// is no override and inherits the class settings, as naming
    /// `extra_run_args` in `clear` does (same convention as the aux patch's `clear`).
    extra_run_args: Option<Vec<String>>,
    /// Start this model's own container at app launch.
    warm_start: Option<bool>,
    /// GPU-hold fallback mode: `inherit` (default,
    /// means "no fallback" for audio) | `none` | `alias`.
    hold_fallback_mode: Option<String>,
    /// The alias `hold_fallback_mode = "alias"` routes a held request to.
    hold_fallback: Option<String>,
    /// Field names to reset to unset, comma- or space-separated — see
    /// the aux patch's `clear`. `residency` (on `update`) forgets the
    /// learned residency, so the row is charged at its on-disk size until a
    /// request teaches it again.
    clear: Option<String>,
}

fn check_task(task: &str) -> Result<(), String> {
    if audio::AUDIO_TASKS.contains(&task) {
        Ok(())
    } else {
        Err(format!(
            "unknown task '{task}' (expected one of: {})",
            audio::AUDIO_TASKS.join(", ")
        ))
    }
}

/// A negative `busy_timeout_ms` is rejected by audiocpp_server **at startup**,
/// so saving one would trade an error message here for a container that never
/// comes up. `0` is legal and means "wait forever" — the engine's own way of
/// disabling the guard.
fn check_busy_timeout(ms: Option<i64>) -> Result<Option<i64>, String> {
    match ms {
        Some(v) if v < 0 => {
            Err("busy timeout: must be 0 or more milliseconds (0 = wait forever, no guard)".into())
        }
        other => Ok(other),
    }
}

/// The spec override is a path under the audio models dir, checked the same
/// way the model path is: it is mounted into the container, and `..` out of
/// the mount would name a file the container cannot read anyway.
fn check_spec_override(path: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = path
        .map(|p| p.trim().trim_matches('/'))
        .filter(|p| !p.is_empty())
    else {
        return Ok(None);
    };
    crate::ops::check_rel_path(raw, "audio models dir")?;
    Ok(Some(raw.to_string()))
}

/// Every preset value needs a `voice_id`/`voice_ref`; a `default_voice_preset`
/// naming a bare preset must resolve to one of them — the rules themselves are
/// `crate::web::audio`'s, applied here over the already-typed JSON map.
fn check_presets(
    presets: &serde_json::Map<String, Value>,
    default: Option<&Value>,
) -> Result<(), String> {
    for (name, p) in presets {
        let obj = p
            .as_object()
            .ok_or_else(|| format!("voice preset '{name}': must be a JSON object"))?;
        audio::check_voice_preset(&format!("voice preset '{name}'"), obj)?;
    }
    if let Some(v) = default {
        audio::check_default_voice_preset(v, presets)?;
    }
    Ok(())
}

/// A row whose task is not the one its package's variant runs (a Qwen3-TTS
/// VoiceDesign package saved as `tts`) is refused with the fix spelled out
/// (audio-class gap 2) — never corrected behind the owner's back. A package
/// lmgw cannot read (not downloaded yet, no variant) is not judged.
async fn check_variant_task(
    st: &SharedState,
    row: &crate::store::NewAudioModel,
) -> Result<(), String> {
    let models_dir = st.snapshot().settings.audio.models_dir.trim().to_string();
    if models_dir.is_empty() {
        return Ok(());
    }
    let models = std::path::PathBuf::from(models_dir);
    let root = models.join(row.path.trim_start_matches('/'));
    let (family, weight_id) = (row.family.clone(), row.weight_id.clone());
    let variant = tokio::task::spawn_blocking(move || {
        crate::audio::variant::package_variant(&models, &root, &family, weight_id.as_deref())
    })
    .await
    .ok()
    .flatten();
    match variant.and_then(|v| crate::audio::variant::mismatch(&row.model_id, &row.task, &v)) {
        Some(why) => Err(why),
        None => Ok(()),
    }
}

/// `Some("")` clears `default_voice_preset` back to `None` on update; `None`
/// (the field absent from the patch) keeps the current value.
fn overlay_default_preset(p: &Option<Value>, cur: Option<Value>) -> Option<Value> {
    match p {
        Some(Value::String(s)) if s.trim().is_empty() => None,
        Some(v) => Some(v.clone()),
        None => cur,
    }
}

pub async fn audio_model_set(st: &SharedState, p: AudioPatch) -> Result<Value, String> {
    use crate::store;
    let snap = st.snapshot();
    let reload = || async { st.reload_snapshot().await.map_err(|e| e.to_string()) };
    match p.action.as_str() {
        "create" => {
            let model_id = p
                .model_id
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or("create requires model_id")?;
            crate::ops::refuse_if_candidate_alias_name(&snap, &snap.audio_public_name(&model_id))?;
            let family = p
                .family
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or("create requires family")?;
            let path = p
                .path
                .as_deref()
                .map(|s| s.trim().trim_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .ok_or("create requires path")?;
            crate::ops::check_rel_path(&path, "audio models dir")?;
            let task = p
                .task
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or("create requires task")?;
            check_task(&task)?;
            let voice_presets = p.voice_presets.clone().unwrap_or_default();
            // Same "blank clears" convention `overlay_default_preset` applies on
            // update: the Leptos form always supplies this field, so an explicit
            // `Some("")` (no default chosen) must not be validated as a literal
            // empty-named preset — there is no "current" value to fall back to on
            // create, so the overlay's `cur` is unconditionally `None`.
            let default_voice_preset = overlay_default_preset(&p.default_voice_preset, None);
            check_presets(&voice_presets, default_voice_preset.as_ref())?;
            let (hold_fallback_mode, hold_fallback) = crate::ops::resolve_hold_fallback_text(
                &st.snapshot(),
                p.hold_fallback_mode.as_deref(),
                p.hold_fallback.as_deref(),
                p.clear.as_deref(),
                (HoldFallbackMode::Inherit, None),
            )?;
            let new = store::NewAudioModel {
                model_id: model_id.clone(),
                family,
                path,
                task,
                mode: match p.mode.as_deref() {
                    Some("streaming") => "streaming".to_string(),
                    _ => "offline".to_string(),
                },
                // A field named in `clear` is unset on create too, whatever
                // value came with it — the dashboard's editor sends a blank
                // field as its empty value *and* its name in `clear`. An empty
                // `extra_run_args` is no override even without the clear
                // (`ops::run_args_override`).
                lazy: if crate::ops::clear_has(p.clear.as_deref(), "lazy") {
                    None
                } else {
                    p.lazy
                },
                busy_timeout_ms: if crate::ops::clear_has(p.clear.as_deref(), "busy_timeout_ms") {
                    None
                } else {
                    check_busy_timeout(p.busy_timeout_ms)?
                },
                backend: engine::backend(
                    p.backend.as_deref(),
                    crate::ops::clear_has(p.clear.as_deref(), "backend"),
                    None,
                )?,
                threads: engine::threads(
                    p.threads,
                    crate::ops::clear_has(p.clear.as_deref(), "threads"),
                    None,
                )?,
                load_options: p.load_options.clone().unwrap_or_default(),
                session_options: p.session_options.clone().unwrap_or_default(),
                default_request_options: p.default_request_options.clone().unwrap_or_default(),
                model_spec_override: check_spec_override(p.model_spec_override.as_deref())?,
                config_id: p.config_id.clone().filter(|s| !s.trim().is_empty()),
                weight_id: p.weight_id.clone().filter(|s| !s.trim().is_empty()),
                voice_presets,
                default_voice_preset,
                enabled: p.enabled.unwrap_or(true),
                image: p.image.clone().filter(|s| !s.is_empty()),
                extra_run_args: if crate::ops::clear_has(p.clear.as_deref(), "extra_run_args") {
                    None
                } else {
                    crate::ops::run_args_override(p.extra_run_args.clone())
                },
                warm_start: p.warm_start.unwrap_or(false),
                hold_fallback_mode,
                hold_fallback,
            };
            audio::check_voice_refs(st, &new)?;
            check_variant_task(st, &new).await?;
            let id = store::insert_audio_model(&st.db, &new)
                .await
                .map_err(|e| e.to_string())?;
            reload().await?;
            let threads = engine::note(new.threads, crate::host::cpu())
                .map(|n| format!("; {n}"))
                .unwrap_or_default();
            Ok(serde_json::json!({
                "ok": true, "id": id,
                "message": format!(
                    "audio model '{model_id}' created — start it to serve it{threads}"
                ),
            }))
        }
        "update" | "enable" | "disable" => {
            let id = p.id.ok_or("pass id")?;
            let cur = store::get_audio_model(&st.db, id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no audio model with id {id}"))?;
            // See `aux_model_set`: the running container is named after the
            // pre-patch id.
            let cur_model_id = cur.model_id.clone();
            let path = match p.path.as_deref() {
                Some(s) => s.trim().trim_matches('/').to_string(),
                None => cur.path,
            };
            crate::ops::check_rel_path(&path, "audio models dir")?;
            let task = p.task.clone().unwrap_or(cur.task);
            check_task(&task)?;
            let voice_presets = p.voice_presets.clone().unwrap_or(cur.voice_presets);
            let default_voice_preset =
                overlay_default_preset(&p.default_voice_preset, cur.default_voice_preset);
            check_presets(&voice_presets, default_voice_preset.as_ref())?;
            let enabled = match p.action.as_str() {
                "enable" => true,
                "disable" => false,
                _ => p.enabled.unwrap_or(cur.enabled),
            };
            let (hold_fallback_mode, hold_fallback) = crate::ops::resolve_hold_fallback_text(
                &st.snapshot(),
                p.hold_fallback_mode.as_deref(),
                p.hold_fallback.as_deref(),
                p.clear.as_deref(),
                (cur.hold_fallback_mode, cur.hold_fallback.clone()),
            )?;
            let new_audio_model_id = p
                .model_id
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(cur.model_id);
            crate::ops::refuse_if_candidate_alias_name(
                &snap,
                &snap.audio_public_name(&new_audio_model_id),
            )?;
            let upd = store::NewAudioModel {
                model_id: new_audio_model_id,
                family: p
                    .family
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(cur.family),
                path,
                task,
                mode: match p.mode.as_deref() {
                    Some("streaming") => "streaming".to_string(),
                    Some(_) => "offline".to_string(),
                    None => cur.mode,
                },
                lazy: if crate::ops::clear_has(p.clear.as_deref(), "lazy") {
                    None
                } else {
                    p.lazy.or(cur.lazy)
                },
                busy_timeout_ms: if crate::ops::clear_has(p.clear.as_deref(), "busy_timeout_ms") {
                    None
                } else {
                    check_busy_timeout(p.busy_timeout_ms)?.or(cur.busy_timeout_ms)
                },
                backend: engine::backend(
                    p.backend.as_deref(),
                    crate::ops::clear_has(p.clear.as_deref(), "backend"),
                    cur.backend.clone(),
                )?,
                threads: engine::threads(
                    p.threads,
                    crate::ops::clear_has(p.clear.as_deref(), "threads"),
                    cur.threads,
                )?,
                load_options: p.load_options.clone().unwrap_or(cur.load_options),
                session_options: p.session_options.clone().unwrap_or(cur.session_options),
                default_request_options: p
                    .default_request_options
                    .clone()
                    .unwrap_or(cur.default_request_options),
                model_spec_override: match p.model_spec_override.as_deref() {
                    Some(s) if s.trim().is_empty() => None,
                    Some(_) => check_spec_override(p.model_spec_override.as_deref())?,
                    None => cur.model_spec_override,
                },
                config_id: match p.config_id.clone() {
                    Some(s) if s.trim().is_empty() => None,
                    Some(s) => Some(s),
                    None => cur.config_id,
                },
                weight_id: match p.weight_id.clone() {
                    Some(s) if s.trim().is_empty() => None,
                    Some(s) => Some(s),
                    None => cur.weight_id,
                },
                voice_presets,
                default_voice_preset,
                enabled,
                image: match p.image.clone() {
                    Some(s) if s.is_empty() => None,
                    Some(s) => Some(s),
                    None => cur.image,
                },
                // An empty list inherits the class, as a clear does.
                extra_run_args: if crate::ops::clear_has(p.clear.as_deref(), "extra_run_args") {
                    None
                } else {
                    crate::ops::run_args_override(p.extra_run_args.clone().or(cur.extra_run_args))
                },
                warm_start: p.warm_start.unwrap_or(cur.warm_start),
                hold_fallback_mode,
                hold_fallback,
            };
            audio::check_voice_refs(st, &upd)?;
            // Enabling or disabling a row saved before this check is never
            // refused for it; an edit has to fix it.
            if p.action == "update" {
                check_variant_task(st, &upd).await?;
            }
            store::update_audio_model(&st.db, id, &upd)
                .await
                .map_err(|e| e.to_string())?;
            // The owner's reset of the learned residency (realtime design
            // §9.4): back to the on-disk charge until a request teaches it
            // again. A configuration change needs no reset — the figure is
            // keyed and simply stops being charged.
            let residency_reset =
                crate::ops::clear_has(p.clear.as_deref(), "residency") && cur.residency.is_some();
            if residency_reset {
                // Under the residency's store lock, so a reading that began
                // before the reset cannot write its figure back after it.
                st.vram.reset_audio_residency(st, id, &cur_model_id).await?;
            }
            reload().await?;
            // Container hygiene (per-model-containers §3.4/§3.6). Audio's
            // per-model `server.json` is rendered fresh at every start, so
            // stopping the container *is* the apply. A rename is the exception
            // (see the chat path in `ops::local_model_set`): the old container
            // — and the old per-model config dir it mounts, which is keyed on
            // the model id too — belong to a model that no longer exists.
            let note = if upd.model_id != cur_model_id {
                lifecycle::drop_model(st, Class::Audio, &cur_model_id).await;
                Some(format!(
                    "it was renamed from '{cur_model_id}', so that container was stopped and \
                     removed — the next request starts '{}' fresh",
                    upd.model_id
                ))
            } else if enabled {
                lifecycle::stop_for_apply(st, Class::Audio, &cur_model_id).await
            } else {
                lifecycle::drop_model(st, Class::Audio, &cur_model_id).await;
                Some("it is disabled — its container was stopped and removed".to_string())
            };
            let mut reset = if residency_reset {
                "; its learned residency was reset, so it is charged at its on-disk size until a \
                 request teaches it again"
                    .to_string()
            } else {
                String::new()
            };
            if let Some(n) = engine::note(upd.threads, crate::host::cpu()) {
                reset.push_str(&format!("; {n}"));
            }
            Ok(serde_json::json!({
                "ok": true, "id": id,
                "residency_reset": residency_reset,
                "message": match note {
                    Some(note) => format!("audio model updated — {note}{reset}"),
                    None => format!("audio model updated — it is not running, so the next request \
                                     starts it with the new configuration{reset}"),
                },
            }))
        }
        "delete" => {
            let id = p.id.ok_or("pass id")?;
            let cur = store::get_audio_model(&st.db, id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no audio model with id {id}"))?;
            store::delete_audio_model(&st.db, id)
                .await
                .map_err(|e| e.to_string())?;
            reload().await?;
            // §3.4: immediately, not at the next boot.
            lifecycle::drop_model(st, Class::Audio, &cur.model_id).await;
            Ok(serde_json::json!({
                "ok": true,
                "message": "audio model deleted — its container was stopped and removed",
            }))
        }
        other => Err(format!(
            "unknown action '{other}' (create, update, delete, enable, disable)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use crate::store;

    /// Args as `audio_model_set` receives them from `POST /api/op/audio_model_set`.
    fn patch(args: serde_json::Value) -> AudioPatch {
        crate::ops::patch_from_args(Some(args.as_object().unwrap().clone())).unwrap()
    }

    /// A fresh in-memory state with `settings.audio.models_dir` pointed at a
    /// scratch dir, so `check_voice_refs` has somewhere real to look.
    async fn state_with_models_dir() -> (SharedState, tempfile::TempDir) {
        let state = AppState::init_for_tests().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut settings = state.snapshot().settings.clone();
        settings.audio.models_dir = dir.path().display().to_string();
        store::save_settings(&state.db, &settings).await.unwrap();
        state.reload_snapshot().await.unwrap();
        (state, dir)
    }

    fn minimal_new_model() -> store::NewAudioModel {
        store::NewAudioModel {
            model_id: "fish".into(),
            family: "fish_audio".into(),
            path: "Fish-Audio-S2-Pro-GGUF".into(),
            task: "tts".into(),
            mode: "offline".into(),
            lazy: None,
            busy_timeout_ms: None,
            backend: None,
            threads: None,
            load_options: Default::default(),
            session_options: Default::default(),
            default_request_options: Default::default(),
            model_spec_override: None,
            config_id: None,
            weight_id: None,
            voice_presets: Default::default(),
            default_voice_preset: None,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: HoldFallbackMode::Inherit,
            hold_fallback: None,
        }
    }

    #[tokio::test]
    async fn create_happy_path_inserts_and_reloads() {
        let (state, _dir) = state_with_models_dir().await;
        let res = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create",
                "model_id": "fish",
                "family": "fish_audio",
                "path": "Fish-Audio-S2-Pro-GGUF",
                "task": "tts",
            })),
        )
        .await
        .unwrap();
        assert_eq!(res["ok"], true);
        let id = res["id"].as_i64().unwrap();
        let saved = store::get_audio_model(&state.db, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.model_id, "fish");
        assert_eq!(saved.family, "fish_audio");
        assert_eq!(saved.path, "Fish-Audio-S2-Pro-GGUF");
        assert_eq!(saved.task, "tts");
        assert_eq!(saved.mode, "offline");
        assert!(saved.enabled);
    }

    /// The Leptos editor always sends `default_voice_preset` — a blank one
    /// means "no default", not a preset literally named "".
    #[tokio::test]
    async fn create_treats_a_blank_default_preset_as_none() {
        let (state, _dir) = state_with_models_dir().await;
        let res = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create",
                "model_id": "pocket",
                "family": "pocket_tts",
                "path": "PocketTTS-GGUF/english",
                "task": "tts",
                "voice_presets": {"me": {"voice_id": "narrator"}},
                "default_voice_preset": "",
            })),
        )
        .await
        .unwrap();
        assert_eq!(res["ok"], true);
        let saved = store::get_audio_model(&state.db, res["id"].as_i64().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(saved.default_voice_preset.is_none());
    }

    /// The engine keys added with audio.cpp's own: they round-trip through the
    /// row, and a blank one on update means *inherit the class* — a reset
    /// named in `clear`, not a zero written into the column (0 is a real
    /// value there: "wait forever").
    #[tokio::test]
    async fn per_model_engine_keys_round_trip_and_clear_back_to_inherit() {
        let (state, _dir) = state_with_models_dir().await;
        let res = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create",
                "model_id": "yue2",
                "family": "yue2",
                "path": "YuE2-GGUF",
                "task": "gen",
                "lazy": false,
                "busy_timeout_ms": 900_000,
                "default_request_options": {"cot": "full"},
                "model_spec_override": "specs/yue2.json",
                "config_id": "base",
                "weight_id": "q8_0",
            })),
        )
        .await
        .unwrap();
        let id = res["id"].as_i64().unwrap();
        let saved = store::get_audio_model(&state.db, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.lazy, Some(false));
        assert_eq!(saved.busy_timeout_ms, Some(900_000));
        assert_eq!(saved.default_request_options["cot"], "full");
        assert_eq!(
            saved.model_spec_override.as_deref(),
            Some("specs/yue2.json")
        );
        assert_eq!(saved.config_id.as_deref(), Some("base"));
        assert_eq!(saved.weight_id.as_deref(), Some("q8_0"));

        audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "update",
                "id": id,
                "clear": "lazy busy_timeout_ms",
                "model_spec_override": "",
                "config_id": "",
                "weight_id": "",
            })),
        )
        .await
        .unwrap();
        let saved = store::get_audio_model(&state.db, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.lazy, None, "back to the class's lazy_load");
        assert_eq!(saved.busy_timeout_ms, None);
        assert!(saved.model_spec_override.is_none());
        assert!(saved.config_id.is_none() && saved.weight_id.is_none());
        assert_eq!(
            saved.default_request_options["cot"], "full",
            "a field the patch left out keeps its value"
        );
    }

    /// audiocpp_server refuses a negative `busy_timeout_ms` at startup, so a
    /// saved one would cost a container that never comes up.
    #[tokio::test]
    async fn create_rejects_a_negative_busy_timeout() {
        let (state, _dir) = state_with_models_dir().await;
        let err = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create", "model_id": "x", "family": "f", "path": "p", "task": "tts",
                "busy_timeout_ms": -1,
            })),
        )
        .await
        .unwrap_err();
        assert!(err.contains("busy timeout"), "{err}");
    }

    /// A spec override is mounted into the container like the model path, so
    /// it is bounded the same way.
    #[tokio::test]
    async fn create_rejects_a_spec_override_outside_the_models_dir() {
        let (state, _dir) = state_with_models_dir().await;
        let err = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create", "model_id": "x", "family": "f", "path": "p", "task": "tts",
                "model_spec_override": "../../etc/passwd",
            })),
        )
        .await
        .unwrap_err();
        assert!(!err.is_empty(), "a traversing path must not be saved");
    }

    #[tokio::test]
    async fn create_rejects_unknown_task() {
        let (state, _dir) = state_with_models_dir().await;
        let err = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create", "model_id": "x", "family": "f", "path": "p",
                "task": "bogus",
            })),
        )
        .await
        .unwrap_err();
        assert!(err.contains("unknown task"), "{err}");
    }

    #[tokio::test]
    async fn create_rejects_a_voice_preset_without_a_voice() {
        let (state, _dir) = state_with_models_dir().await;
        let err = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create", "model_id": "x", "family": "f", "path": "p", "task": "tts",
                "voice_presets": {"narrator": {"reference_text": "hi"}},
            })),
        )
        .await
        .unwrap_err();
        assert!(err.contains("voice_id"), "{err}");
    }

    #[tokio::test]
    async fn create_rejects_a_default_naming_an_undeclared_preset() {
        let (state, _dir) = state_with_models_dir().await;
        let err = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create", "model_id": "x", "family": "f", "path": "p", "task": "tts",
                "voice_presets": {"narrator": {"voice_id": "alba"}},
                "default_voice_preset": "narrater",
            })),
        )
        .await
        .unwrap_err();
        assert!(err.contains("narrater"), "{err}");
    }

    /// audiocpp_server opens every `voice_ref` at startup — a missing file
    /// there must fail the save, not surface as a dead container later.
    #[tokio::test]
    async fn create_rejects_a_voice_ref_to_a_missing_file() {
        let (state, _dir) = state_with_models_dir().await;
        let err = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create", "model_id": "x", "family": "f", "path": "p", "task": "tts",
                "voice_presets": {"narrator": {"voice_ref": "/models/voices/missing.wav"}},
            })),
        )
        .await
        .unwrap_err();
        assert!(err.contains("no such file"), "{err}");
    }

    #[tokio::test]
    async fn create_accepts_a_voice_ref_that_exists_on_disk() {
        let (state, dir) = state_with_models_dir().await;
        std::fs::create_dir_all(dir.path().join("voices")).unwrap();
        std::fs::write(dir.path().join("voices").join("n.wav"), b"fake wav").unwrap();
        let res = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create", "model_id": "x", "family": "f", "path": "p", "task": "tts",
                "voice_presets": {"narrator": {"voice_ref": "/models/voices/n.wav"}},
                "default_voice_preset": "narrator",
            })),
        )
        .await
        .unwrap();
        assert_eq!(res["ok"], true);
    }

    /// A sparse update patch (only `enabled`) must not clobber the fields it
    /// omits — the whole point of overlaying onto the current row instead of
    /// requiring a full replace.
    #[tokio::test]
    async fn update_overlays_only_supplied_fields() {
        let (state, _dir) = state_with_models_dir().await;
        let id = store::insert_audio_model(&state.db, &minimal_new_model())
            .await
            .unwrap();

        let res = audio_model_set(
            &state,
            patch(serde_json::json!({ "action": "update", "id": id, "enabled": false })),
        )
        .await
        .unwrap();
        assert_eq!(res["ok"], true);

        let saved = store::get_audio_model(&state.db, id)
            .await
            .unwrap()
            .unwrap();
        assert!(!saved.enabled);
        assert_eq!(saved.model_id, "fish");
        assert_eq!(saved.family, "fish_audio");
        assert_eq!(saved.path, "Fish-Audio-S2-Pro-GGUF");
        assert_eq!(saved.task, "tts");
        assert_eq!(saved.mode, "offline");
    }

    /// The merged result is re-validated on update too: overlaying an unknown
    /// task onto an otherwise-valid row must still be rejected.
    #[tokio::test]
    async fn update_revalidates_the_merged_result() {
        let (state, _dir) = state_with_models_dir().await;
        let id = store::insert_audio_model(&state.db, &minimal_new_model())
            .await
            .unwrap();

        let err = audio_model_set(
            &state,
            patch(serde_json::json!({ "action": "update", "id": id, "task": "bogus" })),
        )
        .await
        .unwrap_err();
        assert!(err.contains("unknown task"), "{err}");
    }

    #[tokio::test]
    async fn delete_removes_the_row() {
        let (state, _dir) = state_with_models_dir().await;
        let id = store::insert_audio_model(&state.db, &minimal_new_model())
            .await
            .unwrap();

        let res = audio_model_set(
            &state,
            patch(serde_json::json!({ "action": "delete", "id": id })),
        )
        .await
        .unwrap();
        assert_eq!(res["ok"], true);
        assert!(store::get_audio_model(&state.db, id)
            .await
            .unwrap()
            .is_none());
    }
}
