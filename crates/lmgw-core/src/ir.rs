//! Canonical intermediate representation (§5).
//!
//! Provider-neutral request/response types rich enough to round-trip the
//! OpenAI and Anthropic ingress shapes and all three egress shapes.

use serde::{Deserialize, Serialize};

mod anthropic_block;
mod call_id;
pub mod compaction;
mod items;
mod seal;
pub(crate) mod signer;
pub mod structured;
pub(crate) use anthropic_block::{has_unread, text_source, url_source};
pub use anthropic_block::{
    ignored_in_messages, ignored_names, left_out_for, AnthropicBlock, AnthropicSent, BlockEvent,
    ToolText,
};
pub use call_id::{
    call_id_with_signature, split_call_id, wire_call_id, wire_call_ids_in_messages_body,
    wire_call_ids_in_responses_body, THOUGHT_SIGNATURE_MARKER, THOUGHT_SIGNATURE_TEXT_MARKER,
};
pub use compaction::{Compaction, CompactionEvent, UsageIterations};
pub use items::{encrypted_sha256, UpstreamItem, UpstreamItems};
pub use seal::{
    config_fingerprint, install_seal_key, strip_seals, unsealed_record, Seal, SealBy, SealScope,
    OPENAI_CARRIER_PREFIX, SEAL_KEY_LEN,
};
pub use signer::Signer;
pub use structured::{FormatAsk, FormatKind, JsonSchemaFormat, ResponseFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Url { url: String },
    Base64 { data: String },
}

/// The id prefix of a tool call lmgw writes into a request itself, which
/// no model made: a late MCP task result's `lmgw__job_result` call (MCP
/// Tasks design §3.2, `lmgw_task_<row id>`). Such a call carries no
/// model's signature, so where Gemini 3 wants a `thoughtSignature` the
/// Gemini egress gives it the documented skip value, as it gives every
/// step's first call without one (gateway design §7.1).
pub const SYNTHETIC_CALL_ID_PREFIX: &str = "lmgw_task_";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Answer or prompt text. `citations` are an Anthropic text block's
    /// own (gateway design §6.1, "Citations", 2026-11-06): the sources the
    /// answer cites for this text, verbatim, as Anthropic answered them
    /// (`char_location`, `page_location`, `web_search_result_location`, …).
    /// Only the Messages wire writes them and only an Anthropic-protocol
    /// egress sends them back; every other wire leaves them out and names
    /// them (`answer.citations`, `content.citations`, [`ignored_names`]).
    ///
    /// `citations_scope` is the provider whose answer lmgw read them from
    /// ([`Signer`], changed 2026-11-06), as a signature's: a web search's
    /// `encrypted_index` is that provider's to read. lmgw's own records send
    /// the citations to an Anthropic-protocol row at that scope only; a row
    /// elsewhere gets the text without them, named `content.citations`
    /// ([`AnthropicSent`]). Citations a Messages client sent have no scope:
    /// they are the client's own, sent as the client wrote them.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        citations: Vec<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        citations_scope: Option<Signer>,
    },
    Image {
        mime: String,
        source: ImageSource,
    },
    /// Audio input. OpenAI chat's `input_audio` is the only client shape that
    /// carries this — Anthropic has no audio block, so this part never comes
    /// from (or goes to) an Anthropic ingress/egress; it is input-only, never
    /// produced by a model completion.
    Audio {
        mime: String,
        /// Base64 payload, without a `data:` prefix.
        data: String,
    },
    /// Assistant-emitted tool invocation. One lmgw wrote itself, which no
    /// model made, has an id starting with [`SYNTHETIC_CALL_ID_PREFIX`]; one
    /// a Gemini model made may carry its `thoughtSignature` inside the id
    /// ([`split_call_id`]), which only the Gemini egress sends on.
    ToolUse {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    /// Tool output fed back into the conversation.
    ToolResult {
        id: String,
        /// Name of the tool that produced this result (needed by Gemini, which
        /// correlates by name; resolved from the matching ToolUse if absent).
        name: Option<String>,
        content: Vec<ToolResultBlock>,
        is_error: bool,
    },
    /// The reasoning / "thinking" trace of an assistant turn, replayed so a
    /// reasoning model sees what it was thinking earlier in the conversation
    /// (OpenAI/DeepSeek `reasoning_content`, Anthropic `thinking` blocks,
    /// Responses `reasoning` items). llama-server's `--reasoning-preserve`
    /// only does anything when this actually reaches it: before the IR could
    /// carry it, every ingress dropped the trace and that flag was a no-op
    /// for any client behind the gateway.
    ///
    /// `signature` is Anthropic's continuity token — present only when the
    /// block arrived signed from Anthropic itself, and required before one
    /// can be sent back there. A trace from llama.cpp, Gemini or a client's
    /// own hand has none. `signature_scope` is the provider whose answer lmgw
    /// read the signature from ([`Signer`], the row's scope): lmgw's own
    /// records send the block to an Anthropic-protocol row at that scope,
    /// whatever its key or model (Anthropic drops a block the model asked
    /// cannot read), and to a row elsewhere leave it out whole, as that
    /// server cannot read it. A signature a Messages client sent has no
    /// signer: it is the client's own, sent as the client wrote it.
    ///
    /// `sealed` is an OpenAI reasoning item's seal (openai egress design
    /// §7.3, [`Seal`]): its `encrypted_content`, which only the upstream row
    /// it came from is sent. One part is one reasoning item, its summary as
    /// `text` (empty when the item came without one).
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sealed: Option<Seal>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature_scope: Option<Signer>,
    },
    /// The model's refusal, in its own words (openai egress design §7.2.3):
    /// OpenAI's Chat Completions `message.refusal` and the Responses wire's
    /// `refusal` content part, which a structured-output client reads apart
    /// from the answer (its schema's JSON is not there). A wire with a
    /// refusal of its own writes it there (the OpenAI chat ingress's
    /// `refusal`, the Responses ingress's `refusal` part); every other wire,
    /// and every replay of the turn to an upstream, carries it as text.
    Refusal {
        text: String,
    },
    /// Anthropic's `compaction` block ([`compaction`]): its summary and
    /// every other field verbatim, so it goes back to Anthropic as it came.
    /// Every other egress is sent the conversation as Anthropic makes of it
    /// ([`compaction::folded`]): the history before the block dropped, the
    /// summary as a user's text opening what is left. A client wire without the block writes
    /// the summary as text.
    Compaction(Compaction),
    /// An Anthropic content block the IR does not model, verbatim
    /// ([`AnthropicBlock`]): `redacted_thinking`, the provider's own tool
    /// blocks, a type Anthropic adds next. Only an Anthropic-protocol
    /// egress sends it, and only the Messages wire writes it.
    AnthropicBlock(AnthropicBlock),
}

