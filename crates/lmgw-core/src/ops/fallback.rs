//! One fallback per model and per alias (openai egress design §14.2,
//! decisions 25–29): the patch field every setter takes, the save checks it
//! runs before a fallback is written, the notes the save answers with, and
//! what a rename or a delete does to the other rows' fallbacks.
//!
//! The six setters (`local_model_set`, `aux_model_set`, `audio_model_set`,
//! `image_model_set`, `candidate_alias_set`, `model_set`) and the dashboard's
//! alias routes all go through [`patched`] and [`validate`], so "what may a
//! fallback name" has one answer. A write that changes what *another* row's
//! fallback points at (a create, a rename, a retarget, a kind, task, prefix
//! or enable change, an upstream's) is checked for those rows too, by a dry
//! run ([`referrers`]). The use-time twin is the gate's walk, which
//! re-validates every hop: rows and aliases change after a save.

mod first_start;
mod referrers;
mod rename;
mod types;

pub(crate) use first_start::first_start_type_check;
pub(crate) use referrers::{dry_run, upstream_delete_notes, Touch};
pub(crate) use rename::{prefix_rewrites, rename_rewrites, upstream_prefix_rewrites, Rewrites};
pub(crate) use types::{facts_of, Facts, ModelType};

use crate::config::{HopKey, Snapshot};
use crate::state::SharedState;

use super::clear_has;

/// The model or alias a setter writes a fallback on, as far as the save
/// checks need it.
#[derive(Debug, Clone)]
pub(crate) struct Owner {
    /// Its identity in the snapshot before the write; `None` on a create.
    pub(crate) key: Option<HopKey>,
    /// The name a client reaches it by after the write.
    pub(crate) name: String,
    /// The name it had before a rename, when this write renames it.
    pub(crate) old_name: Option<String>,
    /// Its type (and facets, where known): §14.2 item 4.
    pub(crate) facts: Facts,
    /// A cloud alias's upstream row, for the same-upstream note (item 5).
    pub(crate) upstream: Option<i64>,
}

impl Owner {
    /// A row or alias of a known type.
    pub(crate) fn typed(key: Option<HopKey>, name: String, ty: ModelType) -> Self {
        Self {
            key,
            name,
            old_name: None,
            facts: Facts::of_type(ty),
            upstream: None,
        }
    }

    /// With the name it had before this write renames it.
    pub(crate) fn renamed_from(mut self, old: Option<String>) -> Self {
        self.old_name = old.filter(|o| *o != self.name);
        self
    }

    /// Whether `name` reaches this owner: its name before or after the
    /// write, or a name that resolves to its identity now.
    fn is(&self, snap: &Snapshot, name: &str) -> bool {
        name == self.name
            || self.old_name.as_deref() == Some(name)
            || (self.key.is_some() && snap.any_hop_key(name) == self.key)
    }
}

/// A plain alias of `upstream_model` on `upstream_id` as an owner (§14.2
/// item 4): its type is what the alias would publish — its upstream's
/// catalog entry, or the local row it points at, with its own capability
/// override on top — and it carries its upstream for the same-upstream note.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn alias_owner(
    state: &SharedState,
    snap: &Snapshot,
    key: Option<HopKey>,
    name: &str,
    old_name: Option<String>,
    upstream_id: i64,
    upstream_model: &str,
    overrides: &crate::ir::Params,
    capabilities_override: Option<&serde_json::Value>,
) -> Owner {
    let caps = match snap.upstreams.get(&upstream_id) {
        Some(u) => {
            crate::capabilities::exposed::alias_capabilities(
                state,
                snap,
                u,
                upstream_model,
                overrides,
                capabilities_override,
            )
            .await
        }
        None => None,
    };
    Owner {
        key,
        name: name.to_string(),
        old_name: old_name.filter(|o| o != name),
        facts: Facts::from_capabilities(caps.as_ref()),
        upstream: Some(upstream_id),
    }
}

