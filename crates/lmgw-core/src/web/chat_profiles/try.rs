//! Trying a profile without saving it (personality-profiles design §3.1's
//! last three rows, D17–D19): the editor's Preview, Test and Speak, each on
//! the unsaved draft it is sent. Nothing here writes a profile or a thread.
//!
//! **The draft in place of the thread's profile.** `thread_id` assembles as
//! that thread would — its prompt, its languages, its voice overrides, its
//! sampling and reasoning fields — with the draft where its own profile
//! (if any) would be; without it, an empty chat thread with Settings'
//! defaults (the default prompt, no voice overrides). The draft is
//! normalised as a create's content is, and refused as one is (an
//! example's empty side is `400 bad_request`); nothing is capped (D19).
//!
//! - **Preview** (`POST /chat/api/profiles/preview`) answers the system
//!   messages a send builds — the very function a send calls
//!   (`chat_profile::system_message`): the static part, a text turn's, and a
//!   voice turn's with the hint of the TTS the draft resolves to. `{{model}}`
//!   names `model` when given, else the thread's model, as the alias that
//!   would answer it now (a GPU hold's fallback); with neither it stays as
//!   written. With `model`, the three
//!   are counted through the universal counter (`proxy::count_texts`, D18),
//!   with its approximation flags; counting on a local model loads it.
//! - **Test** (`POST /chat/api/profiles/test`) is one model call with that
//!   system message and the user's text — no history, no `max_tokens`, no
//!   tools — through the Chat's egress (`proxy::stream_once_on`). A device's
//!   call is its key's (`policy_checked_call`), the owner's is
//!   `internal:chat`; it writes its request row and stores nothing else.
//! - **Speak** (`POST /chat/api/profiles/speak`) says `text` with the voice
//!   the draft resolves to — the read-aloud's pipeline, the plan's TTS,
//!   voice, style and seed (`chat_voice::speak_planned`) — and answers one
//!   WAV of the clauses joined. A thread's own seed is used; none is drawn
//!   into it. The refusals before any audio are the read-aloud's speech
//!   plan's (`tts_not_configured`, `voice_not_found`, …) as the Chat's flat
//!   JSON. It writes one TTS row.
//!
//! For a device, the TTS the draft resolves to passes its key's alias
//! scope before Preview, a voice Test or Speak looks at it (`403
//! key_scope`).

use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::chat_profiles::{
    PreviewAnswer, PreviewRequest, PreviewTokens, ProfileDraft, SpeakRequest, TestAnswer,
    TestRequest, TestUsage,
};

use super::super::agentchat::refused;
use super::super::chat::err_json;
use super::super::chat_caller::Caller;
use super::super::chat_extract::ChatJson;
use super::super::chat_profile::assemble::Profile;
use super::super::chat_profile::{system_message, Prompt};
use super::super::chat_repo::ChatRepo;
use super::super::chat_turn::{self, VoiceTurn};
use super::super::{chat_sampling, chat_voice};
use crate::config::chat_profile::{normalise_draft, ChatProfile};
use crate::config::Snapshot;
use crate::error::GatewayError;
use crate::ingress::ClientProto;
use crate::ir::{ChatRequest, Message, Params, ReasoningControl, Role, StreamDelta};
use crate::proxy::{self, reasoning_fit::Fitted, PerRoute};
use crate::realtime::expressive;
use crate::state::SharedState;
use crate::store::ChatThread;

/// The name a draft goes by where a profile's name is said (a voice note).
const DRAFT_NAME: &str = "draft";

/// What `{{model}}` is expanded to when nothing names a model: itself.
const MODEL_PLACEHOLDER: &str = "{{model}}";

fn refusal(e: &GatewayError) -> Response {
    err_json(e.http_status(), e.code(), e.to_string())
}

/// A draft, and the thread it is tried as (module doc).
struct Tried {
    thread: ChatThread,
    draft: ProfileDraft,
    /// The draft as a stored profile would be: what voice resolution reads.
    profile: ChatProfile,
}

