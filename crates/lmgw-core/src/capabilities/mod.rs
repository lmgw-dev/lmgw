//! What a model can do, as `/v1/models` publishes it (model-capabilities
//! design `docs/design/2026-09-17-model-capabilities-design.md`
//! §2.1, §3, §4).
//!
//! Five pure builders, one per class of routable model — local llama.cpp chat
//! rows, aux (embedding/rerank) rows, audio.cpp rows, stable-diffusion.cpp
//! image rows, and cloud catalog entries. "Pure" is the point: every builder takes already-read facts
//! (a [`ModelSummary`] the caller pulled through `state.gguf_cache`, a parsed
//! [`ModelInfo`]) and returns a [`Derived`] without touching the filesystem,
//! the network or the clock, so the listing handler stays the only place that
//! does I/O and every rule below is unit-testable on hand-built rows.
//!
//! The cardinal rule of the schema is **absent means unknown**: no field is
//! ever defaulted to make the object look complete. Every optional field is
//! `skip_serializing_if`-elided and every list is dropped when empty, so a
//! consumer can never mistake "lmgw has no idea" for "the model cannot".
//!
//! Notes — the plain-language half of the contract — live in [`notes`], which
//! the builders call with the same facts they published.

pub mod exposed;
pub mod notes;

use serde::{Deserialize, Serialize};

use crate::catalog::ModelInfo;
use crate::config::{AudioModel, AuxKind, AuxModel, ImageModel, LlamaParams, LocalModel, Protocol};
use crate::gguf::{ModelSummary, TemplateSignals};
use crate::ir::Params;
use crate::runtime::image::ImageCapabilities;

// ---------------------------------------------------------------------------
// Schema (design §2.1)
// ---------------------------------------------------------------------------

/// The `capabilities` object of one model on `GET /v1/models` (design §2.1).
///
/// `task`, `endpoints` and `source` are always present — they are what makes
/// the entry actionable at all ("this id exists, here is where to send it,
/// here is how much to trust the rest"). Everything else is absent unless a
/// source stated it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelCapabilities {
    /// `chat` | `embedding` | `rerank` | `tts` | `asr` | an audio.cpp task
    /// name. Drives [`endpoints`](Self::endpoints).
    pub task: String,
    /// The lmgw routes that accept this model id.
    pub endpoints: Vec<String>,
    /// Subset of `text`, `image`, `audio`, `video`, `file`, `embedding`.
    /// Absent = unknown, never "none".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_modalities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_modalities: Option<Vec<String>>,
    /// `input_modalities` contains `image`. Redundant with the list on
    /// purpose: the consuming application reads this one boolean to decide
    /// whether a session gets a screenshot tool. Absent exactly when
    /// `input_modalities` is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningCaps>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<ToolCallCaps>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<StructuredOutputCaps>,
    /// Where the facts came from: `gguf+config`, `catalog`, `config`,
    /// `owner`.
    pub source: String,
}

/// Reasoning / thinking support and how a request controls it (design §2.1,
/// §3.2, §4).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReasoningCaps {
    /// `fixed` (nothing to change per request), `toggle` (on/off per
    /// request), `levels` (effort selectable per request).
    pub kind: String,
    /// The default state as this alias is configured. Absent when no source
    /// states it — "the catalog says the model supports reasoning" does not
    /// say whether it is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Accepted effort values, least → most in the canonical order. Only for
    /// `kind == "levels"`; omitted entirely when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<String>,
    /// The effort in force when the request sets none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Whether a request can switch thinking off entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_disable: Option<bool>,
    /// The configured default thinking-token budget, when set and non-zero
    /// (a zero budget is published as `enabled: false`, not as a budget).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<i64>,
    /// Whether replayed reasoning of earlier turns reaches the model again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserve_history: Option<bool>,
    /// The exact header and body fields that work on this route; omitted when
    /// empty (nothing to control on a `fixed` model).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub control: Vec<String>,
}

/// Tool-calling support (design §2.1, §3.4).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolCallCaps {
    /// `native` (lmgw returns structured `tool_calls`), `none` (the template
    /// renders no tools; aux/audio rows), `text` (the template renders tools
    /// in a syntax llama-server has no parser for, so calls may arrive as
    /// prose the caller must read itself).
    pub kind: String,
    /// The template can render more than one call per turn. Absent for cloud
    /// models, which publish nothing about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<bool>,
    /// The native syntax family the template renders (`qwen-xml`,
    /// `hermes-json`, …), `provider` for cloud, `unknown` when the template
    /// renders tools with none of the known markers. Informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

/// `response_format` support (design §2.1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StructuredOutputCaps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_object: Option<bool>,
}

/// Everything one builder derives for one exposed model: the `capabilities`
/// object, the two top-level model fields that come out of the same facts
/// (`max_output_tokens`, `created`) and the per-model `notes`.
///
/// `capabilities: None` means the facts could not be read at all (an
/// unreadable GGUF) — the notes then say which file, so the answer is a
/// diagnosis rather than a silent gap. `created: None` means the row has no
/// timestamp of its own and the caller should stamp the gateway's start time
/// (design §2.1, `created` bullet).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Derived {
    pub capabilities: Option<ModelCapabilities>,
    pub max_output_tokens: Option<u64>,
    pub notes: Vec<String>,
    pub created: Option<i64>,
}

