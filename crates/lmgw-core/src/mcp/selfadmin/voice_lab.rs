//! `lmgw__voice_generate` and `lmgw__voice_sample_save` dispatch: the flat
//! tool arguments are the ops' own argument types field for field, so a
//! misspelt one is refused by name (`deny_unknown_fields`). The tools
//! themselves are `catalog/voice_lab.rs`.

use serde_json::{Map, Value};

use lmgw_api_types::WithNextStep;

use crate::ops;
use crate::state::SharedState;

pub(super) async fn run(
    state: &SharedState,
    name: &str,
    args: Option<Map<String, Value>>,
) -> Result<Value, String> {
    match name {
        "lmgw__voice_generate" => {
            let round = ops::voice_generate(state, ops::patch_from_args(args)?).await?;
            let next_step = format!(
                "the owner listens in the dashboard's Voices lab (round {}); \
                 lmgw__voice_sample_save round={} candidate=<n> name=<name> saves the chosen \
                 one to the voice library with its transcript",
                round.id, round.id
            );
            ops::backends::to_json(Ok(WithNextStep::new(round, next_step)))
        }
        "lmgw__voice_sample_save" => {
            ops::backends::to_json(ops::voice_sample_save(state, ops::patch_from_args(args)?).await)
        }
        other => Err(format!("unhandled built-in tool '{other}'")),
    }
}
