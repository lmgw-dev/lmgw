//! The shipped image-pipeline recipes and the two ops verbs that drive them
//! (image-generation design §7.2).
//!
//! Three things are pinned here. **The list itself has to be a valid row**:
//! every `files` role and every `args` key an entry carries is checked against
//! the same `sd-server --help` vocabulary `image_model_set` validates a save
//! against, so a recipe can never hand the editor a row the editor would
//! refuse. **Presence is computed against the disk**, because "add from
//! recipe" is only useful if it can say what is already there — a VAE shared
//! with a pipeline you already have must not be downloaded twice. And **the
//! add verb hands back a row rather than creating one**, which is the whole
//! shape of §7.2.
//!
//! The hub-touching half owns the process-wide `HF_ENDPOINT` override, so it
//! is one test fn — the same rule `hf_download.rs` follows — held behind
//! `common::process_env_lock` since `tests/it` folded every suite into one
//! binary.

use lmgw_core::image_recipes::{self, ImageRecipe, ImageRecipeAlternative, ImageRecipeComponent};
use lmgw_core::sdcpp_caps::SdcppCaps;
use lmgw_core::state::AppState;
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// The shipped list
// ---------------------------------------------------------------------------

/// The keys whose pipelines take reference images, and may therefore serve
/// `/v1/images/edits`. Adding a key here is a claim that the family reads a
/// `-r` argument — sd-server segfaults when one that does not is sent one
/// (§12.8), so it is checked against upstream's own docs, never guessed from
/// the name.
const EDIT_FAMILIES: &[&str] = &[
    "flux1-kontext-dev",
    "flux2-klein-4b",
    "flux2-dev",
    "qwen-image-2.1",
    "qwen-image-edit-2509",
];

/// Everything a recipe promises the editor, asserted against the vocabulary
/// the editor validates with. A recipe that fails this would be an "add from
/// recipe" button that produces a row `image_model_set` refuses — the worst
/// possible shape for this feature.
#[test]
fn every_shipped_recipe_is_a_row_image_model_set_would_accept() {
    let caps = SdcppCaps::embedded();
    assert!(!image_recipes::all().is_empty(), "the list ships empty");

    for r in image_recipes::all() {
        let files = r.files_json(None).unwrap();
        let args = r.args_json();

        // Exactly one of the two ways to load a pipeline (§4).
        let loaders: Vec<&str> = ["model", "diffusion_model"]
            .into_iter()
            .filter(|k| files.contains_key(*k))
            .collect();
        assert_eq!(
            loaders.len(),
            1,
            "{} names {loaders:?} — exactly one of model / diffusion_model is required",
            r.key
        );
        assert_eq!(r.primary_role(), loaders[0], "{}", r.key);

        // Every key in both maps is a flag this build of sd-server has.
        let unknown = caps.validate_keys(&files, &args);
        assert!(unknown.is_empty(), "{}: {unknown:?}", r.key);

        assert!(!r.modes.is_empty(), "{} claims no modes", r.key);
        for m in r.modes {
            assert!(
                matches!(*m, "img_gen" | "vid_gen"),
                "{} claims an unknown mode {m}",
                r.key
            );
        }
        // `edit` is load-bearing (lmgw refuses /v1/images/edits without it and
        // sd-server segfaults on the request), so it may only be set on a
        // family that genuinely takes reference images. The list is spelled
        // out rather than sniffed from the key: `flux2-klein-4b` and
        // `qwen-image-2.1` edit without saying so in their names, and a
        // substring rule would have quietly let the next `…-edit-…` key
        // through unread.
        assert_eq!(
            r.edit,
            EDIT_FAMILIES.contains(&r.key),
            "{}: `edit` disagrees with the list of families that take reference images",
            r.key
        );
        assert!(!r.display_name.is_empty(), "{}", r.key);
        assert!(!r.description.is_empty(), "{}", r.key);
        assert!(!r.vram_note.is_empty(), "{}", r.key);

        for c in r.components {
            assert!(c.size_bytes > 0, "{} / {}: size 0", r.key, c.role);
            assert!(!c.note.is_empty(), "{} / {}", r.key, c.role);
            lmgw_core::hf::validate_repo(c.repo).unwrap_or_else(|e| panic!("{}: {e}", r.key));
            // A component's path has to be a legal destination, or the
            // download would be refused after the row was already handed out.
            lmgw_core::hf::dest_rel_path(c.repo, c.file)
                .unwrap_or_else(|e| panic!("{} / {}: {e}", r.key, c.role));
            assert!(
                caps.is_known(c.role),
                "{} / {} is not an sd-server flag",
                r.key,
                c.role
            );
            for a in c.alternatives {
                assert!(a.size_bytes > 0, "{} / {} / {}", r.key, c.role, a.file);
                assert!(!a.label.is_empty(), "{} / {}", r.key, a.file);
                assert_ne!(a.file, c.file, "{}: alternative repeats the default", r.key);
                lmgw_core::hf::dest_rel_path(c.repo, a.file)
                    .unwrap_or_else(|e| panic!("{} / {}: {e}", r.key, a.file));
            }
        }
        // Every recipe is reachable by its own primary file, which is what
        // `local_model_plan target=image` matches on.
        let primary = r.primary().expect("a loader component").dest_rel_path();
        let (hit, _) = image_recipes::match_path(&primary).expect("its own file must match");
        assert_eq!(hit.key, r.key, "{primary} matched the wrong recipe");
    }
}

