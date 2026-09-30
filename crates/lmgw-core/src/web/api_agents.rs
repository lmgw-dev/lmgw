//! The agent catalog's HTTP face (agent-catalog design §5): the reads, the
//! `agent_*` ops, and import/export.
//!
//! Route split follows the rest of the `/api` plane: every read is its own
//! `GET`, every mutation is a `POST /api/op/{name}` the dispatcher in
//! [`super::api`] forwards here by prefix — the same arrangement the mail
//! workflow's ops have. The exception is import, which takes a **file body**
//! rather than an argument object and so needs a route of its own.
//!
//! Three rules run through everything below:
//!
//! - **A secret never leaves.** `masked_values` on every read, `without_secrets`
//!   on every export and duplicate, and an empty submission keeps the stored
//!   value (§2.6).
//! - **A missing tool is a warning, not a refusal** (§5.3). An MCP server is
//!   registered before its image is pulled; an agent is imported before its
//!   server is wired, for the same reason. The card shows the gap.
//! - **An unreadable row is reported, never fatal.** A manifest written by a
//!   newer build makes one card say so; it does not take the catalog down
//!   (§4.1).
//!
//! `agent_run` and `agent_run_cancel` are here too, though the work itself
//! lives in [`crate::agents::batch`]: starting a run is one `jobs` row, so the
//! op is thin and the executor is where the lifecycle is. `agent_open_chat` is
//! the same arrangement for [`crate::agents::chat`].

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use lmgw_api_types as dto;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{json, Map, Value};

use crate::agents::batch::{self, BatchShape};
use crate::agents::ledger::{self, Refusal};
use crate::agents::token::{self, AgentIdentity};
use crate::agents::{self, manifest, mounts, seed, Agent, ToolSurface};
// The `/api` plane's coded refusal (principals §3.9). Both it and the ledger's
// `Refusal` above are "a refusal that names itself"; the ledger's has the short
// name here because this file is mostly the ledger's, and this one is what the
// catalog's ops answer with.
use crate::principal::Refusal as ApiRefusal;
use crate::proxy::RequestCtx;
use crate::state::SharedState;
use crate::store;

/// Keys an export adds around the manifest. Stripped on the way back in, so a
/// downloaded file re-imports unchanged — the manifest itself refuses unknown
/// fields, and an export that could not be imported would be a strange export.
const ENVELOPE_KEYS: [&str; 6] = [
    "exported_at",
    "lmgw_version",
    "config_values",
    "config_omitted",
    // The mount slots the export stripped (mounts §5.2). On the list for the
    // same reason as everything else here: a key the importer did not strip
    // would reach the `deny_unknown_fields` manifest parse, and an export that
    // could not be re-imported would be a strange export.
    "config_unbound",
    // container-runtime §3.4: whether this file lands on another box, and what
    // the receiver would have to do if not. An export that named a
    // `localhost/…` image without saying so would look complete and not be.
    "portability",
];

pub fn routes(state: &SharedState) -> Router<SharedState> {
    use axum::handler::Handler;

    use crate::principal::Cap;
    use crate::server::require;

    // Three capabilities meet on this plane (principals §3.2), so the layers
    // go on individually rather than over the router: a container reads its
    // own row and its own runs (`AgentSelf`), writes its own run's ledger
    // (`Ledger`), and touches nothing else (`Admin`).
    Router::new()
        .route("/api/agents", get(list.layer(require(state, Cap::Admin))))
        // A manifest is as big as its prompts are; axum's silent 2 MiB
        // `DefaultBodyLimit` is dropped here the same way `/api/docs/import`
        // drops it, so nothing rejects a paste with an unnamed 413.
        .route(
            "/api/agents/import",
            post(import)
                .layer(axum::extract::DefaultBodyLimit::disable())
                .route_layer(require(state, Cap::Admin)),
        )
        // Before `/api/agents/{id}` can claim it: `runs` is a reserved id
        // (`manifest::validate_id`), so nothing is shadowed.
        .route(
            "/api/agents/runs/{job_id}",
            get(run_detail).route_layer(require(state, Cap::AgentSelf)),
        )
        // The run ledger (container-runtime §3.2). The layer admits *an*
        // agent; `owned_run` below checks *which* — the ownership half was
        // always the handler's and stays there.
        //
        // `DefaultBodyLimit::disable()` for the same reason the import route
        // above drops it: a batch is as big as its rows are, and axum's silent
        // 2 MiB would answer a perfectly ordinary NDJSON flush with an
        // unexplained plain-text 413 — a cap nobody chose and nothing prints.
        .route(
            "/api/agents/runs/{job_id}/events",
            post(run_events)
                .layer(axum::extract::DefaultBodyLimit::disable())
                .route_layer(require(state, Cap::Ledger)),
        )
        .route(
            "/api/agents/runs/{job_id}/close",
            post(run_close)
                .layer(axum::extract::DefaultBodyLimit::disable())
                .route_layer(require(state, Cap::Ledger)),
        )
        .route(
            "/api/agents/{id}",
            get(detail.layer(require(state, Cap::AgentSelf))),
        )
        .route(
            "/api/agents/{id}/export",
            get(export.layer(require(state, Cap::Admin))),
        )
        // One path, two capabilities — reading your own runs is not opening
        // one — so `Handler::layer` per method, never `route_layer` on the
        // path (§3.5).
        .route(
            "/api/agents/{id}/runs",
            get(runs.layer(require(state, Cap::AgentSelf)))
                .post(run_open.layer(require(state, Cap::Ledger)))
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
}

// ---------------------------------------------------------------------------
// The run ledger (container-runtime §3.2)
// ---------------------------------------------------------------------------

fn refused(r: &Refusal) -> Response {
    (
        StatusCode::from_u16(r.status()).unwrap_or(StatusCode::BAD_REQUEST),
        Json(dto::ApiError {
            code: r.code().into(),
            message: r.message(),
        }),
    )
        .into_response()
}

fn bad_request(code: &str, message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(dto::ApiError {
            code: code.into(),
            message,
        }),
    )
        .into_response()
}

/// The agent this request **is** (principals §3.5).
///
/// It re-verifies nothing: the root resolved the credential once and the
/// `Ledger` layer already refused everything that is not a principal at all —
/// including a disabled agent, by name. What is left is the one question the
/// layer cannot answer, "which agent", and the only non-agent that can reach
/// here is an owner, who holds every capability and owns no run.
fn principal_agent(ctx: &RequestCtx) -> Result<AgentIdentity, Refusal> {
    ctx.agent.clone().ok_or(Refusal::NotOwned)
}

/// The half of `AgentSelf` the layer cannot do (§3.2): it admits *an* agent,
/// and its own id is the only one it may read. `None` for an owner, who reads
/// every row, and for a request the layer has already vouched for otherwise.
fn foreign_agent(ctx: &RequestCtx, id: &str) -> Option<Response> {
    let mine = ctx.principal.agent_id()?;
    if mine == id {
        return None;
    }
    Some(
        (
            StatusCode::FORBIDDEN,
            Json(dto::ApiError {
                code: "agent_not_owned".into(),
                message: format!(
                    "agent '{mine}' may read its own row and its own runs, not '{id}'"
                ),
            }),
        )
            .into_response(),
    )
}

/// [`foreign_agent`] for a run, whose owner is the `(kind, key)` the job row
/// carries. Ownership is answered **before existence**, as in [`owned_run`]:
/// probing must not be a way to enumerate another agent's job ids.
fn foreign_run(ctx: &RequestCtx, key: Option<&str>) -> Option<Response> {
    let mine = ctx.principal.agent_id()?;
    let owner = key.unwrap_or_default();
    if owner == mine || owner == agents::job_key(mine) {
        return None;
    }
    Some(refused(&Refusal::NotOwned))
}

/// The open ledger run `job_id` names, once the bearer has been shown to own it.
///
/// **Ownership is answered before existence.** A foreign token gets
/// `403 run_not_owned` for any id it does not own, whether that run is open,
/// finished or was never this agent's — probing the ledger must not be a way to
/// enumerate another agent's job ids.
///
/// Every other answer is determined from the durable job row, not from whether
/// the desk still holds an entry: the executor forgets its entry the moment a
/// run ends, so "closed" and "never existed" would otherwise be the same 404
/// depending on timing.
async fn owned_run(
    st: &SharedState,
    ctx: &RequestCtx,
    job_id: i64,
) -> Result<std::sync::Arc<ledger::Run>, Refusal> {
    let me = principal_agent(ctx)?;
    let live = st.agent_ledger.get(job_id);
    let row = match store::get_job(&st.db, job_id).await {
        Ok(r) => r.filter(|r| r.kind == agents::JOB_KIND),
        Err(e) => return Err(Refusal::Unavailable(e.to_string())),
    };

    // Who owns it, from whichever of the two knows.
    let owner = live
        .as_ref()
        .map(|r| r.agent_id.clone())
        .or_else(|| row.as_ref().and_then(|r| r.key.clone()));
    match owner {
        Some(o) if o != me.agent_id && o != agents::job_key(&me.agent_id) => {
            return Err(Refusal::NotOwned)
        }
        // Nothing knows this id at all: it is not anybody's, so nobody is
        // foreign to it.
        None => return Err(Refusal::NoRun(job_id)),
        _ => {}
    }

    if let Some(run) = live {
        // Cancel marks the run cancelled immediately; every later events or
        // close POST is refused, which is how a container lmgw cannot signal
        // finds out.
        if run.cancelled() {
            return Err(Refusal::Cancelled);
        }
        return Ok(run);
    }
    // Mine, but the desk has no entry: say which of the three it is.
    let status = row.map(|r| r.status).unwrap_or_default();
    Err(match status.as_str() {
        "canceled" => Refusal::Cancelled,
        "done" | "failed" => Refusal::AlreadyClosed,
        _ => Refusal::NotLedger(job_id),
    })
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub(crate) struct OpenBody {
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    rows: Vec<batch::Row>,
}

/// `POST /api/agents/{id}/runs` — open a run from outside lmgw.
///
/// Opens a job exactly as [`batch::start`] does, so "one live run per agent"
/// still comes free from the `(kind, key)` index.
pub async fn run_open(
    State(st): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let me = match principal_agent(&ctx) {
        Ok(m) => m,
        Err(r) => return refused(&r),
    };
    if me.agent_id != id {
        return refused(&Refusal::NotOwned);
    }
    let open: OpenBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) if body.is_empty() => OpenBody::default(),
        Err(e) => return bad_request("bad_request", format!("run body: {e}")),
    };
    let phase = match open.phase.as_deref() {
        None | Some("run") => batch::Phase::Run,
        Some("apply") => batch::Phase::Apply,
        Some(other) => {
            return bad_request(
                "bad_request",
                format!("phase '{other}': a ledger run is opened for 'run' or 'apply'"),
            )
        }
    };
    let agent = match load_agent(&st, &id).await {
        Ok(a) => a,
        Err(e) => return not_found_or_bad_request(&id, e),
    };
    // Every run kind but `chat` has runs; a chat agent materializes a thread
    // instead, and there is nothing for a ledger to report against.
    if agent.manifest.kind() == "chat" {
        return bad_request(
            "bad_request",
            format!("'{id}' is a chat agent; it has no runs. Use agent_open_chat."),
        );
    }
    // Scope is recomputed at every run start, because a model picker is exactly
    // the field an owner changes between runs (§3.1).
    if let Err(e) = token::recompute_scope(&st, &agent).await {
        return bad_request("op_failed", e);
    }
    let deadline_seconds = ledger::deadline_seconds(&agent.manifest);
    let declared = batch::review_columns(&agent.manifest);
    let spawned = match batch::start(
        &st,
        batch::Input {
            agent_id: id.clone(),
            phase,
            rows: open.rows,
            base_job: None,
            ledger: true,
            // An externally opened run has no form behind it: the stored
            // config is the only config it could mean.
            values: Map::new(),
            // And nothing lmgw starts: the run is driven from outside, so
            // there is no phase whose config this would be (mounts §5.7).
            effective: None,
        },
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return bad_request("op_failed", e),
    };
    let job_id = spawned.id();
    // "One live run per agent" (§2.4) is a refusal here, not a redirect. The
    // job the index handed back may be an in-process run, or a ledger run this
    // caller did not open and whose deadline started somewhere else — handing
    // its id over as though it were freshly opened would have the caller write
    // events into a run with different terms, or into one that takes none.
    if matches!(spawned, crate::jobs::Spawn::AlreadyRunning(_)) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "code": "already_running",
                "message": format!(
                    "'{id}' already has a run in flight (#{job_id}); one live run per agent"
                ),
                "run": job_id,
            })),
        )
            .into_response();
    }
    st.agent_ledger.attach(
        job_id,
        &id,
        phase,
        declared,
        deadline_seconds,
        token::plaintext_of(&st.snapshot(), &id),
    );
    Json(json!({
        "run": job_id,
        // Printed, never silent: the caller is told exactly how long it has.
        "deadline_seconds": deadline_seconds,
    }))
    .into_response()
}

/// `POST /api/agents/runs/{run}/events` — one event, an array, or NDJSON.
pub async fn run_events(
    State(st): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(job_id): Path<i64>,
    body: Bytes,
) -> Response {
    let run = match owned_run(&st, &ctx, job_id).await {
        Ok(r) => r,
        Err(r) => return refused(&r),
    };
    // Not `from_utf8_lossy`: a replacement character silently rewrites a row id
    // or a log line into something the sender never wrote, and the sender has
    // no way to notice.
    let Ok(text) = std::str::from_utf8(&body) else {
        return bad_request(
            "invalid_utf8",
            "the event body is not valid UTF-8; ledger events are JSON, which is".to_string()
                + " UTF-8 by definition",
        );
    };
    let events = ledger::decode_body(text);
    let mut applied = 0usize;
    let mut rejected: Vec<String> = Vec::new();
    for ev in &events {
        match run.apply(ev) {
            ledger::Applied::Yes => applied += 1,
            ledger::Applied::Rejected(line) => rejected.push(line),
        }
    }
    // Straight into the buffer the Run tab reads, rather than waiting for the
    // executor's next tick: a hand-posted row should be on screen by the time
    // the POST returns.
    st.agent_runs.put(job_id, &run.rows());
    Json(json!({ "ok": true, "applied": applied, "rejected": rejected })).into_response()
}

/// `POST /api/agents/runs/{run}/close` — the run's terminal status.
pub async fn run_close(
    State(st): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(job_id): Path<i64>,
    body: Bytes,
) -> Response {
    let run = match owned_run(&st, &ctx, job_id).await {
        Ok(r) => r,
        Err(r) => return refused(&r),
    };
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let raw = v.get("status").and_then(Value::as_str).unwrap_or("done");
    let Some(status) = ledger::CloseStatus::parse(raw) else {
        return bad_request(
            "bad_request",
            format!(
                "status '{raw}': a run closes {}",
                ledger::CloseStatus::names()
            ),
        );
    };
    let close = ledger::Close {
        status,
        detail: v.get("detail").and_then(Value::as_str).map(str::to_string),
        output: v.get("output").cloned(),
    };
    match run.close(close) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(r) => refused(&r),
    }
}

