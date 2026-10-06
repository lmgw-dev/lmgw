//! The conversation (realtime design §7.1): the session's ordered items, and
//! the `conversation.item.*` client events that edit it.
//!
//! **In memory, for the session only**, as with OpenAI — nothing here is
//! persisted. The session core is the only writer (§4.1): client events land
//! here through it, and a response's own items are appended by the
//! response's output ([`super::output`]) as they are announced, so a
//! `retrieve` of an assistant item mid-response finds it.
//!
//! **`call_id`s are session-unique** (§7.4): every one the session hands out
//! or accepts is remembered for its whole life — a deleted call's id
//! included — because `@openai/agents` refuses an id reused across
//! invocations (§2.3).
//!
//! Every refusal is one `error` and changes nothing; the caller echoes the
//! client's `event_id` on it.

use std::collections::{HashMap, HashSet};

use super::heard::HeardTable;
use super::ids::Ids;
use super::protocol::{ContentPart, ErrorObject, Item, ItemStatus, Role, ServerEvent, ITEM_OBJECT};

mod audio;
mod mcp;

pub(in crate::realtime) use mcp::refusal_of_frame as mcp_item_refusal;
use mcp::McpCallRecord;

/// `previous_item_id: "root"` inserts at the start (OpenAI's own spelling).
const ROOT: &str = "root";

/// The session's items, in conversation order.
#[derive(Debug, Default)]
pub(crate) struct Conversation {
    items: Vec<Item>,
    /// Every `call_id` this session has issued or accepted — never shrinks.
    calls_seen: HashSet<String>,
    /// What the listener heard of each assistant audio item (§7.3, `audio`).
    heard: HashMap<String, HeardTable>,
    /// What the session keeps beside each `mcp_call` item, by item id
    /// (`mcp`).
    mcp_calls: HashMap<String, McpCallRecord>,
}

impl Conversation {
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    fn position(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|i| i.id() == Some(id))
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Item> {
        let at = self.position(id)?;
        self.items.get_mut(at)
    }

