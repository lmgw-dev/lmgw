//! `candidate_alias_set` (candidate-aliases design §4.1, §4.6): create,
//! update, delete, enable or disable a candidate alias — mirrors
//! [`super::model_set`]'s find-by-id-or-name and response shape, plus the
//! capability save-time rules the plain-alias patch has none of. `preview`
//! (UI phase, §6) runs the same validation and derivation over an unsaved
//! draft, without writing — see that function's own doc comment.
//!
//! A child module of [`crate::ops`] (like [`super::backends`]) so the parent
//! file only carries the `mod` line and the re-export; everything below
//! freely reaches the parent's private helpers (`opt`, `clear_names`,
//! `clear_has`, `require_all`, `candidate_alias_name_taken`, the fallback
//! save checks in [`super::fallback`]) through `super::`, exactly as if this code
//! still lived there.

use serde::Deserialize;

use lmgw_api_types as dto;

use crate::candidates::{self, FacetSet};
use crate::capabilities::exposed;
use crate::config::{CandidateAlias, FallbackRoute, HopKey, Snapshot};
use crate::detached::detached;
use crate::state::SharedState;
use crate::store::{self, NewCandidateAlias};

/// Sparse patch for a candidate alias. Every list-shaped field is flat text
/// (no nested objects, no arrays), comma/space/newline separated like `extra_run_args`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CandidateAliasPatch {
    pub action: String,
    pub id: Option<i64>,
    pub alias: Option<String>,
    /// Local chat model ids, first = primary, rest = alternates in
    /// preference order. On `update`, absent leaves the stored list
    /// unchanged; there is no `clear` for it — an alias may never have an
    /// empty list, so emptying it means `delete`.
    pub candidates: Option<String>,
    pub background: Option<bool>,
    /// The model or alias that answers for the whole alias when it cannot:
    /// the GPU hold, VRAM outside lmgw, background traffic deferred, or no
    /// usable candidate (openai egress design §14.2). A chat model: a local
    /// model id, a passthrough id or a plain alias; not a candidate alias.
    /// Chains are allowed; a loop is refused. `clear: "fallback"` removes it.
    pub fallback: Option<String>,
    /// Facet wire names switched off (`vision`, `audio`, `tool_calls`,
    /// `reasoning`, `structured_output`) — the **full** desired list, not a
    /// delta (same convention as `extra_run_args`: a save replaces it
    /// whole). Absent leaves the stored list unchanged; `clear=
    /// capabilities_disabled` resets it to empty, re-enabling every facet
    /// that is common to all candidates.
    pub capabilities_disabled: Option<String>,
    pub enabled: Option<bool>,
    pub notes: Option<String>,
    /// Field names to reset to unset, comma/space-separated. Only
    /// `capabilities_disabled`, `fallback` and `notes` are clearable here.
    pub clear: Option<String>,
}

/// The current row, by id or by name (case-insensitive) — same shape as
/// every other `find` closure in [`crate::ops`].
fn find(
    id: Option<i64>,
    alias: Option<String>,
    all: Vec<CandidateAlias>,
) -> Result<CandidateAlias, String> {
    if let Some(id) = id {
        return all
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| format!("no candidate alias with id {id}"));
    }
    let alias = alias.ok_or("this action requires id or alias")?;
    all.into_iter()
        .find(|c| c.alias.eq_ignore_ascii_case(&alias))
        .ok_or_else(|| format!("no candidate alias named '{alias}'"))
}

/// The save's fallback (openai egress design §14.2): the patch field over
/// `current`, checked when it changes — and the notes the save answers
/// with. `owner` is the alias after this save, `key` its identity now.
async fn settle_fallback(
    state: &SharedState,
    p: &CandidateAliasPatch,
    key: Option<HopKey>,
    name: &str,
    old_name: Option<String>,
    current: Option<String>,
) -> Result<(Option<String>, Vec<String>), String> {
    let fallback = super::fallback::patched(p.clear.as_deref(), &p.fallback, current.clone())?;
    let mut notes = Vec::new();
    if let (Some(f), true) = (&fallback, fallback != current) {
        let owner = super::fallback::Owner::typed(
            key,
            name.to_string(),
            super::fallback::ModelType::chat(),
        )
        .renamed_from(old_name);
        notes = super::fallback::validate(state, &owner, f).await?;
    }
    Ok((fallback, notes))
}

