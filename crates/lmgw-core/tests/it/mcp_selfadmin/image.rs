//! Self-admin tools over `/mcp/admin` — the image class through the tool plane.

use super::*;

// ---------------------------------------------------------------------------
// The image class through the tool plane (image-generation design §8)
// ---------------------------------------------------------------------------

/// The one departure from the audio precedent the design makes on purpose:
/// image CRUD is an *ops* verb, so `lmgw__image_model_set` and
/// `POST /api/op/image_model_set` are the same function and an agent can bring
/// a pipeline up without the dashboard.
///
/// The fixture is a real models directory with the three files a Z-Image-style
/// pipeline names, because every `files` value is checked against the disk when
/// the row is saved — a row that names a path nothing is at is refused, not
/// stored and discovered at the first start.
const DIT: &str = "Tongyi-MAI/Z-Image-Turbo/z-image-turbo-Q4_K.gguf";
const VAE: &str = "Comfy-Org/flux/ae.safetensors";
const LLM: &str = "Qwen/Qwen3-4B-Instruct-GGUF/Qwen3-4B-Q4_K_M.gguf";

async fn image_state() -> (SharedState, tempfile::TempDir) {
    let state = state_with_mode(SelfAdmin::Full).await;
    let dir = tempfile::tempdir().unwrap();
    for rel in [DIT, VAE, LLM] {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"weights").unwrap();
    }
    let mut s = state.snapshot().settings.clone();
    s.image.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    (state, dir)
}