// ---------------------------------------------------------------------------
// Shared shaping
// ---------------------------------------------------------------------------

fn install_hint(t: &manifest::ToolRef) -> Option<dto::AgentInstallHint> {
    t.install.as_ref().map(|i| dto::AgentInstallHint {
        kind: match i.kind {
            manifest::InstallKind::Git => "git",
            manifest::InstallKind::Image => "image",
            manifest::InstallKind::Url => "url",
        }
        .to_string(),
        reference: i.reference.clone(),
        notes: i.notes.clone().unwrap_or_default(),
    })
}

fn requirements(m: &manifest::Manifest, surface: &ToolSurface) -> Vec<dto::AgentRequirement> {
    surface
        .check(&m.tools)
        .into_iter()
        .zip(m.tools.iter())
        .map(|(r, t)| dto::AgentRequirement {
            label: r.label,
            registered: r.registered,
            missing_tools: r.missing_tools,
            install: install_hint(t),
        })
        .collect()
}

fn requires_ok(reqs: &[dto::AgentRequirement]) -> bool {
    reqs.iter()
        .all(|r| r.registered && r.missing_tools.is_empty())
}

/// Both halves of the warnings channel for one agent (container-runtime §4.3):
/// the pure, manifest-derived list first, then what the box says right now.
///
/// Split at the source and joined here, so the manifest half can be tested
/// without a gateway and the runtime half is never baked into a stored row.
async fn warnings(
    state: &SharedState,
    agent: &Agent,
    podman: &Result<(), String>,
) -> Vec<dto::AgentWarning> {
    let mut out = agents::manifest_warnings(&agent.manifest, &agent.row.source);
    out.extend(agents::runtime_warnings(state, agent, podman).await);
    out.into_iter()
        .map(|w| dto::AgentWarning {
            code: w.code.to_string(),
            message: w.message,
            blocks_start: w.blocks_start,
        })
        .collect()
}

/// The one warning that is about the **token** rather than about the manifest
/// (principals §3.10).
///
/// While *Require API key* is off — the shipped default — a container that
/// simply omits its token is `Anonymous`, and `Anonymous` holds `Inference`. So
/// the model scope, the `/mcp` allow list and the budget this token carries
/// bind only a container that bothers to present it, which is a thing the owner
/// should be told rather than left to assume the other way round. Non-blocking:
/// the fix is one checkbox, not an edit to this agent.
///
/// Recomputed on every read and stored nowhere — it is a statement about a
/// setting, and ticking the checkbox has to make it go away without anything
/// being rewritten. A `container` agent only, as §3.10 scopes it: that is the
/// shape whose logic is a process on the other side of the wire, presenting the
/// token — a `chat` preset is a thread in this process and holds none.
fn token_scope_advisory(agent: &Agent, auth_enabled: bool) -> Option<dto::AgentWarning> {
    (!auth_enabled && agent.manifest.kind() == "container").then(|| dto::AgentWarning {
        code: "token_scope_advisory".to_string(),
        // Verbatim from §3.10, and the page renders it as it stands: the
        // sentence is the server's, so there is one place it is written.
        message: "Require API key is off, so this token's model scope, tool allow-list and \
                  budget bind only a container that presents it; switch it on under Settings → \
                  Network & access to make them binding"
            .to_string(),
        blocks_start: false,
    })
}

/// What a `container` agent runs under, for the Run tab's Runtime block (§7).
/// `None` for every other kind — there is nothing to print.
async fn runtime_dto(
    state: &SharedState,
    agent: &Agent,
    podman: &Result<(), String>,
) -> Option<dto::AgentRuntime> {
    // A `script` apply step is a container run too, under the very same
    // `run.limits` — 512 MiB, 2 CPUs, 256 pids, a 600 s deadline and a
    // read-only rootfs unless the manifest says otherwise. Principle 5 says a
    // bound the owner is under is a bound the owner can see, so the Runtime
    // block prints them for a scripted batch agent exactly as for a container
    // one; only the image and the pull policy come from Settings instead of
    // from the manifest.
    let (image, pull) = match &agent.manifest.run {
        manifest::RunSpec::Container { pull, .. } => (
            agent.manifest.image().unwrap_or_default().to_string(),
            pull.as_str().to_string(),
        ),
        _ if needs_podman(&agent.manifest) => (
            state.snapshot().settings.agent_script_image.clone(),
            manifest::PullPolicy::Missing.as_str().to_string(),
        ),
        _ => return None,
    };
    let l = agent.manifest.limits();
    Some(dto::AgentRuntime {
        image,
        pull,
        phases: agent.manifest.phases(),
        memory_mb: l.memory_mb,
        cpus: l.cpus,
        pids: l.pids,
        deadline_seconds: l.deadline_seconds,
        stop_grace_seconds: l.stop_grace_seconds,
        read_only: l.read_only,
        output_validated: agent.manifest.output_validated(),
        podman: podman.is_ok(),
        podman_note: podman.clone().err().unwrap_or_default(),
    })
}

/// `podman --version`, asked **once** per listing rather than once per agent.
///
/// Lazy, because the commonest catalog has no container agent at all and must
/// not shell anything to be drawn. `Ok(())` is "podman answered".
/// Does drawing this agent's warnings need podman asked about at all? A
/// container agent, or a batch one whose apply step is a `script` — which is a
/// container run of the script image (container-runtime §4.2).
fn needs_podman(m: &manifest::Manifest) -> bool {
    m.kind() == "container" || m.apply_step().is_some_and(manifest::Step::is_script)
}

async fn podman_once(
    state: &SharedState,
    cached: &mut Option<Result<(), String>>,
) -> Result<(), String> {
    if cached.is_none() {
        *cached = Some(agents::container::podman_available(state).await);
    }
    cached.clone().expect("just filled")
}

/// The alias the run would actually ask for: the manifest's `alias` rendered
/// against the stored config. A card that shows `{{config.model}}` tells the
/// owner nothing about what this agent is pointed at.
fn effective_model(agent: &Agent) -> String {
    let ctx = agents::template::Ctx {
        config: agent.effective_config(),
        ..Default::default()
    }
    .with_identity(&agent.manifest.id, &agent.manifest.name, None);
    agents::template::render_text(&agent.manifest.model.alias, &ctx)
}

fn fields_dto(agent: &Agent) -> Vec<dto::AgentField> {
    let values = agent.config_values();
    agent
        .manifest
        .fields()
        .unwrap_or_default()
        .into_iter()
        .map(|f| dto::AgentField {
            has_value: f.is_secret()
                && values
                    .get(&f.name)
                    .map(|v| !v.is_null() && v.as_str() != Some(""))
                    .unwrap_or(false),
            format: f.format.map(|x| x.as_str().to_string()).unwrap_or_default(),
            // Only a mount field has one, and the form renders it as the chip
            // beside the path rather than as a control (mounts §5.8).
            access: match f.is_mount() {
                true => f.access.as_str().to_string(),
                false => String::new(),
            },
            ty: f.ty.as_str().to_string(),
            title: f.title.unwrap_or_default(),
            description: f.description.unwrap_or_default(),
            default: f.default,
            enum_values: f.enum_values,
            minimum: f.minimum,
            maximum: f.maximum,
            required: f.required,
            name: f.name,
        })
        .collect()
}

/// The Run tab's batch surface, derived from the manifest *and* the stored
/// config (§6.2). `None` for a `chat` agent, which has no review table.
///
/// `surface` fills a container's "Apply may call …" ceiling with the list the
/// agent's token can actually reach — `ToolSurface::allowed`, the same function
/// `/mcp` filters on, because `allowed: None` on a `tools[]` entry means *the
/// whole label*. Without the surface the ceiling is named by label rather than
/// left blank: "every tool of label 'gws'" is true and useful; nothing is
/// neither.
fn batch_shape(agent: &Agent, surface: Option<&ToolSurface>) -> Option<dto::AgentBatchShape> {
    let config = agent.effective_config();
    BatchShape::of(&agent.manifest, &config).map(|mut s| {
        if s.apply_tools_are_ceiling {
            s.apply_tools = match surface {
                Some(surface) => surface.allowed(&agent.manifest.tools),
                None => agent
                    .manifest
                    .tools
                    .iter()
                    .flat_map(|t| match &t.allowed {
                        Some(names) => names.clone(),
                        None => vec![format!("every tool of label '{}'", t.label)],
                    })
                    .collect(),
            };
        }
        dto::AgentBatchShape {
            has_classify: s.has_classify,
            has_apply: s.has_apply,
            apply_tools: s.apply_tools,
            apply_tools_are_ceiling: s.apply_tools_are_ceiling,
            columns: s.columns,
            editable: s
                .editable
                .into_iter()
                .map(|(field, options)| dto::AgentReviewField { field, options })
                .collect(),
        }
    })
}

/// Wall clock from the moment the job was claimed to the moment it ended.
///
/// `None` while it is still going: an elapsed time that keeps growing is a
/// different thing from how long a run took, and the Runs tab reports the
/// second one. The timestamps are SQLite's `datetime('now')`, which is UTC
/// seconds with no offset.
fn job_duration_ms(row: &store::JobRow) -> Option<i64> {
    let parse = |s: &str| {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .ok()
            .or_else(|| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|d| d.naive_utc())
            })
    };
    let start = parse(row.started_at.as_deref().unwrap_or(&row.created_at))?;
    let end = parse(row.finished_at.as_deref()?)?;
    Some((end - start).num_milliseconds().max(0))
}

/// One `agent_run` job row as the API reports it. The `phase` comes out of the
/// job's `input`, which is where the executor puts it (§3).
fn run_summary(row: &store::JobRow) -> dto::AgentRunSummary {
    let progress: crate::jobs::JobProgress =
        serde_json::from_str(&row.progress).unwrap_or_default();
    let input: Value = serde_json::from_str(&row.input).unwrap_or(Value::Null);
    // The run's own totals, from the result the executor wrote (§4.5). Absent
    // while it is still going, and `cost_micro` stays NULL when nothing could
    // be priced — "we do not know" and "it was free" are different answers.
    let result: Value = row
        .result
        .as_deref()
        .and_then(|r| serde_json::from_str(r).ok())
        .unwrap_or(Value::Null);
    let tokens = result.get("usage").map(|u| {
        u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0)
            + u.get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0)
    });
    dto::AgentRunSummary {
        job_id: row.id,
        agent_id: input
            .get("agent_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        phase: input
            .get("phase")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        status: row.status.clone(),
        done: progress.done,
        total: progress.total,
        percent: progress.percent(),
        stage: progress.stage.clone(),
        detail: progress.detail.clone(),
        tokens,
        cost_micro: result.get("cost_micro").and_then(Value::as_i64),
        duration_ms: job_duration_ms(row),
        error: row.error.clone(),
        created_at: row.created_at.clone(),
        started_at: row.started_at.clone(),
        finished_at: row.finished_at.clone(),
    }
}

/// What an agent reading one of its own runs is shown of `log` and `error`
/// (mounts §5.2, §5.3, principals §3.10).
///
/// Two host-shaped strings survive into a run row, and both are the owner's to
/// read and not the container's. The run log prints one line per bound mount,
/// host path to container path, because principle 4 says every bound mount is
/// printed — for the **owner**, who chose the folder. And a use-time refusal's
/// message names the path it refused, which lands in `jobs.error` and comes
/// back out of both runs routes, one of which the agent itself reads.
///
/// So the agent's copy is substituted, not censored: every host path this
/// agent is known to have bound becomes the container path it maps to, which
/// is the same string its own `input.json` and its own row already use. A
/// `mount_path_*` refusal is the one case where substitution is not enough —
/// `mount_path_nested` names *another* agent's id, field and folder, none of
/// which are this agent's business — so that one is rendered down to the field
/// and the code, which is everything the container can act on anyway.
///
/// The owner's view goes through none of this: [`View::Admin`] builds no
/// `RunView` at all.
#[derive(Debug, Default)]
struct RunView {
    /// Host path → container path, longest host path first so a nested pair
    /// cannot be half-substituted by its own parent.
    swap: Vec<(String, String)>,
    /// This agent's mount field names, for reading a refusal's `<field>:`
    /// prefix back off it.
    fields: Vec<String>,
}

impl RunView {
    /// Every host path this agent is known to have bound: what the row stores
    /// now, and what the job recorded it ran with (`values`, and the
    /// `effective` config of mounts §5.7). A run days old bound a folder the
    /// row no longer names, and its log still says so.
    async fn of(state: &SharedState, agent_id: &str, inputs: &[&Value]) -> Self {
        let Ok(agent) = load_agent(state, agent_id).await else {
            return Self::default();
        };
        let mut out = Self::default();
        let mut sources: Vec<Map<String, Value>> = vec![agent.config_values()];
        for input in inputs {
            for key in ["values", "effective"] {
                if let Some(Value::Object(m)) = input.get(key) {
                    sources.push(m.clone());
                }
            }
        }
        for field in agent.manifest.mount_fields() {
            out.fields.push(field.name.clone());
            for values in &sources {
                let host = values
                    .get(&field.name)
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if host.is_empty() || out.swap.iter().any(|(h, _)| h == host) {
                    continue;
                }
                out.swap.push((host.to_string(), field.inside()));
            }
        }
        out.swap
            .sort_by_key(|(host, _)| std::cmp::Reverse(host.len()));
        out
    }

    fn line(&self, text: &str) -> String {
        self.swap
            .iter()
            .fold(text.to_string(), |acc, (host, inside)| {
                acc.replace(host, inside)
            })
    }

    fn log(&self, lines: Vec<String>) -> Vec<String> {
        lines.iter().map(|l| self.line(l)).collect()
    }

    /// The stored `result` carries its own copy of the same log — that is
    /// where `log` comes from once a run has ended — so the substitution has
    /// to reach it too, or the whole document would answer what the `log`
    /// field would not. Nothing else in a result is lmgw's prose: `rows` and
    /// the usage totals are the run's own output.
    fn result(&self, result: Option<Value>) -> Option<Value> {
        let mut result = result?;
        if let Some(Value::Array(lines)) = result.get_mut("log") {
            for line in lines.iter_mut() {
                if let Value::String(text) = line {
                    *text = self.line(text);
                }
            }
        }
        Some(result)
    }

    /// A mount refusal down to `<field>: <code>`; anything else substituted
    /// like a log line.
    fn error(&self, error: Option<String>) -> Option<String> {
        let error = error?;
        let Some(code) = mount_code_of(&error) else {
            return Some(self.line(&error));
        };
        match error.split_once(':').map(|(field, _)| field.trim()) {
            Some(field) if self.fields.iter().any(|f| f == field) => {
                Some(format!("{field}: {code}"))
            }
            // A refusal whose front is not one of this agent's slots: the code
            // alone, rather than a guess at which field it was about.
            _ => Some(code.to_string()),
        }
    }
}