impl Tried {
    /// The draft, normalised, on `thread_id` as `caller` reaches it, or on
    /// an empty chat thread on `model` with Settings' defaults.
    async fn of(
        state: &SharedState,
        caller: &Caller,
        mut draft: ProfileDraft,
        thread_id: Option<i64>,
        model: Option<&str>,
    ) -> Result<Self, Response> {
        normalise_draft(&mut draft).map_err(|r| {
            let status = StatusCode::from_u16(r.status()).unwrap_or(StatusCode::BAD_REQUEST);
            err_json(status, r.code(), format!("profile.{r}"))
        })?;
        let thread = match thread_id {
            Some(id) => match ChatRepo::of(id).thread_as(state, caller, id).await {
                Ok(Some(t)) => t,
                Ok(None) => {
                    return Err(err_json(
                        StatusCode::NOT_FOUND,
                        "not_found",
                        "thread not found",
                    ))
                }
                Err(e) => {
                    return Err(err_json(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal",
                        e.to_string(),
                    ))
                }
            },
            None => ChatThread {
                kind: "chat".into(),
                model_alias: model.unwrap_or_default().to_string(),
                system_prompt: state.snapshot().settings.default_chat_prompt().to_string(),
                ..ChatThread::default()
            },
        };
        let profile = ChatProfile {
            id: thread.profile_id.unwrap_or_default(),
            name: DRAFT_NAME.into(),
            builtin: None,
            persona: draft.persona.clone(),
            length_rule: draft.length_rule.clone(),
            examples: draft.examples.clone(),
            voice_block: draft.voice_block.clone(),
            reasoning: draft.reasoning,
            voice: draft.voice.clone(),
            follows_builtin: Vec::new(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        Ok(Self {
            thread,
            draft,
            profile,
        })
    }

    /// The turn's prompt with `answering` as `{{model}}`, today.
    fn prompt<'a>(&'a self, answering: &'a str) -> Prompt<'a> {
        Prompt {
            thread: &self.thread,
            profile: Some(Profile::from(&self.draft)),
            model: answering,
            today: chrono::Local::now().date_naive(),
        }
    }

    /// The TTS the draft resolves to is one `caller` may use (its key's
    /// alias scope; always for the owner): checked before anything looks
    /// at that alias — a voice turn's hint reads its speech profile, Speak
    /// lists its voices — so a device learns nothing of, and loads nothing
    /// for, an alias its key is fenced off (review fix 5). `403
    /// key_scope`.
    fn tts_in_scope(&self, caller: &Caller, snap: &Snapshot) -> Result<(), Response> {
        let cfg = chat_voice::resolve_with(snap, &self.thread, Some(&self.profile));
        match cfg.tts.alias {
            Some(alias) => caller.alias_in_scope(snap, &alias).map_err(|e| refusal(&e)),
            None => Ok(()),
        }
    }

    /// What makes a turn a voice turn here: the tag hint of the TTS the
    /// draft resolves to, when `realtime.tag_hint` is on — as a bound
    /// session's spoken turn has it.
    async fn voice_turn(&self, state: &SharedState, snap: &Snapshot) -> VoiceTurn {
        let cfg = chat_voice::resolve_with(snap, &self.thread, Some(&self.profile));
        let hint = match cfg.tts.alias {
            Some(alias) => {
                let facts = expressive::facts(state, &alias).await;
                expressive::hint(&facts, snap.settings.realtime.tag_hint)
            }
            None => None,
        };
        VoiceTurn { hint }
    }

    /// A text turn's system message and reasoning (`voice: false`, a plain
    /// send's), or a spoken voice turn's, with `answering` as `{{model}}`.
    async fn system(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        answering: &str,
        voice: bool,
    ) -> (String, Option<ReasoningControl>) {
        let prompt = self.prompt(answering);
        let settings = &snap.settings.realtime;
        if !voice {
            return system_message(&prompt, settings, None, None);
        }
        let turn = self.voice_turn(state, snap).await;
        let language = chat_voice::turn_language(snap, &self.thread, true);
        system_message(&prompt, settings, Some(&turn), language.as_ref())
    }
}

/// A `model` that names something.
fn named(model: Option<&str>) -> Option<&str> {
    model.map(str::trim).filter(|m| !m.is_empty())
}

/// `POST /chat/api/profiles/preview` (module doc).
pub async fn preview_profile(
    State(state): State<SharedState>,
    caller: Caller,
    ChatJson(req): ChatJson<PreviewRequest>,
) -> Response {
    let model = named(req.model.as_deref());
    let tried = match Tried::of(&state, &caller, req.profile, req.thread_id, model).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    let snap = state.snapshot();
    if let Err(r) = tried.tts_in_scope(&caller, &snap) {
        return r;
    }
    // `{{model}}` with no model to name — no `model`, no thread — stays as
    // written: the preview does not invent one.
    let answering = match model.unwrap_or(&tried.thread.model_alias) {
        "" => MODEL_PLACEHOLDER.to_string(),
        m => chat_turn::effective_alias(&state, m),
    };
    let static_part = tried.prompt(&answering).static_part();
    let (text_turn, _) = tried.system(&state, &snap, &answering, false).await;
    let (voice_turn, _) = tried.system(&state, &snap, &answering, true).await;
    let tokens = match model {
        None => None,
        Some(alias) => {
            let texts = [
                static_part.as_str(),
                text_turn.as_str(),
                voice_turn.as_str(),
            ];
            match count(&state, &caller, &snap, alias, &texts).await {
                Ok(t) => Some(t),
                Err(e) => return refusal(&e),
            }
        }
    };
    Json(PreviewAnswer {
        static_part,
        text_turn,
        voice_turn,
        tokens,
    })
    .into_response()
}

/// The three texts' counts on `alias`, as the caller may count (D18).
async fn count(
    state: &SharedState,
    caller: &Caller,
    snap: &Snapshot,
    alias: &str,
    texts: &[&str; 3],
) -> Result<PreviewTokens, GatewayError> {
    let (counts, headers) =
        proxy::count_texts(state, ClientProto::Chat, &caller.ctx(), alias, texts)
            .await
            .map_err(|(_, e)| e)?;
    let mut approx: Vec<proxy::Approx> = counts.iter().flat_map(|c| c.approx.clone()).collect();
    approx.sort();
    approx.dedup();
    let n = |i: usize| counts.get(i).map_or(0, |c| c.tokens);
    Ok(PreviewTokens {
        alias: alias.to_string(),
        answered_by: chat_turn::answered_by(snap, &headers),
        static_part: n(0),
        text_turn: n(1),
        voice_turn: n(2),
        approx: approx.iter().map(|a| a.as_str().to_string()).collect(),
    })
}

/// `POST /chat/api/profiles/test` (module doc).
pub async fn test_profile(
    State(state): State<SharedState>,
    caller: Caller,
    ChatJson(req): ChatJson<TestRequest>,
) -> Response {
    let Some(model) = named(Some(&req.model)) else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "model: name the model to test the profile with",
        );
    };
    if req.text.trim().is_empty() {
        return err_json(
            StatusCode::BAD_REQUEST,
            "empty_message",
            "text: the message to test with is empty",
        );
    }
    let tried = match Tried::of(&state, &caller, req.profile, req.thread_id, Some(model)).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    let snap = state.snapshot();
    if req.voice {
        if let Err(r) = tried.tts_in_scope(&caller, &snap) {
            return r;
        }
    }
    let answering = chat_turn::effective_alias(&state, model);
    let (system, reasoning) = tried.system(&state, &snap, &answering, req.voice).await;
    let mut messages = Vec::with_capacity(2);
    if !system.is_empty() {
        messages.push(Message::text(Role::System, system.clone()));
    }
    messages.push(Message::text(Role::User, req.text));
    let ir = ChatRequest {
        model_alias: model.to_string(),
        messages,
        params: Params {
            // One call with no cap of its own (design §3.1): the model's
            // remaining context is the bound.
            max_tokens: None,
            reasoning,
            ..chat_sampling::params_of(&tried.thread)
        },
        tools: Vec::new(),
        tool_choice: None,
        stream: true,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    };
    match call(&state, &caller, &ir).await {
        Ok(answer) => Json(TestAnswer { system, ..answer }).into_response(),
        Err(e) => refusal(&e),
    }
}

