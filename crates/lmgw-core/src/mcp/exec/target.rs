//! What a `{type: "mcp"}` entry's `server_label` names, decided as
//! [`resolve`](super::resolve) decides it: a built-in toolset by its reserved
//! label, else a registered server by its tool prefix, else by its name
//! ([`find_server`]). Two labels that name the same server — its prefix and
//! its name — are one target; that is what a check comparing entries keys
//! them by (client-apps design §6.6), never the label string.
//!
//! A label that names nothing now is its own target, by the label: it
//! resolves to nothing at a turn either, and is compared with the same label
//! only.

use crate::config::Snapshot;

use super::{builtin_label, find_server};

/// One target, for equality.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Target {
    /// A built-in toolset (`docs`, `kb`, `lmgw`).
    Builtin(&'static str),
    /// A registered server, by its id.
    Server(i64),
    /// A label that names nothing now, trimmed.
    Unresolved(String),
}

/// A label's [`Target`], with the namespace its tools are exposed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LabelTarget {
    pub target: Target,
    /// `<namespace>__` is what the exposed name adds to a tool's own:
    /// a built-in's label, a server's tool prefix; `None` for a server
    /// without one, whose exposed names are its own. A label that names
    /// nothing is read as the prefix a server answering it later would
    /// have.
    namespace: Option<String>,
    /// For a server, its name spelt as a prefix: the namespace a tool of it
    /// takes when another source offers a tool of its name
    /// ([`crate::mcp::names`]) — a bare server's, or one that shares its
    /// prefix. `None` when it is the namespace already, and for a built-in
    /// or a label that names nothing.
    qualifier: Option<String>,
    /// The server's tools a collision gave that namespace now, by their
    /// exposed names (`Aggregate::qualified`): only these read as
    /// `<qualifier>__<own>`. A server's own literal `<qualifier>__x` is a
    /// tool called that, not `x`.
    moved: Vec<String>,
}

impl LabelTarget {
    /// A tool name as a `tool_names` list may spell it, in the tool's own
    /// (upstream) spelling: `<namespace>__` taken off once. Both spellings
    /// of one tool then compare equal, as [`ApprovalRule::requires`]
    /// matches either.
    ///
    /// [`ApprovalRule::requires`]: crate::mcp::spec::ApprovalRule::requires
    pub(crate) fn canonical<'a>(&self, name: &'a str) -> &'a str {
        let strip = |ns: &Option<String>| {
            ns.as_deref()
                .and_then(|ns| name.strip_prefix(ns))
                .and_then(|rest| rest.strip_prefix("__"))
        };
        strip(&self.namespace)
            .or_else(|| {
                self.moved
                    .iter()
                    .any(|m| m == name)
                    .then(|| strip(&self.qualifier))
                    .flatten()
            })
            .unwrap_or(name)
    }

    /// Every spelling of `name` a turn matches for this target: as written,
    /// the tool's own ([`Self::canonical`]), the exposed
    /// `<namespace>__<own>` and, for a tool a collision moved, the
    /// `<qualifier>__<own>` it has now. A list that holds them all matches
    /// the tool whichever spelling its rule was written in.
    pub(crate) fn spellings(&self, name: &str) -> Vec<String> {
        let own = self.canonical(name);
        let mut out = vec![name.to_string(), own.to_string()];
        if let Some(ns) = &self.namespace {
            out.push(format!("{ns}__{own}"));
        }
        if let Some(q) = &self.qualifier {
            let moved = format!("{q}__{own}");
            if self.moved.contains(&moved) {
                out.push(moved);
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

/// What `label` names now (module doc). Spaces around it are trimmed, as
/// the tool-scope check reads it: two labels apart only by them are one.
pub(crate) fn label_target(snap: &Snapshot, label: &str) -> LabelTarget {
    let label = label.trim();
    if let Some(b) = builtin_label(label) {
        return LabelTarget {
            target: Target::Builtin(b),
            namespace: Some(b.to_string()),
            qualifier: None,
            moved: Vec::new(),
        };
    }
    match find_server(snap, label) {
        Some(s) => {
            let namespace = Some(s.tool_prefix.trim().to_string()).filter(|p| !p.is_empty());
            let q = crate::mcp::names::name_qualifier(&s.name);
            LabelTarget {
                target: Target::Server(s.id),
                qualifier: Some(q).filter(|q| !q.is_empty() && Some(q) != namespace.as_ref()),
                namespace,
                moved: Vec::new(),
            }
        }
        None => LabelTarget {
            target: Target::Unresolved(label.to_string()),
            namespace: Some(label.to_string()).filter(|l| !l.is_empty()),
            qualifier: None,
            moved: Vec::new(),
        },
    }
}

/// [`label_target`] with the tools of its server a collision moved now
/// (`agg`), so a name in the server's name's namespace reads as its own
/// only for those ([`LabelTarget::canonical`]).
pub(crate) fn label_target_in(
    snap: &Snapshot,
    agg: &crate::mcp::Aggregate,
    label: &str,
) -> LabelTarget {
    let mut t = label_target(snap, label);
    if let Target::Server(id) = t.target {
        t.moved = agg
            .qualified
            .keys()
            .filter(|e| agg.reverse.get(*e).is_some_and(|(s, _)| *s == id))
            .cloned()
            .collect();
    }
    t
}

#[cfg(test)]
mod tests;
