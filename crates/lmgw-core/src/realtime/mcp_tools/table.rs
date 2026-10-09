//! The session's tool table (realtime-server-tools design §1.2, §1.3): per
//! `mcp` label of its tools, how its listing went and what it listed, and
//! the reverse map from an exposed name to the label the client wrote and
//! the tool's wire name.
//!
//! What a response needs of the session's MCP tools is asked here and never
//! built: which tools its `tools` offer — a response's own `mcp` entry
//! selects or narrows a label the session listed (§1.1) — what a
//! `tool_choice` naming a label maps to, and whose a name the model called
//! is. An exposed name is never made by concatenation: a bare server, a
//! renamed tool and a label that addresses a server by name all break it.
//!
//! A response asks twice ([`Moment`]): when it is created, against what is
//! listed so far — a label still being listed is decided at the launch,
//! once the response has waited for it — and at the launch.
//!
//! A label whose listing failed offers nothing and holds nothing back: the
//! client was told why when it failed (`listing`), and a voice client keeps
//! talking without that server's tools. Only a `tool_choice` that names it
//! is refused.

use std::collections::HashMap;

use super::super::protocol::{ErrorObject, McpChoice, Tool, ToolChoice};
use super::spec_of;
use crate::ir::{self, ToolDef};
use crate::mcp::exec::{LabelTool, LabelTools};
use crate::mcp::spec::McpToolSpec;

/// The session's `mcp` labels, in the order they were first listed.
#[derive(Debug, Default)]
pub(crate) struct McpTable {
    labels: Vec<Entry>,
    /// Exposed name → whose it is, over every listed label.
    reverse: HashMap<String, Owner>,
    /// (label, wire name) → exposed name, over every label ever listed —
    /// one a later update dropped too: a client's replayed call of it still
    /// renders by its name (§2.6).
    named: HashMap<(String, String), String>,
    /// The latest listing's number: a result is its label's current one
    /// only while the label still waits for that number.
    seq: u64,
}

#[derive(Debug)]
struct Entry {
    /// As the client wrote it (§1.3).
    label: String,
    /// The `allowed_tools` it was listed by: an unchanged definition is not
    /// listed again.
    allowed: Option<Vec<String>>,
    state: State,
}

#[derive(Debug)]
enum State {
    Listing {
        seq: u64,
    },
    Listed(LabelTools),
    /// Why it has no tools.
    Failed(String),
}

/// Whose an exposed name is (§1.3): the label the client wrote, the tool's
/// wire name, whether a built-in toolset runs it in-process — and, for a
/// registered server's, which server it was listed from: its calls run there
/// or not at all, whatever another server has come to own the name since
/// (`McpManager::call_listed`, final review #2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Owner {
    pub label: String,
    pub wire: String,
    pub builtin: bool,
    /// The registered server's id; `None` for a built-in toolset.
    pub server: Option<i64>,
}

/// What a `session.update` does to the table ([`McpTable::plan`]).
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Plan {
    /// Labels to list: new ones, and those whose `allowed_tools` changed.
    pub list: Vec<McpToolSpec>,
    /// Labels it keeps as they are, listed or being listed.
    pub keep: Vec<String>,
    /// Labels it drops: their tools go, their items stay in the
    /// conversation.
    pub drop: Vec<String>,
}

/// When a response asks ([`McpTable::offer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Moment {
    /// At `response.create`: a label the session never listed is refused,
    /// one still being listed is decided at the launch.
    Created,
    /// At the launch, nothing in flight: a label a later `session.update`
    /// dropped offers nothing.
    Launched,
}

/// What a response offers of the session's MCP tools (§2.1), and its
/// `tool_choice` mapped (§1.1).
#[derive(Debug, Default, PartialEq)]
pub(crate) struct McpOffer {
    /// Their definitions by exposed name, in the order of the response's
    /// `tools`.
    pub tools: Vec<ToolDef>,
    /// `{type: "mcp", server_label, name}` → that tool; without a `name`,
    /// `Required`.
    pub choice: Option<ir::ToolChoice>,
    /// A choice without a `name` narrows the response to its label: these
    /// are then that label's tools alone, and the functions are not offered.
    pub only: Option<String>,
}

