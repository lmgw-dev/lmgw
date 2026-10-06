//! The session's MCP tools in a rendered request (realtime-server-tools
//! design §2.1, §2.6).
//!
//! **Offered** ([`McpInput`]): the tools a response offers of the session's
//! labels go after the client's functions, by their exposed names, their
//! `$schema` keys taken out as a function's are; an `mcp` tool_choice maps
//! to the choice the table gave it, and one without a `name` narrows the
//! response to its label, so the functions are not offered.
//!
//! **Rendered back** ([`McpRendered`]): an `mcp_call` is an assistant tool
//! call — by the name the model called it, its arguments — followed directly
//! by its result: the blocks the session kept (images included), or the
//! item's `output`, or its `error` message marked as an error. Calls of one
//! response share one assistant turn, as function calls do. A call with
//! neither output nor error — only a client's replayed item can be one —
//! gets the synthetic [`NO_RESULT_YET`]. The call id is the one minted for
//! the item when it was made or replayed; no `mcp_call` carries one.
//!
//! A call made in this session renders by the name the model called it,
//! even after its label left. A client's replayed item renders by the
//! exposed name its label and tool were listed under, and — for a label
//! the session never listed — by `<label>__<name>` as a last resort.

use std::collections::HashMap;

use super::super::conversation::Conversation;
use super::super::mcp_tools::McpTable;
use super::super::protocol::McpCallItem;
use super::{strip_schema_keys, Builder, NO_RESULT_YET};
use crate::ir::{self, ToolDef, ToolResultBlock};

/// What a response offers of the session's MCP tools, and how its
/// `mcp_call` items render ([`super::Input::mcp`]).
pub struct McpInput<'a> {
    /// The tools, by exposed name, after the client's functions.
    pub tools: &'a [ToolDef],
    /// The response's `mcp` tool_choice as the table mapped it; it wins
    /// over [`super::Input::tool_choice`].
    pub choice: Option<&'a ir::ToolChoice>,
    /// The choice narrows the response to one label: only its tools are
    /// offered.
    pub only: bool,
    /// How each `mcp_call` item renders.
    pub call: &'a dyn Fn(&McpCallItem) -> McpRendered,
}

impl Default for McpInput<'_> {
    /// No MCP tools; every call renders from its item alone: its item id
    /// as its call id, `<label>__<name>` as its name.
    fn default() -> Self {
        Self {
            tools: &[],
            choice: None,
            only: false,
            call: &unkept,
        }
    }
}

/// How one `mcp_call` renders (module doc).
#[derive(Debug, Clone, PartialEq)]
pub struct McpRendered {
    pub call_id: String,
    /// The exposed name the model knows the tool by.
    pub name: String,
    /// Its result's blocks, and whether they say it failed; `None` when it
    /// has neither output nor error.
    pub result: Option<(Vec<ToolResultBlock>, bool)>,
}

/// How `c` renders with nothing kept beside it: its item id as its call id,
/// `<label>__<name>` as its name, its result as the item says.
fn unkept(c: &McpCallItem) -> McpRendered {
    rendered(c, None, None, None)
}

/// How `c` renders, given its `call_id`, the exposed `name` it is known by
/// and the result `blocks` kept for it — each as far as known (module doc).
fn rendered(
    c: &McpCallItem,
    call_id: Option<&str>,
    name: Option<&str>,
    blocks: Option<&[ToolResultBlock]>,
) -> McpRendered {
    let call_id = call_id
        .or(c.id.as_deref())
        .unwrap_or("mcp_call")
        .to_string();
    let name = name.map_or_else(|| format!("{}__{}", c.server_label, c.name), str::to_string);
    let result = match (&c.error, &c.output) {
        (Some(e), _) => Some((
            blocks.map_or_else(|| ToolResultBlock::one(e.message.clone()), <[_]>::to_vec),
            true,
        )),
        (None, Some(out)) => Some((
            blocks.map_or_else(|| ToolResultBlock::one(out.clone()), <[_]>::to_vec),
            false,
        )),
        (None, None) => None,
    };
    McpRendered {
        call_id,
        name,
        result,
    }
}

/// How `c` renders in this session: what the conversation kept of it, and
/// the table's names for a client's replayed item (module doc).
pub(crate) fn of_session(c: &McpCallItem, conv: &Conversation, table: &McpTable) -> McpRendered {
    let kept = c.id.as_deref().and_then(|id| conv.mcp_record(id));
    let name = kept
        .and_then(|k| k.exposed.clone())
        .or_else(|| table.exposed(&c.server_label, &c.name).map(str::to_string));
    rendered(
        c,
        kept.map(|k| k.call_id.as_str()),
        name.as_deref(),
        kept.and_then(|k| k.blocks.as_deref()),
    )
}

/// The tools a response offers (module doc): the client's `functions`
/// unless the choice narrows it to one label, then the MCP tools.
pub(super) fn offered(functions: Vec<ToolDef>, mcp: &McpInput<'_>) -> Vec<ToolDef> {
    let mut tools = if mcp.only { Vec::new() } else { functions };
    tools.extend(mcp.tools.iter().map(|t| {
        let mut t = t.clone();
        strip_schema_keys(&mut t.parameters);
        t
    }));
    tools
}

/// The results of the `mcp_call`s rendered so far, by call id: each is
/// given when its call's turn is flushed.
#[derive(Default)]
pub(super) struct McpResults(HashMap<String, (Vec<ToolResultBlock>, bool)>);

impl McpResults {
    pub fn has(&self, call_id: &str) -> bool {
        self.0.contains_key(call_id)
    }

    pub fn take(&mut self, call_id: &str) -> Option<(Vec<ToolResultBlock>, bool)> {
        self.0.remove(call_id)
    }
}

impl Builder<'_> {
    /// An `mcp_call` rendered as `r`, with `raw_args` as streamed: the call
    /// joins the assistant turn, and its result follows it.
    pub(super) fn mcp_call(&mut self, r: McpRendered, raw_args: &str) {
        let result = r
            .result
            .unwrap_or_else(|| (ToolResultBlock::one(NO_RESULT_YET), false));
        self.mcp.0.insert(r.call_id.clone(), result);
        self.call(&r.call_id, &r.name, raw_args);
    }
}
