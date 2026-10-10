//! `local_model_test`: the answer of the load test, by the model's class and
//! whether the row is a ladder. The MCP tool `lmgw__local_model_test`
//! answers the same types as JSON text.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What the load test found. Which shape it is follows from the model:
/// a ladder row answers per rung, an image pipeline has its own probe, and
/// every other model answers a pass or a failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(transform = pass_arms_by_class))]
pub enum ModelTest {
    /// A ladder row, every rung tested in turn.
    Ladder(LadderTest),
    /// An image pipeline that drew.
    ImagePassed(ImageTestPassed),
    /// A chat, aux or image model that did not load or did not answer.
    Failed(ModelTestFailed),
    /// A chat model that loaded and answered.
    ChatPassed(ChatTestPassed),
    /// An aux model that loaded and answered.
    Passed(ModelTestPassed),
}

/// The two passes share a shape (a chat model's is an aux model's with more
/// beside it), so the schema tells them apart by `class`: an aux pass is
/// `class: aux`, a chat pass `class: chat`.
#[cfg(feature = "schema")]
fn pass_arms_by_class(schema: &mut schemars::Schema) {
    let Some(arms) = schema.get_mut("anyOf").and_then(|a| a.as_array_mut()) else {
        return;
    };
    for arm in arms.iter_mut() {
        let class = match arm.get("$ref").and_then(|r| r.as_str()) {
            Some(r) if r.ends_with("/ModelTestPassed") => "aux",
            Some(r) if r.ends_with("/ChatTestPassed") => "chat",
            _ => continue,
        };
        let narrowed = serde_json::json!({"properties": {"class": {"const": class}}});
        *arm = serde_json::json!({"allOf": [arm.clone(), narrowed]});
    }
}

/// An aux model that loaded and answered its probe; the first part of a
/// chat model's pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelTestPassed {
    /// Always `true`.
    #[cfg_attr(feature = "schema", schemars(extend("const" = true)))]
    pub ok: bool,
    /// Always `true`.
    #[cfg_attr(feature = "schema", schemars(extend("const" = true)))]
    pub loaded: bool,
    pub model_id: String,
    /// `aux` for an aux model's pass, `chat` where this is the first part of
    /// a chat model's.
    #[cfg_attr(feature = "schema", schemars(extend("enum" = ["chat", "aux"])))]
    pub class: String,
    /// What was sent: `generate` (one token), `embed` or `rerank`.
    pub probe: String,
    /// How long the load and the probe took, in milliseconds.
    pub latency_ms: u64,
    /// For `embed`: the embedding's length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<usize>,
    /// For `rerank`: how many documents came back scored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scored: Option<usize>,
}

/// A chat model that loaded and answered, with the comparison of its running
/// build against the gateway's static derivation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatTestPassed {
    #[serde(flatten)]
    pub base: ModelTestPassed,
    /// llama-server's own `/props` answer, verbatim; null when it could not
    /// be read.
    pub props: Value,
    /// The capabilities the gateway derived statically, the part `/props`
    /// also states an opinion on.
    #[serde(rename = "static")]
    pub static_caps: StaticCaps,
    /// Where the running build and the static derivation disagree; empty
    /// when they agree or `/props` could not be read.
    pub disagreements: Vec<String>,
    /// Why `props` could not be read, when it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The statically derived capabilities a `/props` read is compared with. Each
/// has the shape the `/v1/models` capabilities object gives it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StaticCaps {
    pub reasoning: Option<Value>,
    pub tool_calls: Option<Value>,
    pub input_modalities: Option<Vec<String>>,
}

/// A chat, aux or image model that did not load or did not answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelTestFailed {
    /// Always `false`.
    #[cfg_attr(feature = "schema", schemars(extend("const" = false)))]
    pub ok: bool,
    /// Always `false`.
    #[cfg_attr(feature = "schema", schemars(extend("const" = false)))]
    pub loaded: bool,
    pub model_id: String,
    /// `chat`, `aux` or `image`.
    #[cfg_attr(feature = "schema", schemars(extend("enum" = ["chat", "aux", "image"])))]
    pub class: String,
    /// For an image model: the name clients call it by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_name: Option<String>,
    /// What was sent: `generate`, `embed`, `rerank` or `image_generation`.
    pub probe: String,
    /// For an image model: the route the probe went to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub latency_ms: u64,
    /// What went wrong.
    pub error: String,
    /// The lines of the model's container log that look like the reason.
    pub container_log: Vec<String>,
    /// What to try.
    pub hint: String,
}

/// An image pipeline that drew its probe image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageTestPassed {
    /// Always `true`.
    #[cfg_attr(feature = "schema", schemars(extend("const" = true)))]
    pub ok: bool,
    /// Always `true`.
    #[cfg_attr(feature = "schema", schemars(extend("const" = true)))]
    pub loaded: bool,
    pub model_id: String,
    /// Always `image`.
    #[cfg_attr(feature = "schema", schemars(extend("const" = "image")))]
    pub class: String,
    pub public_name: String,
    /// Always `image_generation`.
    #[cfg_attr(feature = "schema", schemars(extend("const" = "image_generation")))]
    pub probe: String,
    pub endpoint: String,
    pub latency_ms: u64,
    /// The probe's size, e.g. `256x256`.
    pub size: String,
    pub steps: u32,
    pub seed: i64,
    /// How many images came back.
    pub n: usize,
    /// What the server said it encoded; null when it says nothing.
    pub output_format: Option<Value>,
    /// The first image's size in bytes.
    pub bytes: usize,
    pub note: String,
}

/// A ladder row's test: every rung loaded and probed in turn, then the
/// model stopped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LadderTest {
    /// Every tested rung passed.
    pub ok: bool,
    pub model_id: String,
    /// Always `chat`.
    #[cfg_attr(feature = "schema", schemars(extend("const" = "chat")))]
    pub class: String,
    /// Always `true`.
    #[cfg_attr(feature = "schema", schemars(extend("const" = true)))]
    pub ladder: bool,
    /// How many rungs the ladder has.
    pub top_rung: usize,
    /// The rungs tested, in order; a failure ends the list.
    pub rungs: Vec<RungTest>,
    /// The rung the model was on before the test, when it was running.
    pub was_running_rung: Option<usize>,
    /// The model was stopped again so the next request starts at the base;
    /// false when other traffic kept it busy.
    pub reset_to_base: bool,
}

/// One rung's test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RungTest {
    /// This rung's number, from 1 (the base row).
    pub rung: usize,
    pub of: usize,
    pub gguf_path: Option<String>,
    pub ctx_size: Option<i64>,
    pub per_slot_ctx: Option<i64>,
    pub switchover: Option<i64>,
    /// Seconds it took the rung to become ready.
    pub load_seconds: f64,
    pub ok: bool,
    /// Why the rung failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
