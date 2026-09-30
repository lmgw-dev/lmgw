//! Audio lab: a "test the audio routes" playground, the Chat page's
//! counterpart for audio.cpp models. The panels (TTS + ASR), voice library and
//! result player are the SPA's Audio lab page; this module is its server
//! (`/audio-lab/api/*`).
//!
//! Synthesis and transcription are dispatched **in-process** through the very
//! handlers that serve `POST /v1/audio/speech` and `/v1/audio/transcriptions`
//! ([`proxy::handle_audio_speech`] / [`proxy::handle_audio_transcription`]), so
//! what the panel exercises is the real passthrough — alias resolution, model
//! rewrite, upstream error normalization and telemetry all included, and the
//! turns show up in Logs. Only the auth middleware and the HTTP hop are skipped
//! (same trade as the Chat tab, which drives `proxy::drive_upstream` directly).
//!
//! Reference audio is the one piece with no route behind it: audio.cpp takes
//! `voice_ref` as a path *on the server*, not an upload. The audio models dir
//! is bind-mounted into the container at `/models`, so a clip written under
//! `<models_dir>/voices/` on the host is readable in-container as
//! `/models/voices/<name>` — the voice library below is exactly that directory.

use axum::body::Body;
use axum::extract::{Multipart, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::proxy::{self, RequestCtx};
use crate::state::SharedState;
use crate::store;

/// Subdirectory of the audio models dir holding uploaded reference clips.
/// Inside the container this is `/models/voices` (the models dir is mounted at
/// `/models`), which is what goes into a request's `voice_ref`.
const VOICES_SUBDIR: &str = "voices";
/// The same directory as the container sees it.
const CONTAINER_VOICES_DIR: &str = "/models/voices";
/// audio.cpp's transcript index for a voice library: one
/// `<basename-without-extension>|<transcript>` line per clip, read when a
/// request's `voice` resolves to `<voice_dir>/<name>.wav` so the clone gets
/// its `reference_text` without the caller repeating it.
///
/// Stored in the engine's own format rather than in a sidecar of lmgw's
/// invention: this directory *is* the `voice_dir` the class settings point
/// at, so the file has to be the one audiocpp_server reads.
const PROMPT_TEXT_FILE: &str = "prompt_text";

// ---------------------------------------------------------------------------
// Model list
// ---------------------------------------------------------------------------

/// `GET /audio-lab/api/models` — the enabled audio models with the metadata the
/// panel needs to pick a form: `task` decides TTS vs ASR vs "generic", `mode`
/// decides whether streaming is offered. `GET /v1/models` can't answer this —
/// it flattens every upstream to `{id, context_length, pricing}` — but the
/// gateway owns the `audio_models` rows that generated `server.json`, so the
/// authoritative answer is local. The container's state rides along so the
/// panel can say "not running" instead of failing every request.
pub async fn list_models(State(state): State<SharedState>) -> Response {
    let snap = state.snapshot();
    let s = &snap.settings.audio;
    let prefix = s.public_prefix.trim_matches('/').to_string();
    let models = store::list_audio_models(&state.db)
        .await
        .unwrap_or_default();
    let out: Vec<Value> = models
        .iter()
        .filter(|m| m.enabled)
        .map(|m| {
            let alias = if prefix.is_empty() {
                m.model_id.clone()
            } else {
                format!("{prefix}/{}", m.model_id)
            };
            json!({
                "alias": alias,
                "model_id": m.model_id,
                "family": m.family,
                "task": m.task,
                "mode": m.mode,
            })
        })
        .collect();
    // Per-model containers (§3.2): "is audio up" is no longer one answer, so
    // the panel gets the runtime rows for this class and decides per model.
    let runtime: Vec<_> = state
        .runtime()
        .list()
        .into_iter()
        .filter(|v| v.class == crate::runtime::Class::Audio)
        .collect();
    Json(json!({
        "models": out,
        "runtime": runtime,
        // Surfaced so the panel can explain where a stored clip actually lives.
        "voices_dir": voices_dir(&state).map(|p| p.display().to_string()),
        "container_voices_dir": CONTAINER_VOICES_DIR,
        // Whether the class's `voice_dir` actually points at this library —
        // which decides whether a clip is only a `voice_ref` path or also a
        // *voice name* every TTS model in the class answers to.
        "voice_library_active": s.voice_dir.trim() == CONTAINER_VOICES_DIR,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Voice presets (upstream `GET /v1/audio/voices`)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct VoicesQuery {
    model: String,
    /// `1`/`true`: start the model if it is not running. Without it the list
    /// is only read from a container that is already up.
    #[serde(default)]
    start: Option<String>,
}

/// `GET /audio-lab/api/voices?model=<alias>[&start=1]` — the model's built-in
/// voice ids and configured server presets, for the voice picker. An empty
/// list is normal: it means the model carries no cached voice ids and wants a
/// `voice_ref` clip instead.
///
/// The answer comes out of the model's own container, and admission starts
/// that container when it is down. Here that would mean *opening the page*
/// loads a model onto the GPU, so without `start` the list is read only from
/// a container that is up and answering ([`proxy::audio_voices_if_running`]:
/// no admission, no claim, no dead-container restart). A local model that is
/// not running answers `{"voices": [], "running": false}` instead, and the
/// panel offers an explicit "load voices" that sends `start=1`. The public
/// `GET /v1/audio/voices` keeps admitting: a client asking for voices there is
/// asking the model.
pub async fn list_voices(
    State(state): State<SharedState>,
    Query(q): Query<VoicesQuery>,
) -> Response {
    let alias = q.model.trim();
    let start = q
        .start
        .as_deref()
        .is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"));
    if start {
        return proxy::handle_audio_voices(state, alias).await;
    }
    proxy::audio_voices_if_running(state, alias)
        .await
        .unwrap_or_else(|| Json(json!({ "voices": [], "running": false })).into_response())
}

// ---------------------------------------------------------------------------
// Voice library — reference clips under <models_dir>/voices
// ---------------------------------------------------------------------------

fn voices_dir(state: &SharedState) -> Option<std::path::PathBuf> {
    let dir = state
        .snapshot()
        .settings
        .audio
        .models_dir
        .trim()
        .to_string();
    if dir.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(dir).join(VOICES_SUBDIR))
}

/// Accept only a plain file name built from safe characters. This is a path
/// that ends up inside a container request, so anything that could climb out of
/// the voices dir (`..`, separators, NUL) is rejected outright rather than
/// sanitized into something the user didn't name.
fn safe_clip_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("empty file name".into());
    }
    if name.starts_with('.') {
        return Err("file name may not start with '.'".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' '))
    {
        return Err(format!(
            "invalid file name `{name}` — use letters, digits, space, '.', '_' or '-'"
        ));
    }
    Ok(name.to_string())
}

/// The transcript index as a map, or empty when there is none. Malformed
/// lines are skipped rather than failing the listing — the file is editable by
/// hand and a bad line should cost that line, not the library.
fn read_prompt_text(dir: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    let Ok(raw) = std::fs::read_to_string(dir.join(PROMPT_TEXT_FILE)) else {
        return Default::default();
    };
    raw.lines()
        .filter_map(|line| {
            let (name, text) = line.split_once('|')?;
            let name = name.trim();
            (!name.is_empty()).then(|| (name.to_string(), text.trim().to_string()))
        })
        .collect()
}

/// Write the index back, atomically (tmp + rename inside the directory the
/// container has mounted, so it never reads a half-written file). An empty map
/// removes the file: no index at all is what "no transcripts" means to
/// audiocpp_server.
fn write_prompt_text(
    dir: &std::path::Path,
    map: &std::collections::BTreeMap<String, String>,
) -> std::io::Result<()> {
    let path = dir.join(PROMPT_TEXT_FILE);
    if map.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        };
    }
    let body: String = map
        .iter()
        .map(|(name, text)| format!("{name}|{text}\n"))
        .collect();
    let tmp = dir.join(format!(".{PROMPT_TEXT_FILE}.tmp"));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)
}

