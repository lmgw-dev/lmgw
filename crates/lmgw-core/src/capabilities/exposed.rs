//! One exposed model, fully described — the join of "what is routable"
//! (`Snapshot::exposed_models` plus the expose-all catalogs) with "what it can
//! do" (the pure builders in [`super`]), per the model-capabilities design
//! `docs/design/2026-09-17-model-capabilities-design.md` §2, §3.6
//! and §8 item 8.
//!
//! This module is the **only** place that does the I/O the builders refuse to:
//! GGUF header reads (through `state.gguf_cache`, §3.5), the chat-template
//! override file, the sibling-projector `read_dir`, and the cached upstream
//! catalog fetch. `server::list_models` renders what comes out of here into
//! whichever dialect the caller asked for and does no derivation of its own,
//! so the OpenAI shape, the Anthropic shape, `GET /v1/models/{id}` and (later)
//! `lmgw__models` cannot drift apart.
//!
//! Cost: every row's reads run concurrently ([`futures::future::join_all`]).
//! The summary cache makes a warm call free, but a cold one on this box is 23
//! local rows × a header read that walks the tokenizer arrays — serialising
//! those would make the first `/v1/models` after startup take seconds.

use std::path::{Path, PathBuf};

use futures::future::join_all;

use crate::candidates::Facet;
use crate::catalog::{self, ModelInfo, Pricing};
use crate::config::{
    AudioModel, AuxModel, ImageModel, LocalModel, Protocol, Snapshot, Upstream, UpstreamKind,
};
use crate::ir::Params;
use crate::runtime::image::ImageCapabilities;
use crate::state::SharedState;

use super::{
    Derived, ModelCapabilities, ProjectorStatus, ReasoningCaps, StructuredOutputCaps, ToolCallCaps,
};

/// One client-routable model, with everything `GET /v1/models` publishes about
/// it. Dialect-independent on purpose: the two wire shapes (design §2.1, §2.2)
/// are two renderings of this one struct.
#[derive(Debug, Clone)]
pub struct ExposedEntry {
    pub name: String,
    pub owner: String,
    pub context_length: Option<u64>,
    /// Per-token price. `Some("0", "0")` for local models (free); forwarded
    /// from the upstream catalog for cloud models that advertise it; `None`
    /// when a cloud upstream does not publish pricing.
    pub pricing: Option<Pricing>,
    /// Unix seconds: the catalog's own timestamp when it publishes one, else
    /// the gateway's start time — **never** `now()`, which would tell a client
    /// polling the list that every model was just recreated (design §2.1).
    pub created: i64,
    /// One response's generation cap, when a real number exists.
    pub max_output_tokens: Option<u64>,
    /// `None` = the facts could not be read at all (an unreadable GGUF); the
    /// notes then name the file rather than leaving a silent hole.
    pub capabilities: Option<ModelCapabilities>,
    pub notes: Vec<String>,
}

/// Zero per-token price, shared by everything running on our own hardware.
fn zero_pricing() -> Pricing {
    Pricing {
        prompt: "0".to_string(),
        completion: "0".to_string(),
    }
}

