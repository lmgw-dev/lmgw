//! The aux hoist (decision-models design §3.3, WP2b): existing aux rows'
//! `--n-gpu-layers 0`, `--threads`, `--ubatch-size`, `--batch-size` and
//! `--mmproj` move from their freeform args into the five fields, once, at
//! boot, with a notice naming each row; `--n-gpu-layers 0` only on an image
//! without a GPU backend (§7 Q4 as refined 2026-10-12).
//!
//! The rule itself is pure (`AuxModel::hoist_args`) and is tested first, in
//! the `config_hoisting.rs` style. The boot fold then runs against a fake
//! `CommandRunner` whose `podman images` lists the images and whose
//! throwaway `find` lists each external image's ggml libraries, as the real
//! ones do (checked 2026-10-12 on the images named below).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use lmgw_core::config::{AuxBackend, AuxKind, AuxModel, ImageGpu, Settings};
use lmgw_core::erased::Erased;
use lmgw_core::gguf::synth;
use lmgw_core::ops;
use lmgw_core::runtime::argv::podman_run_argv;
use lmgw_core::runtime::descriptor::model_runtime;
use lmgw_core::runtime::lifecycle::hoist_aux_args;
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner, Registry};
use lmgw_core::runtime::{Class, Placement};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAuxModel};

// ---------------------------------------------------------------------------
// The rule, pure
// ---------------------------------------------------------------------------

fn row(args: &[&str]) -> AuxModel {
    AuxModel {
        id: 1,
        model_id: "m".into(),
        gguf_path: "org/m/m.gguf".into(),
        kind: AuxKind::Embed,
        pooling: None,
        ctx_size: None,
        ubatch_size: None,
        batch_size: None,
        mmproj_path: None,
        backend: None,
        threads: None,
        args: args.iter().map(|s| s.to_string()).collect(),
        idle_seconds: 0,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    }
}

fn cpu_only() -> ImageGpu {
    ImageGpu::CpuOnly("test".into())
}

/// Every spelling the argv renderer's alias table knows moves, the
/// `=value` one included.
#[test]
fn every_spelling_of_the_five_flags_moves() {
    let mut m = row(&[
        "-ngl=0",
        "-t",
        "8",
        "-ub",
        "4096",
        "--batch-size=8192",
        "-mm",
        "/models/o/p.gguf",
    ]);
    let hoist = m.hoist_args(&cpu_only());
    assert!(m.args.is_empty(), "{:?}", m.args);
    assert_eq!(m.backend, Some(AuxBackend::Cpu));
    assert_eq!(m.threads, Some(8));
    assert_eq!(m.ubatch_size, Some(4096));
    assert_eq!(m.batch_size, Some(8192));
    assert_eq!(m.mmproj_path.as_deref(), Some("o/p.gguf"));
    assert_eq!(hoist.moved.len(), 5);
    assert_eq!(hoist.moved[0], ("-ngl=0".into(), "backend cpu".into()));
    assert_eq!(hoist.moved[1], ("-t 8".into(), "threads 8".into()));

    let mut m = row(&["--gpu-layers", "0", "--n-gpu-layers", "0"]);
    let hoist = m.hoist_args(&cpu_only());
    assert_eq!(m.backend, Some(AuxBackend::Cpu));
    assert!(m.args.is_empty());
    // The second copy finds the field set: dropped, and said.
    assert_eq!(
        hoist.dropped,
        vec![("--n-gpu-layers 0".into(), "backend cpu".into())]
    );
}

/// A set field wins, as in the chat hoist: the copy in the args rendered
/// nothing (the renderer claims the key), so it goes, and the report names it.
#[test]
fn a_set_field_wins_and_its_copy_is_dropped() {
    let mut m = row(&["--threads", "8", "--flash-attn", "on"]);
    m.threads = Some(16);
    let hoist = m.hoist_args(&cpu_only());
    assert_eq!(m.threads, Some(16));
    assert_eq!(m.args, vec!["--flash-attn", "on"]);
    assert_eq!(
        hoist.dropped,
        vec![("--threads 8".into(), "threads 16".into())]
    );
    assert!(hoist.moved.is_empty());
}

