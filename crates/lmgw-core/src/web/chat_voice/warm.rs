//! `POST /chat/api/threads/{id}/voice/warm` (chat-voice design §4.1–§4.3):
//! a press warms the thread's speech models before the request it announces.
//!
//! The body names the stages, `{"stages": ["asr"]}` — the mic's press; `tts`
//! and `chat` (the thread's own model) may join it. They are the thread's
//! resolved aliases (`chat_voice::resolve`), warmed as one **Admit** group
//! (`realtime::warm`): through the request admission, so the press may evict
//! idle models exactly as its request would, every claim kept until the
//! group is up, a group that does not fit together warmed without evicting.
//!
//! The answer is SSE: one `state` frame per change of each stage
//! (`loading`, then `ready` with its time, or `held`, `fallback`,
//! `skipped`, `failed`), then `done` once every stage has settled. Closing
//! the stream (the press ended: key released, Esc, thread left) drops what
//! is still waiting for admission; a container start already in flight
//! finishes.
//!
//! Refused before anything is warmed: `404` for a thread that is not there,
//! `400 bad_request` for an empty or unknown stage list, `422
//! asr_not_configured` / `tts_not_configured` for a stage the thread has no
//! alias for at any level (the message names Settings → Chat → Voice).

use std::convert::Infallible;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Deserialize;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::audio::language::SpeechLanguage;
use crate::config::Snapshot;
use crate::proxy::synthesize::SessionSeed;
use crate::realtime::warm::{warm_group, Reporter, Speaks, Warm, WarmMode};
use crate::state::SharedState;
use crate::store::ChatThread;

use super::super::chat::err_json;
use super::super::chat_extract::{ChatJson, ChatPath};
use super::super::chat_repo::ChatRepo;
use super::resolve::{resolve, VoiceConfig};
use super::speech::{style_of, thread_seed, voice_of};

/// The body: which stages to warm.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WarmBody {
    stages: Vec<String>,
}

/// `POST /chat/api/threads/{id}/voice/warm` (module doc).
pub(crate) async fn warm(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatJson(body): ChatJson<WarmBody>,
) -> Response {
    let Some(thread) = ChatRepo::of(id).thread(&state, id).await.ok().flatten() else {
        return err_json(StatusCode::NOT_FOUND, "not_found", "thread not found");
    };
    let snap = state.snapshot();
    let models = match stages(&state, &snap, &thread, &body.stages).await {
        Ok(m) => m,
        Err(refused) => return refused,
    };
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let report = Reporter::to(tx);
    let label = format!("chat thread {id}");
    // Its own task: the stream going away must not cancel a start in flight
    // (module doc) — the warm sees it through the reporter instead.
    tokio::spawn(async move {
        warm_group(&state, &label, WarmMode::Admit, &models, &report).await;
    });
    let frames = UnboundedReceiverStream::new(rx).map(|s| {
        let data = serde_json::to_string(&s).unwrap_or_default();
        Ok::<_, Infallible>(Event::default().event("state").data(data))
    });
    let done = futures::stream::once(async {
        Ok::<_, Infallible>(Event::default().event("done").data("{}"))
    });
    Sse::new(frames.chain(done))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// The stages named, in their order, each once — or the refusal.
async fn stages(
    state: &SharedState,
    snap: &Snapshot,
    thread: &ChatThread,
    names: &[String],
) -> Result<Vec<Warm>, Response> {
    let bad = |m: String| err_json(StatusCode::BAD_REQUEST, "bad_request", m);
    if names.is_empty() {
        return Err(bad(
            "stages is empty: name one or more of asr, tts, chat".into()
        ));
    }
    let cfg = resolve(snap, thread);
    let unset = |code: &'static str, stage: &str| {
        let message = cfg
            .problems
            .iter()
            .find(|p| p.stage == stage)
            .map(|p| p.message.clone())
            .unwrap_or_else(|| format!("no {stage} model is set (Settings → Chat → Voice)"));
        err_json(StatusCode::UNPROCESSABLE_ENTITY, code, message)
    };
    let mut models = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for name in names {
        let name = name.trim();
        if seen.contains(&name) {
            continue;
        }
        seen.push(name);
        models.push(match name {
            "asr" => match &cfg.asr.alias {
                Some(alias) => Warm::Model {
                    stage: "asr",
                    alias: alias.clone(),
                },
                None => return Err(unset("asr_not_configured", "asr")),
            },
            "tts" => match &cfg.tts.alias {
                Some(alias) => {
                    let label = format!("chat thread {}", thread.id);
                    voice(state, snap, (thread, &cfg), alias, &label).await
                }
                None => return Err(unset("tts_not_configured", "tts")),
            },
            "chat" => Warm::Model {
                stage: "chat",
                alias: thread.model_alias.clone(),
            },
            other => {
                return Err(bad(format!(
                    "unknown stage '{other}': name one or more of asr, tts, chat"
                )))
            }
        });
    }
    Ok(models)
}

/// Every stage the thread has a model for — the ASR, its own chat model
/// and the TTS, as the press would name them — for a bound realtime
/// session's connect warm (§4.1: entering realtime mode loads all three,
/// as one Admit group). A stage with no alias at any level is left out:
/// the session says so where it is used.
pub(crate) async fn thread_stages(
    state: &SharedState,
    snap: &Snapshot,
    thread: &ChatThread,
) -> Vec<Warm> {
    let cfg = resolve(snap, thread);
    let mut models = Vec::new();
    if let Some(alias) = &cfg.asr.alias {
        models.push(Warm::Model {
            stage: "asr",
            alias: alias.clone(),
        });
    }
    models.push(Warm::Model {
        stage: "chat",
        alias: thread.model_alias.clone(),
    });
    if let Some(alias) = &cfg.tts.alias {
        let label = format!("chat thread {}", thread.id);
        models.push(voice(state, snap, (thread, &cfg), alias, &label).await);
    }
    models
}

/// The TTS stage as the thread speaks (`speech::plan`): its speech style as
/// the instructions, and the voice realtime's chain resolves for it (realtime
/// §5.3, as if a session had named the thread's voice — or none), with the
/// thread's reply language (a request, `audio::language::tts_fit`) and seed —
/// drawn and stored now on its first use, as a read-aloud draws it, so a row
/// whose voice comes from its seed warms the voice the read-aloud speaks
/// with (review m7). A voice the chain finds nothing for loads nothing: the
/// container is still started.
async fn voice(
    state: &SharedState,
    snap: &Snapshot,
    (thread, cfg): (&ChatThread, &VoiceConfig),
    alias: &str,
    label: &str,
) -> Warm {
    let v = voice_of(state, snap, cfg, alias, label).await;
    let speaks = match v.outcome {
        Ok(crate::realtime::voice::VoiceOutcome::Resolved(voice)) => {
            let repo = ChatRepo::of(thread.id);
            let seed = thread_seed(state, repo, thread.id, cfg, label).await;
            Some(Speaks {
                voice: voice.send,
                language: cfg
                    .reply_language
                    .value
                    .clone()
                    .map(SpeechLanguage::Request),
                seed: Some(SessionSeed {
                    value: seed,
                    pinned: false,
                }),
            })
        }
        _ => None,
    };
    Warm::Voice {
        alias: alias.to_string(),
        instructions: style_of(snap, cfg, &v.expressive).send,
        speaks,
    }
}
