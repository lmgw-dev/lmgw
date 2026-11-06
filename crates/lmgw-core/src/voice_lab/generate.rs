//! A round (`voice_generate`, `POST /voice-lab/api/rounds`): one
//! text-to-speech alias renders the same text as several candidates.
//!
//! The alias is opened **once** for the round — routing, the GPU hold's
//! swap, the speech route's checks and admission — through the in-process
//! speech path a realtime answer uses ([`crate::proxy::synthesize`]), and
//! every candidate is one request on that route. So a hold switched on half
//! way does not hand the second half of a round to another voice, and a
//! configured fallback answers the whole round when it answers at all (the
//! round names it: `answered_by`, `fallback_reason`). The round is one
//! `request_logs` row, like a realtime answer.
//!
//! Each candidate is sent a seed of its own — consecutive from the one the
//! owner gave, else drawn — which reaches only a local row whose engine
//! reads one (`seeds_sent`); a cloud model is sent none, and its candidates
//! differ by its own chance. The language goes to a local row that takes
//! one, in its spelling; the description as the request's `instructions`,
//! shaped for the route that answers as `POST /v1/audio/speech` shapes it.
//!
//! There is no cap on the count: the candidates render one after another,
//! each seed drawn as its candidate starts (nothing is laid out per
//! candidate up front), and a failure ends the round where it happens
//! (`stopped`), keeping what was made. A round with nothing made is not
//! kept.
//!
//! **A stop is a stop, not a drop.** The round renders in a task of its
//! own, which the asking call only waits for: when that call goes away —
//! the page's Stop aborts its request, the page is left, an MCP client
//! hangs up — its guard raises the round's [`StopSignal`], the candidate in
//! flight ends at its next await, and the round still writes its one
//! `request_logs` row (200 `canceled`, with the characters its answered
//! candidates were sent, as a realtime answer's) and its `round.json`,
//! `stopped` saying so. Discarding a round that renders stops it the same
//! way ([`rounds::discard`]).

use lmgw_api_types::voice_lab::{
    VoiceCandidate, VoiceGenerateArgs, VoiceRound, DEFAULT_CANDIDATES,
};

use super::{facts, lab_dir, rounds, sentences, wav, LabFail};
use crate::audio::language::SpeechLanguage;
use crate::ingress::ClientProto;
use crate::proxy::synthesize::{self, seeds_voice, sends_seed, ClauseSpeech, SessionSeed};
use crate::proxy::{canceled, RequestCtx, StopSignal};
use crate::state::SharedState;

/// What a round is asked, checked: the text, language, description, voice,
/// how many candidates and the first seed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub alias: String,
    pub text: String,
    pub language: Option<String>,
    pub description: Option<String>,
    pub voice: Option<String>,
    pub count: u32,
    /// The first candidate's seed; `None`: each candidate draws its own.
    pub first_seed: Option<u32>,
}

impl Asked {
    /// Candidate `k`'s seed (from 0): consecutive from the first one given,
    /// else drawn now.
    pub fn seed(&self, k: u32) -> u32 {
        match self.first_seed {
            Some(first) => first.wrapping_add(k),
            None => rand::random::<u32>(),
        }
    }
}