// ---------------------------------------------------------------------------
// Route constants
// ---------------------------------------------------------------------------

/// Chat routes on an OpenAI-protocol upstream (local rows included):
/// `/v1/completions` refuses every other protocol, so it only appears here.
pub const CHAT_ENDPOINTS_OPENAI: [&str; 4] = [
    "/v1/chat/completions",
    "/v1/messages",
    "/v1/responses",
    "/v1/completions",
];

/// Chat routes on an Anthropic- or Gemini-protocol upstream.
pub const CHAT_ENDPOINTS_OTHER: [&str; 3] =
    ["/v1/chat/completions", "/v1/messages", "/v1/responses"];

/// The image class's two ingress routes (image-generation design §6). Named
/// here rather than spelled out at each site because the published
/// `endpoints` list and the route guard in `proxy` have to be the same
/// strings — a model that advertises a route lmgw then refuses is worse than
/// one that advertises nothing.
pub const IMAGE_GENERATIONS_ENDPOINT: &str = "/v1/images/generations";
/// The multipart twin of [`IMAGE_GENERATIONS_ENDPOINT`].
pub const IMAGE_EDITS_ENDPOINT: &str = "/v1/images/edits";

/// Every reasoning control that can reach a local llama-server row (design
/// §5.3), in the order a row publishes the ones it actually honours
/// (`local_control` filters this list per template).
///
/// The budget header is deliberately absent: the shipped llama.cpp build
/// ignores a per-request `reasoning_budget_tokens` (live-probed 2026-09-17),
/// and `control` states what *works*, not what is forwarded.
pub const LOCAL_REASONING_CONTROL: [&str; 4] = [
    "x-lmgw-reasoning",
    "x-lmgw-reasoning-effort",
    "reasoning_effort",
    "chat_template_kwargs.enable_thinking",
];

/// The subset of [`LOCAL_REASONING_CONTROL`] that llama-server renders into
/// the template's `enable_thinking` variable — worth listing only when the
/// template reads that variable.
pub const LOCAL_TOGGLE_CONTROL: [&str; 2] =
    ["x-lmgw-reasoning", "chat_template_kwargs.enable_thinking"];

/// The subset of [`LOCAL_REASONING_CONTROL`] that llama-server renders into
/// the template's `reasoning_effort` / `resolved_reasoning_effort` variables.
/// A template that reads neither (gpt-oss-style `reasoning_strength`) cannot
/// be steered by them, so they are not published for it.
pub const LOCAL_EFFORT_CONTROL: [&str; 2] = ["x-lmgw-reasoning-effort", "reasoning_effort"];

/// The template variable names a request's `reasoning_effort` actually reaches
/// through llama-server. Every other name `gguf::TemplateSignals` recognises
/// (`reasoning_strength`) is one the model's own template invented for itself.
const SETTABLE_EFFORT_VARS: [&str; 2] = ["reasoning_effort", "resolved_reasoning_effort"];

/// Whether an effort level lmgw sends lands anywhere in this template.
fn reads_settable_effort(s: &TemplateSignals) -> bool {
    s.effort_var_names
        .iter()
        .any(|n| SETTABLE_EFFORT_VARS.contains(&n.as_str()))
}

/// The controls this row honours, in [`LOCAL_REASONING_CONTROL`]'s order: the
/// `enable_thinking` pair when the template reads that variable, the effort
/// pair when it reads a variable llama-server fills from `reasoning_effort`.
///
/// Publishing the full list unconditionally was the bug this replaces: on a
/// template that reads neither variable, a client following `control` sends a
/// header that changes nothing and has no way to find out.
fn local_control(s: &TemplateSignals) -> Vec<String> {
    let effort = reads_settable_effort(s);
    LOCAL_REASONING_CONTROL
        .iter()
        .filter(|c| {
            if LOCAL_TOGGLE_CONTROL.contains(c) {
                s.enable_thinking_var
            } else {
                effort
            }
        })
        .map(|c| (*c).to_string())
        .collect()
}

/// Reasoning controls on a generic OpenAI-protocol upstream (design §5.3):
/// `enabled: false` becomes `reasoning_effort: "none"`, a budget is ignored.
pub const OPENAI_REASONING_CONTROL: [&str; 3] = [
    "x-lmgw-reasoning",
    "x-lmgw-reasoning-effort",
    "reasoning_effort",
];

/// Reasoning controls on an Anthropic upstream (design §5.3): the full
/// triple, plus the two native body fields it is rendered into.
pub const ANTHROPIC_REASONING_CONTROL: [&str; 5] = [
    "x-lmgw-reasoning",
    "x-lmgw-reasoning-effort",
    "x-lmgw-reasoning-budget",
    "thinking",
    "output_config.effort",
];

/// Reasoning controls on a Gemini upstream: the budget is the knob
/// (`thinkingConfig.thinkingBudget`, `0` = off).
pub const GEMINI_REASONING_CONTROL: [&str; 3] = [
    "x-lmgw-reasoning",
    "x-lmgw-reasoning-effort",
    "x-lmgw-reasoning-budget",
];

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

// ---------------------------------------------------------------------------
// Local rows (design §3.2–§3.4)
// ---------------------------------------------------------------------------