/// A tool call's argument text as the IR holds it. Empty or whitespace-only
/// text is a call with no arguments, `{}` (Anthropic streams no
/// `input_json_delta` for one, and OpenAI-compatible upstreams may send
/// `"arguments": ""`). Unparseable text stays a JSON string: the model, not
/// the gateway, produced it, and the tool's own validation reports it better
/// than an error here would.
pub fn parse_call_args(text: &str) -> serde_json::Value {
    if text.trim().is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_string()))
}

/// A stored call's arguments as a wire that requires an object (Anthropic
/// `input`, Gemini `functionCall.args`) takes them. `Null` and blank text are
/// `{}`, text holding a JSON object is that object; any other value is sent
/// as it is (nothing is invented). Threads stored before [`parse_call_args`]
/// existed hold `""` for a call with no arguments.
pub fn object_call_args(args: &serde_json::Value) -> serde_json::Value {
    match args {
        serde_json::Value::Null => serde_json::json!({}),
        serde_json::Value::String(s) if s.trim().is_empty() => serde_json::json!({}),
        serde_json::Value::String(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(v @ serde_json::Value::Object(_)) => v,
            _ => args.clone(),
        },
        other => other.clone(),
    }
}

impl ContentPart {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text {
            text: s.into(),
            citations: Vec::new(),
            citations_scope: None,
        }
    }

    /// A text with its Anthropic citations, read from an answer at `scope`
    /// (`None` for a client's): the scope is kept only where there are
    /// citations for it to bind.
    pub fn cited(
        text: impl Into<String>,
        citations: Vec<serde_json::Value>,
        scope: Option<Signer>,
    ) -> Self {
        let citations_scope = scope.filter(|_| !citations.is_empty());
        Self::Text {
            text: text.into(),
            citations,
            citations_scope,
        }
    }

    /// The answer text this part is, for a reader without a refusal of its
    /// own: a text's and a refusal's alike.
    pub fn answer_text(&self) -> Option<&str> {
        match self {
            Self::Text { text, .. } | Self::Refusal { text } => Some(text),
            _ => None,
        }
    }

    /// An unsigned reasoning trace.
    pub fn reasoning(s: impl Into<String>) -> Self {
        Self::Reasoning {
            text: s.into(),
            signature: None,
            sealed: None,
            signature_scope: None,
        }
    }

    /// One sealed reasoning item: its summary text and its seal.
    pub fn sealed_reasoning(text: impl Into<String>, seal: Seal) -> Self {
        Self::Reasoning {
            text: text.into(),
            signature: None,
            sealed: Some(seal),
            signature_scope: None,
        }
    }

    /// One Anthropic thinking block: its text and its signature, with the
    /// signer of the answer that carried it (`None` for a client's).
    pub fn signed_reasoning(
        text: impl Into<String>,
        signature: impl Into<String>,
        scope: Option<Signer>,
    ) -> Self {
        Self::Reasoning {
            text: text.into(),
            signature: Some(signature.into()),
            sealed: None,
            signature_scope: scope,
        }
    }

    /// The seal of a sealed reasoning part.
    pub fn seal(&self) -> Option<&Seal> {
        match self {
            Self::Reasoning { sealed, .. } => sealed.as_ref(),
            _ => None,
        }
    }
}

/// One block of a tool's output, mirroring MCP's own `CallToolResult.content`
/// shape (§7) so a tool result survives the trip to the model without being
/// stringified first.
///
/// **Why this isn't just `String`.** The three egress protocols accept
/// genuinely different things in their tool-result slot — Anthropic takes a
/// block array (text *and* images), Gemini takes an arbitrary JSON object, and
/// OpenAI chat-completions takes text only. Flattening at the IR would impose
/// the *intersection* on all three; carrying blocks lets each adapter emit what
/// it natively supports and pushes the lossy step to the one protocol that
/// actually requires it, where [`flatten_tool_result`] reports what it dropped.
///
/// A lone [`Text`](Self::Text) block — the overwhelmingly common case, and the
/// only thing the previous `String` could represent — renders byte-identically
/// on every adapter, so text in still means text out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultBlock {
    Text {
        text: String,
    },
    /// Structured output: MCP `structuredContent`, or a tool whose result is
    /// JSON rather than prose. Gemini takes this verbatim.
    Json {
        value: serde_json::Value,
    },
    /// Base64 image returned by a tool (screenshotters, chart renderers, …).
    Image {
        mime: String,
        data: String,
    },
    Audio {
        mime: String,
        data: String,
    },
    /// MCP embedded resource. `text` is the inline content when the resource is
    /// textual; binary resources carry the `uri` alone.
    Resource {
        uri: String,
        mime: Option<String>,
        text: Option<String>,
    },
    /// An Anthropic block a Messages client's `tool_result` carried that
    /// the IR does not read, as the client wrote it (gateway design §6.1,
    /// changed 2026-11-06): a `document`, a `search_result`, an image by
    /// Files API reference, which only Anthropic can fetch, and every other
    /// block type but `text`. An Anthropic-protocol row is sent it as
    /// written ([`AnthropicBlock::image_sent_to`] for an image); every
    /// other egress is sent what of it reads as text and none of the rest
    /// ([`AnthropicBlock::tool_text`]: a plain-text document's or a search
    /// result's text, never a URL or file id), and the request names what
    /// is left out `content.<type>` ([`ignored_names`]).
    AnthropicBlock(AnthropicBlock),
}

impl ToolResultBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }

    /// A single text block — the shape every caller that has only a string
    /// wants, kept as one call so the common case stays a one-liner.
    pub fn one(s: impl Into<String>) -> Vec<Self> {
        vec![Self::text(s)]
    }
}

/// Why [`flatten_tool_result`] leaves a binary block out: the slot it fills
/// takes text only.
pub const TEXT_ONLY_SLOT: &str = "this upstream's tool-result slot is text-only";

/// What a tool-result image is, for a placeholder or a WARN: its mime and the
/// length of its base64.
pub fn tool_image_note(mime: &str, data: &str) -> String {
    format!("{mime} image, {} base64 bytes", data.len())
}