/// A plain alias's save, as far as its fallback goes (§14.2): the value it
/// writes, the notes its answer carries so far, the references its rename
/// rewrites, and whether the fallback changed (the facet notes follow the
/// write, [`facet_notes`]).
#[derive(Debug, Default)]
pub(crate) struct AliasSave {
    pub(crate) fallback: Option<String>,
    pub(crate) notes: Vec<String>,
    pub(crate) rewrites: Rewrites,
    pub(crate) changed: bool,
    /// Whether [`validate`] checked the fallback this save writes (the dry
    /// run's `own`, [`dry_run`]).
    pub(crate) checked: bool,
}

/// The fallback side of a plain alias's create (`cur` `None`) or update,
/// for `model_set` and the dashboard's alias routes alike: the patch field
/// over the stored one, checked when it changes or when what the alias
/// points at does (its type is the target's), and the rewrites a rename
/// makes.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn settle_alias(
    state: &SharedState,
    snap: &Snapshot,
    cur: Option<&crate::config::ModelAlias>,
    name: &str,
    upstream_id: i64,
    upstream_model: &str,
    overrides: &crate::ir::Params,
    capabilities_override: Option<&serde_json::Value>,
    clear: Option<&str>,
    field: &Option<String>,
) -> Result<AliasSave, String> {
    let current = cur.and_then(|c| c.fallback.clone());
    let fallback = patched(clear, field, current.clone())?;
    let changed = fallback != current;
    let retyped = cur.is_some_and(|c| {
        c.upstream_id != upstream_id
            || c.upstream_model_id != upstream_model
            || c.capabilities_override.as_ref() != capabilities_override
    });
    let key = cur.map(|c| HopKey::Alias(c.id));
    let mut notes = Vec::new();
    let checked = fallback.is_some() && (changed || retyped);
    if let (Some(f), true) = (&fallback, changed || retyped) {
        let owner = alias_owner(
            state,
            snap,
            key.clone(),
            name,
            cur.map(|c| c.alias.clone()),
            upstream_id,
            upstream_model,
            overrides,
            capabilities_override,
        )
        .await;
        notes = validate(state, &owner, f).await?;
    }
    let rewrites = match (cur, &key) {
        (Some(c), Some(k)) if c.alias != name => {
            rename_rewrites(snap, &[(k.clone(), name.to_string())], Some(k))
        }
        _ => Rewrites::default(),
    };
    notes.extend(rewrites.notes.iter().cloned());
    Ok(AliasSave {
        fallback,
        notes,
        rewrites,
        changed,
        checked,
    })
}

/// The fallback a create or an update writes (§14.2): `clear: "fallback"`
/// removes it, `fallback: "<name>"` sets it (trimmed, as written: only the
/// target's own spelling resolves), and a patch without either keeps
/// `current`. An empty `fallback` is refused, naming `clear`: an empty
/// string is "not supplied" in every other patch field (`ops::opt`), so
/// taking it as "none" here would be the one place it is not.
pub(crate) fn patched(
    clear: Option<&str>,
    field: &Option<String>,
    current: Option<String>,
) -> Result<Option<String>, String> {
    if clear_has(clear, "fallback") {
        return Ok(None);
    }
    match field.as_deref().map(str::trim) {
        None => Ok(current),
        Some("") => Err(
            "fallback: an empty name names no model; to remove the fallback, send clear: \
             \"fallback\""
                .into(),
        ),
        Some(name) => Ok(Some(name.to_string())),
    }
}