/// Only the family somebody actually measured quotes a VRAM figure; every
/// other row says so in the word the UI shows.
#[test]
fn only_the_measured_family_quotes_a_vram_number() {
    let kontext = image_recipes::find("flux1-kontext-dev").unwrap();
    assert!(kontext.edit, "Kontext is the edit family");
    assert!(
        !image_recipes::find("z-image-turbo").unwrap().edit,
        "Z-Image-Turbo segfaults on an edit request (§12.8)"
    );
    // The measured figures land on the one family that was measured, and
    // nothing else invents any.
    assert!(
        image_recipes::find("z-image-turbo")
            .unwrap()
            .vram_note
            .contains("7.1 GiB"),
        "the spike's measurement should be the Z-Image note"
    );
    for r in image_recipes::all() {
        if r.key != "z-image-turbo" {
            assert!(
                r.vram_note.contains("Not measured yet"),
                "{} claims VRAM nobody measured: {}",
                r.key,
                r.vram_note
            );
        }
    }
}

/// The families upstream documents and this build can actually load, by key.
///
/// A missing family is invisible: "add from recipe" is the only surface that
/// knows a pipeline spans repos, so a model sd.cpp supports but no recipe
/// names does not exist as far as the dashboard is concerned. Pinning the keys
/// makes dropping one a deliberate act.
#[test]
fn every_family_the_list_claims_to_cover_is_reachable_by_key() {
    let keys = image_recipes::keys();
    for want in [
        // The design's original six.
        "z-image-turbo",
        "flux1-schnell",
        "flux1-dev",
        "flux1-kontext-dev",
        "sdxl-base",
        "qwen-image",
        // Added 2026-09-21 from upstream's own per-family docs.
        "z-image",
        "flux2-klein-4b",
        "flux2-dev",
        "chroma1-hd",
        "sd3.5-large",
        "qwen-image-2.1",
        "qwen-image-edit-2509",
        "wan2.1-t2v-1.3b",
    ] {
        assert!(keys.contains(&want), "{want} is missing from {keys:?}");
    }

    // Qwen-Image 2.1 edits, and an edit needs the vision tower beside the
    // encoder — a row that claimed `edit` without `llm_vision` would be one
    // sd-server segfaults on.
    for key in ["qwen-image-2.1", "qwen-image-edit-2509"] {
        let r = image_recipes::find(key).unwrap();
        assert!(r.edit, "{key}");
        assert!(
            r.components.iter().any(|c| c.role == "llm_vision"),
            "{key} claims edit without a vision tower"
        );
    }

    // The one video row is video only: Wan needs sd-server's vid_gen mode even
    // for a single frame, so claiming `img_gen` would advertise a still this
    // pipeline does not draw.
    let wan = image_recipes::find("wan2.1-t2v-1.3b").unwrap();
    assert_eq!(wan.modes, &["vid_gen"]);
    assert!(!wan.edit);

    // FLUX.2 dev only fits a 24 GiB card because its 19 GB DiT and its 14 GB
    // Mistral encoder are streamed in one at a time rather than held together,
    // so `offload_to_cpu` is not advice on this row — it is what makes the
    // pipeline loadable, and dropping it silently would turn the recipe into
    // an out-of-memory report.
    let flux2 = image_recipes::find("flux2-dev").unwrap();
    assert!(
        matches!(
            flux2.args_json().get("offload_to_cpu"),
            Some(Value::Bool(true))
        ),
        "flux2-dev must ship offload_to_cpu: {:?}",
        flux2.args_json()
    );
    assert!(
        flux2.vram_note.contains("offload_to_cpu"),
        "and say why it is there"
    );

    // SD 3.5 is the one family loaded as a checkpoint beside its encoders
    // rather than as a standalone DiT.
    assert_eq!(
        image_recipes::find("sd3.5-large").unwrap().primary_role(),
        "model"
    );
}