/// The voice name a clip answers to: its file name without the extension,
/// which is the key both `prompt_text` and a request's `voice` use.
fn voice_name(file_name: &str) -> &str {
    file_name
        .rsplit_once('.')
        .map(|(n, _)| n)
        .unwrap_or(file_name)
}

fn clip_entries(dir: &std::path::Path) -> Vec<Value> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let texts = read_prompt_text(dir);
    let mut out: Vec<Value> = rd
        .filter_map(Result::ok)
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            // The transcript index lives in the same directory and is not a
            // clip — audio.cpp's own layout puts it there, so it has to be
            // skipped here rather than renamed out of the way.
            if name == PROMPT_TEXT_FILE || safe_clip_name(&name).is_err() {
                return None;
            }
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            let voice = voice_name(&name).to_string();
            let transcript = texts.get(&voice).cloned().unwrap_or_default();
            Some(json!({
                "name": name,
                "size": size,
                // What goes into `voice_ref` / `audio`: the container's view.
                "server_path": format!("{CONTAINER_VOICES_DIR}/{name}"),
                // What a request's `voice` names, once the class points
                // `voice_dir` at this directory.
                "voice": voice,
                "transcript": transcript,
            }))
        })
        .collect();
    out.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });
    out
}

/// `GET /audio-lab/api/refs` — the stored reference clips.
pub async fn list_refs(State(state): State<SharedState>) -> Response {
    let Some(dir) = voices_dir(&state) else {
        return Json(json!({
            "clips": [],
            "error": "audio models dir is not configured (Settings → Runtimes → Audio)",
        }))
        .into_response();
    };
    Json(json!({ "clips": clip_entries(&dir) })).into_response()
}

