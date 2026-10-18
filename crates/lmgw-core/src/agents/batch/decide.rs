//! The classify step through a question set (labs-store design §4).
//!
//! An output with `decide` whose config field names a Decision lab question
//! set is classified by the set's question whose id is the output's `field`,
//! asked of the rendered `user` template (the row's `prompt`) through the
//! real `/v1/systemone` handler in process
//! ([`crate::proxy::systemone::dispatch`]); the `system` template is not
//! sent. The question must be a `choice`: its options are the categories,
//! with the output's fallback appended once and last, so the review select
//! keeps its shape, and `config.categories` (the `enum_from` field) is not
//! read. The set's own `default_model` answers, so privacy is the set's
//! model: a CPU decision row without a fallback keeps the item on the
//! machine, and a configured fallback always applies.
//!
//! The call runs under the run's own context: the agents' identity
//! (`internal:agents`, as the run's model turns are filed) and the run's job
//! id, so the request log's `systemone` row folds into the run's meter as a
//! stamped call does, priced from lmgw's own rows.
//!
//! **Attention:** the output is the winner; the row needs attention when the
//! winner is the fallback (as with the model call), when its confidence is
//! below the set's *measured* threshold for the question (an unmeasured one
//! is shown, not applied: Decision 10), or when the call failed (the
//! fallback then). A failure is the route's answer verbatim, llama.cpp's 500
//! for a state over the ubatch included: nothing is cut to fit. `raw` is the
//! answer's text, so the details show the probabilities. Each row the set
//! classifies carries the set's stamp ([`AgentRowSet`]), which "Save as
//! labelled set" saves the rows into.
//!
//! **Which failures stop what.** An error the model itself answered, relayed
//! as it came (no `lmgw_code` in the body: a state over the ubatch, a
//! cloud provider's 5xx), is that row's own failure and never counts toward
//! the early abort; lmgw's own 5xx (transport, connect, timeout, a container
//! whose image lacks the route) still does. An answer that would be the same
//! for every row ([`SameForEveryItem`]: the hold, a budget, a scope, an
//! alias gone, a container that does not start, the provider refusing
//! lmgw's key) is no row's answer: the stage stops at once and the run fails
//! with it, keeping the rows answered so far as a cancel keeps them.

use axum::body::Bytes;
use lmgw_api_types::decision_lab::{
    DecisionForm, QuestionDraft, QuestionKind, QuestionSet, StateMode, Variant,
};
use lmgw_api_types::lab_store::{LabKind, LabPresetKind};
use lmgw_api_types::systemone::SystemOneResponse;
use lmgw_api_types::{AgentDecide, AgentRowSet};
use serde::Deserialize;
use serde_json::{json, Value};

use super::super::manifest::{ItemOutput, Manifest, RunSpec};
use super::super::template;
use super::{fallback_output, Row};
use crate::agent::Cancel;
use crate::proxy::systemone::SameForEveryItem;
use crate::proxy::RequestCtx;
use crate::state::SharedState;
use crate::store;
use crate::telemetry::AGENT_PROTO;

/// The set a run classifies through, read once as the run starts.
#[derive(Debug, Clone)]
pub struct Decider {
    /// The set as read: stamped on each row it classifies.
    pub stamp: AgentRowSet,
    /// The set's question of the output's field: a `choice`.
    pub question: QuestionDraft,
    /// The set's default model, which answers.
    pub model: String,
    /// The threshold applied: the question's when it is measured, else 0,
    /// "never below it" (Decision 10: the set's *measured* threshold).
    pub threshold: f64,
    /// The threshold the set keeps for the question, measured or not (0
    /// when it has none), for the Run tab to show.
    pub kept: f64,
    /// Whether the kept threshold is measured, so applied.
    pub measured: bool,
}