/// The save checks for `fallback` on `owner` (§14.2), in order: it
/// resolves, it is not a candidate alias, it closes no loop, and it is the
/// owner's type of model. `Ok` carries the notes the save answers with —
/// a type lmgw could not check, the same upstream — and `Err` the refusal.
///
/// A decision row's fallback must also answer `/v1/systemone`: that one is
/// `ops::aux_model`'s, which runs the route guard.
pub(crate) async fn validate(
    state: &SharedState,
    owner: &Owner,
    fallback: &str,
) -> Result<Vec<String>, String> {
    let snap = state.snapshot();
    if snap
        .candidate_aliases
        .contains_key(&fallback.to_lowercase())
    {
        return Err(format!(
            "fallback '{fallback}' is a candidate alias, which picks a model per request; a \
             candidate alias cannot be a fallback yet — name one of its candidates or a plain \
             alias"
        ));
    }
    if owner.is(&snap, fallback) {
        return Err(format!(
            "fallback '{fallback}' would close a loop: {} → {fallback}",
            owner.name
        ));
    }
    let mut notes = Vec::new();
    if snap.hop_key(fallback).is_none() || snap.disabled_key(fallback).is_some() {
        let Some(key) = snap.disabled_key(fallback) else {
            return Err(format!(
                "fallback '{fallback}' does not resolve: no alias, local model or passthrough \
                 model answers to that name"
            ));
        };
        // Saved (§14.3, decision 28): a disabled hop hands a request on to
        // its own fallback, and answers again once enabled.
        notes.push(match snap.fallback_of(&key) {
            Some(next) => format!(
                "fallback '{fallback}' is disabled: until it is enabled again, a request \
                 reaching it goes on to its own fallback '{next}'"
            ),
            None => format!(
                "fallback '{fallback}' is disabled and has no fallback of its own: until it is \
                 enabled again, a request reaching it is refused"
            ),
        });
    }
    walk(&snap, owner, fallback)?;
    let target = facts_of(state, &snap, fallback).await;
    match (&owner.facts.ty, &target.ty) {
        (Some(o), Some(t)) if o != t => {
            return Err(format!(
                "fallback '{fallback}' is {} and '{}' is {}: a fallback must be the same type of \
                 model",
                t.describe(),
                owner.name,
                o.describe()
            ))
        }
        (Some(_), Some(_)) => {}
        (_, None) => notes.push(format!(
            "lmgw cannot tell what type of model '{fallback}' is (its upstream's catalog does not \
             list it, or cannot be read right now), so the fallback is saved without the type \
             check; a request it cannot serve is refused by name when it reaches it"
        )),
        (None, Some(_)) => notes.push(format!(
            "lmgw cannot tell what type of model '{}' is (its upstream's catalog does not list \
             it, or cannot be read right now), so the fallback is saved without the type check; \
             a request '{fallback}' cannot serve is refused by name when it reaches it",
            owner.name
        )),
    }
    if let (Some(up), Ok(route)) = (owner.upstream, snap.resolve(fallback)) {
        if route.upstream.id == up {
            notes.push(format!(
                "fallback '{fallback}' is on the same upstream as '{}', so it may share its \
                 quota (quotas can differ per model). Another upstream row with the same \
                 provider key shares the quota as well, and lmgw cannot see that",
                owner.name
            ));
        }
    }
    Ok(notes)
}