/// What no field can hold stays exactly as written: counts the columns'
/// CHECKs refuse, a flag with no value, a layer count other than 0, a
/// projector outside `/models` or climbing out of it — and a flag that only
/// shares a prefix with one of the five (`-tb`, `--threads-batch`).
#[test]
fn what_no_field_can_hold_stays_in_the_args() {
    let kept = [
        "--threads",
        "0",
        "--ubatch-size",
        "abc",
        "-ngl",
        "99",
        "--mmproj",
        "/opt/proj.gguf",
        "-mm",
        "../up.gguf",
        "-tb",
        "4",
        "--threads-batch",
        "4",
        "--batch-size",
    ];
    let mut m = row(&kept);
    let hoist = m.hoist_args(&cpu_only());
    assert_eq!(m.args, kept, "order and spelling unchanged");
    assert_eq!(hoist, Default::default());
    assert_eq!((m.backend, m.threads, m.ubatch_size), (None, None, None));
    assert_eq!((m.batch_size, m.mmproj_path), (None, None));
}

/// `--n-gpu-layers 0` becomes `backend: cpu` only on an image without a GPU
/// backend; on a GPU image or an unknown one it stays, and the report says
/// so. The other four move whatever the image.
#[test]
fn ngl_0_moves_only_on_an_image_without_a_gpu_backend() {
    for image in [
        ImageGpu::Gpu("cuda".into()),
        ImageGpu::Unknown("not here".into()),
    ] {
        let mut m = row(&["-ngl", "0", "--threads", "8"]);
        assert!(m.hoist_reads_image());
        let hoist = m.hoist_args(&image);
        assert_eq!(m.args, vec!["-ngl", "0"], "{image:?}");
        assert_eq!(m.backend, None);
        assert_eq!(m.threads, Some(8));
        assert_eq!(hoist.kept_cpu_flag.as_deref(), Some("-ngl 0"));
    }
    let mut set = row(&["-ngl", "0"]);
    set.backend = Some(AuxBackend::Cpu);
    assert!(!set.hoist_reads_image(), "the field decides already");
    assert!(!row(&["-ngl", "99"]).hoist_reads_image());
    assert!(!row(&["--threads", "8"]).hoist_reads_image());
}

/// A second pass changes nothing and reports nothing moved.
#[test]
fn the_hoist_is_idempotent() {
    let mut m = row(&["-ngl", "0", "-t", "8", "--mmproj", "dir/only", "-c", "8192"]);
    m.hoist_args(&cpu_only());
    let once = serde_json::to_value(&m).unwrap();
    let again = m.hoist_args(&cpu_only());
    assert!(!again.changed());
    assert_eq!(again.kept_cpu_flag, None);
    assert_eq!(serde_json::to_value(&m).unwrap(), once);
    assert_eq!(m.args, vec!["-c", "8192"]);
}

// ---------------------------------------------------------------------------
// The boot fold
// ---------------------------------------------------------------------------

/// The CPU-only image (`ghcr.io/ggml-org/llama.cpp:server`, b11561).
const CPU_TAG: &str = "ghcr.io/ggml-org/llama.cpp:server";
const CPU_ID: &str = "a4857c89b4ca00000000000000000000000000000000000000000000000000aa";
/// The CUDA image, the shipped class default.
const CUDA_TAG: &str = "ghcr.io/ggml-org/llama.cpp:server-cuda";
const CUDA_ID: &str = "c5daab59931800000000000000000000000000000000000000000000000000bb";
/// An lmgw build labelled cpu: judged by its label, no container runs.
const LMGW_CPU_TAG: &str = "localhost/lmgw-llama-server:cpu-test";
const LMGW_CPU_ID: &str = "cccccccccccc00000000000000000000000000000000000000000000000000cc";
/// An ik_llama.cpp-layout image without a label: one libggml.so.
const IK_TAG: &str = "example.org/ik-llama:latest";
const IK_ID: &str = "dddddddddddd00000000000000000000000000000000000000000000000000dd";
/// Not on this machine.
const ABSENT_TAG: &str = "ghcr.io/example/absent:latest";
/// A second tag of the CPU image: one image, read once.
const CPU_ALIAS: &str = "localhost/llama-cpu:mine";
/// External images whose listing exits 1 (one with a GPU library in what it
/// read, one without), exits 125, and never ends.
const EXIT1_GPU_TAG: &str = "example.org/partial-gpu:1";
const EXIT1_GPU_ID: &str = "e1e1e1e1e1e100000000000000000000000000000000000000000000000000e1";
const EXIT1_CPU_TAG: &str = "example.org/partial-cpu:1";
const EXIT1_CPU_ID: &str = "e2e2e2e2e2e200000000000000000000000000000000000000000000000000e2";
const EXIT125_TAG: &str = "example.org/broken:1";
const EXIT125_ID: &str = "e3e3e3e3e3e300000000000000000000000000000000000000000000000000e3";
const HANG_TAG: &str = "example.org/hangs:1";
const HANG_ID: &str = "e4e4e4e4e4e400000000000000000000000000000000000000000000000000e4";