fn said(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The arguments checked (module doc); `lab` is the lab's directory, where
/// the owner's sentences are. Blocking: reads them.
pub fn check(lab: &std::path::Path, args: &VoiceGenerateArgs) -> Result<Asked, LabFail> {
    let alias = said(Some(&args.alias))
        .ok_or_else(|| LabFail::bad("name a text-to-speech model (alias)"))?;
    let language = said(args.language.as_deref())
        .map(|l| sentences::normalize(&l))
        .transpose()
        .map_err(LabFail::bad)?;
    let text = match (said(args.text.as_deref()), &language) {
        (Some(t), _) => t,
        (None, Some(l)) => sentences::sentence_for(lab, l).ok_or_else(|| {
            LabFail::bad(format!(
                "there is no sentence for {} ({l}) yet: write the text the candidates say",
                crate::audio::language::display_name(l)
            ))
        })?,
        (None, None) => {
            return Err(LabFail::bad(
                "write the text the candidates say, or name a language whose sentence they say",
            ))
        }
    };
    let count = args.count.unwrap_or(DEFAULT_CANDIDATES);
    if count == 0 {
        return Err(LabFail::bad("count must be at least 1"));
    }
    Ok(Asked {
        alias,
        text,
        language,
        description: said(args.description.as_deref()),
        voice: said(args.voice.as_deref()),
        count,
        first_seed: args.seed,
    })
}

/// The content type of a candidate's bytes: a WAV as one, anything else as
/// what its bytes are.
fn mime_of(bytes: &[u8]) -> String {
    if wav::Wav::parse(bytes).is_some() {
        return "audio/wav".into();
    }
    crate::extract::sniff::sniff_artifact(&bytes[..bytes.len().min(4096)])
        .map(|s| s.mime.to_string())
        .unwrap_or_else(|_| "application/octet-stream".into())
}

/// Raises a round's stop when the call waiting for it goes away.
struct StopOnDrop(std::sync::Arc<rounds::Active>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// Render a round (module doc): in a task of its own, stopped when this
/// call is dropped.
pub async fn generate(
    state: &SharedState,
    args: &VoiceGenerateArgs,
) -> Result<VoiceRound, LabFail> {
    let lab = lab_dir(state);
    let asked = {
        let (lab, args) = (lab.clone(), args.clone());
        tokio::task::spawn_blocking(move || check(&lab, &args))
            .await
            .map_err(|e| LabFail::internal(e.to_string()))??
    };
    let id = rounds::new_id();
    let (rendering, stop) = rounds::start(&lab, &id)?;
    let _guard = StopOnDrop(rendering.stopper());
    let task = tokio::spawn(render(state.clone(), lab, id, asked, stop, rendering));
    task.await.map_err(|e| LabFail::internal(e.to_string()))?
}

/// The file work of a round, on the blocking pool.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, LabFail> + Send + 'static,
) -> Result<T, LabFail> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| LabFail::internal(e.to_string()))?
}

