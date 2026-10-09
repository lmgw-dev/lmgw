//! Canonical intermediate representation (§5).
//!
//! Provider-neutral request/response types rich enough to round-trip the
//! OpenAI and Anthropic ingress shapes and all three egress shapes.

use serde::{Deserialize, Serialize};

mod call_id;
pub use call_id::{
    call_id_with_signature, split_call_id, wire_call_id, wire_call_ids_in_messages_body,
    wire_call_ids_in_responses_body, THOUGHT_SIGNATURE_MARKER, THOUGHT_SIGNATURE_TEXT_MARKER,
};

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
    Text {
        text: String,
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
    /// own hand has none.
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

impl ContentPart {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }

    /// An unsigned reasoning trace.
    pub fn reasoning(s: impl Into<String>) -> Self {
        Self::Reasoning {
            text: s.into(),
            signature: None,
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
/// so the caller can log the loss rather than hide it (§14). Binary blocks are
/// deliberately **not** inlined: base64ing a 1 MB image into the prompt costs
/// ~350k tokens, which is a far worse failure than a named placeholder.
pub fn flatten_tool_result(blocks: &[ToolResultBlock]) -> (String, Vec<String>) {
    let mut parts: Vec<String> = Vec::with_capacity(blocks.len());
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
        }
    }
    (parts.join("\n"), notes)
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
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Concatenated text of all `Reasoning` parts — the trace as protocols
    /// that carry it in one field (`reasoning_content`) want it. Empty when
    /// the turn has none.
    pub fn reasoning_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Reasoning { text, .. } => Some(text.as_str()),
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
}

impl ReasoningControl {
    /// Nothing set — the egress emits no reasoning keys at all.
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none() && self.effort.is_none() && self.budget_tokens.is_none()
    }

    /// Field-wise merge with `self` as the higher tier (§5.1/§5.2): a client
    /// that sets only `enabled` keeps the alias default's `effort`, and a
    /// header that sets only `effort` keeps the body's `budget_tokens`.
    pub fn merge_over(mut self, lower: Option<Self>) -> Self {
        if let Some(l) = lower {
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
    /// - `enabled == Some(false)` ⇒ no effort, no budget;
    /// - a real level or a positive budget ⇒ `enabled = Some(true)`.
    ///
    /// That last rule is what makes precedence work **per tier** rather than
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
    #[serde(default)]
    pub stream: bool,
    /// Top-level request fields the IR does not otherwise model (e.g.
    /// `response_format`, `grammar`, `json_schema`, extra llama.cpp sampler
    /// params). Captured verbatim at OpenAI ingress and re-emitted verbatim on
    /// OpenAI egress so OpenAI-compatible upstreams (llama-server) keep
    /// receiving them — without this the OpenAI→OpenAI route, documented as
    /// "near pass-through", would silently strip anything not explicitly
    /// modeled above. Empty for cross-protocol routes, where these
    /// OpenAI-shaped fields have no equivalent and are dropped per-egress.
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
}

impl ChatRequest {
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
    pub prompt_ms: f64,
    pub prompt_per_second: f64,
    /// Generated (decode) tokens so far.
    pub predicted_n: u64,
    pub predicted_ms: f64,
    pub predicted_per_second: f64,
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
    Length,
    ToolUse,
    ContentFilter,
    Other(String),
}

impl FinishReason {
    pub fn to_openai(&self) -> &str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolUse => "tool_calls",
            Self::ContentFilter => "content_filter",
            Self::Other(s) => s,
        }
    }

    pub fn to_anthropic(&self) -> &str {
        match self {
            Self::Stop => "end_turn",
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
}

/// Incremental streaming event, provider-neutral.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamDelta {
    TextDelta(String),
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
    /// Mid-stream upstream failure; terminates the client stream cleanly.
    Error(String),
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