const CPU_LIBS: &str = "/app/libggml-base.so\n/app/libggml-base.so.0\n\
    /app/libggml-cpu-haswell.so\n/app/libggml-cpu-zen4.so\n/app/libggml.so\n/app/libggml.so.0\n";

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Vec<String>>>,
    /// `find`'s exit status and listing per image ID.
    files: HashMap<&'static str, (i32, String)>,
    /// `podman images` fails.
    images_broken: AtomicBool,
}

fn ok(stdout: &str) -> CmdOutput {
    CmdOutput {
        status: 0,
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

fn images_json() -> String {
    let img = |id: &str, names: &[&str], labels: serde_json::Value| {
        serde_json::json!({
            "Id": id, "Names": names, "RepoDigests": [], "Size": 1, "Created": 0,
            "Labels": labels,
        })
    };
    let none = serde_json::Value::Null;
    serde_json::json!([
        img(
            CPU_ID,
            &[CPU_TAG, CPU_ALIAS],
            serde_json::json!({"org.opencontainers.image.version": "b11561"})
        ),
        img(CUDA_ID, &[CUDA_TAG], none.clone()),
        img(
            LMGW_CPU_ID,
            &[LMGW_CPU_TAG],
            serde_json::json!({"dev.lmgw.backend": "cpu"})
        ),
        img(IK_ID, &[IK_TAG], none.clone()),
        img(EXIT1_GPU_ID, &[EXIT1_GPU_TAG], none.clone()),
        img(EXIT1_CPU_ID, &[EXIT1_CPU_TAG], none.clone()),
        img(EXIT125_ID, &[EXIT125_TAG], none.clone()),
        img(HANG_ID, &[HANG_TAG], none),
    ])
    .to_string()
}

impl CommandRunner for Fake {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
    ) -> Erased<'a, std::io::Result<CmdOutput>> {
        Box::pin(async move {
            assert_eq!(program, "podman");
            self.calls.lock().unwrap().push(args.to_vec());
            match args[0].as_str() {
                "images" if self.images_broken.load(Ordering::Relaxed) => Ok(CmdOutput {
                    status: 125,
                    stdout: String::new(),
                    stderr: "Error: cannot open the image store".into(),
                }),
                "images" => Ok(ok(&images_json())),
                "run" if args.iter().any(|a| a == "find") => {
                    // `--entrypoint find <image ID> / -xdev -name libggml*`
                    let at = args.iter().position(|a| a == "find").unwrap();
                    let id = args[at + 1].as_str();
                    if id == HANG_ID {
                        // Under a paused clock: the listing's timeout fires.
                        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    }
                    let (status, listing) = self.files.get(id).cloned().unwrap_or_default();
                    Ok(CmdOutput {
                        status,
                        stdout: listing,
                        stderr: if status == 0 {
                            String::new()
                        } else {
                            "find: '/root': Permission denied".into()
                        },
                    })
                }
                _ => Ok(ok("")),
            }
        })
    }
}

struct Fixture {
    state: SharedState,
    podman: Arc<Fake>,
    aux_dir: tempfile::TempDir,
}

const EMBEDDER: &str = "Qwen/Qwen3-Embedding-4B-GGUF/Qwen3-Embedding-4B-Q4_K_M.gguf";

async fn fixture() -> Fixture {
    let state = AppState::init_for_tests().await.unwrap();
    let aux_dir = tempfile::tempdir().unwrap();
    synth::embedding("qwen3", 3, 32768).write_to(&aux_dir.path().join(EMBEDDER));
    let cuda_libs = format!("{CPU_LIBS}/app/libggml-cuda.so\n");
    let podman = Arc::new(Fake {
        files: HashMap::from([
            (CPU_ID, (0, CPU_LIBS.to_string())),
            (CUDA_ID, (0, cuda_libs.clone())),
            (IK_ID, (0, "/usr/local/lib/libggml.so\n".to_string())),
            (EXIT1_GPU_ID, (1, cuda_libs)),
            (EXIT1_CPU_ID, (1, CPU_LIBS.to_string())),
            (EXIT125_ID, (125, String::new())),
        ]),
        ..Fake::default()
    });
    state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        podman.clone(),
        reqwest::Client::new(),
        Arc::new(|| Ok(1)),
    )));
    let mut s = Settings::default();
    s.aux_router.models_dir = aux_dir.path().display().to_string();
    s.aux_router.image = CUDA_TAG.into();
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    Fixture {
        state,
        podman,
        aux_dir,
    }
}