/// `POST /audio-lab/api/refs` — multipart upload of one or more clips into the
/// voices dir. Body size is deliberately unbounded (the route disables axum's
/// limit): a reference clip or a recording to transcribe is as big as it is,
/// and a guessed cap would reject legitimate audio silently.
pub async fn upload_ref(State(state): State<SharedState>, mut mp: Multipart) -> Response {
    let Some(dir) = voices_dir(&state) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "audio models dir is not configured (Settings → Runtimes → Audio)" })),
        )
            .into_response();
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("creating {}: {e}", dir.display()) })),
        )
            .into_response();
    }
    let mut saved: Vec<String> = Vec::new();
    let mut transcript: Option<String> = None;
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("invalid multipart body: {e}") })),
                )
                    .into_response()
            }
        };
        let Some(file_name) = field.file_name().map(String::from) else {
            // A plain form value: the only one this route reads is the
            // transcript, which becomes the clip's `prompt_text` line.
            if field.name() == Some("transcript") {
                transcript = field.text().await.ok().map(|t| t.trim().to_string());
            }
            continue;
        };
        let name = match safe_clip_name(&file_name) {
            Ok(n) => n,
            Err(e) => {
                return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response()
            }
        };
        let bytes = match field.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("reading upload: {e}") })),
                )
                    .into_response()
            }
        };
        // tmp + rename, so the container never sees a half-written clip.
        let tmp = dir.join(format!(".{name}.part"));
        if let Err(e) = std::fs::write(&tmp, &bytes) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("writing {}: {e}", tmp.display()) })),
            )
                .into_response();
        }
        if let Err(e) = std::fs::rename(&tmp, dir.join(&name)) {
            let _ = std::fs::remove_file(&tmp);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("saving {name}: {e}") })),
            )
                .into_response();
        }
        saved.push(name);
    }
    if saved.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no file field in the upload" })),
        )
            .into_response();
    }
    // One transcript belongs to one clip: with several files in the same
    // upload there is no way to say which, so it is only applied when the
    // upload is unambiguous.
    if let (Some(text), [only]) = (transcript.filter(|t| !t.is_empty()), saved.as_slice()) {
        let mut texts = read_prompt_text(&dir);
        texts.insert(voice_name(only).to_string(), text);
        if let Err(e) = write_prompt_text(&dir, &texts) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("writing {PROMPT_TEXT_FILE}: {e}") })),
            )
                .into_response();
        }
    }
    Json(json!({ "saved": saved, "clips": clip_entries(&dir) })).into_response()
}

/// `POST /audio-lab/api/refs/{name}/delete`.
pub async fn delete_ref(State(state): State<SharedState>, Path(name): Path<String>) -> Response {
    let (Some(dir), Ok(name)) = (voices_dir(&state), safe_clip_name(&name)) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid clip" })),
        )
            .into_response();
    };
    match std::fs::remove_file(dir.join(&name)) {
        Ok(()) => {
            // A transcript for a clip that is gone would silently re-attach
            // itself to the next clip uploaded under the same name.
            let mut texts = read_prompt_text(&dir);
            if texts.remove(voice_name(&name)).is_some() {
                let _ = write_prompt_text(&dir, &texts);
            }
            Json(json!({ "ok": true, "clips": clip_entries(&dir) })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `POST /audio-lab/api/refs/{name}/text` — set (or, with an empty string,
/// drop) the clip's transcript in the library's `prompt_text` index.
///
/// This is what makes a library *voice* work rather than just a path: with
/// the class's `voice_dir` pointing here, a request `"voice": "<name>"`
/// clones `<name>.wav` and audiocpp_server injects this line as the
/// `reference_text` the caller did not send. A cloning model without one
/// reads the target text in an unconditioned voice.
pub async fn set_ref_text(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let (Some(dir), Ok(name)) = (voices_dir(&state), safe_clip_name(&name)) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid clip" })),
        )
            .into_response();
    };
    if !dir.join(&name).is_file() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no clip named {name}") })),
        )
            .into_response();
    }
    let text = body
        .get("transcript")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        // The index is one line per clip, so a pasted newline would split it
        // into a line that names no clip and a clip that lost its text.
        .replace(['\n', '\r'], " ");
    let mut texts = read_prompt_text(&dir);
    let voice = voice_name(&name).to_string();
    match text.is_empty() {
        true => {
            texts.remove(&voice);
        }
        false => {
            texts.insert(voice, text);
        }
    }
    match write_prompt_text(&dir, &texts) {
        Ok(()) => Json(json!({ "ok": true, "clips": clip_entries(&dir) })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("writing {PROMPT_TEXT_FILE}: {e}") })),
        )
            .into_response(),
    }
}