/// What the caller found out about this row's multimodal projector before
/// calling [`for_local_row`] — the file work (does the path exist, does a
/// sibling `*mmproj*.gguf` sit next to the weights, did the header parse)
/// belongs to the listing handler; the rules that turn it into modalities and
/// notes belong here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectorStatus<'a> {
    /// No `--mmproj` configured and no sibling projector on disk.
    None,
    /// A projector is configured and was read — its summary is the
    /// `projector` argument.
    Configured { path: &'a str },
    /// A projector is configured but its header could not be read.
    Unreadable { path: &'a str, err: &'a str },
    /// No projector is configured, but one sits next to the weights.
    /// llama-server started with `-m` never picks that up by itself (§3.3),
    /// so this is text-only plus a note.
    SiblingPresentNotConfigured { path: &'a str },
}

/// The projector llama-server will actually load for this row: the typed
/// field first, then a `--mmproj` left in the freeform args (design §3.3).
///
/// This mirrors `runtime::argv`'s own precedence rather than restating §3.3
/// loosely, because the published modalities have to describe the process that
/// actually starts:
///
/// - `mmproj_path` set ⇒ that projector, even with `no_mmproj` also set — the
///   renderer emits `--mmproj` and drops `--no-mmproj` in that case.
/// - `mmproj_path` unset and `no_mmproj` set ⇒ **no** projector: the renderer
///   emits `--no-mmproj` and claims the `mmproj` key, so a `--mmproj` left in
///   the freeform args never reaches the command line.
/// - otherwise, a `--mmproj` in the freeform args.
pub fn configured_projector(model: &LocalModel) -> Option<&str> {
    if let Some(p) = model.params.mmproj_path.as_deref() {
        if !p.trim().is_empty() {
            return Some(p);
        }
    }
    if model.params.no_mmproj {
        return None;
    }
    arg_value(&model.args, "mmproj")
}

/// The value of `--<flag>` in a freeform arg list, in either the spaced or the
/// `=` spelling. Mirrors `LocalModel::hoist_promoted_args`' own parse,
/// including its "the next token is the value unless it is itself an option"
/// rule.
fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut i = 0;
    while i < args.len() {
        let raw = args[i].as_str();
        let (tok, inline) = match raw.split_once('=') {
            Some((f, v)) if f.starts_with('-') => (f, Some(v)),
            _ => (raw, None),
        };
        if tok.starts_with('-') && tok.trim_start_matches('-') == flag {
            return match inline {
                Some(v) => Some(v),
                None => args
                    .get(i + 1)
                    .map(String::as_str)
                    .filter(|v| !crate::runtime::argv::is_opt(v)),
            };
        }
        i += 1;
    }
    None
}

/// Capabilities of a local llama.cpp chat row (design §3.2–§3.4), and of an
/// alias onto one (§3.6 — the alias path is this same function with
/// `overrides = Some(&alias.param_overrides)`; there is no second builder,
/// because an alias that changed nothing must publish exactly what the row
/// publishes).
///
/// - `weights` — the row's GGUF summary; `None` means the file could not be
///   read, which yields no `capabilities` at all plus a note naming it. A
///   guess would be worse than a hole here: the whole point of `source:
///   gguf+config` is that someone read the file.
/// - `projector` — the configured projector's summary, when
///   `projector_status` is [`ProjectorStatus::Configured`].
/// - `template_override` — the raw text of the row's `chat_template_file`
///   when it has one. That file, not the GGUF's embedded template, is what
///   llama-server renders, so it wins (§3.1).
/// - `overrides` — an alias' `param_overrides`, whose `reasoning` control
///   overrides the row's configured default state/effort. `max_tokens` is
///   deliberately *not* read: it is a per-request default, not a cap, while
///   `max_output_tokens` publishes the row's `--n-predict`, which llama-server
///   enforces.
pub fn for_local_row(
    model: &LocalModel,
    weights: Option<&ModelSummary>,
    projector: Option<&ModelSummary>,
    template_override: Option<&str>,
    projector_status: ProjectorStatus<'_>,
    overrides: Option<&Params>,
) -> Derived {
    // `--n-predict` is the only real "one response may generate this many"
    // number a local row has; `-1` (llama.cpp's unbounded sentinel) is not one.
    let max_output_tokens = model.params.n_predict.filter(|n| *n > 0).map(|n| n as u64);

    let Some(weights) = weights else {
        return Derived {
            capabilities: None,
            max_output_tokens,
            notes: notes::notes_for_local(
                model,
                None,
                None,
                max_output_tokens,
                projector_status,
                None,
            ),
            created: None,
        };
    };

    let signals = match template_override {
        Some(text) => TemplateSignals::from_template(text),
        None => weights.signals.clone().unwrap_or_default(),
    };

    let (input_modalities, output_modalities) = local_modalities(projector_status, projector);
    let vision = input_modalities
        .as_ref()
        .map(|m| m.iter().any(|x| x == "image"));

    let caps = ModelCapabilities {
        task: "chat".to_string(),
        endpoints: strings(&CHAT_ENDPOINTS_OPENAI),
        input_modalities,
        output_modalities,
        vision,
        reasoning: Some(local_reasoning(&signals, &model.params, overrides)),
        tool_calls: Some(local_tool_calls(&signals)),
        // llama-server backs `response_format` with a GBNF grammar for both
        // shapes, on every chat model it serves.
        structured_output: Some(StructuredOutputCaps {
            json_schema: Some(true),
            json_object: Some(true),
        }),
        source: "gguf+config".to_string(),
    };

    let notes = notes::notes_for_local(
        model,
        Some(&caps),
        Some(&signals),
        max_output_tokens,
        projector_status,
        projector,
    );
    Derived {
        capabilities: Some(caps),
        max_output_tokens,
        notes,
        created: None,
    }
}