    pub fn get(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.id() == Some(id))
    }

    /// The id of the item before `id`, `None` at the start — the
    /// `previous_item_id` of `conversation.item.added` / `.done`.
    pub fn previous_of(&self, id: &str) -> Option<String> {
        let at = self.position(id)?;
        at.checked_sub(1)
            .and_then(|p| self.items[p].id())
            .map(str::to_string)
    }

    /// Append an item the server produced; returns the id it follows.
    pub fn append(&mut self, item: Item) -> Option<String> {
        let previous = self.items.last().and_then(Item::id).map(str::to_string);
        self.items.push(item);
        previous
    }

    /// A freshly minted item id no item of this conversation has. Minted ids
    /// are session-unique by construction (`ids`), but an id a client chose
    /// for its own item is not minted — and the session tag is visible in
    /// every id, so a client can claim one the session would mint later.
    pub fn fresh_item_id(&self, ids: &Ids) -> String {
        loop {
            let id = ids.item();
            if self.position(&id).is_none() {
                return id;
            }
        }
    }

    /// [`Self::fresh_item_id`] for `call_id`s, against every one the session
    /// has seen.
    pub fn fresh_call_id(&self, ids: &Ids) -> String {
        loop {
            let id = ids.call();
            if !self.call_id_seen(&id) {
                return id;
            }
        }
    }

    /// Whether `call_id` has been issued or accepted in this session.
    pub fn call_id_seen(&self, call_id: &str) -> bool {
        self.calls_seen.contains(call_id)
    }

    /// Remember `call_id` as used, for the rest of the session.
    pub fn note_call_id(&mut self, call_id: &str) {
        self.calls_seen.insert(call_id.to_string());
    }

    /// `conversation.item.create` (§7.1): check the item, give it an id when
    /// it has none, insert it after `previous_item_id` (absent = append,
    /// `"root"` = at the start), and answer `conversation.item.added` and
    /// `.done` — a client item is complete the moment it exists.
    pub fn create(
        &mut self,
        mut item: Item,
        previous_item_id: Option<&str>,
        ids: &Ids,
    ) -> Result<[ServerEvent; 2], ErrorObject> {
        check_item(&item)?;
        if let Some(id) = item.id() {
            if self.position(id).is_some() {
                return Err(ErrorObject::invalid(
                    "invalid_value",
                    format!("an item with id '{id}' already exists in this conversation"),
                )
                .with_param("item.id"));
            }
        }
        if let Item::FunctionCall(c) = &item {
            // `check_item` made sure there is one.
            let call_id = c.call_id.as_deref().unwrap_or_default();
            if self.call_id_seen(call_id) {
                return Err(ErrorObject::invalid(
                    "invalid_value",
                    format!(
                        "call_id '{call_id}' is already used in this session; call ids are \
                         session-unique"
                    ),
                )
                .with_param("item.call_id"));
            }
        }
        let at = match previous_item_id {
            None => self.items.len(),
            Some(ROOT) => 0,
            Some(prev) => match self.position(prev) {
                Some(p) => p + 1,
                None => {
                    return Err(ErrorObject::invalid(
                        "item_not_found",
                        format!("previous_item_id '{prev}' is not an item of this conversation"),
                    )
                    .with_param("previous_item_id"))
                }
            },
        };

        item.set_id_if_missing(|| self.fresh_item_id(ids));
        mark_complete(&mut item);
        if let Item::FunctionCall(c) = &item {
            self.note_call_id(c.call_id.as_deref().unwrap_or_default());
        }
        self.note_replayed(&item, ids);
        let previous = at
            .checked_sub(1)
            .and_then(|p| self.items[p].id())
            .map(str::to_string);
        self.items.insert(at, item.clone());
        Ok([
            ServerEvent::ItemAdded {
                previous_item_id: previous.clone(),
                item: item.clone(),
            },
            ServerEvent::ItemDone {
                previous_item_id: previous,
                item,
            },
        ])
    }

    /// `conversation.item.retrieve`.
    pub fn retrieve(&self, item_id: &str) -> Result<ServerEvent, ErrorObject> {
        let item = self.get(item_id).ok_or_else(|| not_found(item_id))?;
        Ok(ServerEvent::ItemRetrieved { item: item.clone() })
    }

    /// `conversation.item.delete`. An item the active response is still
    /// producing is refused: its remaining events would name an item the
    /// client was told is gone — cancel the response first.
    pub fn delete(&mut self, item_id: &str) -> Result<ServerEvent, ErrorObject> {
        let at = self.position(item_id).ok_or_else(|| not_found(item_id))?;
        if status_of(&self.items[at]) == Some(ItemStatus::InProgress) {
            return Err(ErrorObject::invalid(
                "invalid_value",
                format!(
                    "item '{item_id}' is still being produced by the active response; send \
                     response.cancel first"
                ),
            )
            .with_param("item_id"));
        }
        // An `mcp_call` has no status to say so (realtime-server-tools §2.5).
        if let Some(e) = self.mcp_delete_refusal(item_id) {
            return Err(e);
        }
        self.items.remove(at);
        self.heard.remove(item_id);
        self.mcp_calls.remove(item_id);
        Ok(ServerEvent::ItemDeleted {
            item_id: item_id.to_string(),
        })
    }
}

pub(super) fn not_found(item_id: &str) -> ErrorObject {
    ErrorObject::invalid(
        "item_not_found",
        format!("no item '{item_id}' in this conversation"),
    )
    .with_param("item_id")
}

fn status_of(item: &Item) -> Option<ItemStatus> {
    match item {
        Item::Message(m) => m.status,
        Item::FunctionCall(c) => c.status,
        Item::FunctionCallOutput(o) => o.status,
        Item::McpCall(_) | Item::McpListTools(_) => None,
    }
}