/// Components are shared between recipes on purpose, and every row that shares
/// one can say so.
///
/// This is the fact behind the bug it was written for: queueing Z-Image-Turbo
/// starts exactly three transfers, but one of them is the FLUX autoencoder six
/// rows point at — so six cards saw a live download row. The UI needs the
/// neighbours' names to say *whose* transfer it is; without them the only
/// honest reading was "everything started downloading".
#[test]
fn a_shared_component_names_the_other_recipes_that_want_it() {
    let z = image_recipes::find("z-image-turbo").unwrap();
    let vae = z.components.iter().find(|c| c.role == "vae").unwrap();
    let shared = image_recipes::shared_with("z-image-turbo", vae.repo, vae.file);
    assert!(
        shared.len() >= 4,
        "the FLUX ae is shared with the FLUX and Chroma rows: {shared:?}"
    );
    assert!(shared.contains(&"FLUX.1 dev"), "{shared:?}");
    assert!(shared.contains(&"Chroma1 HD"), "{shared:?}");
    assert!(
        !shared.contains(&"Z-Image-Turbo"),
        "the asking recipe is never its own neighbour: {shared:?}"
    );

    // A file only one recipe names has no neighbours, and says nothing.
    let dit = z.primary().unwrap();
    assert!(
        image_recipes::shared_with("z-image-turbo", dit.repo, dit.file).is_empty(),
        "the loader of a pipeline is its own"
    );

    // Every claim is symmetric: if A lists B, B lists A.
    for r in image_recipes::all() {
        for c in r.components {
            for other in image_recipes::shared_with(r.key, c.repo, c.file) {
                let back = image_recipes::all()
                    .iter()
                    .find(|o| o.display_name == other)
                    .map(|o| image_recipes::shared_with(o.key, c.repo, c.file))
                    .unwrap_or_default();
                assert!(
                    back.contains(&r.display_name),
                    "{} / {} claims {other} but not the other way round",
                    r.key,
                    c.role
                );
            }
        }
    }
}

/// Every shipped recipe names an un-gated source today, so "add from recipe"
/// works on a box with no Hugging Face token at all. The refusal path still
/// has to be right for the family that eventually has no mirror.
#[test]
fn no_shipped_recipe_needs_a_token_and_the_refusal_still_names_the_component() {
    for r in image_recipes::all() {
        for c in r.components {
            assert!(
                !c.gated,
                "{} / {} is gated — either find an un-gated mirror or accept that the \
                 recipe cannot be added without a token",
                r.key, c.role
            );
        }
        assert!(r.gated_refusal(None, false).is_none(), "{}", r.key);
    }

    // A hypothetical family with no mirror: the refusal names the component,
    // because "set a token" is useless without knowing which file needs one.
    static GATED: ImageRecipe = ImageRecipe {
        key: "hypothetical",
        display_name: "Hypothetical",
        description: "d",
        components: &[ImageRecipeComponent {
            role: "diffusion_model",
            repo: "black-forest-labs/FLUX.2-dev",
            file: "flux2-dev.safetensors",
            size_bytes: 1,
            gated: true,
            note: "n",
            alternatives: &[],
        }],
        args: &[],
        modes: &["img_gen"],
        edit: false,
        vram_note: "Unmeasured.",
    };
    let msg = GATED.gated_refusal(None, false).expect("a refusal");
    assert!(msg.contains("diffusion_model"), "{msg}");
    assert!(msg.contains("black-forest-labs/FLUX.2-dev"), "{msg}");
    assert!(msg.contains("gated repo"), "{msg}");
    assert!(
        GATED.gated_refusal(None, true).is_none(),
        "a configured token lifts it"
    );
}