/// The text a tool-result image that is not sent becomes: what it was, and
/// `why` it is not there. [`flatten_tool_result`]'s `why` is
/// [`TEXT_ONLY_SLOT`]; the llama.cpp egress names its own reasons (llama
/// egress design §8.2).
pub fn tool_image_placeholder(mime: &str, data: &str, why: &str) -> String {
    format!("[{} — omitted: {why}]", tool_image_note(mime, data))
}

/// [`tool_image_note`] for any image a request carries, a user message's
/// too: an inline one is its mime and base64 length, one by URL says so.
pub fn image_note(mime: &str, source: &ImageSource) -> String {
    match source {
        ImageSource::Base64 { data } => tool_image_note(mime, data),
        ImageSource::Url { .. } => format!("{mime} image by URL"),
    }
}

/// [`tool_image_placeholder`] for any image a request carries: the same
/// text, so a placeholder reads alike wherever an image was left out
/// (`gate::fallback_images`: a fallback that cannot see).
pub fn image_placeholder(mime: &str, source: &ImageSource, why: &str) -> String {
    format!("[{} — omitted: {why}]", image_note(mime, source))
}

/// Render tool-result blocks down to a single string, for protocols whose
/// tool-result slot is text-only (OpenAI chat-completions `role: "tool"`).
///
/// Returns the text plus a note for every block that could not be represented,
/// so the caller can log the loss rather than hide it (§14). A verbatim
/// Anthropic block ([`ToolResultBlock::AnthropicBlock`]) adds its text form
/// ([`AnthropicBlock::tool_text`]) and no note: the request's
/// `x-lmgw-params-ignored` names what it leaves out. Binary blocks are
/// deliberately **not** inlined: base64ing a 1 MB image into the prompt
/// costs ~350k tokens, which is a far worse failure than a named
/// placeholder.
pub fn flatten_tool_result(blocks: &[ToolResultBlock]) -> (String, Vec<String>) {
    let mut parts: Vec<String> = Vec::with_capacity(blocks.len());
    let mut verbatim: Vec<bool> = Vec::with_capacity(blocks.len());
    let mut notes: Vec<String> = Vec::new();
    for b in blocks {
        match b {
            ToolResultBlock::Text { text } => parts.push(text.clone()),
            ToolResultBlock::Json { value } => {
                parts.push(serde_json::to_string(value).unwrap_or_else(|_| "null".into()))
            }
            ToolResultBlock::Image { mime, data } => {
                parts.push(tool_image_placeholder(mime, data, TEXT_ONLY_SLOT));
                notes.push(tool_image_note(mime, data));
            }
            ToolResultBlock::Audio { mime, data } => {
                let note = format!("{mime} audio, {} base64 bytes", data.len());
                parts.push(format!("[{note} — omitted: {TEXT_ONLY_SLOT}]"));
                notes.push(note);
            }
            ToolResultBlock::Resource { uri, mime, text } => match text {
                Some(t) => parts.push(t.clone()),
                None => {
                    let kind = mime.as_deref().unwrap_or("binary");
                    parts.push(format!("[{kind} resource: {uri}]"));
                    notes.push(format!("{kind} resource {uri}"));
                }
            },
            // Off an Anthropic route: what of it reads as text, and the
            // request names the rest (`content.<type>`, [`ignored_names`]).
            ToolResultBlock::AnthropicBlock(b) => parts.extend(b.tool_text().text),
        }
        verbatim.resize(parts.len(), matches!(b, ToolResultBlock::AnthropicBlock(_)));
    }
    (join_tool_texts(parts.into_iter().zip(verbatim)), notes)
}

/// The texts of one tool result's blocks as one string (`.1` is whether the
/// text is a verbatim Anthropic block's, [`AnthropicBlock::tool_text`]): a
/// line break between texts, a blank line between two verbatim blocks', so
/// one document's end and the next title tell apart (changed 2026-11-06).
pub fn join_tool_texts(parts: impl IntoIterator<Item = (String, bool)>) -> String {
    let mut out = String::new();
    let mut last: Option<bool> = None;
    for (text, verbatim) in parts {
        if let Some(prev) = last {
            out.push_str(if prev && verbatim { "\n\n" } else { "\n" });
        }
        out.push_str(&text);
        last = Some(verbatim);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
}

impl Message {
    pub fn text(role: Role, s: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentPart::text(s)],
        }
    }

    /// Concatenated text of all `Text` parts.
    pub fn joined_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                // A refusal replays as the text it is (`ContentPart::Refusal`).
                ContentPart::Text { text, .. } | ContentPart::Refusal { text } => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Concatenated text of all `Reasoning` parts — the trace as protocols
    /// that carry it in one field (`reasoning_content`) want it. Empty when
    /// the turn has none. A part without text (a sealed item replayed after
    /// the one that carries a plain reply's whole trace, an item that came
    /// with no summary) adds nothing, not a stray line break.
    pub fn reasoning_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Reasoning { text, .. } if !text.is_empty() => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON schema of the tool input.
    pub parameters: serde_json::Value,
    /// The client's `strict` (OpenAI's function tools, chat and Responses;
    /// Anthropic's tools): whether the model's arguments must match
    /// `parameters` exactly. Each ingress fills its own dialect's default
    /// where the client said nothing (openai egress design §7.2.2): a Chat
    /// Completions or Anthropic tool is not strict, so those ingresses write
    /// `Some(false)`; a Responses tool without `strict` is strict when its
    /// schema allows it, which only `None` (no key on the Responses wire)
    /// says. `None` is also an in-process caller's (lmgw's own MCP tools),
    /// which takes each upstream's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Tool { name: String },
}

/// Per-request reasoning control (model-capabilities design §5.1): the three
/// knobs every dialect spells differently, held in one provider-neutral shape
/// so a control given in any of them reaches whichever egress answers.
///
/// Every field is optional and `None` means "say nothing" — the route's own
/// configuration (a local row's `--reasoning` flags, a provider's default)
/// applies untouched. That is deliberately different from `Some(false)` /
/// `Some("none")`, which are an explicit "turn it off" that the egress *does*
/// send upstream.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReasoningControl {
    /// Reason at all. `None` leaves the route's default alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Effort level, verbatim — lmgw checks no vocabulary (§5.3): a level the
    /// template or the provider does not know is their 400, with their message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Thinking-token budget (llama-server `reasoning_budget_tokens` and
    /// `thinking_budget_tokens`, Anthropic `thinking.budget_tokens`, Gemini
    /// `thinkingBudget`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<i64>,
    /// The off spelled as Anthropic's `thinking: {type: "between_tools"}`:
    /// no up-front thinking, only the short progress notes some models write
    /// between tool calls — the lowest thinking setting of a model that
    /// takes no `disabled` (Claude Sonnet 5.5). Always an off
    /// ([`Self::normalised`] makes `enabled` `Some(false)`). Only the
    /// Anthropic egress has the spelling; every other egress sends its own
    /// off, and the handler names `between_tools` in
    /// `x-lmgw-reasoning-ignored` there (model-capabilities design §5.3).
    /// Set by a Messages client's own `thinking` type, or by the reasoning
    /// fit for a model whose catalog says this is its lowest setting (§5.6).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub between_tools: bool,
}