/// `(input, output)` modalities of a local row (design §3.3). Text is always
/// in and out; everything else is the configured projector's header talking.
fn local_modalities(
    status: ProjectorStatus<'_>,
    projector: Option<&ModelSummary>,
) -> (Option<Vec<String>>, Option<Vec<String>>) {
    let text_out = Some(vec!["text".to_string()]);
    let input = match status {
        ProjectorStatus::None | ProjectorStatus::SiblingPresentNotConfigured { .. } => {
            Some(vec!["text".to_string()])
        }
        // A configured projector lmgw could not inspect leaves the input side
        // unknown: it is exactly the file that would add `image`/`audio`.
        ProjectorStatus::Unreadable { .. } => None,
        ProjectorStatus::Configured { .. } => match projector {
            Some(p) => {
                let vision = p.has_vision_encoder == Some(true);
                let audio = p.has_audio_encoder == Some(true);
                // A header that states neither encoder says nothing at all —
                // `gguf::summarize` already falls back to the `clip.vision.*` /
                // `clip.audio.*` blocks, so reaching here means unknown.
                if p.has_vision_encoder.is_none() && p.has_audio_encoder.is_none() {
                    None
                } else {
                    let mut m = vec!["text".to_string()];
                    if vision {
                        m.push("image".to_string());
                    }
                    if audio {
                        m.push("audio".to_string());
                    }
                    Some(m)
                }
            }
            None => None,
        },
    };
    (input, text_out)
}

