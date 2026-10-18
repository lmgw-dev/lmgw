//! `GET /decision-lab/api/models` (labs-store design §2.2): every
//! `exposed_entries` entry whose endpoints include `/v1/systemone` — the
//! local `decide/*` rows and every cloud alias or catalog entry of task
//! `decision` — as the Image lab lists its models, with what produces the
//! visible errors: the header's shape and type, the ubatch and context in
//! effect, the hold, the container, and the limits a request must fit; and
//! the price rows the gateway prices its requests by.

use std::path::Path;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::decision_lab::form::ENDPOINT;
use lmgw_api_types::decision_lab::{
    CountSource, DecisionBackend, DecisionFacts, DecisionFallbackPrice, DecisionLabModel,
    DecisionLabModels, DecisionPrice,
};

use crate::capabilities::exposed::{exposed_entries, ExposedEntry};
use crate::config::{AuxBackend, AuxKind, AuxModel, FallbackRoute, Snapshot};
use crate::modelinfo::decision::limits::{self, LocalFacts};
use crate::modelinfo::decision::Shape;
use crate::ops::price_row_view;
use crate::runtime::aux::{
    batch_in_effect, threads_in_effect, ubatch_in_effect, CountFrom, LLAMA_DEFAULT_BATCH,
    LLAMA_DEFAULT_UBATCH,
};
use crate::runtime::registry::RuntimeView;
use crate::runtime::Class;
use crate::state::SharedState;
use crate::vram::Target;

pub async fn list_models(State(state): State<SharedState>) -> Response {
    let models = models(&state).await;
    let snap = state.snapshot();
    Json(DecisionLabModels {
        models,
        hold: snap.settings.hold.active,
        currency: snap.settings.currency.clone(),
    })
    .into_response()
}

/// Every model that answers `/v1/systemone`.
pub(crate) async fn models(state: &SharedState) -> Vec<DecisionLabModel> {
    let snap = state.snapshot();
    let runtime: Vec<RuntimeView> = state
        .runtime()
        .list()
        .into_iter()
        .filter(|v| v.class == Class::Aux)
        .collect();
    let mut out = Vec::new();
    for e in exposed_entries(state).await {
        let answers = e
            .capabilities
            .as_ref()
            .is_some_and(|c| c.endpoints.iter().any(|p| p == ENDPOINT));
        if answers {
            out.push(model_of(state, &snap, &runtime, e).await);
        }
    }
    out
}

/// The model `name`, when it answers `/v1/systemone`.
pub(crate) async fn model(state: &SharedState, name: &str) -> Option<DecisionLabModel> {
    models(state).await.into_iter().find(|m| m.name == name)
}

/// What prices a request to `name`: the rows the gateway's own lookup
/// ([`Snapshot::sheet_rows`], under [`Snapshot::sheet_for`]) selects for its
/// primary route, the one a request takes when no fallback answers.
pub(crate) fn price_of(snap: &Snapshot, name: &str) -> DecisionPrice {
    let f = fallback_price(snap, name);
    DecisionPrice {
        local: f.local,
        rows: f.rows,
        unresolved: f.unresolved,
        fallback: None,
    }
}

/// [`price_of`] for the alias `name`, as the fallback's price names it.
fn fallback_price(snap: &Snapshot, name: &str) -> DecisionFallbackPrice {
    let mut out = DecisionFallbackPrice {
        alias: name.to_string(),
        ..Default::default()
    };
    match snap.resolve(name) {
        Err(e) => out.unresolved = Some(e.to_string()),
        Ok(route) => {
            match snap.sheet_rows(name, Some(route.upstream.id), Some(&route.upstream_model)) {
                None => out.local = true,
                Some(rows) => out.rows = rows.into_iter().map(price_row_view).collect(),
            }
        }
    }
    out
}

/// A count in effect as a number and its source, llama.cpp's default when
/// nothing names one.
fn in_effect(count: Option<(i64, CountFrom)>, default: i64) -> (u64, CountSource) {
    match count {
        Some((n, from)) if n > 0 => (
            n as u64,
            match from {
                CountFrom::Field => CountSource::Row,
                CountFrom::Args => CountSource::Args,
            },
        ),
        _ => (default.unsigned_abs(), CountSource::LlamaCppDefault),
    }
}