/// A row as it was stored before the hoist: its flags in `args`.
fn new_row(model_id: &str, image: Option<&str>, args: &[&str]) -> NewAuxModel {
    NewAuxModel {
        model_id: model_id.into(),
        gguf_path: EMBEDDER.into(),
        kind: AuxKind::Embed,
        pooling: Some("last".into()),
        ctx_size: Some(8192),
        ubatch_size: None,
        batch_size: None,
        mmproj_path: None,
        backend: None,
        threads: None,
        args: args.iter().map(|s| s.to_string()).collect(),
        idle_seconds: 0,
        enabled: true,
        image: image.map(String::from),
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    }
}

impl Fixture {
    async fn add(&self, model_id: &str, image: Option<&str>, args: &[&str]) -> i64 {
        self.insert(new_row(model_id, image, args)).await
    }

    async fn insert(&self, row: NewAuxModel) -> i64 {
        let id = store::insert_aux_model(&self.state.db, &row).await.unwrap();
        self.state.reload_snapshot().await.unwrap();
        id
    }

    /// Every `find` probe's argv.
    fn find_runs(&self) -> Vec<Vec<String>> {
        self.podman
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == "run" && c.iter().any(|a| a == "find"))
            .cloned()
            .collect()
    }

    fn row(&self, model_id: &str) -> AuxModel {
        self.state
            .snapshot()
            .aux_models
            .iter()
            .find(|m| m.model_id == model_id)
            .cloned()
            .unwrap()
    }

    /// The full `podman run` argv the row's container starts with.
    fn argv(&self, model_id: &str) -> Vec<String> {
        let snap = self.state.snapshot();
        let spec = model_runtime(&snap, Class::Aux, model_id)
            .unwrap()
            .preview_spec(
                &snap.settings.container_prefix,
                1,
                &snap.settings.aux_router.models_dir,
                Path::new("/nonexistent"),
            )
            .unwrap();
        podman_run_argv(&spec)
    }

    fn placement(&self, model_id: &str) -> Placement {
        self.state.snapshot().placement(Class::Aux, model_id)
    }

    fn calls(&self) -> usize {
        self.podman.calls.lock().unwrap().len()
    }

    fn finds(&self) -> usize {
        self.find_runs().len()
    }
}

fn only<'a>(notices: &'a [String], model_id: &str) -> &'a str {
    let named: Vec<&String> = notices
        .iter()
        .filter(|n| n.starts_with(&format!("aux model '{model_id}'")))
        .collect();
    assert_eq!(named.len(), 1, "one notice for '{model_id}': {notices:#?}");
    named[0]
}

/// The acceptance case of §6.1's tests: a CPU-only-image row with
/// `--n-gpu-layers 0 --threads 8 --ubatch-size 4096` loads with the fields
/// set and the args emptied, stored, idempotently.
#[tokio::test]
async fn a_cpu_image_row_moves_its_flags_into_its_fields_once() {
    let f = fixture().await;
    let id = f
        .add(
            "cpu-embed",
            Some(CPU_TAG),
            &[
                "--n-gpu-layers",
                "0",
                "--threads",
                "8",
                "--ubatch-size",
                "4096",
            ],
        )
        .await;
    assert_eq!(
        f.placement("cpu-embed"),
        Placement::Gpu,
        "before: the field decides"
    );

    let notices = hoist_aux_args(&f.state).await;
    let m = f.row("cpu-embed");
    assert!(m.args.is_empty(), "{:?}", m.args);
    assert_eq!(m.backend, Some(AuxBackend::Cpu));
    assert_eq!(m.threads, Some(8));
    assert_eq!(m.ubatch_size, Some(4096));
    assert_eq!(f.placement("cpu-embed"), Placement::Cpu);
    let stored = store::get_aux_model(&f.state.db, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&stored).unwrap(),
        serde_json::to_value(&m).unwrap(),
        "written back, not only folded in memory"
    );
    let notice = only(&notices, "cpu-embed");
    for part in [
        "moved `--n-gpu-layers 0`, `--threads 8`, `--ubatch-size 4096` from its extra args",
        "(backend cpu, threads 8, ubatch_size 4096)",
        &format!("Its image '{CPU_TAG}' has no GPU backend"),
        "serves under the GPU hold, and its container gets no GPU device.",
        "It stays on the CPU if its image later changes to a GPU build: set backend back to \
         empty to return it to the GPU.",
        // `--ubatch-size` renders ahead of the other two from the fields, and
        // the inherited run args lose the GPU passthrough.
        "replaced on this start rather than adopted",
    ] {
        assert!(notice.contains(part), "{part:?} in {notice}");
    }
    // The probe: a throwaway of the image's ID that never pulls, with no
    // network and no GPU.
    let runs = f.find_runs();
    assert_eq!(runs.len(), 1, "the external image's files were read once");
    let probe = &runs[0];
    let pair = |a: &str, b: &str| probe.windows(2).any(|w| w[0] == a && w[1] == b);
    assert!(pair("--pull", "never"), "{probe:?}");
    assert!(probe.iter().any(|a| a == "--network=none"), "{probe:?}");
    assert!(pair("-e", "NVIDIA_VISIBLE_DEVICES=void"), "{probe:?}");
    assert!(pair("find", CPU_ID), "{probe:?}");
    assert!(probe.ends_with(&["/", "-xdev", "-name", "libggml*"].map(String::from)));

    // Idempotent: nothing left to move, nothing said, podman not asked.
    let before = (
        serde_json::to_value(&m).unwrap(),
        f.argv("cpu-embed"),
        f.calls(),
    );
    let again = hoist_aux_args(&f.state).await;
    assert!(again.is_empty(), "{again:#?}");
    let m = f.row("cpu-embed");
    assert_eq!(
        (
            serde_json::to_value(&m).unwrap(),
            f.argv("cpu-embed"),
            f.calls()
        ),
        before
    );
}

