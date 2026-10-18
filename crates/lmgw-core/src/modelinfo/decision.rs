//! The decision kind's half of `modelinfo` (decision-models design §3.4,
//! §3.7): the aux plan's branch for a decision header, and the load test's
//! probe, one `choice` question built and read with
//! `lmgw-api-types::systemone`.
//!
//! The plan reads the header and nothing else. Every value is the header's
//! (`context_length`) or a stated choice, and its rationale says which. The
//! per-type facts from llama.cpp's source (which types are whole-prompt,
//! joint, image-capable) are not coded here, because a table of llama.cpp's
//! types goes stale with the next type llama.cpp adds (§1.5, §3.4).

use lmgw_api_types::systemone::{
    Answer, ChoiceQuestion, Question, SystemOneRequest, SystemOneResponse,
};
use lmgw_api_types::{DecideAnswer, OptionProbability};
use serde_json::{json, Value};

use crate::config::{Settings, Snapshot};
use crate::gguf::{self, ModelSummary};
use crate::runtime::aux::{LLAMA_DEFAULT_BATCH, LLAMA_DEFAULT_UBATCH};
use crate::runtime::Class;
use crate::state::SharedState;
use crate::vram::compute::{self, Ask, Weights};
use crate::vram::LocalHold;

/// What a request must fit, per header shape (labs-store design §2.2).
pub(crate) mod limits;

/// Whether the model `name` answers `/v1/systemone`: an exposed entry of
/// that name lists the route among its endpoints (a local `decide/*` row,
/// or a cloud alias or catalog entry of task `decision`). The Decision lab's
/// set runs and a batch agent classifying through a set ask it before they
/// start, without reading the model's facts.
pub(crate) async fn answers(state: &SharedState, name: &str) -> bool {
    use lmgw_api_types::decision_lab::form::ENDPOINT;
    crate::capabilities::exposed::exposed_entries(state)
        .await
        .into_iter()
        .any(|e| {
            e.name == name
                && e.capabilities
                    .as_ref()
                    .is_some_and(|c| c.endpoints.iter().any(|p| p == ENDPOINT))
        })
}

/// The three header shapes of §3.4's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    /// `attention.causal = false` (Laya, Julia-1): llama.cpp evaluates the
    /// whole prompt in one ubatch.
    NonCausal,
    /// Causal, with a decision head (`cls.output*` or `decision.*` tensors;
    /// Kev, Clef): the server runs in embedding mode.
    CausalHead,
    /// Causal, reading its answer from label logits (no such head; lev,
    /// OpenJev): the server keeps a normal context.
    LabelLogits,
}

impl Shape {
    /// The header's shape. The head test is a header signal, not llama.cpp's
    /// own switch (which is the decision type): a type it misreads costs a
    /// visible 400 or a larger compute buffer (§3.4).
    pub(crate) fn of(s: &ModelSummary) -> Self {
        if s.attention_causal == Some(false) {
            Self::NonCausal
        } else if s.has_classifier_head || s.has_decision_tensors {
            Self::CausalHead
        } else {
            Self::LabelLogits
        }
    }

    /// The plan's warning for the sibling projector `cand` when it does not
    /// belong on the row: never on a non-causal type (§3.4; §3.5 flags one).
    /// `None` proposes it.
    pub(crate) fn unproposed_projector(self, cand: &str) -> Option<String> {
        (self == Self::NonCausal).then(|| {
            format!(
                "a projector sits beside it ({cand}), but a non-causal decision model takes \
                 none (the row's checks flag one), so it is not proposed"
            )
        })
    }
}

/// Whether a configured decision row's `current` value of `field` is tuning
/// the plan's own rationale asks for, taken past `planned` rather than
/// against it: on a causal type, a `ctx_size` at or below the trained context
/// (its KV cost is the reason to lower it), and a `ubatch_size` or
/// `batch_size` at or above lmgw's 2048 (a whole-prompt type such as Clef
/// needs it raised). A re-plan then counts the tuned row as in sync and does
/// not offer to undo it. A non-causal type's values are compared as planned:
/// its ubatch is its whole prompt.
pub(super) fn within_tuning(shape: Shape, field: &str, current: &Value, planned: &Value) -> bool {
    if shape == Shape::NonCausal {
        return false;
    }
    let (Some(current), Some(planned)) = (current.as_i64(), planned.as_i64()) else {
        return false;
    };
    match field {
        "ctx_size" => current <= planned,
        "ubatch_size" | "batch_size" => current >= planned,
        _ => false,
    }
}