/// The set `set` read: its stamp (id, name, `updated_at`) and its body.
pub async fn read_set(state: &SharedState, set: i64) -> Result<(AgentRowSet, QuestionSet), String> {
    let row = store::lab_preset(&state.db, Some(LabKind::Decision), set)
        .await
        .map_err(|e| format!("reading question set {set}: {e}"))?
        .ok_or_else(|| {
            format!(
                "question set {set} is not there (deleted?): pick a set in Config, or empty the \
                 field to classify with the model"
            )
        })?;
    if row.kind != LabPresetKind::QuestionSet.as_str() {
        return Err(format!(
            "preset {set} is a {}, not a question set",
            row.kind
        ));
    }
    let body = crate::labs::presets::question_set(&row)?;
    let stamp = AgentRowSet {
        id: row.id,
        name: row.name,
        updated_at: row.updated_at,
        digest: String::new(),
    };
    Ok((stamp, body))
}

/// A digest of a question's definition (id, instructions, kind with its
/// options, descriptions and levels), for [`AgentRowSet::digest`].
pub fn question_digest(q: &lmgw_api_types::decision_lab::QuestionDraft) -> String {
    use sha2::{Digest, Sha256};
    let json = serde_json::to_string(q).unwrap_or_default();
    Sha256::digest(json.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The question of `set` (named `name`) that answers the output field
/// `field`: it must be there and be a `choice`.
pub fn question_of<'s>(
    name: &str,
    set: &'s QuestionSet,
    field: &str,
) -> Result<&'s QuestionDraft, String> {
    let q = set.question(field).ok_or_else(|| {
        format!(
            "the question set '{name}' has no question '{field}': the output field is \
             '{field}', so the set needs a choice question of that id"
        )
    })?;
    match &q.kind {
        QuestionKind::Choice { .. } => Ok(q),
        other => Err(format!(
            "the question set '{name}' asks '{field}' as a {} question; an agent classifies \
             through a choice question, whose options are its categories",
            other.type_name()
        )),
    }
}

/// The values a decided output takes: the question's options in their
/// order, the fallback appended once and last (as `enum_values` does).
pub fn options(out: &ItemOutput, q: &QuestionDraft) -> Vec<String> {
    let mut values: Vec<String> = match &q.kind {
        QuestionKind::Choice { options } => options.iter().map(|o| o.name.clone()).collect(),
        _ => Vec::new(),
    };
    if let Some(Value::String(fb)) = &out.fallback {
        values.retain(|v| v != fb);
        values.push(fb.clone());
    }
    values
}

/// The decider for set `set` and the output `out`: the set, its question,
/// its model and the question's threshold. The model is not checked here.
async fn decider(state: &SharedState, out: &ItemOutput, set: i64) -> Result<Decider, String> {
    let (stamp, body) = read_set(state, set).await?;
    let field = out.field.as_deref().unwrap_or_default();
    let question = question_of(&stamp.name, &body, field)?.clone();
    let stamp = AgentRowSet {
        digest: question_digest(&question),
        ..stamp
    };
    let model = body.default_model.trim().to_string();
    if model.is_empty() {
        return Err(format!(
            "the question set '{}' names no default model: pick one for it in the Decision lab",
            stamp.name
        ));
    }
    let (kept, measured) = body
        .thresholds
        .get(field)
        .map_or((0.0, false), |t| (t.value, t.measured.is_some()));
    Ok(Decider {
        stamp,
        threshold: if measured { kept } else { 0.0 },
        kept,
        measured,
        question,
        model,
    })
}