/// The whole chain an agent drives: create the row from a `files` block, read
/// it back, see it on the model list, change one default, and delete it.
#[tokio::test]
async fn an_agent_creates_reads_updates_and_deletes_an_image_model() {
    let (state, _dir) = image_state().await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({
                "action": "create",
                "model_id": "z-image-turbo",
                "files": format!("diffusion_model = {DIT}\nvae = {VAE}\nllm = {LLM}"),
                "args": "diffusion_fa\ncfg_scale = 1.0\nsteps = 8",
                "modes": "img_gen",
                "idle_seconds": 120,
            }),
        )
        .await,
    );
    assert_eq!(created["ok"], true, "{created}");
    assert_eq!(created["public_name"], "image/z-image-turbo");
    // A failed vocabulary probe (no container runtime in a test) degrades to
    // the vocabulary lmgw ships with and *says so* — it never blocks the save.
    let warnings = created["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("flags lmgw ships with")),
        "the degraded key check should be reported: {created}"
    );

    let row = state.snapshot().image_models[0].clone();
    assert_eq!(row.files["diffusion_model"], json!(DIT));
    assert_eq!(row.files["vae"], json!(VAE));
    // A bare key is the switch it says it is; a number keeps its type, because
    // `--cfg-scale "1.0"` and `--cfg-scale 1.0` are not the same argv.
    assert_eq!(row.args["diffusion_fa"], json!(true));
    assert_eq!(row.args["cfg_scale"], json!(1.0));
    assert_eq!(row.args["steps"], json!(8));
    assert_eq!(row.idle_seconds, 120);

    // Read back through the shared read tool, which now answers for the class.
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "z-image-turbo", "target": "image" }),
        )
        .await,
    );
    assert_eq!(got["class"], "image");
    assert_eq!(got["public_name"], "image/z-image-turbo");
    assert_eq!(got["edit"], false);
    assert_eq!(got["endpoints"], json!(["/v1/images/generations"]));
    assert_eq!(got["files_present"]["diffusion_model"], true);
    assert_eq!(got["problems"].as_array().map(Vec::len), Some(0), "{got}");
    // The learned transient peak (image-generation §9): nothing has generated
    // on this row, so the tool reports the gap and the one thing that closes
    // it rather than a bare null an agent would read as "zero".
    assert!(got["peak_extra_bytes"].is_null(), "{got}");
    assert!(
        got["peak"]
            .as_str()
            .unwrap_or_default()
            .contains("not learned yet"),
        "{got}"
    );
    // The rendered command line is the thing an owner is really verifying.
    let cmd = got["command_line"].as_str().unwrap();
    assert!(cmd.contains("--eager-load") && cmd.contains(DIT), "{cmd}");

    // And on the model list, under the class prefix.
    let listed = payload(&call(&base, &sid, "lmgw__models", json!({ "kind": "image" })).await);
    let names: Vec<&str> = listed["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["image/z-image-turbo"], "{listed}");
    assert_eq!(listed["models"][0]["kind"], "image");
    assert_eq!(listed["models"][0]["modes"], json!(["img_gen"]));

    // One generation's worth of measurement, as the sampler would have written
    // it — so the update below has something to invalidate.
    let learned = state.snapshot().image_models[0].id;
    lmgw_core::store::set_image_model_peak(&state.db, learned, Some(6 * 1024 * 1024 * 1024))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();

    // Update is partial in the fields and wholesale in the maps: `args`
    // replaces, everything not mentioned stays.
    let updated = payload(
        &call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({
                "action": "update",
                "model_id": "z-image-turbo",
                "args": "steps = 4",
                "edit": true,
            }),
        )
        .await,
    );
    assert_eq!(updated["ok"], true, "{updated}");
    // `steps = 4` is a different pipeline to the GPU, so the measurement is
    // dropped — and the tool says it, because it is a promise admission has
    // stopped making.
    assert_eq!(updated["peak_reset"], true, "{updated}");
    assert!(
        updated["message"]
            .as_str()
            .unwrap_or_default()
            .contains("learned peak reset"),
        "{updated}"
    );
    let row = state.snapshot().image_models[0].clone();
    assert_eq!(row.peak_extra_bytes, None);
    assert_eq!(row.args, json!({"steps": 4}).as_object().cloned().unwrap());
    assert_eq!(row.files.len(), 3, "files must survive an args-only update");
    assert!(row.edit);
    assert_eq!(row.idle_seconds, 120);

    // A path pasted back out of the rendered command line carries the
    // in-container `/models/` prefix; it is dropped rather than stored and
    // rendered twice.
    payload(
        &call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({
                "action": "update",
                "model_id": "z-image-turbo",
                "files": format!("--diffusion-model = /models/{DIT}"),
            }),
        )
        .await,
    );
    let row = state.snapshot().image_models[0].clone();
    assert_eq!(
        row.files,
        json!({"diffusion_model": DIT})
            .as_object()
            .cloned()
            .unwrap()
    );

    let deleted = payload(
        &call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({ "action": "delete", "model_id": "z-image-turbo" }),
        )
        .await,
    );
    assert_eq!(deleted["ok"], true, "{deleted}");
    assert!(state.snapshot().image_models.is_empty());
}

/// The four refusals §4 asks for, each naming what to do about it. All of them
/// happen on the way in: a row that cannot start is never stored.
#[tokio::test]
async fn an_image_row_is_refused_for_the_four_things_that_cannot_start() {
    let (state, _dir) = image_state().await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let refuse = |args: Value| {
        let base = base.clone();
        let sid = sid.clone();
        async move {
            let r = call(&base, &sid, "lmgw__image_model_set", args).await;
            assert!(is_error(&r), "this should have been refused: {r}");
            text_of(&r)
        }
    };

    // 1. Both ways to load a pipeline at once.
    let msg = refuse(json!({
        "action": "create", "model_id": "both",
        "files": format!("model = {DIT}\ndiffusion_model = {DIT}"),
    }))
    .await;
    assert!(
        msg.contains("alternatives, not a pair"),
        "should name the exclusivity: {msg}"
    );

    // 2. Neither of them.
    let msg = refuse(json!({
        "action": "create", "model_id": "neither", "files": format!("vae = {VAE}"),
    }))
    .await;
    assert!(msg.contains("neither 'model'"), "{msg}");

    // 3. A path nothing is at.
    let msg = refuse(json!({
        "action": "create", "model_id": "missing",
        "files": "diffusion_model = nowhere/weights.gguf",
    }))
    .await;
    assert!(
        msg.contains("nowhere/weights.gguf") && msg.contains("not a file"),
        "should name the missing path: {msg}"
    );

    // 4. A key sd-server does not have — with the suggestion, because an
    //    over-long or truncated key is the common typo.
    let msg = refuse(json!({
        "action": "create", "model_id": "typo",
        "files": format!("diffusion_model = {DIT}"),
        "args": "vae_tiling_x",
    }))
    .await;
    assert!(
        msg.contains("vae_tiling_x") && msg.contains("did you mean") && msg.contains("vae-tiling"),
        "should suggest the real flag: {msg}"
    );

    assert!(
        state.snapshot().image_models.is_empty(),
        "no refused row may have been stored"
    );
}