/// Every save-time refusal of §4.1/§4.6 in one place, and the resulting row
/// — everything but `id`, which the caller fills in (0 for a fresh `create`,
/// the existing row's on `update`).
///
/// `disabled` is this save's full, stated `capabilities_disabled` — the
/// owner's explicit switches only, resolved by the caller (an untouched
/// `update` carries the row's own stored value forward; a fresh `create`
/// that never mentioned the field is `FacetSet::EMPTY`, not a materialized
/// complement — §12 entry 49). `previously_saved` is the row's last stored
/// `(capabilities_enabled, capabilities_disabled)`, `None` only on a fresh
/// `create`. See [`candidates::validate::resolve_enabled`] for what the two
/// together decide.
///
/// `derive` is run once against a **probe** row (this save's shape, with a
/// placeholder empty `capabilities_disabled`/`capabilities_enabled` — the
/// probe's own capability columns feed nothing back into `derive`'s `common`/
/// `unsupported_by`, which depend only on what each candidate supports)
/// purely to read those two fields. The *final* enabled/disabled pair is only
/// known after [`candidates::validate::resolve_enabled`], so the fallback's
/// capability check runs separately, against that final `enabled` — not
/// against whatever `derive` computed for the probe's placeholder.
#[allow(clippy::too_many_arguments)]
async fn build_and_validate(
    state: &SharedState,
    snap: &Snapshot,
    alias: String,
    candidates_list: Vec<String>,
    background: bool,
    fallback: Option<String>,
    disabled: FacetSet,
    previously_saved: Option<(FacetSet, FacetSet)>,
    notes: String,
    row_enabled: bool,
) -> Result<CandidateAlias, String> {
    candidates::validate::refuse_empty_or_duplicate(&candidates_list)?;
    for id in &candidates_list {
        let row = snap
            .local_models
            .iter()
            .find(|m| &m.model_id == id)
            .ok_or_else(|| format!("'{id}' is not a local chat model"))?;
        if !row.enabled {
            return Err(format!("'{id}' is a local chat model but disabled"));
        }
    }

    let probe = CandidateAlias {
        id: 0,
        alias: alias.clone(),
        candidates: candidates_list,
        background,
        fallback,
        capabilities_disabled: Vec::new(),
        capabilities_enabled: Vec::new(),
        enabled: true,
        notes,
    };
    let derived = candidates::derive::derive(state, snap, &probe).await;
    let (enabled_facets, disabled_facets) = candidates::validate::resolve_enabled(
        disabled,
        previously_saved,
        derived.common,
        &derived.unsupported_by,
    )?;
    // A fallback that lacks a facet the alias enables is no reason to refuse
    // the save (changed 2026-10-06, the owner's ruling: a configured
    // fallback is always used): it answers with what it cannot take
    // degraded, which `derive`'s advisories say.

    Ok(CandidateAlias {
        capabilities_disabled: disabled_facets.names(),
        capabilities_enabled: enabled_facets.names(),
        enabled: row_enabled,
        ..probe
    })
}

fn to_new(row: &CandidateAlias) -> NewCandidateAlias {
    NewCandidateAlias {
        alias: row.alias.clone(),
        candidates: row.candidates.clone(),
        background: row.background,
        fallback: row.fallback.clone(),
        capabilities_disabled: row.capabilities_disabled.clone(),
        capabilities_enabled: row.capabilities_enabled.clone(),
        enabled: row.enabled,
        notes: row.notes.clone(),
    }
}

/// The response every write below answers with: the saved row's id plus a
/// **fresh** re-derivation (post-reload) of what it means right now — the
/// same shape the editor and `lmgw__models` read, so a caller never has to
/// make a second call just to see whether the alias it just saved is
/// actually usable.
async fn saved_view(
    state: &SharedState,
    id: i64,
    alias: &str,
    message: String,
    mut fallback_notes: Vec<String>,
) -> Result<dto::CandidateAliasSaved, String> {
    let snap = state.reload_snapshot().await.map_err(|e| e.to_string())?;
    let Some(row) = snap.candidate_aliases.get(&alias.to_lowercase()) else {
        // Deleted by a concurrent call between the write and this read —
        // report success on the write itself rather than erroring on a
        // vanished row.
        return Ok(dto::CandidateAliasSaved {
            ok: true,
            id,
            message,
            fallback_notes,
            ..Default::default()
        });
    };
    if let Some(f) = row.fallback.as_deref() {
        fallback_notes.extend(super::fallback::facet_notes(state, &row.alias, f).await);
    }
    let derived = candidates::derive::derive(state, &snap, row).await;
    Ok(dto::CandidateAliasSaved {
        ok: true,
        id,
        enabled_facets: Some(derived.enabled.names()),
        common_facets: Some(derived.common.names()),
        routable_candidates: Some(derived.routable),
        problems: Some(derived.problems),
        advisories: Some(derived.advisories),
        fallback_usable: Some(derived.fallback_usable),
        context_length: Some(derived.context_length),
        max_output_tokens: Some(derived.max_output_tokens),
        message,
        fallback_notes,
    })
}

