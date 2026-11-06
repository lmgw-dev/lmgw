//! Self-admin tools over `/mcp/admin` — the mode gate: what each self-admin level lists and refuses, and the tool schemas.

use super::*;

// ---------------------------------------------------------------------------
// The mode gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_only_lists_reads_and_hides_mutations() {
    let base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let sid = initialize(&base).await;
    let names = list_tool_names(&base, &sid).await;

    for expected in [
        "lmgw__status",
        "lmgw__models",
        "lmgw__upstreams",
        "lmgw__mcp_servers",
        "lmgw__logs",
        "lmgw__settings",
    ] {
        assert!(names.iter().any(|n| n == expected), "missing {expected}");
    }
    for forbidden in [
        "lmgw__upstream_set",
        "lmgw__model_set",
        "lmgw__local_model_set",
        "lmgw__mcp_server_set",
        "lmgw__container",
        "lmgw__settings_set",
    ] {
        assert!(
            !names.iter().any(|n| n == forbidden),
            "{forbidden} must not be listed at read_only"
        );
    }
}

#[tokio::test]
async fn read_only_refuses_a_mutation_with_an_actionable_error() {
    let base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let sid = initialize(&base).await;

    let result = call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "delete", "id": 1 }),
    )
    .await;

    // A refusal is a tool error the model can read and act on, not a protocol
    // error — and it must name the setting that would allow the call.
    assert!(is_error(&result));
    let msg = text_of(&result);
    assert!(msg.contains("read_only"), "unhelpful refusal: {msg}");
    assert!(msg.contains("full"), "refusal should name the fix: {msg}");
}

#[tokio::test]
async fn off_exposes_nothing_but_still_explains_itself() {
    let base = serve_with(state_with_mode(SelfAdmin::Off).await).await;
    let sid = initialize(&base).await;

    assert!(
        list_tool_names(&base, &sid).await.is_empty(),
        "self_admin=off must expose no tools at all"
    );

    // Reserved names never fall through to the aggregate, so a call while off
    // reports the disabled feature rather than a misleading "unknown tool".
    let result = call(&base, &sid, "lmgw__status", json!({})).await;
    assert!(is_error(&result));
    assert!(
        text_of(&result).contains("disabled"),
        "{}",
        text_of(&result)
    );
}

#[tokio::test]
async fn full_lists_every_tool() {
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;
    let names = list_tool_names(&base, &sid).await;

    assert!(names.iter().all(|n| n.starts_with("lmgw__")));
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "duplicate tool name: {names:?}");

    // Named rather than counted: a count assertion has to be edited every time
    // a tool is added, which makes it a chore instead of a check. These are
    // the tools the documented add-a-model path depends on, so their absence
    // is a real regression.
    for required in [
        "lmgw__status",
        "lmgw__hf_repo",
        "lmgw__hf_add",
        "lmgw__hf_downloads",
        "lmgw__gguf_files",
        "lmgw__model_inspect",
        "lmgw__local_model_plan",
        "lmgw__local_model_set",
        "lmgw__local_model_get",
        "lmgw__local_model_test",
        "lmgw__container",
    ] {
        assert!(names.iter().any(|n| n == required), "missing {required}");
    }

    // Everything visible at read_only must still be visible at full.
    let ro_base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let ro_sid = initialize(&ro_base).await;
    for n in list_tool_names(&ro_base, &ro_sid).await {
        assert!(names.contains(&n), "{n} is readable but missing at full");
    }
}

/// `extra_run_args` is a list of strings wherever a tool takes it: one `podman
/// run` argument per entry, the type the dashboard's answers use.
#[tokio::test]
async fn extra_run_args_is_a_list_of_strings_on_every_tool_input() {
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;
    let tools = list_tools(&base, &sid).await;
    for name in [
        "lmgw__aux_model_set",
        "lmgw__image_model_set",
        "lmgw__audio_model_set",
        "lmgw__mcp_server_set",
    ] {
        let schema = &tool(&tools, name)["inputSchema"]["properties"]["extra_run_args"];
        assert_eq!(schema["type"], "array", "{name}: {schema}");
        assert_eq!(schema["items"]["type"], "string", "{name}: {schema}");
    }
    // The llama-server flags of the chat and aux rows are lists too.
    for name in ["lmgw__local_model_set", "lmgw__aux_model_set"] {
        let schema = &tool(&tools, name)["inputSchema"]["properties"]["extra_args"];
        assert_eq!(schema["type"], "array", "{name}: {schema}");
        assert_eq!(schema["items"]["type"], "string", "{name}: {schema}");
    }
}

