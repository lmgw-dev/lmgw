//! The I/O half of candidate-alias derivation (candidate-aliases design
//! §4.6): [`derive`] reads every candidate's and the fallback's live
//! capabilities (through [`capabilities::exposed`], which owns the GGUF
//! cache and the upstream catalog cache) and folds them into what the
//! editor, `/v1/models`, `lmgw__models` and the gate worker each need.
//! [`cached_pick`] is the per-request entry point the gate worker's pick
//! uses: in the steady state it reads nothing, and only stats the files each
//! pick was derived from (`super::stamps`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use super::stamps::{self, Stamp};
use super::{supports, Facet, FacetSet};
use crate::capabilities::exposed::{self, ExposedEntry};
use crate::capabilities::ModelCapabilities;
use crate::config::{CandidateAlias, FallbackRoute, Snapshot};
use crate::state::SharedState;

/// Everything [`derive`] computes about one candidate alias, right now.
#[derive(Debug, Clone, Default)]
pub struct CandidateDerived {
    /// Facets every *valid* candidate (an enabled local chat row lmgw could
    /// read) supports — the ceiling a save may enable (§4.6 rule 1).
    pub common: FacetSet,
    /// Per facet, the valid candidates that do *not* support it — the
    /// editor's "not supported by: <ids>".
    pub unsupported_by: HashMap<String, Vec<String>>,
    /// The stored enabled set, copied verbatim from
    /// [`CandidateAlias::capabilities_enabled`] — never recomputed here.
    pub enabled: FacetSet,
    /// Candidates the gate may actually route to, in list order (primary
    /// first when it is itself routable): enabled local chat rows that
    /// support every facet in `enabled`.
    pub routable: Vec<String>,
    /// The published capabilities of every `routable` candidate, same order
    /// — what `capabilities::exposed`'s `"candidate_alias"` publishing arm
    /// reads to decide which detail fields (reasoning levels, tool-call
    /// format) are identical across all of them and so worth publishing
    /// (§8 item 8). `None` for a routable candidate whose GGUF could not be
    /// read (that candidate would not be `routable` in practice, since an
    /// absent capabilities object supports no facet — kept `Option` only so
    /// this list can never desync in length from `routable`).
    pub routable_capabilities: Vec<Option<ModelCapabilities>>,
    /// Why the **primary** (`candidates[0]`) specifically is not in
    /// `routable`, when it is not — the gate's error messages and the
    /// editor's headline problem read this one instead of grepping
    /// `problems` for the primary's id. `None` when the primary is routable,
    /// or when the alias has no candidates at all.
    pub primary_skipped: Option<(String, SkipReason)>,
    /// What is wrong with a candidate or the fallback, in prose — shown by
    /// the editor, `lmgw__models` and `lmgw__status`.
    pub problems: Vec<String>,
    /// A candidate whose configured `--cache-ram` is exactly `0` loses the
    /// owner's idle conversation cache when background traffic shares it
    /// (§4.5).
    pub advisories: Vec<String>,
    /// The alias fallback resolves and is not local. Whether it supports
    /// every facet in `enabled` no longer matters (changed 2026-10-06): one
    /// that does not is named in `advisories`.
    pub fallback_usable: bool,
    /// Minimum across `routable` candidates (a ladder candidate counts at
    /// its top rung, `capabilities::exposed`'s own rule); absent if any
    /// routable candidate's value is unknown, or if nothing is routable.
    pub context_length: Option<u64>,
    /// Same rule as `context_length`.
    pub max_output_tokens: Option<u64>,
}

/// Why one candidate is not in [`CandidateDerived::routable`] — the
/// structured twin of the prose pushed onto `problems`, for a caller (the
/// gate's error messages) that wants to match on the reason rather than
/// parse a sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// No local chat row has this id.
    Missing,
    /// The row exists but is disabled.
    Disabled,
    /// The row is enabled but does not support one of the alias' enabled
    /// facets.
    LacksFacet(Facet),
}

impl SkipReason {
    pub fn describe(self) -> String {
        match self {
            SkipReason::Missing => "does not exist".to_string(),
            SkipReason::Disabled => "is disabled".to_string(),
            SkipReason::LacksFacet(f) => format!("lacks {}", f.as_str()),
        }
    }
}