/// lmgw's ubatch for a causal type with a decision head (§3.4): llama.cpp's
/// own default `n_batch` for a normal context, where embedding mode would
/// leave the effective 512.
const CAUSAL_HEAD_UBATCH: u64 = LLAMA_DEFAULT_BATCH.unsigned_abs();

/// The compute buffer a GPU row of this header reserves at `ctx` with
/// `ubatch` = batch ([`crate::vram::compute`], what the VRAM ledger charges
/// it), as a clause.
fn gpu_compute(s: &ModelSummary, ctx: Option<u64>, ubatch: u64) -> String {
    let Some(ctx) = ctx else {
        return "not estimated here (the header states no context length)".into();
    };
    let ask = Ask {
        ctx,
        ubatch,
        batch: ubatch,
        flash_attn_off: false,
        weights: Weights::Device,
    };
    match compute::bytes(s, &ask) {
        Ok(b) => format!("about {} for this model", crate::hf::fmt_bytes(b)),
        Err(why) => format!("not estimated here ({why})"),
    }
}

/// The decision arm of `aux_plan`: `put` records a planned field with its
/// reason, `warnings` collects what the caller must know before creating the
/// row. `rel` is the file's path, for the warnings; `settings` gives the aux
/// class's request timeout the CPU rationale names. The compute buffer a
/// planned ubatch costs on the GPU is estimated from the header
/// ([`crate::vram::compute`]), as the ledger charges it. The shape comes back
/// so the caller knows whether to propose a projector.
pub(super) fn plan(
    s: &ModelSummary,
    rel: &str,
    settings: &Settings,
    put: &mut dyn FnMut(&str, Value, String),
    warnings: &mut Vec<String>,
) -> Shape {
    let decision_type = s.decision_type.as_deref().unwrap_or_default();
    let shape = Shape::of(s);
    let how = match shape {
        Shape::NonCausal => {
            "a non-causal model (attention.causal = false): llama-server reads its embeddings, \
             in embedding mode, and evaluates each prompt (the state and one question) in one \
             ubatch"
                .to_string()
        }
        Shape::CausalHead => "a causal model with a decision head (cls.output or decision.* \
                              tensors): llama-server reads its embeddings, in embedding mode"
            .to_string(),
        Shape::LabelLogits => format!(
            "a causal model without a decision head: it answers from label logits, so \
             llama-server keeps a normal context and its own batch defaults (n_batch \
             {LLAMA_DEFAULT_BATCH}, n_ubatch {LLAMA_DEFAULT_UBATCH}), and no ubatch_size or batch_size is \
             planned"
        ),
    };
    put(
        "kind",
        json!("decision"),
        format!(
            "the header declares decision.type '{decision_type}': a decision model, which \
             llama-server serves on /v1/systemone; it renders no kind flag and takes no \
             pooling. It is {how}"
        ),
    );
    if !s.has_systemone_template {
        warnings.push(format!(
            "{rel} declares a decision type but carries no tokenizer.chat_template.systemone \
             — llama-server refuses to load a decision model without it"
        ));
    }

    // What the trained context decides: the ctx, and a non-causal type's
    // ubatch and batch. A header without one gets none of them, and says so.
    match (shape, s.context_length) {
        (Shape::NonCausal, Some(ctx)) => {
            put(
                "ctx_size",
                json!(ctx),
                format!(
                    "the model's trained context ({ctx}), its real ceiling: llama-server \
                     refuses a longer prompt with an error naming the batch size and does not \
                     truncate it"
                ),
            );
            put(
                "ubatch_size",
                json!(ctx),
                format!(
                    "= ctx_size ({ctx}, the header's context_length): a non-causal model \
                     evaluates its whole prompt in one ubatch, and embedding mode forces \
                     n_batch down to n_ubatch, whose default is 512, so without it every \
                     request over 512 tokens fails with a 500. The compute buffer grows with \
                     it, and on this CPU row it is RAM: Laya's container held 2.22 GB after a \
                     7,669-token prompt at 8192. On the GPU the VRAM ledger would charge it \
                     with the row: {}",
                    gpu_compute(s, Some(ctx), ctx)
                ),
            );
            put(
                "batch_size",
                json!(ctx),
                "= ubatch_size: llama.cpp lowers the ubatch to the batch, and embedding mode \
                 lowers the batch to the ubatch, so the two are raised together"
                    .into(),
            );
        }
        (Shape::NonCausal, None) => warnings.push(
            "the GGUF declares no context length, so ctx_size, ubatch_size and batch_size are \
             not planned. A non-causal decision model reads its whole prompt in one ubatch, \
             and at llama-server's default (512) every prompt over 512 tokens fails with a \
             500: set all three to the model card's context"
                .into(),
        ),
        (Shape::CausalHead | Shape::LabelLogits, Some(ctx)) => {
            put("ctx_size", json!(ctx), causal_ctx_rationale(s, ctx))
        }
        (Shape::CausalHead | Shape::LabelLogits, None) => warnings.push(
            "the GGUF declares no context length; leave ctx_size unset to take llama.cpp's \
             default"
                .into(),
        ),
    }

    // What the shape alone decides.
    match shape {
        Shape::NonCausal => put_cpu(s, settings, put),
        Shape::CausalHead => {
            put(
                "ubatch_size",
                json!(CAUSAL_HEAD_UBATCH),
                format!(
                    "{CAUSAL_HEAD_UBATCH}, lmgw's choice over the engine's effective 512: \
                     embedding mode forces n_batch down to n_ubatch (default 512), and the \
                     span from the first option marker to the end of the prompt must fit one \
                     batch (llama-server answers a 400 naming the size), which 512 is short \
                     for with a long option list (Kev-4B's 255 options took 1,780 tokens). The \
                     state before it is split across batches and shared across questions. It \
                     is llama.cpp's own n_batch for a normal context. The compute buffer grows \
                     with it (and with ctx_size), and the VRAM ledger charges it with the row: \
                     at the trained context, {}. Kev-4B (Q8_0) measured 2.1 GiB at ctx 32768 \
                     and 3.0 GiB at its trained 262144 (761 MiB at 512). A type that reads its \
                     whole prompt in one batch (Clef) needs ubatch_size raised to its longest \
                     prompt",
                    gpu_compute(s, s.context_length, CAUSAL_HEAD_UBATCH)
                ),
            );
            put(
                "batch_size",
                json!(CAUSAL_HEAD_UBATCH),
                "= ubatch_size: embedding mode lowers the batch to the ubatch, so the two are \
                 raised together"
                    .into(),
            );
        }
        Shape::LabelLogits => {}
    }
    shape
}