/// The decider a classify run of `out` uses under `config`: `None` when the
/// config names no set (the model call classifies). An unusable set fails
/// the run before any tool call, saying why: gone, no such question, not a
/// choice, no model, or a model that answers no `/v1/systemone`.
pub async fn for_run(
    state: &SharedState,
    out: &ItemOutput,
    config: &Value,
) -> Result<Option<Decider>, String> {
    let Some(set) = out.decide_set(config) else {
        return Ok(None);
    };
    let d = decider(state, out, set).await?;
    if !crate::modelinfo::decision::answers(state, &d.model).await {
        return Err(format!(
            "the question set '{}' asks '{}', which answers no /v1/systemone: pick a model of \
             the Decision lab for the set",
            d.stamp.name, d.model
        ));
    }
    // The gate's per-request half the first row's dispatch would hit (the
    // GPU hold's swap and the route check; nothing is started or claimed):
    // under the hold a row without a usable fallback is refused here, before
    // any tool call, as the model call's path refuses. A held row with a
    // fallback passes, since a configured fallback always applies.
    crate::gate::resolve(state, &d.model, crate::gate::RouteCheck::Decision)
        .await
        .map_err(|f| format!("model '{}': {}", d.model, f.error))?;
    Ok(Some(d))
}

/// How a classify through the set went.
pub(super) struct Decided {
    pub row: Row,
    /// The HTTP status of a call lmgw itself failed (its error carries an
    /// `lmgw_code`), which the early abort counts at 5xx. `None` for an
    /// error the model answered, relayed: that row's own.
    pub failed: Option<u16>,
    /// The route answered with a 2xx: the model is up.
    pub answered: bool,
    /// lmgw refused the call with an answer that would be the same for
    /// every row ([`SameForEveryItem`]): what it said. The row is left as
    /// listed, and the stage stops.
    pub stop: Option<String>,
}

/// lmgw's own error envelope (`llama_error`, `{"error": {"message",
/// "lmgw_code"}}`): its code and message. An error the model answered,
/// relayed as it came, has no `lmgw_code` and reads `None`.
fn lmgw_error(text: &str) -> Option<(String, String)> {
    #[derive(Deserialize)]
    struct Envelope {
        error: Body,
    }
    #[derive(Deserialize)]
    struct Body {
        lmgw_code: Option<String>,
        message: Option<String>,
    }
    let e = serde_json::from_str::<Envelope>(text).ok()?.error;
    Some((e.lmgw_code?, e.message.unwrap_or_default()))
}

/// The config field the model alias `alias` is, when it is exactly one
/// (`{{config.<field>}}`).
fn model_field(alias: &str) -> Option<String> {
    template::whole_value_path(alias.trim())?
        .strip_prefix("config.")
        .filter(|f| !f.is_empty() && !f.contains('.'))
        .map(str::to_string)
}

/// The run's own context for its decision calls: the agents' identity and
/// the run's job id, so the row is the agent's and folds into its meter.
fn run_ctx(run: i64) -> RequestCtx {
    RequestCtx {
        client_key: crate::telemetry::internal_identity(AGENT_PROTO).map(str::to_string),
        run: Some(run),
        ..Default::default()
    }
}

