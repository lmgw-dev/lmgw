//! The op dispatcher's vocabulary, enumerated (api-docs design §4.7).
//!
//! `POST /api/op/{name}` used to accept anything [`super::api::op`]'s `match`
//! or its two sibling dispatchers (`api_agents::op`, `api_settings::key_op`)
//! happened to have an arm for, with everything else falling through to a
//! flat `op_failed "unknown op"`. That made the vocabulary an emergent
//! property of three `match` statements rather than something a doc — or a
//! test — could read.
//!
//! These four lists are that vocabulary, made explicit: `tests/it`'s
//! `op_names_match_the_dispatcher_arms` scans the three dispatchers' source
//! and asserts the arms equal these lists, both ways, so the two cannot
//! drift. [`is_op`] is the gate `op()` checks *before* it looks at its own
//! arms (§4.7): a name not listed here is unreachable, and adding an op means
//! adding it here, which is what forces it into the docs.

/// The 57 arms of [`super::api::op`]'s own `match` — everything dispatched
/// there directly, including the legacy `embed_model_set` spelling.
pub const MAIN_OPS: &[&str] = &[
    "upstream_set",
    "model_set",
    "local_model_set",
    "candidate_alias_set",
    "mcp_server_set",
    "settings_set",
    "aux_model_set",
    "embed_model_set",
    "audio_model_set",
    "image_model_set",
    "model_visibility",
    "alias_set",
    "upstream_set_full",
    "price_set",
    "price_delete",
    "prices_sync",
    "tool_set",
    "update_check",
    "realtime_budget",
    "job_cancel",
    "response_chain_delete",
    "responses_gc",
    "upstream_test",
    "hf_add",
    "hf_set",
    "container",
    "hold_set",
    "audio_catalog",
    "voice_transcribe",
    "image_recipes",
    "image_recipe_add",
    "local_model_test",
    "builds",
    "build_get",
    "build_set",
    "build_resolve",
    "build_check_merge",
    "build_run",
    "build_run_log",
    "build_promote",
    "build_verify",
    "build_env",
    "forge_refs",
    "forge_prs",
    "forge_pr",
    "container_images",
    "container_image_delete",
    "container_image_tag",
    "build_updates_check",
    "container_image_pull",
    "container_image_pull_status",
    "bench_plan",
    "bench_start",
    "bench_runs",
    "bench_run",
    "bench_cancel",
    "bench_run_set",
    "bench_delete",
];

/// The agent catalog's ops (agent-catalog design §5), dispatched to
/// `super::api_agents::op` — 19, `agents_restore` included.
pub const AGENT_OPS: &[&str] = &[
    "agent_set",
    "agent_duplicate",
    "agent_config_set",
    "agent_install",
    "agent_pull",
    "agent_reimport",
    "agent_dev_url_set",
    "agent_enable",
    "agent_delete",
    "agent_reset",
    "agent_open_chat",
    "agent_run",
    "agent_run_cancel",
    "agent_token_get",
    "agent_token_rotate",
    "agent_service_start",
    "agent_service_stop",
    "agent_service_log",
    "agents_restore",
];

/// The credential ops (principals design §3.12), dispatched to
/// `super::api_settings::key_op`.
pub const KEY_OPS: &[&str] = &[
    "key_create",
    "key_set",
    "key_delete",
    "key_reveal",
    "key_rotate",
];

/// The dashboard's settings save — special-cased in `op()` for the same
/// reason the agent and key ops are (its own refusal shape), but a list of
/// one so [`all`] has nowhere else to special-case it.
pub const SETTINGS_OPS: &[&str] = &["settings_set_full"];

/// Every op name, in list order — `MAIN_OPS`, then `AGENT_OPS`, `KEY_OPS`,
/// `SETTINGS_OPS`. 83 in total (§4.7, plus the benchmark design's seven,
/// §8.1, `voice_transcribe`, audio-class gap 5, and the realtime design's
/// `realtime_budget`, §12).
pub fn all() -> impl Iterator<Item = &'static str> {
    MAIN_OPS
        .iter()
        .chain(AGENT_OPS.iter())
        .chain(KEY_OPS.iter())
        .chain(SETTINGS_OPS.iter())
        .copied()
}

/// Is `name` one of the 83? What `op()` checks before it looks at its own
/// arms (§4.7): unlisted means unreachable, whatever a `match` arm below it
/// might otherwise have answered.
pub fn is_op(name: &str) -> bool {
    all().any(|n| n == name)
}

/// The gate itself: `op()`'s refusal for a name [`is_op`] does not know, or
/// `None` to go on. Worded apart from the dispatchers' own fall-through
/// `unknown op '…'` so a caller — and `tests/it`'s
/// `an_unlisted_op_is_refused_before_dispatch` — can tell the gate answered,
/// not an arm that happened to be missing (review R2 #6).
pub fn refuse_unlisted(name: &str) -> Option<String> {
    (!is_op(name)).then(|| {
        format!("unknown op '{name}': not an op name (GET /api/openapi.json lists every op)")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn the_four_lists_are_82_names_with_no_duplicate() {
        let all: Vec<&str> = all().collect();
        assert_eq!(all.len(), 83, "{all:#?}");
        let set: HashSet<&str> = all.iter().copied().collect();
        assert_eq!(set.len(), all.len(), "a name appears in more than one list");
    }

    #[test]
    fn is_op_agrees_with_all() {
        assert!(is_op("upstream_set"));
        assert!(is_op("agents_restore"));
        assert!(is_op("key_rotate"));
        assert!(is_op("settings_set_full"));
        assert!(!is_op("route-walk-probe"));
        assert!(!is_op(""));
    }

    #[test]
    fn the_gate_refuses_exactly_the_unlisted_names() {
        for name in all() {
            assert_eq!(refuse_unlisted(name), None, "{name}");
        }
        // Near misses of real names are not names.
        for probe in [
            "",
            "route-walk-probe",
            "Builds",
            "builds ",
            "agent_set\n",
            "key_op",
        ] {
            let refusal = refuse_unlisted(probe).unwrap_or_else(|| panic!("{probe:?} passed"));
            assert!(refusal.starts_with("unknown op '"), "{refusal}");
            assert!(refusal.contains("not an op name"), "{refusal}");
        }
    }
}
