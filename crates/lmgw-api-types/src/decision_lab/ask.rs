//! What the Decision lab posts (labs-store design §2.5, §2.6): an Ask, which
//! stores a group and its plan, and a count for the token gauge.

use serde::{Deserialize, Serialize};

use super::form::{DecisionForm, Variant};
use super::models::DecisionLimit;
use crate::lab_store::LabCell;

/// Why Perturb is off while an override is in effect: rewriting an override
/// would mean parsing it into a typed request, which drops unknown fields.
pub const PERTURB_WITH_OVERRIDE: &str =
    "perturbations rewrite the form; drop the override to use them";

/// `POST /decision-lab/api/groups`: the form, the models to ask, the
/// perturbations to add, and the override when one is in effect.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionAsk {
    pub form: DecisionForm,
    /// The head model first, then the compared ones.
    pub models: Vec<String>,
    /// The checked perturbations; every model is also asked as written,
    /// first.
    pub variants: Vec<Variant>,
    /// The request override, as typed: sent instead of the form, with only
    /// `model` rewritten per model.
    #[serde(rename = "override")]
    pub override_text: Option<String>,
}

impl DecisionAsk {
    /// The checked perturbations in the order given, `as_written` and
    /// repeats left out.
    pub fn perturbations(&self) -> Vec<Variant> {
        let mut out: Vec<Variant> = Vec::new();
        for v in &self.variants {
            if *v != Variant::AsWritten && !out.contains(v) {
                out.push(*v);
            }
        }
        out
    }

    /// The cells: the models × (as written, then each perturbation), models
    /// outer. `Err` names what makes it no plan: no model, a blank one, or a
    /// perturbation with an override.
    pub fn plan(&self) -> Result<Vec<LabCell>, String> {
        if self.models.is_empty() {
            return Err("models: pick a model to ask".into());
        }
        if let Some(i) = self.models.iter().position(|m| m.trim().is_empty()) {
            return Err(format!("models: model {} has no name", i + 1));
        }
        let perturb = self.perturbations();
        if self.override_text.is_some() && !perturb.is_empty() {
            return Err(PERTURB_WITH_OVERRIDE.into());
        }
        let variants: Vec<Variant> = std::iter::once(Variant::AsWritten).chain(perturb).collect();
        Ok(self
            .models
            .iter()
            .flat_map(|m| {
                variants.iter().map(move |v| LabCell {
                    model: m.clone(),
                    variant: v.as_str().to_string(),
                })
            })
            .collect())
    }
}

/// `POST /decision-lab/api/count`: count the form's pieces on `model`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionCount {
    pub model: String,
    pub form: DecisionForm,
    /// Count even when that starts a container: a local model the count
    /// would hit (after the hold's swap) that is not ready is otherwise not
    /// counted (`not_counted` says so).
    pub load: bool,
}

/// One question's tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct QuestionTokens {
    pub id: String,
    /// Its instructions and its options, levels or criteria.
    pub tokens: u64,
    /// A `choice` question's options alone; `null` for the other types.
    pub option_span: Option<u64>,
}

/// One limit with the form's estimate against it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionEstimate {
    pub limit: DecisionLimit,
    /// The state plus the largest question, or the largest option span.
    pub tokens: u64,
    /// The estimate is past the limit. A warning: Ask is not blocked.
    pub over: bool,
}

/// What a count found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionCounted {
    pub model: String,
    /// Why nothing was counted ("decide/kev-4b is not loaded", or the
    /// count's own failure); everything below but `answered_by` is empty
    /// then.
    pub not_counted: Option<String>,
    /// The state's tokens.
    pub state: Option<u64>,
    pub questions: Vec<QuestionTokens>,
    /// Why the numbers are approximate: always `flattened` (llama.cpp's
    /// template wraps the pieces and is not counted), `media_omitted` with
    /// images, `tokenizer_guess` on a cloud model without a counter of its
    /// own.
    pub approx: Vec<String>,
    pub estimates: Vec<DecisionEstimate>,
    /// The fallback that counted instead (`x-lmgw-fallback`).
    pub answered_by: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_is_models_outer_as_written_first() {
        let ask = DecisionAsk {
            models: vec!["decide/laya".into(), "decide/kev-4b".into()],
            variants: vec![
                Variant::Reversed,
                Variant::AsWritten,
                Variant::NoDescriptions,
            ],
            ..Default::default()
        };
        let plan: Vec<(String, String)> = ask
            .plan()
            .unwrap()
            .into_iter()
            .map(|c| (c.model, c.variant))
            .collect();
        let want = [
            ("decide/laya", "as_written"),
            ("decide/laya", "reversed"),
            ("decide/laya", "no_descriptions"),
            ("decide/kev-4b", "as_written"),
            ("decide/kev-4b", "reversed"),
            ("decide/kev-4b", "no_descriptions"),
        ];
        assert_eq!(
            plan,
            want.map(|(m, v)| (m.to_string(), v.to_string())).to_vec()
        );
    }

    #[test]
    fn no_model_or_a_perturbed_override_is_no_plan() {
        assert!(DecisionAsk::default().plan().is_err());
        let ask = DecisionAsk {
            models: vec!["decide/laya".into()],
            variants: vec![Variant::Reversed],
            override_text: Some("{}".into()),
            ..Default::default()
        };
        assert_eq!(ask.plan().unwrap_err(), PERTURB_WITH_OVERRIDE);
        let ask = DecisionAsk {
            variants: vec![Variant::AsWritten],
            ..ask
        };
        assert_eq!(ask.plan().unwrap().len(), 1);
    }
}
