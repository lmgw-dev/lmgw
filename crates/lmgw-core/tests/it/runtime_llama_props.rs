//! What a managed llama-server says about itself (llama egress design §4.2,
//! §9.3): `GET /props` read once per start and per adoption, kept on the
//! registry entry with the start row's projector advisory, shown on the
//! container surfaces, and refreshed by the local model test. A container
//! that does not serve `/props` still starts, with its facts unknown and a
//! warning that says so.

use lmgw_core::config::LlamaParams;
use lmgw_core::runtime::lifecycle;
use lmgw_core::store::NewLocalModel;
use lmgw_core::vram::{self, LocalHold};
use serde_json::{json, Value};

use crate::support::gpu_world::{Gpu, GIB};

/// An official llama-server's answer (0c6a6a7), the template left out.
fn official() -> Value {
    json!({
        "default_generation_settings": {"params": {}, "n_ctx": 8192},
        "total_slots": 1,
        "modalities": {"vision": true, "video": false, "audio": false},
        "chat_template_caps": {"supports_tools": true, "supports_reasoning_effort": false},
        "build_info": "b11226-0c6a6a7",
    })
}

/// ik_llama.cpp's answer (7ff619c): no video, no build, no caps.
fn ik() -> Value {
    json!({
        "default_generation_settings": {"n_ctx": 4096},
        "total_slots": 2,
        "chat_template_caps": {},
        "modalities": {"vision": false, "audio": false},
        "n_ctx": 8192,
    })
}

async fn world(props: Option<Value>) -> Gpu {
    let gpu = Gpu::new(24 * GIB, 2, 5).await;
    gpu.model("seer", GIB).await;
    if let Some(body) = props {
        gpu.world().props.insert("seer".into(), body);
    }
    gpu
}

async fn start(gpu: &Gpu) {
    lmgw_core::ops::container(&gpu.state, None, Some("seer"), "start", false, None)
        .await
        .expect("the model starts");
}

async fn hold(gpu: &Gpu) -> LocalHold {
    vram::admit(&gpu.state, &gpu.route("seer"), "seer")
        .await
        .unwrap()
        .expect("a local model")
}

fn warnings(gpu: &Gpu) -> Vec<String> {
    gpu.state.runtime().list()[0].warnings.clone()
}

/// A start reads `/props` once, right after `/health`; the facts are on the
/// entry, on `lmgw__container status` and on the hold that claims it.
#[tokio::test]
async fn a_start_reads_what_the_server_says_about_itself() {
    let gpu = world(Some(official())).await;
    start(&gpu).await;
    assert_eq!(gpu.world().props_reads, ["seer"]);
    assert!(warnings(&gpu).is_empty(), "{:?}", warnings(&gpu));

    let status = lmgw_core::ops::container(&gpu.state, None, Some("seer"), "status", false, None)
        .await
        .unwrap();
    let shown = &status["runtime"]["llama_props"];
    assert_eq!(
        *shown,
        json!({
            "vision": true, "audio": false, "video": false,
            "caps": {"supports_tools": true, "supports_reasoning_effort": false},
            "n_ctx_slot": 8192, "build_info": "b11226-0c6a6a7",
        }),
        "{status}"
    );

    let held = hold(&gpu).await.llama().expect("a llama container");
    let facts = held.facts().expect("read at the start");
    assert_eq!(facts.vision, Some(true));
    assert_eq!(
        facts.raw,
        Value::Null,
        "the template-sized body is not kept"
    );
    // A row without a projector has nothing to advise.
    assert_eq!(held.ubatch_advisory, None);
    // A claim on a running container reads nothing again.
    assert_eq!(gpu.world().props_reads, ["seer"]);
}

/// ik's shape: what it does not send stays unknown.
#[tokio::test]
async fn an_ik_server_leaves_its_unsent_facts_unknown() {
    let gpu = world(Some(ik())).await;
    start(&gpu).await;
    let view = &gpu.state.runtime().list()[0];
    let facts = view.llama_props.as_ref().expect("read at the start");
    assert_eq!((facts.vision, facts.video), (Some(false), None));
    assert_eq!(
        (facts.n_ctx_slot, facts.build_info.as_deref()),
        (Some(4096), None)
    );
    assert!(view.warnings.is_empty(), "{:?}", view.warnings);
}

/// A server without the route still starts: the facts are unknown, and the
/// entry says why — never a failed start.
#[tokio::test]
async fn a_server_without_props_starts_with_a_warning() {
    let gpu = world(None).await;
    start(&gpu).await;
    let view = &gpu.state.runtime().list()[0];
    assert_eq!(view.state.as_str(), "ready");
    assert!(view.llama_props.is_none());
    assert_eq!(view.warnings.len(), 1, "{:?}", view.warnings);
    assert!(
        view.warnings[0].starts_with("GET /props could not be read (HTTP 404"),
        "{:?}",
        view.warnings
    );
    let frame = serde_json::to_value(view).unwrap();
    assert!(frame.get("llama_props").is_none(), "{frame}");
    let held = hold(&gpu).await.llama().expect("a llama container");
    assert!(held.facts().is_none());
    assert_eq!(gpu.runs(), ["seer"]);
}

