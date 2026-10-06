//! A response's server-side calls on the wire (realtime-server-tools design
//! §2.2–§2.4).
//!
//! At `ToolCallStart` a name the response offered as an MCP tool (§2.1,
//! [`Output::serve_mcp`]) opens an `mcp_call` item — `server_label` as the
//! client wrote it and the tool's wire name (§1.3), `arguments: ""`,
//! `output` and `error` `null` — announced as a function call's is; any
//! other name is the client's function. Its arguments go out as
//! `response.mcp_call_arguments.delta`. The item carries no `call_id`: the
//! one minted for it is kept beside it in the conversation, for rendering
//! (§2.6).
//!
//! At the model's `Stop` each call's `response.mcp_call_arguments.done`
//! goes out, and the items **stay open**, each in a [`Stage`]: *unmade*
//! (announced, not started), *running*, *done*. An open call keeps
//! `all_closed` false, so a cancel still has something to stop. The
//! responder's reports move them on: `response.mcp_call.in_progress` when
//! one starts, then `response.mcp_call.completed` with the result's text as
//! `output`, or `.failed` with `output: null` and a `tool_execution_error`
//! — and the item's `output_item.done` and `conversation.item.done`. The
//! conversation keeps the result's blocks, images included; only the wire
//! `output` is flattened text.
//!
//! A response cut off (`Length`, `ContentFilter`) runs none of its calls,
//! and one whose stream failed after its `Stop` runs none either: their
//! calls close as never made ([`UNMADE_CALL`]) — at the `Stop`, or when the
//! response ends ([`Output::mcp_unfinished`]), where a call still running
//! closes as abandoned ([`ABANDONED_CALL`]): it was sent, so it may have
//! run. A cancel closes them the same way (`Output::abandon`, §2.5): the
//! responder's later reports are an old generation's, and dropped.
//!
//! **Sent is not the same as reported** (§2.5). In speech mode a call's
//! `in_progress` reaches the core through the speaker's queue, behind the
//! clauses before it, while the call itself starts at once; the responder
//! also says so directly ([`Output::mcp_sent`]), so a cancel in between
//! closes the call as abandoned — the server has it — and not as never
//! made, which would tell the model it may simply call it again. So are its
//! own deltas queued — the call's item may not be announced yet when the
//! response ends: cancelled, or its voice failed with them queued. Such a
//! call is announced then, from what the responder said of it — its item,
//! its arguments in one delta and their done — and closes as abandoned too:
//! a call the server has must never vanish from the response and the
//! conversation, or the model would make it again.

use std::collections::{BTreeMap, HashMap};

use super::super::conversation::Conversation;
use super::super::ids::Ids;
use super::super::mcp_tools::Owner;
use super::super::protocol::{Item, McpCallError, McpCallItem, McpErrorKind, ServerEvent};
use super::super::responder::SentCall;
use super::super::writer::Outbox;
use super::Output;
use crate::agent::{ToolOutcome, ABANDONED_CALL, UNMADE_CALL};
use crate::ir::flatten_tool_result;

/// Where a server-side call is (module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Unmade,
    Running,
    Done,
}

/// A response's server-side calls.
#[derive(Debug, Default)]
pub(super) struct McpCalls {
    /// The owners of the tools the response offered, by exposed name.
    owners: HashMap<String, Owner>,
    /// Each call's stage, by output index.
    stages: BTreeMap<u32, Stage>,
    /// The calls' `response.mcp_call_arguments.done` went out.
    args_done: bool,
    /// The calls that went to their server (module doc), by upstream
    /// ordinal, whether or not their item is announced yet.
    sent: BTreeMap<usize, SentCall>,
}

impl Output {
    /// The response offers these MCP tools, by exposed name (§2.1): a call
    /// of one of them is server-side. Set at its launch.
    pub fn serve_mcp(&mut self, owners: HashMap<String, Owner>) {
        self.mcp.owners = owners;
    }

    /// Whether the item at `at` is a server-side call.
    pub(super) fn is_mcp(&self, at: u32) -> bool {
        self.mcp.stages.contains_key(&at)
    }