/// Who answered the test's call on the route it went out on, and how its
/// reasoning off went out there.
#[derive(Default)]
struct Answering {
    answered_by: Option<String>,
    fitted: Option<Fitted>,
}

/// The test call's say in what each route it lands on is sent: the
/// sampling and reasoning that route takes, as a Chat turn's
/// (`chat_turn::fit_route`).
struct TestRoute<'a> {
    state: &'a SharedState,
    admitted: crate::gate::GateHeaders,
    answering: std::sync::Mutex<Answering>,
}

#[async_trait::async_trait]
impl PerRoute for TestRoute<'_> {
    async fn request(
        &self,
        route: &crate::config::Route,
        hold: Option<&crate::vram::LocalHold>,
        rerouted: Option<&crate::gate::GateHeaders>,
        ir: &ChatRequest,
    ) -> Result<Option<ChatRequest>, GatewayError> {
        let headers = rerouted.unwrap_or(&self.admitted);
        let answered_by = chat_turn::answered_by(&self.state.snapshot(), headers);
        let answering = chat_turn::answering(answered_by.as_deref(), headers, &ir.model_alias);
        let fit =
            chat_turn::fit_route(self.state, (route, hold), answering, ir, (false, false)).await?;
        self.lock().answered_by = answered_by;
        if !fit.params_changed {
            return Ok(None);
        }
        let mut req = ir.clone();
        req.params = fit.params;
        Ok(Some(req))
    }

    fn fitted(&self, fitted: &Fitted) {
        self.lock().fitted = Some(fitted.clone());
    }
}