/// The round itself; `rendering` marks it active until this returns.
async fn render(
    state: SharedState,
    lab: std::path::PathBuf,
    id: String,
    asked: Asked,
    stop: StopSignal,
    rendering: rounds::Rendering,
) -> Result<VoiceRound, LabFail> {
    let state = &state;
    let mut syn = synthesize::open(
        state,
        &RequestCtx::default(),
        ClientProto::OpenaiChat,
        "voice lab",
        &asked.alias,
        asked.description.as_deref(),
        asked.voice.as_deref(),
        Some(&stop),
    )
    .await
    .map_err(|e| LabFail::gateway(&e))?;

    let answered_by = syn.fallback().unwrap_or(&asked.alias).to_string();
    let speech = syn.speech().cloned();
    let seeds_sent = speech.as_ref().is_some_and(|(row, s)| {
        sends_seed(
            s.profile.reads_seed,
            seeds_voice(&s.profile),
            row.default_request_options.contains_key("seed"),
            true,
        )
    });
    let language_note = asked.language.as_deref().and_then(|code| match &speech {
        Some((_, s)) => crate::audio::language::tts_fit(code, &s.profile, asked.voice.as_deref())
            .note
            .map(|n| format!("{answered_by} {n}")),
        None => Some(format!(
            "{answered_by} is a cloud model: lmgw sends it no language, it reads the text as \
             written"
        )),
    });
    let licence = match &speech {
        Some((row, _)) => facts::row_licence(state, row).await,
        None => None,
    };
    let mut round = VoiceRound {
        id,
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        alias: asked.alias.clone(),
        answered_by,
        fallback_reason: syn.fallback_reason().map(|r| r.as_str().to_string()),
        upstream: syn.answering_route().upstream.name.clone(),
        upstream_model: syn.answering_route().upstream_model.clone(),
        row: speech
            .as_ref()
            .map(|(row, s)| facts::row_ref(row, &s.profile)),
        licence,
        text: asked.text.clone(),
        sent_text: None,
        language: asked.language.clone(),
        language_note,
        description: asked.description.clone(),
        voice: asked.voice.clone(),
        seeds_sent,
        shaping: None,
        asked: asked.count,
        candidates: Vec::new(),
        stopped: None,
    };
    let language = asked.language.clone().map(SpeechLanguage::Request);
    {
        let (lab, id) = (lab.clone(), round.id.clone());
        if let Err(f) = blocking(move || rounds::create(&lab, &id)).await {
            // Nothing was sent: the row says why the round never began.
            let e = crate::error::GatewayError::Refused {
                status: 500,
                code: "internal",
                message: f.message.clone(),
            };
            syn.finish(Some(&e), 0).await;
            return Err(f);
        }
    }
    let mut audio_ms = 0u64;
    let mut failed = None;
    for k in 0..asked.count {
        if stop.is_raised() {
            failed = Some(canceled("stopped by the caller"));
            break;
        }
        let seed = asked.seed(k);
        let clause = ClauseSpeech {
            input: &asked.text,
            voice: asked.voice.as_deref(),
            language: language.as_ref(),
            instructions: asked.description.as_deref(),
            seed: Some(SessionSeed {
                value: seed,
                pinned: true,
            }),
            ..ClauseSpeech::default()
        };
        let spoken = match syn.speak(&clause, Some(&stop)).await {
            Ok(s) => s,
            Err(e) => {
                failed = Some(e);
                break;
            }
        };
        let first = round.candidates.is_empty();
        if first {
            round.shaping = spoken
                .shaped
                .header_value()
                .and_then(|v| v.to_str().ok().map(str::to_string));
            round.sent_text = Some(spoken.input).filter(|sent| *sent != asked.text);
        }
        let audio = spoken.audio;
        let index = k + 1;
        let duration_ms = wav::duration_ms(&audio);
        audio_ms += duration_ms.unwrap_or(0);
        let c = VoiceCandidate {
            index,
            seed: seeds_sent.then_some(seed),
            duration_ms,
            bytes: audio.len() as u64,
            mime: mime_of(&audio),
            url: rounds::candidate_url(&round.id, index),
        };
        // Its audio and record; the round's own file once, with the first.
        let kept = {
            let (lab, id, c2) = (lab.clone(), round.id.clone(), c.clone());
            let head = first.then(|| round.clone());
            blocking(move || {
                rounds::write_candidate(&lab, &id, &c2, &audio)?;
                match head {
                    Some(h) => rounds::write_round(&lab, &h),
                    None => Ok(()),
                }
            })
            .await
        };
        match kept {
            Ok(()) => round.candidates.push(c),
            Err(e) => {
                // The model answered; the disk did not take it. The round
                // ends here, with what it kept.
                round.stopped = Some(format!("candidate {index} could not be kept: {e}"));
                break;
            }
        }
    }
    syn.finish(failed.as_ref(), audio_ms).await;
    let made = round.candidates.len();
    if let Some(e) = &failed {
        if made == 0 {
            let (lab, id) = (lab.clone(), round.id.clone());
            let _ = blocking(move || rounds::delete(&lab, &id)).await;
            return Err(LabFail::gateway(e));
        }
        round.stopped = Some(if crate::proxy::is_canceled(e) {
            "stopped (Stop, the page left, or the round discarded)".to_string()
        } else {
            format!("candidate {} failed, the round ends there: {e}", made + 1)
        });
    }
    if made == 0 {
        let why = round.stopped.take().unwrap_or_default();
        let (lab, id) = (lab.clone(), round.id.clone());
        let _ = blocking(move || rounds::delete(&lab, &id)).await;
        return Err(LabFail::internal(why));
    }
    {
        let (lab, head) = (lab.clone(), round.clone());
        blocking(move || rounds::write_round(&lab, &head)).await?;
    }
    drop(rendering);
    Ok(round)
}