/// An lmgw build says its backend in its label; no container runs to read it.
#[tokio::test]
async fn an_lmgw_build_is_judged_by_its_label() {
    let f = fixture().await;
    f.add("labelled", Some(LMGW_CPU_TAG), &["-ngl", "0"]).await;
    let notices = hoist_aux_args(&f.state).await;
    assert_eq!(f.row("labelled").backend, Some(AuxBackend::Cpu));
    assert!(only(&notices, "labelled").contains("dev.lmgw.backend label says cpu"));
    assert_eq!(f.finds(), 0);
}

/// A CUDA-image row keeps `-ngl 0` in its args and its GPU placement, and
/// its notice names the trade with the measured numbers. Its `-t 8` moves
/// anyway. Each start judges it again.
#[tokio::test]
async fn a_cuda_image_row_keeps_ngl_0_and_its_gpu_placement() {
    let f = fixture().await;
    // No image of its own: the class default, the CUDA image.
    f.add("cuda-embed", None, &["-ngl", "0", "-t", "8"]).await;

    let notices = hoist_aux_args(&f.state).await;
    let m = f.row("cuda-embed");
    assert_eq!(m.args, vec!["-ngl", "0"]);
    assert_eq!(m.backend, None);
    assert_eq!(m.threads, Some(8));
    assert_eq!(f.placement("cuda-embed"), Placement::Gpu);
    let notice = only(&notices, "cuda-embed");
    for part in [
        "moved `-t 8` from its extra args into its fields (threads 8)",
        "It keeps `-ngl 0` in its extra args and stays a GPU row",
        &format!("its image '{CUDA_TAG}' has a GPU backend (cuda)"),
        "~5.7 GiB of VRAM for compute buffers and 2.2 s per batch of 32 inputs",
        "0 VRAM and 50.6 s",
        "Setting backend: cpu is a deliberate choice",
    ] {
        assert!(notice.contains(part), "{part:?} in {notice}");
    }
    assert!(!notice.contains("has no GPU backend"), "{notice}");

    let argv = f.argv("cuda-embed");
    let again = hoist_aux_args(&f.state).await;
    let notice = only(&again, "cuda-embed");
    assert!(
        notice.starts_with("aux model 'cuda-embed' keeps `-ngl 0`"),
        "{notice}"
    );
    assert!(!notice.contains("moved"), "{notice}");
    assert_eq!(f.row("cuda-embed").args, vec!["-ngl", "0"]);
    assert_eq!(
        f.argv("cuda-embed"),
        argv,
        "the second start changes nothing"
    );
}

/// An image lmgw cannot read keeps the row's GPU placement, and the notice
/// says why: not on this machine, or files that do not tell (ik's single
/// `libggml.so`).
#[tokio::test]
async fn an_unknown_image_keeps_gpu_placement_and_says_why() {
    let f = fixture().await;
    f.add("absent", Some(ABSENT_TAG), &["--gpu-layers=0"]).await;
    f.add("ik", Some(IK_TAG), &["-ngl", "0"]).await;

    let notices = hoist_aux_args(&f.state).await;
    for (id, kept, why) in [
        ("absent", "--gpu-layers=0", "it is not on this machine"),
        (
            "ik",
            "-ngl 0",
            "its one libggml.so compiles its backends in",
        ),
    ] {
        let m = f.row(id);
        assert_eq!(m.backend, None, "{id}");
        assert_eq!(f.placement(id), Placement::Gpu, "{id}");
        let notice = only(&notices, id);
        assert!(
            notice.starts_with(&format!("aux model '{id}' keeps `{kept}`")),
            "{notice}"
        );
        assert!(
            notice.contains("lmgw cannot tell whether its image"),
            "{notice}"
        );
        assert!(notice.contains(why), "{notice}");
        assert!(notice.contains("The next start looks again"), "{notice}");
    }
    assert_eq!(f.row("absent").args, vec!["--gpu-layers=0"]);
}