/// The facets two chat models differ in (§14.2 item 4), once the save is
/// written and published: what `fallback` is not known to take that
/// `owner_name` takes, and what it takes besides. Nothing when either is not
/// a chat model, or not published (a disabled row), or when they agree.
pub(crate) async fn facet_notes(
    state: &SharedState,
    owner_name: &str,
    fallback: &str,
) -> Vec<String> {
    let snap = state.snapshot();
    let (owner, target) = (
        facts_of(state, &snap, owner_name).await,
        facts_of(state, &snap, fallback).await,
    );
    let both_chat = owner.ty.as_ref().is_some_and(ModelType::is_chat)
        && target.ty.as_ref().is_some_and(ModelType::is_chat);
    let (Some(o), Some(t), true) = (owner.facets, target.facets, both_chat) else {
        return Vec::new();
    };
    let labels = |set: crate::candidates::FacetSet| {
        set.iter()
            .map(|f| f.label().to_lowercase())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut notes = Vec::new();
    let lacks = o.minus(t);
    if !lacks.is_empty() {
        // A facet's label may carry parentheses of its own ("vision (image
        // input)"): the owner is named after a comma, not in more of them.
        notes.push(format!(
            "fallback '{fallback}' is not known to take {}, which '{owner_name}' takes: a \
             request that uses it reaches the fallback degraded, or is refused by it",
            labels(lacks)
        ));
    }
    let adds = t.minus(o);
    if !adds.is_empty() {
        notes.push(format!(
            "fallback '{fallback}' takes {} besides what '{owner_name}' takes",
            labels(adds)
        ));
    }
    notes
}

/// What deleting the model or alias `key` leaves behind (§14.2, "confirm,
/// then do"): every other row or alias whose fallback names it, which has
/// no working fallback once it is gone.
pub(crate) fn delete_notes(snap: &Snapshot, key: &HopKey) -> Vec<String> {
    snap.fallback_owners()
        .into_iter()
        .filter(|o| o.key != *key && snap.any_hop_key(&o.fallback).as_ref() == Some(key))
        .map(|o| {
            format!(
                "{} falls back to '{}', which this delete removes: it has no working fallback \
                 now",
                o.label, o.fallback
            )
        })
        .collect()
}

/// The cycle check (§14.2 item 3): walk from `first` through each hop's own
/// fallback, local rows included, and refuse a walk that comes back to the
/// owner or to a hop already on it. A name that does not resolve ends the
/// walk: the use-time check names it.
fn walk(snap: &Snapshot, owner: &Owner, first: &str) -> Result<(), String> {
    let mut path = vec![owner.name.clone(), first.to_string()];
    let mut seen: Vec<HopKey> = owner.key.iter().cloned().collect();
    let mut cur = first.to_string();
    loop {
        let Some(key) = snap.any_hop_key(&cur) else {
            return Ok(());
        };
        if seen.contains(&key) {
            // A loop the owner's chain runs into without coming back to it.
            let from = path
                .iter()
                .position(|n| snap.any_hop_key(n).as_ref() == Some(&key));
            let looped = from.map_or_else(|| path.join(" → "), |i| path[i..].join(" → "));
            return Err(format!("fallback '{first}' leads into a loop: {looped}"));
        }
        seen.push(key.clone());
        let Some(next) = snap.fallback_of(&key) else {
            return Ok(());
        };
        let next = next.to_string();
        path.push(next.clone());
        if owner.is(snap, &next) {
            return Err(format!(
                "fallback '{first}' would close a loop: {}",
                path.join(" → ")
            ));
        }
        cur = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_patch_sets_clears_keeps_and_refuses_an_empty_name() {
        let cur = Some("cloud".to_string());
        assert_eq!(patched(None, &None, cur.clone()), Ok(cur.clone()));
        assert_eq!(
            patched(None, &Some("  other ".into()), cur.clone()),
            Ok(Some("other".into()))
        );
        assert_eq!(patched(Some("fallback"), &None, cur.clone()), Ok(None));
        assert_eq!(
            patched(Some("fallback"), &Some("x".into()), cur.clone()),
            Ok(None)
        );
        let empty = patched(None, &Some("  ".into()), cur).unwrap_err();
        assert!(empty.contains("clear: \"fallback\""), "{empty}");
    }

    #[test]
    fn the_type_folds_image_tasks_and_names_audio_tasks() {
        assert_eq!(ModelType::from_task("image_edit"), ModelType::image());
        assert_eq!(ModelType::audio("asr").describe(), "a speech-to-text model");
        assert_eq!(ModelType::audio("tts").describe(), "a text-to-speech model");
        assert_ne!(ModelType::audio("tts"), ModelType::audio("vdes"));
        assert_eq!(ModelType::audio("midi").describe(), "a 'midi' model");
    }
}