/// `clear` names **whole fields**, not substrings. `clear=extra_run_args`
/// used to empty `args` as well (`"extra_run_args".contains("args")`), which
/// silently threw away every generation default a row had — and the row it
/// left behind still saved.
#[tokio::test]
async fn clear_names_a_whole_field_and_never_a_substring_of_one() {
    let (state, _dir) = image_state().await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let create = |args: Value| {
        let base = base.clone();
        let sid = sid.clone();
        async move { payload(&call(&base, &sid, "lmgw__image_model_set", args).await) }
    };

    let r = create(json!({
        "action": "create", "model_id": "z",
        "files": format!("diffusion_model = {DIT}"),
        "args": "steps = 8\ncfg_scale = 1.0",
        "extra_run_args": ["--device", "nvidia.com/gpu=all"],
    }))
    .await;
    assert_eq!(r["ok"], true, "{r}");

    // Clearing the run args leaves the generation defaults exactly as they
    // were — two fields, two names.
    let r = create(json!({
        "action": "update", "model_id": "z", "clear": "extra_run_args",
    }))
    .await;
    assert_eq!(r["ok"], true, "{r}");
    let row = state.snapshot().image_models[0].clone();
    assert_eq!(row.extra_run_args, None);
    assert_eq!(
        row.args["steps"],
        json!(8),
        "args must survive: {:?}",
        row.args
    );
    assert_eq!(row.args["cfg_scale"], json!(1.0));

    // And naming `args` really does clear them.
    let r = create(json!({ "action": "update", "model_id": "z", "clear": "args" })).await;
    assert_eq!(r["ok"], true, "{r}");
    assert!(state.snapshot().image_models[0].args.is_empty());

    // `files` is not clearable — a row with no files names no pipeline — and
    // a misspelled field name is refused rather than silently ignored.
    for (clear, needle) in [
        ("files", "not clearable"),
        ("agrs", "not a clearable field name"),
    ] {
        let r = call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({ "action": "update", "model_id": "z", "clear": clear }),
        )
        .await;
        assert!(is_error(&r), "clear={clear} should be refused: {r}");
        assert!(
            text_of(&r).contains(needle),
            "clear={clear}: {}",
            text_of(&r)
        );
    }
    assert_eq!(state.snapshot().image_models[0].files.len(), 1);
}

