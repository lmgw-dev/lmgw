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
//! `clear_has`, `require_all`, `validate_fallback_alias`,
//! `candidate_alias_name_taken`) through `super::`, exactly as if this code
//! still lived there.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::candidates::{self, FacetSet};
use crate::capabilities::exposed;
use crate::config::{CandidateAlias, FallbackRoute, HoldFallbackMode, Snapshot};
use crate::state::SharedState;
use crate::store::{self, NewCandidateAlias};

/// Sparse patch for a candidate alias. Every list-shaped field is flat text
/// (module doc of [`crate::mcp::selfadmin`]: no nested objects, no arrays),
/// comma/space/newline separated like `extra_run_args`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CandidateAliasPatch {
    pub action: String,
    pub id: Option<i64>,
    pub alias: Option<String>,
    /// Local chat model ids, first = primary, rest = alternates in
    /// preference order. On `update`, absent leaves the stored list
    /// unchanged; there is no `clear` for it — an alias may never have an
    /// empty list (§4.1), so emptying it means `delete`.
    pub candidates: Option<String>,
    pub background: Option<bool>,
    pub fallback_mode: Option<HoldFallbackMode>,
    /// The alias `fallback_mode = alias` routes to. Required (here or
    /// already on the row) when that mode is set; validated like every
    /// other fallback (must resolve, must not be local, must not be another
    /// candidate alias).
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
    /// `capabilities_disabled` and `notes` are clearable here.
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

/// `fallback_mode`/`fallback` resolution, the candidate-alias twin of
/// `super::resolve_hold_fallback_text` — not reused directly because that
/// one's `clear` name is `"hold_fallback"` and this column has no `hold_`
/// prefix (§4.1's own naming).
fn resolve_fallback(
    snap: &Snapshot,
    p: &CandidateAliasPatch,
    current: (HoldFallbackMode, Option<String>),
) -> Result<(HoldFallbackMode, Option<String>), String> {
    if super::clear_has(p.clear.as_deref(), "fallback") {
        return Ok((HoldFallbackMode::Inherit, None));
    }
    let (cur_mode, cur_alias) = current;
    let mode = p.fallback_mode.unwrap_or(cur_mode);
    match mode {
        HoldFallbackMode::Alias => {
            let alias = super::opt(&p.fallback).or(cur_alias).ok_or(
                "fallback_mode 'alias' requires fallback (the alias this candidate alias falls \
                 back to)",
            )?;
            super::validate_fallback_alias(snap, &alias).map_err(|e| format!("fallback: {e}"))?;
            Ok((mode, Some(alias)))
        }
        HoldFallbackMode::Inherit | HoldFallbackMode::None => Ok((mode, None)),
    }
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
    fallback_mode: HoldFallbackMode,
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
        fallback_mode,
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

    if let crate::config::FallbackRoute::Usable { alias: fb, .. } = snap.alias_fallback(&probe) {
        if let Err(f) = candidates::derive::fallback_supports(state, &fb, enabled_facets).await {
            return Err(format!(
                "fallback '{fb}' does not support {} — disable that facet, or pick a fallback \
                 that supports it",
                f.as_str()
            ));
        }
    }

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
        fallback_mode: row.fallback_mode,
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
) -> Result<Value, String> {
    let snap = state.reload_snapshot().await.map_err(|e| e.to_string())?;
    let Some(row) = snap.candidate_aliases.get(&alias.to_lowercase()) else {
        // Deleted by a concurrent call between the write and this read —
        // report success on the write itself rather than erroring on a
        // vanished row.
        return Ok(json!({ "ok": true, "id": id, "message": message }));
    };
    let derived = candidates::derive::derive(state, &snap, row).await;
    Ok(json!({
        "ok": true,
        "id": id,
        "enabled_facets": derived.enabled.names(),
        "common_facets": derived.common.names(),
        "routable": derived.routable,
        "problems": derived.problems,
        "advisories": derived.advisories,
        "fallback_usable": derived.fallback_usable,
        "context_length": derived.context_length,
        "max_output_tokens": derived.max_output_tokens,
        "message": message,
    }))
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
async fn preview(state: &SharedState, snap: &Snapshot, p: &CandidateAliasPatch) -> Value {
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
    let base_fallback = current
        .as_ref()
        .map(|c| (c.fallback_mode, c.fallback.clone()))
        .unwrap_or((HoldFallbackMode::Inherit, None));
    let (fallback_mode, fallback) = match resolve_fallback(snap, p, base_fallback) {
        Ok(v) => v,
        Err(e) => {
            error.get_or_insert(e);
            (HoldFallbackMode::Inherit, None)
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
        fallback_mode,
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
            let usable = match snap.alias_fallback(&probe) {
                FallbackRoute::Usable { alias: fb, .. } => {
                    match candidates::derive::fallback_supports(state, &fb, enabled).await {
                        Ok(()) => true,
                        Err(f) => {
                            error.get_or_insert(format!(
                                "fallback '{fb}' does not support {} — disable that facet, or \
                                 pick a fallback that supports it",
                                f.as_str()
                            ));
                            false
                        }
                    }
                }
                _ => false,
            };
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
        addable.push(json!({ "id": m.model_id, "missing": missing }));
    }

    json!({
        "ok": error.is_none(),
        "enabled_facets": enabled.names(),
        "common_facets": derived.common.names(),
        "unsupported_by": derived.unsupported_by,
        "routable": derived.routable,
        "problems": derived.problems,
        "advisories": derived.advisories,
        "fallback_usable": fallback_usable,
        "context_length": derived.context_length,
        "max_output_tokens": derived.max_output_tokens,
        "addable": addable,
        "error": error,
    })
}

pub async fn candidate_alias_set(
    state: &SharedState,
    p: CandidateAliasPatch,
) -> Result<Value, String> {
    let snap = state.snapshot();

    match p.action.as_str() {
        "preview" => Ok(preview(state, &snap, &p).await),
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
            let (fallback_mode, fallback) =
                resolve_fallback(&snap, &p, (HoldFallbackMode::Inherit, None))?;
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
                fallback_mode,
                fallback,
                disabled,
                None,
                notes,
                p.enabled.unwrap_or(true),
            )
            .await?;
            let id = store::insert_candidate_alias(&state.db, &to_new(&row))
                .await
                .map_err(|e| e.to_string())?;
            saved_view(
                state,
                id,
                &alias,
                format!("candidate alias '{alias}' created"),
            )
            .await
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
            let (fallback_mode, fallback) =
                resolve_fallback(&snap, &p, (cur.fallback_mode, cur.fallback.clone()))?;
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
                fallback_mode,
                fallback,
                disabled,
                Some((prev_enabled, prev_disabled)),
                notes,
                row_enabled,
            )
            .await?;
            row.id = cur.id;
            store::update_candidate_alias(&state.db, cur.id, &to_new(&row))
                .await
                .map_err(|e| e.to_string())?;
            saved_view(
                state,
                cur.id,
                &new_alias,
                format!("candidate alias '{new_alias}' updated"),
            )
            .await
        }
        "delete" => {
            let all = store::list_candidate_aliases(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            let cur = find(p.id, super::opt(&p.alias), all)?;
            store::delete_candidate_alias(&state.db, cur.id)
                .await
                .map_err(|e| e.to_string())?;
            state.reload_snapshot().await.map_err(|e| e.to_string())?;
            Ok(json!({
                "ok": true, "id": cur.id,
                "message": format!("candidate alias '{}' deleted", cur.alias),
            }))
        }
        other => Err(format!(
            "unknown action '{other}' (create|update|delete|enable|disable|preview)"
        )),
    }
}