async fn model_of(
    state: &SharedState,
    snap: &Snapshot,
    runtime: &[RuntimeView],
    e: ExposedEntry,
) -> DecisionLabModel {
    let caps = e.capabilities.as_ref();
    let input_modalities = caps.and_then(|c| c.input_modalities.clone());
    let sees = input_modalities
        .as_ref()
        .is_some_and(|m| m.iter().any(|x| x == "image"));
    let row = snap
        .aux_models
        .iter()
        .find(|m| m.kind == AuxKind::Decision && snap.aux_public_name(m) == e.name);
    let mut price = price_of(snap, &e.name);
    let Some(m) = row else {
        return DecisionLabModel {
            price,
            limits: limits::cloud(&e.name, e.context_length),
            reads_images: sees,
            name: e.name,
            owner: e.owner,
            local: false,
            model_id: None,
            input_modalities,
            context_length: e.context_length,
            notes: e.notes,
            state: None,
            warnings: Vec::new(),
            held: false,
            hold_fallback: None,
            facts: None,
        };
    };
    let rt = runtime.iter().find(|r| r.model_id == m.model_id);
    let props = rt.and_then(|r| r.llama_props.clone());
    let (facts, shape, who) = local_facts(state, snap, m, props.as_deref()).await;
    let who = who.unwrap_or_else(|| e.name.clone());
    let limits = match shape {
        Some(shape) => limits::local(&LocalFacts {
            who: &who,
            shape,
            ubatch: facts.ubatch,
            ubatch_from: facts.ubatch_source.label(),
            n_ctx_slot: facts.n_ctx_slot,
            ctx_size: facts.ctx_size,
        }),
        None => Vec::new(),
    };
    let target = Target {
        class: Class::Aux,
        model_id: m.model_id.clone(),
    };
    let hold_fallback = match snap.fallback_route(&target) {
        FallbackRoute::Usable { alias, .. } => Some(alias),
        FallbackRoute::None | FallbackRoute::Unusable { .. } => None,
    };
    let held = snap.gpu_block_for(Class::Aux, &m.model_id).is_some();
    // Held with a fallback that can answer: it answers every request, and
    // the gateway records them at its rows.
    if let (true, Some(alias)) = (held, &hold_fallback) {
        price.fallback = Some(fallback_price(snap, alias));
    }
    DecisionLabModel {
        name: e.name,
        owner: e.owner,
        local: true,
        model_id: Some(m.model_id.clone()),
        reads_images: props.as_ref().and_then(|p| p.vision).unwrap_or(sees),
        input_modalities,
        context_length: e.context_length,
        notes: e.notes,
        state: rt.map(|r| r.state.as_str().to_string()),
        warnings: rt.map(|r| r.warnings.clone()).unwrap_or_default(),
        held,
        hold_fallback,
        facts: Some(facts),
        limits,
        price,
    }
}

/// A local row's facts, its header's shape, and the decision type a rule
/// names it by.
async fn local_facts(
    state: &SharedState,
    snap: &Snapshot,
    m: &AuxModel,
    props: Option<&crate::egress::llama_cpp::props::LlamaFacts>,
) -> (DecisionFacts, Option<Shape>, Option<String>) {
    let path = Path::new(&snap.settings.aux_router.models_dir).join(&m.gguf_path);
    let header = state.gguf_cache.summarize_cached(&path).await;
    let shape = header.as_ref().ok().map(|s| Shape::of(s));
    let (batch, batch_source) = in_effect(batch_in_effect(m), LLAMA_DEFAULT_BATCH);
    let (mut ubatch, mut ubatch_source) = in_effect(ubatch_in_effect(m), LLAMA_DEFAULT_UBATCH);
    // llama.cpp lowers the ubatch to the batch.
    if ubatch > batch {
        (ubatch, ubatch_source) = (batch, CountSource::Batch);
    }
    // Embedding mode lowers the batch to the ubatch.
    let embedding = matches!(shape, Some(Shape::NonCausal | Shape::CausalHead));
    let (batch, batch_source) = if embedding && batch > ubatch {
        (ubatch, CountSource::Ubatch)
    } else {
        (batch, batch_source)
    };
    let (threads, threads_source) = threads_in_effect(m, crate::host::cpu());
    let decision_type = header.as_ref().ok().and_then(|s| s.decision_type.clone());
    let facts = DecisionFacts {
        header_error: header.as_ref().err().cloned(),
        decision_type: decision_type.clone(),
        max_head_tokens: header
            .as_ref()
            .ok()
            .and_then(|s| s.decision_max_head_tokens),
        shape: shape.map(Into::into),
        ctx_size: m.ctx_size.and_then(|n| u64::try_from(n).ok()),
        n_ctx_slot: props.and_then(|p| p.n_ctx_slot),
        ubatch,
        ubatch_source,
        batch,
        batch_source,
        backend: match m.backend {
            Some(AuxBackend::Cpu) => DecisionBackend::Cpu,
            _ => DecisionBackend::Gpu,
        },
        threads_in_effect: threads,
        threads_source,
    };
    (facts, shape, decision_type)
}