/// Four things a row can say that would render an argv nobody meant, each
/// refused at save time with the sentence that fixes it.
#[tokio::test]
async fn an_image_row_is_refused_for_the_keys_that_render_wrong() {
    let (state, _dir) = image_state().await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let refuse = |args: Value| {
        let base = base.clone();
        let sid = sid.clone();
        async move {
            let r = call(&base, &sid, "lmgw__image_model_set", args).await;
            assert!(is_error(&r), "this should have been refused: {r}");
            text_of(&r)
        }
    };

    // 1. A path in `args` is passed through verbatim, so it names a host path
    //    the container cannot see — `files` is the map that gets mounted.
    let msg = refuse(json!({
        "action": "create", "model_id": "path-in-args",
        "files": format!("diffusion_model = {DIT}"),
        "args": format!("vae = {VAE}"),
    }))
    .await;
    assert!(
        msg.contains("takes a path") && msg.contains("Move it to files"),
        "{msg}"
    );

    // 2. An alias of a flag lmgw renders itself: `l` *is* `--listen-ip`, and
    //    reading the raw key let it through to render a second copy.
    let msg = refuse(json!({
        "action": "create", "model_id": "alias",
        "files": format!("diffusion_model = {DIT}"),
        "args": "l = 127.0.0.1",
    }))
    .await;
    assert!(
        msg.contains("--listen-ip") && msg.contains("lmgw renders itself"),
        "{msg}"
    );

    // 3. `m` is `--model`, so a row spelling it that way beside a
    //    diffusion_model names both ways to load a pipeline.
    let msg = refuse(json!({
        "action": "create", "model_id": "both-by-alias",
        "files": format!("m = {DIT}\ndiffusion_model = {DIT}"),
    }))
    .await;
    assert!(msg.contains("alternatives, not a pair"), "{msg}");

    // 4. A directory value that climbs out of the models dir — this one is
    //    resolved and created on the *host*, so it never gets to be relative
    //    to something else.
    let msg = refuse(json!({
        "action": "create", "model_id": "escape",
        "files": format!("diffusion_model = {DIT}\nlora_model_dir = ../../tmp/x"),
    }))
    .await;
    assert!(
        msg.contains("../../tmp/x") && msg.contains("plain path relative to the image models dir"),
        "{msg}"
    );

    assert!(
        state.snapshot().image_models.is_empty(),
        "no refused row may have been stored"
    );
}

/// A quoted scalar in the text form is the string it quotes — the quotes are
/// JSON syntax, not two bytes of the value. Stored with them, `cfg_scale`
/// rendered as `--cfg-scale "1.0"` and sd-server could not parse it.
#[tokio::test]
async fn a_quoted_value_in_the_text_form_keeps_its_value_not_its_quotes() {
    let (state, _dir) = image_state().await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let r = payload(
        &call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({
                "action": "create", "model_id": "z",
                "files": format!("diffusion_model = {DIT}"),
                "args": "cfg_scale = \"1.0\"\nsampling_method = \"euler\"\ntype = q8_0",
            }),
        )
        .await,
    );
    assert_eq!(r["ok"], true, "{r}");
    let row = state.snapshot().image_models[0].clone();
    assert_eq!(row.args["cfg_scale"], json!("1.0"));
    assert_eq!(row.args["sampling_method"], json!("euler"));
    assert_eq!(row.args["type"], json!("q8_0"));
    let cmd = r["command_line"].as_str().unwrap();
    assert!(cmd.contains("--cfg-scale 1.0"), "{cmd}");
    assert!(
        !cmd.contains("\\\""),
        "no quote bytes reach the argv: {cmd}"
    );
}

/// `lmgw__llama_flags` reads *llama-server's* vocabulary. Handed an image
/// model id it used to take the id for an image reference and try to pull it;
/// now it says which verb answers for that class.
#[tokio::test]
async fn llama_flags_refuses_an_image_model_by_name() {
    let (state, _dir) = image_state().await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    payload(
        &call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({
                "action": "create", "model_id": "z-image-turbo",
                "files": format!("diffusion_model = {DIT}"),
            }),
        )
        .await,
    );

    let r = call(
        &base,
        &sid,
        "lmgw__llama_flags",
        json!({ "model": "z-image-turbo" }),
    )
    .await;
    assert!(is_error(&r), "{r}");
    let msg = text_of(&r);
    assert!(msg.contains("is an image"), "{msg}");
    assert!(msg.contains("target=image"), "{msg}");
    let _ = state;
}

