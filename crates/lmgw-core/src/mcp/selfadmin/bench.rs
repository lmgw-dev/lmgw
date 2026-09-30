//! `lmgw__bench_*` dispatch (benchmark design §8.1): the flat tool
//! arguments into the ops' typed structs — the same structs the dashboard
//! sends, so a misspelt argument is refused by name (`deny_unknown_fields`).
//! Two differences from the ops: `phases` is a comma-separated string, and
//! `lmgw__bench_run` leaves the timeline out unless asked.

use lmgw_api_types::bench::Phase;
use serde_json::{Map, Value};

use crate::ops::{self, bench};
use crate::state::SharedState;

pub(super) async fn run(
    state: &SharedState,
    name: &str,
    args: Option<Map<String, Value>>,
) -> Result<Value, String> {
    let mut a = args.unwrap_or_default();
    match name {
        "lmgw__bench_plan" => {
            phases_to_array(&mut a)?;
            ops::backends::to_json(bench::bench_plan(state, ops::patch_from_args(Some(a))?).await)
        }
        "lmgw__bench_start" => {
            phases_to_array(&mut a)?;
            let started = bench::bench_start(state, ops::patch_from_args(Some(a))?).await?;
            let mut v = ops::backends::to_json(Ok(started.clone()))?;
            v["next_step"] = bench::start_next_step(&started);
            Ok(v)
        }
        "lmgw__bench_runs" => {
            ops::backends::to_json(bench::bench_runs(state, ops::patch_from_args(Some(a))?).await)
        }
        "lmgw__bench_run" => {
            a.entry("timeline").or_insert(Value::Bool(false));
            ops::backends::to_json(bench::bench_run(state, ops::patch_from_args(Some(a))?).await)
        }
        "lmgw__bench_cancel" => {
            ops::backends::to_json(bench::bench_cancel(state, ops::patch_from_args(Some(a))?).await)
        }
        "lmgw__bench_delete" => {
            ops::backends::to_json(bench::bench_delete(state, ops::patch_from_args(Some(a))?).await)
        }
        other => Err(format!("unhandled built-in tool '{other}'")),
    }
}

/// `phases: "probes, decode"` → the op's array. An unknown name is refused,
/// naming the phases there are.
fn phases_to_array(a: &mut Map<String, Value>) -> Result<(), String> {
    let Some(v) = a.remove("phases") else {
        return Ok(());
    };
    let csv = match &v {
        Value::String(s) => s.clone(),
        Value::Null => return Ok(()),
        _ => return Err("argument 'phases' must be a comma-separated string".into()),
    };
    let phases = Phase::parse_list(&csv)?;
    a.insert(
        "phases".into(),
        serde_json::to_value(phases).map_err(|e| e.to_string())?,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_become_the_ops_array() {
        let mut a: Map<String, Value> =
            serde_json::from_str(r#"{"model_id":"m","phases":"decode, probes"}"#).unwrap();
        phases_to_array(&mut a).unwrap();
        assert_eq!(a["phases"], serde_json::json!(["load", "probes", "decode"]));
        let mut bad: Map<String, Value> = serde_json::from_str(r#"{"phases":"prefil"}"#).unwrap();
        assert!(phases_to_array(&mut bad).unwrap_err().contains("prefil"));
        let mut none: Map<String, Value> = serde_json::from_str(r#"{"model_id":"m"}"#).unwrap();
        phases_to_array(&mut none).unwrap();
        assert!(!none.contains_key("phases"));
    }
}