impl TestRoute<'_> {
    fn lock(&self) -> std::sync::MutexGuard<'_, Answering> {
        self.answering.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What the call streamed, and when.
struct Heard {
    started: Instant,
    reply: String,
    reasoning: String,
    first_reasoning: Option<Instant>,
    first_token: Option<Instant>,
}

impl crate::agent::DeltaSink for Heard {
    fn on_delta(&mut self, d: &StreamDelta) {
        match d {
            StreamDelta::TextDelta(t) if !t.is_empty() => {
                self.first_token.get_or_insert_with(Instant::now);
                self.reply.push_str(t);
            }
            StreamDelta::ReasoningDelta(r) if !r.is_empty() => {
                self.first_reasoning.get_or_insert_with(Instant::now);
                self.reasoning.push_str(r);
            }
            _ => {}
        }
    }
}

/// The test's one model call (module doc): a device's key checked first,
/// the alias resolved and admitted as a Chat turn's is, then streamed.
/// `first_token_ms` is from the request's start to the reply's first text,
/// a cold start included; `reasoning_ms` from the first reasoning to the
/// first text (or the end), absent when it did not reason.
async fn call(
    state: &SharedState,
    caller: &Caller,
    ir: &ChatRequest,
) -> Result<TestAnswer, GatewayError> {
    let alias = ir.model_alias.as_str();
    // A device's call holds one of its key's concurrent-request slots, as
    // its Chat turns do (review W3-8), and passes its key's check.
    let _slot = caller.turn_slot(state, ClientProto::Chat, alias).await?;
    caller
        .check(
            state,
            ClientProto::Chat,
            alias,
            crate::telemetry::RequestClass::Chat,
        )
        .await?;
    let started = Instant::now();
    let routed = crate::gate::resolve(state, alias, crate::gate::RouteCheck::None)
        .await
        .map_err(|f| f.error)?;
    let uses = crate::gate::request_facets(ir, None);
    let admitted = match routed.using(uses) {
        Ok(routed) => routed.admit(state).await,
        Err(f) => Err(f),
    };
    let opened = match admitted {
        Ok(o) => o,
        Err(f) => {
            // The refusal is its request row, as a Chat turn's is.
            if let Some(route) = f.route.as_deref() {
                let fallback = f.headers.fallback_reason();
                let who = (caller.key(), alias);
                refused::record(state, who, "chat", route, fallback, started, &f.error).await;
            }
            return Err(f.error);
        }
    };
    let per_route = TestRoute {
        state,
        admitted: opened.headers.clone(),
        answering: Default::default(),
    };
    let mut heard = Heard {
        started,
        reply: String::new(),
        reasoning: String::new(),
        first_reasoning: None,
        first_token: None,
    };
    let done = proxy::stream_once_on(
        state,
        opened.hold.as_ref(),
        &opened.route,
        opened.headers.fallback_reason(),
        ir,
        ClientProto::Chat.as_str(),
        caller.key(),
        // No deadline of its own: the route's request timeout is the bound.
        Duration::MAX,
        &mut heard,
        Some((&per_route, None)),
    )
    .await?;
    drop(opened);
    let end = Instant::now();
    let answering = std::mem::take(&mut *per_route.lock());
    let ms = |from: Instant, to: Instant| to.saturating_duration_since(from).as_millis() as u64;
    let reasoning_note = answering
        .fitted
        .as_ref()
        .and_then(|f| f.note(answering.answered_by.as_deref().unwrap_or(alias)));
    Ok(TestAnswer {
        system: String::new(),
        reply: heard.reply,
        reasoning: heard.reasoning,
        reasoning_note,
        usage: TestUsage {
            prompt_tokens: done.usage.prompt_tokens,
            completion_tokens: done.usage.completion_tokens,
        },
        first_token_ms: heard.first_token.map(|t| ms(heard.started, t)),
        total_ms: ms(heard.started, end),
        reasoning_ms: heard
            .first_reasoning
            .map(|r| ms(r, heard.first_token.unwrap_or(end))),
        answered_by: answering.answered_by,
    })
}

