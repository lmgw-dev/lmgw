//! The audio class's catalog and read-back on the tool plane: what
//! `lmgw__audio_catalog` and `lmgw__local_model_get target=audio` answer.
//! The dashboard reads the same catalog (`GET /api/audio/catalog`) and posts
//! the same refresh and download (`/api/op/audio_catalog`).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::config::AudioModel;
use crate::runtime::argv;
use crate::state::SharedState;
use crate::web::audio;

/// `lmgw__audio_catalog`: `list` the audio.cpp catalog cached on this
/// machine (no network call), `refresh` it from audio.cpp's `model_specs`,
/// or `download` one package's files.
///
/// `list` answers every family and package. A family's option schema is
/// long, so it comes only for the families `family` names; the listing says
/// so on each family it left out, rather than leaving the field empty.
pub async fn audio_catalog(
    state: &SharedState,
    action: &str,
    family: Option<&str>,
    package: Option<&str>,
    search: Option<&str>,
) -> Result<Value, String> {
    let family = family.map(str::trim).filter(|s| !s.is_empty());
    match action {
        "list" => Ok(list(state, family, search).await),
        "refresh" => {
            let snapshot = audio::catalog_refresh(state).await?;
            let families = snapshot.specs.len();
            // Counted in the message, listed in full beside it: a refresh
            // that could not list some repos still succeeded.
            let message = match snapshot.warnings.len() {
                0 => format!("catalog refreshed — {families} families"),
                n => format!(
                    "catalog refreshed — {families} families, {n} warning{} (see warnings; the \
                     catalog shows them until the next refresh)",
                    if n == 1 { "" } else { "s" }
                ),
            };
            Ok(json!({
                "ok": true,
                "families": families,
                "fetched_at": snapshot.fetched_at,
                "warnings": snapshot.warnings,
                "message": message,
            }))
        }
        "download" => {
            let package = package.map(str::trim).unwrap_or_default();
            let (Some(family), false) = (family, package.is_empty()) else {
                return Err("pass family and package".into());
            };
            let install = audio::catalog_download(state, family, package).await?;
            serde_json::to_value(install).map_err(|e| e.to_string())
        }
        other => Err(format!(
            "unknown action '{other}' (list, refresh, download)"
        )),
    }
}