impl McpTable {
    fn entry(&self, label: &str) -> Option<&Entry> {
        self.labels.iter().find(|e| e.label == label)
    }

    /// Whether a listing is in flight: a response waits for it before it
    /// renders (§1.2).
    pub fn listing(&self) -> bool {
        self.labels
            .iter()
            .any(|e| matches!(e.state, State::Listing { .. }))
    }

    /// What `tools` — the session's tools after an update — does to the
    /// table. Pure: the update may still be refused. `named`: the update
    /// carried a `tools` array, so it names each of them — and a label whose
    /// last listing failed is listed again, unchanged or not: a client keeps
    /// one socket open for hours, and a server down at its first listing may
    /// be up now.
    pub fn plan(&self, tools: &[Tool], named: bool) -> Plan {
        let mut plan = Plan::default();
        for t in tools.iter().filter_map(Tool::as_mcp) {
            // `merge` checked every entry, so each parses.
            let Ok(spec) = spec_of(t) else {
                continue;
            };
            match self.entry(&spec.server_label) {
                Some(e)
                    if e.allowed == spec.allowed_tools
                        && !(named && matches!(e.state, State::Failed(_))) =>
                {
                    plan.keep.push(spec.server_label)
                }
                _ => plan.list.push(spec),
            }
        }
        plan.drop = self
            .labels
            .iter()
            .filter(|e| {
                !tools
                    .iter()
                    .filter_map(Tool::as_mcp)
                    .any(|t| t.server_label == e.label)
            })
            .map(|e| e.label.clone())
            .collect();
        plan
    }

    /// `spec`'s listing starts: its label waits for the number returned.
    pub fn start(&mut self, spec: &McpToolSpec) -> u64 {
        self.seq += 1;
        let state = State::Listing { seq: self.seq };
        match self
            .labels
            .iter_mut()
            .find(|e| e.label == spec.server_label)
        {
            Some(e) => {
                e.allowed.clone_from(&spec.allowed_tools);
                e.state = state;
            }
            None => self.labels.push(Entry {
                label: spec.server_label.clone(),
                allowed: spec.allowed_tools.clone(),
                state,
            }),
        }
        self.rebuild();
        self.seq
    }

    /// `label` left the session's tools.
    pub fn drop_label(&mut self, label: &str) {
        self.labels.retain(|e| e.label != label);
        self.rebuild();
    }

    /// Whether listing `seq` is still `label`'s current one — not dropped or
    /// redefined since it started.
    pub fn current(&self, label: &str, seq: u64) -> bool {
        self.entry(label)
            .is_some_and(|e| matches!(e.state, State::Listing { seq: s } if s == seq))
    }

    /// `label`'s current listing `seq` ended: its tools, or why it has none.
    pub fn settle(&mut self, label: &str, seq: u64, result: Result<LabelTools, String>) {
        if !self.current(label, seq) {
            return;
        }
        if let Some(e) = self.labels.iter_mut().find(|e| e.label == label) {
            e.state = match result {
                Ok(tools) => State::Listed(tools),
                Err(why) => State::Failed(why),
            };
        }
        self.rebuild();
    }

    /// Why `label`'s freshly listed `tools` cannot join a session whose
    /// tools are `session_tools`: one of them would share its name with a
    /// function, or with another label's tool — the model could not tell
    /// them apart, and a call could not be routed (§1.1). Another label for
    /// the same server is named as such; one for another server is a bare
    /// sibling that took the name since the other label was listed.
    pub fn clash_of(
        &self,
        label: &str,
        tools: &LabelTools,
        session_tools: &[Tool],
    ) -> Option<String> {
        tools.tools.iter().find_map(|t| {
            let name = t.def.name.as_str();
            if session_tools
                .iter()
                .any(|s| matches!(s, Tool::Function { name: f, .. } if f == name))
            {
                return Some(format!(
                    "its tool '{name}' has the name of the session's function '{name}', and the \
                     model could not tell the two apart: rename the function, or leave the tool \
                     out with allowed_tools"
                ));
            }
            let o = self.reverse.get(name).filter(|o| o.label != label)?;
            Some(if o.server == tools.server {
                format!(
                    "its tool '{name}' is already in this session as a tool of MCP server_label \
                     '{}': both labels name the same server — name it once",
                    o.label
                )
            } else {
                format!(
                    "its tool '{name}' has the name of a tool this session listed for MCP \
                     server_label '{}', another server: both offer a tool of that name, and the \
                     gateway gives each its server's name as a prefix — list '{}' again, \
                     or leave the tool out of one label with allowed_tools",
                    o.label, o.label
                )
            })
        })
    }

