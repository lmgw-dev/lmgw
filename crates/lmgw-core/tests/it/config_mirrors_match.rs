//! The API mirrors of the config row types against the rows themselves
//! (`ops::local_model::mirror` refuses a row its mirror cannot hold whole;
//! this fails before any row is read): the same keys, in both directions.

use lmgw_api_types as dto;
use lmgw_core::config::LlamaParams;
use serde_json::Value;

fn keys<T: serde::Serialize + Default>() -> Vec<String> {
    let Value::Object(m) = serde_json::to_value(T::default()).unwrap() else {
        panic!("not an object");
    };
    m.keys().cloned().collect()
}

#[test]
fn config_mirrors_match() {
    assert_eq!(
        keys::<LlamaParams>(),
        keys::<dto::LlamaParams>(),
        "config::LlamaParams and its API mirror have different fields"
    );
    // A row written by the core type reads into the mirror and back whole.
    let core = serde_json::to_value(LlamaParams::default()).unwrap();
    let mirrored: dto::LlamaParams = serde_json::from_value(core.clone()).unwrap();
    assert_eq!(serde_json::to_value(&mirrored).unwrap(), core);
}