/// Rows the hoist has nothing to move in keep their argv byte for byte, so
/// boot adopts their containers: freeform flags of their own, flags that
/// only look like one of the five, a layer count other than 0, and a
/// CUDA-image row whose only flag is the `-ngl 0` it keeps.
#[tokio::test]
async fn rows_with_nothing_to_move_keep_byte_identical_argv() {
    let f = fixture().await;
    let rows: [(&str, Option<&str>, &[&str]); 4] = [
        ("plain", None, &["--flash-attn", "on", "--no-mmproj"]),
        (
            "lookalikes",
            Some(CPU_TAG),
            &["-tb", "4", "--threads-batch", "4", "-np", "2"],
        ),
        ("ngl99", Some(CPU_TAG), &["-ngl", "99"]),
        ("cuda-kept", Some(CUDA_TAG), &["-ngl", "0"]),
    ];
    for (id, image, args) in rows {
        f.add(id, image, args).await;
    }
    let before: Vec<(Vec<String>, serde_json::Value)> = rows
        .iter()
        .map(|(id, ..)| (f.argv(id), serde_json::to_value(f.row(id)).unwrap()))
        .collect();

    let notices = hoist_aux_args(&f.state).await;
    for ((id, ..), (argv, row)) in rows.iter().zip(&before) {
        assert_eq!(&f.argv(id), argv, "{id}: argv byte-identical");
        assert_eq!(&serde_json::to_value(f.row(id)).unwrap(), row, "{id}");
    }
    // Only the kept `-ngl 0` is worth a word.
    assert_eq!(notices.len(), 1, "{notices:#?}");
    only(&notices, "cuda-kept");
}

/// A projector path that is not a file — a directory, as a hand-written
/// row may name — moves as written, and the row's existing finding reports
/// it. The in-container prefix is dropped as the chat hoist drops it; a path
/// outside `/models` stays in the args.
#[tokio::test]
async fn a_directory_projector_moves_as_written_and_is_flagged() {
    let f = fixture().await;
    let dir = "mradermacher/Qwen3-VL-Embedding-8B-GGUF";
    std::fs::create_dir_all(f.aux_dir.path().join(dir)).unwrap();
    f.add("vl-dir", None, &["--mmproj", dir]).await;
    f.add(
        "vl-prefixed",
        None,
        &["--mmproj", "/models/org/m/mmproj.gguf"],
    )
    .await;
    f.add("vl-outside", None, &["-mm", "/opt/proj.gguf"]).await;

    let notices = hoist_aux_args(&f.state).await;
    let m = f.row("vl-dir");
    assert_eq!(m.mmproj_path.as_deref(), Some(dir));
    assert!(m.args.is_empty());
    let notice = only(&notices, "vl-dir");
    assert!(notice.contains(&format!("(mmproj_path {dir})")), "{notice}");
    // Written relative, it was llama-server's working directory's; the field
    // renders it under /models, and the notice says so.
    assert!(
        notice.contains(&format!(
            "Its projector `{dir}` was read against llama-server's working directory and is \
             now read as /models/{dir}."
        )),
        "{notice}"
    );
    assert_eq!(
        f.row("vl-prefixed").mmproj_path.as_deref(),
        Some("org/m/mmproj.gguf")
    );
    let prefixed = only(&notices, "vl-prefixed");
    assert!(!prefixed.contains("now read as"), "{prefixed}");
    let outside = f.row("vl-outside");
    assert_eq!(outside.mmproj_path, None);
    assert_eq!(outside.args, vec!["-mm", "/opt/proj.gguf"]);

    let check = crate::common::check_wire(
        ops::local_model_check(&f.state, Some("vl-dir"), Some("aux"))
            .await
            .unwrap(),
    );
    let problems = check["models"][0]["problems"].to_string();
    assert!(
        problems.contains(&format!(
            "mmproj_path '{dir}' is not a file under the aux models dir"
        )),
        "{check}"
    );
}