    /// The functions of `tools` (at `param`) whose name is an exposed name
    /// of a label `keeps` admits, refused (§1.1): a client function and a
    /// server tool of one name could not be told apart.
    pub fn clash(
        &self,
        tools: &[Tool],
        param: &str,
        keeps: impl Fn(&str) -> bool,
    ) -> Result<(), ErrorObject> {
        for (n, t) in tools.iter().enumerate() {
            let Tool::Function { name, .. } = t else {
                continue;
            };
            if let Some(owner) = self.reverse.get(name).filter(|o| keeps(&o.label)) {
                return Err(ErrorObject::invalid(
                    "invalid_value",
                    format!(
                        "function '{name}' has the name of a tool this session listed for MCP \
                         server_label '{}', and the model could not tell the two apart: rename \
                         the function",
                        owner.label
                    ),
                )
                .with_param(format!("{param}[{n}].name")));
            }
        }
        Ok(())
    }

    /// What a response whose tools are `tools` offers of the session's MCP
    /// tools, and its `choice` mapped — or why it cannot be made. `params`
    /// name where the tools and the choice came from (`response.tools`,
    /// `session.tool_choice`, …).
    pub fn offer(
        &self,
        tools: &[Tool],
        choice: Option<&ToolChoice>,
        (tools_param, choice_param): (&str, &str),
        at: Moment,
    ) -> Result<McpOffer, ErrorObject> {
        let mut offered: Vec<(&str, Vec<&LabelTool>)> = Vec::new();
        for (n, tool) in tools.iter().enumerate() {
            let Some(t) = tool.as_mcp() else {
                continue;
            };
            let label = t.server_label.as_str();
            let at_entry = format!("{tools_param}[{n}]");
            let entry = match self.entry(label) {
                Some(e) => e,
                None if at == Moment::Launched => continue,
                None => {
                    return Err(ErrorObject::invalid(
                        "invalid_value",
                        format!(
                            "{at_entry} names MCP server_label '{label}', which this session has \
                             not listed: list it in session.update first — a response does not \
                             list a label of its own"
                        ),
                    )
                    .with_param(format!("{at_entry}.server_label")))
                }
            };
            let State::Listed(listed) = &entry.state else {
                continue;
            };
            let spec = spec_of(t).map_err(|(field, why)| {
                ErrorObject::invalid("invalid_value", why).with_param(format!("{at_entry}.{field}"))
            })?;
            // The session's own definition offers what it listed; another
            // `allowed_tools` narrows that, by either name (§1.1).
            let narrowed: Vec<&LabelTool> = listed
                .tools
                .iter()
                .filter(|x| {
                    spec.allowed_tools == entry.allowed || spec.allows(&x.def.name, &x.wire)
                })
                .collect();
            if narrowed.is_empty() {
                return Err(ErrorObject::invalid(
                    "invalid_value",
                    format!(
                        "{at_entry}.allowed_tools leaves none of the tools this session listed for \
                         MCP server_label '{label}' (it listed: {}) — a response selects or \
                         narrows a label's tools, and lists none of its own",
                        wire_names(&listed.tools.iter().collect::<Vec<_>>())
                    ),
                )
                .with_param(format!("{at_entry}.allowed_tools")));
            }
            offered.push((label, narrowed));
        }

        let mut offer = McpOffer::default();
        if let Some(ToolChoice::Mcp(c)) = choice {
            let refused =
                |why: String| ErrorObject::invalid("invalid_value", why).with_param(choice_param);
            match self.choose(c, tools, &offered, at).map_err(refused)? {
                Some(Pick::Tool(name)) => offer.choice = Some(ir::ToolChoice::Tool { name }),
                Some(Pick::Label) => {
                    offer.choice = Some(ir::ToolChoice::Required);
                    offer.only = Some(c.server_label.clone());
                }
                None => {}
            }
        }
        offer.tools = offered
            .into_iter()
            .filter(|(label, _)| offer.only.as_deref().is_none_or(|only| only == *label))
            .flat_map(|(_, tools)| tools.into_iter().map(|t| t.def.clone()))
            .collect();
        Ok(offer)
    }