/// The mounts code a failed start left on `jobs.error`, if any.
///
/// The three are written by one `format!` — `"{message} ({code})"` — in both
/// start paths, so the suffix is the reliable half and the message in front of
/// it is the part that must not travel.
fn mount_code_of(error: &str) -> Option<&'static str> {
    [mounts::REFUSED, mounts::NESTED, mounts::MISSING]
        .into_iter()
        .find(|code| error.trim_end().ends_with(&format!("({code})")))
}

async fn last_run(state: &SharedState, id: &str) -> Option<dto::AgentRunSummary> {
    store::list_jobs_by_key(&state.db, agents::JOB_KIND, &agents::job_key(id), 1)
        .await
        .ok()
        .and_then(|rows| rows.first().map(run_summary))
}

async fn live_run(state: &SharedState, id: &str) -> Option<dto::AgentRunSummary> {
    store::active_job_by_key(&state.db, agents::JOB_KIND, &agents::job_key(id))
        .await
        .ok()
        .flatten()
        .as_ref()
        .map(run_summary)
}

// ---------------------------------------------------------------------------
// Reads (§5)
// ---------------------------------------------------------------------------

/// `GET /api/agents` — the catalog.
pub async fn list(State(st): State<SharedState>) -> Response {
    match list_inner(&st).await {
        Ok(cards) => Json(cards).into_response(),
        Err(e) => super::api::ops_result(Err(e)),
    }
}

pub(crate) async fn list_inner(state: &SharedState) -> Result<Vec<dto::AgentCard>, String> {
    let rows = store::list_agents(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let surface = ToolSurface::load(state).await;
    let mut podman: Option<Result<(), String>> = None;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row.id.clone();
        let (enabled, source) = (row.enabled, row.source.clone());
        let agent = match Agent::from_row(row) {
            Ok(a) => a,
            // A row this build cannot read still appears, saying why. The
            // alternative — dropping it — is an agent that silently vanishes
            // after a downgrade.
            Err(e) => {
                out.push(dto::AgentCard {
                    id,
                    enabled,
                    source,
                    error: Some(e),
                    ..Default::default()
                });
                continue;
            }
        };
        let reqs = requirements(&agent.manifest, &surface);
        let warns = if needs_podman(&agent.manifest) {
            let probe = podman_once(state, &mut podman).await;
            warnings(state, &agent, &probe).await
        } else {
            // Nothing in the runtime half applies, so nothing is shelled.
            warnings(state, &agent, &Ok(())).await
        };
        let threads = store::count_chat_threads_by_agent(&state.db, &id)
            .await
            .unwrap_or(0);
        out.push(dto::AgentCard {
            name: agent.manifest.name.clone(),
            description: agent.manifest.description.clone(),
            version: agent.manifest.version.clone().unwrap_or_default(),
            kind: agent.manifest.kind().to_string(),
            model_alias: agent.manifest.model.alias.clone(),
            effective_model: effective_model(&agent),
            labels: agent.manifest.labels(),
            enabled,
            source,
            requires_ok: requires_ok(&reqs),
            requires: reqs,
            warnings: warns,
            last_run: last_run(state, &id).await,
            threads,
            app: agents::service::service_of(&agent).is_some(),
            error: None,
            id,
        });
    }
    Ok(out)
}

/// `GET /api/agents/{id}`.
pub async fn detail(
    State(st): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> Response {
    if let Some(denied) = foreign_agent(&ctx, &id) {
        return denied;
    }
    match detail_inner(&st, &id, View::of(&ctx)).await {
        Ok(d) => Json(d).into_response(),
        Err(e) => not_found_or_bad_request(&id, e),
    }
}

/// Which rendering of an agent a read gets (principals §3.10).
///
/// Two readers, one document: the owner's page, and what the container sees of
/// itself when it reads its own row with its own token. A `bool` at the call
/// site would say `true` and not *which* of the two, and the agent side is
/// about to grow a second difference — so it is a word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    /// The owner's: today's document, host paths and all.
    Admin,
    /// The container's own. Only an `Admin` principal ever receives a host
    /// path from this route.
    Agent,
}

impl View {
    /// The view this principal reads in. An agent token is the only principal
    /// that *is* an agent; everything else that reaches a read here holds
    /// `Admin`.
    fn of(ctx: &RequestCtx) -> Self {
        match ctx.principal.agent_id() {
            Some(_) => Self::Agent,
            None => Self::Admin,
        }
    }

    /// Derive the container's copy from the finished document — **the** place
    /// the two views differ.
    ///
    /// Applied last, to an `AgentDetail` that was built once, so nothing above
    /// has to know who is reading. The mount substitution is here and nowhere
    /// else: every mount field's value in `config` becomes its container path
    /// (`/lmgw/mounts/<field>`), and `service.mounts[]` keeps `inside`, `kind`
    /// and `access` but drops `host` (mounts §5.2, principals §3.10).
    fn render(self, d: &mut dto::AgentDetail) {
        if self == Self::Admin {
            return;
        }
        // A `dev_url` is a port on *this* desk: it is the owner's browser that
        // is sent there, never the container, which has no business knowing
        // there is a dev server up at all (§3.10).
        d.dev_url = String::new();
        // A manifest names a slot and only the owner names the folder, so the
        // container is told where the folder *is from inside* and nothing more.
        // Replaced rather than removed: `{{config.notes}}` resolves to the same
        // string in the container's own templates, and a field that read as
        // absent here would look unbound.
        //
        // **Only a slot that is actually bound.** The key exists for a value
        // the owner cleared as well — `agent_config_set { "notes": "" }`
        // leaves `""` in the row — and substituting there told the container it
        // had a folder at `/lmgw/mounts/notes` while nothing was mounted on
        // it. An empty value is an unbound slot and an unbound slot is absent,
        // which is the rule `input.json` already follows (mounts §5.6).
        if let Value::Object(config) = &mut d.config {
            for name in mount_field_names_of(&d.fields, &d.manifest) {
                match config.get(&name).and_then(Value::as_str) {
                    Some("") => {
                        config.remove(&name);
                    }
                    Some(_) => {
                        config.insert(name.clone(), Value::String(manifest::mount_inside(&name)));
                    }
                    // Not a string at all: a masked value on a degraded row,
                    // which carries no path to take out.
                    None => {}
                }
            }
        }
        if let Some(service) = d.service.as_mut() {
            for m in &mut service.mounts {
                m.host = None;
            }
        }
    }
}