/// The ctx rationale of a causal decision model: its trained context and
/// what its KV cache costs there, as `gguf::kv_cache_bytes` reads it from
/// the header (an aux row renders no cache type, so f16).
fn causal_ctx_rationale(s: &ModelSummary, ctx: u64) -> String {
    let kv = match gguf::kv_cache_bytes(s, ctx, 16) {
        Some(b) => format!(
            "its f16 KV cache at that context is about {}",
            crate::hf::fmt_bytes(b)
        ),
        None => {
            "the header does not state its attention shape, so its KV cost is unknown".to_string()
        }
    };
    format!(
        "the model's trained context ({ctx}); {kv}, charged to the row's VRAM footprint. Lower \
         ctx_size to shrink it: a request needs room for its state and questions, not the \
         whole trained context"
    )
}

/// `backend: cpu` for a non-causal type (§3.4), with what a CPU row costs
/// in time and what bounds it.
fn put_cpu(s: &ModelSummary, settings: &Settings, put: &mut dyn FnMut(&str, Value, String)) {
    let size = match &s.size_label {
        Some(label) => format!("a {label}-parameter"),
        None => format!("a {}", crate::hf::fmt_bytes(s.file_size)),
    };
    let timeout = match settings.aux_router.request_timeout_seconds {
        0 => "the aux class has no request timeout (aux_router.request_timeout_seconds is 0)"
            .to_string(),
        t => format!(
            "the aux class's request timeout ({t} s, aux_router.request_timeout_seconds) \
             bounds the whole request"
        ),
    };
    let cpu = crate::host::cpu();
    let threads = match cpu.source {
        crate::host::CoreSource::Topology => format!("{} physical cores", cpu.physical_cores),
        crate::host::CoreSource::Logical => format!(
            "{} logical CPUs (its core topology could not be read)",
            cpu.physical_cores
        ),
    };
    put(
        "backend",
        json!("cpu"),
        format!(
            "the CPU, for {size} non-causal encoder: it takes no VRAM and keeps serving \
             under the GPU hold. Only Laya (421M parameters, Q8_0) was measured there: 33 ms \
             for one question on 16 threads, and 14.9 s for one question on a 7,669-token \
             state. The time grows with the model's size, with the state and with the number \
             of questions, each its own pass, and {timeout}; raise it for long states with \
             many questions, or clear backend to run on the GPU. threads is left unset, so \
             the row runs this machine's {threads}"
        ),
    );
}