/// `candidate_alias_set action=preview` (UI phase, candidate-aliases design
/// §6): the alias editor's live "what would saving this draft mean right
/// now" — the same validation and derivation `create`/`update` run, over a
/// patch that is never written to `store`.
///
/// Unlike a real save, no single refusal aborts the whole answer: a draft is
/// the normal state of an editor no one has clicked Save on yet (an empty
/// candidate list while it is still being built, a facet a just-added
/// candidate does not support). Every check `build_and_validate` would `?`
/// out of instead only sets `error` the first time (further ones are
/// dropped — one message is what the editor shows), and derivation keeps
/// going with the most useful stand-in it can: `common`/`unsupported_by` are
/// unaffected by any of this (they only ever read the candidate rows
/// themselves), and a `resolve_enabled` refusal falls back to `common` minus
/// whatever the draft is explicitly asking to disable, so the checkboxes the
/// editor renders still mean something.
///
/// `addable` is the one field with no equivalent in [`saved_view`]: every
/// enabled local chat model not already in the draft's `candidates`, and
/// which of the facets it would need to keep it lacks — the "add candidate"
/// list's filter, computed once here (same cached GGUF reads `derive` itself
/// already pays for its own candidates) instead of one request per row a
/// picker would otherwise cost.
async fn preview(
    state: &SharedState,
    snap: &Snapshot,
    p: &CandidateAliasPatch,
) -> dto::CandidateAliasPreview {
    // The row this draft edits, when it names one — an untouched field then
    // previews the same "carried forward" value `update` would use. Nothing
    // found (a fresh draft, or a name not saved yet) previews like `create`.
    let current = if p.id.is_some() || super::opt(&p.alias).is_some() {
        store::list_candidate_aliases(&state.db)
            .await
            .unwrap_or_default()
            .into_iter()
            .find(|c| {
                p.id.is_some_and(|id| id == c.id)
                    || super::opt(&p.alias).is_some_and(|a| c.alias.eq_ignore_ascii_case(&a))
            })
    } else {
        None
    };

    let mut error: Option<String> = None;

    let candidates_list: Vec<String> = match p.candidates.as_deref() {
        Some(raw) => super::clear_names(Some(raw)).map(String::from).collect(),
        None => current
            .as_ref()
            .map(|c| c.candidates.clone())
            .unwrap_or_default(),
    };
    if let Err(e) = candidates::validate::refuse_empty_or_duplicate(&candidates_list) {
        error.get_or_insert(e);
    }

    let background = p
        .background
        .unwrap_or_else(|| current.as_ref().is_some_and(|c| c.background));
    let preview_name = super::opt(&p.alias)
        .or_else(|| current.as_ref().map(|c| c.alias.clone()))
        .unwrap_or_default();
    let fallback = match settle_fallback(
        state,
        p,
        current.as_ref().map(|c| HopKey::Candidate(c.id)),
        &preview_name,
        current.as_ref().map(|c| c.alias.clone()),
        current.as_ref().and_then(|c| c.fallback.clone()),
    )
    .await
    {
        Ok((v, _)) => v,
        Err(e) => {
            error.get_or_insert(e);
            None
        }
    };

    // The row's last stored (enabled, disabled) — `None` only for a brand-new
    // draft, exactly the same "nothing decided yet" case `create` itself
    // leaves lenient (`resolve_enabled`'s own doc comment).
    let previously_saved = current.as_ref().map(|cur| {
        (
            FacetSet::from_names(&cur.capabilities_enabled).unwrap_or_default(),
            FacetSet::from_names(&cur.capabilities_disabled).unwrap_or_default(),
        )
    });
    // This save's stated `disabled` — an untouched field on an existing row
    // carries its stored value forward; a brand-new draft that never touched
    // the field previews with nothing disabled, so an empty "New candidate
    // alias" form previews as "everything common, enabled".
    let disabled = if let Some(cur) = &current {
        if super::clear_has(p.clear.as_deref(), "capabilities_disabled") {
            FacetSet::EMPTY
        } else {
            match p.capabilities_disabled.as_deref() {
                Some(raw) => match FacetSet::from_names(super::clear_names(Some(raw))) {
                    Ok(f) => f,
                    Err(e) => {
                        error.get_or_insert(e);
                        FacetSet::EMPTY
                    }
                },
                None => FacetSet::from_names(&cur.capabilities_disabled).unwrap_or_default(),
            }
        }
    } else {
        match p.capabilities_disabled.as_deref() {
            Some(raw) => match FacetSet::from_names(super::clear_names(Some(raw))) {
                Ok(f) => f,
                Err(e) => {
                    error.get_or_insert(e);
                    FacetSet::EMPTY
                }
            },
            None => FacetSet::EMPTY,
        }
    };

    let notes = if super::clear_has(p.clear.as_deref(), "notes") {
        String::new()
    } else {
        super::opt(&p.notes).unwrap_or_else(|| {
            current
                .as_ref()
                .map_or_else(String::new, |c| c.notes.clone())
        })
    };
    let alias_name = super::opt(&p.alias)
        .or_else(|| current.as_ref().map(|c| c.alias.clone()))
        .unwrap_or_default();

    let probe = CandidateAlias {
        id: current.as_ref().map_or(0, |c| c.id),
        alias: alias_name,
        candidates: candidates_list.clone(),
        background,
        fallback,
        capabilities_disabled: Vec::new(),
        capabilities_enabled: Vec::new(),
        enabled: true,
        notes,
    };
    let derived = candidates::derive::derive(state, snap, &probe).await;

    let enabled_result = candidates::validate::resolve_enabled(
        disabled,
        previously_saved,
        derived.common,
        &derived.unsupported_by,
    );
    let (enabled, fallback_usable) = match enabled_result {
        Ok((enabled, _disabled)) => {
            // One that lacks an enabled facet is usable all the same
            // (changed 2026-10-06); `derive`'s advisories name what it lacks.
            let usable = matches!(
                snap.fallback_named(probe.fallback.as_deref()),
                FallbackRoute::Usable { .. }
            );
            (enabled, usable)
        }
        Err(e) => {
            error.get_or_insert(e);
            (derived.common.minus(disabled), derived.fallback_usable)
        }
    };

    // Every enabled local chat model not already a candidate, and which of
    // `enabled` it would be missing if added right now.
    let mut addable = Vec::new();
    for m in snap.local_models.iter().filter(|m| m.enabled) {
        if candidates_list.iter().any(|id| id == &m.model_id) {
            continue;
        }
        let entry = exposed::local_row_entry(state, snap, m, m.model_id.clone()).await;
        let missing: Vec<String> = enabled
            .iter()
            .filter(|f| {
                !entry
                    .capabilities
                    .as_ref()
                    .is_some_and(|c| candidates::supports(c, *f))
            })
            .map(|f| f.as_str().to_string())
            .collect();
        addable.push(dto::AddableCandidate {
            id: m.model_id.clone(),
            missing,
        });
    }

    dto::CandidateAliasPreview {
        ok: error.is_none(),
        enabled_facets: enabled.names(),
        common_facets: derived.common.names(),
        unsupported_by: derived.unsupported_by,
        routable_candidates: derived.routable,
        problems: derived.problems,
        advisories: derived.advisories,
        fallback_usable,
        context_length: derived.context_length,
        max_output_tokens: derived.max_output_tokens,
        addable,
        error,
    }
}

