//! What a System One request must fit on a model (labs-store design §2.2,
//! restating decision-models §3.4 and §1.4): one or two token limits per
//! header shape, each with what it bounds and the rule in one sentence.
//! Pure: the facts come in, the limits go out. No per-type option limit:
//! that is llama.cpp's to name in its 400 (labs-store Decision 24).

use lmgw_api_types::decision_lab::{DecisionLimit, DecisionMeasure, DecisionShape};

use super::Shape;

impl From<Shape> for DecisionShape {
    fn from(s: Shape) -> Self {
        match s {
            Shape::NonCausal => Self::NonCausal,
            Shape::CausalHead => Self::CausalHead,
            Shape::LabelLogits => Self::LabelLogits,
        }
    }
}

/// A local decision row's facts the limits are read from.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LocalFacts<'a> {
    /// How a rule names the model: its decision type, else its public name.
    pub who: &'a str,
    pub shape: Shape,
    /// The ubatch in effect, and where it comes from ("set on the row").
    pub ubatch: u64,
    pub ubatch_from: &'a str,
    /// One slot's context from the running server's `/props`.
    pub n_ctx_slot: Option<u64>,
    /// The row's `ctx_size`.
    pub ctx_size: Option<u64>,
}

/// The context a causal type's state and question must fit: `n_ctx_slot`,
/// else `ctx_size`.
fn context(f: &LocalFacts<'_>) -> DecisionLimit {
    let (tokens, of) = match (f.n_ctx_slot, f.ctx_size) {
        (Some(n), _) => (
            Some(n),
            format!("one slot's context as the running server's /props says ({n})"),
        ),
        (None, Some(n)) => (Some(n), format!("the row's ctx_size ({n})")),
        (None, None) => (
            None,
            "unknown: the row sets no ctx_size and no container has run yet".to_string(),
        ),
    };
    let rule = match tokens {
        Some(n) => format!(
            "{} reads the state and the question in one slot's context ({n}); llama.cpp \
             refuses a longer prompt",
            f.who
        ),
        None => format!(
            "{} reads the state and the question in one slot's context; llama.cpp refuses a \
             longer prompt",
            f.who
        ),
    };
    DecisionLimit {
        applies_to: DecisionMeasure::StateAndQuestion,
        tokens,
        of,
        rule,
    }
}

/// The limits of a local decision row (§2.2's table).
pub(crate) fn local(f: &LocalFacts<'_>) -> Vec<DecisionLimit> {
    let ub = f.ubatch;
    match f.shape {
        Shape::NonCausal => vec![DecisionLimit {
            applies_to: DecisionMeasure::StateAndQuestion,
            tokens: Some(ub),
            of: format!("the ubatch in effect ({ub}, {})", f.ubatch_from),
            rule: format!(
                "{} evaluates the whole prompt (the state, one question with its options, and \
                 the template) in one ubatch ({ub}); llama.cpp answers 500 past it",
                f.who
            ),
        }],
        Shape::CausalHead => vec![
            DecisionLimit {
                applies_to: DecisionMeasure::OptionSpan,
                tokens: Some(ub),
                of: format!(
                    "the ubatch in effect ({ub}, {}), which is the batch in embedding mode",
                    f.ubatch_from
                ),
                rule: format!(
                    "{} needs the span from the first option marker to the end of the prompt \
                     in one batch ({ub}); llama.cpp answers 400 naming the size past it",
                    f.who
                ),
            },
            context(f),
        ],
        Shape::LabelLogits => vec![context(f)],
    }
}

/// The limit of a cloud model: its catalog's context.
pub(crate) fn cloud(name: &str, context_length: Option<u64>) -> Vec<DecisionLimit> {
    let (of, rule) = match context_length {
        Some(n) => (
            format!("the catalog's context_length ({n})"),
            format!(
                "{name} takes a request that fits its context ({n}); TypeSafe documents 32k \
                 for the state and the longest question"
            ),
        ),
        None => (
            "unknown: the catalog states no context length".to_string(),
            format!(
                "{name} takes a request that fits its context; TypeSafe documents 32k for the \
                 state and the longest question"
            ),
        ),
    };
    vec![DecisionLimit {
        applies_to: DecisionMeasure::StateAndQuestion,
        tokens: context_length,
        of,
        rule,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(shape: Shape) -> LocalFacts<'static> {
        LocalFacts {
            who: "laya",
            shape,
            ubatch: 8192,
            ubatch_from: "set on the row",
            n_ctx_slot: None,
            ctx_size: Some(32768),
        }
    }

    #[test]
    fn a_non_causal_model_has_one_limit_the_ubatch() {
        let l = local(&facts(Shape::NonCausal));
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].applies_to, DecisionMeasure::StateAndQuestion);
        assert_eq!(l[0].tokens, Some(8192));
        assert_eq!(l[0].of, "the ubatch in effect (8192, set on the row)");
        assert!(l[0].rule.contains("one ubatch (8192)") && l[0].rule.contains("500"));
    }

    #[test]
    fn a_causal_model_with_a_head_has_the_option_span_and_the_context() {
        let mut f = facts(Shape::CausalHead);
        f.ubatch = 2048;
        f.n_ctx_slot = Some(16384);
        let l = local(&f);
        assert_eq!(
            l.iter()
                .map(|l| (l.applies_to, l.tokens))
                .collect::<Vec<_>>(),
            [
                (DecisionMeasure::OptionSpan, Some(2048)),
                (DecisionMeasure::StateAndQuestion, Some(16384)),
            ],
            "n_ctx_slot before ctx_size"
        );
        assert!(l[0].rule.contains("400"));
        assert!(l[1].of.contains("/props"));
    }

    #[test]
    fn a_label_logits_model_has_the_context_known_or_not() {
        let l = local(&facts(Shape::LabelLogits));
        assert_eq!(
            (l.len(), l[0].tokens, l[0].of.as_str()),
            (1, Some(32768), "the row's ctx_size (32768)")
        );
        let mut f = facts(Shape::LabelLogits);
        f.ctx_size = None;
        let l = local(&f);
        assert_eq!(l[0].tokens, None);
        assert!(l[0].of.starts_with("unknown"));
    }

    #[test]
    fn a_cloud_model_has_its_catalogs_context() {
        let l = cloud("jev", Some(32768));
        assert_eq!(
            (l[0].applies_to, l[0].tokens),
            (DecisionMeasure::StateAndQuestion, Some(32768))
        );
        assert!(l[0].rule.contains("32k"));
        assert_eq!(cloud("jev", None)[0].tokens, None);
    }
}