impl ReasoningControl {
    /// Whether the Anthropic `thinking` type this (normalised) control
    /// renders as takes a `display`: `adaptive` and `enabled` do, `disabled`
    /// and `between_tools` take no other field, and no control sends no
    /// `thinking` to put it in.
    pub fn takes_display(&self) -> bool {
        self.enabled != Some(false)
            && (self.enabled == Some(true) || self.effort.is_some() || self.budget_tokens.is_some())
    }

    /// Nothing set — the egress emits no reasoning keys at all.
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.effort.is_none()
            && self.budget_tokens.is_none()
            && !self.between_tools
    }

    /// Field-wise merge with `self` as the higher tier (§5.1/§5.2): a client
    /// that sets only `enabled` keeps the alias default's `effort`, and a
    /// header that sets only `effort` keeps the body's `budget_tokens`.
    ///
    /// `between_tools` is how an off is spelled, so it goes with `enabled`:
    /// a lower tier's spelling survives only while the merged control is
    /// still an off that the higher tier did not turn on.
    pub fn merge_over(mut self, lower: Option<Self>) -> Self {
        if let Some(l) = lower {
            if self.enabled != Some(true) {
                self.between_tools |= l.between_tools;
            }
            self.enabled = self.enabled.or(l.enabled);
            self.effort = self.effort.or(l.effort);
            self.budget_tokens = self.budget_tokens.or(l.budget_tokens);
        }
        self
    }

    /// Collapse one tier's triple to a form no egress cell can contradict
    /// (§5.1):
    ///
    /// - `effort == "none"` ⇒ `enabled = Some(false)`, no effort;
    /// - `budget_tokens == Some(0)` ⇒ `enabled = Some(false)`, no budget;
    /// - `between_tools` ⇒ `enabled = Some(false)`: it is a spelling of off;
    /// - `enabled == Some(false)` ⇒ no effort, no budget;
    /// - a real level or a positive budget ⇒ `enabled = Some(true)`;
    /// - anything but an off ⇒ no `between_tools`.
    ///
    /// The level rule is what makes precedence work **per tier** rather than
    /// per field. Asking for `high` is asking for thinking to be on, so the
    /// tier that says `high` carries its own `enabled`; without it, an alias
    /// default of `{enabled: false}` would survive the merge and then erase the
    /// level the header just asked for — the higher tier losing to the lower
    /// one it was meant to override.
    ///
    /// So this runs on **every tier before merging**, and once more on the
    /// merged result. It is idempotent, so an egress may re-apply it to a
    /// control that reached it through a path that had not.
    pub fn normalised(mut self) -> Self {
        if self.between_tools {
            self.enabled = Some(false);
        }
        if self
            .effort
            .as_deref()
            .is_some_and(|e| e.eq_ignore_ascii_case("none"))
        {
            self.enabled = Some(false);
            self.effort = None;
        }
        if self.budget_tokens == Some(0) {
            self.enabled = Some(false);
            self.budget_tokens = None;
        }
        if self.enabled == Some(false) {
            self.effort = None;
            self.budget_tokens = None;
        }
        if self.effort.is_some() || self.budget_tokens.is_some_and(|b| b > 0) {
            self.enabled = Some(true);
        }
        if self.enabled != Some(false) {
            self.between_tools = false;
        }
        self
    }
}

/// Sampling / generation parameters. All optional; unknown or unsupported
/// params are dropped per-egress with a logged note (§5).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Params {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// llama.cpp `min_p` (an OpenAI-compatible extension, modelled since the
    /// Chat's sampling settings need it); only the OpenAI egress emits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_p: Option<f64>,
    /// llama.cpp `repeat_penalty`; only the OpenAI egress emits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_penalty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// Reasoning control (§5.1). Merged field-wise, not whole-value, by
    /// [`Params::with_defaults`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningControl>,
    /// The provider refused a reasoning summary on this send's route, so
    /// none is asked for (openai egress design §7.2.2): the OpenAI
    /// platform's Responses wire asks for one on every send, and
    /// `proxy::reasoning_fit::send_chat` sets this from what a refusal
    /// taught it. No client's dialect: never read or written by serde.
    #[serde(skip)]
    pub reasoning_summary_refused: bool,
    /// What the client asked of the reasoning summary: a Responses
    /// client's `reasoning.summary` (openai egress design §7.2.2). `None`
    /// when it said nothing, which the platform's Responses wire fills with
    /// `auto` (and the summary lesson may take back); `Some(None)` its
    /// `null`, no summary; `Some(Some(kind))` that kind, verbatim, whose
    /// refusal reaches the client. Never an alias default: serde skips it.
    #[serde(skip)]
    pub reasoning_summary: Option<Option<String>>,
}

impl Params {
    /// Fill unset fields from `defaults` (alias `param_overrides` act as
    /// defaults for the upstream call; explicit client values win).
    pub fn with_defaults(mut self, defaults: &Params) -> Params {
        self.temperature = self.temperature.or(defaults.temperature);
        self.top_p = self.top_p.or(defaults.top_p);
        self.top_k = self.top_k.or(defaults.top_k);
        self.min_p = self.min_p.or(defaults.min_p);
        self.repeat_penalty = self.repeat_penalty.or(defaults.repeat_penalty);
        self.max_tokens = self.max_tokens.or(defaults.max_tokens);
        self.presence_penalty = self.presence_penalty.or(defaults.presence_penalty);
        self.frequency_penalty = self.frequency_penalty.or(defaults.frequency_penalty);
        self.seed = self.seed.or(defaults.seed);
        if self.stop.is_empty() {
            self.stop = defaults.stop.clone();
        }
        // Field-wise, not whole-value (§5.1): a client that sends only
        // `enabled` must keep the alias's configured effort level, and
        // `Option::or` on the whole struct would drop it. Each tier is
        // normalised *before* the merge, so an alias `{enabled: false}` cannot
        // outlive the client's explicit level — see
        // [`ReasoningControl::normalised`].
        let lower = defaults.reasoning.clone().map(ReasoningControl::normalised);
        self.reasoning = match self.reasoning.take().map(ReasoningControl::normalised) {
            Some(c) => Some(c.merge_over(lower)),
            None => lower,
        };
        self
    }

