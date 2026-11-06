//! **The fallbacks that name what a write changes** (openai egress design
//! §14.2, decision 29): a fallback of another model type cannot be set,
//! and a loop is refused at save. A setter checks
//! the fallback it writes ([`super::validate`]), but a write can also change
//! what *another* row's stored fallback points at: a create whose name a
//! dangling fallback already holds, a rename onto such a name, an alias
//! retargeted to a model of another type, an aux kind or an audio task
//! changed, a class or upstream prefix changed, a row or alias enabled or
//! disabled (an alias shadows a local name of the same spelling only while
//! it is enabled), an upstream that starts or stops exposing its catalog.
//!
//! So every such write runs as a **dry run** first: the write inside a
//! transaction, the names read back as they would be
//! ([`store::routing_after`]), the transaction rolled back. Every fallback
//! owner whose stored name now reaches something else, or reaches a hop the
//! write touched, is checked in that view, and the write is refused when it
//! would give one of them a fallback of another type, a candidate alias or
//! a loop, naming each. What stood before the write (a mismatch migration
//! 0095 kept, a loop a hand edit wrote) is not the write's, and does not
//! refuse it. A delete is never refused: its answer names the rows it
//! leaves without a working fallback ([`super::delete_notes`]).

use std::collections::HashSet;

use crate::config::{FallbackOwner, HopKey, Snapshot};
use crate::runtime::Class;
use crate::state::SharedState;
use crate::store::{self, FallbackTable};

use super::types::{local_type, Facts, ModelType};

/// What a write touches, beyond the names it changes: the hops whose type
/// may change with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Touch {
    /// One row or alias, by its table and id.
    Row(FallbackTable, i64),
    /// An upstream: its plain aliases and its passthrough models.
    Upstream(i64),
}

/// A hop's identity across a rename: a row's table and id (a local row's
/// [`HopKey`] carries its `model_id`, which a rename changes), or a
/// passthrough model.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ident {
    Row(FallbackTable, i64),
    Passthrough(i64, String),
}

fn ident(snap: &Snapshot, key: &HopKey) -> Option<Ident> {
    use FallbackTable as T;
    let local = |table: T, id: Option<i64>| id.map(|id| Ident::Row(table, id));
    match key {
        HopKey::Alias(id) => Some(Ident::Row(T::Models, *id)),
        HopKey::Candidate(id) => Some(Ident::Row(T::CandidateAliases, *id)),
        HopKey::Local(Class::Chat, m) => local(
            T::LocalModels,
            snap.local_models
                .iter()
                .find(|r| &r.model_id == m)
                .map(|r| r.id),
        ),
        HopKey::Local(Class::Aux, m) => local(
            T::AuxModels,
            snap.aux_models
                .iter()
                .find(|r| &r.model_id == m)
                .map(|r| r.id),
        ),
        HopKey::Local(Class::Audio, m) => local(
            T::AudioModels,
            snap.audio_models
                .iter()
                .find(|r| &r.model_id == m)
                .map(|r| r.id),
        ),
        HopKey::Local(Class::Image, m) => local(
            T::ImageModels,
            snap.image_models
                .iter()
                .find(|r| &r.model_id == m)
                .map(|r| r.id),
        ),
        HopKey::Passthrough(up, m) => Some(Ident::Passthrough(*up, m.clone())),
    }
}

fn touches(touched: &[Touch], snap: &Snapshot, id: &Ident) -> bool {
    touched.iter().any(|t| match (t, id) {
        (Touch::Row(table, row), Ident::Row(t2, r2)) => table == t2 && row == r2,
        (Touch::Upstream(up), Ident::Passthrough(u2, _)) => up == u2,
        (Touch::Upstream(up), Ident::Row(FallbackTable::Models, alias)) => snap
            .aliases
            .values()
            .any(|a| a.id == *alias && a.upstream_id == *up),
        _ => false,
    })
}

