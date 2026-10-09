//! Who has a URI: the candidates, the claims and their order (the module
//! doc of [`super`]), and which servers a request must ask to know.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::config::{McpServer, Snapshot};

use super::super::super::scope::ToolScope;
use super::super::uri::{has_authority, namespaced, strip, template_matches, template_usable};

/// How a server claims a URI, strongest first (module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Claim {
    /// A tool of its own names it in `_meta.ui.resourceUri`.
    Tool,
    /// Its `resources/list` lists it.
    Listed,
    /// One of its usable `uriTemplate`s fits it.
    Template,
}

/// One connected server's resources as a request found them, in its own
/// spelling: what its tools name, and what it was asked to list (nothing
/// when it was not asked).
pub(super) struct Known {
    pub(super) server: McpServer,
    /// The URIs its tools name in `_meta.ui.resourceUri`, as `/mcp` shows
    /// them.
    pub(super) ui: HashSet<String>,
    pub(super) resources: Vec<Value>,
    pub(super) templates: Vec<Value>,
}

impl Known {
    /// How this server claims `exposed`, if it does (module doc).
    fn claim(&self, exposed: &str) -> Option<Claim> {
        let p = &self.server.tool_prefix;
        if self.ui.contains(exposed) {
            return Some(Claim::Tool);
        }
        if self
            .resources
            .iter()
            .filter_map(uri_of)
            .any(|u| namespaced(p, u) == exposed)
        {
            return Some(Claim::Listed);
        }
        self.templates
            .iter()
            .filter_map(|t| t.get("uriTemplate").and_then(Value::as_str))
            .filter(|t| template_usable(p, t))
            .any(|t| template_matches(&namespaced(p, t), exposed))
            .then_some(Claim::Template)
    }
}

/// A listed resource's URI.
fn uri_of(resource: &Value) -> Option<&str> {
    resource.get("uri").and_then(Value::as_str)
}

/// The enabled servers in name order, the order every "first by name"
/// reads.
pub(super) struct Servers<'a> {
    by_name: Vec<&'a McpServer>,
    rank: HashMap<i64, usize>,
}

impl<'a> Servers<'a> {
    pub(super) fn of(snap: &'a Snapshot) -> Self {
        let mut by_name: Vec<&McpServer> =
            snap.mcp_servers.values().filter(|s| s.enabled).collect();
        by_name.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        let rank = by_name.iter().enumerate().map(|(i, s)| (s.id, i)).collect();
        Self { by_name, rank }
    }

    /// The length of the longest tool prefix whose namespace holds
    /// `exposed`, if one does.
    fn namespace(&self, exposed: &str) -> Option<usize> {
        self.by_name
            .iter()
            .filter(|s| strip(&s.tool_prefix, exposed).is_some())
            .map(|s| s.tool_prefix.trim().len())
            .max()
    }

    /// Is `server` a candidate for `exposed`, whose [`namespace`](Self::namespace)
    /// is `ns` (module doc)?
    fn may_have(server: &McpServer, ns: Option<usize>, exposed: &str) -> bool {
        let prefix = server.tool_prefix.trim();
        match ns {
            Some(len) => prefix.len() == len && strip(prefix, exposed).is_some(),
            None => prefix.is_empty() || !has_authority(exposed),
        }
    }

    /// Is `server` a candidate for `exposed`?
    pub(super) fn is_candidate(&self, server: &McpServer, exposed: &str) -> bool {
        Self::may_have(server, self.namespace(exposed), exposed)
    }

    /// The servers `exposed` can belong to, by name, each with the URI in
    /// its own spelling; and whether it is in a prefix's namespace (module
    /// doc).
    pub(super) fn candidates(&self, exposed: &str) -> (Vec<(&'a McpServer, String)>, bool) {
        let ns = self.namespace(exposed);
        let cands = self
            .by_name
            .iter()
            .copied()
            .filter(|s| Self::may_have(s, ns, exposed))
            .map(|s| match ns {
                Some(_) => (s, strip(&s.tool_prefix, exposed).unwrap_or_default()),
                None => (s, exposed.to_string()),
            })
            .collect();
        (cands, ns.is_some())
    }
}

/// Which of `cands` (by name, as [`Servers::candidates`] gives them) has
/// `exposed`, by what `known` says of them: the strongest claim, the first
/// by name within it; `None` for a URI no server has.
pub(super) fn owner<'a>(
    cands: Vec<(&'a McpServer, String)>,
    in_namespace: bool,
    known: &HashMap<i64, Known>,
    exposed: &str,
) -> Option<(&'a McpServer, String)> {
    let best = cands
        .iter()
        .enumerate()
        .filter_map(|(i, (s, _))| known.get(&s.id)?.claim(exposed).map(|c| (c, i)))
        .min();
    match best {
        Some((_, i)) => cands.into_iter().nth(i),
        None if in_namespace => cands.into_iter().next(),
        None => None,
    }
}

/// Who has each URI that a known server's tools name or its listing lists:
/// `(claim, rank by name, server id)`, the strongest claim and the first by
/// name of the candidates. One pass, so a listing judges each of its URIs
/// by one lookup. A template never takes a listed URI (a listing is the
/// stronger claim), so templates have no part in it.
pub(super) fn listed_owners(
    servers: &Servers,
    known: &HashMap<i64, Known>,
) -> HashMap<String, (Claim, usize, i64)> {
    let mut out: HashMap<String, (Claim, usize, i64)> = HashMap::new();
    for k in known.values() {
        let Some(&rank) = servers.rank.get(&k.server.id) else {
            continue;
        };
        let p = &k.server.tool_prefix;
        let claims = k.ui.iter().map(|u| (u.clone(), Claim::Tool)).chain(
            k.resources
                .iter()
                .filter_map(uri_of)
                .map(|u| (namespaced(p, u), Claim::Listed)),
        );
        for (exposed, claim) in claims {
            if !servers.is_candidate(&k.server, &exposed) {
                continue;
            }
            let entry = (claim, rank, k.server.id);
            out.entry(exposed)
                .and_modify(|e| *e = (*e).min(entry))
                .or_insert(entry);
        }
    }
    out
}

/// Is `server` left unasked about `exposed` for `scope`'s caller: a device
/// row it does not reach, and a URI with an authority (module doc)?
pub(super) fn unasked(scope: &ToolScope, server: &McpServer, exposed: &str) -> bool {
    server.is_device() && has_authority(exposed) && !scope.may_reach(server)
}

/// The servers a caller does not reach that could take one of the URIs
/// listed by those in `reached`: a candidate earlier by name than the
/// server that lists it. A URI that server's own tool names needs none:
/// only another tool's claim beats it, and those are known without asking.
pub(super) fn rivals(
    servers: &Servers,
    known: &HashMap<i64, Known>,
    reached: &HashSet<i64>,
    scope: &ToolScope,
) -> HashSet<i64> {
    let mut out = HashSet::new();
    for k in reached.iter().filter_map(|id| known.get(id)) {
        for own in k.resources.iter().filter_map(uri_of) {
            let exposed = namespaced(&k.server.tool_prefix, own);
            if k.ui.contains(&exposed) {
                continue;
            }
            let (cands, _) = servers.candidates(&exposed);
            if !cands.iter().any(|(s, _)| s.id == k.server.id) {
                continue;
            }
            for (c, _) in cands.iter().take_while(|(c, _)| c.id != k.server.id) {
                if !reached.contains(&c.id)
                    && known.contains_key(&c.id)
                    && !unasked(scope, c, &exposed)
                {
                    out.insert(c.id);
                }
            }
        }
    }
    out
}