/// The quant picker acts on the component that loads the pipeline, and on
/// nothing else.
#[test]
fn the_quant_picker_swaps_one_component_and_refuses_a_name_the_repo_lacks() {
    let r = image_recipes::find("qwen-image").unwrap();
    let files = r.files_json(Some("Qwen_Image-Q8_0.gguf")).unwrap();
    assert_eq!(
        files["diffusion_model"],
        "QuantStack/Qwen-Image-GGUF/Qwen_Image-Q8_0.gguf"
    );
    assert_eq!(
        files["llm"], "unsloth/Qwen2.5-VL-7B-Instruct-GGUF/Qwen2.5-VL-7B-Instruct-Q4_K_M.gguf",
        "the encoder is untouched by a diffusion quant pick"
    );
    let err = r.files_json(Some("Qwen_Image-Q9_K.gguf")).unwrap_err();
    assert!(err.contains("is not a file of qwen-image's"), "{err}");
    assert!(
        err.contains("Qwen_Image-Q8_0.gguf"),
        "it lists what is: {err}"
    );

    // Unused import guard: the alternative type is part of the public surface
    // a UI reads.
    let _ = ImageRecipeAlternative {
        file: "x",
        size_bytes: 1,
        label: "x",
    };
}

// ---------------------------------------------------------------------------
// The ops verbs, against a mock hub
// ---------------------------------------------------------------------------

/// A hub tree listing for one repo.
fn tree(files: &[(&str, u64)]) -> Value {
    Value::Array(
        files
            .iter()
            .map(|(p, s)| json!({ "type": "file", "path": p, "size": s }))
            .collect(),
    )
}