/// The type of the hop `key` in `snap` (§14.2 item 4): a local row's from
/// the row, a candidate alias's (its candidates are chat rows), a plain
/// alias's and a passthrough model's from what an alias of it publishes,
/// reading its upstream's catalog. `None` when lmgw cannot know it.
async fn type_in(state: &SharedState, snap: &Snapshot, key: &HopKey) -> Option<ModelType> {
    use crate::capabilities::exposed::alias_capabilities;
    match key {
        HopKey::Local(class, id) => local_type(snap, *class, id),
        HopKey::Candidate(_) => Some(ModelType::chat()),
        HopKey::Alias(id) => {
            let a = snap.aliases.values().find(|a| a.id == *id)?;
            let u = snap.upstreams.get(&a.upstream_id)?;
            let caps = alias_capabilities(
                state,
                snap,
                u,
                &a.upstream_model_id,
                &a.param_overrides,
                a.capabilities_override.as_ref(),
            )
            .await;
            Facts::from_capabilities(caps.as_ref()).ty
        }
        HopKey::Passthrough(up, model) => {
            let u = snap.upstreams.get(up)?;
            let caps =
                alias_capabilities(state, snap, u, model, &crate::ir::Params::default(), None)
                    .await;
            Facts::from_capabilities(caps.as_ref()).ty
        }
    }
}

/// [`type_in`] in the view a write would leave (`after`): the catalog of
/// an upstream the write changes is that configuration's, read fresh and
/// never cached (`catalog::uncached_for`) — the cache holds the live
/// configuration's under the same id, and a refused dry run must leave
/// nothing behind. `before`'s side reads the live catalogs as every request
/// does.
async fn type_after(
    state: &SharedState,
    after: &Snapshot,
    changed: &HashSet<i64>,
    key: &HopKey,
) -> Option<ModelType> {
    crate::catalog::uncached_for(changed.clone(), type_in(state, after, key)).await
}

/// The upstreams whose row `after` has otherwise than `before`, or does
/// not have at all (a create the dry run rolls back), and every upstream
/// the write touches.
fn changed_upstreams(before: &Snapshot, after: &Snapshot, touched: &[Touch]) -> HashSet<i64> {
    let row = |u: &crate::config::Upstream| serde_json::to_value(u).ok();
    let mut ids: HashSet<i64> = after
        .upstreams
        .iter()
        .filter(|(id, u)| before.upstreams.get(id).map(row) != Some(row(u)))
        .map(|(id, _)| *id)
        .collect();
    ids.extend(touched.iter().filter_map(|t| match t {
        Touch::Upstream(id) => Some(*id),
        Touch::Row(..) => None,
    }));
    ids
}

/// The loop `o`'s chain closes in `snap`, when it comes back to `o`
/// ("a → b → a"). A loop further along that does not come back to `o` is
/// its own owners' to name.
fn loop_back(snap: &Snapshot, o: &FallbackOwner) -> Option<String> {
    let start = snap.hop_name(&o.key).unwrap_or_else(|| o.label.clone());
    let mut path = vec![start, o.fallback.clone()];
    let mut seen = vec![o.key.clone()];
    let mut cur = o.fallback.clone();
    loop {
        let key = snap.any_hop_key(&cur)?;
        if key == o.key {
            return Some(path.join(" → "));
        }
        if seen.contains(&key) {
            return None;
        }
        seen.push(key.clone());
        let next = snap.fallback_of(&key)?.to_string();
        path.push(next.clone());
        cur = next;
    }
}