/// The started row's projector advisory goes on the entry with the facts:
/// a projector whose header lmgw cannot read, under llama.cpp's default
/// batches, may abort the server on a large image (§8.2).
#[tokio::test]
async fn the_start_rows_projector_advisory_is_kept_with_the_facts() {
    let gpu = Gpu::new(24 * GIB, 1, 5).await;
    gpu.file("seer-mmproj.gguf", 1024);
    gpu.row(
        NewLocalModel {
            model_id: "seer".into(),
            gguf_path: "seer.gguf".into(),
            params: LlamaParams {
                mmproj_path: Some("seer-mmproj.gguf".into()),
                ..Default::default()
            },
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            ladder: vec![],
        },
        GIB,
    )
    .await;
    gpu.world().props.insert("seer".into(), official());
    start(&gpu).await;
    let held = hold(&gpu).await.llama().expect("a llama container");
    let advisory = held
        .ubatch_advisory
        .as_deref()
        .expect("the projector cannot be read");
    assert!(advisory.contains("non-causal"), "{advisory}");
    assert!(held.facts().is_some());
}

/// Adoption reads the facts again, after its probe and before its insert;
/// a container that does not serve them is still adopted, with a warning.
#[tokio::test]
async fn adoption_reads_the_facts_again() {
    for serves in [true, false] {
        let gpu = world(serves.then(official)).await;
        start(&gpu).await;
        gpu.restart();
        lifecycle::boot(&gpu.state).await;

        let view = gpu.state.runtime().list();
        assert_eq!(view.len(), 1, "adopted: {view:?}");
        assert_eq!(view[0].state.as_str(), "ready");
        // Adopted, not started again.
        assert_eq!(gpu.runs(), ["seer"]);
        assert!(gpu.stops().is_empty(), "{:?}", gpu.stops());
        assert_eq!(gpu.world().props_reads, ["seer", "seer"]);
        if serves {
            let facts = view[0].llama_props.as_ref().expect("read at adoption");
            assert_eq!(facts.build_info.as_deref(), Some("b11226-0c6a6a7"));
            assert!(view[0].warnings.is_empty(), "{:?}", view[0].warnings);
        } else {
            assert!(view[0].llama_props.is_none());
            assert_eq!(view[0].warnings.len(), 1, "{:?}", view[0].warnings);
            assert!(view[0].warnings[0].contains("GET /props"));
        }
        let held = hold(&gpu).await.llama().expect("a llama container");
        assert_eq!(held.facts().is_some(), serves);
    }
}

/// The local model test reads `/props` live through the same reader and
/// refreshes the entry, so a read that failed at the start does not last for
/// the container's life; a live read that fails replaces only a failure, and
/// leaves facts read before on the entry.
#[tokio::test]
async fn the_local_model_test_refreshes_the_entry() {
    let gpu = world(None).await;
    start(&gpu).await;
    assert_eq!(warnings(&gpu).len(), 1);
    let test = || async {
        lmgw_core::modelinfo::local_model_test(&gpu.state, "seer", None)
            .await
            .unwrap()
    };

    // No facts yet: a failed read is stored as the start's was.
    let out = test().await;
    assert_eq!(out["props"], Value::Null);
    let note = out["note"].as_str().unwrap();
    assert!(note.contains("GET /props could not be read"), "{out}");
    assert!(note.contains("unknown until it starts again"), "{out}");
    assert_eq!(warnings(&gpu).len(), 1);
    assert!(warnings(&gpu)[0].starts_with("GET /props could not be read (HTTP 404"));

    gpu.world().props.insert("seer".into(), official());
    let out = test().await;
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["props"], official(), "the body as the server sent it");
    assert!(out.get("note").is_none(), "{out}");

    let ctx = |gpu: &Gpu| {
        gpu.state.runtime().list()[0]
            .llama_props
            .as_ref()
            .and_then(|f| f.n_ctx_slot)
    };
    assert_eq!(ctx(&gpu), Some(8192));
    assert!(warnings(&gpu).is_empty(), "{:?}", warnings(&gpu));
    assert_eq!(gpu.runs(), ["seer"]);

    // A live read that fails says so on the test, and the entry keeps the
    // facts it had.
    gpu.world().props.clear();
    let out = test().await;
    assert_eq!(out["props"], Value::Null);
    let note = out["note"].as_str().unwrap();
    assert!(note.contains("GET /props could not be read"), "{out}");
    assert!(
        note.contains("the facts read before stay on the container"),
        "{out}"
    );
    assert_eq!(ctx(&gpu), Some(8192), "kept");
    assert!(warnings(&gpu).is_empty(), "{:?}", warnings(&gpu));
    let held = hold(&gpu).await.llama().expect("a llama container");
    assert!(held.facts().is_some(), "the resolver still reads them");
}
