//! The names of OpenAI's Responses wire that lmgw both writes and reads
//! (openai egress design §7.2.2): the stream events, the item types and the
//! content part types, as the create reference spells them
//! (<https://developers.openai.com/api/reference/resources/responses/methods/create.md>,
//! checked 2026-10-24). The platform's Responses-wire egress
//! (`egress/openai/responses`) reads and writes them, and the Responses
//! ingress (`ingress/responses.rs`) writes them for its clients, so the two
//! cannot spell one name two ways.

/// Stream event types (each event's `type`, and its SSE `event:` line).
pub mod event {
    pub const CREATED: &str = "response.created";
    pub const IN_PROGRESS: &str = "response.in_progress";
    pub const OUTPUT_ITEM_ADDED: &str = "response.output_item.added";
    pub const OUTPUT_ITEM_DONE: &str = "response.output_item.done";
    pub const OUTPUT_TEXT_DELTA: &str = "response.output_text.delta";
    pub const REFUSAL_DELTA: &str = "response.refusal.delta";
    pub const REASONING_SUMMARY_TEXT_DELTA: &str = "response.reasoning_summary_text.delta";
    pub const FUNCTION_CALL_ARGUMENTS_DELTA: &str = "response.function_call_arguments.delta";
    pub const COMPLETED: &str = "response.completed";
    pub const INCOMPLETE: &str = "response.incomplete";
    pub const FAILED: &str = "response.failed";
    /// A stream error that is not a Response object's: `{type, code,
    /// message, param}`.
    pub const ERROR: &str = "error";
}

/// Input and output item types.
pub mod item {
    pub const MESSAGE: &str = "message";
    pub const FUNCTION_CALL: &str = "function_call";
    pub const FUNCTION_CALL_OUTPUT: &str = "function_call_output";
    pub const REASONING: &str = "reasoning";
}

/// Content part types.
pub mod part {
    pub const INPUT_TEXT: &str = "input_text";
    pub const INPUT_IMAGE: &str = "input_image";
    pub const OUTPUT_TEXT: &str = "output_text";
    pub const REFUSAL: &str = "refusal";
    pub const SUMMARY_TEXT: &str = "summary_text";
}
