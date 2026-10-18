//! `GET /api/models/full`: every configured model of the five kinds, built
//! from [`dto::ModelsFull`] so the document and the answer are one type.

use axum::extract::State;
use axum::response::Response;
use lmgw_api_types as dto;

use super::ops_result;
use crate::modelinfo::licence;
use crate::ops::backends::to_json;
use crate::ops::mirror;
use crate::state::SharedState;

/// `GET /api/models/full`.
///
/// The typed answer goes out as the `Value` the op dispatchers send, not as
/// `Json(T)`: a `Value`'s keys are sorted, so the bytes are the same as when
/// the answer was built with `json!`.
pub(super) async fn models_full(State(st): State<SharedState>) -> Response {
    match models_view(&st).await {
        Ok(full) => ops_result(to_json(Ok(full))),
        Err(e) => ops_result(Err(e)),
    }
}

/// The model lists as the dashboard reads them. A row goes through
/// [`mirror`], which refuses a row its API type cannot hold whole.
async fn models_view(st: &SharedState) -> Result<dto::ModelsFull, String> {
    let snap = st.snapshot();
    let failed = |e: String| format!("failed to load model lists: {e}");
    let mut aliases: Vec<dto::AliasView> = snap
        .aliases
        .values()
        .map(|a| dto::AliasView {
            id: a.id,
            alias: a.alias.clone(),
            upstream_id: a.upstream_id,
            upstream_name: snap.upstreams.get(&a.upstream_id).map(|u| u.name.clone()),
            upstream_model_id: a.upstream_model_id.clone(),
            param_overrides: (&a.param_overrides).into(),
            enabled: a.enabled,
            capabilities_override: a.capabilities_override.clone(),
        })
        .collect();
    aliases.sort_by(|a, b| a.alias.cmp(&b.alias));
    // One cached header read per GGUF row for its licence
    // (`modelinfo::licence`): a warm call is a `stat` per row.
    let chat_dir = snap.settings.router.models_dir.clone();
    let aux_dir = snap.settings.aux_router.models_dir.clone();
    let mut local = Vec::with_capacity(snap.local_models.len());
    for m in &snap.local_models {
        local.push(dto::LocalModelView {
            public_name: snap.local_public_name(&m.model_id),
            model: mirror(m).map_err(failed)?,
            licence: licence::of_file(st, &chat_dir, &m.gguf_path).await,
        });
    }
    let mut aux = Vec::with_capacity(snap.aux_models.len());
    for m in &snap.aux_models {
        let (threads, source) = crate::runtime::aux::threads_in_effect(m, crate::host::cpu());
        aux.push(dto::AuxModelView {
            public_name: snap.aux_public_name(m),
            model: mirror(m).map_err(failed)?,
            licence: licence::of_file(st, &aux_dir, &m.gguf_path).await,
            threads_in_effect: threads,
            threads_source: source,
        });
    }
    let image = snap
        .image_models
        .iter()
        .map(|m| {
            Ok(dto::ImageModelView {
                public_name: snap.image_public_name(&m.model_id),
                model: mirror(m)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()
        .map_err(failed)?;
    let rows = crate::store::list_audio_models(&st.db)
        .await
        .map_err(|_| "failed to load model lists".to_string())?;
    // What each audio row is charged and why (realtime design §9.4): the
    // learned residency, or the on-disk size it stands in for. A plain loop
    // for the same reason as the candidate loop below; the on-disk size is
    // a memoized directory walk.
    let settings = &snap.settings.audio;
    let mut audio = Vec::with_capacity(rows.len());
    for m in &rows {
        let note = st.vram.audio_residency_note(st, &snap, m).await;
        let (threads, source) =
            crate::runtime::audio::threads_in_effect(m, settings, crate::host::cpu());
        audio.push(dto::AudioModelView {
            public_name: snap.audio_public_name(&m.model_id),
            model: mirror(m).map_err(failed)?,
            residency_note: Some(note),
            residency_charged_bytes: crate::vram::residency::learned(m, settings),
            runs_on: crate::runtime::audio::placement(m, settings)
                .as_str()
                .to_string(),
            threads_in_effect: threads,
            threads_source: source.as_str().to_string(),
        });
    }
    // One `candidates::derive` per row (candidate-aliases design §4.6) — a
    // cached GGUF read per candidate, warm calls free (see that function's
    // own cost note). A plain loop, not `join_all`: this is a desktop app for
    // one user, and a handful of aliases at most.
    //
    // The deferral count (§6) is one grouped query for every alias, read
    // once before the loop rather than once per alias
    // (`candidates::deferrals::deferrals_24h`'s own doc comment).
    let deferral_counts = crate::candidates::deferrals::deferrals_24h(&st.db)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("deferrals_24h query failed, showing zero for every alias: {e}");
            Default::default()
        });
    let mut candidate_aliases: Vec<dto::CandidateAliasView> = Vec::new();
    for c in snap.candidate_aliases.values() {
        let d = crate::candidates::derive::derive(st, &snap, c).await;
        candidate_aliases.push(dto::CandidateAliasView {
            id: c.id,
            alias: c.alias.clone(),
            candidates: c.candidates.clone(),
            background: c.background,
            fallback_mode: c.fallback_mode.as_str().to_string(),
            fallback: c.fallback.clone(),
            capabilities_disabled: c.capabilities_disabled.clone(),
            enabled: c.enabled,
            notes: c.notes.clone(),
            enabled_facets: d.enabled.names(),
            common_facets: d.common.names(),
            unsupported_by: d.unsupported_by,
            routable_candidates: d.routable,
            problems: d.problems,
            advisories: d.advisories,
            fallback_usable: d.fallback_usable,
            context_length: d.context_length,
            max_output_tokens: d.max_output_tokens,
            deferrals_24h: crate::candidates::deferrals::for_alias(&deferral_counts, &c.alias),
        });
    }
    candidate_aliases.sort_by(|a, b| a.alias.cmp(&b.alias));
    // The map the snapshot keeps them in has no order, so the answer fixes
    // one: upstreams by name, each one's hidden models by id.
    let mut passthrough: Vec<dto::PassthroughUpstream> = snap
        .upstreams
        .values()
        .filter(|u| u.enabled && u.expose_all)
        .map(|u| {
            let mut hidden: Vec<String> = snap
                .hidden_passthrough
                .iter()
                .filter(|(uid, _)| *uid == u.id)
                .map(|(_, model_id)| model_id.clone())
                .collect();
            hidden.sort();
            dto::PassthroughUpstream {
                upstream: u.name.clone(),
                id: u.id,
                prefix: u.prefix().to_string(),
                hidden,
            }
        })
        .collect();
    passthrough.sort_by(|a, b| a.upstream.cmp(&b.upstream));
    Ok(dto::ModelsFull {
        aliases,
        candidate_aliases,
        local,
        aux,
        audio,
        image,
        passthrough,
    })
}

/// The API shape of an alias's overrides. Destructured without `..`: a field
/// added to `ir::Params` stops compiling here until the API type has it too,
/// instead of being dropped from the answer.
impl From<&crate::ir::Params> for dto::Params {
    fn from(p: &crate::ir::Params) -> Self {
        let crate::ir::Params {
            temperature,
            top_p,
            top_k,
            min_p,
            repeat_penalty,
            max_tokens,
            presence_penalty,
            frequency_penalty,
            seed,
            stop,
            reasoning,
        } = p.clone();
        Self {
            temperature,
            top_p,
            top_k,
            min_p,
            repeat_penalty,
            max_tokens,
            presence_penalty,
            frequency_penalty,
            seed,
            reasoning: reasoning.as_ref().map(Into::into),
            stop,
        }
    }
}

/// The API shape of a reasoning control, destructured for the same reason.
impl From<&crate::ir::ReasoningControl> for dto::ReasoningControl {
    fn from(r: &crate::ir::ReasoningControl) -> Self {
        let crate::ir::ReasoningControl {
            enabled,
            effort,
            budget_tokens,
        } = r.clone();
        Self {
            enabled,
            effort,
            budget_tokens,
        }
    }
}