/// The per-request shape [`cached_pick`] hands the gate worker: no prose, no
/// per-facet breakdown, nothing that needs a read to build again until the
/// snapshot or a file it was derived from changes.
#[derive(Debug, Clone, Default)]
pub struct CandidatePick {
    pub enabled: FacetSet,
    pub routable: Vec<String>,
    pub primary_skipped: Option<(String, SkipReason)>,
    /// Informational only: the gate never reads it, and judges the fallback
    /// at use instead, from the live catalog cache
    /// ([`crate::gate::open::fallback_serves`]) — so its capabilities
    /// changing does not make a cached pick stale.
    pub fallback_usable: bool,
}

impl From<&CandidateDerived> for CandidatePick {
    fn from(d: &CandidateDerived) -> Self {
        CandidatePick {
            enabled: d.enabled,
            routable: d.routable.clone(),
            primary_skipped: d.primary_skipped.clone(),
            fallback_usable: d.fallback_usable,
        }
    }
}

/// Derive [`CandidateDerived`] for `alias`, from the snapshot current when
/// this runs — never trusted from save time, since rows and the fallback
/// change in between (the same reasoning [`Snapshot::usable_fallback`]'s own
/// doc comment gives for row fallbacks).
///
/// **Cost.** One cached GGUF header read per candidate
/// ([`exposed::local_row_entry`], via `state.gguf_cache` — a warm call is
/// free, matching `capabilities::exposed`'s own cost note) plus, only when
/// the fallback resolves to something other than a candidate/local route,
/// one [`exposed::exposed_entry`] lookup for it (a cached upstream-catalog
/// fetch on a cloud alias). Safe to call on every save and on every listing
/// (`ops::models`, `/v1/models`, the dashboard's alias editor). **Not** meant
/// for the request hot path directly — see [`cached_pick`], which wraps this
/// behind a cache so the steady state costs neither a GGUF read nor a
/// catalog fetch.
pub async fn derive(
    state: &SharedState,
    snap: &Snapshot,
    alias: &CandidateAlias,
) -> CandidateDerived {
    let mut out = CandidateDerived {
        enabled: FacetSet::from_names(&alias.capabilities_enabled).unwrap_or_default(),
        ..Default::default()
    };

    // One entry per *valid* candidate (an enabled local chat row) — the rest
    // become problems and are never judged for capability support: there is
    // nothing to read, so they cannot decide what is "common" either.
    // Candidates are looked up in `local_models` only, so every entry found
    // here is definitionally a local chat row — the save-time refusal's
    // third clause ("not a local chat row") can only ever be hit by an id
    // that also fails one of the first two checks, since nothing else
    // populates this table.
    let mut valid: Vec<(String, ExposedEntry, Option<i64>)> = Vec::new();
    for id in &alias.candidates {
        match snap.local_models.iter().find(|m| &m.model_id == id) {
            None => out
                .problems
                .push(format!("candidate '{id}' does not exist")),
            Some(m) if !m.enabled => out.problems.push(format!("candidate '{id}' is disabled")),
            Some(m) => {
                let entry = exposed::local_row_entry(state, snap, m, id.clone()).await;
                valid.push((id.clone(), entry, m.params.cache_ram));
            }
        }
    }

    // `common` / `unsupported_by`: only ever over `valid`. An absent
    // capabilities object (unreadable GGUF) supports nothing, so it drops
    // every facet out of `common` and is named under each one rather than
    // silently excluded from the count.
    for facet in Facet::ALL {
        let lacking: Vec<String> = valid
            .iter()
            .filter(|(_, entry, _)| {
                !entry
                    .capabilities
                    .as_ref()
                    .is_some_and(|c| supports(c, facet))
            })
            .map(|(id, _, _)| id.clone())
            .collect();
        if lacking.is_empty() {
            out.common = out.common.insert(facet);
        } else {
            out.unsupported_by
                .insert(facet.as_str().to_string(), lacking);
        }
    }

    // `routable`: list order preserved; valid, and lacking none of the
    // *stored* enabled facets — never recomputed here, so a candidate edited
    // afterwards to drop a facet leaves `routable` and is reported, instead
    // of silently shrinking the alias' published contract (§4.6).
    for (id, entry, cache_ram) in &valid {
        let caps = entry.capabilities.as_ref();
        let missing: Vec<Facet> = out
            .enabled
            .iter()
            .filter(|f| !caps.is_some_and(|c| supports(c, *f)))
            .collect();
        if missing.is_empty() {
            out.routable.push(id.clone());
            out.routable_capabilities.push(entry.capabilities.clone());
        } else {
            for f in &missing {
                out.problems
                    .push(format!("'{id}' is skipped: lacks {}", f.as_str()));
            }
        }
        if *cache_ram == Some(0) {
            out.advisories.push(format!(
                "'{id}' runs with --cache-ram 0: sharing it with background traffic drops the \
                 owner's idle conversation cache instead of keeping it (§4.5)."
            ));
        }
    }

    // The fallback: resolved through the one lookup every fallback goes
    // through (`Snapshot::alias_fallback`). One that lacks a facet the alias
    // enables is used all the same (changed 2026-10-06, the owner's ruling:
    // a configured fallback is always used): what it cannot take goes to it
    // degraded, and the editor says so beforehand.
    match snap.alias_fallback(alias) {
        FallbackRoute::None => out.fallback_usable = false,
        FallbackRoute::Unusable { alias: fb, why } => {
            out.problems
                .push(format!("fallback '{fb}' {why}: treated as none"));
            out.fallback_usable = false;
        }
        FallbackRoute::Usable { alias: fb, .. } => {
            out.fallback_usable = true;
            if let Err(f) = fallback_supports(state, &fb, out.enabled).await {
                out.advisories.push(format!(
                    "fallback '{fb}' lacks {}: it answers all the same, and what it cannot take \
                     goes to it degraded (images as placeholders, a Chat PDF's pages as text, a \
                     voice turn as its transcript), marked on its request rows",
                    f.as_str()
                ));
            }
        }
    }

    // Published limits: minimum across `routable`, absent if any of them is
    // unknown or if nothing is routable at all.
    let mut ctx: Option<u64> = None;
    let mut ctx_known = true;
    let mut mot: Option<u64> = None;
    let mut mot_known = true;
    for id in &out.routable {
        let Some((_, entry, _)) = valid.iter().find(|(vid, _, _)| vid == id) else {
            continue;
        };
        match entry.context_length {
            Some(c) => ctx = Some(ctx.map_or(c, |cur| cur.min(c))),
            None => ctx_known = false,
        }
        match entry.max_output_tokens {
            Some(c) => mot = Some(mot.map_or(c, |cur| cur.min(c))),
            None => mot_known = false,
        }
    }
    out.context_length = ctx.filter(|_| ctx_known);
    out.max_output_tokens = mot.filter(|_| mot_known);

    // The primary's own reason, for the gate's error messages — a lookup
    // over what is already in hand, no extra I/O.
    out.primary_skipped = alias.candidates.first().and_then(|id| {
        if out.routable.contains(id) {
            return None;
        }
        match snap.local_models.iter().find(|m| &m.model_id == id) {
            None => Some((id.clone(), SkipReason::Missing)),
            Some(m) if !m.enabled => Some((id.clone(), SkipReason::Disabled)),
            Some(_) => {
                let caps = valid
                    .iter()
                    .find(|(vid, _, _)| vid == id)
                    .and_then(|(_, e, _)| e.capabilities.as_ref());
                out.enabled
                    .iter()
                    .find(|f| !caps.is_some_and(|c| supports(c, *f)))
                    .map(|f| (id.clone(), SkipReason::LacksFacet(f)))
            }
        }
    });

    out
}