    /// The reasoning control as a total triple, normalised (§5.1). Empty when
    /// no tier set anything — which is what every egress reads to decide it
    /// should emit no reasoning keys at all.
    pub fn reasoning_control(&self) -> ReasoningControl {
        self.reasoning.clone().unwrap_or_default().normalised()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// The alias the client asked for (router input).
    pub model_alias: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub params: Params,
    #[serde(default)]
    pub tools: Vec<ToolDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// The structured output the client asked for (structured-output design
    /// §4): every ingress fills it, every egress writes it in its own dialect
    /// or refuses it (§6). `None` when nothing was asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(default)]
    pub stream: bool,
    /// Top-level request fields the IR does not otherwise model (e.g.
    /// `grammar`, extra llama.cpp sampler params). Captured verbatim at OpenAI
    /// ingress and re-emitted verbatim on OpenAI egress so OpenAI-compatible
    /// upstreams (llama-server) keep receiving them — without this the
    /// OpenAI→OpenAI route, documented as "near pass-through", would silently
    /// strip anything not explicitly modeled above. Empty for cross-protocol
    /// routes, where these OpenAI-shaped fields have no equivalent and are
    /// dropped per-egress. Structured output is modelled
    /// ([`Self::response_format`]), not carried here.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub passthrough: serde_json::Map<String, serde_json::Value>,
    /// `chat_template_kwargs.enable_thinking` as the client sent it — a
    /// **llama-server-only** control, kept out of [`Params::reasoning`] on
    /// purpose (§5.2).
    ///
    /// It names a variable of a Jinja chat template, which only a route that
    /// renders one has. Folding it into the protocol-neutral control would let
    /// `{"enable_thinking": false}` — a key a cloud provider ignores outright —
    /// turn into an Anthropic `thinking: {type: "disabled"}` or an OpenAI
    /// `reasoning_effort: "none"` the client never asked for. So it is merged
    /// into the control only when the resolved route is
    /// [`UpstreamKind::LlamaServer`](crate::config::UpstreamKind::LlamaServer);
    /// everywhere else the key simply rides along in `passthrough`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llama_kwargs_enabled: Option<bool>,
    /// The client's `anthropic-beta` flags, one per entry — an
    /// **Anthropic-only** control, as `llama_kwargs_enabled` above is
    /// llama-server-only. Only the Anthropic egress sends them, folded into
    /// one header with any flags the upstream row's extra headers carry;
    /// every other protocol has no equivalent and ignores them. Filled from
    /// the request header by the chat-shaped handlers, whichever dialect the
    /// body speaks: a client that names an Anthropic beta means it for an
    /// Anthropic upstream.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anthropic_beta: Vec<String>,
    /// A Messages request's `context_management`, verbatim — an
    /// **Anthropic-only** control, as `anthropic_beta` above is. Only the
    /// Anthropic egress sends it, as the client wrote it (Anthropic answers
    /// for it, and its beta flag rides in `anthropic_beta`); every other
    /// protocol has no equivalent, and the handler names it in
    /// `x-lmgw-params-ignored` ([`Self::fields_ignored`]). Only the Messages
    /// ingress fills it yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_management: Option<serde_json::Value>,
    /// A Messages request's top-level `compaction` (on-demand compaction,
    /// beta `compact-2026-09-04`), verbatim — Anthropic-only like
    /// `context_management`. Only the Anthropic egress sends it, as the
    /// client wrote it; Anthropic answers it with a `compaction` block
    /// ([`ContentPart::Compaction`]). Every other route names it in
    /// `x-lmgw-params-ignored` ([`Self::fields_ignored`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<serde_json::Value>,
    /// A Messages request's `thinking.display` (`summarized`, `omitted`, or a
    /// newer value), verbatim: how Anthropic returns the thinking text. Only
    /// the Anthropic egress sends it, inside the `thinking` object of the
    /// types that take it (`adaptive`, `enabled`); everywhere else, and
    /// beside an off there, the handler names `thinking.display` in
    /// `x-lmgw-params-ignored` ([`Self::fields_ignored`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_display: Option<serde_json::Value>,
}

impl ChatRequest {
    /// Whether the request asked for JSON — `json_schema` or `json_object`:
    /// what the candidate-alias facet `structured_output` counts.
    pub fn asks_json(&self) -> bool {
        self.response_format
            .as_ref()
            .is_some_and(|f| f.kind().is_some())
    }

    /// The request's own fields a route of `protocol` does not send, for
    /// `x-lmgw-params-ignored` beside the samplers
    /// ([`crate::proxy::params_ignored`]): `context_management` and
    /// `compaction` on every route but an Anthropic one (which sends both),
    /// and `thinking.display` wherever it does
    /// not go out — off an Anthropic route, and on one beside an off, whose
    /// types take no `display`. `control` is the send's final reasoning
    /// control, after the fit (§5.6).
    pub fn fields_ignored(
        &self,
        protocol: crate::config::Protocol,
        control: &ReasoningControl,
    ) -> Vec<&'static str> {
        let anthropic = protocol == crate::config::Protocol::Anthropic;
        let mut out = Vec::new();
        if self.context_management.is_some() && !anthropic {
            out.push("context_management");
        }
        if self.compaction.is_some() && !anthropic {
            out.push("compaction");
        }
        if self.thinking_display.is_some() && !(anthropic && control.takes_display()) {
            out.push("thinking.display");
        }
        out
    }

    /// What of the request's Anthropic blocks the row `up` is not sent, as
    /// `x-lmgw-params-ignored` names it beside [`Self::fields_ignored`]: off
    /// an Anthropic route every verbatim block ([`AnthropicBlock`]) as
    /// `content.<type>` ([`ignored_names`]); on one, what lmgw read from an
    /// answer at another scope ([`left_out_for`]: `content.thinking`,
    /// `content.citations`, `content.<type>`).
    pub fn blocks_ignored(&self, up: &crate::config::Upstream) -> Vec<String> {
        match up.protocol {
            crate::config::Protocol::Anthropic => left_out_for(&self.messages, &Signer::of(up)),
            _ => ignored_in_messages("content", &self.messages),
        }
    }