/// Reasoning kind, state and controls of a local row — design §3.2's table,
/// with the alias' `param_overrides.reasoning` folded over the row's defaults
/// (§3.6).
fn local_reasoning(
    s: &TemplateSignals,
    p: &LlamaParams,
    overrides: Option<&Params>,
) -> ReasoningCaps {
    let markers = s.thinking_markers || s.enable_thinking_var || s.reasoning_effort_var;

    let kwargs_on = p
        .chat_template_kwargs
        .get("enable_thinking")
        .and_then(serde_json::Value::as_bool);
    let budget_off = p.reasoning_budget == Some(0);
    let configured_on = if budget_off {
        Some(false)
    } else {
        match p.reasoning.as_deref() {
            Some("on") => Some(true),
            Some("off") => Some(false),
            _ => None,
        }
        .or(kwargs_on)
    };
    let effective_on = configured_on
        .or(s.enable_thinking_default)
        .unwrap_or(markers);

    // `fold_reasoning_effort()` normally moves a `reasoning_effort` kwarg into
    // the field on load; replicated (field wins) so a hand-built row that
    // never went through `hoist_promoted_args` derives the same default.
    let default_effort = p
        .reasoning_effort
        .clone()
        .or_else(|| {
            p.chat_template_kwargs
                .get("reasoning_effort")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| s.effort_default.clone());

    // What a *request* can change here. `reasoning_effort_var` is not enough:
    // llama-server only ever fills `reasoning_effort` /
    // `resolved_reasoning_effort`, so a template reading its own
    // `reasoning_strength` has an effort knob no client can turn (§2.1
    // `control`).
    let settable_effort = reads_settable_effort(s);
    let controllable = s.enable_thinking_var || settable_effort;

    let mut caps = if !markers {
        ReasoningCaps {
            kind: "fixed".to_string(),
            enabled: Some(false),
            ..Default::default()
        }
    } else if settable_effort && !s.effort_levels.is_empty() {
        ReasoningCaps {
            kind: "levels".to_string(),
            enabled: Some(effective_on),
            levels: s.effort_levels.clone(),
            default: default_effort,
            can_disable: Some(s.enable_thinking_var),
            ..Default::default()
        }
    } else if controllable {
        ReasoningCaps {
            kind: "toggle".to_string(),
            enabled: Some(effective_on),
            // A toggle template that still reads `reasoning_effort` (Cohere
            // North compares it against `"none"` only) has a meaningful
            // configured default; one that reads `enable_thinking` alone does
            // not, and publishing an effort it never looks at would be a lie.
            default: settable_effort.then_some(default_effort).flatten(),
            can_disable: Some(s.enable_thinking_var),
            ..Default::default()
        }
    } else {
        // Thinking markers but nothing a request can turn: the template thinks
        // on its own terms — `fixed`, not a `toggle` whose switch is wired to
        // nothing. §3.2's gloss — "budget 0 is the only brake" — is applied
        // rather than merely noted, so the object cannot claim a row with
        // `--reasoning-budget 0` thinks.
        ReasoningCaps {
            kind: "fixed".to_string(),
            enabled: Some(!budget_off),
            ..Default::default()
        }
    };

    caps.budget_tokens = p.reasoning_budget.filter(|b| *b > 0);
    // Stated beats inferred: the flag, then the kwarg that reaches the same
    // template variable, then the template's mere mention of it.
    caps.preserve_history = p
        .reasoning_preserve
        .or_else(|| {
            p.chat_template_kwargs
                .get("preserve_thinking")
                .and_then(serde_json::Value::as_bool)
        })
        .or_else(|| s.preserve_thinking_var.then_some(true));

    if caps.kind != "fixed" {
        // An alias' own reasoning defaults sit above the row's (§3.6). The
        // control is normalised first, so `effort: "none"` and `budget: 0`
        // arrive as the `enabled: false` they mean.
        if let Some(ov) = overrides
            .and_then(|o| o.reasoning.clone())
            .map(crate::ir::ReasoningControl::normalised)
        {
            if let Some(enabled) = ov.enabled {
                caps.enabled = Some(enabled);
            }
            if let Some(effort) = ov.effort {
                caps.default = Some(effort);
            }
            if let Some(budget) = ov.budget_tokens {
                caps.budget_tokens = Some(budget);
            }
            if ov.enabled == Some(false) {
                caps.budget_tokens = None;
            }
        }
        caps.control = local_control(s);
    }
    caps
}

/// Tool-calling of a local row (design §3.4): the template renders `tools` or
/// it does not — `--jinja` is on in the shipped build and lmgw never emits
/// `--no-jinja`, so no param enters this.
///
/// A template that renders tools in a syntax matching none of the known
/// markers is `text`, not `native`: llama-server has no parser for it, so the
/// calls may come back as prose in `content` that the caller has to read
/// itself. That is exactly what the consuming spec means by `text`, and
/// calling it `native` would promise structured `tool_calls` lmgw cannot
/// deliver.
fn local_tool_calls(s: &TemplateSignals) -> ToolCallCaps {
    match (s.tools_var, s.tool_call_format.as_deref()) {
        (false, _) => ToolCallCaps {
            kind: "none".to_string(),
            parallel: None,
            format: None,
        },
        (true, Some(format)) => ToolCallCaps {
            kind: "native".to_string(),
            parallel: Some(s.parallel_tool_calls),
            format: Some(format.to_string()),
        },
        (true, None) => ToolCallCaps {
            kind: "text".to_string(),
            parallel: Some(s.parallel_tool_calls),
            format: Some("unknown".to_string()),
        },
    }
}

// ---------------------------------------------------------------------------
// Aux rows (embedding / rerank)
// ---------------------------------------------------------------------------

/// Capabilities of an aux row. `source: config` — an embedder's GGUF carries
/// no chat template and no projector header to read, so the row's own kind is
/// the whole story.
///
/// `summary` is the row's GGUF summary when the caller could read it; `None`
/// only earns a note (unlike a chat row, nothing published here was derived
/// from the file, so the capabilities stand).
pub fn for_aux(model: &AuxModel, summary: Option<&ModelSummary>) -> Derived {
    let projector = arg_value(&model.args, "mmproj");
    // A multimodal embedder's extra input modalities live in the projector
    // header, which this builder does not read — so the list is withheld
    // rather than published as text-only, which would be wrong.
    let input_modalities = match projector {
        Some(_) => None,
        None => Some(vec!["text".to_string()]),
    };
    let vision = input_modalities
        .as_ref()
        .map(|m| m.iter().any(|x| x == "image"));

    let (task, endpoints, output_modalities) = match model.kind {
        AuxKind::Embed => (
            "embedding",
            vec!["/v1/embeddings".to_string()],
            Some(vec!["embedding".to_string()]),
        ),
        // A reranker answers with relevance scores, which is none of the
        // modality vocabulary — absent rather than mislabelled.
        AuxKind::Rerank => ("rerank", vec!["/v1/rerank".to_string()], None),
    };

    let caps = ModelCapabilities {
        task: task.to_string(),
        endpoints,
        input_modalities,
        output_modalities,
        vision,
        reasoning: None,
        tool_calls: Some(ToolCallCaps {
            kind: "none".to_string(),
            parallel: None,
            format: None,
        }),
        structured_output: None,
        source: "config".to_string(),
    };
    let notes = notes::notes_for_aux(model, projector, summary.is_none());
    Derived {
        capabilities: Some(caps),
        max_output_tokens: None,
        notes,
        created: None,
    }
}

// ---------------------------------------------------------------------------
// Audio rows (audio.cpp)
// ---------------------------------------------------------------------------

/// Capabilities of an audio.cpp row. The row's `task` decides both the
/// modalities and the routes: `tts` and `asr` have OpenAI-shaped endpoints,
/// the other twelve tasks are reached only through `/v1/tasks/run`, whose
/// nested `request` object lmgw relays untouched — so their modalities are
/// audio.cpp's business, not something to guess here.
pub fn for_audio(model: &AudioModel) -> Derived {
    let (input, output, endpoints): (Option<Vec<String>>, Option<Vec<String>>, Vec<&str>) =
        match model.task.as_str() {
            "tts" => (
                Some(vec!["text".to_string()]),
                Some(vec!["audio".to_string()]),
                vec!["/v1/audio/speech", "/v1/audio/voices"],
            ),
            "asr" => (
                Some(vec!["audio".to_string()]),
                Some(vec!["text".to_string()]),
                vec![
                    "/v1/audio/transcriptions",
                    "/v1/audio/transcriptions/details",
                ],
            ),
            // Forced alignment takes both halves — the clip and the
            // transcript it should be aligned against — and has had an
            // OpenAI-shaped upload route of its own since audio.cpp added
            // `/v1/audio/alignments`. The generic route still reaches it, so
            // both are published.
            "align" => (
                Some(vec!["audio".to_string(), "text".to_string()]),
                Some(vec!["text".to_string()]),
                vec!["/v1/audio/alignments", "/v1/tasks/run", "/v1/tasks/stream"],
            ),
            _ => (None, None, vec!["/v1/tasks/run", "/v1/tasks/stream"]),
        };
    let vision = input.as_ref().map(|m| m.iter().any(|x| x == "image"));

    let caps = ModelCapabilities {
        task: model.task.clone(),
        endpoints: strings(&endpoints),
        input_modalities: input,
        output_modalities: output,
        vision,
        reasoning: None,
        tool_calls: Some(ToolCallCaps {
            kind: "none".to_string(),
            parallel: None,
            format: None,
        }),
        structured_output: None,
        source: "config".to_string(),
    };
    let notes = notes::notes_for_audio(model);
    Derived {
        capabilities: Some(caps),
        max_output_tokens: None,
        notes,
        created: None,
    }
}

// ---------------------------------------------------------------------------
// Image rows (stable-diffusion.cpp)
// ---------------------------------------------------------------------------

/// Capabilities of an sd-server row (image-generation design §5).
///
/// `source: config` for the same reason aux and audio are: there is no header
/// to read and no catalog to ask, so the row's own `modes` and `edit` columns
/// are the whole published story. Everything a diffusion pipeline does not
/// have — reasoning, tool calls, structured output, a context window, a token
/// budget — stays absent rather than being published as a negative.
///
/// `probed` is the `GET /sdcpp/v1/capabilities` body this model's container
/// answered with, when one is running. Nothing in the capability object
/// depends on it: it fills **notes only** (the accepted sizes, the samplers,
/// the LoRAs), because those describe the loaded process and disappear with
/// it, and a capability field that came and went with a container would be a
/// worse contract than no field at all.
pub fn for_image(model: &ImageModel, probed: Option<&ImageCapabilities>) -> Derived {
    let modes = model.modes();
    // A row that only does video is not an image generator — it is published
    // as what it is, with `video` out. Its routes are still the image ones:
    // §13 leaves the video contract unbuilt, and inventing a `/v1/videos`
    // endpoint here would advertise something no build serves.
    let video_only = modes.iter().all(|m| m == "vid_gen");

    let task = if video_only {
        "video_generation"
    } else if model.edit {
        "image_edit"
    } else {
        "image_generation"
    };

    let mut endpoints = vec![IMAGE_GENERATIONS_ENDPOINT.to_string()];
    let mut input = vec!["text".to_string()];
    // The `edit` column, not the pipeline's self-reported `ref_images`: a
    // pipeline that cannot take a reference image **segfaults** on one
    // (§12.8), so this list is also the gate `/v1/images/edits` enforces.
    if model.edit {
        endpoints.push(IMAGE_EDITS_ENDPOINT.to_string());
        input.push("image".to_string());
    }
    let output = if video_only { "video" } else { "image" };

    let caps = ModelCapabilities {
        task: task.to_string(),
        endpoints,
        vision: Some(input.iter().any(|m| m == "image")),
        input_modalities: Some(input),
        output_modalities: Some(vec![output.to_string()]),
        reasoning: None,
        tool_calls: None,
        structured_output: None,
        source: "config".to_string(),
    };
    let notes = notes::notes_for_image(model, probed);
    Derived {
        capabilities: Some(caps),
        max_output_tokens: None,
        notes,
        created: None,
    }
}

// ---------------------------------------------------------------------------
// Cloud catalog entries (design §4)
// ---------------------------------------------------------------------------

/// Capabilities of a cloud model, from what its upstream's catalog publishes
/// and nothing else (design §4).
///
/// A catalog that publishes nothing still yields a `capabilities` object with
/// `task`, `endpoints` and `source`: "this id is routable and here is where"
/// is itself a fact, and every unknown stays absent inside it.
pub fn for_catalog(info: &ModelInfo, protocol: Protocol, upstream_name: &str) -> Derived {
    // §2.1: `task` drives `endpoints`. An embedding entry in a chat catalog is
    // reachable on `/v1/embeddings` and nowhere else, so the protocol only
    // decides *which* chat routes a chat model gets.
    let task = info.task.clone().unwrap_or_else(|| "chat".to_string());
    let endpoints = match task.as_str() {
        "embedding" => vec!["/v1/embeddings".to_string()],
        "rerank" => vec!["/v1/rerank".to_string()],
        // A cloud image model reaches the same two routes a local sd-server
        // row does (§5). The edits route is added only when the catalog says
        // the model takes an image *in* — for cloud there is no `edit` column
        // to read, and the provider's own modality list is the whole of what
        // is known.
        "image_generation" | "image_edit" => {
            let mut v = vec![IMAGE_GENERATIONS_ENDPOINT.to_string()];
            if info
                .input_modalities
                .as_ref()
                .is_some_and(|m| m.iter().any(|x| x == "image"))
            {
                v.push(IMAGE_EDITS_ENDPOINT.to_string());
            }
            v
        }
        _ => match protocol {
            Protocol::Openai => strings(&CHAT_ENDPOINTS_OPENAI),
            Protocol::Anthropic | Protocol::Gemini => strings(&CHAT_ENDPOINTS_OTHER),
        },
    };

    let vision = info
        .input_modalities
        .as_ref()
        .map(|m| m.iter().any(|x| x == "image"));

    let reasoning = info.reasoning.as_ref().map(|r| {
        let control = if r.kind == "fixed" {
            Vec::new()
        } else {
            match protocol {
                Protocol::Openai => strings(&OPENAI_REASONING_CONTROL),
                Protocol::Anthropic => strings(&ANTHROPIC_REASONING_CONTROL),
                Protocol::Gemini => strings(&GEMINI_REASONING_CONTROL),
            }
        };
        ReasoningCaps {
            kind: r.kind.clone(),
            enabled: r.enabled,
            levels: r.levels.clone(),
            // No cloud catalog states a default effort or a preserve-history
            // policy, and none publishes a default budget.
            default: None,
            can_disable: r.can_disable,
            budget_tokens: None,
            preserve_history: None,
            control,
        }
    });

    let tool_calls = info.tools.map(|t| {
        if t {
            ToolCallCaps {
                kind: "native".to_string(),
                // Whether the provider runs calls in parallel is not in any
                // catalog.
                parallel: None,
                format: Some("provider".to_string()),
            }
        } else {
            ToolCallCaps {
                kind: "none".to_string(),
                parallel: None,
                format: None,
            }
        }
    });

    let caps = ModelCapabilities {
        task,
        endpoints,
        input_modalities: info.input_modalities.clone(),
        output_modalities: info.output_modalities.clone(),
        vision,
        reasoning,
        tool_calls,
        structured_output: info
            .structured_output
            .as_ref()
            .map(|s| StructuredOutputCaps {
                json_schema: s.json_schema,
                json_object: s.json_object,
            }),
        source: "catalog".to_string(),
    };

    let notes = notes::notes_for_catalog(info, protocol, upstream_name, &caps);
    Derived {
        capabilities: Some(caps),
        max_output_tokens: info.max_output_tokens,
        notes,
        created: info.created,
    }
}

// ---------------------------------------------------------------------------
// Owner overrides (design §7)
// ---------------------------------------------------------------------------

/// Apply an owner's `capabilities_override` (design §7) over what a builder
/// above derived. `override_` is a JSON **object** with up to three optional
/// keys:
///
/// - `capabilities`: an object deep-merged over `derived.capabilities`
///   ([`deep_merge_objects`]) — a key holding an object recurses so sibling
///   keys of that nested object survive; an array or scalar replaces the
///   base value whole; `null` deletes the key. When `derived.capabilities`
///   is `None` (an unreadable GGUF), the override object becomes the whole
///   thing, and it must then carry at least `task` — there is nothing to
///   inherit it from. The merged object is deserialised back into
///   [`ModelCapabilities`], so a shape the schema does not accept (a bad
///   `reasoning.kind`, a missing `task`/`endpoints`/`source` after a `null`
///   deleted it) is an `Err` naming the problem rather than a silently
///   accepted partial object; on success `source` is set to `"owner"`.
/// - `max_output_tokens`: a non-negative integer, or `null` to clear it.
/// - `notes`: an array of strings, appended to `derived.notes` (never
///   replaces — the derived notes stay true, the owner is adding to them).
///
/// Any other top-level key, or a wrong shape for one of the three above, is
/// an `Err` naming it — this is the escape hatch the design calls "unknown
/// is better than guessed", so a malformed override must be loud rather than
/// silently dropped.
pub fn apply_owner_override(
    mut derived: Derived,
    override_: &serde_json::Value,
) -> Result<Derived, String> {
    let obj = override_
        .as_object()
        .ok_or_else(|| "capabilities_override must be a JSON object".to_string())?;

    for key in obj.keys() {
        if !matches!(key.as_str(), "capabilities" | "max_output_tokens" | "notes") {
            return Err(format!("capabilities_override: unknown key '{key}'"));
        }
    }

    if let Some(caps_override) = obj.get("capabilities") {
        let caps_override_obj = caps_override.as_object().ok_or_else(|| {
            "capabilities_override.capabilities must be a JSON object".to_string()
        })?;

        let mut merged = match &derived.capabilities {
            Some(existing) => match serde_json::to_value(existing) {
                Ok(serde_json::Value::Object(m)) => m,
                // `ModelCapabilities` always serialises to an object; this
                // arm is unreachable in practice.
                _ => serde_json::Map::new(),
            },
            None => serde_json::Map::new(),
        };
        deep_merge_objects(&mut merged, caps_override_obj);

        if derived.capabilities.is_none() && !merged.get("task").is_some_and(|v| v.is_string()) {
            return Err(
                "capabilities_override.capabilities must include \"task\" — this model's own \
                 capabilities could not be derived, so there is nothing to merge over"
                    .to_string(),
            );
        }

        let mut new_caps: ModelCapabilities =
            serde_json::from_value(serde_json::Value::Object(merged))
                .map_err(|e| format!("capabilities_override.capabilities: {e}"))?;
        validate_vocabulary(&new_caps)?;
        new_caps.source = "owner".to_string();
        derived.capabilities = Some(new_caps);
    }

    if let Some(mot) = obj.get("max_output_tokens") {
        derived.max_output_tokens = match mot {
            serde_json::Value::Null => None,
            serde_json::Value::Number(n) => Some(n.as_u64().ok_or_else(|| {
                "capabilities_override.max_output_tokens must be a non-negative integer".to_string()
            })?),
            _ => {
                return Err(
                    "capabilities_override.max_output_tokens must be a number or null".to_string(),
                )
            }
        };
        // `source` describes the capabilities object; a hand-set cap lives
        // outside it and would otherwise be indistinguishable from one read
        // off the model. Only a *set* value is attributed — clearing one
        // leaves nothing to attribute.
        if derived.max_output_tokens.is_some() {
            derived.notes.push(
                "max_output_tokens was set by the owner (capabilities_override), not read from \
                 the model."
                    .to_string(),
            );
        }
    }

    if let Some(notes) = obj.get("notes") {
        let arr = notes
            .as_array()
            .ok_or_else(|| "capabilities_override.notes must be an array of strings".to_string())?;
        for n in arr {
            let s = n.as_str().ok_or_else(|| {
                "capabilities_override.notes must be an array of strings".to_string()
            })?;
            derived.notes.push(s.to_string());
        }
    }

    Ok(derived)
}

/// Every `task` this schema knows (design §2.1): the routable classes plus
/// audio.cpp's own task names, which are the ids `/v1/tasks/run` accepts, plus
/// the image class's three (image-generation design §5).
const KNOWN_TASKS: [&str; 20] = [
    "chat",
    "embedding",
    "rerank",
    "tts",
    "asr",
    "image_generation",
    "image_edit",
    "video_generation",
    "gen",
    "clon",
    "vc",
    "svc",
    "s2s",
    "sep",
    "vad",
    "diar",
    "align",
    "vdes",
    "spk",
    "midi",
];

/// Every modality name this schema knows (design §2.1, plus `pdf`, which the
/// Kilo/OpenRouter catalogs publish and lmgw passes through verbatim).
const KNOWN_MODALITIES: [&str; 7] = [
    "text",
    "image",
    "audio",
    "video",
    "file",
    "pdf",
    "embedding",
];

const KNOWN_REASONING_KINDS: [&str; 3] = ["fixed", "toggle", "levels"];
const KNOWN_TOOL_CALL_KINDS: [&str; 3] = ["native", "text", "none"];

/// Vocabulary check for the **merged** capabilities object (design §7).
///
/// `deny_unknown_fields` catches a misspelled *key*; this catches a misspelled
/// *value*, which is the more damaging half: a consumer switches on
/// `reasoning.kind` and `tool_calls.kind`, so `"levelz"` silently disables a
/// feature on the client side rather than erroring anywhere. Every message
/// names the offending key and what it may hold.
fn validate_vocabulary(c: &ModelCapabilities) -> Result<(), String> {
    let one_of = |vals: &[&str]| vals.join(", ");
    if !KNOWN_TASKS.contains(&c.task.as_str()) {
        return Err(format!(
            "capabilities_override.capabilities.task: '{}' is not a known task (one of: {})",
            c.task,
            one_of(&KNOWN_TASKS)
        ));
    }
    for (key, list) in [
        ("input_modalities", &c.input_modalities),
        ("output_modalities", &c.output_modalities),
    ] {
        for m in list.iter().flatten() {
            if !KNOWN_MODALITIES.contains(&m.as_str()) {
                return Err(format!(
                    "capabilities_override.capabilities.{key}: '{m}' is not a known modality \
                     (one of: {})",
                    one_of(&KNOWN_MODALITIES)
                ));
            }
        }
    }
    if let Some(r) = &c.reasoning {
        if !KNOWN_REASONING_KINDS.contains(&r.kind.as_str()) {
            return Err(format!(
                "capabilities_override.capabilities.reasoning.kind: '{}' is not one of: {}",
                r.kind,
                one_of(&KNOWN_REASONING_KINDS)
            ));
        }
    }
    if let Some(t) = &c.tool_calls {
        if !KNOWN_TOOL_CALL_KINDS.contains(&t.kind.as_str()) {
            return Err(format!(
                "capabilities_override.capabilities.tool_calls.kind: '{}' is not one of: {}",
                t.kind,
                one_of(&KNOWN_TOOL_CALL_KINDS)
            ));
        }
    }
    Ok(())
}

/// Recursive JSON object merge for [`apply_owner_override`]: an overlay key
/// holding an object recurses into the base's object at that key (inserting
/// fresh if the base has none there), so sibling keys neither side mentions
/// survive; any other overlay value (array, string, number, bool) replaces
/// the base value whole; `null` deletes the key from the base. Only ever
/// walks object-vs-object — a scalar/array overlay value never recurses, it
/// replaces.
fn deep_merge_objects(
    base: &mut serde_json::Map<String, serde_json::Value>,
    overlay: &serde_json::Map<String, serde_json::Value>,
) {
    for (k, v) in overlay {
        match v {
            serde_json::Value::Null => {
                base.remove(k);
            }
            serde_json::Value::Object(overlay_obj) => match base.get_mut(k) {
                Some(serde_json::Value::Object(base_obj)) => {
                    deep_merge_objects(base_obj, overlay_obj);
                }
                _ => {
                    base.insert(k.clone(), serde_json::Value::Object(overlay_obj.clone()));
                }
            },
            other => {
                base.insert(k.clone(), other.clone());
            }
        }
    }
}