/// The mount fields of a finished detail document, by name (mounts §5.1).
///
/// Two sources for one question, because the document has two shapes: a row
/// this build could parse carries its rendered `fields`, and a **degraded**
/// one carries none — only the manifest text it could not read. Both ask
/// [`manifest::Format::mount_kind`] which formats count, so a newer build's
/// mount field is still recognised here as one.
fn mount_field_names_of(fields: &[dto::AgentField], manifest_text: &str) -> Vec<String> {
    let is_mount = |format: &str| {
        manifest::Format::parse(format)
            .and_then(manifest::Format::mount_kind)
            .is_some()
    };
    if !fields.is_empty() {
        return fields
            .iter()
            .filter(|f| is_mount(&f.format))
            .map(|f| f.name.clone())
            .collect();
    }
    let raw: Value = serde_json::from_str(manifest_text).unwrap_or(Value::Null);
    raw.pointer("/config/schema/properties")
        .and_then(Value::as_object)
        .map(|props| {
            props
                .iter()
                .filter(|(_, p)| is_mount(p.get("format").and_then(Value::as_str).unwrap_or("")))
                .map(|(name, _)| name.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// A detail document for a row whose manifest this build cannot read
/// (container-runtime §4.3).
///
/// `list_inner`'s degradation, extended here: a 400 would put the Definition
/// editor out of reach of the very manifest that needs editing — including the
/// pre-existing case of a row written by a newer build, which used to 400 its
/// own detail page. Whatever the raw JSON still yields is filled in; everything
/// else stays at its default, and `error` says why.
///
/// Degraded is not a hole in the masking (principals §3.10): this used to hand
/// back the stored config **in the clear**, on the reasoning that a row nothing
/// can parse has no schema to mask against. It goes through
/// [`degraded_config`] for every principal now, and through [`View::render`]
/// like any other read — a build that cannot read a manifest can still read a
/// secret out of it.
fn degraded_detail(
    row: &store::AgentRow,
    error: String,
    currency: String,
    view: View,
) -> dto::AgentDetail {
    let raw: Value = serde_json::from_str(&row.manifest).unwrap_or(Value::Null);
    let text = |key: &str| {
        raw.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let mut d = dto::AgentDetail {
        id: row.id.clone(),
        name: text("name"),
        description: text("description"),
        version: text("version"),
        kind: raw
            .get("run")
            .and_then(|r| r.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        source: row.source.clone(),
        enabled: row.enabled,
        // Verbatim, never re-serialized: this is the text the Definition editor
        // has to be able to open and fix.
        manifest: row.manifest.clone(),
        config: degraded_config(row),
        // No manifest means no schema to check against, so no Start either.
        requires_ok: false,
        resettable: seed::shipped(&row.id).is_some(),
        warnings: vec![dto::AgentWarning {
            code: "manifest_unreadable".to_string(),
            message: error.clone(),
            blocks_start: true,
        }],
        // Not `Default`, which would read as "not portable" with no reason
        // given: the manifest is what the answer is derived from, and it could
        // not be read. Say that instead of implying a verdict.
        portability: dto::AgentPortability {
            portable: false,
            notes: vec![
                "this agent's manifest could not be read by this build, so whether an export \
                 of it lands on another box cannot be judged here"
                    .to_string(),
            ],
        },
        error: Some(error),
        currency,
        created_at: row.created_at.clone(),
        updated_at: row.updated_at.clone(),
        ..Default::default()
    };
    view.render(&mut d);
    d
}

/// The stored config of a row whose manifest this build cannot read, masked
/// anyway (principals §3.10).
///
/// [`manifest::masked_values`] wants `Field`s, and the manifest they would have
/// been converted from is exactly what could not be read — so the schema is
/// taken straight out of the raw JSON instead: a property whose `format` is
/// `secret` reads back as the `has_value` view, and a key the schema does not
/// declare is dropped, both as `masked_values` does.
///
/// A manifest that does not even parse as JSON masks **every** value. "This
/// build could not tell" has to read as "secret" here; the alternative is the
/// one this function replaces, which printed it.
fn degraded_config(row: &store::AgentRow) -> Value {
    let Ok(Value::Object(stored)) = serde_json::from_str::<Value>(&row.config) else {
        return Value::Null;
    };
    let raw: Value = serde_json::from_str(&row.manifest).unwrap_or(Value::Null);
    let Some(props) = raw
        .pointer("/config/schema/properties")
        .and_then(Value::as_object)
    else {
        return Value::Object(
            stored
                .iter()
                .map(|(k, v)| (k.clone(), manifest::secret_view(is_set(v))))
                .collect(),
        );
    };
    let mut out = Map::new();
    for (name, value) in &stored {
        let Some(declared) = props.get(name) else {
            continue;
        };
        let secret = declared.get("format").and_then(Value::as_str) == Some("secret");
        out.insert(
            name.clone(),
            if secret {
                manifest::secret_view(is_set(value))
            } else {
                value.clone()
            },
        );
    }
    Value::Object(out)
}

/// What [`manifest::masked_values`] counts as a secret that is filled in:
/// present, not null, not the empty string.
fn is_set(v: &Value) -> bool {
    !v.is_null() && v.as_str() != Some("")
}

pub(crate) async fn detail_inner(
    state: &SharedState,
    id: &str,
    view: View,
) -> Result<dto::AgentDetail, String> {
    let row = store::get_agent(&state.db, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no agent with id '{id}'"))?;
    let agent = match Agent::from_row(row.clone()) {
        Ok(a) => a,
        Err(e) => {
            return Ok(degraded_detail(
                &row,
                e,
                state.snapshot().settings.currency.clone(),
                view,
            ))
        }
    };
    let surface = ToolSurface::load(state).await;
    let reqs = requirements(&agent.manifest, &surface);
    let podman = if needs_podman(&agent.manifest) {
        agents::container::podman_available(state).await
    } else {
        Ok(())
    };
    let mut warns = warnings(state, &agent, &podman).await;
    let runtime = runtime_dto(state, &agent, &podman).await;
    let fields = agent.manifest.fields().unwrap_or_default();
    let snap = state.snapshot();
    // Computed here rather than in `warnings`, which the cards and the Start
    // gate also read: this one is about the token beside it on the Definition
    // tab, and it is neither a manifest problem nor a reason not to start.
    warns.extend(token_scope_advisory(&agent, snap.settings.auth_enabled));
    let mut detail = dto::AgentDetail {
        id: agent.row.id.clone(),
        name: agent.manifest.name.clone(),
        description: agent.manifest.description.clone(),
        version: agent.manifest.version.clone().unwrap_or_default(),
        kind: agent.manifest.kind().to_string(),
        source: agent.row.source.clone(),
        enabled: agent.row.enabled,
        model_alias: agent.manifest.model.alias.clone(),
        effective_model: effective_model(&agent),
        // Canonical text, never a `Value`: see `dto::AgentDetail::manifest`.
        manifest: agent.manifest.to_json(),
        config: manifest::masked_values(&fields, &agent.config_values()),
        fields: fields_dto(&agent),
        requires_ok: requires_ok(&reqs),
        requires: reqs,
        budget: dto::AgentBudget {
            max_tool_calls: snap.settings.responses_max_tool_calls,
            timeout_seconds: snap.settings.responses_timeout_seconds,
        },
        live_job: live_run(state, id).await,
        threads: store::count_chat_threads_by_agent(&state.db, id)
            .await
            .unwrap_or(0),
        resettable: seed::shipped(id).is_some(),
        batch: batch_shape(&agent, Some(&surface)),
        warnings: warns,
        runtime,
        service: service_dto(state, &agent).await,
        // §3.4: where this row came from, when there is an answer. A row
        // written from a pasted manifest has none, and says so by being absent
        // rather than by carrying a record full of empty strings.
        provenance: {
            let p = agents::package::Provenance::of_row(&agent.row);
            (!p.is_empty()).then_some(dto::AgentProvenance {
                image: p.image,
                digest: p.digest,
                manifest_path: p.manifest_path,
                installed_at: p.installed_at,
                pulled_at: p.pulled_at,
            })
        },
        dev_url: agents::service::dev_url_of(&agent).unwrap_or_default(),
        portability: portability_dto(&agent, false),
        token: {
            let (mode, patterns) = token::derive_scope(&agent);
            dto::AgentToken {
                name: token::key_name(id),
                has_value: store::agent_key(&state.db, id)
                    .await
                    .map_err(|e| e.to_string())?
                    .is_some(),
                scope_note: token::scope_note(mode, &patterns),
            }
        },
        currency: state.snapshot().settings.currency.clone(),
        created_at: agent.row.created_at.clone(),
        updated_at: agent.row.updated_at.clone(),
        error: None,
    };
    view.render(&mut detail);
    Ok(detail)
}

/// Service mode as the App tab reads it (§3.3, §8): the manifest's declaration,
/// and what is running against it right now.
///
/// Every bound here is the manifest's own field with its value printed —
/// nothing is defaulted silently, and `0` means what it says everywhere else in
/// the runtime: never idle-stop, and wait as long as the start takes.
async fn service_dto(state: &SharedState, agent: &Agent) -> Option<dto::AgentService> {
    let s = agents::service::service_of(agent)?;
    let id = &agent.row.id;
    let live = state.agent_services.get(id);
    let settings = &state.snapshot().settings;
    Some(dto::AgentService {
        port: s.port,
        health_path: s.health_path.clone(),
        idle_seconds: s.idle_seconds,
        start_timeout_seconds: s.start_timeout_seconds,
        provides_mcp: agents::service::provides_mcp(agent).map(str::to_string),
        origin: agents::service::agent_origin(settings, id),
        // Asked at request time, with its own short timeout
        // (`service::ORIGIN_LOOKUP_TIMEOUT`): the answer is a property of the
        // machine's resolver, not of the row, and caching it would keep
        // printing "does not resolve" at an owner who has just added the hosts
        // line. A resolver that does not answer reads as `false`, never as an
        // error on this route.
        origin_resolves: agents::service::origin_resolves(settings, id).await,
        starting: state.agent_services.starting(id),
        running: live.is_some(),
        host_port: live.as_ref().map(|l| l.host_port).unwrap_or(0),
        container: live
            .as_ref()
            .map(|l| l.container.clone())
            .unwrap_or_default(),
        started_at: live.as_ref().map(|l| l.started_at_utc.to_rfc3339()),
        idle_seconds_now: live.as_ref().map(|l| l.idle_for().as_secs()),
        in_flight: live.as_ref().map(|l| l.in_flight()).unwrap_or(0),
        // §7: the App tab shows the container's log tail. Only while it is
        // running — `podman logs` on a container that is gone has nothing to
        // say — and the line count travels with it, so nobody has to guess
        // whether they are seeing all of it.
        log_tail: match &live {
            Some(l) => {
                agents::service::log_tail(
                    state,
                    id,
                    &l.container,
                    agents::service::LOG_EXCERPT_LINES,
                )
                .await
            }
            None => String::new(),
        },
        log_tail_lines: agents::service::LOG_EXCERPT_LINES,
        // What this container is holding of the owner's filesystem (mounts
        // §5.5, §5.8), bound slots only — an empty slot is nothing the App tab
        // can show a path for. The host path is here because this document is
        // built once for every reader; [`View::render`] is what takes it back
        // out again for the container's own copy (principals §3.10).
        mounts: {
            let values = agent.config_values();
            mounts::bound_fields(&agent.manifest, &values)
                .iter()
                .map(|f| dto::AgentServiceMount {
                    field: f.name.clone(),
                    host: values
                        .get(&f.name)
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    inside: f.inside(),
                    kind: f.kind.as_str().to_string(),
                    access: f.access.as_str().to_string(),
                })
                .collect()
        },
    })
}

/// `GET /api/agents/{id}/runs` — every run of this agent, newest first.
///
/// No limit: the jobs table is already bounded by two visible retention
/// settings, so a second invisible cap here would only hide rows the owner
/// asked to keep.
///
/// Read by the agent itself as well (principals §3.2). Almost nothing here
/// splits by view: a summary is progress, status, tokens, cost and the job's
/// own timestamps, the `(kind, key)` filter is what keeps another agent's runs
/// out, and the run's `input` is read for `agent_id` and `phase` and nothing
/// else — the config a run was started with never reaches this shape, which is
/// load-bearing since mounts §5.7, where `input.effective` holds the **host**
/// paths that run bound.
///
/// The one field that does split is `error`. A start refused by the path rules
/// fails the job with the refusal's own sentence, which names the folder — and
/// for `mount_path_nested`, another agent's id and field as well. [`RunView`]
/// is what the agent's copy goes through.
pub async fn runs(
    State(st): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> Response {
    if let Some(denied) = foreign_agent(&ctx, &id) {
        return denied;
    }
    let rows =
        match store::list_jobs_by_key(&st.db, agents::JOB_KIND, &agents::job_key(&id), 0).await {
            Ok(rows) => rows,
            Err(e) => return super::api::ops_result(Err(e.to_string())),
        };
    let mut out: Vec<dto::AgentRunSummary> = rows.iter().map(run_summary).collect();
    if View::of(&ctx) == View::Agent {
        let inputs: Vec<Value> = rows
            .iter()
            .map(|r| serde_json::from_str(&r.input).unwrap_or(Value::Null))
            .collect();
        let view = RunView::of(&st, &id, &inputs.iter().collect::<Vec<_>>()).await;
        for summary in &mut out {
            summary.error = view.error(summary.error.take());
        }
    }
    Json(out).into_response()
}

/// `GET /api/agents/runs/{job_id}` — one run with its rows.
///
/// Rows come from the job's `result` once it has finished. While it is running
/// they come from the executor's per-job buffer (WP3); they are never pushed
/// through the generic jobs feed, because fifty rows of raw model replies on
/// every 500 ms frame to every open dashboard tab is the wrong pipe (§3).
///
/// The agent reads its own runs here too (principals §3.2). `rows`, `result`
/// and `batch` are that run's own output and its own manifest's shape, the
/// token is redacted out of the log at the ledger already (container-runtime
/// §3.1), and `foreign_run` above is what keeps another agent's job id out.
///
/// `log` and `error` are the two that carry a **host path**, and they go
/// through [`RunView`] for an agent: the run log prints one line per bound
/// mount because the owner has to be able to see what was bound (principle 4),
/// and a use-time refusal's sentence names the folder it refused. `result.log`
/// is the same log by another name — it is where `log` comes from once a run
/// has ended — and goes through the same substitution. lmgw's own
/// diagnostic prose about this very run — the `secrets_dir_fallback` note
/// names the run directory — is left as it is: it is not a mount, a container
/// has no way to that path, and pattern-matching it out would cost the owner a
/// diagnostic.
///
/// **Ownership is answered before existence**, as in [`owned_run`]: for an
/// agent, a job id that is not its own and a job id that is nobody's are the
/// same `403 run_not_owned`, byte for byte. A `404` there would answer "this
/// id exists, it is simply not yours", which is a walk of every other agent's
/// job ids one small integer at a time. The owner, who reads every run, still
/// gets the `404` that says the id is gone.
pub async fn run_detail(
    State(st): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(job_id): Path<i64>,
) -> Response {
    let found = match store::get_job(&st.db, job_id).await {
        Ok(r) => r.filter(|r| r.kind == agents::JOB_KIND),
        Err(e) => return super::api::ops_result(Err(e.to_string())),
    };
    // `None` is "owned by nobody", which no agent is, so a missing job and a
    // foreign one leave by the same door.
    if let Some(denied) = foreign_run(&ctx, found.as_ref().and_then(|r| r.key.as_deref())) {
        return denied;
    }
    let Some(row) = found else {
        return (
            StatusCode::NOT_FOUND,
            Json(dto::ApiError {
                code: "not_found".into(),
                message: format!("no agent run with job id {job_id}"),
            }),
        )
            .into_response();
    };
    let result: Option<Value> = row
        .result
        .as_deref()
        .and_then(|r| serde_json::from_str(r).ok());
    // The live buffer while the run is in flight, the stored result after —
    // one call answers both, so the Run tab does not have to know which (§3).
    let rows: Vec<Value> = batch::rows_of(&st, job_id, result.as_ref())
        .iter()
        .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
        .collect();
    let mut summary = run_summary(&row);
    // From the agent as it is *now*: a re-run classifies against the current
    // config, so the review table has to offer the current enum (§2.4).
    let mut batch = match load_agent(&st, &summary.agent_id).await {
        // No surface here on purpose: the Run tab polls this route while a run
        // is in flight and only reads `columns`/`editable` from it, so paying
        // for a `tools/list` per poll to fill a ceiling nothing renders would be
        // the wrong trade. The ceiling the page shows comes from the detail.
        Ok(a) => batch_shape(&a, None),
        Err(_) => None,
    };
    // A ledger run may have carried column keys the manifest never declared.
    // They are appended after the declared ones, in first-seen order, so the
    // table shows them instead of quietly holding a value nothing renders
    // (container-runtime §3.2).
    let live = st.agent_ledger.get(job_id);
    let extra: Vec<String> = match &live {
        Some(run) => run.extra_columns(),
        None => result
            .as_ref()
            .and_then(|r| r.get("extra_columns"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default(),
    };
    if let Some(shape) = batch.as_mut() {
        for c in extra {
            if !shape.columns.contains(&c) {
                shape.columns.push(c);
            }
        }
    }
    // Three places, in the order a run passes through them: the ledger desk
    // while a hand-opened run is live, the executor's buffer while a container
    // run is (transport A keeps its accumulator off the desk, §3.2), and the
    // stored result once either has ended.
    let mut log: Vec<String> = match &live {
        Some(run) => run.log(),
        None => st.agent_runs.log(job_id).unwrap_or_else(|| {
            result
                .as_ref()
                .and_then(|r| r.get("log"))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default()
        }),
    };
    let mut result = result;
    if View::of(&ctx) == View::Agent {
        let input: Value = serde_json::from_str(&row.input).unwrap_or(Value::Null);
        let view = RunView::of(&st, &summary.agent_id, &[&input]).await;
        summary.error = view.error(summary.error.take());
        log = view.log(log);
        result = view.result(result);
    }
    Json(dto::AgentRunDetail {
        job: summary,
        rows,
        result,
        batch,
        log,
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// Export (§5)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ExportQuery {
    /// `1` adds the **non-secret** config values as `config_values`.
    #[serde(default)]
    include_config: Option<String>,
}

fn truthy(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("1") | Some("true") | Some("yes"))
}

/// `GET /api/agents/{id}/export` → `<id>.agent.json`.
pub async fn export(
    State(st): State<SharedState>,
    Path(id): Path<String>,
    Query(q): Query<ExportQuery>,
) -> Response {
    let body = match export_inner(&st, &id, truthy(&q.include_config)).await {
        Ok(d) => d,
        Err(e) => return not_found_or_bad_request(&id, e),
    };
    (
        [
            (header::CONTENT_TYPE, "application/json".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{id}.agent.json\""),
            ),
        ],
        body,
    )
        .into_response()
}

/// The export document: the manifest, then the envelope.
///
/// `flatten` rather than `serde_json::to_value` and insert: `serde_json::Map`
/// is a `BTreeMap` in this build, so a detour through a [`Value`] alphabetizes
/// the config schema's properties and the review columns. The form's field
/// order is the author's, not the alphabet's (§2.6), which is why the manifest
/// holds an order-preserving map — serializing this struct straight to text is
/// what keeps that order in the downloaded file.
#[derive(Serialize)]
struct ExportDoc<'a> {
    #[serde(flatten)]
    manifest: &'a manifest::Manifest,
    exported_at: String,
    lmgw_version: &'static str,
    /// The `secret` fields left out, so the receiver knows what to fill in.
    #[serde(skip_serializing_if = "Option::is_none")]
    config_omitted: Option<Vec<String>>,
    /// The mount slots left out (mounts §5.2) — beside `config_omitted`, and
    /// meaning the other half of the same sentence: these are not values the
    /// receiver types in, they are folders on the receiver's own box.
    #[serde(skip_serializing_if = "Option::is_none")]
    config_unbound: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config_values: Option<Map<String, Value>>,
    /// Whether this file lands on another box (§3.4). Always written, portable
    /// or not: "nothing here is local to the exporter" is worth saying too, and
    /// a key that appears only on failure is a key nobody looks for.
    portability: dto::AgentPortability,
}

/// The export document as text: the manifest, plus the envelope, plus
/// (optionally) the non-secret config.
///
/// Config is **deployment state**, not part of the agent, so it is left out by
/// default. Secret fields are never written with or without the flag, and
/// `config_omitted` names them so the receiver knows what to fill in.
///
/// **Mount values go the same way** (mounts §5.2): a `directory` or `file`
/// field's value is a path on *this* desk, and a manifest names a slot rather
/// than a host path (principle 3). `config_unbound` names the slots, so the
/// receiver knows there is a folder to choose rather than a value to retype.
///
/// Three things on the row are deliberately **never** written here, whatever
/// the flag says (§3.4): the agent's token, which is a credential of *this*
/// gateway; `provenance`, which is where *this* box got the package; and
/// `dev_url`, which is a path on *this* desk. What is written instead is
/// `portability` — the sentence that says an export carrying none of them may
/// still not run on the receiving box, and why.
pub(crate) async fn export_inner(
    state: &SharedState,
    id: &str,
    include_config: bool,
) -> Result<String, String> {
    let agent = load_agent(state, id).await?;
    let fields = agent.manifest.fields().unwrap_or_default();
    let omitted = manifest::secret_names(&fields);
    let unbound = manifest::mount_names(&fields);
    let doc = ExportDoc {
        manifest: &agent.manifest,
        exported_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        lmgw_version: env!("CARGO_PKG_VERSION"),
        config_omitted: (!omitted.is_empty()).then_some(omitted),
        config_unbound: (!unbound.is_empty()).then_some(unbound),
        config_values: include_config
            .then(|| manifest::without_secrets_or_mounts(&fields, &agent.config_values())),
        portability: portability_dto(&agent, true),
    };
    serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())
}

/// §3.4's portability answer, in the shape both the file and the detail page
/// carry — one function, so the dialog cannot say one thing and the downloaded
/// file another.
///
/// `redact_dev_url` is the one thing they do not share: the **verdict** and the
/// reasons are identical, but the address of a dev server on this machine is
/// for the owner reading their own dashboard, not for a file that gets mailed
/// around. `dev_url` is on the never-exported list with the token and the
/// provenance, and a URL interpolated into a note would have walked straight
/// past it.
fn portability_dto(agent: &Agent, redact_dev_url: bool) -> dto::AgentPortability {
    let (portable, notes) = agents::package::portability(agent, redact_dev_url);
    dto::AgentPortability { portable, notes }
}

// ---------------------------------------------------------------------------
// Import (§5)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ImportQuery {
    #[serde(default)]
    replace: Option<String>,
    #[serde(default)]
    validate_only: Option<String>,
}

/// `POST /api/agents/import?replace=&validate_only=` with the file as the body.
/// Paste uses the same endpoint; there is no second code path for it.
pub async fn import(
    State(st): State<SharedState>,
    Query(q): Query<ImportQuery>,
    body: Bytes,
) -> Response {
    let text = match std::str::from_utf8(&body) {
        Ok(t) => t,
        Err(e) => return super::api::ops_result(Err(format!("the file is not UTF-8: {e}"))),
    };
    match import_inner(&st, text, truthy(&q.replace), truthy(&q.validate_only)).await {
        Ok(report) => Json(report).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

/// A refused mount path, with its code on this plane (mounts §5.9).
///
/// The rules and their sentences are [`mounts`]', because a start checks them
/// again where there is no HTTP status to return; what is added here is the
/// `400` the dashboard's save reads.
fn mount_refusal(r: mounts::Refusal) -> ApiRefusal {
    ApiRefusal {
        status: StatusCode::BAD_REQUEST,
        code: r.code,
        message: r.message,
    }
}

/// The two origin refusals a manifest write can earn (origins §4.1), with
/// their codes attached.
///
/// One function because `import_inner` and `agent_duplicate` are the two ways
/// a manifest reaches the catalog, and a rule that held on one of them only
/// would be a rule an agent could be renamed around. The rules themselves are
/// `agents::service`'s; what is here is the code, which is where every other
/// code on this plane is.
fn origin_refusal(state: &SharedState, m: &manifest::Manifest) -> Option<ApiRefusal> {
    let settings = &state.snapshot().settings;
    let coded = |code: &'static str, message: String| ApiRefusal {
        status: StatusCode::BAD_REQUEST,
        code,
        message,
    };
    if let Some(why) = agents::service::origin_label_refusal(m) {
        return Some(coded("origin_label_invalid", why));
    }
    agents::service::origin_shadows_refusal(settings, m)
        .map(|why| coded("origin_shadows_gateway", why))
}

/// The one import path: the HTTP endpoint, the Definition editor's save, and
/// `lmgw__agent_set` all land here, so all three enforce the same order (§5).
///
/// Answers `Result<_, Refusal>` rather than the plane's usual
/// `Result<_, String>`: a manifest that would take the gateway's own address
/// is refused with a code of its own (`origin_shadows_gateway`), and so is an
/// id that is not a DNS label (`origin_label_invalid`). Everything else it
/// refuses is the flat `400 op_failed` it always was — `Refusal`'s
/// `From<String>` renders exactly that.
pub(crate) async fn import_inner(
    state: &SharedState,
    text: &str,
    replace: bool,
    validate_only: bool,
) -> Result<dto::AgentImportReport, ApiRefusal> {
    // 0. Strip the export envelope, keeping the config values it may carry.
    let (manifest_text, carried_config) = strip_envelope(text)?;

    // 1 + 2. Version, id, no unknown fields, templates, steps, run kind.
    let m = manifest::load(&manifest_text)?;

    // 2b. The agent origin (origins §4.1). A service-declaring manifest's id is
    // also a DNS label and also a host name, and both of those say more about
    // an id than `validate_id` can: the label rules are DNS's, and what the
    // gateway itself answers on is this install's. Before anything is written,
    // and before `validate_only` returns — "would this land?" has to include
    // this.
    if let Some(refusal) = origin_refusal(state, &m) {
        return Err(refusal);
    }

    // 3. Tools: a gap is a warning, never a refusal.
    let surface = ToolSurface::load(state).await;
    let reqs = requirements(&m, &surface);
    let mut warnings = agents::requirement_warnings(&surface.check(&m.tools), &surface);
    // The manifest half of §4.3's warnings channel is a pure function of the
    // document, which is exactly why it can run here: the import report is the
    // first place an owner sees a `localhost/` image or a container with no
    // image at all, and it is written before the row exists.
    warnings.extend(
        agents::manifest_warnings(&m, "imported")
            .into_iter()
            .map(|w| w.message),
    );

    // 4. An existing id is refused unless `replace=1`.
    let existing = store::get_agent(&state.db, &m.id)
        .await
        .map_err(|e| e.to_string())?;
    // What the row's config is right now. A replace keeps it (below), so it is
    // also the answer to "which mount slots would still be unbound", which
    // both `validate_only` and the real path report (mounts §5.2).
    let existing_config: Map<String, Value> = existing
        .as_ref()
        .and_then(|row| serde_json::from_str::<Value>(&row.config).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default();
    if let (Some(row), false, false) = (&existing, replace, validate_only) {
        // Which kind of agent is being overwritten matters: replacing a
        // **built-in** is not the same decision as replacing something the
        // owner imported, and "Reset to shipped" is the way back only from the
        // first one.
        let what = if row.source == store::AGENT_SOURCE_BUILTIN {
            " — it is a built-in that ships with lmgw, and \"Reset to shipped\" is what puts              the original back afterwards"
        } else {
            ""
        };
        return Err(format!(
            "an agent with id '{}' already exists{what}; pass replace=1 to overwrite it (its \
             stored config is kept)",
            m.id
        )
        .into());
    }

    // 4b. A carried `config_values` is a **host path** the importer did not
    // choose (mounts §5.3). `validate_values` below judges the shape of a
    // value and has nothing to say about where a path points, so a hand-made
    // package with `{"notes": "/"}` used to land in the store — and from there
    // into every other agent's rule 5 as a bound path containing the whole
    // box. The store-time rules run here, before anything is written and
    // before `validate_only` answers "would this land?", because it would not.
    // Only for a new row: a replace keeps the config it already has.
    if let (Some(values), None, true) = (
        carried_config.as_ref(),
        existing.as_ref(),
        m.declares_mounts(),
    ) {
        let ctx = mounts::ctx(state).await;
        mounts::check_values(&m.id, &m, values, &ctx, mounts::Moment::Store).map_err(|r| {
            ApiRefusal {
                status: StatusCode::BAD_REQUEST,
                code: r.code,
                message: format!(
                    "this file carries a config_values entry naming a folder this gateway will \
                     not bind, so it was not imported — {}",
                    r.message
                ),
            }
        })?;
    }

    if validate_only {
        // The collision the real import would have refused on. `validate_only`
        // answers the question "would this land?", so it has to be asked here
        // rather than discovered by pressing Import afterwards.
        if existing.is_some() && !replace {
            warnings.push(format!(
                "an agent with id '{}' already exists; importing this file would need replace=1 \
                 (its stored config would be kept).",
                m.id
            ));
        }
        return Ok(dto::AgentImportReport {
            ok: true,
            id: m.id.clone(),
            warnings,
            requires: reqs,
            replaced: existing.is_some(),
            // Nothing was written, so nothing was dropped. What *would* be is
            // not guessed at here: `validate_only` answers "would this land?",
            // and a list of keys that were not removed reads as though they had
            // been.
            dropped_config: Vec::new(),
            // The slots this document would leave for the owner to bind
            // (mounts §5.2), read against the config the row has now — the
            // same answer the real import below gives, minus the write.
            config_unbound: unbound_mounts(&m, &existing_config),
            validate_only: true,
        });
    }

    let canonical = m.to_json();
    let mut dropped_config: Vec<String> = Vec::new();
    // What the row will hold when this import is done — `existing_config`
    // unless a carried `config_values` seeds a new row below.
    let mut stored_config = existing_config.clone();
    let replaced = match &existing {
        // Replacing keeps the stored config: a manifest update is not a reason
        // to lose the taxonomy someone tuned. `source` is untouched, so an
        // edited built-in stays resettable.
        //
        // "Keeping it" stops at a value whose **field the new manifest no
        // longer declares** (WP5 review): `validate_values` refuses an
        // undeclared key, so leaving one behind produces a row that fails on
        // every run and every config save — unusable *and* unfixable from the
        // form. Those keys go with the manifest that declared them, in the same
        // transaction, and the report names them. Exactly what §5.1's built-in
        // upgrade already does, now on every replace.
        Some(_) => {
            let keep = store::AgentConfigField::of(&m);
            dropped_config =
                store::update_agent_manifest_pruned(&state.db, &m.id, &canonical, &keep)
                    .await
                    .map_err(|e| e.to_string())?;
            if !dropped_config.is_empty() {
                warnings.push(format!(
                    "the stored config values {} are not declared by the new manifest and were \
                     removed with the old one; everything else was kept.",
                    dropped_config.join(", ")
                ));
            }
            true
        }
        None => {
            store::insert_agent(&state.db, &m.id, &canonical, store::AGENT_SOURCE_IMPORTED)
                .await
                .map_err(|e| e.to_string())?;
            false
        }
    };

    // Every manifest write can move the scope: a new document may name a
    // different `format: model_alias` field, or drop the one that was there
    // (§3.1). One call here covers `agent_set`, the import endpoint and the
    // Definition editor's Save, which all land in this function.
    token::resync(state, &m.id).await?;
    // And the `agent:<id>` MCP row a `provides.mcp` manifest earns (§3.3),
    // beside the token for the same reason: every manifest write can add it,
    // change it or take it away, and one call here covers `agent_set`, the
    // import endpoint and the Definition editor's Save.
    agents::service::resync(state, &m.id).await?;
    // A manifest change is a change to what the running container *is*. It is
    // holding the old image's argv, the old limits and the old config; leaving
    // it up would serve the previous agent under the new one's name.
    if let Some(stopped) = agents::service::stop(state, &m.id, "the manifest was replaced").await {
        warnings.push(format!(
            "this agent's app was started from the manifest the import just replaced, so it was \
             stopped ({}). The next request to {} starts the new one.",
            stopped.describe(),
            agents::service::agent_origin(&state.snapshot().settings, &m.id)
        ));
    }

    // A file carrying `config_values` seeds a *new* agent's config; a replace
    // keeps what is stored, which §5 is explicit about. Values that do not fit
    // the schema are reported and skipped — a bad default in a downloaded file
    // must not block the import of the agent itself.
    if let Some(values) = carried_config {
        if replaced {
            // Silently dropping them would look like they had been applied the
            // next time someone read the Run tab.
            if !values.is_empty() {
                warnings.push(format!(
                    "the file's config_values ({}) were ignored: replacing an agent keeps the \
                     config it already has.",
                    values.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
        } else {
            let fields = m.fields().unwrap_or_default();
            match manifest::validate_values(&fields, &values) {
                Ok(()) => {
                    let json = Value::Object(values.clone()).to_string();
                    store::set_agent_config(&state.db, &m.id, &json)
                        .await
                        .map_err(|e| e.to_string())?;
                    stored_config = values;
                }
                Err(e) => warnings.push(format!(
                    "the file's config_values were not applied: {e}. Set them on the Run tab."
                )),
            }
        }
    }

    let config_unbound = unbound_mounts(&m, &stored_config);
    Ok(dto::AgentImportReport {
        ok: true,
        id: m.id,
        warnings,
        requires: reqs,
        replaced,
        dropped_config,
        config_unbound,
        validate_only: false,
    })
}

/// The mount slots this agent has and nothing has bound (mounts §5.2).
///
/// An import is where a manifest and a stored config meet, and a mount field
/// arrives empty by construction: the export stripped the value. Naming the
/// slots in the report is what turns "imported, nothing to do" into "imported;
/// bind these" — the Run tab is where they are bound, and the blocking
/// `mount_unbound` warning follows for the required ones.
fn unbound_mounts(m: &manifest::Manifest, values: &Map<String, Value>) -> Vec<String> {
    m.mount_fields()
        .filter(|f| {
            values
                .get(&f.name)
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        })
        .map(|f| f.name)
        .collect()
}

/// Split an export document into the manifest text and the config it carried.
///
/// The values are re-emitted as their **raw text**, never through a [`Value`]:
/// `serde_json::Map` is a `BTreeMap` here, so a `Value` detour would
/// alphabetize the config schema's properties on the way back in and quietly
/// re-sort the form the author laid out (§2.6). The envelope keys can sit
/// anywhere in the document, so the top level is rebuilt; everything below it
/// is copied verbatim.
fn strip_envelope(text: &str) -> Result<(String, Option<Map<String, Value>>), String> {
    let doc: manifest::OrderedMap<Box<RawValue>> = match serde_json::from_str(text) {
        Ok(d) => d,
        // Not JSON, not an object, or a duplicate top-level key: `load` says so
        // better than anything phrased here, so hand it the text unchanged.
        Err(_) => return Ok((text.to_string(), None)),
    };
    if !ENVELOPE_KEYS.iter().any(|k| doc.get(k).is_some()) {
        return Ok((text.to_string(), None));
    }
    let carried = doc
        .get("config_values")
        .and_then(|raw| serde_json::from_str::<Map<String, Value>>(raw.get()).ok());
    let mut out = String::with_capacity(text.len());
    out.push('{');
    for (k, v) in doc.iter() {
        if ENVELOPE_KEYS.contains(&k.as_str()) {
            continue;
        }
        if out.len() > 1 {
            out.push(',');
        }
        out.push_str(&Value::String(k.clone()).to_string());
        out.push(':');
        out.push_str(v.get());
    }
    out.push('}');
    Ok((out, carried))
}

// ---------------------------------------------------------------------------
// Ops (§5) — dispatched from `super::api::op` by the `agent` prefix
// ---------------------------------------------------------------------------

type Args = Map<String, Value>;

fn arg_str<'a>(args: &'a Args, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn need_id(args: &Args) -> Result<String, String> {
    match arg_str(args, "id") {
        Some(id) if !id.is_empty() => Ok(id.to_string()),
        _ => Err("pass id".to_string()),
    }
}

async fn load_agent(state: &SharedState, id: &str) -> Result<Agent, String> {
    let row = store::get_agent(&state.db, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no agent with id '{id}'"))?;
    Agent::from_row(row)
}

/// `no agent with id …` is a 404; everything else is the plane's usual 400.
fn not_found_or_bad_request(id: &str, e: String) -> Response {
    if e == format!("no agent with id '{id}'") {
        return (
            StatusCode::NOT_FOUND,
            Json(dto::ApiError {
                code: "not_found".into(),
                message: e,
            }),
        )
            .into_response();
    }
    super::api::ops_result(Err(e))
}

/// The catalog's ops, dispatched off `api::op` (§5).
///
/// `Result<Value, Refusal>` for the three writes that answer with a code of
/// their own: an id that is not a DNS label, an origin that is the gateway's
/// own address (origins §4.1), and a mount path the rules refuse (mounts
/// §5.3). Every other op here refuses with the flat `op_failed` it always did
/// — the `?` below is what renders it.
pub(super) async fn op(state: &SharedState, name: &str, args: Args) -> Result<Value, ApiRefusal> {
    match name {
        "agent_set" => return agent_set(state, &args).await,
        "agent_duplicate" => return agent_duplicate(state, &args).await,
        "agent_config_set" => return agent_config_set(state, &args).await,
        _ => {}
    }
    Ok(match name {
        "agent_install" => agent_install(state, &args).await,
        "agent_pull" => agent_pull(state, &need_id(&args)?).await,
        "agent_reimport" => agent_reimport(state, &need_id(&args)?).await,
        "agent_dev_url_set" => agent_dev_url_set(state, &args).await,
        "agent_enable" => agent_enable(state, &args).await,
        "agent_delete" => agent_delete(state, &need_id(&args)?).await,
        "agent_reset" => agent_reset(state, &need_id(&args)?).await,
        "agent_open_chat" => agents::chat::open(state, &need_id(&args)?, &values_arg(&args)?).await,
        "agent_run" => agent_run(state, &args).await,
        "agent_run_cancel" => agent_run_cancel(state, &need_id(&args)?).await,
        "agent_token_get" => agent_token(state, &need_id(&args)?, false).await,
        "agent_token_rotate" => agent_token(state, &need_id(&args)?, true).await,
        "agent_service_start" => agent_service(state, &need_id(&args)?, true).await,
        "agent_service_stop" => agent_service(state, &need_id(&args)?, false).await,
        "agent_service_log" => agent_service_log(state, &args).await,
        "agents_restore" => seed::restore(state).await,
        other => Err(format!("unknown op '{other}'")),
    }?)
}

/// A `manifest` argument as text, plus the warning an object form earns.
///
/// Both shapes are accepted: a JSON **string** (what the Definition editor and
/// the self-admin tool hand over) and an object (what a JSON client naturally
/// sends). They are not equivalent, and the difference is not the caller's to
/// guess at — by the time an op or a tool call reaches this point an object has
/// already been parsed into a `serde_json::Map`, a `BTreeMap` in this build, so
/// its keys are in alphabetical order and the config form the author laid out
/// top to bottom (§2.6) has been re-sorted. That is accepted and *said*, rather
/// than refused: the order is cosmetic, the agent is not.
/// A `values` argument: the Run tab's form as it stands, a sparse patch over
/// the stored config applied to **this** start only.
///
/// Absent is the honest default for a caller that has no form — the MCP tool,
/// a curl — and means "use what is saved". The dashboard always sends one,
/// because on that page the form *is* the config for this run.
fn values_arg(args: &Args) -> Result<Map<String, Value>, String> {
    match args.get("values") {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => Ok(m.clone()),
        Some(_) => Err("values must be an object".to_string()),
    }
}

pub(crate) fn manifest_arg(v: Option<&Value>) -> Result<(String, Option<String>), String> {
    match v {
        Some(Value::String(s)) => Ok((s.clone(), None)),
        Some(obj @ Value::Object(_)) => Ok((
            obj.to_string(),
            Some(
                "the manifest arrived as a JSON object, so its config form is in the key order \
                 the object parsed into — alphabetical, not the order the fields were written \
                 in. Pass the manifest as a JSON string to keep the author's order."
                    .to_string(),
            ),
        )),
        Some(_) => Err("manifest must be a JSON object or a JSON string".to_string()),
        None => Err("pass manifest".to_string()),
    }
}

/// `agent_set` — create or update from a manifest. The import path verbatim,
/// so the Definition editor and a dropped file cannot diverge.
/// `agent_set { manifest, replace?, validate_only? }` — the Definition editor's
/// Save and the self-admin tool's write.
///
/// One of the two ops that answers with a code of its own (origins §4.1,
/// design §7): a manifest whose id is not a DNS label, or whose origin is the
/// gateway's own address, is refused as `origin_label_invalid` /
/// `origin_shadows_gateway` rather than as a flat `op_failed`.
async fn agent_set(state: &SharedState, args: &Args) -> Result<Value, ApiRefusal> {
    let (text, order_warning) = manifest_arg(args.get("manifest"))?;
    // An update through this op is an update: `replace` defaults to true,
    // because the editor's Save is by definition aimed at the agent it opened.
    // The import *endpoint* defaults the other way, where a dropped file
    // overwriting something is a surprise.
    let replace = args.get("replace").and_then(Value::as_bool).unwrap_or(true);
    let validate_only = args
        .get("validate_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut report = import_inner(state, &text, replace, validate_only).await?;
    report.warnings.extend(order_warning);
    Ok(serde_json::to_value(report).map_err(|e| e.to_string())?)
}

// ---------------------------------------------------------------------------
// The package: install, pull, re-import (container-runtime §3.4, WP5)
// ---------------------------------------------------------------------------

/// `agent_install { image, pull?, replace?, validate_only? }` (§3.4, §8).
///
/// An install is **an image reference and nothing else**: lmgw reads
/// `/lmgw/agent.json` out of the image with `create`/`cp`/`rm` and hands the
/// text to [`import_inner`] — the same path a dropped file takes, so every
/// validation error, every tool-gap warning and `local_image_on_import` all
/// apply unchanged and there is no second import to keep in step.
///
/// **The manifest inside the image is not rewritten.** If it names a different
/// `run.image` than the reference that was installed, the manifest wins — it is
/// what every phase and the app actually start — and both the report and the
/// row's warnings say the two disagree (`install_image_mismatch`). Rewriting it
/// would mean the exported document no longer matched the package it came from.
///
/// `replace` defaults to **false**, the import *endpoint*'s default rather than
/// `agent_set`'s: the id being installed is the one inside the image, which the
/// caller has not read yet, so overwriting an existing agent has to be asked
/// for.
pub(crate) async fn agent_install(state: &SharedState, args: &Args) -> Result<Value, String> {
    use agents::package::{self, Provenance};

    let image = arg_str(args, "image")
        .unwrap_or_default()
        .trim()
        .to_string();
    if image.is_empty() {
        return Err(
            "pass image — an agent package is an OCI image carrying its manifest at \
                    /lmgw/agent.json"
                .to_string(),
        );
    }
    let pull = match arg_str(args, "pull") {
        None => manifest::PullPolicy::default(),
        Some(s) => manifest::PullPolicy::parse(s).ok_or_else(|| {
            format!("pull is 'never', 'missing' or 'always'; got '{s}'. The default is 'never'.")
        })?,
    };
    let replace = args
        .get("replace")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let validate_only = args
        .get("validate_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let pulled = package::ensure_image(state, &image, pull).await?;
    let text = package::read_manifest(state, &image).await?;
    // Parsed here as well as inside the import, for the one question the report
    // has to answer that the import knows nothing about: which image this
    // document says it runs. `load` is a pure parse of text already in memory.
    let declared = manifest::load(&text)
        .ok()
        .and_then(|m| m.image().map(str::to_string));

    // The id collision is only discoverable *after* the image has been read —
    // the id is inside the manifest — so under `missing`/`always` the download
    // has already happened by the time the import refuses. Say so, rather than
    // leaving the owner to wonder whether the several gigabytes are on the box
    // (they are).
    let mut report = match import_inner(state, &text, replace, validate_only).await {
        Ok(r) => r,
        // The refusal's *code* stops here: an install answers on the flat
        // `op_failed` this op has always had, and the message — which names
        // the rule — is what the owner reads either way.
        Err(e) if pulled => {
            return Err(format!(
                "{} The image '{image}' was downloaded before this was discovered and is on the \
                 box now, so retrying with replace costs nothing.",
                e.message
            ))
        }
        Err(e) => return Err(e.message),
    };
    let digest = package::image_digest(state, &image).await;
    if digest.is_none() {
        report.warnings.push(format!(
            "podman could not report a digest for '{image}', so this row records none. \
             'Pull image' will fill it in once it can."
        ));
    }
    if let Some(declared) = &declared {
        if declared != &image {
            report.warnings.push(format!(
                "the manifest inside '{image}' names run.image '{declared}'. The manifest wins — \
                 that is the image a run, an apply and the app start — and the install is \
                 recorded against '{image}'."
            ));
        }
    }
    if !validate_only {
        let now = package::now();
        let prov = Provenance {
            image: image.clone(),
            digest: digest.clone().unwrap_or_default(),
            manifest_path: package::MANIFEST_INSIDE.to_string(),
            installed_at: now.clone(),
            pulled_at: now,
        };
        store::set_agent_provenance(&state.db, &report.id, &prov.to_json())
            .await
            .map_err(|e| e.to_string())?;
    }
    let id = report.id.clone();
    let mut out = serde_json::to_value(&report).map_err(|e| e.to_string())?;
    if let Some(obj) = out.as_object_mut() {
        obj.insert("image".into(), json!(image));
        obj.insert("digest".into(), json!(digest.unwrap_or_default()));
        // A download is not a detail to infer from how long the call took.
        obj.insert("pulled".into(), json!(pulled));
        obj.insert("manifest_path".into(), json!(package::MANIFEST_INSIDE));
        obj.insert(
            "message".into(),
            json!(if report.validate_only {
                format!("'{id}' is what '{image}' would install — nothing written")
            } else if report.replaced {
                format!("installed '{id}' from '{image}', keeping its config")
            } else {
                format!("installed '{id}' from '{image}'")
            }),
        );
    }
    Ok(out)
}

/// `agent_pull { id }` (§3.4, §8, §12) — fetch this agent's image again and say
/// whether it moved.
///
/// **Pressing this is the consent**, so it pulls under every policy, `never`
/// included: `never` is what stops a *Start* from turning into a download
/// nobody asked for, and this button is exactly the explicit act §3.4 says the
/// UI then has to offer. Under `always` there is nothing extra to force.
///
/// §12's image-update question, at its honest minimum: the digest before and
/// after are compared, and **when it moved the manifest is read out of the new
/// image and compared with the stored one**. No background polling, no
/// automatic re-import — the answer names `agent_reimport` and stops there,
/// because adopting a new document over a row an owner may have edited is their
/// decision, exactly as §5.1's built-in upgrade is.
async fn agent_pull(state: &SharedState, id: &str) -> Result<Value, String> {
    use agents::package::{self, Provenance};

    let agent = load_agent(state, id).await?;
    let image = package::image_of(&agent).ok_or_else(|| {
        format!(
            "'{id}' names no image: there is nothing to pull. Set run.image on the \
                 Definition tab, or install it from an image with agent_install."
        )
    })?;
    let mut prov = Provenance::of_row(&agent.row);
    // Two "befores", and they answer different questions: what podman has right
    // now (did the tag move?) and what provenance recorded (is this row still
    // on the image it was installed from?). The comparison below uses the
    // first; the second is reported so a divergence is visible.
    let before = package::image_digest(state, &image).await;
    package::pull_image(state, &image).await?;
    let after = package::image_digest(state, &image).await;

    prov.image = if prov.image.is_empty() {
        image.clone()
    } else {
        prov.image
    };
    // **`manifest_path` is not filled in here.** A pull reads no manifest, and
    // an authored row that has never been installed from a package would
    // otherwise end up claiming its document came out of `/lmgw/agent.json`
    // when nothing ever looked there. Same for `installed_at`, which stays
    // empty and is what the Package block keys "installed from" on: a row with
    // a digest and no `installed_at` is an image that was pulled, not a package
    // this row came from.
    let recorded = std::mem::take(&mut prov.digest);
    prov.digest = after.clone().unwrap_or_default();
    prov.pulled_at = package::now();
    store::set_agent_provenance(&state.db, id, &prov.to_json())
        .await
        .map_err(|e| e.to_string())?;

    let moved = match (&before, &after) {
        (Some(b), Some(a)) => b != a,
        _ => false,
    };
    // Only asked when it moved: reading the manifest is three more podman
    // invocations, and an image that did not move cannot be carrying a
    // different document than the one that was read from it.
    let (manifest_differs, note) = if moved {
        match package::read_manifest(state, &image).await {
            Ok(text) => match manifest::load(&text) {
                Ok(m) => {
                    let differs = m.to_json() != agent.manifest.to_json();
                    let id_differs = m.id != agent.row.id;
                    (
                        Some(differs),
                        if id_differs {
                            format!(
                                "the new image carries an agent with id '{}', not '{id}' — \
                                 'Re-import from image' would refuse it; install it as its own \
                                 agent instead.",
                                m.id
                            )
                        } else if differs {
                            "the new image carries a different manifest. 'Re-import from image' \
                             adopts it and keeps this row's config."
                                .to_string()
                        } else {
                            "the new image carries the same manifest, so there is nothing to \
                             re-import."
                                .to_string()
                        },
                    )
                }
                Err(e) => (
                    None,
                    format!("the new image's manifest could not be read by this build: {e}"),
                ),
            },
            Err(e) if e.code == "package_no_manifest" => (
                None,
                format!(
                    "the new image carries no {}, so it is an ordinary image rather than an \
                     agent package and there is nothing to re-import.",
                    package::MANIFEST_INSIDE
                ),
            ),
            Err(e) => (None, e.to_string()),
        }
    } else {
        (None, String::new())
    };

    let message = match (&before, &after) {
        (Some(b), Some(a)) if b == a => format!("'{image}' is unchanged ({a})"),
        (Some(b), Some(a)) => format!("'{image}' moved: {b} → {a}"),
        (None, Some(a)) => format!("pulled '{image}' ({a}); it was not on this box before"),
        (_, None) => format!("pulled '{image}'; podman could not report a digest for it"),
    };
    Ok(json!({
        "ok": true,
        "id": id,
        "image": image,
        "old_digest": before.unwrap_or_default(),
        "new_digest": after.unwrap_or_default(),
        // What the row said before this pull — different from `old_digest`
        // whenever something else on the box moved the image.
        "recorded_digest": recorded,
        "changed": moved,
        "manifest_differs": manifest_differs,
        "note": note,
        "message": message,
    }))
}

/// `agent_reimport { id }` (§12's answer, WP5) — run the install path again
/// over an existing row.
///
/// The built-in upgrade's shape (§5.1) for a package: the manifest is replaced
/// and **the config is kept**, because a new document is not a reason to lose
/// the taxonomy someone tuned. It is never automatic — `agent_pull` reports
/// that the image's manifest differs, and this is the owner saying yes.
///
/// A package whose manifest carries a *different id* is refused naming both:
/// importing it would write a second row and leave this one pointing at an
/// image that no longer describes it.
async fn agent_reimport(state: &SharedState, id: &str) -> Result<Value, String> {
    use agents::package::{self, Provenance};

    let agent = load_agent(state, id).await?;
    let image = package::image_of(&agent).ok_or_else(|| {
        format!("'{id}' names no image, so there is no package to re-import from")
    })?;
    package::ensure_image(state, &image, agent.manifest.pull()).await?;
    let text = package::read_manifest(state, &image).await?;
    let incoming = manifest::load(&text)?;
    if incoming.id != agent.row.id {
        return Err(format!(
            "the image '{image}' carries the agent '{}', not '{id}'; re-importing it here would \
             rename this row. Install it as its own agent with agent_install instead.",
            incoming.id
        ));
    }
    let changed = incoming.to_json() != agent.manifest.to_json();
    // `replace = true`: the row exists and this is the act of adopting the
    // image's document over it. `import_inner` keeps the stored config, stops a
    // running app container and re-syncs the token and the MCP row.
    let mut report = import_inner(state, &text, true, false)
        .await
        .map_err(|e| e.message)?;
    if !changed {
        report.warnings.push(format!(
            "the image's manifest is identical to the one already stored, so nothing about \
             '{id}' changed."
        ));
    }
    let mut prov = Provenance::of_row(&agent.row);
    let now = package::now();
    if prov.image.is_empty() {
        prov.image = image.clone();
    }
    prov.manifest_path = package::MANIFEST_INSIDE.to_string();
    prov.digest = package::image_digest(state, &image)
        .await
        .unwrap_or_default();
    prov.installed_at = now.clone();
    prov.pulled_at = now;
    store::set_agent_provenance(&state.db, id, &prov.to_json())
        .await
        .map_err(|e| e.to_string())?;

    let mut out = serde_json::to_value(&report).map_err(|e| e.to_string())?;
    if let Some(obj) = out.as_object_mut() {
        obj.insert("image".into(), json!(image));
        obj.insert("changed".into(), json!(changed));
        obj.insert(
            "message".into(),
            json!(if changed {
                format!("re-imported '{id}' from '{image}', keeping its config")
            } else {
                format!("'{id}' already matches '{image}'; nothing changed")
            }),
        );
    }
    Ok(out)
}

/// `agent_dev_url_set { id, url? }` (§3.4, §8) — point this agent's app at a
/// dev server, or (`null`) back at its image.
///
/// A **row** setting and never part of the manifest: a manifest naming
/// `localhost:5173` would ship a broken agent. It overrides service mode only —
/// a run or an apply still starts the image, because their dev loop is a
/// rebuild — and the `provides.mcp` registration is untouched, because it
/// points at lmgw's own stable proxy URL either way.
///
/// Setting one **stops a running app container**: it was started from the image
/// this override replaces, and leaving it up would burn memory serving nobody.
async fn agent_dev_url_set(state: &SharedState, args: &Args) -> Result<Value, String> {
    let id = need_id(args)?;
    let agent = load_agent(state, &id).await?;
    let raw = match args.get("url") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.trim().is_empty() => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err("url must be a string, or null to clear it".to_string()),
    };
    let Some(raw) = raw else {
        store::set_agent_dev_url(&state.db, &id, None)
            .await
            .map_err(|e| e.to_string())?;
        // Answered: whatever a bind-address change had to take away, the owner
        // has now decided about (§3.4).
        agents::service::forget_cleared_dev_url(state, &id).await;
        return Ok(json!({
            "ok": true,
            "id": id,
            "dev_url": "",
            "message": format!(
                "'{id}' is served from its image again; the next request to {} starts the \
                 container",
                agents::service::agent_origin(&state.snapshot().settings, &id)
            ),
        }));
    };
    if agents::service::service_of(&agent).is_none() {
        return Err(format!(
            "'{id}' declares no run.service, so it has no app to point at a dev server. Add a \
             service block to its manifest first."
        ));
    }
    let url = agents::service::validate_dev_url(&raw, &state.snapshot().settings.bind_addr)?;
    store::set_agent_dev_url(&state.db, &id, Some(&url))
        .await
        .map_err(|e| e.to_string())?;
    agents::service::forget_cleared_dev_url(state, &id).await;
    let stopped = agents::service::stop(state, &id, "a dev_url was set for this agent").await;
    Ok(json!({
        "ok": true,
        "id": id,
        "dev_url": url,
        "stopped": stopped.as_ref().and_then(|s| s.container.clone()),
        "message": match &stopped {
            Some(s) => format!(
                "'{id}' is served from {url}; its app container was started from the image, so \
                 it was stopped ({})",
                s.describe()
            ),
            None => format!("'{id}' is served from {url}; no container is started for its app"),
        },
    }))
}

/// `agent_config_set` — a sparse patch over the stored values (§2.6).
///
/// `values` is the patch; `clear` is the way out of the patch's one asymmetry.
/// An empty submission **keeps** a stored secret (the house convention for
/// tokens), which leaves no gesture for "this token is revoked, forget it" —
/// so removing a value is its own, explicit list. It is not limited to
/// secrets: any field can be reset to its schema default this way, and a
/// required field with no default then fails validation naming itself, which
/// is the honest answer to "clear the thing the agent cannot run without".
async fn agent_config_set(state: &SharedState, args: &Args) -> Result<Value, ApiRefusal> {
    let id = need_id(args)?;
    let incoming = match args.get("values") {
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err("values must be an object".to_string().into()),
        None => return Err("pass values".to_string().into()),
    };
    let clear: Vec<String> = match args.get("clear") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => {
                        return Err(format!("clear names {item}, which is not a field name").into())
                    }
                }
            }
            out
        }
        Some(_) => return Err("clear must be an array of field names".to_string().into()),
    };
    let agent = load_agent(state, &id).await?;
    let fields = agent.manifest.fields().map_err(|e| e.join("; "))?;
    // A name the schema does not declare is **cleared, not refused** (WP5
    // review): a stored value whose field a new manifest dropped is exactly the
    // thing that needs removing, and refusing to name it would leave the only
    // gesture that could remove it out of reach. `merge_values` never
    // reintroduces it, so this is a one-way door out.
    let undeclared: Vec<String> = clear
        .iter()
        .filter(|name| !fields.iter().any(|f| &&f.name == name))
        .cloned()
        .collect();
    let mut merged = manifest::merge_values(&fields, &agent.config_values(), &incoming);
    // After the merge, so `{ "values": { "token": "new" }, "clear": ["token"] }`
    // is a clear rather than a coin toss about map ordering.
    merged.retain(|k, _| !clear.iter().any(|c| c == k));
    manifest::validate_values(&fields, &merged)?;
    let before = agent.config_values();
    // The path rules, at store time (mounts §5.3): the value the owner picked
    // is refused here with the field and the rule named, and what is written is
    // the **canonical** path — symlinks resolved — so the row, the argv and
    // the run log all say the same string.
    //
    // Read for a manifest that declares a slot and not otherwise: rule 5 asks
    // the whole catalog what is bound right now, and an agent with no mount
    // field has no question to ask it.
    //
    // **Only the slots this call touches.** Checking every mount field of the
    // merged map made one dead folder a wall in front of every other field:
    // `label_prefix` could not be saved while `notes` pointed at a disk that
    // was unplugged, and the only field that would have fixed it was behind
    // the same refusal. A save is judged on what it *changes*; a mount nobody
    // touched is still refused at use, where it is about to matter.
    if agent.manifest.declares_mounts() {
        let touched: Map<String, Value> = merged
            .iter()
            .filter(|(name, value)| {
                agent.manifest.mount_fields().any(|f| &&f.name == name)
                    && (incoming.contains_key(*name) || before.get(*name) != Some(value))
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        let ctx = mounts::ctx(state).await;
        for bound in
            mounts::check_values(&id, &agent.manifest, &touched, &ctx, mounts::Moment::Store)
                .map_err(mount_refusal)?
        {
            merged.insert(
                bound.field.name.clone(),
                Value::String(bound.host.display().to_string()),
            );
        }
    }
    // Which mounts moved — cleared as much as re-pointed, since a slot that is
    // now empty is not the one the container is holding either.
    let repointed: Vec<String> = agent
        .manifest
        .mount_fields()
        .filter(|f| {
            let was = before.get(&f.name).and_then(Value::as_str).unwrap_or("");
            let now = merged.get(&f.name).and_then(Value::as_str).unwrap_or("");
            was != now
        })
        .map(|f| f.name)
        .collect();
    let json = Value::Object(merged.clone()).to_string();
    store::set_agent_config(&state.db, &id, &json)
        .await
        .map_err(|e| e.to_string())?;
    // The token's scope is derived from the model-alias fields, so saving a
    // config is exactly when it has to be recomputed (§3.1). A no-op for an
    // agent that has never minted one.
    let saved = load_agent(state, &id).await?;
    token::recompute_scope(state, &saved).await?;
    let (mode, patterns) = token::derive_scope(&saved);
    // The tenth reason a running app container is stopped (mounts §5.7): it
    // was started with the old folder bound, and a service container binds
    // what the **stored** config says. Leaving it up would serve an agent
    // reading a directory its owner has just re-pointed.
    let stopped = match repointed.is_empty() {
        true => None,
        false => {
            agents::service::stop(state, &id, "it is holding a mount the owner has re-pointed")
                .await
        }
    };
    let mut message = if undeclared.is_empty() {
        "config saved".to_string()
    } else {
        format!(
            "config saved; {} {} not declared by this manifest and {} removed from the stored \
             config",
            undeclared.join(", "),
            if undeclared.len() == 1 { "is" } else { "are" },
            if undeclared.len() == 1 { "was" } else { "were" },
        )
    };
    if let Some(s) = &stopped {
        message.push_str(&format!(
            "; the app container was holding the mount for {} and was stopped ({})",
            repointed.join(", "),
            s.describe()
        ));
    }
    Ok(json!({
        "ok": true,
        "id": id,
        "token_scope": token::scope_note(mode, &patterns),
        "mounts_repointed": repointed,
        "service_stopped": stopped.as_ref().and_then(|s| s.container.clone()),
        // What is now stored, masked — echoing the pre-merge values would make
        // a saved change look like it had not been saved.
        "config": manifest::masked_values(&fields, &merged),
        // Said rather than silent: clearing a name the schema does not declare
        // is legitimate (it is how a value orphaned by a manifest change goes
        // away) but it is not the ordinary case, and a typo in `clear` looks
        // exactly like it.
        "cleared_undeclared": undeclared,
        "message": message,
    }))
}

async fn agent_enable(state: &SharedState, args: &Args) -> Result<Value, String> {
    let id = need_id(args)?;
    let enabled = args
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or("pass enabled: true or false")?;
    let n = store::set_agent_enabled(&state.db, &id, enabled)
        .await
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Err(format!("no agent with id '{id}'"));
    }
    // Disable is the kill switch (§3.1): the agent's token stops authenticating
    // anywhere, which is one `UPDATE` because `verify_api_key` already filters
    // on `enabled` — no call site has to remember to check.
    token::resync(state, &id).await?;
    agents::service::resync(state, &id).await?;
    // Disabling is the kill switch for the app too: `service::ensure` refuses a
    // disabled agent, and a container already up would otherwise keep serving.
    let stopped = if enabled {
        None
    } else {
        agents::service::stop(state, &id, "the agent was disabled")
            .await
            .map(|s| s.describe())
    };
    Ok(json!({
        "ok": true,
        "id": id,
        "enabled": enabled,
        "service_stopped": stopped,
        "message": if enabled { "agent enabled" } else { "agent disabled" },
    }))
}

/// A built-in is deleted like any other agent, and **stays** deleted: the seed
/// never resurrects it (§3). "Restore shipped agents" is the deliberate way
/// back.
///
/// The agent's Chat threads are **kept** and unlinked
/// ([`store::clear_chat_thread_agent`]). `chat_threads.agent_id` has no foreign
/// key and the spec does not say what deleting an agent does to them, so the
/// rule is the one the rest of the app follows for user data: a conversation is
/// what was said, an agent is only the preset it was said through. The op
/// reports the count rather than doing it quietly.
pub(crate) async fn agent_delete(state: &SharedState, id: &str) -> Result<Value, String> {
    let existed = store::delete_agent(&state.db, id)
        .await
        .map_err(|e| e.to_string())?;
    if !existed {
        return Err(format!("no agent with id '{id}'"));
    }
    // `ON DELETE` is manual (§3.1): the agent's credential goes with it, and
    // the AUTOINCREMENT in 0032 is what stops the next key inheriting the id
    // its `request_logs` rows point at.
    store::delete_agent_key(&state.db, id)
        .await
        .map_err(|e| e.to_string())?;
    // The `agent:<id>` MCP row goes with the agent (§3.3), and so does the
    // container serving it.
    agents::service::stop(state, id, "the agent was deleted").await;
    agents::service::drop_mcp_registration(state, id).await?;
    state.reload_snapshot().await.map_err(|e| e.to_string())?;
    let unlinked = store::clear_chat_thread_agent(&state.db, id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "ok": true,
        "id": id,
        "threads_kept": unlinked,
        "message": match unlinked {
            0 => format!("deleted agent '{id}'"),
            1 => format!("deleted agent '{id}'; its 1 chat thread was kept, on the Chat page"),
            n => format!("deleted agent '{id}'; its {n} chat threads were kept, on the Chat page"),
        },
        "hint": if seed::shipped(id).is_some() {
            "this was a shipped agent; 'Restore shipped agents' (agents_restore) brings it back"
        } else {
            "gone; import the exported file to bring it back"
        },
    }))
}

/// `agent_duplicate` — same manifest under a new id, config copied **minus
/// secrets**: a copy must not silently inherit a credential.
/// `agent_duplicate { id, new_id, name? }` — a copy under a new id.
///
/// The second op that can answer with an origin code: the copy's id is a new
/// host name, and a manifest that declares `run.service` has to earn it the
/// same way an import does.
async fn agent_duplicate(state: &SharedState, args: &Args) -> Result<Value, ApiRefusal> {
    let id = need_id(args)?;
    let new_id = arg_str(args, "new_id")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "pass new_id".to_string())?
        .to_string();
    manifest::validate_id(&new_id)?;
    if store::get_agent(&state.db, &new_id)
        .await
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Err(format!("an agent with id '{new_id}' already exists").into());
    }
    let agent = load_agent(state, &id).await?;
    let mut m = agent.manifest.clone();
    m.id = new_id.clone();
    m.name = arg_str(args, "name")
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} (copy)", m.name));
    m.validate()?;
    if let Some(refusal) = origin_refusal(state, &m) {
        return Err(refusal);
    }

    let fields = agent.manifest.fields().unwrap_or_default();
    let config = manifest::without_secrets(&fields, &agent.config_values());
    let dropped = manifest::secret_names(&fields);
    // A copy carries the original's folders (§5.2), which makes this a
    // **store** of a mount value like any other: the rules run before the row
    // exists (mounts §5.3). The same path bound twice is allowed — that is
    // what the shared label is for — so an ordinary duplicate goes through;
    // what does not is a value the original was holding from before these
    // rules, or one whose folder has gone since.
    if m.declares_mounts() {
        let ctx = mounts::ctx(state).await;
        mounts::check_values(&new_id, &m, &config, &ctx, mounts::Moment::Store).map_err(|r| {
            ApiRefusal {
                status: StatusCode::BAD_REQUEST,
                code: r.code,
                message: format!(
                    "'{id}' cannot be copied as it stands — {}. Clear or re-point the field on \
                     '{id}' and duplicate again.",
                    r.message
                ),
            }
        })?;
    }

    store::insert_agent(
        &state.db,
        &new_id,
        &m.to_json(),
        store::AGENT_SOURCE_AUTHORED,
    )
    .await
    .map_err(|e| e.to_string())?;
    store::set_agent_config(&state.db, &new_id, &Value::Object(config).to_string())
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "ok": true,
        "id": new_id,
        "name": m.name,
        "config_omitted": dropped,
        "message": format!("duplicated '{id}' as '{new_id}'"),
    }))
}

/// `agent_reset` — a built-in back to its shipped manifest, keeping the config.
async fn agent_reset(state: &SharedState, id: &str) -> Result<Value, String> {
    let Some(m) = seed::shipped(id) else {
        return Err(format!(
            "'{id}' is not a shipped agent, so there is no original to reset to"
        ));
    };
    let text = m.to_json();
    // Row and hash in one transaction (container-runtime §5.1). Without the
    // hash the reset row would carry the *new* manifest against the *old*
    // recorded one, read as "edited" on the next start, and stay pinned out of
    // the upgrade path for good. `put_builtin_manifest` also inserts when the
    // row is gone — deleted, then reset.
    let seeded_value = seed::seeded_with(state, id, &seed::manifest_hash(&text)).await;
    // A reset is deliberate and its config is kept, but a value the shipped
    // schema no longer declares would make the very next Start fail on it.
    let keep = store::AgentConfigField::of(&m);
    let dropped = store::put_builtin_manifest(
        &state.db,
        id,
        &text,
        agents::SEEDED_KEY,
        &seeded_value,
        Some(&keep),
    )
    .await
    .map_err(|e| e.to_string())?;
    // A different manifest can name a different model-alias field (§3.1).
    token::resync(state, id).await?;
    agents::service::resync(state, id).await?;
    Ok(json!({
        "ok": true,
        "id": id,
        "dropped_config": dropped.clone(),
        "message": format!(
            "'{id}' restored to the shipped manifest; its config was kept{}",
            if dropped.is_empty() {
                String::new()
            } else {
                format!(
                    " except {}, which the shipped schema no longer declares",
                    dropped.join(", ")
                )
            }
        ),
    }))
}

// ---------------------------------------------------------------------------
// Runs (§2.4, §5)
// ---------------------------------------------------------------------------

/// A finished run's own config, under what the form is sending now (§5.7).
///
/// `base_job`'s `input.effective` is the merged, non-secret config that run
/// actually used; a key the caller is resending wins over it, because an
/// explicit value is still an explicit value. A row from before `effective`
/// existed has none and this is the identity function — such an apply behaves
/// exactly as it did, against the stored config.
///
/// Silent about a `base_job` it cannot read: the worker refuses a base run that
/// is not this agent's, with the sentence that says so, and a second refusal
/// here would only get there first with less to say.
async fn with_base_config(
    state: &SharedState,
    base_job: Option<i64>,
    id: &str,
    sent: Map<String, Value>,
) -> Map<String, Value> {
    let Some(job_id) = base_job else {
        return sent;
    };
    let Ok(Some(row)) = store::get_job(&state.db, job_id).await else {
        return sent;
    };
    if row.kind != agents::JOB_KIND || row.key.as_deref() != Some(&agents::job_key(id)) {
        return sent;
    }
    let Ok(input) = serde_json::from_str::<batch::Input>(&row.input) else {
        return sent;
    };
    let Some(mut merged) = input.effective else {
        return sent;
    };
    for (k, v) in sent {
        merged.insert(k, v);
    }
    merged
}

/// `agent_run` — start one phase of a batch run as a job.
///
/// Thin on purpose: everything a run decides is in the executor
/// ([`crate::agents::batch`]), which is also where it is *re*-decided if the
/// process restarts mid-run. What happens here is argument shaping and the one
/// thing an op is better placed to answer than a job — that a run is already in
/// flight, which is reported as the running job rather than as an error (§2.4,
/// "one live run per agent").
pub(crate) async fn agent_run(state: &SharedState, args: &Args) -> Result<Value, String> {
    let id = need_id(args)?;
    let phase = match arg_str(args, "phase") {
        Some(p) => batch::Phase::parse(p)
            .ok_or_else(|| format!("unknown phase '{p}' (expected {})", batch::Phase::names()))?,
        None => return Err(format!("pass phase: {}", batch::Phase::names())),
    };
    let rows: Vec<batch::Row> = match args.get("rows") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| format!("rows are not review rows from a finished run: {e}"))?,
    };
    let base_job = match args.get("base_job") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_i64()
                .ok_or("base_job must be the job id of a finished run")?,
        ),
    };
    // Read before starting, so a misspelled id is a 400 rather than a job row
    // that exists only to fail.
    let sent = values_arg(args)?;
    let mut agent = load_agent(state, &id).await?;
    if agent.manifest.kind() == "chat" {
        return Err(format!(
            "'{id}' is a chat agent; it has no runs. Use agent_open_chat."
        ));
    }
    let stored = agent.config_values();
    // An apply reuses the run's config (mounts §5.7). The reviewer is looking
    // at rows a particular run produced, and the Apply button sends `base_job`
    // and no values at all — so the config that run recorded goes **under**
    // whatever the form is sending now, and the apply writes where the run
    // read. A rerun is deliberately not included: it re-classifies against the
    // agent as it is now, which is what the Run tab's form is for.
    let mut values = match phase {
        batch::Phase::Apply => with_base_config(state, base_job, &id, sent.clone()).await,
        _ => sent.clone(),
    };
    // Applied here too, not only in the worker: a form that is missing a
    // required field is a 400 at the click rather than a job row that exists
    // only to fail, which is the same bargain the id check above strikes.
    agent.override_config(&values)?;
    // A per-run mount override rides on the same form (mounts §5.7) and is
    // never written, so the store-time rules run against the patch here
    // instead: it is about to be *used*, and a folder nobody may bind is a 400
    // at the click rather than a job row that exists only to fail. The stored
    // values are not re-checked here — the start path re-checks every bound
    // mount at `Moment::Use` immediately before `podman run`, which is where a
    // path that vanished since it was saved is caught.
    if agent
        .manifest
        .mount_fields()
        .any(|f| sent.contains_key(&f.name))
    {
        let ctx = mounts::ctx(state).await;
        match mounts::check_values(&id, &agent.manifest, &sent, &ctx, mounts::Moment::Store) {
            // The canonical path travels, exactly as `agent_config_set` writes
            // it: the job row, the argv and the run log then all say the same
            // string, and an apply that inherits this run's config inherits the
            // resolved folder rather than a link that may have moved.
            Ok(bound) => {
                for b in bound {
                    values.insert(
                        b.field.name.clone(),
                        Value::String(b.host.display().to_string()),
                    );
                }
            }
            Err(r) => return Err(format!("{} ({})", r.message, r.code)),
        }
    }
    // A phase the manifest's run kind does not have is refused naming both
    // (§4.3): a container declares its phases, and a batch manifest's are the
    // pipeline stages it actually declares.
    let phases = agent.manifest.phases();
    if !phases.iter().any(|p| p == phase.as_str()) {
        return Err(format!(
            "'{id}' is a {} agent whose phases are {}; it has no '{}' phase",
            agent.manifest.kind(),
            phases.join(", "),
            phase.as_str()
        ));
    }
    // The Start gate (§4.3): `requires_ok && warnings.none(blocks_start)`. The
    // op enforces it too, not just the button — the reason is named here, where
    // a caller that is not the dashboard can read it.
    let podman = match agent.manifest.kind() {
        "container" => agents::container::podman_available(state).await,
        _ => Ok(()),
    };
    if let Some(w) = warnings(state, &agent, &podman)
        .await
        .into_iter()
        .find(|w| w.blocks_start)
    {
        return Err(format!("{} ({})", w.message, w.code));
    }

    // What this run is about to run with, recorded on the row before it starts
    // (mounts §5.7) — non-secret, because a job row is read back by the API and
    // a credential has no business surviving a run in the clear.
    let fields = agent.manifest.fields().unwrap_or_default();
    let effective =
        manifest::without_secrets(&fields, &manifest::merge_values(&fields, &stored, &values));
    let spawned = batch::start(
        state,
        batch::Input {
            agent_id: id.clone(),
            phase,
            rows,
            base_job,
            ledger: false,
            values,
            effective: Some(effective),
        },
    )
    .await?;
    let already = matches!(spawned, crate::jobs::Spawn::AlreadyRunning(_));
    Ok(json!({
        "ok": true,
        "id": id,
        "job_id": spawned.id(),
        "phase": phase.as_str(),
        "already_running": already,
        "message": if already {
            format!("a run of '{id}' is already in flight; showing that one")
        } else {
            format!("started the {} run of '{id}'", phase.as_str())
        },
    }))
}