    /// Which structured-output refusals an upstream's error may be read as
    /// (structured-output design §11.8; [`crate::egress::chat_error`]).
    pub fn format_ask(&self) -> FormatAsk {
        self.response_format
            .as_ref()
            .map_or(FormatAsk::Nothing, ResponseFormat::ask)
    }

    /// The `service_tier` the client asked for, verbatim: it rides in
    /// [`Self::passthrough`] to the upstream, and the row's price note names
    /// it beside the tier that served the request (openai egress design
    /// §5.6). It is never what the row is priced at (decision 10).
    pub fn requested_service_tier(&self) -> Option<String> {
        self.passthrough
            .get("service_tier")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    }

    /// The tier an answer to this request reported, on `route`
    /// ([`crate::pricing::ServedTier::of`]).
    pub fn served_tier(
        &self,
        route: &crate::config::Route,
        served: Option<String>,
    ) -> crate::pricing::ServedTier {
        crate::pricing::ServedTier::of(
            route.upstream.protocol,
            served,
            self.requested_service_tier(),
        )
    }

    /// Concatenated text of all system messages (Anthropic/Gemini hoist this
    /// to a top-level field).
    pub fn system_text(&self) -> Option<String> {
        let parts: Vec<String> = self
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| m.joined_text())
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }

    /// Messages excluding system ones.
    pub fn non_system_messages(&self) -> impl Iterator<Item = &Message> {
        self.messages.iter().filter(|m| m.role != Role::System)
    }

    /// Find the tool name for a tool-call id by scanning prior assistant
    /// ToolUse parts (Gemini correlates results by name, not id). The id
    /// as given wins; failing that, the bare ids are compared, for a client
    /// that kept a call's signature on the call but not on its result.
    pub fn tool_name_for_id(&self, id: &str) -> Option<&str> {
        self.tool_name_for_result(id, self.messages.len())
    }

    /// [`Self::tool_name_for_id`] for the result in `messages[at]`: the
    /// nearest call at or before it wins, so an id that repeats in the
    /// history (an older thread's per-answer `call_0`, an upstream that
    /// numbers its calls per answer) pairs with its own step's call, not the
    /// newest one.
    pub fn tool_name_for_result(&self, id: &str, at: usize) -> Option<&str> {
        let upto = &self.messages[..(at + 1).min(self.messages.len())];
        let find = |same: &dyn Fn(&str) -> bool| {
            upto.iter().rev().find_map(|m| {
                m.content.iter().find_map(|p| match p {
                    ContentPart::ToolUse { id: tid, name, .. } if same(tid) => Some(name.as_str()),
                    _ => None,
                })
            })
        };
        find(&|tid| tid == id).or_else(|| find(&|tid| wire_call_id(tid) == wire_call_id(id)))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// **Total** input tokens for the turn, cache included.
    ///
    /// The three dialects disagree about this and the IR picks one meaning:
    /// OpenAI and Gemini already report the total (their cached counts are a
    /// *detail of* it), while Anthropic reports three disjoint counters, so the
    /// Anthropic adapter sums them (usage-analytics design §2.3). Without that
    /// normalisation the same conversation costs different amounts depending on
    /// which wire shape the client happened to speak, and
    /// `cached_input_tokens` would be a subset of one provider's number and an
    /// addition to another's.
    pub prompt_tokens: Option<u64>,
    /// **Total** output tokens for the turn, reasoning included. OpenAI's
    /// `completion_tokens` and Anthropic's `output_tokens` already mean that;
    /// Gemini reports its thoughts beside `candidatesTokenCount`, not inside
    /// it, so the Gemini adapter adds the two.
    pub completion_tokens: Option<u64>,
    /// Subset of `prompt_tokens` served from the provider's prompt cache and
    /// billed at the (cheaper) cache-read rate. `None` = not reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    /// Subset of `prompt_tokens` written *into* the prompt cache, billed at the
    /// (dearer) cache-write rate in place of the input rate. Anthropic reports
    /// it as `cache_creation_input_tokens`, OpenAI as
    /// `prompt_tokens_details.cache_write_tokens` (billed at 1.25x input since
    /// GPT-5.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Reasoning tokens, when the provider breaks them out. **Informational
    /// only**: they are a subset of `completion_tokens`, so pricing them again
    /// would double-charge a thinking model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

/// llama.cpp `timings` block — exact server-measured prefill/decode metrics.
/// Reported only by llama-server upstreams (cloud providers don't emit it):
/// per-chunk when the request sets `timings_per_token`, otherwise once in the
/// final chunk. Surfaced to the Chat tab's stats panel so it shows real
/// numbers instead of client-side estimates; the public API encoders drop it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    /// Prompt (prefill) tokens actually processed this turn (excludes cache).
    pub prompt_n: u64,
    /// Milliseconds spent on the prompt; `None` when llama.cpp gave no finite
    /// number (it writes a non-finite one as `null`), never a measured zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_ms: Option<f64>,
    /// Prompt tokens processed per second; `None` when not finite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_per_second: Option<f64>,
    /// Generated (decode) tokens so far.
    pub predicted_n: u64,
    /// Milliseconds spent generating; `None` when not finite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicted_ms: Option<f64>,
    /// Tokens generated per second; `None` when not finite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicted_per_second: Option<f64>,
    /// Prompt tokens served from the KV cache (reused, not recomputed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_n: Option<u64>,
    /// Speculative / MTP draft tokens proposed and accepted (when drafting).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_n: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_n_accepted: Option<u64>,
}

impl Usage {
    /// Whether the upstream reported any token count: an answer cut before
    /// its usage did not.
    pub fn reported(&self) -> bool {
        self.prompt_tokens.is_some() || self.completion_tokens.is_some()
    }

    /// Latest-wins. Streaming chunks report *cumulative* counts, so the last
    /// report for a turn is the turn's total — adding them would multiply it.
    pub fn merge(&mut self, other: &Usage) {
        if other.prompt_tokens.is_some() {
            self.prompt_tokens = other.prompt_tokens;
        }
        if other.completion_tokens.is_some() {
            self.completion_tokens = other.completion_tokens;
        }
        // The input group moves together. Replacing `prompt_tokens` while
        // keeping a stale cache subset would leave a total from one report and
        // its subsets from another, and `plain_in` would subtract tokens the
        // new total never contained. No provider does this today (Anthropic's
        // `message_delta` carries only `output_tokens`), which is exactly why
        // it would be a silent wrong number the first time one did.
        if other.prompt_tokens.is_some() {
            self.cached_input_tokens = other.cached_input_tokens;
            self.cache_write_tokens = other.cache_write_tokens;
        }
        if other.reasoning_tokens.is_some() {
            self.reasoning_tokens = other.reasoning_tokens;
        }
    }

