//! A fallback's identity (openai egress design §14.2–§14.3): every model and
//! alias a request can be routed to is one **hop**, named by its
//! [`HopKey`], and carries at most one configured fallback
//! ([`Snapshot::fallback_of`]). Along a chain and in the cycle check hops are
//! compared by identity, never by text, so a renamed prefix or a case variant
//! of a candidate alias cannot hide a loop.

use crate::runtime::Class;

use super::*;

/// One model or alias a request can be routed to, by identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HopKey {
    /// A plain alias (`models.id`).
    Alias(i64),
    /// A candidate alias (`candidate_aliases.id`).
    Candidate(i64),
    /// A local row of `class` by its `model_id`.
    Local(Class, String),
    /// A passthrough id: an `expose_all` upstream's model. Never has a
    /// fallback of its own (§14.2: an owner who wants one aliases it).
    Passthrough(i64, String),
}

/// The clause a refusal names the chain with (§14.8): " (tried: A → B →
/// C)", the requested name first; nothing when no fallback was tried.
pub fn tried_clause(start: &str, hops: &[String]) -> String {
    if hops.is_empty() {
        return String::new();
    }
    format!(" (tried: {start} → {})", hops.join(" → "))
}

/// The clause of a hop the walk met again (§14.3): the guard for a loop the
/// save check could not see (a hand edit, two writes racing).
pub fn already_tried(fb: &str, start: &str, hops: &[String]) -> String {
    let mut path = vec![start.to_string()];
    path.extend(hops.iter().cloned());
    path.push(fb.to_string());
    format!(
        " (fallback '{fb}' was already tried in this request: {})",
        path.join(" → ")
    )
}

/// A row or alias with a fallback, as the save checks and a rename's
/// rewrite list them ([`Snapshot::fallback_owners`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackOwner {
    pub key: HopKey,
    /// Where it is stored.
    pub table: crate::store::FallbackTable,
    pub id: i64,
    /// What it is, for a sentence: "chat model 'qwen3'".
    pub label: String,
    /// Its fallback, as stored.
    pub fallback: String,
}

impl Snapshot {
    /// The hop `name` is, resolved as a client's `model` is
    /// ([`Snapshot::resolve`]): an enabled candidate alias (case-insensitive),
    /// an enabled plain alias (whatever its upstream says — an alias on a
    /// disabled upstream is still that alias), a local public name of any
    /// class, then a passthrough id. `None` for a name nothing answers to.
    pub fn hop_key(&self, name: &str) -> Option<HopKey> {
        if let Some(ca) = self.candidate_alias(name) {
            return Some(HopKey::Candidate(ca.id));
        }
        if let Some(a) = self.aliases.get(name).filter(|a| a.enabled) {
            return Some(HopKey::Alias(a.id));
        }
        let route = self
            .resolve_local(name)
            .or_else(|| self.resolve_passthrough(name))?;
        Some(Self::route_key(&route))
    }