/// Presence, the add verb, the prefilled row, and the two refusals — one test
/// fn because it owns the process-wide `HF_ENDPOINT`.
#[tokio::test]
async fn recipe_add_queues_only_what_is_missing_and_hands_back_a_row() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());

    let recipe = image_recipes::find("z-image-turbo").unwrap();
    let bytes = b"not really weights".to_vec();
    for c in recipe.components {
        Mock::given(method("GET"))
            .and(path(format!("/api/models/{}/tree/main", c.repo)))
            .respond_with(ResponseTemplate::new(200).set_body_json(tree(&[(c.file, 18)])))
            .mount(&hub)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/{}/resolve/main/{}", c.repo, c.file)))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"e1\"")
                    .set_body_bytes(bytes.clone()),
            )
            .mount(&hub)
            .await;
    }

    let state = AppState::init_for_tests().await.unwrap();

    // 1. No models dir: the add verb refuses by naming the setting, before it
    //    writes anything.
    let err = lmgw_core::ops::image_recipe_add(&state, "z-image-turbo", None)
        .await
        .unwrap_err();
    assert!(
        err.contains("image.models_dir") && err.contains("not configured"),
        "{err}"
    );
    let view = lmgw_core::ops::image_recipes(&state).await.unwrap();
    assert_eq!(view["models_dir_missing"], true);
    assert_eq!(view["hf_token_set"], false);

    // 2. With a models dir, and the VAE already on disk: it counts as present
    //    and is not queued again.
    let dir = tempfile::tempdir().unwrap();
    let vae = recipe
        .components
        .iter()
        .find(|c| c.role == "vae")
        .unwrap()
        .dest_rel_path();
    let vae_path = dir.path().join(&vae);
    std::fs::create_dir_all(vae_path.parent().unwrap()).unwrap();
    std::fs::write(&vae_path, &bytes).unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.image.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let view = lmgw_core::ops::image_recipes(&state).await.unwrap();
    assert_eq!(view["models_dir_missing"], false);
    let z = view["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "z-image-turbo")
        .expect("the recipe is listed")
        .clone();
    assert_eq!(z["installed"], false);
    assert_eq!(z["partial"], true, "{z}");
    assert_eq!(z["served"], false);
    assert_eq!(z["suggested_model_id"], "z-image-turbo");
    let present: Vec<bool> = z["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["present"].as_bool().unwrap())
        .collect();
    assert_eq!(present, vec![false, true, false], "{z}");
    // The list is offline: sizes and gating come out of the binary, so a
    // component carries them before anything has been fetched.
    let dit = &z["components"][0];
    assert_eq!(dit["role"], "diffusion_model");
    assert_eq!(dit["repo"], "leejet/Z-Image-Turbo-GGUF");
    assert_eq!(dit["size_bytes"], 3_864_250_304u64);
    assert_eq!(dit["gated"], false);
    assert!(
        dit["alternatives"].as_array().unwrap().len() >= 5,
        "the repo's other quants are offered: {dit}"
    );

    // 3. Add: two downloads queued, the VAE skipped, and a prefilled row back.
    let added = lmgw_core::ops::image_recipe_add(&state, "z-image-turbo", None)
        .await
        .unwrap();
    assert_eq!(added["ok"], true);
    assert_eq!(added["files_queued"], 2, "{added}");
    assert_eq!(added["downloads"].as_array().unwrap().len(), 2);
    let skipped = added["already_present"][0].as_str().unwrap();
    assert!(skipped.starts_with("vae ("), "{skipped}");
    let row = &added["row"];
    assert_eq!(row["action"], "create");
    assert_eq!(row["model_id"], "z-image-turbo");
    assert_eq!(
        row["files"],
        json!({
            "diffusion_model": "leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf",
            "vae": "Comfy-Org/z_image_turbo/split_files/vae/ae.safetensors",
            "llm": "unsloth/Qwen3-4B-Instruct-2507-GGUF/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
        })
    );
    assert_eq!(row["args"]["cfg_scale"], 1.0);
    assert_eq!(row["args"]["steps"], 8);
    assert_eq!(row["args"]["diffusion_fa"], true);
    assert_eq!(row["modes"], json!(["img_gen"]));
    assert_eq!(row["edit"], false);
    // It does not create the row: that is the editor's (or the agent's) call,
    // after the files land.
    assert!(
        state.snapshot().image_models.is_empty(),
        "image_recipe_add must not create a row"
    );
    assert!(
        added["message"].as_str().unwrap().contains("hf_downloads"),
        "the message teaches the next step: {added}"
    );
    let bad = lmgw_core::ops::image_recipe_add(&state, "nope", None)
        .await
        .unwrap_err();
    assert!(bad.contains("z-image-turbo"), "it lists what it has: {bad}");

    // 4. Let the transfers finish; presence flips and the pipeline is
    //    complete.
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let rows = store::list_hf_models(&state.db).await.unwrap();
        if rows.len() == 2 && rows.iter().all(|r| r.status == "done") {
            break;
        }
    }
    let rows = store::list_hf_models(&state.db).await.unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    for r in &rows {
        assert_eq!(r.status, "done", "{:?}", r.error);
        assert_eq!(r.target, "image");
    }
    let view = lmgw_core::ops::image_recipes(&state).await.unwrap();
    let z = view["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "z-image-turbo")
        .unwrap()
        .clone();
    assert_eq!(z["installed"], true, "{z}");
    assert_eq!(z["partial"], false);
    assert_eq!(z["components"][0]["done"], true);
    assert_eq!(z["components"][0]["downloading"], false);

    // 5. Adding again is a clean no-op that still returns the row.
    let again = lmgw_core::ops::image_recipe_add(&state, "z-image-turbo", None)
        .await
        .unwrap();
    assert_eq!(again["files_queued"], 0, "{again}");
    assert_eq!(again["already_present"].as_array().unwrap().len(), 3);

    // 6. The files are visible to the class's own listing — including the
    //    `.safetensors`, which is what the GGUF-only gate used to hide.
    let listed = lmgw_core::modelinfo::gguf_files(&state, None, Some("image"))
        .await
        .unwrap();
    let paths: Vec<&str> = listed["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths.len(), 3, "{listed}");
    assert!(paths.contains(&vae.as_str()), "{paths:?}");

    // 7. …and the plan verb turns one of them back into the whole row.
    let plan = lmgw_core::modelinfo::local_model_plan(
        &state,
        "leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf",
        false,
        Some("image"),
    )
    .await
    .unwrap();
    assert_eq!(plan["class"], "image");
    assert_eq!(plan["planned"], true, "{plan}");
    assert_eq!(plan["recipe"], "z-image-turbo");
    assert_eq!(plan["complete"], true, "{plan}");
    assert_eq!(plan["matched_role"], "diffusion_model");
    assert_eq!(plan["params"]["files"], row["files"]);
    assert_eq!(plan["params"]["args"]["steps"], 8);
    assert!(
        plan["why"]["args.steps"].is_string(),
        "every value carries a rationale: {plan}"
    );
    assert_eq!(plan["missing"], json!([]));

    // 8. A **sharded** `.safetensors` is queued as the one shard it is, with a
    //    warning that says so. sd-server loads a single file per component and
    //    has no loader that reassembles shards, and lmgw will not guess the
    //    sibling names of a format whose index it does not read — so the
    //    honest move is to fetch what was asked for and name what it is worth.
    let shard = "diffusion_pytorch_model-00001-of-00003.safetensors";
    Mock::given(method("GET"))
        .and(path("/api/models/some/sharded/tree/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tree(&[
            (shard, 9),
            ("diffusion_pytorch_model-00002-of-00003.safetensors", 9),
        ])))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/some/sharded/resolve/main/{shard}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"shardbytes".to_vec()))
        .mount(&hub)
        .await;
    let sharded = lmgw_core::ops::hf_add(&state, "some/sharded", Some(shard), None, "image", false)
        .await
        .unwrap();
    assert_eq!(
        sharded["files_queued"], 1,
        "only the named shard: {sharded}"
    );
    let w = sharded["warnings"].as_array().unwrap();
    assert_eq!(w.len(), 1, "{sharded}");
    let w = w[0].as_str().unwrap();
    assert!(w.contains("shard 1 of 3"), "{w}");
    assert!(w.contains("cannot reassemble shards"), "{w}");
    assert!(w.contains("image_recipes"), "it names what does work: {w}");

    // 9. A quant picked for a target that cannot load anything in the repo
    //    names the kinds, like the explicit `file=` branch does — "available:
    //    (none)" sent the caller hunting for a label that does not exist.
    let chat_dir = tempfile::tempdir().unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.router.models_dir = chat_dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let err = lmgw_core::ops::hf_add(&state, "some/sharded", None, Some("Q4_K_M"), "chat", false)
        .await
        .unwrap_err();
    assert!(err.contains("no file the chat class loads"), "{err}");
    assert!(err.contains(".gguf"), "the accepted kinds are named: {err}");
    assert!(err.contains("target=image"), "{err}");
}