    /// Sum, for totalling *separate* upstream calls — the tool loop's turns
    /// (§21). Distinct from [`merge`](Self::merge) on purpose: using that one
    /// across turns silently reports only the final turn, undercounting an
    /// agentic run by however many tool round trips it took.
    pub fn add(&mut self, other: &Usage) {
        if let Some(p) = other.prompt_tokens {
            self.prompt_tokens = Some(self.prompt_tokens.unwrap_or(0) + p);
        }
        if let Some(c) = other.completion_tokens {
            self.completion_tokens = Some(self.completion_tokens.unwrap_or(0) + c);
        }
        // The cache and reasoning subsets sum across turns exactly like the
        // totals they belong to: a tool loop that reads the cache on every turn
        // paid the cache rate on every turn.
        if let Some(v) = other.cached_input_tokens {
            self.cached_input_tokens = Some(self.cached_input_tokens.unwrap_or(0) + v);
        }
        if let Some(v) = other.cache_write_tokens {
            self.cache_write_tokens = Some(self.cache_write_tokens.unwrap_or(0) + v);
        }
        if let Some(v) = other.reasoning_tokens {
            self.reasoning_tokens = Some(self.reasoning_tokens.unwrap_or(0) + v);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    /// The model stopped at one of the request's stop sequences, the one
    /// named: only an upstream that says which one sends it (Anthropic's
    /// `stop_reason: "stop_sequence"` with its `stop_sequence`). Every other
    /// upstream's stop at a sequence is [`Self::Stop`], as it does not say
    /// (api-compat design §9, not yet). Serialized, it is
    /// `{"stop_sequence": "…"}`, where the other reasons are bare strings.
    StopSequence(String),
    Length,
    ToolUse,
    ContentFilter,
    Other(String),
}

impl FinishReason {
    pub fn to_openai(&self) -> &str {
        match self {
            Self::Stop | Self::StopSequence(_) => "stop",
            Self::Length => "length",
            Self::ToolUse => "tool_calls",
            Self::ContentFilter => "content_filter",
            Self::Other(s) => s,
        }
    }

    /// The stop sequence the model stopped at, when the upstream named it:
    /// Anthropic's `stop_sequence`.
    pub fn stop_sequence(&self) -> Option<&str> {
        match self {
            Self::StopSequence(s) => Some(s),
            _ => None,
        }
    }

    pub fn to_anthropic(&self) -> &str {
        match self {
            Self::Stop => "end_turn",
            Self::StopSequence(_) => "stop_sequence",
            Self::Length => "max_tokens",
            Self::ToolUse => "tool_use",
            Self::ContentFilter => "refusal",
            Self::Other(s) => s,
        }
    }
}

/// Non-streaming completion result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Completion {
    /// Text and ToolUse parts.
    pub content: Vec<ContentPart>,
    /// Reasoning / "thinking" trace, kept separate from the answer text
    /// (OpenAI `reasoning_content`, Anthropic `thinking` blocks, Gemini
    /// `thought` parts). Empty for non-reasoning models.
    pub reasoning: String,
    pub finish_reason: FinishReason,
    pub usage: Usage,
    /// Model name reported by the upstream (logged; clients see the alias).
    pub model: String,
    /// llama.cpp `timings`, when the upstream is a llama-server and the request
    /// was *not* streamed. The streaming path carries it as a
    /// [`StreamDelta::Timings`]; before usage analytics there was nowhere for a
    /// non-streamed one to go, so it was parsed by nobody and a non-streaming
    /// local request had no measured throughput at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<Timings>,
    /// The `service_tier` the OpenAI platform's answer reported, verbatim
    /// (openai egress design §5.2): what the row is priced at and the OpenAI
    /// wire relays. Only the platform egress reads it; `None` everywhere
    /// else, and when the answer named none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// The upstream's own record of the answer, where it keeps one: the
    /// OpenAI platform's Responses-wire answers (openai egress design
    /// §7.2.3), which lmgw records under the request's key. `None`
    /// everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_items: Option<UpstreamItems>,
    /// The `context_management` an Anthropic answer carried (the edits it
    /// applied to the request's [`ChatRequest::context_management`]),
    /// verbatim: the Messages wire relays it. Only the Anthropic egress
    /// fills it; `None` everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_management: Option<serde_json::Value>,
    /// An Anthropic answer's `usage.iterations` (one entry per sampling
    /// iteration, a `compaction` entry for a compaction it ran), verbatim:
    /// the Messages wire relays it. [`Self::usage`] already counts the
    /// compaction entries, which Anthropic leaves out of its top-level
    /// counts. Only the Anthropic egress fills it; `None` everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_iterations: Option<UsageIterations>,
}

impl Completion {
    /// Whether the answer's reasoning items are parts of `content`, where
    /// they stood: an answer whose reasoning came sealed (openai egress
    /// design §7.3.1) or signed (an Anthropic upstream's thinking blocks).
    /// `reasoning` is then their texts joined, a view for readers that want
    /// the trace alone.
    pub fn reasoning_in_place(&self) -> bool {
        self.content
            .iter()
            .any(|p| matches!(p, ContentPart::Reasoning { .. }))
    }