/// `POST /chat/api/profiles/speak` (module doc).
pub async fn speak_profile(
    State(state): State<SharedState>,
    caller: Caller,
    ChatJson(req): ChatJson<SpeakRequest>,
) -> Response {
    let started = Instant::now();
    if req.text.trim().is_empty() {
        return err_json(
            StatusCode::BAD_REQUEST,
            "empty_message",
            "text: there is nothing to speak",
        );
    }
    let tried = match Tried::of(&state, &caller, req.profile, req.thread_id, None).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    let snap = state.snapshot();
    if let Err(r) = tried.tts_in_scope(&caller, &snap) {
        return r;
    }
    let cfg = chat_voice::resolve_with(&snap, &tried.thread, Some(&tried.profile));
    let plan = match chat_voice::plan_with(&state, &snap, &tried.thread, cfg, None).await {
        Ok(p) => p,
        Err(r) => return speech_refusal(StatusCode::BAD_REQUEST, &r.code, r.message),
    };
    // A device speaks only with a TTS its key may use: scope and budget
    // first, the speaker counting each call (as a read-aloud's).
    if let Err(e) = chat_voice::device_check(&state, caller.charged().as_ref(), &plan).await {
        return refusal(&e);
    }
    // Dropped with the request: a client gone stops the synthesis.
    let (_stop, stop) = proxy::stop_pair();
    let mut reader = chat_voice::speak_planned(&state, &caller, plan, (started, stop), req.text);
    let mut pcm: Vec<i16> = Vec::new();
    while let Some(frame) = reader.recv().await {
        let data: serde_json::Value = serde_json::from_str(&frame.data).unwrap_or_default();
        match frame.event {
            "speech" => {
                let raw = data["pcm"].as_str().unwrap_or_default();
                match crate::realtime::audio::pcm::decode_pcm16(raw) {
                    Ok(samples) => pcm.extend(samples),
                    Err(e) => {
                        return err_json(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "internal",
                            format!("a spoken clause's audio could not be read: {e}"),
                        )
                    }
                }
            }
            // The voice failed while it spoke: what it said so far is no
            // sample.
            "speech_error" => {
                let code = data["code"].as_str().unwrap_or("speech_failed");
                let message = data["message"].as_str().unwrap_or_default().to_string();
                return speech_refusal(StatusCode::BAD_GATEWAY, code, message);
            }
            "speech_done" => break,
            _ => {}
        }
    }
    let rate = crate::realtime::audio::resample::INPUT_RATE;
    match crate::realtime::audio::pcm::write_wav_pcm16_mono(&pcm, rate) {
        Ok(wav) => ([(header::CONTENT_TYPE, "audio/wav")], wav).into_response(),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("the spoken sample could not be written as a WAV: {e:?}"),
        ),
    }
}

/// A refusal of Speak in the Chat's flat JSON, with the speech's own code.
fn speech_refusal(status: StatusCode, code: &str, message: String) -> Response {
    (
        status,
        Json(serde_json::json!({ "code": code, "message": message })),
    )
        .into_response()
}