/// Without a models directory there is nothing for a relative path to be
/// relative *to*, so the refusal names the setting rather than the file.
#[tokio::test]
async fn an_image_row_with_no_models_dir_refuses_by_naming_the_setting() {
    let state = state_with_mode(SelfAdmin::Full).await;
    assert!(state.snapshot().settings.image.models_dir.is_empty());
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let r = call(
        &base,
        &sid,
        "lmgw__image_model_set",
        json!({
            "action": "create", "model_id": "z",
            "files": "diffusion_model = a/b.gguf",
        }),
    )
    .await;
    assert!(is_error(&r), "{r}");
    let msg = text_of(&r);
    assert!(
        msg.contains("image models directory is not set")
            && msg.contains("Settings → Runtimes → Image → Models dir")
            && !msg.contains("models_dir"),
        "{msg}"
    );
    assert!(state.snapshot().image_models.is_empty());
}

/// `writes: true`, like every other `*_set`: hidden at `read_only` and refused
/// by name with the setting that would allow it.
#[tokio::test]
async fn image_model_set_is_gated_at_full_like_every_other_mutation() {
    let base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let sid = initialize(&base).await;
    let names = list_tool_names(&base, &sid).await;
    assert!(
        !names.iter().any(|n| n == "lmgw__image_model_set"),
        "a write tool must not be listed at read_only: {names:?}"
    );

    let r = call(
        &base,
        &sid,
        "lmgw__image_model_set",
        json!({ "action": "delete", "model_id": "whatever" }),
    )
    .await;
    assert!(is_error(&r));
    let msg = text_of(&r);
    assert!(msg.contains("read_only") && msg.contains("full"), "{msg}");
}

/// The two read tools that answer for the class without being able to *plan*
/// it: `local_model_check` runs the same pre-flight group apply runs, and
/// `local_model_plan` matches the file against the shipped recipes rather than
/// inventing a plan — this fixture's files belong to no recipe, so the answer
/// is the honest "unknown family" with the keys it does know.
#[tokio::test]
async fn check_reports_an_image_rows_problems_and_plan_names_the_recipes_it_knows() {
    let (state, dir) = image_state().await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    payload(
        &call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({
                "action": "create", "model_id": "z",
                "files": format!("diffusion_model = {DIT}\nvae = {VAE}"),
            }),
        )
        .await,
    );
    // Clean while the files are there…
    let ok = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_check",
            json!({ "model_id": "z", "target": "image" }),
        )
        .await,
    );
    assert_eq!(ok["checked"], 1, "{ok}");
    assert_eq!(ok["broken"], 0, "{ok}");
    assert_eq!(ok["models"][0]["class"], "image");
    assert_eq!(ok["models"][0]["modes"], json!(["img_gen"]));

    // …and broken the moment one of them is gone, which is exactly the state a
    // save could not have refused because it happened afterwards.
    std::fs::remove_file(dir.path().join(VAE)).unwrap();
    let broken = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_check",
            json!({ "target": "image" }),
        )
        .await,
    );
    assert_eq!(broken["broken"], 1, "{broken}");
    let issues = broken["models"][0]["problems"][0].as_str().unwrap();
    assert!(
        issues.contains("vae") && issues.contains("missing"),
        "{issues}"
    );

    let plan = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_plan",
            json!({ "gguf_path": DIT, "target": "image" }),
        )
        .await,
    );
    assert_eq!(plan["class"], "image");
    assert_eq!(plan["planned"], false);
    assert!(
        plan["reason"].as_str().unwrap().contains("unknown family"),
        "{plan}"
    );
    let known: Vec<&str> = plan["known_recipes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert!(known.contains(&"z-image-turbo"), "{known:?}");
    assert!(
        plan["next_step"]
            .as_str()
            .unwrap()
            .contains("lmgw__image_model_set"),
        "{plan}"
    );
}

