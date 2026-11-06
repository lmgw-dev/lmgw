//! **A disabled hop** (openai egress design §14.3, decision 28): a name
//! that would otherwise not resolve, or an alias whose upstream row is
//! disabled, is looked up among the disabled rows and aliases, so the
//! model's own fallback answers it (reason `disabled`). A name that resolves
//! today resolves as it does: a disabled alias that shadows nothing keeps
//! falling through to a local or passthrough name of the same spelling.

use crate::runtime::Class;

use super::*;

impl Snapshot {
    /// The disabled model or alias `name` names, when nothing enabled
    /// answers to it (module doc): a disabled plain alias or one whose
    /// upstream is disabled, a disabled candidate alias, a disabled local
    /// row of any class under its public name (a chat row only when it is
    /// public). `None` for a name that resolves, or that nothing names.
    pub fn disabled_key(&self, name: &str) -> Option<HopKey> {
        if self.candidate_alias(name).is_some() || self.resolve(name).is_ok() {
            return None;
        }
        if let Some(a) = self.aliases.get(name) {
            let upstream_on = self
                .upstreams
                .get(&a.upstream_id)
                .is_some_and(|u| u.enabled);
            if !a.enabled || !upstream_on {
                return Some(HopKey::Alias(a.id));
            }
        }
        if let Some(c) = self
            .candidate_aliases
            .get(&name.to_lowercase())
            .filter(|c| !c.enabled)
        {
            return Some(HopKey::Candidate(c.id));
        }
        let local = |class: Class, id: &str| Some(HopKey::Local(class, id.to_string()));
        if let Some(m) = self
            .local_models
            .iter()
            .find(|m| !m.enabled && m.public && self.local_public_name(&m.model_id) == name)
        {
            return local(Class::Chat, &m.model_id);
        }
        if let Some(m) = self
            .aux_models
            .iter()
            .find(|m| !m.enabled && self.aux_public_name(m) == name)
        {
            return local(Class::Aux, &m.model_id);
        }
        if let Some(m) = self
            .audio_models
            .iter()
            .find(|m| !m.enabled && self.audio_public_name(&m.model_id) == name)
        {
            return local(Class::Audio, &m.model_id);
        }
        self.image_models
            .iter()
            .find(|m| !m.enabled && self.image_public_name(&m.model_id) == name)
            .and_then(|m| local(Class::Image, &m.model_id))
    }

    /// `next` past the disabled hops at its front (§14.3, `disabled`): each
    /// is handed the request and, as it cannot answer, hands it on to its
    /// own fallback. The hops passed, in order, and the first that is not
    /// disabled — or a plain `Unusable`: "is disabled" for a disabled hop
    /// with no fallback of its own, "was already tried in this request" for
    /// one met again (`seen`, or among those passed).
    pub fn past_disabled(
        &self,
        mut next: FallbackRoute,
        seen: impl Fn(&HopKey) -> bool,
    ) -> (Vec<(String, HopKey)>, FallbackRoute) {
        let mut passed: Vec<(String, HopKey)> = Vec::new();
        loop {
            let FallbackRoute::Unusable {
                alias,
                disabled: Some(key),
                ..
            } = next
            else {
                return (passed, next);
            };
            let ended = |why| FallbackRoute::Unusable {
                alias: alias.clone(),
                why,
                disabled: None,
            };
            if seen(&key) || passed.iter().any(|(_, k)| *k == key) {
                return (passed, ended("was already tried in this request"));
            }
            let after = self.next_hop(&key);
            if matches!(after, FallbackRoute::None) {
                return (passed, ended("is disabled"));
            }
            passed.push((alias, key));
            next = after;
        }
    }

    /// The hop `name` is, enabled ([`Self::hop_key`]) or disabled
    /// ([`Self::disabled_key`]): what a chain's identity checks compare.
    pub fn any_hop_key(&self, name: &str) -> Option<HopKey> {
        self.hop_key(name).or_else(|| self.disabled_key(name))
    }
}