/// A client item is complete as it arrives, and is echoed as a server item.
/// An `mcp_call` has neither field (realtime-server-tools §2.2).
fn mark_complete(item: &mut Item) {
    let (object, status) = match item {
        Item::Message(m) => (&mut m.object, &mut m.status),
        Item::FunctionCall(c) => (&mut c.object, &mut c.status),
        Item::FunctionCallOutput(o) => (&mut o.object, &mut o.status),
        Item::McpCall(_) | Item::McpListTools(_) => return,
    };
    *object = Some(ITEM_OBJECT.into());
    *status = Some(ItemStatus::Completed);
}

/// What a client may create (§7.1): each role with the content types it
/// speaks, a function call that names its `call_id`, and an `mcp_call` as
/// history (`mcp`).
fn check_item(item: &Item) -> Result<(), ErrorObject> {
    match item {
        Item::Message(m) => {
            for (n, part) in m.content.iter().enumerate() {
                let param = format!("item.content[{n}]");
                let fits = match (m.role, part) {
                    (Role::User, ContentPart::InputText { .. }) => true,
                    (Role::User, ContentPart::InputAudio { audio, transcript }) => {
                        // The cascade answers from text, so audio a client
                        // sends as an item would need an ASR call of its own;
                        // only committed turns are transcribed so far.
                        if audio.is_some() && transcript.is_none() {
                            return Err(ErrorObject::invalid(
                                "not_implemented_yet",
                                "an input_audio item without a transcript needs transcribing, \
                                 which is not implemented yet in this lmgw build (realtime work \
                                 in progress); send input_text, or input_audio with its \
                                 transcript",
                            )
                            .with_param(param));
                        }
                        true
                    }
                    (Role::Assistant, ContentPart::OutputText { .. })
                    | (Role::Assistant, ContentPart::OutputAudio { .. })
                    | (Role::System, ContentPart::InputText { .. }) => true,
                    _ => false,
                };
                if !fits {
                    return Err(ErrorObject::invalid(
                        "invalid_value",
                        format!(
                            "a {} message cannot carry {} content: user messages take \
                             input_text or input_audio, assistant messages output_text or \
                             output_audio, system messages input_text",
                            role_name(m.role),
                            part_name(part)
                        ),
                    )
                    .with_param(param));
                }
            }
            Ok(())
        }
        Item::FunctionCall(c) => match c.call_id.as_deref() {
            Some(id) if !id.trim().is_empty() => Ok(()),
            _ => Err(ErrorObject::invalid(
                "missing_required_parameter",
                "a function_call item needs a call_id — its function_call_output names it",
            )
            .with_param("item.call_id")),
        },
        Item::FunctionCallOutput(_) | Item::McpCall(_) => Ok(()),
        Item::McpListTools(_) => Err(mcp::list_tools_refusal()),
    }
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    }
}

fn part_name(p: &ContentPart) -> &'static str {
    match p {
        ContentPart::InputText { .. } => "input_text",
        ContentPart::InputAudio { .. } => "input_audio",
        ContentPart::OutputText { .. } => "output_text",
        ContentPart::OutputAudio { .. } => "output_audio",
    }
}

#[cfg(test)]
mod tests {
    use super::super::ids::Ids;
    use super::super::protocol::{ContentPart, Item, MessageItem, Role};
    use super::Conversation;

    fn user(id: &str) -> Item {
        Item::Message(MessageItem {
            id: Some(id.into()),
            object: None,
            status: None,
            role: Role::User,
            content: vec![ContentPart::InputText { text: "hi".into() }],
        })
    }

    #[test]
    fn a_minted_id_never_takes_one_a_client_claimed() {
        let ids = Ids::new();
        let mut conv = Conversation::default();
        // The session tag is in every id, so a client can name the one the
        // session would mint next.
        let seen = ids.item();
        let n = u64::from_str_radix(&seen[seen.len() - 1..], 16).unwrap();
        let next = format!("{}{:x}", &seen[..seen.len() - 1], n + 1);
        conv.create(user(&next), None, &ids).unwrap();
        let minted = conv.fresh_item_id(&ids);
        assert_ne!(minted, next);
        // And the client cannot claim it twice.
        let e = conv.create(user(&next), None, &ids).unwrap_err();
        assert_eq!(e.code.as_deref(), Some("invalid_value"));
    }
}