/// A save that lands between the fold's read and its write wins: the write
/// compares the row's args and five fields, and the image and run args the
/// `-ngl 0` verdict was read by.
#[tokio::test]
async fn a_save_between_the_read_and_the_write_wins() {
    let f = fixture().await;
    let id = f.add("raced", Some(CPU_TAG), &["--threads", "8"]).await;
    let was = f.row("raced");
    let mut now = was.clone();
    now.hoist_args(&ImageGpu::CpuOnly("test".into()));
    let stored = || async {
        store::get_aux_model(&f.state.db, id)
            .await
            .unwrap()
            .unwrap()
    };

    let mut saved = new_row("raced", Some(CPU_TAG), &["--threads", "4"]);
    for (what, change) in [
        ("its args", None),
        ("its image", Some(CUDA_TAG)),
        ("its run args", Some("run args")),
    ] {
        saved.args = vec![
            "--threads".into(),
            if what == "its args" { "4" } else { "8" }.into(),
        ];
        saved.image = Some(
            if change == Some(CUDA_TAG) {
                CUDA_TAG
            } else {
                CPU_TAG
            }
            .into(),
        );
        saved.extra_run_args = (change == Some("run args")).then(|| vec!["--ipc=host".into()]);
        store::update_aux_model(&f.state.db, id, &saved)
            .await
            .unwrap();
        assert!(
            !store::store_aux_hoist(&f.state.db, &was, &now)
                .await
                .unwrap(),
            "a save of {what} in between wins"
        );
        assert_eq!(stored().await.threads, None, "{what}");
    }
    // The row as the fold read it: written.
    store::update_aux_model(
        &f.state.db,
        id,
        &new_row("raced", Some(CPU_TAG), &["--threads", "8"]),
    )
    .await
    .unwrap();
    assert!(store::store_aux_hoist(&f.state.db, &was, &now)
        .await
        .unwrap());
    let m = stored().await;
    assert_eq!((m.threads, m.args.len()), (Some(8), 0));
}

/// A podman that cannot list its images reads every image as unknown.
#[tokio::test]
async fn podman_images_failing_keeps_the_flag_and_says_why() {
    let f = fixture().await;
    f.podman.images_broken.store(true, Ordering::Relaxed);
    f.add("no-list", Some(CPU_TAG), &["-ngl", "0"]).await;
    let notices = hoist_aux_args(&f.state).await;
    assert_eq!(f.row("no-list").backend, None);
    assert_eq!(f.row("no-list").args, vec!["-ngl", "0"]);
    let notice = only(&notices, "no-list");
    assert!(
        notice.contains("podman could not list the local images"),
        "{notice}"
    );
    assert_eq!(f.finds(), 0);
}

/// Only a whole listing proves an image has no GPU backend. A listing that
/// exits 1 can still show a GPU library; one that exits 1 without one, or
/// fails, says nothing.
#[tokio::test]
async fn a_partial_or_failed_listing_never_proves_cpu_only() {
    let f = fixture().await;
    f.add("partial-gpu", Some(EXIT1_GPU_TAG), &["-ngl", "0"])
        .await;
    f.add("partial-cpu", Some(EXIT1_CPU_TAG), &["-ngl", "0"])
        .await;
    f.add("broken", Some(EXIT125_TAG), &["-ngl", "0"]).await;
    let notices = hoist_aux_args(&f.state).await;
    for (id, said) in [
        ("partial-gpu", "has a GPU backend (cuda)"),
        ("partial-cpu", "left some of it unread (exit 1"),
        ("broken", "listing its files failed (exit 125)"),
    ] {
        assert_eq!(f.row(id).backend, None, "{id}");
        assert_eq!(f.row(id).args, vec!["-ngl", "0"], "{id}");
        let notice = only(&notices, id);
        assert!(notice.contains(said), "{said:?} in {notice}");
    }
}

/// A listing that never ends is cut short at its timeout, its container is
/// removed by name, and the row keeps its flag.
#[tokio::test]
async fn a_listing_that_never_ends_is_cut_short() {
    let f = fixture().await;
    f.add("hangs", Some(HANG_TAG), &["-ngl", "0"]).await;
    // The fake's listing sleeps for an hour; a paused clock jumps to the
    // timeout instead of waiting it out.
    tokio::time::pause();
    let notices = hoist_aux_args(&f.state).await;
    tokio::time::resume();
    let notice = only(&notices, "hangs");
    assert!(
        notice.contains("listing its files did not finish within 30 s"),
        "{notice}"
    );
    assert_eq!(f.row("hangs").backend, None);
    let probe = &f.find_runs()[0];
    let name = &probe[probe.iter().position(|a| a == "--name").unwrap() + 1];
    let removed = f
        .podman
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c == &["rm", "-f", name.as_str()]);
    assert!(removed, "the timed-out probe {name} is removed");
}

