//! Tool call ids minted for an answer whose calls carry none (Gemini's
//! never do; an OpenAI-shaped upstream may leave them out: chat-media-tools
//! design §14).

/// Tool call ids for one answer whose upstream gave none: a random tag plus a
/// counter, `call_<12 hex digits><n in hex>`, as the realtime session mints
/// its own (`realtime::ids`). Gemini's answers have no ids at all; an
/// OpenAI-shaped upstream (llama-server, a local template) may leave a call's
/// `id` out. Unique across answers too, not only within one: a tool loop's
/// steps each get a fresh minter, and a per-answer `call_0` would repeat in
/// the history the client sends back — duplicate `tool_use` ids on an
/// Anthropic fallback, a result paired with an older call's name
/// (`ChatRequest::tool_name_for_id`), clients that refuse a reused id
/// (`@openai/agents`), and two cards of one Chat reply under one id.
pub(crate) struct CallIds {
    tag: String,
    next: u64,
}

impl Default for CallIds {
    fn default() -> Self {
        // 48 random bits: plenty to keep one gateway's answers apart.
        let tag: u64 = rand::random::<u64>() & 0xffff_ffff_ffff;
        Self {
            tag: format!("{tag:012x}"),
            next: 0,
        }
    }
}

impl CallIds {
    pub(crate) fn mint(&mut self) -> String {
        let n = self.next;
        self.next += 1;
        format!("call_{}{n:x}", self.tag)
    }
}
