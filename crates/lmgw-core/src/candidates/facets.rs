//! The facet vocabulary (candidate-aliases design §4.6, fact 13): the five
//! capability toggles a candidate alias can enable or disable, a `Copy`
//! bitset over them, and the positive-support rule.

use crate::capabilities::ModelCapabilities;

/// The five capability toggles a candidate alias can enable or disable
/// (§4.6, fact 13). Wire names are what `capabilities_disabled`/
/// `capabilities_enabled` store on disk and what the MCP tool and the editor
/// speak. `#[repr(u8)]` with explicit discriminants because [`FacetSet`]
/// indexes a bit per variant by it — reordering this list would silently
/// remap old bits to new meanings in a stored `FacetSet`, so nothing besides
/// appending is safe without a migration of its own (there is none stored
/// this way today; only the wire-name strings are persisted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Facet {
    Vision = 0,
    Audio = 1,
    ToolCalls = 2,
    Reasoning = 3,
    StructuredOutput = 4,
}

impl Facet {
    /// Every facet, in the order the editor, `/v1/models` and every error
    /// message list them.
    pub const ALL: [Facet; 5] = [
        Facet::Vision,
        Facet::Audio,
        Facet::ToolCalls,
        Facet::Reasoning,
        Facet::StructuredOutput,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Facet::Vision => "vision",
            Facet::Audio => "audio",
            Facet::ToolCalls => "tool_calls",
            Facet::Reasoning => "reasoning",
            Facet::StructuredOutput => "structured_output",
        }
    }

    /// A human label for the editor's toggle row — `as_str` is the wire name,
    /// this is the sentence fragment next to the checkbox.
    pub fn label(self) -> &'static str {
        match self {
            Facet::Vision => "Vision (image input)",
            Facet::Audio => "Audio input",
            Facet::ToolCalls => "Tool calls",
            Facet::Reasoning => "Reasoning",
            Facet::StructuredOutput => "Structured output",
        }
    }

    /// Parse a wire name, or an `Err` naming every valid one — used at save
    /// time (`ops::candidate_alias`) so a typo in `capabilities_disabled` or
    /// an MCP argument is refused by name rather than silently ignored or
    /// stored unrecognisable.
    pub fn parse(s: &str) -> Result<Facet, String> {
        Facet::ALL
            .iter()
            .copied()
            .find(|f| f.as_str() == s)
            .ok_or_else(|| {
                format!(
                    "'{s}' is not a known capability facet (one of: {})",
                    Facet::ALL
                        .iter()
                        .map(|f| f.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }
}

/// A `Copy` bitset of [`Facet`]s — the enabled set, the common set, a
/// candidate's own support, all the same small shape, cheap enough to pass
/// by value on the request path (the whole point of [`super::CandidatePick`]
/// costing no per-request I/O).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FacetSet(u8);

impl FacetSet {
    pub const EMPTY: FacetSet = FacetSet(0);

    /// Every facet at once — the complement basis for "disabled" when the
    /// caller states no opinion (`ops::candidate_alias`'s save-time default:
    /// disable everything that turns out not to be common).
    pub fn everything() -> FacetSet {
        Facet::ALL.iter().fold(FacetSet::EMPTY, |s, f| s.insert(*f))
    }

    fn bit(f: Facet) -> u8 {
        1 << (f as u8)
    }

    pub fn contains(self, f: Facet) -> bool {
        self.0 & Self::bit(f) != 0
    }

    #[must_use]
    pub fn insert(self, f: Facet) -> Self {
        FacetSet(self.0 | Self::bit(f))
    }

    /// Every facet in `self` that is not in `other`.
    #[must_use]
    pub fn minus(self, other: FacetSet) -> FacetSet {
        FacetSet(self.0 & !other.0)
    }

    #[must_use]
    pub fn union(self, other: FacetSet) -> FacetSet {
        FacetSet(self.0 | other.0)
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = Facet> {
        Facet::ALL.into_iter().filter(move |f| self.contains(*f))
    }

    /// Every name in `names` (§4.1's `capabilities_disabled`/
    /// `capabilities_enabled` JSON arrays), or the first parse error.
    pub fn from_names<S: AsRef<str>>(
        names: impl IntoIterator<Item = S>,
    ) -> Result<FacetSet, String> {
        let mut out = FacetSet::EMPTY;
        for n in names {
            out = out.insert(Facet::parse(n.as_ref())?);
        }
        Ok(out)
    }

    pub fn names(self) -> Vec<String> {
        self.iter().map(|f| f.as_str().to_string()).collect()
    }
}

/// Whether `caps` supports `facet`, **positively** (§4.6): absent or a
/// negative published fact both count as unsupported.
///
/// - `vision`: `capabilities.vision == Some(true)`.
/// - `audio`: `input_modalities` contains `"audio"`.
/// - `tool_calls`: `tool_calls.kind == "native"` — `"text"` is a template
///   that renders tool syntax lmgw has no parser for, so calls may come back
///   as prose in `content` (`capabilities::mod`'s own `local_tool_calls` doc
///   comment); that is not tool-call support an API client can rely on, so
///   it does not count here either.
/// - `reasoning`: `reasoning.enabled == Some(true)` — **not** `reasoning.
///   is_some()` as the design's own first draft of this rule read (§12's
///   entry on this correction). Every local chat row's builder
///   (`capabilities::for_local_row` → `local_reasoning`) publishes a
///   `reasoning` object unconditionally, even a model with no thinking
///   markers at all publishes `Some(ReasoningCaps { kind: "fixed", enabled:
///   Some(false), .. })` — and candidates are local chat rows exclusively
///   (§4.1), so `is_some()` would make this facet trivially "common" and
///   "supported" on every candidate alias ever saved, never excluding the
///   plain non-reasoning models it exists to distinguish. The positive facts
///   are: a request can switch thinking on (`kind` is `toggle` or `levels`,
///   whatever the row's default — a thinking model configured off still
///   reasons when asked), or it thinks on its own terms (`fixed` with
///   `enabled == Some(true)`). `fixed` + `enabled: false` is the "no
///   thinking markers" shape and counts as unsupported. A cloud row's
///   `reasoning` stays `None`
///   (absent) when its catalog says nothing, so this rule is exactly
///   "absent or a stated negative both count as unsupported" for that class
///   too — no special case needed.
/// - `structured_output`: `structured_output.json_schema == Some(true)`.
///   `json_object` alone is not enough: a client asking for a JSON Schema
///   response format needs the stronger guarantee this names.
pub fn supports(caps: &ModelCapabilities, facet: Facet) -> bool {
    match facet {
        Facet::Vision => caps.vision == Some(true),
        Facet::Audio => caps
            .input_modalities
            .as_ref()
            .is_some_and(|m| m.iter().any(|x| x == "audio")),
        Facet::ToolCalls => caps.tool_calls.as_ref().is_some_and(|t| t.kind == "native"),
        Facet::Reasoning => caps
            .reasoning
            .as_ref()
            .is_some_and(|r| r.kind != "fixed" || r.enabled == Some(true)),
        Facet::StructuredOutput => caps
            .structured_output
            .as_ref()
            .is_some_and(|s| s.json_schema == Some(true)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{ReasoningCaps, StructuredOutputCaps, ToolCallCaps};

    /// `reasoning` is the *enabled* state, not presence: `None` means no
    /// `reasoning` object at all (a cloud row whose catalog said nothing),
    /// `Some(false)` means a local row's "publishes unconditionally, thinks
    /// nothing" shape (`kind: "fixed", enabled: Some(false)`), `Some(true)`
    /// means it reasons by default — the one positive case.
    fn caps(
        vision: Option<bool>,
        modalities: Option<Vec<&str>>,
        tool_kind: Option<&str>,
        reasoning: Option<bool>,
        json_schema: Option<bool>,
    ) -> ModelCapabilities {
        ModelCapabilities {
            task: "chat".to_string(),
            endpoints: vec![],
            input_modalities: modalities.map(|m| m.into_iter().map(String::from).collect()),
            output_modalities: None,
            vision,
            reasoning: reasoning.map(|enabled| ReasoningCaps {
                kind: "fixed".to_string(),
                enabled: Some(enabled),
                ..Default::default()
            }),
            tool_calls: tool_kind.map(|k| ToolCallCaps {
                kind: k.to_string(),
                parallel: None,
                format: None,
            }),
            structured_output: json_schema.map(|j| StructuredOutputCaps {
                json_schema: Some(j),
                json_object: None,
            }),
            source: "gguf+config".to_string(),
        }
    }

    #[test]
    fn facet_names_round_trip() {
        for f in Facet::ALL {
            assert_eq!(Facet::parse(f.as_str()).unwrap(), f);
        }
        assert!(Facet::parse("nope").is_err());
    }

    #[test]
    fn support_is_positive_only() {
        let none = caps(None, None, None, None, None);
        for f in Facet::ALL {
            assert!(!supports(&none, f), "{f:?} must be unsupported when absent");
        }
        let all = caps(
            Some(true),
            Some(vec!["text", "image", "audio"]),
            Some("native"),
            Some(true),
            Some(true),
        );
        for f in Facet::ALL {
            assert!(supports(&all, f), "{f:?} should be supported");
        }
        // Negative/other-than-native facts still count as unsupported —
        // including a *present* `reasoning` object whose `enabled` is
        // `false` (every local chat row's shape when it has no thinking
        // markers at all, `capabilities::for_local_row`'s own
        // `local_reasoning`): presence alone must not read as support.
        let negatives = caps(
            Some(false),
            Some(vec!["text"]),
            Some("text"),
            Some(false),
            Some(false),
        );
        for f in Facet::ALL {
            assert!(
                !supports(&negatives, f),
                "{f:?} must reject a negative fact"
            );
        }
        // A thinking model configured off still reasons when a request asks:
        // `toggle`/`levels` count whatever the default.
        let mut off = negatives.clone();
        off.reasoning = Some(ReasoningCaps {
            kind: "toggle".to_string(),
            enabled: Some(false),
            ..Default::default()
        });
        assert!(supports(&off, Facet::Reasoning));
    }

    #[test]
    fn facet_set_bit_ops() {
        let s = FacetSet::from_names(["vision", "reasoning"]).unwrap();
        assert!(s.contains(Facet::Vision));
        assert!(s.contains(Facet::Reasoning));
        assert!(!s.contains(Facet::Audio));
        let minus = s.minus(FacetSet::from_names(["vision"]).unwrap());
        assert!(!minus.contains(Facet::Vision));
        assert!(minus.contains(Facet::Reasoning));
        assert_eq!(minus.names(), vec!["reasoning".to_string()]);
        assert!(FacetSet::from_names(["nope"]).is_err());
    }
}