// ---------------------------------------------------------------------------
// Probe::Decide (§3.7)
// ---------------------------------------------------------------------------

/// The probe's question id.
const QUESTION: &str = "topic";

/// The probe's options, in the order they are offered.
const OPTIONS: [&str; 3] = ["software", "cooking", "sports"];

/// How far the probe's probabilities may sum from 1.
const SUM_TOLERANCE: f64 = 1e-6;

/// The load test's question (§3.7): one `choice` with three options about a
/// fixed one-line state. Sent as this type, so its options keep their order.
pub(super) fn probe_request(model_id: &str) -> SystemOneRequest {
    SystemOneRequest {
        model: model_id.to_string(),
        state: json!(
            "lmgw is a self-hosted gateway that routes LLM API requests to local and cloud \
             models."
        ),
        questions: [(
            QUESTION,
            Question::Choice(ChoiceQuestion {
                instructions: json!("What is this text about?"),
                criteria: OPTIONS.iter().map(|o| (*o, None)).collect(),
            }),
        )]
        .into_iter()
        .collect(),
        images: None,
        files: None,
    }
}

/// The image row `model_id`'s container runs, as its descriptor renders it.
fn image_of(snap: &Snapshot, model_id: &str) -> String {
    crate::runtime::descriptor::model_runtime(snap, Class::Aux, model_id)
        .map(|r| r.image)
        .unwrap_or_default()
}

/// What the probe's HTTP answer said (§3.7).
pub(super) struct Read {
    pub ok: bool,
    /// Why not, when not `ok`.
    pub detail: String,
    /// What a pass read.
    pub decided: Option<Decided>,
    /// The body was read to its end.
    pub read_whole: bool,
}

/// Read the probe's answer `r` from row `model_id`'s container: a 200 is
/// [`judge`]d, a 404 is the image without the route (§3.6), anything else
/// fails with its status and body.
pub(super) async fn read_answer(snap: &Snapshot, model_id: &str, r: reqwest::Response) -> Read {
    let status = r.status();
    if status.is_success() {
        return match r.bytes().await {
            Ok(body) => match judge(&body) {
                Ok(d) => Read {
                    ok: true,
                    detail: String::new(),
                    decided: Some(d),
                    read_whole: true,
                },
                Err(why) => Read {
                    ok: false,
                    detail: why,
                    decided: None,
                    read_whole: true,
                },
            },
            Err(e) => Read {
                ok: false,
                detail: format!("HTTP 200 but the body could not be read: {e}"),
                decided: None,
                read_whole: false,
            },
        };
    }
    let body = r.text().await;
    let read_whole = body.is_ok();
    let body = body.unwrap_or_default();
    let detail = match status {
        reqwest::StatusCode::NOT_FOUND => no_route(snap, model_id, body.trim()),
        _ => format!("HTTP {}: {}", status.as_u16(), body.trim()),
    };
    Read {
        ok: false,
        detail,
        decided: None,
        read_whole,
    }
}