/// The check (module doc): every fallback owner in `after` — the names as
/// the write would leave them — whose stored fallback reaches another hop
/// than it did in `before`, or whose hop or target the write touches, must
/// not then fall back to a candidate alias, close a loop or name a model of
/// another type, unless that stood before the write already. `own` is the
/// row the write itself sets a fallback on, which [`super::validate`]
/// checked.
pub(crate) async fn check_referrers(
    state: &SharedState,
    before: &Snapshot,
    after: &Snapshot,
    touched: &[Touch],
    own: Option<(FallbackTable, i64)>,
) -> Result<(), String> {
    let before_owners = before.fallback_owners();
    let changed = changed_upstreams(before, after, touched);
    let mut problems = Vec::new();
    for o in after.fallback_owners() {
        if own == Some((o.table, o.id)) {
            continue;
        }
        let Some(target) = after.any_hop_key(&o.fallback) else {
            continue;
        };
        let was = before_owners
            .iter()
            .find(|b| b.table == o.table && b.id == o.id);
        let target_was = was.and_then(|b| before.any_hop_key(&b.fallback));
        let now_id = ident(after, &target);
        let was_id = target_was.as_ref().and_then(|k| ident(before, k));
        let me = Ident::Row(o.table, o.id);
        let affected = now_id != was_id
            || touches(touched, after, &me)
            || now_id.as_ref().is_some_and(|t| touches(touched, after, t));
        if !affected {
            continue;
        }
        if matches!(target, HopKey::Candidate(_)) {
            if !matches!(target_was, Some(HopKey::Candidate(_))) {
                problems.push(format!(
                    "{} falls back to '{}', which would be a candidate alias, and a candidate \
                     alias cannot be a fallback yet",
                    o.label, o.fallback
                ));
            }
            continue;
        }
        if let Some(path) = loop_back(after, &o) {
            if was.is_none_or(|b| loop_back(before, b).is_none()) {
                problems.push(format!(
                    "{} falls back to '{}', which would close a loop: {path}",
                    o.label, o.fallback
                ));
            }
            continue;
        }
        let (Some(mine), Some(theirs)) = (
            type_after(state, after, &changed, &o.key).await,
            type_after(state, after, &changed, &target).await,
        ) else {
            continue;
        };
        if mine == theirs {
            continue;
        }
        // A fallback of another type before the write stays one: the write
        // does not make it so.
        if let (Some(b), Some(tw)) = (was, &target_was) {
            let (o_was, t_was) = (
                type_in(state, before, &b.key).await,
                type_in(state, before, tw).await,
            );
            if matches!((o_was, t_was), (Some(a), Some(b)) if a != b) {
                continue;
            }
        }
        problems.push(format!(
            "{} falls back to '{}', which would be {} while it is {}",
            o.label,
            o.fallback,
            theirs.describe(),
            mine.describe()
        ));
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(format!(
        "this save would break another model's fallback: {}. A fallback must be the same type \
         of model and must not close a loop: change or clear that fallback first",
        problems.join("; ")
    ))
}

/// The dry run (module doc): `tx` holds the write, not committed; read the
/// names back as it would leave them, roll it back, and check
/// ([`check_referrers`]).
pub(crate) async fn dry_run(
    state: &SharedState,
    before: &Snapshot,
    tx: sqlx::Transaction<'static, sqlx::Sqlite>,
    touched: &[Touch],
    own: Option<(FallbackTable, i64)>,
) -> Result<(), String> {
    let after = store::routing_after(tx).await.map_err(|e| e.to_string())?;
    check_referrers(state, before, &after, touched, own).await
}

/// What deleting the upstream `upstream_id` leaves behind: its plain aliases
/// go with it (`ON DELETE CASCADE`) and its passthrough models stop
/// resolving, so every other row or alias whose fallback names one of them
/// has no working fallback once it is gone.
pub(crate) fn upstream_delete_notes(snap: &Snapshot, upstream_id: i64) -> Vec<String> {
    let gone = |key: &HopKey| match key {
        HopKey::Alias(id) => snap
            .aliases
            .values()
            .any(|a| a.id == *id && a.upstream_id == upstream_id),
        HopKey::Passthrough(up, _) => *up == upstream_id,
        _ => false,
    };
    snap.fallback_owners()
        .into_iter()
        .filter(|o| !gone(&o.key))
        .filter(|o| snap.any_hop_key(&o.fallback).is_some_and(|k| gone(&k)))
        .map(|o| {
            format!(
                "{} falls back to '{}', which this delete removes: it has no working fallback \
                 now",
                o.label, o.fallback
            )
        })
        .collect()
}