/// The recipe chain over the wire, end to end: `lmgw__image_recipes` to see
/// what is shippable, `lmgw__image_recipe_add` to get the prefilled row, that
/// row straight into `lmgw__image_model_set action=create`, and the row it
/// makes checked and planned back.
///
/// The fixture puts the recipe's real files on disk, so nothing is queued and
/// no hub is needed here — the queueing half owns the process-wide
/// `HF_ENDPOINT` override and therefore lives in `tests/it/image_recipes.rs`,
/// which is single-purpose for exactly that reason. What this covers is the
/// *wire*: that the two tools exist on the admin plane, that the row comes
/// back in the field names `lmgw__image_model_set` takes, and that handing it
/// straight over works without an agent editing anything.
#[tokio::test]
async fn an_agent_brings_up_a_pipeline_from_a_recipe_over_the_wire() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let dir = tempfile::tempdir().unwrap();
    let recipe = lmgw_core::image_recipes::find("z-image-turbo").unwrap();
    for c in recipe.components {
        let p = dir.path().join(c.dest_rel_path());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"weights").unwrap();
    }
    let mut settings = state.snapshot().settings.clone();
    settings.image.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    // 1. The list, with this box's state folded in.
    let list = payload(&call(&base, &sid, "lmgw__image_recipes", json!({})).await);
    let z = list["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "z-image-turbo")
        .expect("z-image-turbo is shipped")
        .clone();
    assert_eq!(z["installed"], true, "{z}");
    assert_eq!(z["served"], false);
    assert_eq!(z["components"].as_array().unwrap().len(), 3);
    assert!(
        z["vram_note"].as_str().unwrap().contains("7.1 GiB"),
        "the measured figure travels with the recipe: {z}"
    );

    // 2. Add: nothing to queue, and a row in image_model_set's field names.
    let added = payload(
        &call(
            &base,
            &sid,
            "lmgw__image_recipe_add",
            json!({ "key": "z-image-turbo" }),
        )
        .await,
    );
    assert_eq!(added["files_queued"], 0, "{added}");
    assert_eq!(added["already_present"].as_array().unwrap().len(), 3);
    let row = added["row"].clone();
    assert_eq!(row["action"], "create");
    assert!(
        state.snapshot().image_models.is_empty(),
        "the recipe hands over a row, it does not create one"
    );

    // 3. That row, unedited, through the CRUD tool. `files` and `args` arrive
    //    as JSON objects here — the patch takes both spellings (§14, WP4).
    let created = payload(&call(&base, &sid, "lmgw__image_model_set", row.clone()).await);
    assert_eq!(created["ok"], true, "{created}");
    assert_eq!(created["public_name"], "image/z-image-turbo");
    let stored = state.snapshot().image_models[0].clone();
    assert_eq!(
        stored.files["diffusion_model"],
        json!("leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf")
    );
    assert_eq!(stored.args["steps"], json!(8));
    assert_eq!(stored.args["cfg_scale"], json!(1.0));
    assert_eq!(stored.args["diffusion_fa"], json!(true));
    assert_eq!(stored.modes, vec!["img_gen".to_string()]);

    // 4. It passes the class's own pre-flight…
    let checked = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_check",
            json!({ "model_id": "z-image-turbo", "target": "image" }),
        )
        .await,
    );
    assert_eq!(checked["broken"], 0, "{checked}");

    // 5. …and planning its diffusion model back returns the same recipe, now
    //    complete, so the loop closes.
    let plan = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_plan",
            json!({
                "gguf_path": "leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf",
                "target": "image",
            }),
        )
        .await,
    );
    assert_eq!(plan["planned"], true, "{plan}");
    assert_eq!(plan["recipe"], "z-image-turbo");
    assert_eq!(plan["complete"], true);
    assert_eq!(plan["params"]["files"], row["files"]);

    // …and the recipe now reports itself served.
    let list = payload(&call(&base, &sid, "lmgw__image_recipes", json!({})).await);
    let z = list["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "z-image-turbo")
        .unwrap()
        .clone();
    assert_eq!(z["served"], true, "{z}");

    // An unknown key is refused with the list of what is known.
    let bad = call(
        &base,
        &sid,
        "lmgw__image_recipe_add",
        json!({ "key": "stable-cascade" }),
    )
    .await;
    assert!(is_error(&bad));
    assert!(text_of(&bad).contains("flux1-schnell"), "{}", text_of(&bad));
}