    /// A call of `name`, if the response offered it as an MCP tool: its
    /// item opens with `call_id` kept beside it. `false`: a client's call.
    pub(super) fn mcp_start(
        &mut self,
        conv: &mut Conversation,
        ids: &Ids,
        ob: &mut Outbox,
        (index, call_id): (usize, &str),
        name: &str,
    ) -> bool {
        let Some(owner) = self.mcp.owners.get(name) else {
            return false;
        };
        let item_id = conv.fresh_item_id(ids);
        let item = Item::McpCall(Box::new(McpCallItem {
            id: Some(item_id.clone()),
            server_label: owner.label.clone(),
            name: owner.wire.clone(),
            arguments: String::new(),
            approval_request_id: None,
            output: None,
            error: None,
        }));
        let at = self.announce(conv, ob, item);
        conv.note_mcp_call(&item_id, call_id, name);
        self.calls.insert(index, at);
        self.mcp.stages.insert(at, Stage::Unmade);
        true
    }

    /// A fragment of the arguments of the call at `at`, if it is a
    /// server-side one.
    pub(super) fn mcp_args(
        &mut self,
        conv: &mut Conversation,
        ob: &mut Outbox,
        at: u32,
        delta: &str,
    ) -> bool {
        if !self.is_mcp(at) {
            return false;
        }
        let item_id = self.items[at as usize].clone();
        if let Some(Item::McpCall(c)) = conv.get_mut(&item_id) {
            c.arguments.push_str(delta);
            ob.send(ServerEvent::McpCallArgumentsDelta {
                response_id: self.id.clone(),
                item_id,
                output_index: at,
                delta: delta.to_string(),
            });
        }
        true
    }

    /// The model's `Stop` (module doc): every call's arguments are done. A
    /// response that was `cut` off runs none of them.
    pub(super) fn mcp_generated(
        &mut self,
        conv: &mut Conversation,
        ids: &Ids,
        ob: &mut Outbox,
        cut: bool,
    ) {
        if !std::mem::replace(&mut self.mcp.args_done, true) {
            for &at in self.mcp.stages.keys() {
                let item_id = self.items[at as usize].clone();
                let Some(Item::McpCall(c)) = conv.get(&item_id) else {
                    continue;
                };
                ob.send(ServerEvent::McpCallArgumentsDone {
                    response_id: self.id.clone(),
                    item_id,
                    output_index: at,
                    arguments: c.arguments.clone(),
                });
            }
        }
        if cut {
            self.mcp_unfinished(conv, ids, ob);
        }
    }

    /// The responder started the call it knows by `index`.
    pub fn mcp_running(&mut self, ob: &mut Outbox, index: usize) {
        let Some(&at) = self.calls.get(&index) else {
            return;
        };
        match self.mcp.stages.get_mut(&at) {
            Some(s @ Stage::Unmade) => *s = Stage::Running,
            _ => return,
        }
        ob.send(ServerEvent::McpCallInProgress {
            item_id: self.items[at as usize].clone(),
            output_index: at,
        });
    }

    /// The call the responder knows by `index` ended with `outcome`: its
    /// text is the item's `output`, or its `error` when the tool failed,
    /// and its blocks are kept for the model (module doc). A sent call
    /// whose item never came is announced first: a voice that failed with
    /// the call's deltas queued sends its result straight here.
    pub fn mcp_done(
        &mut self,
        conv: &mut Conversation,
        ids: &Ids,
        ob: &mut Outbox,
        index: usize,
        outcome: ToolOutcome,
    ) {
        let Some(at) = self.mcp_at(conv, ids, ob, index) else {
            return;
        };
        if !matches!(
            self.mcp.stages.get(&at),
            Some(Stage::Unmade | Stage::Running)
        ) {
            return;
        }
        let (text, _) = flatten_tool_result(&outcome.blocks);
        let result = if outcome.is_error {
            Err(text)
        } else {
            Ok(text)
        };
        conv.keep_mcp_result(&self.items[at as usize], outcome.blocks);
        self.mcp_close(conv, ob, at, result);
    }