async fn list(state: &SharedState, family: Option<&str>, search: Option<&str>) -> Value {
    let catalog = audio::catalog(state).await;
    if catalog.fetched_at.is_empty() {
        return json!({
            "fetched_at": null,
            "families": [],
            "message": "the audio.cpp catalog has not been fetched on this machine — call \
                        lmgw__audio_catalog action=refresh first",
        });
    }
    let needle = search
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    let matches = |f: &lmgw_api_types::AudioFamily| {
        family.is_none_or(|want| f.family == want)
            && needle.as_ref().is_none_or(|n| {
                let hay = format!(
                    "{} {} {} {} {}",
                    f.family,
                    f.display_name,
                    f.description,
                    f.tasks.join(" "),
                    f.packages
                        .iter()
                        .map(|p| p.id.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                )
                .to_ascii_lowercase();
                hay.contains(n.as_str())
            })
    };
    let families: Vec<Value> = catalog
        .families
        .iter()
        .filter(|f| matches(f))
        .map(|f| {
            let mut v = serde_json::to_value(f).unwrap_or_default();
            if family.is_none() {
                v["options"] = json!(format!(
                    "left out of the full listing — lmgw__audio_catalog action=list \
                     family={} answers its load, session and request options",
                    f.family
                ));
            }
            v
        })
        .collect();
    json!({
        "fetched_at": catalog.fetched_at,
        "families": families,
        "warnings": catalog.warnings,
        "message": if families.is_empty() {
            "no family matches".to_string()
        } else {
            format!(
                "{} famil{} — a package that is 'installed' can be served now; its \
                 suggested_* fields are what lmgw__audio_model_set action=create takes",
                families.len(),
                if families.len() == 1 { "y" } else { "ies" }
            )
        },
    })
}

/// One audio row, read back for `lmgw__local_model_get`: every stored field,
/// whether its model directory is on disk, what its container is started
/// with (the rendered `server.json`, and the image and run args in effect),
/// where it runs (`runs_on`, `threads_in_effect`, `threads_source`), whether
/// it runs, and its learned residency with the sentence that says what it is
/// charged. `None` when no row matches.
pub(super) async fn audio_model_get(
    state: &SharedState,
    id: Option<i64>,
    model_id: Option<&str>,
) -> Result<Option<Value>, String> {
    let snap = state.snapshot();
    let found: Option<&AudioModel> = match (id, model_id) {
        (Some(id), _) => snap.audio_models.iter().find(|m| m.id == id),
        (None, Some(mid)) => snap.audio_models.iter().find(|m| m.model_id == mid),
        (None, None) => return Err("pass id or model_id".into()),
    };
    let Some(m) = found else {
        return Ok(None);
    };
    let s = &snap.settings.audio;
    let models_dir = s.models_dir.clone();
    // A directory, or the GGUF file itself: audio.cpp loads either.
    let path_present =
        !models_dir.trim().is_empty() && std::path::Path::new(&models_dir).join(&m.path).exists();
    let mut problems: Vec<String> = Vec::new();
    if models_dir.trim().is_empty() {
        problems.push("the audio class has no models_dir configured".into());
    } else if !path_present {
        problems.push(format!("path '{}' is missing from the models dir", m.path));
    }
    if crate::runtime::audio::own_args_pass_gpu(m, s) {
        problems.push(crate::runtime::audio::OWN_ARGS_PASS_GPU.into());
    }
    if path_present {
        let root = crate::audio::files::row_root(&models_dir, m);
        let links =
            crate::audio::files::container_view::links_outside(Path::new(&models_dir), &root);
        if !links.is_empty() {
            problems.push(links_outside_problem(&links));
        }
    }
    // What a start renders, from the descriptor: the class settings with the
    // row's own backend and threads in effect, and the run args a CPU row
    // inherits without the GPU passthrough.
    let host = crate::host::cpu();
    let engine = crate::runtime::audio::engine_settings(m, s, host);
    let (threads, threads_source) = crate::runtime::audio::threads_in_effect(m, s, host);
    let server_json: Value = serde_json::from_str(
        &crate::runtime::audio::render_single_model_config(&engine, m),
    )
    .unwrap_or(Value::Null);
    let running = state
        .runtime()
        .list()
        .into_iter()
        .find(|e| e.class == crate::runtime::Class::Audio && e.model_id == m.model_id)
        .map(|e| json!({"state": e.state.as_str(), "in_flight": e.in_flight}));
    let residency_note = state.vram.audio_residency_note(state, &snap, m).await;
    let mut row = serde_json::to_value(m).map_err(|e| e.to_string())?;
    if let Some(o) = row.as_object_mut() {
        o.insert("class".into(), json!("audio"));
        o.insert(
            "public_name".into(),
            json!(snap.audio_public_name(&m.model_id)),
        );
        o.insert("path_present".into(), json!(path_present));
        o.insert(
            "extra_run_args".into(),
            json!(m.extra_run_args.as_ref().map(|a| argv::args_to_lines(a))),
        );
        // What the container is started with: the row's override, else the
        // class's — `image`/`extra_run_args` above are the override alone.
        o.insert(
            "effective_image".into(),
            json!(m.image.clone().unwrap_or_else(|| s.image.clone())),
        );
        o.insert(
            "effective_extra_run_args".into(),
            json!(argv::args_to_lines(&crate::runtime::audio::run_args(m, s))),
        );
        // Where it runs, and its thread count with where that comes from:
        // `row`, the audio `class`, or this machine's physical `cores`.
        o.insert(
            "runs_on".into(),
            json!(crate::runtime::audio::placement(m, s).as_str()),
        );
        o.insert("threads_in_effect".into(), json!(threads));
        o.insert("threads_source".into(), json!(threads_source.as_str()));
        o.insert("server_json".into(), server_json);
        o.insert("running".into(), running.unwrap_or(Value::Null));
        o.insert("residency_note".into(), json!(residency_note));
        o.insert(
            "residency_charged_bytes".into(),
            json!(crate::vram::residency::learned(m, s)),
        );
        o.insert("problems".into(), json!(problems));
        o.insert(
            "next_step".into(),
            json!(format!(
                "audio.cpp loads the model on its first request: POST /v1/audio/speech \
                 (task tts) or /v1/audio/transcriptions (task asr) with model '{}' proves \
                 it works; lmgw__container model={} action=logs shows a start that failed",
                snap.audio_public_name(&m.model_id),
                m.model_id
            )),
        );
    }
    Ok(Some(row))
}

/// The problem the links out of the models dir on the way to a row's GGUFs
/// make — a GGUF's, or the row directory's (`audio::files::container_view`):
/// the container lmgw starts mounts only the models dir, so audio.cpp finds
/// each of them dangling and does not count a GGUF behind it — and neither
/// does lmgw, in what it renders and shows. Said, because the row's own run
/// args can mount the target, which lmgw does not read.
fn links_outside_problem(links: &[(String, PathBuf)]) -> String {
    let named: Vec<String> = links
        .iter()
        .map(|(name, target)| format!("{name} -> {}", target.display()))
        .collect();
    format!(
        "{} link(s) to this row's GGUFs point outside the models directory, which is all the \
         container mounts (at /models): {}. audio.cpp finds them dangling, so neither it nor \
         lmgw counts a GGUF behind them, unless this row's own run args mount their target; \
         link them inside the models directory, relatively or under /models/, or copy the \
         files there",
        links.len(),
        named.join(", ")
    )
}
