//! `/voice-lab/api/*`: the Voices lab's server (`crate::voice_lab` holds
//! what each route does). Owner-only ([`Cap::Admin`]), like the other labs:
//! a round spends compute or money on the owner's aliases, and a save writes
//! into the voice library every cloning model reads.
//!
//! Every refusal is the dashboard's [`lmgw_api_types::ApiError`]; the
//! cloning check relays the speech route's own answer, its OpenAI error
//! envelope included.
//!
//! [`Cap::Admin`]: crate::principal::Cap::Admin

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::voice_lab::{
    VoiceCloneCheck, VoiceGenerateArgs, VoiceModelQuery, VoiceSampleSaveArgs, VoiceSamples,
    VoiceSentenceSet,
};

use super::audio_lab::{clip_entries, voices_dir, CONTAINER_VOICES_DIR};
use super::chat::err_json;
use super::chat_extract::{ChatJson, ChatPath, ChatQuery};
use crate::state::SharedState;
use crate::voice_lab::{self, clone_check, facts, generate, provenance, rounds, save, LabFail};

fn fail(f: LabFail) -> Response {
    err_json(f.status, f.code, f.message)
}

fn answer<T: serde::Serialize>(r: Result<T, LabFail>) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err(f) => fail(f),
    }
}

/// `GET /voice-lab/api/model`: what the form offers for an alias.
pub async fn model(
    State(state): State<SharedState>,
    ChatQuery(q): ChatQuery<VoiceModelQuery>,
) -> Response {
    answer(facts::facts(&state, &q.alias, q.language.as_deref(), q.voice.as_deref()).await)
}

/// The blocking pool's answer, a panic there as a 500.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, LabFail> + Send + 'static,
) -> Result<T, LabFail> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| LabFail::internal(e.to_string()))?
}

/// `GET /voice-lab/api/sentences`.
pub async fn sentences(State(state): State<SharedState>) -> Response {
    let dir = voice_lab::lab_dir(&state);
    answer(blocking(move || Ok(voice_lab::sentences::table(&dir))).await)
}

/// `POST /voice-lab/api/sentences`: save (or, empty, drop) the owner's
/// sentence for a language; the table as it is now.
pub async fn set_sentence(
    State(state): State<SharedState>,
    ChatJson(body): ChatJson<VoiceSentenceSet>,
) -> Response {
    let dir = voice_lab::lab_dir(&state);
    let lock = voice_lab::library_write().await;
    answer(
        blocking(move || {
            let _lock = lock;
            voice_lab::sentences::save(&dir, &body.language, &body.text)
                .map(|_| voice_lab::sentences::table(&dir))
        })
        .await,
    )
}

/// `GET /voice-lab/api/rounds`: every kept round, newest first.
pub async fn list_rounds(State(state): State<SharedState>) -> Response {
    let dir = voice_lab::lab_dir(&state);
    answer(
        tokio::task::spawn_blocking(move || rounds::list(&dir))
            .await
            .map_err(|e| LabFail::internal(e.to_string())),
    )
}

/// `POST /voice-lab/api/rounds`: render a round.
pub async fn generate(
    State(state): State<SharedState>,
    ChatJson(args): ChatJson<VoiceGenerateArgs>,
) -> Response {
    answer(generate::generate(&state, &args).await)
}

/// `POST /voice-lab/api/rounds/{id}/delete`: discard a round — one still
/// rendering is stopped and waited for first; the rounds left.
pub async fn delete_round(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<String>,
) -> Response {
    let dir = voice_lab::lab_dir(&state);
    if let Err(f) = rounds::discard(&dir, &id).await {
        return fail(f);
    }
    answer(blocking(move || Ok(rounds::list(&dir))).await)
}

/// `POST /voice-lab/api/rounds/clear`: discard every round, a rendering
/// one stopped first, and every directory there that is no round.
pub async fn clear_rounds(State(state): State<SharedState>) -> Response {
    let dir = voice_lab::lab_dir(&state);
    if let Err(f) = rounds::discard_all(&dir).await {
        return fail(f);
    }
    answer(blocking(move || Ok(rounds::list(&dir))).await)
}

/// `GET /voice-lab/api/rounds/{id}/candidates/{n}`: a candidate's audio,
/// streamed, whole or the one `Range` the player asks ([`super::ranged`]).
pub async fn candidate(
    State(state): State<SharedState>,
    ChatPath((id, n)): ChatPath<(String, u32)>,
    ask: super::ranged::RangeAsk,
) -> Response {
    let dir = voice_lab::lab_dir(&state);
    let found = tokio::task::spawn_blocking(move || rounds::candidate_path(&dir, &id, n))
        .await
        .map_err(|e| LabFail::internal(e.to_string()))
        .and_then(|r| r);
    let (path, mime) = match found {
        Ok(f) => f,
        Err(f) => return fail(f),
    };
    match super::ranged::file(&ask, &path, super::ranged::Head::typed(&mime)).await {
        Ok(resp) => resp,
        Err(e) => fail(LabFail::not_found(format!(
            "reading {}: {e}",
            path.display()
        ))),
    }
}

/// `GET /voice-lab/api/samples`: the voice library with how each sample was
/// made.
pub async fn samples(State(state): State<SharedState>) -> Response {
    let library_active = state.snapshot().settings.audio.voice_dir.trim() == CONTAINER_VOICES_DIR;
    let Some(voices) = voices_dir(&state) else {
        return Json(VoiceSamples {
            samples: Vec::new(),
            library_active,
            error: Some(crate::web::audio_lab::no_audio_models_dir()),
            records_error: None,
        })
        .into_response();
    };
    let lab = voice_lab::lab_dir(&state);
    answer(
        tokio::task::spawn_blocking(move || {
            let (samples, records_error) =
                provenance::samples(&lab, &voices, clip_entries(&voices));
            VoiceSamples {
                samples,
                library_active,
                error: None,
                records_error,
            }
        })
        .await
        .map_err(|e| LabFail::internal(e.to_string())),
    )
}

/// `POST /voice-lab/api/samples`: save a candidate to the library.
pub async fn save_sample(
    State(state): State<SharedState>,
    ChatJson(args): ChatJson<VoiceSampleSaveArgs>,
) -> Response {
    answer(save::save(&state, &args).await)
}

/// `POST /voice-lab/api/clone-check`: a cloning model speaks a test line
/// with a sample; the speech route's answer as it came, and a refusal of
/// the check's own in that route's OpenAI envelope.
pub async fn clone_check(
    State(state): State<SharedState>,
    ChatJson(check): ChatJson<VoiceCloneCheck>,
) -> Response {
    match clone_check::run(&state, &check).await {
        Ok(resp) => resp,
        // In the speech route's envelope, as everything else it answers.
        Err(f) => (
            f.status,
            Json(lmgw_api_types::OpenAiError::new(
                f.message,
                "invalid_request_error",
                f.code,
            )),
        )
            .into_response(),
    }
}