/// Two tags of one image are one image: read once, by its ID.
#[tokio::test]
async fn one_image_under_two_tags_is_read_once() {
    let f = fixture().await;
    f.add("by-tag", Some(CPU_TAG), &["-ngl", "0"]).await;
    f.add("by-alias", Some(CPU_ALIAS), &["-ngl", "0"]).await;
    hoist_aux_args(&f.state).await;
    assert_eq!(f.row("by-tag").backend, Some(AuxBackend::Cpu));
    assert_eq!(f.row("by-alias").backend, Some(AuxBackend::Cpu));
    assert_eq!(f.finds(), 1);
}

/// A row that only loses a copy its field already rendered: stored without
/// it, its argv the same, its container adopted, and the notice says so.
#[tokio::test]
async fn a_dropped_only_row_is_adopted_and_says_so() {
    let f = fixture().await;
    let mut row = new_row("dropped", Some(CPU_TAG), &["-ngl", "0"]);
    row.backend = Some(AuxBackend::Cpu);
    f.insert(row).await;
    let argv = f.argv("dropped");
    let notices = hoist_aux_args(&f.state).await;
    assert!(f.row("dropped").args.is_empty());
    assert_eq!(f.argv("dropped"), argv);
    let notice = only(&notices, "dropped");
    assert!(
        notice.contains("dropped `-ngl 0` from its extra args"),
        "{notice}"
    );
    assert!(
        notice.contains("is unchanged, so a container of it still running from before is adopted"),
        "{notice}"
    );
    assert!(!notice.contains("replaced"), "{notice}");
    assert_eq!(f.finds(), 0, "the field decides; the image is not read");
}

/// CPU rows set up by hand: `--n-gpu-layers 0 --threads 16` with run args of
/// their own that pass no GPU. On an image without a GPU backend both move,
/// the fields render the same argv, and the container is adopted. On the
/// CUDA image `-ngl 0` stays, `--threads 16` now renders ahead of it (a
/// replacement), and the notice says the row computes on the CPU already.
/// Own run args that do pass a GPU are named.
#[tokio::test]
async fn cpu_rows_with_their_own_run_args() {
    let f = fixture().await;
    let cpu_row = |id: &str, image: Option<&str>, own: &[&str]| {
        let mut row = new_row(id, image, &["--n-gpu-layers", "0", "--threads", "16"]);
        row.extra_run_args = Some(own.iter().map(|s| s.to_string()).collect());
        row
    };
    let no_gpu = ["--security-opt", "label=disable"];
    f.insert(cpu_row("emb-cpu", Some(CPU_TAG), &no_gpu)).await;
    f.insert(cpu_row("emb-cpu-cuda", None, &no_gpu)).await;
    f.insert(cpu_row(
        "emb-cpu-gpu-args",
        Some(CPU_TAG),
        &["--device", "nvidia.com/gpu=all"],
    ))
    .await;
    let before = |id: &str| f.argv(id);
    let (cpu, cuda) = (before("emb-cpu"), before("emb-cpu-cuda"));

    let notices = hoist_aux_args(&f.state).await;

    let m = f.row("emb-cpu");
    assert_eq!((m.backend, m.threads), (Some(AuxBackend::Cpu), Some(16)));
    assert!(m.args.is_empty());
    assert_eq!(f.argv("emb-cpu"), cpu, "the same argv: adopted");
    let notice = only(&notices, "emb-cpu");
    for part in [
        "and its container gets no GPU device.",
        "is unchanged, so a container of it still running from before is adopted as it is.",
    ] {
        assert!(notice.contains(part), "{part:?} in {notice}");
    }
    assert!(!notice.contains("replaced"), "{notice}");

    let m = f.row("emb-cpu-cuda");
    assert_eq!((m.backend, m.threads), (None, Some(16)));
    assert_eq!(m.args, vec!["--n-gpu-layers", "0"]);
    assert_ne!(f.argv("emb-cpu-cuda"), cuda);
    let notice = only(&notices, "emb-cpu-cuda");
    for part in [
        "moved `--threads 16` from its extra args into its fields (threads 16).",
        "replaced on this start rather than adopted.",
        "It keeps `--n-gpu-layers 0` in its extra args and stays a GPU row",
        "but its run args pass its container no GPU device, so it computes on the CPU already",
    ] {
        assert!(notice.contains(part), "{part:?} in {notice}");
    }
    assert!(!notice.contains("5.7 GiB"), "no device, no trade: {notice}");

    let notice = only(&notices, "emb-cpu-gpu-args");
    assert!(
        notice.contains("but its own run args still hand its container a GPU"),
        "{notice}"
    );
}