/// `agent_run_cancel` — ask this agent's run to stop. Cooperative: the executor
/// stops between items and keeps the rows it has (§4.1).
/// `agent_token_get` / `agent_token_rotate` (§8).
///
/// A read of a column that already holds the value, not a reveal-once: lmgw has
/// to hand the token to a container on the *second* run too, and to a process
/// it did not start at all. Both ops are deliberately **not** exposed as
/// `lmgw__*` self-admin tools — defence in depth, not a boundary, since they
/// sit on the unauthenticated `/api` plane either way (§8).
async fn agent_token(state: &SharedState, id: &str, rotate: bool) -> Result<Value, String> {
    let agent = load_agent(state, id).await?;
    let (plaintext, scope_warning) = if rotate {
        token::rotate(state, &agent).await?
    } else {
        (token::ensure(state, &agent).await?, None)
    };
    // §12's open question, answered: **rotation stops the service container.**
    // There is one `(key_hash, key_plain)` pair per agent and rotation replaces
    // both in one write, so a running service is holding a token that stopped
    // working a moment ago and can now only fail. Leaving it up would trade a
    // visible restart for an invisible 401 on its next gateway call. The next
    // request to the agent's origin starts it again with the new token.
    let service_stopped = if rotate {
        agents::service::stop(state, id, "the agent's token was rotated").await
    } else {
        None
    };
    let service_note = service_stopped.as_ref().map(|s| s.describe());
    let (mode, patterns) = token::derive_scope(&agent);
    Ok(json!({
        "ok": true,
        "id": id,
        "name": token::key_name(id),
        "token": plaintext,
        "scope_mode": mode.as_str(),
        "scope_patterns": patterns,
        "scope_note": token::scope_note(mode, &patterns),
        "service_stopped": service_stopped.as_ref().and_then(|s| s.container.clone()),
        // A rotation that could not re-derive the scope still rotated: the
        // token above is the live one, and this says what did not happen.
        "warning": scope_warning,
        "message": match (&service_note, rotate) {
            (Some(note), _) => format!(
                "rotated the token for '{id}' — the previous one stopped working now, and its \
                 app was holding it: {note}"
            ),
            (None, true) => {
                format!("rotated the token for '{id}' — the previous one stopped working now")
            }
            (None, false) => format!("the token for '{id}'"),
        },
    }))
}