/// A component already on its way is **not** queued again.
///
/// `queue_files` upserts the row, which resets its status to `queued` — under
/// the job that is downloading it. The Downloads page then showed a file as
/// queued while it was being written, and the second start was refused as
/// already running anyway. So a row that is `queued` or `downloading` is
/// skipped and reported, which is also the answer an agent polling
/// `hf_downloads` needs.
#[tokio::test]
async fn a_component_already_downloading_is_reported_not_queued_again() {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.image.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    // Every component of the recipe is in flight and none is on disk, so the
    // verb has nothing left to do — and therefore never touches the hub.
    let recipe = image_recipes::find("z-image-turbo").unwrap();
    for (i, c) in recipe.components.iter().enumerate() {
        let id = store::upsert_hf_model(&state.db, c.repo, c.file, &c.dest_rel_path(), "image")
            .await
            .unwrap();
        let status = if i == 0 { "downloading" } else { "queued" };
        store::set_hf_status(&state.db, id, status, None)
            .await
            .unwrap();
    }

    let added = lmgw_core::ops::image_recipe_add(&state, "z-image-turbo", None)
        .await
        .unwrap();
    assert_eq!(added["files_queued"], 0, "{added}");
    assert_eq!(added["already_present"], json!([]));
    let queued = added["already_queued"].as_array().unwrap();
    assert_eq!(queued.len(), recipe.components.len(), "{added}");
    assert!(
        queued
            .iter()
            .any(|q| q.as_str().unwrap().contains("downloading")),
        "the status each one is in is named: {queued:?}"
    );
    assert!(
        added["message"]
            .as_str()
            .unwrap()
            .contains("already downloading or queued"),
        "{added}"
    );
    // The rows were left exactly as they were: nothing was flipped back.
    let rows = store::list_hf_models(&state.db).await.unwrap();
    assert_eq!(rows.len(), recipe.components.len());
    assert_eq!(
        rows.iter().filter(|r| r.status == "downloading").count(),
        1,
        "the running download must not have been reset to queued"
    );
}