    /// Every call not done yet closes (module doc): a running or sent one as
    /// abandoned, any other as never made — each with
    /// `response.mcp_call.failed` and its done events. A sent call whose
    /// item was never announced is announced first.
    pub fn mcp_unfinished(&mut self, conv: &mut Conversation, ids: &Ids, ob: &mut Outbox) {
        let unannounced: Vec<usize> = self
            .mcp
            .sent
            .keys()
            .filter(|index| !self.calls.contains_key(index))
            .copied()
            .collect();
        for index in unannounced {
            self.mcp_at(conv, ids, ob, index);
        }
        let sent: Vec<u32> = self
            .mcp
            .sent
            .keys()
            .filter_map(|index| self.calls.get(index).copied())
            .collect();
        let open: Vec<(u32, Stage)> = self
            .mcp
            .stages
            .iter()
            .filter(|(_, s)| **s != Stage::Done)
            .map(|(at, s)| (*at, *s))
            .collect();
        for (at, stage) in open {
            let why = match stage {
                Stage::Running => ABANDONED_CALL,
                _ if sent.contains(&at) => ABANDONED_CALL,
                _ => UNMADE_CALL,
            };
            self.mcp_close(conv, ob, at, Err(why.to_string()));
        }
    }

    /// `call` went to its server, whatever its deltas and reports still
    /// wait behind (module doc).
    pub fn mcp_sent(&mut self, call: SentCall) {
        self.mcp.sent.insert(call.index, call);
    }

    /// The output index of the call the responder knows by `index` — a
    /// sent call whose own deltas have not reached the core is announced
    /// now, as they would have (module doc). `None`: no such call.
    fn mcp_at(
        &mut self,
        conv: &mut Conversation,
        ids: &Ids,
        ob: &mut Outbox,
        index: usize,
    ) -> Option<u32> {
        if let Some(&at) = self.calls.get(&index) {
            return Some(at);
        }
        let call = self.mcp.sent.get(&index)?.clone();
        let call_id = self.call_id(conv, ids, &call.id, &call.name);
        if !self.mcp_start(conv, ids, ob, (index, &call_id), &call.name) {
            return None;
        }
        let at = *self.calls.get(&index)?;
        tracing::debug!(
            "realtime response {}: call '{}' went to its server before its item was announced; \
             announced now",
            self.id,
            call.name
        );
        if !call.args.is_empty() {
            self.mcp_args(conv, ob, at, &call.args);
        }
        ob.send(ServerEvent::McpCallArgumentsDone {
            response_id: self.id.clone(),
            item_id: self.items[at as usize].clone(),
            output_index: at,
            arguments: call.args,
        });
        Some(at)
    }

    /// Whether the response made a server-side call.
    pub fn has_mcp(&self) -> bool {
        !self.mcp.stages.is_empty()
    }

    /// Whether a server-side call of the response is not done yet: unmade
    /// or running.
    pub fn mcp_open(&self) -> bool {
        self.mcp.stages.values().any(|s| *s != Stage::Done)
    }

    /// Close the call at `at` with its output, or the message of its
    /// `tool_execution_error`.
    fn mcp_close(
        &mut self,
        conv: &mut Conversation,
        ob: &mut Outbox,
        at: u32,
        result: Result<String, String>,
    ) {
        self.mcp.stages.insert(at, Stage::Done);
        self.closed[at as usize] = true;
        let item_id = self.items[at as usize].clone();
        conv.mcp_call_done(&item_id);
        // Always there: its delete is refused until now (§2.5).
        let Some(Item::McpCall(c)) = conv.get_mut(&item_id) else {
            return;
        };
        let completed = result.is_ok();
        match result {
            Ok(output) => {
                c.output = Some(output);
                c.error = None;
            }
            Err(message) => {
                c.output = None;
                c.error = Some(McpCallError {
                    kind: McpErrorKind::ToolExecutionError,
                    code: None,
                    message,
                });
            }
        }
        let item = Item::McpCall(c.clone());
        let (item_id, output_index) = (item_id.clone(), at);
        ob.send(if completed {
            ServerEvent::McpCallCompleted {
                item_id: item_id.clone(),
                output_index,
            }
        } else {
            ServerEvent::McpCallFailed {
                item_id: item_id.clone(),
                output_index,
            }
        });
        ob.send(ServerEvent::OutputItemDone {
            response_id: self.id.clone(),
            output_index,
            item: item.clone(),
        });
        ob.send(ServerEvent::ItemDone {
            previous_item_id: conv.previous_of(&item_id),
            item,
        });
    }
}