/// `agent_service_log { id, lines? }` (§3.3, §7) — the App tab's log block, as
/// long as the reader asks for.
///
/// The detail payload carries a fixed [`LOG_EXCERPT_LINES`](agents::service::LOG_EXCERPT_LINES)
/// excerpt, which is the right default for a page that draws itself. It is the
/// wrong *only* option: for a detached service container the tail is the single
/// account anyone has of it — it writes no run ledger, no job row and no
/// `result` — so "the last twelve lines" was a hidden cap on the only
/// diagnostic there is. `lines` is the visible field behind it, and `0` means
/// the whole log.
///
/// The count is echoed back with the text so the caller can say what it is
/// showing rather than implying it is everything.
async fn agent_service_log(state: &SharedState, args: &Args) -> Result<Value, String> {
    let id = need_id(args)?;
    let agent = load_agent(state, &id).await?;
    if agents::service::service_of(&agent).is_none() {
        return Err(format!(
            "'{id}' declares no run.service, so it has no app container to read a log from"
        ));
    }
    let lines = match args.get("lines") {
        None | Some(Value::Null) => agents::service::LOG_EXCERPT_LINES,
        Some(v) => match v.as_i64() {
            Some(n) if n >= 0 => n as usize,
            _ => return Err("lines must be a whole number; 0 is the whole log".to_string()),
        },
    };
    let Some(live) = state.agent_services.get(&id) else {
        return Ok(json!({
            "ok": true,
            "id": id,
            "lines": lines,
            "running": false,
            "log": "",
            "message": format!("'{id}' has no app container running, so there is no log to read"),
        }));
    };
    let log = agents::service::log_tail(state, &id, &live.container, lines).await;
    Ok(json!({
        "ok": true,
        "id": id,
        "lines": lines,
        "running": true,
        "container": live.container,
        "log": log,
        "message": match lines {
            0 => format!("the whole log of '{}'", live.container),
            n => format!("the last {n} lines of '{}'", live.container),
        },
    }))
}