    /// What `{type: "mcp", server_label, name?}` picks among `offered`;
    /// `None` while its label is still being listed at `Moment::Created`.
    fn choose(
        &self,
        c: &McpChoice,
        tools: &[Tool],
        offered: &[(&str, Vec<&LabelTool>)],
        at: Moment,
    ) -> Result<Option<Pick>, String> {
        let label = c.server_label.as_str();
        if !tools
            .iter()
            .filter_map(Tool::as_mcp)
            .any(|t| t.server_label == label)
        {
            return Err(format!(
                "tool_choice names MCP server_label '{label}', which is not among this response's \
                 tools"
            ));
        }
        match self.entry(label).map(|e| &e.state) {
            Some(State::Listing { .. }) if at == Moment::Created => return Ok(None),
            Some(State::Failed(why)) => {
                return Err(format!(
                    "tool_choice names MCP server_label '{label}', whose listing failed: {why}"
                ))
            }
            Some(State::Listed(_)) => {}
            Some(State::Listing { .. }) | None => {
                return Err(format!(
                    "tool_choice names MCP server_label '{label}', which this session has no tools \
                     of"
                ))
            }
        }
        let of_label: &[&LabelTool] = offered
            .iter()
            .find(|(l, _)| *l == label)
            .map_or(&[], |(_, t)| t.as_slice());
        let Some(name) = &c.name else {
            return Ok(Some(Pick::Label));
        };
        // Either name, as `allowed_tools` takes them: the client sees the
        // wire name, the exposed one is what the model is offered.
        match of_label
            .iter()
            .find(|t| t.wire == *name || t.def.name == *name)
        {
            Some(t) => Ok(Some(Pick::Tool(t.def.name.clone()))),
            None => Err(format!(
                "tool_choice names '{name}', which is not among the tools of MCP server_label \
                 '{label}' this response offers (it has: {})",
                wire_names(of_label)
            )),
        }
    }

    /// Rebuild the reverse map from the listed labels; their names join
    /// the ones kept for rendering.
    fn rebuild(&mut self) {
        self.reverse = self
            .labels
            .iter()
            .filter_map(|e| match &e.state {
                State::Listed(l) => Some((e, l)),
                _ => None,
            })
            .flat_map(|(e, l)| {
                l.tools.iter().map(move |t| {
                    let owner = Owner {
                        label: e.label.clone(),
                        wire: t.wire.clone(),
                        builtin: l.builtin.contains(&t.def.name),
                        server: l.server,
                    };
                    (t.def.name.clone(), owner)
                })
            })
            .collect();
        for (exposed, o) in &self.reverse {
            self.named
                .insert((o.label.clone(), o.wire.clone()), exposed.clone());
        }
    }
}

/// The call's half (realtime-server-tools §2.1, §2.4, §2.6).
impl McpTable {
    /// Whose `exposed` is — the label the client wrote and the tool's wire
    /// name — or `None` for a name no listed label has: a client function.
    pub fn owner(&self, exposed: &str) -> Option<&Owner> {
        self.reverse.get(exposed)
    }

    /// The exposed names a built-in toolset runs in-process, over every
    /// listed label: the split executor's in-process half.
    pub fn builtin(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .reverse
            .iter()
            .filter(|(_, o)| o.builtin)
            .map(|(n, _)| n.clone())
            .collect();
        names.sort();
        names
    }

    /// The exposed name `label`'s tool `wire` was listed under, whether or
    /// not the label is still the session's — for rendering a client's
    /// replayed call (§2.6).
    pub fn exposed(&self, label: &str, wire: &str) -> Option<&str> {
        self.named
            .get(&(label.to_string(), wire.to_string()))
            .map(String::as_str)
    }
}

/// What an `mcp` tool_choice picks.
enum Pick {
    /// One tool, by its exposed name.
    Tool(String),
    /// Any tool of its label.
    Label,
}

fn wire_names(tools: &[&LabelTool]) -> String {
    tools
        .iter()
        .map(|t| t.wire.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests;