/// Whether the fallback named `fallback_alias` supports every facet in
/// `enabled` — `Err(f)` names the first one it does not: what [`derive`]'s
/// advisory names. Nothing refuses or skips a fallback for it any more
/// (changed 2026-10-06, the owner's ruling: a configured fallback is always
/// used).
pub async fn fallback_supports(
    state: &SharedState,
    fallback_alias: &str,
    enabled: FacetSet,
) -> Result<(), Facet> {
    // `Box::pin`: `exposed_entry` → `entry_for` → `candidate_alias_entry` →
    // `derive` → here is a call cycle the compiler sees statically (it never
    // actually recurses at runtime — a fallback can never itself be a
    // candidate alias, `Snapshot::usable_fallback` refuses that at save
    // time), so the two mutually async fns need one heap indirection to
    // have a finite size.
    let caps = Box::pin(exposed::exposed_entry(state, fallback_alias))
        .await
        .and_then(|e| e.capabilities);
    for f in enabled.iter() {
        if !caps.as_ref().is_some_and(|c| supports(c, f)) {
            return Err(f);
        }
    }
    // Audio is a capability of the egress too: the Anthropic API has no
    // audio part, whatever an override claims, so such a fallback counts as
    // none for an alias that enables audio — the walk and the heard turn's
    // check (`capabilities::hears`) agree (voice-audio-input review V2).
    let anthropic = state
        .snapshot()
        .resolve(fallback_alias)
        .is_ok_and(|r| r.upstream.protocol == crate::config::Protocol::Anthropic);
    if enabled.contains(Facet::Audio) && anthropic {
        return Err(Facet::Audio);
    }
    Ok(())
}