/// A file no recipe knows says so, and names the surface that does configure
/// the class — never a plan built out of guesses.
#[tokio::test]
async fn an_unknown_family_says_so_and_lists_the_recipes_it_knows() {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let rel = "someone/their-model/mystery-dit-Q4.gguf";
    let p = dir.path().join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, b"x").unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.image.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let plan = lmgw_core::modelinfo::local_model_plan(&state, rel, false, Some("image"))
        .await
        .unwrap();
    assert_eq!(plan["planned"], false, "{plan}");
    let reason = plan["reason"].as_str().unwrap();
    assert!(reason.contains("unknown family"), "{reason}");
    assert!(reason.contains("Set `files` by hand"), "{reason}");
    let known: Vec<&str> = plan["known_recipes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert!(known.contains(&"flux1-schnell"), "{known:?}");
}

/// A partial pipeline plans anyway, and names what is still missing with the
/// call that fetches it — the state an owner is actually in halfway through.
#[tokio::test]
async fn a_half_downloaded_pipeline_plans_with_its_gaps_named() {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let recipe = image_recipes::find("flux1-schnell").unwrap();
    let dit = recipe.primary().unwrap().dest_rel_path();
    let p = dir.path().join(&dit);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, b"x").unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.image.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let plan = lmgw_core::modelinfo::local_model_plan(&state, &dit, false, Some("image"))
        .await
        .unwrap();
    assert_eq!(plan["planned"], true, "{plan}");
    assert_eq!(plan["recipe"], "flux1-schnell");
    assert_eq!(plan["complete"], false);
    assert_eq!(plan["params"]["files"], json!({ "diffusion_model": dit }));
    let missing = plan["missing"].as_array().unwrap();
    assert_eq!(missing.len(), 3, "vae, clip_l and t5xxl are absent: {plan}");
    let roles: Vec<&str> = missing
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, vec!["vae", "clip_l", "t5xxl"]);
    assert!(
        missing[0]["hf_add"]
            .as_str()
            .unwrap()
            .contains("target=image"),
        "{missing:?}"
    );
    assert!(
        plan["next_step"]
            .as_str()
            .unwrap()
            .contains("image_recipe_add key=flux1-schnell"),
        "{plan}"
    );
}

// ---------------------------------------------------------------------------
// The live check (skips cleanly when the spike's weights are not on this box)
// ---------------------------------------------------------------------------

/// Not a mock: the three files the WP0 spike left under
/// `~/.local/share/lmgw/sdcpp` are exactly the Z-Image-Turbo recipe's, in the
/// `<owner>/<repo>/<file>` layout the downloader writes — so on this box the
/// recipe has to report all three present without anything being downloaded.
///
/// That is the one assertion a mock cannot make: that the names and the layout
/// in the compiled-in list match what a real transfer produces.
#[tokio::test]
async fn the_spike_pipeline_reports_every_component_present_on_this_box() {
    let dir = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".local/share/lmgw/sdcpp");
    let recipe = image_recipes::find("z-image-turbo").unwrap();
    let all_there = recipe
        .components
        .iter()
        .all(|c| dir.join(c.dest_rel_path()).is_file());
    if !all_there {
        eprintln!(
            "skipping: the spike's Z-Image-Turbo files are not under {}",
            dir.display()
        );
        return;
    }

    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.image.models_dir = dir.display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let view = lmgw_core::ops::image_recipes(&state).await.unwrap();
    let z = view["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "z-image-turbo")
        .unwrap()
        .clone();
    assert_eq!(z["installed"], true, "{z}");
    for c in z["components"].as_array().unwrap() {
        assert_eq!(c["present"], true, "{c}");
        // The recipe's size is the hub's; the file on disk is the same one.
        let on_disk = std::fs::metadata(dir.join(c["dest_path"].as_str().unwrap()))
            .unwrap()
            .len();
        assert_eq!(
            c["size_bytes"].as_u64().unwrap(),
            on_disk,
            "{} differs from the size the recipe claims",
            c["dest_path"]
        );
    }
}