/// `agent_service_start` / `agent_service_stop` (§3.3, §8) — the App tab's two
/// buttons.
///
/// Start is the same on-demand path a proxied request takes, pressed by hand:
/// one start per agent however many callers ask, the manifest's health probe,
/// and the manifest's `start_timeout_seconds` bounding the wait. Stop is the
/// stop ladder. Stopping something that is not running is success — "make sure
/// this is not running" is what the button means.
async fn agent_service(state: &SharedState, id: &str, start: bool) -> Result<Value, String> {
    let agent = load_agent(state, id).await?;
    if agents::service::service_of(&agent).is_none() {
        return Err(format!(
            "'{id}' declares no run.service, so it has no app to start or stop"
        ));
    }
    if !start {
        let stopped = agents::service::stop(state, id, "asked to stop from the App tab").await;
        return Ok(json!({
            "ok": true,
            "id": id,
            "running": false,
            "stopped": stopped.as_ref().and_then(|s| s.container.clone()),
            "cancelled_start": stopped.as_ref().is_some_and(|s| s.cancelled_start),
            "message": match &stopped {
                Some(s) => s.describe(),
                None => format!("'{id}' had no app container running"),
            },
        }));
    }
    // §3.4: with a dev_url set there is nothing to start — the app is served
    // from the owner's own server and a container started here would be one
    // nothing routes to. Refused with the reason rather than started silently.
    if let Some(url) = agents::service::dev_url_of(&agent) {
        return Err(format!(
            "'{id}' has a dev_url set ({url}), so its app is served from there and no container \
             is started for it. Clear the dev_url to go back to the image."
        ));
    }
    match agents::service::ensure(state, &agent).await {
        Ok(live) => Ok(json!({
            "ok": true,
            "id": id,
            "running": true,
            "container": live.container,
            "host_port": live.host_port,
            // Where it is now answering (§7): the published host port is
            // lmgw's business, the origin is the reader's.
            "origin": agents::service::agent_origin(&state.snapshot().settings, id),
            "message": format!(
                "'{id}' is serving on 127.0.0.1:{} as {}",
                live.host_port, live.container
            ),
        })),
        Err(e) => Err(if e.log.trim().is_empty() {
            e.reason.clone()
        } else {
            format!("{}\n{}", e.reason, e.log.trim())
        }),
    }
}

