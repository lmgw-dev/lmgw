//! An output's `decide` (labs-store design §4): `config.<field>`, the config
//! field holding a Decision lab question set's id (format
//! `lmgw-question-set`, an integer). With a set, the classify step asks the
//! set's question whose id is the output's `field` instead of the model
//! call; without one, the step is the model call as before. Only the
//! `enum_from` form decides: a `choice` question answers one field.

use serde_json::Value;

use super::{lookup_path, Field, Format, ItemOutput};

impl ItemOutput {
    /// The config field `decide` names, without `config.`.
    pub fn decide_field(&self) -> Option<&str> {
        self.decide.as_deref()?.strip_prefix("config.")
    }

    /// The question set `config` names for this output: its id, or `None`
    /// when the output classifies with the model call (no `decide`, or the
    /// field empty).
    pub fn decide_set(&self, config: &Value) -> Option<i64> {
        lookup_path(config, self.decide.as_deref()?)?.as_i64()
    }
}

/// `decide`'s path against the config fields: a `config.<field>` naming a
/// field of format `lmgw-question-set`.
pub(super) fn check(path: &str, names: &[String], fields: &[Field], errors: &mut Vec<String>) {
    let at = "run.item.output.decide";
    let Some(name) = path.strip_prefix("config.") else {
        errors.push(format!(
            "{at} is '{path}'; it must name a config field as config.<field>"
        ));
        return;
    };
    match fields.iter().find(|f| f.name == name) {
        None => errors.push(format!(
            "{at} names no config field ({})",
            if names.is_empty() {
                "this agent declares no config schema".to_string()
            } else {
                format!("fields: {}", names.join(", "))
            }
        )),
        Some(f) if f.format != Some(Format::QuestionSet) => errors.push(format!(
            "{at} names '{name}', which holds no question set: give it type integer and format \
             {}",
            Format::QuestionSet.as_str()
        )),
        Some(_) => {}
    }
}