    /// The hop `name` names as a fallback (§14.3): its identity and its
    /// route, marked as the fallback it is ([`Route::fallback`] — this is
    /// where every fallback's route is born, so every send on it knows,
    /// in-process turns included). Any model or alias a client could name
    /// may be one, a local model included; `Err` is why `name` cannot be, as
    /// the clause a refusal names it with: it does not resolve, or it is a
    /// candidate alias, which has no static route (which of its candidates
    /// answers is decided per request), not yet allowed (§14.14).
    pub fn hop(&self, name: &str) -> Result<(HopKey, Route), &'static str> {
        if self.candidate_aliases.contains_key(&name.to_lowercase()) {
            return Err("is itself a candidate alias");
        }
        let key = self.hop_key(name).ok_or("does not resolve")?;
        let mut route = self.resolve(name).map_err(|_| "does not resolve")?;
        route.fallback = Some(name.into());
        Ok((key, route))
    }

    /// A configured fallback `name` as the hop it is ([`Self::hop`]):
    /// none, one that cannot stand in and why, or one that can. Re-validated
    /// per call, never trusted from save time: rows and aliases change in
    /// between.
    pub fn fallback_named(&self, name: Option<&str>) -> FallbackRoute {
        let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
            return FallbackRoute::None;
        };
        if !self.candidate_aliases.contains_key(&name.to_lowercase()) {
            if let Some(key) = self.disabled_key(name) {
                return FallbackRoute::Unusable {
                    alias: name.to_string(),
                    why: "is disabled",
                    disabled: Some(key),
                };
            }
        }
        match self.hop(name) {
            Ok((key, route)) => FallbackRoute::Usable {
                alias: name.to_string(),
                key,
                route,
            },
            Err(why) => FallbackRoute::Unusable {
                alias: name.to_string(),
                why,
                disabled: None,
            },
        }
    }

    /// The next hop after `key`: its own configured fallback, as
    /// [`Self::fallback_named`] reads it — the one lookup every reason a
    /// model or alias cannot answer goes through (§14.3).
    pub fn next_hop(&self, key: &HopKey) -> FallbackRoute {
        self.fallback_named(self.fallback_of(key))
    }

    /// The hop a resolved route lands on, when the route is not an alias's:
    /// a local row, or a passthrough model.
    pub fn route_key(route: &Route) -> HopKey {
        match crate::vram::classify(route) {
            Some(t) => HopKey::Local(t.class, t.model_id),
            None => HopKey::Passthrough(route.upstream.id, route.upstream_model.clone()),
        }
    }

    /// The configured fallback of the model or alias `key` names, as stored
    /// (trimmed at save, so as written): the one lookup every fallback goes
    /// through (§14.3). `None` for none, a blank one, a passthrough model
    /// (which has none) and a key this snapshot does not know.
    pub fn fallback_of(&self, key: &HopKey) -> Option<&str> {
        let f = match key {
            HopKey::Alias(id) => self
                .aliases
                .values()
                .find(|a| a.id == *id)?
                .fallback
                .as_deref(),
            HopKey::Candidate(id) => self
                .candidate_aliases
                .values()
                .find(|c| c.id == *id)?
                .fallback
                .as_deref(),
            HopKey::Local(Class::Chat, m) => self
                .local_models
                .iter()
                .find(|r| &r.model_id == m)?
                .fallback
                .as_deref(),
            HopKey::Local(Class::Aux, m) => self
                .aux_models
                .iter()
                .find(|r| &r.model_id == m)?
                .fallback
                .as_deref(),
            HopKey::Local(Class::Audio, m) => self
                .audio_models
                .iter()
                .find(|r| &r.model_id == m)?
                .fallback
                .as_deref(),
            HopKey::Local(Class::Image, m) => self
                .image_models
                .iter()
                .find(|r| &r.model_id == m)?
                .fallback
                .as_deref(),
            HopKey::Passthrough(..) => None,
        };
        f.filter(|f| !f.trim().is_empty())
    }

    /// The name a client reaches `key` by now: an alias's own, a local row's
    /// public name under its class prefix, a passthrough model under its
    /// upstream's prefix. `None` for a key this snapshot does not know.
    pub fn hop_name(&self, key: &HopKey) -> Option<String> {
        Some(match key {
            HopKey::Alias(id) => self.aliases.values().find(|a| a.id == *id)?.alias.clone(),
            HopKey::Candidate(id) => self
                .candidate_aliases
                .values()
                .find(|c| c.id == *id)?
                .alias
                .clone(),
            HopKey::Local(Class::Chat, m) => self.local_public_name(m),
            HopKey::Local(Class::Aux, m) => {
                let row = self.aux_models.iter().find(|r| &r.model_id == m)?;
                self.aux_public_name(row)
            }
            HopKey::Local(Class::Audio, m) => self.audio_public_name(m),
            HopKey::Local(Class::Image, m) => self.image_public_name(m),
            HopKey::Passthrough(up, m) => {
                let u = self.upstreams.get(up)?;
                prefixed(u.prefix(), m)
            }
        })
    }

    /// Every model and alias that has a fallback, in a stable order (the
    /// six tables in turn, each by its own order): what the save checks
    /// walk and a rename rewrites (§14.2).
    pub fn fallback_owners(&self) -> Vec<FallbackOwner> {
        use crate::store::FallbackTable as T;
        let mut out = Vec::new();
        let mut push =
            |key: HopKey, table: T, id: i64, label: String, fallback: &Option<String>| {
                if let Some(f) = fallback.as_deref().filter(|f| !f.trim().is_empty()) {
                    out.push(FallbackOwner {
                        key,
                        table,
                        id,
                        label,
                        fallback: f.to_string(),
                    });
                }
            };
        let mut aliases: Vec<&ModelAlias> = self.aliases.values().collect();
        aliases.sort_by_key(|a| a.id);
        for a in aliases {
            push(
                HopKey::Alias(a.id),
                T::Models,
                a.id,
                format!("alias '{}'", a.alias),
                &a.fallback,
            );
        }
        let mut cands: Vec<&CandidateAlias> = self.candidate_aliases.values().collect();
        cands.sort_by_key(|c| c.id);
        for c in cands {
            push(
                HopKey::Candidate(c.id),
                T::CandidateAliases,
                c.id,
                format!("candidate alias '{}'", c.alias),
                &c.fallback,
            );
        }
        for m in &self.local_models {
            push(
                HopKey::Local(Class::Chat, m.model_id.clone()),
                T::LocalModels,
                m.id,
                format!("chat model '{}'", m.model_id),
                &m.fallback,
            );
        }
        for m in &self.aux_models {
            push(
                HopKey::Local(Class::Aux, m.model_id.clone()),
                T::AuxModels,
                m.id,
                format!(
                    "{} '{}'",
                    match m.kind {
                        AuxKind::Embed => "embedding model",
                        AuxKind::Rerank => "reranker",
                        AuxKind::Decision => "decision model",
                    },
                    m.model_id
                ),
                &m.fallback,
            );
        }
        for m in &self.audio_models {
            push(
                HopKey::Local(Class::Audio, m.model_id.clone()),
                T::AudioModels,
                m.id,
                format!("audio model '{}'", m.model_id),
                &m.fallback,
            );
        }
        for m in &self.image_models {
            push(
                HopKey::Local(Class::Image, m.model_id.clone()),
                T::ImageModels,
                m.id,
                format!("image model '{}'", m.model_id),
                &m.fallback,
            );
        }
        out
    }
}