/// `GET /audio-lab/api/refs/{name}` — serve a stored clip back so the panel can
/// preview it in an `<audio>` element.
pub async fn get_ref(State(state): State<SharedState>, Path(name): Path<String>) -> Response {
    let (Some(dir), Ok(name)) = (voices_dir(&state), safe_clip_name(&name)) else {
        return (StatusCode::BAD_REQUEST, "invalid clip").into_response();
    };
    let path = dir.join(&name);
    match std::fs::read(&path) {
        Ok(bytes) => (
            [(
                header::CONTENT_TYPE,
                mime_guess::from_path(&path)
                    .first_or_octet_stream()
                    .as_ref()
                    .to_string(),
            )],
            bytes,
        )
            .into_response(),
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// The routes under test
// ---------------------------------------------------------------------------

/// `POST /audio-lab/api/speech` — the panel's synthesis call, handed straight to
/// the `/v1/audio/speech` handler. The response (a WAV body, a JSON envelope or
/// an SSE stream, per `response_format` / `stream_format`) is relayed verbatim.
pub async fn speech(State(state): State<SharedState>, Json(body): Json<Value>) -> Response {
    proxy::handle_audio_speech(state, RequestCtx::default(), body).await
}

/// `POST /audio-lab/api/transcriptions` — same idea for `/v1/audio/transcriptions`.
/// Both request shapes the real route accepts work here: a JSON body naming a
/// server-local `audio` path, or a multipart upload.
///
/// `?details=1` sends the same request to `/v1/audio/transcriptions/details`
/// instead, which answers with the word timings, segments and speaker turns a
/// model produced — the lab is where you find out whether *this* model
/// produces any.
pub async fn transcriptions(
    State(state): State<SharedState>,
    Query(q): Query<DetailsQuery>,
    req: Request<Body>,
) -> Response {
    let which = if q.details {
        proxy::AudioUpload::TranscriptionDetails
    } else {
        proxy::AudioUpload::Transcription
    };
    proxy::handle_audio_upload(state, RequestCtx::default(), req, which).await
}

#[derive(Deserialize, Default)]
pub struct DetailsQuery {
    #[serde(default, deserialize_with = "de_flag")]
    details: bool,
}

/// `POST /audio-lab/api/alignments` — the multipart forced-alignment route,
/// for an `align` model. Upload-only, exactly like `/v1/audio/alignments`.
pub async fn alignments(State(state): State<SharedState>, req: Request<Body>) -> Response {
    proxy::handle_audio_upload(
        state,
        RequestCtx::default(),
        req,
        proxy::AudioUpload::Alignment,
    )
    .await
}

/// `POST /audio-lab/api/tasks/run` — the generic task route behind the twelve
/// tasks with no OpenAI shape. `?stream=1` selects `/v1/tasks/stream` instead,
/// which a streaming-mode model answers with its buffered event list.
pub async fn tasks_run(
    State(state): State<SharedState>,
    Query(q): Query<TaskRunQuery>,
    Json(body): Json<Value>,
) -> Response {
    let ctx = RequestCtx::default();
    if q.stream {
        proxy::handle_task_stream(state, ctx, body).await
    } else {
        proxy::handle_task_run(state, ctx, body).await
    }
}

#[derive(Deserialize, Default)]
pub struct TaskRunQuery {
    #[serde(default, deserialize_with = "de_flag")]
    stream: bool,
}

/// `?stream=1` / `?stream=true` / absent — a bare query flag, which serde's
/// bool parser alone would reject for `1`.
fn de_flag<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let s = String::deserialize(d)?;
    Ok(matches!(s.as_str(), "1" | "true" | "yes" | "on"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_names_cannot_escape_the_voices_dir() {
        assert!(safe_clip_name("demo_01_man.wav").is_ok());
        assert!(safe_clip_name("My Voice 2.wav").is_ok());
        assert!(safe_clip_name("../../etc/passwd").is_err());
        assert!(safe_clip_name("a/b.wav").is_err());
        assert!(safe_clip_name(".hidden").is_err());
        assert!(safe_clip_name("").is_err());
        assert!(safe_clip_name("nul\0.wav").is_err());
    }
}