/// The ladder design (§6, §8 WP6): `local_model_set` gains a `ladder`
/// property and `clear` names it, `local_model_get`/`local_model_test`
/// describe the per-rung behaviour, and `status`/`logs` name the rung.
/// `ladder` stays a flat string like every other argument in this module
/// (`all_parameters_are_flat_scalars` pins that for the whole catalog) —
/// `hoist_ladder_arg` is what turns it into the real array before
/// `patch_from_args` ever sees it.
#[tokio::test]
async fn local_model_set_schema_describes_ladder_as_a_json_string() {
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;
    let tools = list_tools(&base, &sid).await;

    let set = tool(&tools, "lmgw__local_model_set");
    let ladder = &set["inputSchema"]["properties"]["ladder"];
    assert_eq!(ladder["type"], "string", "{ladder}");
    let desc = ladder["description"].as_str().unwrap();
    assert!(desc.contains("gguf_path"), "{desc}");
    assert!(desc.contains("ctx_size"), "{desc}");

    let clear = set["inputSchema"]["properties"]["clear"]["description"]
        .as_str()
        .unwrap();
    assert!(clear.contains("ladder"), "{clear}");

    let get = tool(&tools, "lmgw__local_model_get");
    assert!(get["description"].as_str().unwrap().contains("rungs"));

    let test = tool(&tools, "lmgw__local_model_test");
    assert!(test["description"].as_str().unwrap().contains("rung"));

    let status = tool(&tools, "lmgw__status");
    assert!(status["description"].as_str().unwrap().contains("climbing"));

    let logs = tool(&tools, "lmgw__logs");
    assert!(logs["description"].as_str().unwrap().contains("rung"));
}

/// The ladder plumbing over the real wire, the schema's own JSON-string
/// shape: `hoist_ladder_arg` parses the tool call's raw `ladder` string
/// argument into the real array before `patch_from_args`'s generic
/// deserialize turns it into `Vec<Rung>` (`ladder_models.rs` covers
/// `ops::local_model_set` directly with a hand-built `LocalModelPatch` —
/// this is what an actual MCP caller sends). `local_model_get` reads every
/// rung's own command line and derived numbers back, and `clear: "ladder"`
/// round-trips to not-a-ladder.
#[tokio::test]
async fn ladder_set_and_get_round_trip_over_the_wire() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let dir = tempfile::tempdir().unwrap();
    // Same architecture, no tokenizer keys at all (as `ladder_models.rs`'s own
    // fixture): every rung's signature is trivially identical, so rule 5
    // never blocks this round trip.
    synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    synth::chat("qwen3", 65536).write_to(&dir.path().join("top.gguf"));

    let mut settings = store::load_settings(&state.db).await.unwrap();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "laddered", "gguf_path": "base.gguf",
                "ctx_size": 32768, "parallel": 2, "n_predict": 4096,
                "ladder": "[{\"gguf_path\": \"top.gguf\", \"ctx_size\": 65536}]",
            }),
        )
        .await,
    );
    assert_eq!(created["ok"], true, "{created}");

    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "laddered" }),
        )
        .await,
    );
    assert_eq!(
        got["ladder"],
        json!([{ "gguf_path": "top.gguf", "ctx_size": 65536 }]),
        "{got}"
    );
    let rungs = got["rungs"].as_array().unwrap_or_else(|| panic!("{got}"));
    assert_eq!(rungs.len(), 2, "{got}");
    assert_eq!(rungs[0]["rung"], 1);
    assert_eq!(rungs[0]["of"], 2);
    assert_eq!(rungs[0]["gguf_path"], "base.gguf");
    assert_eq!(rungs[0]["per_slot_ctx"], 16384);
    assert!(
        rungs[0]["command_line"]
            .as_str()
            .unwrap()
            .contains("base.gguf"),
        "{got}"
    );
    assert_eq!(rungs[1]["rung"], 2);
    assert_eq!(rungs[1]["gguf_path"], "top.gguf");
    assert_eq!(rungs[1]["per_slot_ctx"], 32768);
    assert_eq!(rungs[1]["switchover"], 32768 - 4096);
    assert!(
        rungs[1]["command_line"]
            .as_str()
            .unwrap()
            .contains("top.gguf"),
        "{got}"
    );

    let cleared = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({ "action": "update", "model_id": "laddered", "clear": "ladder" }),
        )
        .await,
    );
    assert_eq!(cleared["ok"], true, "{cleared}");
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "laddered" }),
        )
        .await,
    );
    assert_eq!(got["ladder"], json!([]), "{got}");
    assert_eq!(got["rungs"], Value::Null, "{got}");
}