pub async fn candidate_alias_set(
    state: &SharedState,
    p: CandidateAliasPatch,
) -> Result<dto::CandidateAliasAnswer, String> {
    // From the snapshot the checks read to the commit (§14.2).
    let routing = state.routing_write().await;
    let snap = state.snapshot();

    match p.action.as_str() {
        "preview" => Ok(dto::CandidateAliasAnswer::Preview(
            preview(state, &snap, &p).await,
        )),
        "create" => {
            super::require_all(&[
                ("alias", super::opt(&p.alias).is_some()),
                ("candidates", super::opt(&p.candidates).is_some()),
            ])?;
            let alias = super::opt(&p.alias).ok_or("alias must not be empty")?;
            if let Some(what) = super::candidate_alias_name_taken(&snap, &alias, None) {
                return Err(format!(
                    "'{alias}' is already the name of {what} — pick another name"
                ));
            }
            let candidates_list: Vec<String> = super::clear_names(p.candidates.as_deref())
                .map(String::from)
                .collect();
            let background = p.background.unwrap_or(false);
            let (fallback, fallback_notes) =
                settle_fallback(state, &p, None, &alias, None, None).await?;
            // Nothing stored before: a patch that never mentioned the field
            // disables nothing, and `build_and_validate`'s `previously_saved
            // : None` below means none of it can be refused either
            // (`resolve_enabled`'s own doc comment) — an uncommon facet is
            // simply left off.
            let disabled = match p.capabilities_disabled.as_deref() {
                Some(raw) => FacetSet::from_names(super::clear_names(Some(raw)))?,
                None => FacetSet::EMPTY,
            };
            let notes = super::opt(&p.notes).unwrap_or_default();
            let row = build_and_validate(
                state,
                &snap,
                alias.clone(),
                candidates_list,
                background,
                fallback,
                disabled,
                None,
                notes,
                p.enabled.unwrap_or(true),
            )
            .await?;
            // A fallback that holds the new name reaches this alias from now
            // on, and a candidate alias cannot be one: a dry run (§14.2).
            let mut tx = store::begin_write(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            let dry = store::insert_candidate_alias_in(&mut tx, &to_new(&row))
                .await
                .map_err(|e| e.to_string())?;
            let own = (store::FallbackTable::CandidateAliases, dry);
            super::fallback::dry_run(
                state,
                &snap,
                tx,
                &[super::fallback::Touch::Row(own.0, dry)],
                Some(own),
            )
            .await?;
            // The write and the reload after it are a task of their own
            // (*changed 2026-10-24*, see [`crate::detached::detached`]): a
            // dropped request would leave the snapshot without the new alias.
            let st = state.clone();
            detached("creating the candidate alias", async move {
                // Until the view below has reloaded the snapshot.
                let _routing = routing;
                let id = store::insert_candidate_alias(&st.db, &to_new(&row))
                    .await
                    .map_err(|e| e.to_string())?;
                saved_view(
                    &st,
                    id,
                    &alias,
                    format!("candidate alias '{alias}' created"),
                    fallback_notes,
                )
                .await
                .map(dto::CandidateAliasAnswer::Saved)
            })
            .await
            .map_err(|r| r.message)?
        }
        "update" | "enable" | "disable" => {
            let all = store::list_candidate_aliases(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            let cur = find(p.id, super::opt(&p.alias), all)?;
            let row_enabled = match p.action.as_str() {
                "enable" => true,
                "disable" => false,
                _ => p.enabled.unwrap_or(cur.enabled),
            };
            // `alias` doubles as the lookup key, so it only renames when the
            // caller also passed an explicit id — same convention as
            // `model_set`.
            let new_alias = match (p.id, super::opt(&p.alias)) {
                (Some(_), Some(a)) => a,
                _ => cur.alias.clone(),
            };
            if !new_alias.eq_ignore_ascii_case(&cur.alias) {
                if let Some(what) =
                    super::candidate_alias_name_taken(&snap, &new_alias, Some(cur.id))
                {
                    return Err(format!(
                        "'{new_alias}' is already the name of {what} — pick another name"
                    ));
                }
            }
            if super::clear_has(p.clear.as_deref(), "candidates") {
                return Err(
                    "candidates cannot be cleared to empty — delete the alias instead \
                     (candidate_alias_set action=delete)"
                        .to_string(),
                );
            }
            let candidates_list = match p.candidates.as_deref() {
                Some(raw) => super::clear_names(Some(raw)).map(String::from).collect(),
                None => cur.candidates.clone(),
            };
            let background = p.background.unwrap_or(cur.background);
            let key = HopKey::Candidate(cur.id);
            let (fallback, mut fallback_notes) = settle_fallback(
                state,
                &p,
                Some(key.clone()),
                &new_alias,
                Some(cur.alias.clone()),
                cur.fallback.clone(),
            )
            .await?;
            // A rename never breaks a chain (decision 27).
            let rewrites = match new_alias != cur.alias {
                true => super::fallback::rename_rewrites(
                    &snap,
                    &[(key.clone(), new_alias.clone())],
                    Some(&key),
                ),
                false => super::fallback::Rewrites::default(),
            };
            fallback_notes.extend(rewrites.notes.iter().cloned());
            // The row's own stored (enabled, disabled) — `previously_saved`
            // below, held to account (`resolve_enabled`'s own doc comment):
            // an untouched field carries `disabled` forward unchanged, but
            // the save is still re-checked against today's `common`, which
            // is exactly what catches a candidate that dropped an enabled
            // facet.
            let prev_enabled = FacetSet::from_names(&cur.capabilities_enabled)?;
            let prev_disabled = FacetSet::from_names(&cur.capabilities_disabled)?;
            let disabled = if super::clear_has(p.clear.as_deref(), "capabilities_disabled") {
                FacetSet::EMPTY
            } else {
                match p.capabilities_disabled.as_deref() {
                    Some(raw) => FacetSet::from_names(super::clear_names(Some(raw)))?,
                    None => prev_disabled,
                }
            };
            let notes = if super::clear_has(p.clear.as_deref(), "notes") {
                String::new()
            } else {
                super::opt(&p.notes).unwrap_or_else(|| cur.notes.clone())
            };
            let mut row = build_and_validate(
                state,
                &snap,
                new_alias.clone(),
                candidates_list,
                background,
                fallback,
                disabled,
                Some((prev_enabled, prev_disabled)),
                notes,
                row_enabled,
            )
            .await?;
            row.id = cur.id;
            // What a rename, an enable or a disable does to the fallbacks
            // naming this alias, in a dry run (§14.2).
            let mut tx = store::begin_write(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            store::update_candidate_alias_in(&mut tx, cur.id, &to_new(&row))
                .await
                .map_err(|e| e.to_string())?;
            store::rewrite_fallbacks_in(&mut tx, &rewrites.rows)
                .await
                .map_err(|e| e.to_string())?;
            let own = (store::FallbackTable::CandidateAliases, cur.id);
            super::fallback::dry_run(
                state,
                &snap,
                tx,
                &[super::fallback::Touch::Row(own.0, own.1)],
                (row.fallback.is_some() && row.fallback != cur.fallback).then_some(own),
            )
            .await?;
            let message = format!(
                "candidate alias '{new_alias}' {}",
                super::saved_verb(&p.action)
            );
            // The write and the reload after it are a task of their own
            // (*changed 2026-10-24*, see [`crate::detached::detached`]): a
            // dropped request would leave the snapshot serving the old alias.
            let st = state.clone();
            let id = cur.id;
            detached("updating the candidate alias", async move {
                // Until the view below has reloaded the snapshot.
                let _routing = routing;
                let mut tx = store::begin_write(&st.db)
                    .await
                    .map_err(|e| e.to_string())?;
                store::update_candidate_alias_in(&mut tx, id, &to_new(&row))
                    .await
                    .map_err(|e| e.to_string())?;
                store::rewrite_fallbacks_in(&mut tx, &rewrites.rows)
                    .await
                    .map_err(|e| e.to_string())?;
                tx.commit().await.map_err(|e| e.to_string())?;
                saved_view(&st, id, &new_alias, message, fallback_notes)
                    .await
                    .map(dto::CandidateAliasAnswer::Saved)
            })
            .await
            .map_err(|r| r.message)?
        }
        "delete" => {
            let all = store::list_candidate_aliases(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            let cur = find(p.id, super::opt(&p.alias), all)?;
            let fallback_notes = super::fallback::delete_notes(&snap, &HopKey::Candidate(cur.id));
            // The delete and the reload are a task of their own (*changed
            // 2026-10-24*, see [`crate::detached::detached`]): a dropped
            // request would leave the snapshot serving the deleted alias.
            let st = state.clone();
            detached("deleting the candidate alias", async move {
                store::delete_candidate_alias(&st.db, cur.id)
                    .await
                    .map_err(|e| e.to_string())?;
                st.reload_snapshot_releasing(routing)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(dto::CandidateAliasAnswer::Saved(dto::CandidateAliasSaved {
                    ok: true,
                    id: cur.id,
                    message: format!("candidate alias '{}' deleted", cur.alias),
                    fallback_notes,
                    ..Default::default()
                }))
            })
            .await
            .map_err(|r| r.message)?
        }
        other => Err(format!(
            "unknown action '{other}' (create|update|delete|enable|disable|preview)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    /// A row deleted between the write and the read-back is still a success of
    /// the write: `{ok, id, message}`, with none of the derived keys (and not
    /// `null` ones).
    #[tokio::test]
    async fn a_row_that_vanished_before_the_read_back_answers_the_write_alone() {
        let state = AppState::init_for_tests().await.unwrap();
        let answer = saved_view(
            &state,
            7,
            "ghost",
            "candidate alias 'ghost' created".into(),
            Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            crate::ops::backends::to_json(Ok(answer)).unwrap(),
            serde_json::json!({
                "ok": true,
                "id": 7,
                "message": "candidate alias 'ghost' created",
            })
        );
    }
}