/// Ask `d`'s question of `prompt` for `row`, raced against `cancel`. An
/// abandoned call leaves the row as listed, with its prompt and the set's
/// stamp, and no failure.
pub(super) async fn classify(
    state: &SharedState,
    run: i64,
    cancel: &Cancel,
    d: &Decider,
    out: &ItemOutput,
    mut row: Row,
    prompt: String,
) -> Decided {
    row.prompt = Some(prompt.clone());
    row.set = Some(d.stamp.clone());
    let form = DecisionForm {
        state_mode: StateMode::Text,
        state: prompt,
        images: Vec::new(),
        questions: vec![d.question.clone()],
    };
    let failed = |mut row: Row, error: String, status: Option<u16>| {
        row.error = Some(error);
        row.output = fallback_output(out);
        row.attention = true;
        Decided {
            row,
            failed: status,
            answered: false,
            stop: None,
        }
    };
    let body = match form.body(&d.model, Variant::AsWritten, &[]) {
        Ok(b) => b,
        Err(e) => return failed(row, e, None),
    };
    let sent = crate::proxy::systemone::dispatch(state.clone(), run_ctx(run), Bytes::from(body));
    let Some(resp) = cancel.guard(sent).await else {
        return Decided {
            row,
            failed: None,
            answered: false,
            stop: None,
        };
    };
    let status = resp.status();
    let (parts, answer) = resp.into_parts();
    let same_for_all = parts.extensions.get::<SameForEveryItem>().is_some();
    // The handler answers with its body read whole; there is nothing to cap.
    let bytes = match axum::body::to_bytes(answer, usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            return failed(
                row,
                format!("reading {}'s answer: {e}", d.model),
                Some(status.as_u16()),
            )
        }
    };
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    if same_for_all {
        // Not this row's answer: the next row would get the same. Left as
        // listed, as a cancel leaves a row it never reached.
        let said = match lmgw_error(&text) {
            Some((code, message)) => format!("({} {code}): {message}", status.as_u16()),
            None => format!("({}): {text}", status.as_u16()),
        };
        row.prompt = None;
        row.set = None;
        return Decided {
            row,
            failed: None,
            answered: false,
            stop: Some(format!("lmgw could not ask {} {said}", d.model)),
        };
    }
    if !status.is_success() {
        // lmgw's own failure, or a relayed 502/503/504 (a loading server, a
        // proxy in front of a dead one, a cloud outage), may be the model
        // being down, which the early abort is for; any other answer of the
        // model, relayed (a 4xx, a 500 over the ubatch), is this row's.
        let own = lmgw_error(&text).is_some() || matches!(status.as_u16(), 502..=504);
        return failed(
            row,
            format!("{} answered {}: {text}", d.model, status.as_u16()),
            own.then_some(status.as_u16()),
        );
    }
    let field = out.field.clone().unwrap_or_default();
    let read = serde_json::from_str::<SystemOneResponse>(&text)
        .map_err(|e| format!("{}'s answer does not read as System One's ({e})", d.model))
        .and_then(|r| {
            let a = r.answers.get(&d.question.id).ok_or_else(|| {
                format!("{}'s answer has no answer to '{}'", d.model, d.question.id)
            })?;
            crate::labs::metrics::predict(&d.question, a).ok_or_else(|| {
                format!(
                    "{}'s answer to '{}' is no choice of its options",
                    d.model, d.question.id
                )
            })
        });
    row.raw = Some(text);
    match read {
        Ok(p) => {
            // The fallback answered needs a look as it does on the model
            // call's path, whatever its confidence.
            let fallback = matches!(&out.fallback, Some(Value::String(fb)) if *fb == p.class);
            row.attention = fallback || p.confidence < d.threshold;
            row.output = json!({ field: p.class });
            Decided {
                row,
                failed: None,
                answered: true,
                stop: None,
            }
        }
        // A 2xx that is no answer: the model is up, this row is not.
        Err(why) => Decided {
            answered: true,
            ..failed(row, why, None)
        },
    }
}

/// The Run tab's view of `m`'s `decide` under `config`, and with a usable
/// set the review's values; `None` when the output cannot decide.
pub async fn review(
    state: &SharedState,
    m: &Manifest,
    config: &Value,
) -> Option<(AgentDecide, Option<Vec<String>>)> {
    let RunSpec::Batch { item, .. } = &m.run else {
        return None;
    };
    let out = item.output.as_ref()?;
    let field = out.decide_field()?.to_string();
    let mut view = AgentDecide {
        field,
        question: out.field.clone().unwrap_or_default(),
        enum_from: out
            .enum_from
            .as_deref()
            .and_then(|p| p.strip_prefix("config."))
            .map(str::to_string),
        set: out.decide_set(config),
        model_field: model_field(&m.model.alias),
        ..Default::default()
    };
    let Some(set) = view.set else {
        return Some((view, None));
    };
    match decider(state, out, set).await {
        Ok(d) => {
            let values = options(out, &d.question);
            view.set_name = Some(d.stamp.name);
            view.threshold = Some(d.kept);
            view.measured = d.measured;
            Some((view, Some(values)))
        }
        Err(e) => {
            view.problem = Some(e);
            Some((view, None))
        }
    }
}