async fn agent_run_cancel(state: &SharedState, id: &str) -> Result<Value, String> {
    let message = batch::cancel_for_agent(state, id).await?;
    Ok(json!({ "ok": true, "id": id, "message": message }))
}

// ---------------------------------------------------------------------------
// The `lmgw__agent*` self-admin surface (§5)
// ---------------------------------------------------------------------------

/// `lmgw__agents` — the catalog, trimmed to what a model needs to decide what
/// to call next.
pub(crate) async fn tool_list(state: &SharedState) -> Result<Value, String> {
    let cards = list_inner(state).await?;
    let agents: Vec<Value> = cards
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "name": c.name,
                "description": c.description,
                "kind": c.kind,
                "model": c.effective_model,
                "enabled": c.enabled,
                "source": c.source,
                "tools": c.labels,
                "requires_ok": c.requires_ok,
                "error": c.error,
            })
        })
        .collect();
    Ok(json!({
        "agents": agents,
        "count": agents.len(),
        "next_step": "lmgw__agent_get id=<id> for the manifest and the config form",
    }))
}

/// `lmgw__agent_get` — the manifest and the masked config for one agent.
pub(crate) async fn tool_get(state: &SharedState, id: &str) -> Result<Value, String> {
    // The self-admin plane is the owner's own (§3.7), so it reads the owner's
    // document.
    let d = detail_inner(state, id, View::Admin).await?;
    serde_json::to_value(d).map_err(|e| e.to_string())
}