/// `hoist_ladder_arg`'s leniency: a caller that ignores the declared string
/// schema and sends the array directly still works — the same "an object
/// where a string is declared" tolerance `capabilities_override` extends the
/// other way round.
#[tokio::test]
async fn ladder_set_also_accepts_a_literal_array_argument() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let dir = tempfile::tempdir().unwrap();
    synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    synth::chat("qwen3", 65536).write_to(&dir.path().join("top.gguf"));

    let mut settings = store::load_settings(&state.db).await.unwrap();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "laddered-2", "gguf_path": "base.gguf",
                "ctx_size": 32768, "parallel": 2, "n_predict": 4096,
                "ladder": [{ "gguf_path": "top.gguf", "ctx_size": 65536 }],
            }),
        )
        .await,
    );
    assert_eq!(created["ok"], true, "{created}");
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "laddered-2" }),
        )
        .await,
    );
    assert_eq!(
        got["ladder"],
        json!([{ "gguf_path": "top.gguf", "ctx_size": 65536 }]),
        "{got}"
    );
}

/// Review third pass, T3: every other optional string in this module means
/// "leave unchanged" when empty (`ops::opt`) — an agent that fills every
/// optional field with "" is common with flat-string schemas, and `ladder`
/// must follow the same convention rather than silently clearing an
/// existing one. Clearing stays explicit (`clear: "ladder"`, or `"[]"`).
#[tokio::test]
async fn an_empty_string_ladder_argument_leaves_it_unchanged() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let dir = tempfile::tempdir().unwrap();
    synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    synth::chat("qwen3", 65536).write_to(&dir.path().join("top.gguf"));

    let mut settings = store::load_settings(&state.db).await.unwrap();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "laddered-3", "gguf_path": "base.gguf",
                "ctx_size": 32768, "parallel": 2, "n_predict": 4096,
                "ladder": "[{\"gguf_path\": \"top.gguf\", \"ctx_size\": 65536}]",
            }),
        )
        .await,
    );

    let updated = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "update", "model_id": "laddered-3", "idle_seconds": 600,
                "ladder": "",
            }),
        )
        .await,
    );
    assert_eq!(updated["ok"], true, "{updated}");

    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "laddered-3" }),
        )
        .await,
    );
    assert_eq!(
        got["ladder"],
        json!([{ "gguf_path": "top.gguf", "ctx_size": 65536 }]),
        "an empty ladder string must not clear it: {got}"
    );
    assert_eq!(
        got["idle_seconds"], 600,
        "the rest of the update still applied: {got}"
    );
}

/// Every tool that only reads must be reachable at `read_only`, and every tool
/// that changes something must not be. The gate is per-tool metadata, so a new
/// tool added with the wrong flag is exactly the mistake worth catching.
#[tokio::test]
async fn the_new_read_tools_are_available_without_write_access() {
    let base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let sid = initialize(&base).await;
    let names = list_tool_names(&base, &sid).await;

    for readable in [
        "lmgw__local_model_get",
        "lmgw__gguf_files",
        "lmgw__model_inspect",
        "lmgw__local_model_plan",
        "lmgw__llama_flags",
        "lmgw__hf_repo",
        "lmgw__hf_downloads",
        // Looking at the corpora and at the queue of what agents asked for
        // changes nothing, so a read-only grant keeps both.
        "lmgw__docs_corpora",
        "lmgw__docs_requests",
    ] {
        assert!(names.iter().any(|n| n == readable), "missing {readable}");
    }
    for gated in [
        "lmgw__hf_add",
        "lmgw__hf_set",
        // Loads a model off disk and pins VRAM — not a "read only" grant.
        "lmgw__local_model_test",
        // Creating, crawling or dropping a corpus is the owner's call.
        "lmgw__docs_corpus_set",
        "lmgw__docs_ingest",
        "lmgw__docs_request_set",
    ] {
        assert!(!names.iter().any(|n| n == gated), "{gated} leaked");
        let refused = call(&base, &sid, gated, json!({ "action": "delete" })).await;
        assert!(is_error(&refused), "{gated} should refuse at read_only");
    }
}