    /// The assistant message's parts this answer is in a conversation: its
    /// content as it stands when its reasoning is in place, else its trace
    /// leading the content, as a thinking model expects to see it again.
    pub fn assistant_parts(&self) -> Vec<ContentPart> {
        if self.reasoning_in_place() {
            return self.content.clone();
        }
        let mut parts = Vec::with_capacity(self.content.len() + 1);
        if !self.reasoning.is_empty() {
            parts.push(ContentPart::reasoning(self.reasoning.clone()));
        }
        parts.extend(self.content.iter().cloned());
        parts
    }
}

/// Incremental streaming event, provider-neutral.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamDelta {
    TextDelta(String),
    /// A fragment of the model's refusal ([`ContentPart::Refusal`]): OpenAI's
    /// `delta.refusal` and `response.refusal.delta`. A consumer that has no
    /// refusal of its own reads it as text ([`StreamDelta::refusal_as_text`]).
    RefusalDelta(String),
    /// A fragment of the model's reasoning / "thinking" trace (OpenAI/DeepSeek
    /// `reasoning_content`; llama.cpp emits it for reasoning models). Kept
    /// separate from the answer text so clients can render it distinctly.
    ReasoningDelta(String),
    /// A new tool call began. `index` is a 0-based tool-call ordinal.
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },
    /// A fragment of the JSON-encoded arguments of tool call `index`.
    ToolCallArgsDelta {
        index: usize,
        fragment: String,
    },
    Usage(Usage),
    Stop(FinishReason),
    /// llama.cpp timing snapshot (prefill/decode speeds). Live per-chunk when
    /// `timings_per_token` is enabled, else a single final snapshot. Consumed
    /// by the Chat tab's stats panel; the public API stream encoders drop it.
    Timings(Timings),
    /// The `service_tier` the OpenAI platform's stream reported (openai
    /// egress design §5.2): sent when first seen and again whenever it
    /// changes, ahead of the chunk's other deltas. Only the platform egress
    /// emits it; the OpenAI wire relays it on its chunks, the other wires
    /// drop it.
    ServiceTier(String),
    /// The upstream's record of the answer ([`UpstreamItems`]), sent once
    /// with the terminal event, ahead of the stop: only the OpenAI
    /// platform's Responses wire emits it. lmgw's send sites record it
    /// under the request's key; no client wire carries it.
    UpstreamItems(UpstreamItems),
    /// The seal of the reasoning item just streamed (openai egress design
    /// §7.3.1): the OpenAI platform's Responses wire reads it from the
    /// item's `response.output_item.done`, after that item's summary deltas.
    /// It closes the reasoning part being built: the next reasoning delta
    /// is another item's. Only the platform egress emits it; a client wire
    /// with a carrier writes it there (`/v1/messages`' thinking `signature`,
    /// the `/v1/responses` loop's `encrypted_content`), every other wire
    /// drops it.
    ReasoningSeal(Seal),
    /// Anthropic's `signature` of the thinking block just streamed, from
    /// its `signature_delta` ([`ContentPart::Reasoning`]'s `signature`), and
    /// the provider that answered (`signature_scope`, [`Signer`]). It closes the reasoning
    /// part being built, as [`Self::ReasoningSeal`] does, and a block that
    /// streamed no thinking text (`thinking.display` omitted, the default on
    /// current Claude models) is a part of its own. Only the Anthropic
    /// egress emits it; the Messages wire writes it as that block's
    /// `signature_delta`, the in-process assembler keeps it on the part with
    /// its scope, every other wire drops it.
    ReasoningSignature {
        signature: String,
        scope: Signer,
    },
    /// A verbatim Anthropic block's stream events ([`BlockEvent`]). Only the
    /// Anthropic egress emits them; the Messages wire relays them as they
    /// came, the in-process assembler records the block whole, every other
    /// wire leaves them out.
    AnthropicBlock(BlockEvent),
    /// An Anthropic text block's `content_block_stop`, with the citations
    /// its `citations_delta` events carried, in order (none for most;
    /// [`ContentPart::Text`]'s `citations`): the text after it is another
    /// block's. Only the Anthropic egress emits it; the Messages wire writes
    /// each citation as a `citations_delta` at the text block it closes,
    /// then the block's stop; the in-process assembler keeps them on that
    /// text part, with the `scope` of the row that answered
    /// ([`ContentPart::Text`]'s `citations_scope`); every other wire leaves
    /// them out.
    TextBlockEnd {
        citations: Vec<serde_json::Value>,
        scope: Signer,
    },
    /// The `context_management` an Anthropic stream's `message_delta`
    /// carried ([`Completion::context_management`]), ahead of the stop. Only
    /// the Anthropic egress emits it; the Messages wire relays it on its own
    /// `message_delta`, every other wire drops it.
    ContextManagement(serde_json::Value),
    /// A compaction block's stream events ([`CompactionEvent`]). Only the
    /// Anthropic egress emits them; the Messages wire relays them as they
    /// came, every other wire writes the summary as text.
    Compaction(CompactionEvent),
    /// The `usage.iterations` an Anthropic stream's `message_delta` carried
    /// ([`Completion::usage_iterations`]), ahead of its [`Self::Usage`]. Only
    /// the Anthropic egress emits it; the Messages wire relays it on its own
    /// `message_delta`, every other wire drops it.
    UsageIterations(UsageIterations),
    /// Mid-stream upstream failure; terminates the client stream cleanly.
    Error(String),
}

impl StreamDelta {
    /// This delta as a consumer without a refusal of its own reads it: a
    /// refusal delta as the text delta it is to it, every other as it is.
    pub fn refusal_as_text(&self) -> std::borrow::Cow<'_, StreamDelta> {
        match self {
            Self::RefusalDelta(t) => std::borrow::Cow::Owned(Self::TextDelta(t.clone())),
            other => std::borrow::Cow::Borrowed(other),
        }
    }
}

// ---------------------------------------------------------------------------
// Embeddings (OpenAI shape in/out; routed to embeddings-capable upstreams)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingsRequest {
    pub model_alias: String,
    pub inputs: Vec<String>,
    /// OpenAI's `dimensions`: shorter vectors, cut by the model itself.
    /// Each egress spells it its own way (Gemini: `outputDimensionality`),
    /// and the length that comes back is checked, because a backend without
    /// the parameter (llama-server) ignores it rather than refusing it.
    pub dimensions: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingsResponse {
    pub embeddings: Vec<Vec<f32>>,
    pub usage: Usage,
    pub model: String,
}

// ---------------------------------------------------------------------------
// Rerank (quickdoc §9a; Jina shape in/out, which is what llama-server serves)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct RerankRequest {
    pub model_alias: String,
    pub query: String,
    pub documents: Vec<String>,
    /// Truncate the ranked list upstream. `None` returns a score for every
    /// document — which is what the retrieval pipeline needs, since it maps the
    /// scores back onto its own candidate order.
    pub top_n: Option<usize>,
}

/// One document's score, carrying the index it had in the request. Reranker
/// backends are free to return the list re-ordered, so the index — not the
/// position — is what maps a score back to its document.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerankScore {
    pub index: usize,
    pub score: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RerankResponse {
    pub results: Vec<RerankScore>,
    pub usage: Usage,
    pub model: String,
}

impl RerankResponse {
    /// Scores in the order the documents were sent, which is the order
    /// `quickdoc_core::embed::Reranker` is defined to return them in.
    ///
    /// `None` when the upstream did not score every document — a truncated
    /// answer silently padded with zeros would demote real hits, so the caller
    /// is told instead.
    pub fn in_request_order(&self, n: usize) -> Option<Vec<f32>> {
        let mut out = vec![None; n];
        for r in &self.results {
            *out.get_mut(r.index)? = Some(r.score);
        }
        out.into_iter().collect()
    }
}