/// The two facts (well, four — see [`CandidatePick`]) the gate worker's
/// per-request pick needs, without holding onto the rest of
/// [`CandidateDerived`]. Not a cheaper *computation* than [`derive`] — see
/// [`cached_pick`] for the function that actually avoids repeating the I/O.
pub async fn routable(
    state: &SharedState,
    snap: &Snapshot,
    alias: &CandidateAlias,
) -> (Vec<String>, bool) {
    let d = derive(state, snap, alias).await;
    (d.routable, d.fallback_usable)
}

// ---------------------------------------------------------------------------
// Per-request cache
// ---------------------------------------------------------------------------

/// One alias's cached pick, and the stamps of the files it was derived from
/// ([`super::stamps`]): a call whose stamps differ derives again.
#[derive(Debug, Clone)]
struct Cached {
    stamps: Vec<Stamp>,
    pick: CandidatePick,
}

/// `(the Arc<Snapshot> pointer this cache was filled against, alias name ->
/// Cached)`. Cleared whole whenever the pointer changes — a config reload
/// swaps in a brand new `Arc<Snapshot>` (`AppState::reload_snapshot`), so
/// the pointer *is* the snapshot's generation counter, with no new field
/// needed on `Snapshot` or `AppState` to carry one. Not evicted
/// entry-by-entry: reloads are owner-driven and rare, one alias' entry is a
/// handful of small strings and paths, and a desktop app for one user never
/// accumulates enough stale generations between reloads for that to matter.
///
/// The snapshot is only half of what a pick is derived from; the files are
/// the other half, and each entry carries their stamps
/// ([`super::stamps`], §12 entry 87).
///
/// The key is kept as a `Weak`, not a bare address: a `Weak` keeps the old
/// snapshot's *allocation* alive (not the snapshot itself), so a later
/// `Arc<Snapshot>` can never be handed the same address while this cache
/// still compares against it — a freed-and-reused address would otherwise
/// serve the previous configuration's pick.
type PickCache = Mutex<(Weak<Snapshot>, HashMap<String, Cached>)>;

fn cache() -> &'static PickCache {
    static CACHE: OnceLock<PickCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new((Weak::new(), HashMap::new())))
}

/// The gate worker's per-request pick: [`CandidatePick`] for `alias`,
/// derived by [`derive`] when the snapshot or any file it reads changed
/// since, and read from an in-memory cache otherwise.
///
/// **Cost in the steady state:** one `stat` per candidate file (weights,
/// plus a configured projector and chat template file when the row has
/// them) on the blocking pool, a mutex lock and a clone of a few small
/// strings — no GGUF read, no catalog fetch ([`super::stamps`]).
///
/// **Never cached:** a pick derived while any of those files could not be
/// read (missing, not a GGUF yet, unreadable). Like the GGUF cache's errors,
/// that is derived again on the next call, so a transient problem cannot
/// keep a candidate out of — or in — `routable` until the next config
/// write.
///
/// `snap` is the caller's own `Arc<Snapshot>` (typically `state.snapshot()`)
/// rather than a bare `&Snapshot`, because the cache's generation key is the
/// Arc's pointer identity — see [`PickCache`]'s doc comment.
pub async fn cached_pick(
    state: &SharedState,
    snap: &Arc<Snapshot>,
    alias: &CandidateAlias,
) -> CandidatePick {
    let stamps = stamps::stamp(snap, alias).await;
    let current = |key: &Weak<Snapshot>| std::ptr::eq(key.as_ptr(), Arc::as_ptr(snap));
    {
        let mut guard = cache().lock().unwrap();
        if !current(&guard.0) {
            guard.0 = Arc::downgrade(snap);
            guard.1.clear();
        }
        if let Some(hit) = guard.1.get(&alias.alias).filter(|c| c.stamps == stamps) {
            return hit.pick.clone();
        }
    }
    let derived = derive(state, snap, alias).await;
    let pick = CandidatePick::from(&derived);
    if !stamps::all_readable(state, &stamps).await {
        return pick;
    }
    let mut guard = cache().lock().unwrap();
    // Another caller may have raced this fill for the same (ptr, alias); the
    // result is identical either way (derive is pure over the same snapshot
    // and files), so last write wins with no correctness cost. Stamps taken
    // before the derive: a file that changed while it ran differs from them
    // on the next call, which derives again.
    if current(&guard.0) {
        guard.1.insert(
            alias.alias.clone(),
            Cached {
                stamps,
                pick: pick.clone(),
            },
        );
    }
    pick
}