/// The failure of a probe that met a 404 (§3.6): the image predates the
/// route, as the route's own 501 says (§2.7). `body` is what llama-server
/// answered.
fn no_route(snap: &Snapshot, model_id: &str, body: &str) -> String {
    format!(
        "HTTP 404 on /v1/systemone ({body}): the image '{}' of the decision model \
         '{model_id}' has no System One route. The route needs a llama.cpp build from b11361 \
         (2026-10-02) on; pick a newer image for the row or the aux class on the Backends \
         page.",
        image_of(snap, model_id)
    )
}

/// Refresh what the container behind `hold` says about itself (§3.6, §3.7):
/// its `/props` and whether it has the route, read live and written to its
/// registry entry, so a start's warning goes once the route answers. The
/// caller asks only after the container answered the probe over HTTP; each
/// read gets what one readiness probe gets
/// ([`crate::runtime::registry::LIVE_FACT_READ`]), and one cut short leaves
/// what the entry had.
pub(super) async fn refresh_facts(state: &SharedState, snap: &Snapshot, hold: &LocalHold) {
    let (port, generation) = hold.attempt();
    let budget = crate::runtime::registry::LIVE_FACT_READ;
    let runtime = state.runtime();
    let props = runtime.read_llama_props(port, budget).await;
    runtime.refresh_llama_props(hold.class(), hold.model_id(), generation, props.clone());
    let build = props.as_ref().ok().and_then(|f| f.build_info.clone());
    let route = runtime
        .read_decision_route(
            port,
            budget,
            &image_of(snap, hold.model_id()),
            build.as_deref(),
        )
        .await;
    runtime.refresh_decision_route(hold.class(), hold.model_id(), generation, route);
}

/// What a passed probe read.
pub(super) struct Decided {
    pub input_tokens: u64,
    pub answer: DecideAnswer,
}

/// Whether a 200's body answers the probe (§3.7): a System One answer,
/// keyed by the question id, naming one of the options, with probabilities
/// that sum to 1 within 1e-6. The model's choice is not judged, only that
/// the route works. `Err` says what was wrong, with the body's start.
pub(super) fn judge(body: &[u8]) -> Result<Decided, String> {
    let start = || {
        String::from_utf8_lossy(body)
            .chars()
            .take(300)
            .collect::<String>()
    };
    let answered: SystemOneResponse = serde_json::from_slice(body).map_err(|e| {
        format!(
            "HTTP 200 but the body is not a System One answer ({e}): {}",
            start()
        )
    })?;
    let answer = answered.answers.get(QUESTION).ok_or_else(|| {
        format!(
            "HTTP 200 but no answer under the question id '{QUESTION}': {}",
            start()
        )
    })?;
    let Answer::Choice(choice) = answer else {
        return Err(format!(
            "HTTP 200 but the choice question got another type of answer: {}",
            start()
        ));
    };
    if !OPTIONS.contains(&choice.choice.as_str()) {
        return Err(format!(
            "the answer names '{}', which is none of the options ({})",
            choice.choice,
            OPTIONS.join(", ")
        ));
    }
    let sum: f64 = choice.probabilities.iter().map(|(_, p)| *p).sum();
    if (sum - 1.0).abs() > SUM_TOLERANCE {
        return Err(format!(
            "the answer's probabilities sum to {sum}, not 1 (within {SUM_TOLERANCE})"
        ));
    }
    Ok(Decided {
        input_tokens: answered.usage.input_tokens,
        answer: DecideAnswer {
            choice: choice.choice.clone(),
            probabilities: choice
                .probabilities
                .iter()
                .map(|(option, p)| OptionProbability {
                    option: option.to_string(),
                    probability: *p,
                })
                .collect(),
            confidence: choice.confidence,
        },
    })
}