/// All client-routable model names with their capabilities: explicit aliases,
/// every enabled local model of the three classes, and (live, cached) the
/// catalogs of expose-all upstreams. Hidden passthrough models are excluded.
///
/// Aux and audio arrive from their tables rather than from an HTTP catalog
/// fetch (§5). There is no always-on class router to ask any more, so the
/// numbers come from what lmgw already parsed: an aux model's `context_length`
/// is the context its KV cache is planned against (`vram::plan`, memoized),
/// and audio models have no such metadata at all — reported as unknown rather
/// than invented.
pub async fn exposed_entries(state: &SharedState) -> Vec<ExposedEntry> {
    let snap = state.snapshot();
    // Every row without a timestamp of its own shares this one, so two calls
    // in the same process publish the same `created` (design §2.1).
    let default_created = state.started_at_utc.timestamp();

    let listed = snap.exposed_models();
    let mut out: Vec<ExposedEntry> = join_all(
        listed
            .into_iter()
            .map(|e| entry_for(state, &snap, e, default_created)),
    )
    .await;

    let mut passthrough: Vec<Upstream> = snap
        .upstreams
        .values()
        .filter(|u| u.enabled && u.expose_all)
        .cloned()
        .collect();
    passthrough.sort_by(|a, b| a.name.cmp(&b.name));
    for u in &passthrough {
        match catalog::upstream_models(state, u).await {
            Ok(models) => {
                for m in models {
                    if snap.hidden_passthrough.contains(&(u.id, m.id.clone())) {
                        continue;
                    }
                    let name = match u.prefix() {
                        "" => m.id.clone(),
                        p => format!("{p}/{}", m.id),
                    };
                    if out.iter().any(|e| e.name == name) {
                        continue;
                    }
                    // A passthrough local-container upstream (llama.cpp
                    // router, audio.cpp) is still free.
                    let pricing = m.pricing.clone().or_else(|| {
                        matches!(
                            u.kind,
                            UpstreamKind::LlamaServer
                                | UpstreamKind::AudioCpp
                                | UpstreamKind::SdCpp
                        )
                        .then(zero_pricing)
                    });
                    let derived = super::for_catalog(&m, u.protocol, &u.name);
                    out.push(assemble(
                        name,
                        u.name.clone(),
                        m.context_length,
                        pricing,
                        derived,
                        default_created,
                    ));
                }
            }
            Err(e) => tracing::warn!("listing models of upstream {}: {e}", u.name),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// One exposed model's full description by name — exactly what
/// [`exposed_entries`] would produce for this one name, computed without
/// building the rest of the catalog or fetching every enabled `expose_all`
/// upstream's live models. `GET /v1/models/{id}` uses this (rather than
/// finding a name in the full list, which it used to build just to throw
/// almost all of away), and so does Chat's send-time vision check
/// (`web::chat::model_vision`, chat-archive-pin-attachments review finding
/// 9) — one function, so the two answers about the same model cannot drift.
///
/// A statically enumerated name (an alias, or a local/aux/audio/image row —
/// [`Snapshot::exposed_models`]) costs exactly what deriving that one row
/// costs. A name that is not statically enumerated is checked against each
/// enabled `expose_all` upstream in the same order [`exposed_entries`]'s
/// second pass would, but only *that* upstream's catalog is fetched, and only
/// until one answers — not all of them upfront.
pub async fn exposed_entry(state: &SharedState, name: &str) -> Option<ExposedEntry> {
    let snap = state.snapshot();
    let default_created = state.started_at_utc.timestamp();

    if let Some(e) = snap.exposed_models().into_iter().find(|e| e.name == name) {
        return Some(entry_for(state, &snap, e, default_created).await);
    }

    // Not statically enumerated: maybe a bare passthrough id under one of the
    // enabled expose-all upstreams (`exposed_entries`'s second pass).
    let mut passthrough: Vec<Upstream> = snap
        .upstreams
        .values()
        .filter(|u| u.enabled && u.expose_all)
        .cloned()
        .collect();
    passthrough.sort_by(|a, b| a.name.cmp(&b.name));
    for u in &passthrough {
        let model_id = match u.prefix() {
            "" => name.to_string(),
            p => match name.strip_prefix(p).and_then(|s| s.strip_prefix('/')) {
                Some(rest) => rest.to_string(),
                None => continue,
            },
        };
        if snap.hidden_passthrough.contains(&(u.id, model_id.clone())) {
            continue;
        }
        match catalog::upstream_models(state, u).await {
            Ok(models) => {
                if let Some(m) = models.into_iter().find(|m| m.id == model_id) {
                    let pricing = m.pricing.clone().or_else(|| {
                        matches!(
                            u.kind,
                            UpstreamKind::LlamaServer
                                | UpstreamKind::AudioCpp
                                | UpstreamKind::SdCpp
                        )
                        .then(zero_pricing)
                    });
                    let derived = super::for_catalog(&m, u.protocol, &u.name);
                    return Some(assemble(
                        name.to_string(),
                        u.name.clone(),
                        m.context_length,
                        pricing,
                        derived,
                        default_created,
                    ));
                }
            }
            Err(e) => tracing::warn!("listing models of upstream {}: {e}", u.name),
        }
    }
    None
}

/// The `Derived` half of an entry folded onto the four fields that come from
/// the listing itself.
fn assemble(
    name: String,
    owner: String,
    context_length: Option<u64>,
    pricing: Option<Pricing>,
    derived: Derived,
    default_created: i64,
) -> ExposedEntry {
    ExposedEntry {
        name,
        owner,
        context_length,
        pricing,
        created: derived.created.unwrap_or(default_created),
        max_output_tokens: derived.max_output_tokens,
        capabilities: derived.capabilities,
        notes: derived.notes,
    }
}

/// One statically-known exposed model (alias / local / aux / audio / image).
async fn entry_for(
    state: &SharedState,
    snap: &Snapshot,
    e: crate::config::ExposedModel,
    default_created: i64,
) -> ExposedEntry {
    match e.source {
        "aux" => {
            let row = snap
                .enabled_aux_models()
                .find(|m| snap.aux_public_name(&m.model_id) == e.name);
            let (ctx, derived) = match row {
                Some(m) => (
                    state
                        .vram
                        .context_tokens(snap, crate::runtime::Class::Aux, &m.model_id)
                        .await,
                    aux_derived(state, snap, m).await,
                ),
                None => (None, Derived::default()),
            };
            assemble(
                e.name,
                crate::config::AUX_UPSTREAM_NAME.to_string(),
                ctx,
                Some(zero_pricing()),
                derived,
                default_created,
            )
        }
        "audio" => {
            // audio.cpp exposes no per-model metadata (see `vram::plan`), so
            // there is no context window to publish.
            let row = snap
                .enabled_audio_models()
                .find(|m| snap.audio_public_name(&m.model_id) == e.name);
            let derived = match row {
                Some(m) => audio_derived(state, m).await,
                None => Derived::default(),
            };
            assemble(
                e.name,
                crate::config::AUDIO_UPSTREAM_NAME.to_string(),
                None,
                Some(zero_pricing()),
                derived,
                default_created,
            )
        }
        "image" => {
            // No context window and no per-token price: a diffusion pipeline
            // has neither, and the row's own columns are the whole story (§5).
            let derived = snap
                .enabled_image_models()
                .find(|m| snap.image_public_name(&m.model_id) == e.name)
                .map(|m| image_derived(state, m))
                .unwrap_or_default();
            assemble(
                e.name,
                crate::config::IMAGE_UPSTREAM_NAME.to_string(),
                None,
                Some(zero_pricing()),
                derived,
                default_created,
            )
        }
        "local" => {
            let row = snap
                .local_models
                .iter()
                .find(|m| snap.local_public_name(&m.model_id) == e.name);
            match row {
                Some(m) => local_row_entry(state, snap, m, e.name).await,
                None => assemble(
                    e.name,
                    crate::config::ROUTER_UPSTREAM_NAME.to_string(),
                    None,
                    Some(zero_pricing()),
                    Derived::default(),
                    default_created,
                ),
            }
        }
        "candidate_alias" => candidate_alias_entry(state, snap, e.name, default_created).await,
        _ => alias_entry(state, snap, e.name, default_created).await,
    }
}

/// A candidate alias's `/v1/models` entry (candidate-aliases design §4.6, §8
/// item 8): capabilities are **exactly** the enabled facets, published
/// positively — a facet the alias does not enable is an explicit negative
/// where the schema has one (`vision: false`, `tool_calls.kind: "none"`,
/// `structured_output` both flags `false`), and simply absent where it does
/// not (`reasoning`, which has no "off" shape).
///
/// Detail fields (reasoning `levels`/`default`/`control`/`can_disable`,
/// tool-call `format`/`parallel`) are published only when every `routable`
/// candidate agrees on the value — [`identical`] — otherwise left absent
/// rather than picking one candidate's answer arbitrarily. A `reasoning.kind`
/// that differs across `routable` publishes `fixed` (nothing reliably
/// controllable per request through this alias) and says so in a note.
async fn candidate_alias_entry(
    state: &SharedState,
    snap: &Snapshot,
    name: String,
    default_created: i64,
) -> ExposedEntry {
    let Some(ca) = snap.candidate_alias(&name) else {
        return assemble(
            name,
            "lmgw".to_string(),
            None,
            None,
            Derived::default(),
            default_created,
        );
    };
    let d = crate::candidates::derive::derive(state, snap, ca).await;
    let enabled = |f: Facet| d.enabled.contains(f);
    let routable_caps: Vec<&ModelCapabilities> = d.routable_capabilities.iter().flatten().collect();

    let vision = enabled(Facet::Vision);
    let audio = enabled(Facet::Audio);
    let mut input_modalities = vec!["text".to_string()];
    if vision {
        input_modalities.push("image".to_string());
    }
    if audio {
        input_modalities.push("audio".to_string());
    }

    let tool_calls_on = enabled(Facet::ToolCalls);
    let tool_calls = ToolCallCaps {
        kind: if tool_calls_on { "native" } else { "none" }.to_string(),
        parallel: if tool_calls_on {
            identical(
                routable_caps
                    .iter()
                    .map(|c| c.tool_calls.as_ref().and_then(|t| t.parallel)),
            )
            .flatten()
        } else {
            None
        },
        format: if tool_calls_on {
            identical(
                routable_caps
                    .iter()
                    .map(|c| c.tool_calls.as_ref().and_then(|t| t.format.clone())),
            )
            .flatten()
        } else {
            None
        },
    };

    let structured_on = enabled(Facet::StructuredOutput);
    let structured_output = StructuredOutputCaps {
        json_schema: Some(structured_on),
        json_object: Some(
            structured_on
                && !routable_caps.is_empty()
                && routable_caps.iter().all(|c| {
                    c.structured_output
                        .as_ref()
                        .is_some_and(|s| s.json_object == Some(true))
                }),
        ),
    };

    let mut notes = vec![format!(
        "Candidate alias: primary '{}'{}{}.",
        ca.primary().unwrap_or("?"),
        if ca.candidates.len() > 1 {
            format!(", alternates {}", ca.candidates[1..].join(", "))
        } else {
            String::new()
        },
        if ca.background { ", background" } else { "" },
    )];

    let reasoning = enabled(Facet::Reasoning).then(|| {
        let kinds: Vec<&str> = routable_caps
            .iter()
            .filter_map(|c| c.reasoning.as_ref().map(|r| r.kind.as_str()))
            .collect();
        match identical(kinds.iter().copied()) {
            Some(kind) => ReasoningCaps {
                kind: kind.to_string(),
                levels: identical(routable_caps.iter().map(|c| {
                    c.reasoning
                        .as_ref()
                        .map(|r| r.levels.clone())
                        .unwrap_or_default()
                }))
                .unwrap_or_default(),
                default: identical(
                    routable_caps
                        .iter()
                        .map(|c| c.reasoning.as_ref().and_then(|r| r.default.clone())),
                )
                .flatten(),
                can_disable: identical(
                    routable_caps
                        .iter()
                        .map(|c| c.reasoning.as_ref().and_then(|r| r.can_disable)),
                )
                .flatten(),
                control: identical(routable_caps.iter().map(|c| {
                    c.reasoning
                        .as_ref()
                        .map(|r| r.control.clone())
                        .unwrap_or_default()
                }))
                .unwrap_or_default(),
                ..Default::default()
            },
            _ => {
                notes.push(
                    "Reasoning kind differs across routable candidates; publishing 'fixed' \
                     since nothing is reliably controllable per request through this alias."
                        .to_string(),
                );
                ReasoningCaps {
                    kind: "fixed".to_string(),
                    ..Default::default()
                }
            }
        }
    });

    let caps = ModelCapabilities {
        task: "chat".to_string(),
        endpoints: super::CHAT_ENDPOINTS_OPENAI
            .iter()
            .map(|s| s.to_string())
            .collect(),
        input_modalities: Some(input_modalities),
        output_modalities: Some(vec!["text".to_string()]),
        vision: Some(vision),
        reasoning,
        tool_calls: Some(tool_calls),
        structured_output: Some(structured_output),
        speech: None,
        source: "candidate_alias".to_string(),
    };

    let derived = Derived {
        capabilities: Some(caps),
        max_output_tokens: d.max_output_tokens,
        notes,
        created: None,
    };
    assemble(
        name,
        crate::config::ROUTER_UPSTREAM_NAME.to_string(),
        d.context_length,
        Some(zero_pricing()),
        derived,
        default_created,
    )
}

/// `Some(v)` when every item `it` yields is `v` and there is at least one —
/// used by [`candidate_alias_entry`] to decide whether a detail field is
/// identical across every routable candidate, and so worth publishing, or
/// whether it is left absent instead of picking one candidate's answer
/// arbitrarily.
fn identical<T: Clone + PartialEq>(mut it: impl Iterator<Item = T>) -> Option<T> {
    let first = it.next()?;
    it.all(|v| v == first).then_some(first)
}

/// One local chat row's full [`ExposedEntry`] under the given `name` — the
/// guts of [`entry_for`]'s `"local"` arm, factored out so a caller that
/// already holds the row (candidate-aliases design §4.6:
/// `candidates::derive`, judging a candidate that need not be `public`) does
/// not have to round-trip through [`Snapshot::local_public_name`] and
/// [`Snapshot::exposed_models`] to reach it. Local models run on our own
/// hardware: always free.
///
/// Cost: one cached GGUF header read (`state.gguf_cache`, free once warm),
/// plus — on a ladder row — a second cached read of the top rung's GGUF for
/// its trained context. Safe to call once per candidate per alias save/list;
/// not sized for a per-request hot-path call (see `candidates::derive`'s own
/// cost note for the cheaper query the gate worker needs instead).
pub(crate) async fn local_row_entry(
    state: &SharedState,
    snap: &Snapshot,
    m: &LocalModel,
    name: String,
) -> ExposedEntry {
    let default_created = state.started_at_utc.timestamp();
    // The per-request context a client can actually use — split (ctx_size /
    // parallel) or unified (min of the pool, the per-slot cap and the
    // trained context), per `LlamaParams::per_request_ctx` (unified-KV
    // design §3.2). A ladder row publishes the **top** rung's per-slot
    // context instead (ladder design §4.4): the ladder delivers it by
    // climbing. A ladder is never unified (§4.3 rule 2), but llama-server
    // caps every slot at the weights' trained context, so the top rung's
    // slot is capped at its own GGUF's (`ladder::slot_ctx`) — the number the
    // gate judges by.
    let ctx = if m.is_ladder() {
        let top = m
            .all_rungs()
            .and_then(|r| r.last().map(|r| r.gguf_path.to_string()));
        let trained = match &top {
            Some(path) => trained_context(state, chat_models_dir(snap), path).await,
            None => None,
        };
        m.per_slot_ctx(m.top_rung())
            .map(|c| crate::ladder::slot_ctx(c, trained) as u64)
    } else {
        let trained = trained_context(state, chat_models_dir(snap), &m.gguf_path).await;
        m.params.per_request_ctx(trained).map(|c| c as u64)
    };
    let derived = local_derived(state, m, chat_models_dir(snap), None).await;
    assemble(
        name,
        crate::config::ROUTER_UPSTREAM_NAME.to_string(),
        ctx,
        Some(zero_pricing()),
        derived,
        default_created,
    )
}

/// An explicit alias: context window and pricing come from the upstream's
/// cached catalog, capabilities from whatever backs it (design §3.6 — an alias
/// onto a local row publishes the row's own facts, with the alias'
/// `param_overrides` folded over them).
async fn alias_entry(
    state: &SharedState,
    snap: &Snapshot,
    name: String,
    default_created: i64,
) -> ExposedEntry {
    let target = snap
        .aliases
        .values()
        .find(|a| a.alias == name)
        .and_then(|a| snap.upstreams.get(&a.upstream_id).map(|u| (a, u)))
        .map(|(a, u)| {
            (
                a.upstream_model_id.clone(),
                a.param_overrides.clone(),
                a.capabilities_override.clone(),
                u.clone(),
            )
        });

    let Some((model_id, overrides, owner_override, upstream)) = target else {
        return assemble(
            name,
            "lmgw".to_string(),
            None,
            None,
            Derived::default(),
            default_created,
        );
    };

    // An alias onto a local container (llama.cpp, audio.cpp, sd-server) is
    // still free.
    let local_zero = matches!(
        upstream.kind,
        UpstreamKind::LlamaServer | UpstreamKind::AudioCpp | UpstreamKind::SdCpp
    )
    .then(zero_pricing);

    // The backing row, when the alias points at one of our own containers
    // (§3.6); `None` means the capabilities have to come from the catalog.
    let backing = backing_derived(state, snap, &upstream, &model_id, &overrides).await;

    // The context window and the price always come from the catalog, exactly
    // as they did before capabilities existed — including the "not listed" and
    // "upstream unreachable" cases, which both fall back to the local-zero
    // price and no context window.
    let info = catalog_lookup(state, &upstream, &model_id).await;
    let (ctx, pricing) = match &info {
        Some(m) => (m.context_length, m.pricing.clone().or(local_zero)),
        None => (None, local_zero),
    };

    let derived = match (backing, info) {
        (Some(d), _) => d,
        (None, Some(m)) => super::for_catalog(&m, upstream.protocol, &upstream.name),
        (None, None) => Derived {
            capabilities: None,
            max_output_tokens: None,
            notes: vec![format!(
                "The upstream catalog of '{}' does not list this model, so nothing about its \
                 capabilities could be read; requests are still forwarded to \
                 '{model_id}' there.",
                upstream.name
            )],
            created: None,
        },
    };
    // The alias' own override sits above the backing row's (which
    // `local_derived` already applied), the same way `param_overrides` sit
    // above the row's params.
    let derived = with_owner_override(derived, owner_override.as_ref(), "alias", upstream.protocol);

    assemble(
        name,
        "lmgw".to_string(),
        ctx,
        pricing,
        derived,
        default_created,
    )
}

/// The local row behind an alias, when there is one (design §3.6): a
/// `LlamaServer` upstream resolves against the chat rows first and the aux
/// rows second, an `AudioCpp` upstream against the audio rows and an `SdCpp`
/// one against the image rows. `None` means
/// "this alias is a cloud route" — or names a row that no longer exists, which
/// the catalog path then reports.
async fn backing_derived(
    state: &SharedState,
    snap: &Snapshot,
    upstream: &Upstream,
    model_id: &str,
    overrides: &Params,
) -> Option<Derived> {
    match upstream.kind {
        UpstreamKind::LlamaServer => {
            if let Some(m) = snap.local_models.iter().find(|m| m.model_id == model_id) {
                return Some(local_derived(state, m, chat_models_dir(snap), Some(overrides)).await);
            }
            let aux = snap.aux_models.iter().find(|m| m.model_id == model_id)?;
            Some(aux_derived(state, snap, aux).await)
        }
        UpstreamKind::AudioCpp => {
            let row = snap.audio_models.iter().find(|m| m.model_id == model_id)?;
            Some(audio_derived(state, row).await)
        }
        UpstreamKind::SdCpp => snap
            .image_models
            .iter()
            .find(|m| m.model_id == model_id)
            .map(|m| image_derived(state, m)),
        UpstreamKind::Generic => None,
    }
}

/// One entry of an upstream's (cached) catalog. A fetch failure is a miss:
/// `/v1/models` must answer even when a provider is down.
async fn catalog_lookup(
    state: &SharedState,
    upstream: &Upstream,
    model_id: &str,
) -> Option<ModelInfo> {
    catalog::upstream_models(state, upstream)
        .await
        .ok()?
        .into_iter()
        .find(|m| m.id == model_id)
}

// ---------------------------------------------------------------------------
// Local classes — the file reads the pure builders refuse to do
// ---------------------------------------------------------------------------

fn chat_models_dir(snap: &Snapshot) -> &str {
    &snap.settings.router.models_dir
}

fn aux_models_dir(snap: &Snapshot) -> &str {
    &snap.settings.aux_router.models_dir
}

/// The model's trained maximum context (`<arch>.context_length` in the GGUF
/// header) — one of the terms [`crate::config::LlamaParams::per_request_ctx`]
/// takes the minimum of for a unified row (unified-KV design §3.2). Not
/// consulted by [`crate::config::LlamaParams::pool_tokens`] any more (review
/// finding 2): the pool's own size has to come from `ctx_size` or
/// `kv_unified_per_slot`, never a guess at what `--fit` would have shrunk an
/// unset `ctx_size` to. `pub(crate)` because quickdoc's own context lookup
/// (`quickdoc::ingest::context_length`) threads the same fact through for the
/// ingest model. `None` when the file cannot be read or the header does not
/// say — never guessed.
pub(crate) async fn trained_context(
    state: &SharedState,
    dir: &str,
    gguf_path: &str,
) -> Option<i64> {
    let path = Path::new(dir).join(gguf_path);
    let summary = state.gguf_cache.summarize_cached(&path).await.ok()?;
    summary.context_length.and_then(|v| i64::try_from(v).ok())
}

/// An aux row: the summary is read only so the notes can say the file is
/// missing — nothing published for this class is derived from it (§3.5).
async fn aux_derived(state: &SharedState, snap: &Snapshot, model: &AuxModel) -> Derived {
    let path = Path::new(aux_models_dir(snap)).join(&model.gguf_path);
    let summary = state.gguf_cache.summarize_cached(&path).await.ok();
    super::for_aux(model, summary.as_deref())
}

/// An image row: everything published comes from the row's own columns
/// (design §5), plus — for the notes only — whatever this model's *running*
/// container reported about the pipeline it loaded.
/// An audio row's capabilities, with what its package says
/// ([`super::AudioFacts`]): a speaking row's `speech` object from its
/// profile (cached, computed on the blocking pool when a row changed), and
/// any row's notes about its package — a task its variant does not run,
/// voices whose file is missing.
async fn audio_derived(state: &SharedState, model: &AudioModel) -> Derived {
    let speech = crate::audio::voices::row_speech(state, model).await;
    let mut notes = Vec::new();
    // A task its package does not run (audio-class gap 2): every speech
    // request is refused until the owner fixes it.
    if let Some(why) = speech
        .profile
        .variant
        .as_deref()
        .and_then(|v| crate::audio::variant::mismatch(&model.model_id, &model.task, v))
    {
        notes.push(format!(
            "Speech is refused (400 task_mismatch) until this row is fixed: {why}."
        ));
    }
    // A voice the spec names whose file the package lacks (audio-class gap
    // 4): the catalog's download completes the install.
    if !speech.voices.missing.is_empty() {
        notes.push(format!(
            "Voices the package's spec names but whose file is missing here: {} — Download on \
             the package in the Audio catalog (or lmgw__audio_catalog action=download) fetches \
             only the files it lacks.",
            speech.voices.missing.join(", ")
        ));
    }
    // A cloning model that wants the clip's transcript, and library clips
    // without one (audio-class gap 5) — refused outright by an engine that
    // cannot clone without it (`crate::audio::transcript`).
    let untranscribed: Vec<&str> = speech
        .voices
        .entries
        .iter()
        .filter(|e| e.kind == crate::audio::voices::VoiceKind::Library)
        .filter(|e| e.transcript == Some(false))
        .map(|e| e.id.as_str())
        .collect();
    if speech.profile.needs_reference_text && !untranscribed.is_empty() {
        let how = if crate::audio::transcript::refuses_untranscribed(model, &speech.profile) {
            "This model cannot clone a voice-library clip without the clip's transcript \
             (reference_text): speech with these clips is refused (400 voice_needs_transcript) \
             until they have one"
        } else {
            "This model clones a voice-library clip well only with the clip's transcript \
             (reference_text), and these clips have none"
        };
        notes.push(format!(
            "{how}: {} — transcribe them in the Audio lab or with lmgw__voice_transcribe (a \
             speech-to-text model).",
            untranscribed.join(", ")
        ));
    }
    let facts = super::AudioFacts {
        speech: super::speech::speaks(&model.task).then(|| {
            let rate = state.audio_rates.get(&model.model_id);
            super::speech::from_profile(&speech.profile, model, rate)
        }),
        notes,
    };
    super::for_audio_with(model, Some(&facts))
}

fn image_derived(state: &SharedState, model: &ImageModel) -> Derived {
    let probed = probed_image_capabilities(state, &model.model_id);
    let derived = super::for_image(model, probed.as_ref());
    with_owner_override(
        derived,
        model.capabilities_override.as_ref(),
        "image row",
        Protocol::Openai,
    )
}

/// The `GET /sdcpp/v1/capabilities` body this model's container answered with,
/// if it is up and answered one. A registry read, not an HTTP call: the probe
/// ran at start and its result lives on the entry, so `/v1/models` stays a
/// listing rather than a fan-out that wakes containers.
fn probed_image_capabilities(state: &SharedState, model_id: &str) -> Option<ImageCapabilities> {
    state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.class == crate::runtime::Class::Image && v.model_id == model_id)
        .and_then(|v| v.image_capabilities)
}

/// The projector situation of a local row, owned so
/// [`ProjectorStatus`] (which borrows) can be built from it.
enum Projector {
    None,
    Configured(String),
    Unreadable(String, String),
    Sibling(String),
}

/// The static capabilities of one local chat row, for a caller that already
/// has the row and wants exactly what `/v1/models` would publish for it
/// without paying for the rest of [`exposed_entries`] (design §8 item 9 —
/// `lmgw__local_model_test`'s cross-check against the running build's
/// `/props`). Same derivation as the `"local"` arm of [`entry_for`], not a
/// second implementation: both funnel through [`local_derived`].
pub async fn derived_for_local(state: &SharedState, model: &LocalModel) -> Derived {
    let snap = state.snapshot();
    local_derived(state, model, chat_models_dir(&snap), None).await
}

/// A local llama.cpp chat row, or an alias onto one (design §3.2–§3.4, §3.6):
/// the weights' header, the chat-template override file, and the projector —
/// read here, judged in [`super::for_local_row`].
async fn local_derived(
    state: &SharedState,
    model: &LocalModel,
    models_dir: &str,
    overrides: Option<&Params>,
) -> Derived {
    let dir = Path::new(models_dir);
    let weights_path = dir.join(&model.gguf_path);
    let weights = state.gguf_cache.summarize_cached(&weights_path).await.ok();

    // Notes this module owns because the builder never sees the failure.
    let mut extra_notes: Vec<String> = Vec::new();

    // `--chat-template-file` is what llama-server renders, so it wins over the
    // GGUF's embedded template (§3.1). Unreadable ⇒ fall back to the embedded
    // one and say so: the two may disagree, and a silent fallback would
    // publish effort levels the running model does not have.
    let template_override = match model
        .params
        .chat_template_file
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        Some(rel) => match tokio::fs::read_to_string(dir.join(rel)).await {
            Ok(text) => Some(text),
            Err(err) => {
                extra_notes.push(format!(
                    "The configured chat template file {rel} could not be read ({err}); the \
                     reasoning and tool-call facts below come from the template embedded in the \
                     GGUF instead, which is not what llama-server renders for this row."
                ));
                None
            }
        },
        None => None,
    };

    let (projector, projector_summary) = match super::configured_projector(model) {
        Some(rel) => {
            let rel = rel.to_string();
            match state.gguf_cache.summarize_cached(&dir.join(&rel)).await {
                Ok(s) => (Projector::Configured(rel), Some(s)),
                Err(err) => (Projector::Unreadable(rel, err), None),
            }
        }
        None => match sibling_mmproj(weights_path).await {
            Some(name) => (Projector::Sibling(name), None),
            None => (Projector::None, None),
        },
    };
    let status = match &projector {
        Projector::None => ProjectorStatus::None,
        Projector::Configured(p) => ProjectorStatus::Configured { path: p },
        Projector::Unreadable(p, err) => ProjectorStatus::Unreadable { path: p, err },
        Projector::Sibling(p) => ProjectorStatus::SiblingPresentNotConfigured { path: p },
    };

    let mut derived = super::for_local_row(
        model,
        weights.as_deref(),
        projector_summary.as_deref(),
        template_override.as_deref(),
        status,
        overrides,
    );
    derived.notes.extend(extra_notes);

    // The owner's own word about this row, over everything the files said
    // (§7). On the alias path the alias' override lands on top of this one,
    // in `alias_entry` — row first, alias second, exactly as `param_overrides`
    // stack.
    with_owner_override(
        derived,
        model.capabilities_override.as_ref(),
        "model",
        Protocol::LlamaCpp,
    )
}

/// Deep-merge an owner's `capabilities_override` over a derived object (design
/// §7), for a model served over `protocol` (a changed task's routes depend on
/// it). A malformed override is **loud**: the derived facts stay, and the
/// error becomes a note on the model it was written for, so the owner sees it
/// where they will look rather than only in a log.
fn with_owner_override(
    derived: Derived,
    override_: Option<&serde_json::Value>,
    what: &str,
    protocol: Protocol,
) -> Derived {
    let Some(value) = override_ else {
        return derived;
    };
    match super::apply_owner_override(derived.clone(), value, protocol) {
        Ok(applied) => applied,
        Err(e) => {
            let mut derived = derived;
            derived.notes.push(format!(
                "The owner's capabilities override on this {what} was rejected and is not \
                 applied ({e}); everything above is what lmgw derived by itself."
            ));
            derived
        }
    }
}

/// A `*mmproj*.gguf` sitting next to the weights (design §3.3). llama-server
/// started with `-m` never picks one up by itself, so this is only ever a
/// note — but it is the note that explains why a multimodal repo answers
/// text-only.
///
/// The `read_dir` goes to the blocking pool with the header reads: one
/// directory listing is cheap, thousands of them on a cold listing are not.
async fn sibling_mmproj(weights_path: PathBuf) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        let dir = weights_path.parent()?;
        let mut hits: Vec<String> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let lower = name.to_lowercase();
                (lower.contains("mmproj") && lower.ends_with(".gguf")).then_some(name)
            })
            .collect();
        // Stable across calls: a directory listing is in whatever order the
        // filesystem feels like, and the note names the file.
        hits.sort();
        hits.into_iter().next()
    })
    .await
    .ok()
    .flatten()
}
