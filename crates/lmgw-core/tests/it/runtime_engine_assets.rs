//! Engines as release assets (Windows design §4.2), through the registry
//! and the real native launcher: a pinned build is downloaded from a local
//! release server, checked against its SHA-256, unpacked and started; a
//! wrong digest installs nothing; a CUDA build whose start ends in "no
//! kernel image is available" is marked and the registry's start goes on
//! with the engine's Vulkan build, prepared again outside the load timeout;
//! only an exit of the engine's own, not with code 0, with ggml's line marks
//! a build, and the owner can clear the mark. The engine is
//! `lmgw-fake-engine` zipped as each build's `llama-server`.
//!
//! The live tests at the end use the real pins and skip unless their
//! variables are set (each one's doc says which).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lmgw_core::config::{AudioModel, AudioSettings, ImageModel, LlamaParams};
use lmgw_core::erased::Erased;
use lmgw_core::runtime::argv::LlamaArgs;
use lmgw_core::runtime::descriptor::ModelRuntime;
use lmgw_core::runtime::engine_assets::choice::{Card, GpuFacts};
use lmgw_core::runtime::engine_assets::coverage::Version;
use lmgw_core::runtime::engine_assets::pins::{
    Asset, BuildPin, CudaFile, CudaPin, EnginePins, PlatformPins, SinglePin, Variant,
};
use lmgw_core::runtime::engine_assets::{self, fallback, install, Engines};
use lmgw_core::runtime::launcher::{Native, Unwatched};
use lmgw_core::runtime::registry::{
    ephemeral_port, AcquireSpec, CmdOutput, CommandRunner, Registry, RuntimeState,
};
use lmgw_core::runtime::{container_name, Class};
use sha2::Digest;

const FAKE_ENGINE: &str = env!("CARGO_BIN_EXE_lmgw-fake-engine");

struct NoPodman;

impl CommandRunner for NoPodman {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
    ) -> Erased<'a, std::io::Result<CmdOutput>> {
        panic!(
            "the registry shelled `{program} {}` past the native launcher",
            args.join(" ")
        )
    }
}

/// A registry over `native`. Its ports come from `LMGW_LIVE_PORT_RANGE`
/// (`18100-18199`) when that is set — a shared test machine gives each user
/// a range — and are ephemeral otherwise.
fn registry(native: Native) -> Arc<Registry> {
    registry_over(Arc::new(native))
}

fn registry_over(native: Arc<Native>) -> Arc<Registry> {
    Arc::new(Registry::with_launcher(
        native,
        Arc::new(NoPodman),
        reqwest::Client::new(),
        Arc::new(port),
    ))
}

fn port() -> std::io::Result<u16> {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let Some((lo, hi)) = std::env::var("LMGW_LIVE_PORT_RANGE").ok().and_then(|r| {
        let (a, b) = r.split_once('-')?;
        Some((a.trim().parse::<u16>().ok()?, b.trim().parse::<u16>().ok()?))
    }) else {
        return ephemeral_port();
    };
    let span = hi.saturating_sub(lo) + 1;
    for _ in 0..span {
        let p = lo + NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % span;
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return Ok(p);
        }
    }
    Err(std::io::Error::other(format!("no free port in {lo}-{hi}")))
}

fn chat_runtime(model_id: &str, gguf: &str, params: LlamaParams) -> ModelRuntime {
    chat_runtime_with(model_id, gguf, params, &[])
}

/// [`chat_runtime`] with freeform args (the fake engine's `--fake-*`).
fn chat_runtime_with(
    model_id: &str,
    gguf: &str,
    params: LlamaParams,
    args: &[&str],
) -> ModelRuntime {
    ModelRuntime {
        class: Class::Chat,
        model_id: model_id.into(),
        image: "unused-by-the-native-launcher".into(),
        extra_run_args: vec![],
        idle_seconds: 0,
        warm_start: false,
        enabled: true,
        llama: Some(LlamaArgs::Chat {
            gguf_path: gguf.into(),
            params: Box::new(params),
            args: args.iter().map(|a| a.to_string()).collect(),
        }),
        audio: None,
        audio_settings: None,
        image_model: None,
        sdcpp_caps: None,
        rung: None,
    }
}

fn spec<'a>(
    rt: &'a ModelRuntime,
    models: &'a str,
    data: &'a Path,
    load: Duration,
) -> AcquireSpec<'a> {
    AcquireSpec {
        runtime: rt,
        container_prefix: "lmgw",
        models_dir: models,
        data_dir: data,
        may_write_models_dir: false,
        load_timeout: load,
        stop_timeout: Duration::from_secs(30),
    }
}

/// A zip of `entries`, each executable.
fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);
    for (name, bytes) in entries {
        w.start_file(*name, opts).unwrap();
        w.write_all(bytes).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn asset(name: &str, bytes: &[u8]) -> Asset {
    Asset {
        name: name.into(),
        sha256: hex::encode(sha2::Sha256::digest(bytes)),
        size: bytes.len() as u64,
    }
}

/// A release server: `GET /<repo>/releases/download/<tag>/<name>` answers
/// the bytes registered under that path; every request is counted.
struct Server {
    base: String,
    hits: Arc<std::sync::Mutex<Vec<String>>>,
    _task: tokio::task::JoinHandle<()>,
}

async fn serve(files: HashMap<String, Vec<u8>>) -> Server {
    serve_slow(files, HashMap::new()).await
}

/// [`serve`], where each path in `delays` answers after its delay.
async fn serve_slow(files: HashMap<String, Vec<u8>>, delays: HashMap<String, Duration>) -> Server {
    let files = Arc::new(files);
    let delays = Arc::new(delays);
    let hits = Arc::new(std::sync::Mutex::new(Vec::new()));
    let h = hits.clone();
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let files = files.clone();
        let delays = delays.clone();
        let hits = h.clone();
        async move {
            hits.lock().unwrap().push(uri.path().to_string());
            if let Some(d) = delays.get(uri.path()) {
                tokio::time::sleep(*d).await;
            }
            match files.get(uri.path()) {
                Some(b) => (axum::http::StatusCode::OK, b.clone()),
                None => (axum::http::StatusCode::NOT_FOUND, Vec::new()),
            }
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(l, app).await.unwrap();
    });
    Server {
        base,
        hits,
        _task: task,
    }
}

const REPO: &str = "example/llama";
const TAG: &str = "b1";

/// The pins of a llama.cpp-like release with a CUDA 12 build covering
/// `sm_86`, a Vulkan and a CPU build, each zip holding the fake engine
/// (the CUDA one also `fake-no-kernel-image` when `cuda_lacks_kernels`),
/// and the server's files for them.
fn release(cuda_lacks_kernels: bool) -> (PlatformPins, HashMap<String, Vec<u8>>) {
    let exe = engine_assets::executable(Class::Chat);
    let fake = std::fs::read(FAKE_ENGINE).unwrap();
    let mut cuda_entries: Vec<(&str, &[u8])> = vec![(&exe, &fake)];
    if cuda_lacks_kernels {
        cuda_entries.push(("fake-no-kernel-image", b""));
    }
    let zips = [
        ("llama-b1-cuda-12.4.zip", zip_of(&cuda_entries)),
        (
            "cudart-12.4.zip",
            zip_of(&[("cudart64_12.dll", b"not a dll")]),
        ),
        ("llama-b1-vulkan.zip", zip_of(&[(&exe, &fake)])),
        ("llama-b1-cpu.zip", zip_of(&[(&exe, &fake)])),
    ];
    let a = |n: &str| asset(n, &zips.iter().find(|(z, _)| *z == n).unwrap().1);
    let engine = |builds| EnginePins {
        repo: REPO.into(),
        tag: TAG.into(),
        published: "2026-11-10".into(),
        builds,
    };
    let llama = engine(vec![
        BuildPin {
            variant: Variant::Cuda12,
            asset: a("llama-b1-cuda-12.4.zip"),
            cudart: Some(a("cudart-12.4.zip")),
            cuda: Some(CudaPin {
                toolkit: "12.4".into(),
                files: vec![CudaFile {
                    file: "ggml-cuda.dll".into(),
                    sass: vec!["86".into()],
                    ptx: vec![],
                    ptx_isa: None,
                }],
            }),
        },
        BuildPin {
            variant: Variant::Vulkan,
            asset: a("llama-b1-vulkan.zip"),
            cudart: None,
            cuda: None,
        },
        BuildPin {
            variant: Variant::Cpu,
            asset: a("llama-b1-cpu.zip"),
            cudart: None,
            cuda: None,
        },
    ]);
    let cpu_only = engine(vec![BuildPin {
        variant: Variant::Cpu,
        asset: a("llama-b1-cpu.zip"),
        cudart: None,
        cuda: None,
    }]);
    let pins = PlatformPins {
        llama,
        sdcpp: cpu_only.clone(),
        audio: cpu_only,
        espeak: SinglePin {
            repo: "example/espeak".into(),
            tag: "1".into(),
            published: "2026-11-10".into(),
            asset: asset("espeak.msi", b"unused"),
        },
    };
    let files = zips
        .into_iter()
        .map(|(n, b)| (format!("/{REPO}/releases/download/{TAG}/{n}"), b))
        .collect();
    (pins, files)
}

/// An RTX-30-class card on a CUDA 12.6 driver, with a Vulkan loader.
fn ampere() -> engine_assets::FactsSource {
    Arc::new(|| GpuFacts {
        cards: vec![Card {
            index: 0,
            name: "Example GPU".into(),
            cc: Some(Version::new(8, 6)),
        }],
        driver_cuda: Some(Version::new(12, 6)),
        nvml_error: None,
        vulkan_loader: true,
    })
}

#[tokio::test]
async fn a_pinned_build_is_downloaded_verified_unpacked_and_started() {
    let (pins, files) = release(false);
    let server = serve(files).await;
    let data = tempfile::tempdir().unwrap();
    let root = engine_assets::engines_root(data.path());
    let engines = Engines::with(root.clone(), &server.base, Some(pins), ampere());
    let reg = registry(Native::with_engines(engines).unwrap());
    let models = data.path().join("models");
    std::fs::create_dir_all(&models).unwrap();
    let models = models.display().to_string();
    let rt = chat_runtime("pinned-chat", "pinned-chat.gguf", LlamaParams::default());
    let name = container_name("lmgw", Class::Chat, "pinned-chat");

    let guard = reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(30)))
        .await
        .unwrap();
    assert_eq!(reg.list()[0].state, RuntimeState::Ready);
    let build = root.join("llama").join("b1-cuda12");
    assert!(build.join(install::MARKER).is_file());
    assert!(
        build.join("cudart64_12.dll").is_file(),
        "cudart beside the engine"
    );
    // The download dir is gone, and nothing ran from it.
    assert!(!root.join(install::DOWNLOADS_DIR).join("b1-cuda12").exists());
    let log = reg.logs_tail(&name, 50).await.unwrap();
    let cwd = log
        .lines()
        .find_map(|l| l.strip_prefix("fake engine: cwd "))
        .unwrap_or_default();
    assert!(cwd.ends_with("b1-cuda12"), "{log}");
    drop(guard);
    reg.stop(Class::Chat, "pinned-chat", false).await.unwrap();

    // A second start uses the install; nothing is downloaded again.
    let downloads = server.hits.lock().unwrap().len();
    assert_eq!(downloads, 2, "the build and its cudart");
    let guard = reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(30)))
        .await
        .unwrap();
    assert_eq!(server.hits.lock().unwrap().len(), downloads);
    drop(guard);
    reg.stop(Class::Chat, "pinned-chat", false).await.unwrap();
}

#[tokio::test]
async fn a_download_with_the_wrong_digest_installs_nothing() {
    let (mut pins, files) = release(false);
    pins.llama.builds.retain(|b| b.variant == Variant::Cpu);
    pins.llama.builds[0].asset.sha256 = "0".repeat(64);
    let server = serve(files).await;
    let data = tempfile::tempdir().unwrap();
    let root = engine_assets::engines_root(data.path());
    let engines = Engines::with(root.clone(), &server.base, Some(pins), ampere());
    let reg = registry(Native::with_engines(engines).unwrap());
    let models = data.path().display().to_string();
    let rt = chat_runtime("bad-digest", "x.gguf", LlamaParams::default());

    let err = reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(30)))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("SHA-256"), "{err}");
    assert!(err.contains("nothing of it is used"), "{err}");
    assert!(!root.join("llama").join("b1-cpu").exists());
    let leftovers: Vec<_> = walk(&root.join(install::DOWNLOADS_DIR))
        .into_iter()
        .filter(|p| p.is_file())
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test]
async fn a_cuda_build_without_a_kernel_image_falls_back_to_vulkan() {
    let (pins, files) = release(true);
    let server = serve(files).await;
    let data = tempfile::tempdir().unwrap();
    let root = engine_assets::engines_root(data.path());
    let engines = Engines::with(root.clone(), &server.base, Some(pins.clone()), ampere());
    let reg = registry(Native::with_engines(engines).unwrap());
    let models = data.path().display().to_string();
    let rt = chat_runtime("fallback-chat", "x.gguf", LlamaParams::default());
    let name = container_name("lmgw", Class::Chat, "fallback-chat");

    let guard = reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(30)))
        .await
        .unwrap();
    let entry = reg.list().remove(0);
    assert_eq!(entry.state, RuntimeState::Ready);
    // The start's warnings name the fallback and why.
    assert!(
        entry.warnings.iter().any(|w| w.contains("no kernel image")
            && w.contains("b1-cuda12")
            && w.contains("Vulkan")),
        "{:?}",
        entry.warnings
    );
    let cuda = root.join("llama").join("b1-cuda12");
    let mark = fallback::marked(&cuda).expect("the CUDA build is marked");
    assert!(
        mark.line
            .starts_with("[E] ggml: CUDA error: no kernel image"),
        "{mark:?}"
    );
    assert_eq!(mark.exit, "exited with code 1");
    let log = reg.logs_tail(&name, 50).await.unwrap();
    assert!(
        log.lines()
            .any(|l| l.starts_with("fake engine: cwd ") && l.ends_with("b1-vulkan")),
        "{log}"
    );
    drop(guard);
    reg.stop(Class::Chat, "fallback-chat", false).await.unwrap();

    // The next start goes to the Vulkan build directly.
    let t0 = Instant::now();
    let guard = reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(30)))
        .await
        .unwrap();
    let log = reg.logs_tail(&name, 50).await.unwrap();
    assert!(!log.contains("no kernel image"), "{log}");
    assert!(t0.elapsed() < Duration::from_secs(20));
    drop(guard);
    reg.stop(Class::Chat, "fallback-chat", false).await.unwrap();

    // The owner clears the mark: the next start tries the CUDA build again
    // (which, having no kernels, falls back once more and is marked again).
    let owner = Engines::with(root.clone(), &server.base, Some(pins), ampere());
    let said = owner.clear_fallback("llama", "cuda12").unwrap();
    assert!(said.contains("cleared"), "{said}");
    assert_eq!(fallback::marked(&cuda), None);
    let again = owner.clear_fallback("llama", "cuda12").unwrap_err();
    assert!(again.contains("has no"), "{again}");
    assert!(owner.clear_fallback("llama", "cuda14").is_err());
    let guard = reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(30)))
        .await
        .unwrap();
    assert!(
        fallback::marked(&cuda).is_some(),
        "tried again, marked again"
    );
    drop(guard);
    reg.stop(Class::Chat, "fallback-chat", false).await.unwrap();
}

/// The fallback's Vulkan build is installed in a second `prepare`, outside
/// the load timeout: its download takes longer than the whole budget, and
/// the start still ends ready.
#[tokio::test]
async fn the_fallbacks_download_is_outside_the_load_timeout() {
    let (pins, files) = release(true);
    let slow = Duration::from_secs(5);
    let server = serve_slow(
        files,
        HashMap::from([(
            format!("/{REPO}/releases/download/{TAG}/llama-b1-vulkan.zip"),
            slow,
        )]),
    )
    .await;
    let data = tempfile::tempdir().unwrap();
    let root = engine_assets::engines_root(data.path());
    let engines = Engines::with(root.clone(), &server.base, Some(pins), ampere());
    let reg = registry(Native::with_engines(engines).unwrap());
    let models = data.path().display().to_string();
    let rt = chat_runtime("slow-fallback", "x.gguf", LlamaParams::default());
    let load = Duration::from_secs(4);

    let t0 = Instant::now();
    let guard = match reg.acquire(&spec(&rt, &models, data.path(), load)).await {
        Ok(g) => g,
        Err(e) => panic!("{e}"),
    };
    assert!(t0.elapsed() > slow, "the Vulkan download was waited for");
    assert!(root
        .join("llama")
        .join("b1-vulkan")
        .join(install::MARKER)
        .is_file());
    drop(guard);
    reg.stop(Class::Chat, "slow-fallback", false).await.unwrap();
}

/// Only an exit of the engine's own, not with code 0, with ggml's CUDA
/// error line marks a build: not one lmgw asked for, not a clean exit, not
/// the words anywhere else.
#[tokio::test]
async fn only_an_unrequested_failed_exit_with_ggmls_line_marks_a_build() {
    let (pins, files) = release(false);
    let server = serve(files).await;
    let data = tempfile::tempdir().unwrap();
    let root = engine_assets::engines_root(data.path());
    let engines = Engines::with(root.clone(), &server.base, Some(pins), ampere());
    let reg = registry(Native::with_engines(engines).unwrap());
    let models = data.path().display().to_string();
    let cuda = root.join("llama").join("b1-cuda12");
    const LINE: &str = "CUDA error: no kernel image is available for execution on the device";

    // lmgw stops it (the idle reaper's, a user's stop): never a mark.
    let rt = chat_runtime_with(
        "stopped",
        "x.gguf",
        LlamaParams::default(),
        &["--fake-say", LINE],
    );
    let guard = reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(30)))
        .await
        .unwrap();
    drop(guard);
    reg.stop(Class::Chat, "stopped", false).await.unwrap();

    // It ends with code 0 while loading: a failed start, no mark.
    let rt = chat_runtime_with(
        "clean",
        "x.gguf",
        LlamaParams::default(),
        &["--fake-say", LINE, "--fake-exit", "0"],
    );
    let err = reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(30)))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("exited with code 0"), "{err}");
    assert!(!err.contains("fallback"), "{err}");

    // The words, but not ggml's line.
    let rt = chat_runtime_with(
        "words",
        "x.gguf",
        LlamaParams::default(),
        &[
            "--fake-say",
            "prompt: what does \"no kernel image is available\" mean?",
            "--fake-exit",
            "1",
        ],
    );
    let err = reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(30)))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("exited with code 1"), "{err}");

    // A new start of the engine looks at every earlier child: none marks.
    let rt = chat_runtime("later", "x.gguf", LlamaParams::default());
    let guard = reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(30)))
        .await
        .unwrap();
    assert_eq!(fallback::marked(&cuda), None);
    let log = reg
        .logs_tail(&container_name("lmgw", Class::Chat, "later"), 50)
        .await
        .unwrap();
    assert!(log.contains("b1-cuda12"), "{log}");
    drop(guard);
    reg.stop(Class::Chat, "later", false).await.unwrap();
}

/// A CUDA build that loads and then ends at its first request with ggml's
/// line (audio.cpp's way on a GTX 970, cc 5.2) is marked at the next start of its
/// engine, which runs the Vulkan build.
#[tokio::test]
async fn a_cuda_build_that_fails_after_ready_is_marked_at_the_next_start() {
    let (pins, files) = release(false);
    let server = serve(files).await;
    let data = tempfile::tempdir().unwrap();
    let root = engine_assets::engines_root(data.path());
    let engines = Engines::with(root.clone(), &server.base, Some(pins), ampere());
    let reg = registry(Native::with_engines(engines).unwrap());
    let models = data.path().display().to_string();
    let rt = chat_runtime_with(
        "late",
        "x.gguf",
        LlamaParams::default(),
        &["--fake-compute-fails"],
    );
    let name = container_name("lmgw", Class::Chat, "late");
    let cuda = root.join("llama").join("b1-cuda12");

    let guard = reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(30)))
        .await
        .unwrap();
    let r = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", guard.endpoint()))
        .body("{}")
        .send()
        .await;
    assert!(r.is_err() || !r.unwrap().status().is_success());
    drop(guard);
    // The engine is gone on its own before this stop asks anything.
    let t = Instant::now();
    while !reg
        .logs_tail(&name, 50)
        .await
        .unwrap()
        .contains("current device")
        && t.elapsed() < Duration::from_secs(10)
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    reg.stop(Class::Chat, "late", false).await.unwrap();
    assert_eq!(fallback::marked(&cuda), None, "not until the next start");

    let guard = reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(30)))
        .await
        .unwrap();
    let mark = fallback::marked(&cuda).expect("marked at the next start");
    assert!(
        mark.line.starts_with("CUDA error: no kernel image"),
        "{mark:?}"
    );
    assert_eq!(mark.exit, "exited with code 1");
    let log = reg.logs_tail(&name, 50).await.unwrap();
    assert!(
        log.lines()
            .any(|l| l.starts_with("fake engine: cwd ") && l.ends_with("b1-vulkan")),
        "{log}"
    );
    drop(guard);
    reg.stop(Class::Chat, "late", false).await.unwrap();
}

/// A row that names a card and runs a pinned Vulkan build starts with a
/// warning: the selection reaches CUDA only (`CUDA_VISIBLE_DEVICES`).
#[tokio::test]
async fn a_named_card_on_the_vulkan_build_is_a_start_warning() {
    let (pins, files) = release(false);
    let server = serve(files).await;
    let data = tempfile::tempdir().unwrap();
    let root = engine_assets::engines_root(data.path());
    // No NVIDIA card NVML can read, a Vulkan loader: the Vulkan build.
    let facts: engine_assets::FactsSource = Arc::new(|| GpuFacts {
        vulkan_loader: true,
        ..GpuFacts::default()
    });
    let engines = Engines::with(root.clone(), &server.base, Some(pins), facts);
    let reg = registry(Native::with_engines(engines).unwrap());
    let models = data.path().display().to_string();
    let rt = ModelRuntime {
        extra_run_args: vec!["--device".into(), "nvidia.com/gpu=0".into()],
        ..chat_runtime("named-card", "x.gguf", LlamaParams::default())
    };
    let guard = reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(30)))
        .await
        .unwrap();
    let entry = reg.list().remove(0);
    assert!(
        entry.warnings.iter().any(|w| w.contains("card 0")
            && w.contains("does not reach the Vulkan build yet")
            && w.contains("b1-vulkan")),
        "{:?}",
        entry.warnings
    );
    drop(guard);
    reg.stop(Class::Chat, "named-card", false).await.unwrap();
}

/// An engine process removed while its model loads ends the start at once
/// with that sentence, not at the load timeout.
#[tokio::test]
async fn an_engine_removed_while_loading_ends_the_start_at_once() {
    use lmgw_core::runtime::launcher::Launcher;
    let (pins, files) = release(false);
    let server = serve(files).await;
    let data = tempfile::tempdir().unwrap();
    let root = engine_assets::engines_root(data.path());
    let engines = Engines::with(root.clone(), &server.base, Some(pins), ampere());
    let native = Arc::new(Native::with_engines(engines).unwrap());
    let reg = registry_over(native.clone());
    let models = data.path().display().to_string();
    let rt = chat_runtime_with(
        "removed",
        "x.gguf",
        LlamaParams::default(),
        &["--fake-load-ms", "120000"],
    );
    let name = container_name("lmgw", Class::Chat, "removed");

    let remover = {
        let (native, name) = (native.clone(), name.clone());
        tokio::spawn(async move {
            // Until the engine runs (its install included), then remove it.
            let t = Instant::now();
            while native.logs_tail(&name, 10).await.is_err()
                && t.elapsed() < Duration::from_secs(20)
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            native.remove(&name).await
        })
    };
    let t0 = Instant::now();
    let err = reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(120)))
        .await
        .unwrap_err()
        .to_string();
    remover.await.unwrap().unwrap();
    // The exit check sees either the table without the child (it was
    // removed) or the killed child before its removal: both end the start.
    assert!(
        err.contains("removed while the model was loading")
            || err.contains("while the model was loading"),
        "{err}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(30),
        "ended by the exit check, not the 120 s load timeout: {:?}",
        t0.elapsed()
    );
}

/// Another start's `prepare` that judges a loading CUDA child first (and
/// marks its build) leaves that child's own start its fallback: the verdict
/// is kept with the child, not used up by whoever looked first.
#[tokio::test]
async fn another_starts_look_at_a_failed_child_leaves_its_start_the_fallback() {
    use lmgw_core::runtime::launcher::Launcher;
    let (pins, files) = release(true);
    let server = serve(files).await;
    let data = tempfile::tempdir().unwrap();
    let root = engine_assets::engines_root(data.path());
    let engines = Engines::with(root.clone(), &server.base, Some(pins), ampere());
    let native = Arc::new(Native::with_engines(engines).unwrap());
    let reg = registry_over(native.clone());
    let models = data.path().display().to_string();
    let first_name = container_name("lmgw", Class::Chat, "first");

    let first = {
        let (reg, models, data) = (reg.clone(), models.clone(), data.path().to_path_buf());
        tokio::spawn(async move {
            let rt = chat_runtime("first", "x.gguf", LlamaParams::default());
            reg.acquire(&spec(&rt, &models, &data, Duration::from_secs(30)))
                .await
                .map(drop)
                .map_err(|e| e.to_string())
        })
    };
    // Until the first start's CUDA child has ended on ggml's line, which
    // its readiness poll looks at only every 2 s.
    let t = Instant::now();
    loop {
        let log = native.logs_tail(&first_name, 50).await.unwrap_or_default();
        let ended = native
            .host_pids(std::slice::from_ref(&first_name))
            .await
            .unwrap()
            .is_empty();
        if log.contains("no kernel image") && ended {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "{log}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let rt = chat_runtime("second", "x.gguf", LlamaParams::default());
    let second = reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(30)))
        .await
        .unwrap();
    first.await.unwrap().unwrap();
    let views = reg.list();
    let warnings = |id: &str| {
        views
            .iter()
            .find(|v| v.model_id == id)
            .map(|v| v.warnings.clone())
            .unwrap()
    };
    assert!(
        warnings("first")
            .iter()
            .any(|w| w.contains("no kernel image") && w.contains("Vulkan")),
        "{:?}",
        warnings("first")
    );
    // The second start found the build marked and ran Vulkan directly.
    assert!(
        !warnings("second")
            .iter()
            .any(|w| w.contains("no kernel image")),
        "{:?}",
        warnings("second")
    );
    let log = native.logs_tail(&first_name, 50).await.unwrap();
    assert!(
        log.lines()
            .any(|l| l.starts_with("fake engine: cwd ") && l.ends_with("b1-vulkan")),
        "{log}"
    );
    drop(second);
    for id in ["first", "second"] {
        reg.stop(Class::Chat, id, false).await.unwrap();
    }
}

/// A stop while the fallback's Vulkan build downloads ends the start: the
/// download stops, nothing is launched, and the CUDA attempt's log stays.
#[tokio::test]
async fn a_stop_during_the_fallbacks_download_launches_nothing() {
    use lmgw_core::runtime::launcher::Launcher;
    let (pins, files) = release(true);
    let slow = Duration::from_secs(10);
    let vulkan = format!("/{REPO}/releases/download/{TAG}/llama-b1-vulkan.zip");
    let server = serve_slow(files, HashMap::from([(vulkan.clone(), slow)])).await;
    let data = tempfile::tempdir().unwrap();
    let root = engine_assets::engines_root(data.path());
    let engines = Engines::with(root.clone(), &server.base, Some(pins), ampere());
    let native = Arc::new(Native::with_engines(engines).unwrap());
    let reg = registry_over(native.clone());
    let models = data.path().display().to_string();
    let name = container_name("lmgw", Class::Chat, "stopped-fallback");

    let t0 = Instant::now();
    let start = {
        let (reg, models, data) = (reg.clone(), models.clone(), data.path().to_path_buf());
        tokio::spawn(async move {
            let rt = chat_runtime("stopped-fallback", "x.gguf", LlamaParams::default());
            reg.acquire(&spec(&rt, &models, &data, Duration::from_secs(30)))
                .await
                .map(drop)
                .map_err(|e| e.to_string())
        })
    };
    let t = Instant::now();
    while !server.hits.lock().unwrap().contains(&vulkan) {
        assert!(t.elapsed() < Duration::from_secs(20), "no Vulkan download");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    reg.stop(Class::Chat, "stopped-fallback", false)
        .await
        .unwrap();
    let err = start.await.unwrap().unwrap_err();
    assert!(err.contains("superseded by a stop"), "{err}");
    assert!(t0.elapsed() < slow, "the download was not waited for");
    let log = native.logs_tail(&name, 50).await.unwrap();
    assert!(log.contains("no kernel image"), "{log}");
    assert!(!log.contains("b1-vulkan"), "{log}");
    assert!(native
        .host_pids(std::slice::from_ref(&name))
        .await
        .unwrap()
        .is_empty());
    assert!(!root
        .join("llama")
        .join("b1-vulkan")
        .join(install::MARKER)
        .exists());
    assert!(reg.list().is_empty());
}

/// A wait for another lmgw process's install is on the starting entry.
#[tokio::test]
async fn a_wait_for_another_processs_install_is_on_the_entry() {
    let (pins, files) = release(false);
    let server = serve(files).await;
    let data = tempfile::tempdir().unwrap();
    let root = engine_assets::engines_root(data.path());
    let engines = Engines::with(root.clone(), &server.base, Some(pins), ampere());
    let reg = registry(Native::with_engines(engines).unwrap());
    let models = data.path().display().to_string();
    // The other process: the build's lock, held on a file of its own.
    std::fs::create_dir_all(root.join("llama")).unwrap();
    let other = std::fs::File::create(root.join("llama").join(".b1-cuda12.lock")).unwrap();
    other.lock().unwrap();

    let start = {
        let (reg, models, data) = (reg.clone(), models.clone(), data.path().to_path_buf());
        tokio::spawn(async move {
            let rt = chat_runtime("waits", "x.gguf", LlamaParams::default());
            reg.acquire(&spec(&rt, &models, &data, Duration::from_secs(30)))
                .await
                .map(drop)
                .map_err(|e| e.to_string())
        })
    };
    let t = Instant::now();
    loop {
        let warnings: Vec<String> = reg.list().into_iter().flat_map(|v| v.warnings).collect();
        if warnings.iter().any(|w| {
            w.starts_with(
                "waiting for another lmgw process to finish installing llama.cpp b1 cuda12",
            )
        }) {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "{warnings:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(reg.list()[0].state, RuntimeState::Starting);
    drop(other);
    start.await.unwrap().unwrap();
    reg.stop(Class::Chat, "waits", false).await.unwrap();
}

/// A failed eSpeak NG install is remembered: the next audio start within
/// the interval is told at once, without a download, and both say when it
/// is tried again.
#[tokio::test]
async fn a_failed_espeak_install_is_not_tried_again_at_once() {
    let (pins, files) = release(false);
    let server = serve(files).await;
    let data = tempfile::tempdir().unwrap();
    let root = engine_assets::engines_root(data.path());
    let engines = Engines::with(root, &server.base, Some(pins), ampere());
    let first = engines.ensure_espeak(&Unwatched).await.unwrap_err();
    assert!(first.contains("404"), "{first}");
    let minutes = engine_assets::ESPEAK_RETRY_AFTER.as_secs() / 60;
    let retry = format!("first audio start {minutes} min after the failure");
    assert!(first.contains(&retry), "{first}");
    let second = engines.ensure_espeak(&Unwatched).await.unwrap_err();
    assert!(second.contains("not tried again"), "{second}");
    assert!(second.contains(&retry), "{second}");
    let asked = server
        .hits
        .lock()
        .unwrap()
        .iter()
        .filter(|h| h.ends_with("espeak.msi"))
        .count();
    assert_eq!(asked, 1);
}

/// The Windows tab's answer, from a pinned release on a test machine: the
/// choice, each build's state, a mark with its reason, eSpeak NG's row.
#[tokio::test]
async fn the_windows_tabs_answer_says_the_choice_the_builds_and_a_mark() {
    use lmgw_core::runtime::engine_assets::{releases::ReleaseObservation, view};
    let (pins, files) = release(false);
    let server = serve(files).await;
    let data = tempfile::tempdir().unwrap();
    let root = engine_assets::engines_root(data.path());
    let engines = Engines::with(root.clone(), &server.base, Some(pins), ampere());
    let facts = engines.facts().await;
    let observed = std::collections::BTreeMap::from([(
        "llama".to_string(),
        ReleaseObservation {
            repo: REPO.into(),
            pinned_tag: TAG.into(),
            checked_at: "2026-11-10T12:00:00Z".into(),
            latest_tag: Some("b2".into()),
            latest_published: Some("2026-11-11T00:00:00Z".into()),
            update_available: true,
            error: None,
        },
    )]);
    let r = view::response(
        &engines,
        &facts,
        true,
        &observed,
        Some("2026-11-10T12:00:00Z".into()),
    );
    assert!(r.native);
    assert_eq!(r.machine.cards, ["Example GPU (8.6)"]);
    let llama = r.engines.iter().find(|e| e.engine == "llama").unwrap();
    assert_eq!(llama.chosen, "cuda12", "{}", llama.chosen_why);
    assert!(llama
        .builds
        .iter()
        .all(|b| !b.installed && b.no_kernel_image.is_none()));
    assert!(llama.release.as_ref().unwrap().update_available);
    assert!(r.espeak.as_ref().is_some_and(|e| !e.installed));

    // Installed and marked: the tab says both, with the mark's reason, and
    // the choice moves to Vulkan.
    engines.ensure(Class::Chat, &Unwatched).await.unwrap();
    let cuda = root.join("llama").join("b1-cuda12");
    fallback::mark(
        &cuda,
        &fallback::mark_now(
            "exited with code 1",
            "CUDA error: no kernel image is available",
        ),
    )
    .unwrap();
    let r = view::response(&engines, &facts, true, &observed, None);
    let llama = r.engines.iter().find(|e| e.engine == "llama").unwrap();
    assert_eq!(llama.chosen, "vulkan");
    let b = llama.builds.iter().find(|b| b.variant == "cuda12").unwrap();
    assert!(b.installed);
    let m = b.no_kernel_image.as_ref().unwrap();
    assert_eq!(m.exit, "exited with code 1");
    assert!(m.line.starts_with("CUDA error"));
}

/// The two ops over HTTP. On a platform without pins (Linux) the answer
/// says the engines are not native and lists none, and there is no mark to
/// clear; with pins (Windows) it lists every engine.
#[tokio::test]
async fn the_engine_ops_answer_over_http() {
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let gw = crate::common::serve(state).await;
    let (status, body) =
        crate::agents_wire::op(&gw, "engine_releases", serde_json::json!({})).await;
    assert_eq!(status, 200, "{body}");
    let r: lmgw_api_types::engine_releases::EngineReleasesResponse =
        serde_json::from_value(body).unwrap();
    assert_eq!(r.native, cfg!(windows));
    assert_eq!(
        r.engines.is_empty(),
        engine_assets::pins::current().is_none()
    );
    let (status, body) = crate::agents_wire::op(
        &gw,
        "engine_fallback_clear",
        serde_json::json!({ "engine": "llama", "variant": "cuda12" }),
    )
    .await;
    assert_ne!(status, 200, "nothing is marked: {body}");
    assert!(
        body.to_string().contains("mark") || body.to_string().contains("pinned"),
        "{body}"
    );
}

/// Where engines run as native processes, the flag-vocabulary reads say so
/// and run nothing: no podman (`NoPodman` panics on a call), no CDI spec, as
/// a model's create on Windows asked both before the WP8 live round.
#[tokio::test]
async fn native_engines_read_no_flag_vocabulary_from_an_image() {
    let dir = tempfile::tempdir().unwrap();
    let reg = registry(Native::new(dir.path()).unwrap());
    let gpu = gpu_all();
    let llama = reg
        .help_text("lmgw", "ghcr.io/ggml-org/llama.cpp:server-cuda", &gpu)
        .await
        .unwrap_err();
    assert!(llama.contains("native processes"), "{llama}");
    assert!(llama.contains("--help"), "{llama}");
    let sd = reg
        .sdcpp_caps(
            "lmgw",
            "ghcr.io/leejet/stable-diffusion.cpp:master-cuda",
            &gpu,
        )
        .await
        .unwrap_err();
    assert!(sd.contains("native processes"), "{sd}");
}

/// The classes' default run args: every card (`--device nvidia.com/gpu=all`).
fn gpu_all() -> Vec<String> {
    vec!["--device".into(), "nvidia.com/gpu=all".into()]
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p.clone());
                }
                out.push(p);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Live: the real pins (Windows design §4.2's records)
// ---------------------------------------------------------------------------

/// The real eSpeak NG MSI unpacks, and the directory audio.cpp is pointed
/// at holds its library and data. Skips unless `LMGW_LIVE_ESPEAK_MSI` names
/// the downloaded `espeak-ng.msi` (1.52.0).
#[test]
fn espeak_live_the_pinned_msi_unpacks_into_a_home() {
    let Some(msi) = std::env::var_os("LMGW_LIVE_ESPEAK_MSI") else {
        eprintln!("skipped: LMGW_LIVE_ESPEAK_MSI is not set");
        return;
    };
    let out = tempfile::tempdir().unwrap();
    engine_assets::msi::extract(Path::new(&msi), out.path()).unwrap();
    let home = engine_assets::espeak_home(out.path()).unwrap();
    let files = walk(&home).into_iter().filter(|p| p.is_file()).count();
    eprintln!(
        "espeak live: home {} ({} files below it)",
        home.strip_prefix(out.path()).unwrap().display(),
        files
    );
    assert!(home.join("espeak-ng-data").join("phontab").is_file());
}

/// The real pins on the machine running the test: prints the variant each
/// engine gets, then downloads and verifies the llama.cpp build chosen and
/// starts `LMGW_LIVE_NATIVE_GGUF` on it with every layer on the card, for
/// one short chat. Skips unless `LMGW_LIVE_ENGINE_ASSETS` names an engines
/// dir (kept between runs, so a second run downloads nothing) and
/// `LMGW_LIVE_NATIVE_GGUF` a small chat GGUF. Windows only (the pins).
#[tokio::test]
async fn engine_assets_live_the_chosen_llama_build_answers_a_chat() {
    let (Some(root), Some(gguf)) = (
        std::env::var_os("LMGW_LIVE_ENGINE_ASSETS"),
        std::env::var_os("LMGW_LIVE_NATIVE_GGUF"),
    ) else {
        eprintln!("skipped: LMGW_LIVE_ENGINE_ASSETS and LMGW_LIVE_NATIVE_GGUF are not set");
        return;
    };
    let engines = Engines::new(PathBuf::from(&root));
    let Some(pins) = engines.pins().cloned() else {
        eprintln!("skipped: this platform has no pinned engine builds");
        return;
    };
    let facts = engines.facts().await;
    eprintln!("engine assets live: facts {facts:?}");
    for (engine, p) in pins.engines() {
        let c = engines.choose(engine, p, &facts);
        eprintln!("engine assets live: {engine} -> {} ({})", c.variant, c.why);
    }
    let gguf = PathBuf::from(gguf);
    let models = gguf.parent().unwrap().display().to_string();
    let file = gguf.file_name().unwrap().to_string_lossy().into_owned();
    let t_install = Instant::now();
    let installed = engines
        .ensure(Class::Chat, &Unwatched)
        .await
        .unwrap()
        .unwrap();
    eprintln!(
        "engine assets live: llama {} installed/verified in {:?} at {}",
        installed.build,
        t_install.elapsed(),
        installed.dir.display()
    );
    let reg = registry(Native::with_engines(Engines::new(PathBuf::from(&root))).unwrap());
    let data = tempfile::tempdir().unwrap();
    // The class default's card: every GPU, as `--device nvidia.com/gpu=all`.
    let rt = ModelRuntime {
        extra_run_args: gpu_all(),
        ..chat_runtime(
            "live-gpu-chat",
            &file,
            LlamaParams {
                n_gpu_layers: Some(99),
                ctx_size: Some(4096),
                jinja: true,
                ..LlamaParams::default()
            },
        )
    };
    let name = container_name("lmgw", Class::Chat, "live-gpu-chat");
    let t0 = Instant::now();
    let guard = match reg
        .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(900)))
        .await
    {
        Ok(g) => g,
        Err(e) => panic!(
            "{e}\n--- log ---\n{}",
            reg.logs_tail(&name, 80).await.unwrap_or_default()
        ),
    };
    let ready = t0.elapsed();
    let t1 = Instant::now();
    let body: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", guard.endpoint()))
        .json(&serde_json::json!({
            "messages": [{"role": "user", "content": "Name three colors."}],
            "max_tokens": 32,
            "chat_template_kwargs": {"enable_thinking": false}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let request = t1.elapsed();
    let text = body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let tps = body["timings"]["predicted_per_second"].as_f64();
    let log = reg.logs_tail(&name, 400).await.unwrap_or_default();
    let offload = log
        .lines()
        .filter(|l| l.contains("offload") || l.contains("CUDA0"))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" | ");
    drop(guard);
    reg.stop(Class::Chat, "live-gpu-chat", false).await.unwrap();
    assert!(!text.trim().is_empty(), "{body}");
    eprintln!(
        "engine assets live: {} ready in {ready:?}, chat in {request:?} ({tps:?} tok/s); {offload}; \
         answer: {}",
        installed.build,
        text.trim().replace('\n', " ")
    );
}

/// An audio row through the real pins: Kokoro (it phonemizes with eSpeak
/// NG) speaks one sentence. With `LMGW_LIVE_FORCE_VARIANT=cuda12` the
/// audio.cpp CUDA 12 build is forced, which has no kernel for a card below
/// compute capability 6.1 (§2): the start or the first request ends in "no
/// kernel image", the build is marked and the Vulkan build takes over.
/// Skips unless `LMGW_LIVE_ENGINE_ASSETS` names an engines dir and
/// `LMGW_LIVE_KOKORO_DIR` the directory holding `Kokoro-82M-GGUF`.
#[tokio::test]
async fn engine_assets_live_kokoro_speaks_through_espeak() {
    let (Some(root), Some(models)) = (
        std::env::var_os("LMGW_LIVE_ENGINE_ASSETS"),
        std::env::var_os("LMGW_LIVE_KOKORO_DIR"),
    ) else {
        eprintln!("skipped: LMGW_LIVE_ENGINE_ASSETS and LMGW_LIVE_KOKORO_DIR are not set");
        return;
    };
    let mut engines = Engines::new(PathBuf::from(&root));
    if engines.pins().is_none() {
        eprintln!("skipped: this platform has no pinned engine builds");
        return;
    }
    if let Ok(v) = std::env::var("LMGW_LIVE_FORCE_VARIANT") {
        let v: Variant = serde_json::from_value(serde_json::json!(v)).unwrap();
        engines = engines.force_variant("audio", v);
    }
    let models = PathBuf::from(models).display().to_string();
    let native = Arc::new(Native::with_engines(engines).unwrap());
    let reg = registry_over(native.clone());
    let data = tempfile::tempdir().unwrap();
    let rt = ModelRuntime {
        class: Class::Audio,
        model_id: "kokoro".into(),
        image: "unused-by-the-native-launcher".into(),
        extra_run_args: gpu_all(),
        idle_seconds: 0,
        warm_start: false,
        enabled: true,
        llama: None,
        audio: Some(AudioModel {
            id: 1,
            model_id: "kokoro".into(),
            family: "kokoro_tts".into(),
            path: "Kokoro-82M-GGUF".into(),
            task: "tts".into(),
            mode: "offline".into(),
            lazy: None,
            busy_timeout_ms: None,
            backend: None,
            threads: None,
            load_options: Default::default(),
            session_options: Default::default(),
            default_request_options: Default::default(),
            model_spec_override: None,
            config_id: None,
            weight_id: None,
            voice_presets: Default::default(),
            default_voice_preset: None,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            fallback: None,
            residency: None,
        }),
        audio_settings: Some(AudioSettings {
            models_dir: models.clone(),
            lazy_load: false,
            ..AudioSettings::default()
        }),
        image_model: None,
        sdcpp_caps: None,
        rung: None,
    };
    let name = container_name("lmgw", Class::Audio, "kokoro");
    let speak = |endpoint: String| async move {
        let t = Instant::now();
        let resp = reqwest::Client::new()
            .post(format!("{endpoint}/v1/audio/speech"))
            .json(&serde_json::json!({
                "model": "kokoro",
                "input": "Hello from Windows. Kokoro speaks through eSpeak.",
                "voice": "af_heart",
                "response_format": "wav"
            }))
            .send()
            .await;
        match resp {
            Ok(r) => {
                let status = r.status();
                let bytes = r.bytes().await.map(|b| b.len()).unwrap_or(0);
                (status.as_u16(), bytes, t.elapsed())
            }
            Err(_) => (0, 0, t.elapsed()),
        }
    };
    let mut attempts = Vec::new();
    for attempt in 0..2 {
        let t0 = Instant::now();
        let guard = match reg
            .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(900)))
            .await
        {
            Ok(g) => g,
            Err(e) => panic!(
                "{e}\n--- log ---\n{}",
                reg.logs_tail(&name, 80).await.unwrap_or_default()
            ),
        };
        let ready = t0.elapsed();
        eprintln!(
            "engine assets live: kokoro attempt {attempt} warnings {:?}",
            reg.list()
                .first()
                .map(|e| e.warnings.clone())
                .unwrap_or_default()
        );
        let (status, bytes, took) = speak(guard.endpoint()).await;
        if status != 200 {
            // An engine that died at its request ends on its own: waited
            // for (at most 30 s), so the stop below is not what ended it.
            use lmgw_core::runtime::launcher::Launcher;
            let t = Instant::now();
            let mut exit = None;
            while exit.is_none() && t.elapsed() < Duration::from_secs(30) {
                exit = native.exited(&name).await;
                if exit.is_none() {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
            eprintln!("engine assets live: kokoro attempt {attempt} ended: {exit:?}");
        }
        let log = reg.logs_tail(&name, 400).await.unwrap_or_default();
        let backend_line = log
            .lines()
            .rev()
            .find(|l| l.contains("backend") || l.contains("Vulkan") || l.contains("CUDA"))
            .unwrap_or_default()
            .to_string();
        drop(guard);
        reg.stop(Class::Audio, "kokoro", false).await.unwrap();
        eprintln!(
            "engine assets live: kokoro attempt {attempt}: ready in {ready:?}, speech {status} \
             ({bytes} bytes) in {took:?}; {backend_line}"
        );
        attempts.push((status, bytes));
        if status == 200 {
            break;
        }
        if !log.to_ascii_lowercase().contains(fallback::NO_KERNEL_IMAGE) {
            panic!("speech failed without a kernel-image error:\n{log}");
        }
        let cuda = PathBuf::from(&root).join("audio");
        eprintln!(
            "engine assets live: marks {:?}",
            std::fs::read_dir(&cuda)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| fallback::marked(&e.path()).map(|m| (e.file_name(), m)))
                .collect::<Vec<_>>()
        );
    }
    let (status, bytes) = *attempts.last().unwrap();
    assert_eq!(status, 200, "{attempts:?}");
    assert!(bytes > 44, "a WAV with samples");
}

/// An image row through the real pins: Z-Image-Turbo draws one 512² picture
/// with its weights offloaded (`--offload-to-cpu`, as a 4 GiB card needs).
/// With `LMGW_LIVE_FORCE_VARIANT=cuda12` stable-diffusion.cpp's CUDA 12
/// build is forced, which has no kernel below compute capability 6.1: on
/// such a card the start can end in "no kernel image" while it loads (the
/// registry then marks the build and starts the Vulkan build in the same
/// acquire, which the start's warnings say), or the build gets to ready and
/// its first request ends the process, and the next start marks it and
/// falls back. On a GTX 970 test card (2026-11-11) it went the second way: sd-server
/// loads its weights without a kernel launch. It prints which way it went,
/// the mark, and the times. Skips unless
/// `LMGW_LIVE_ENGINE_ASSETS` names an engines dir and `LMGW_LIVE_IMAGE_DIR`
/// the directory holding `z_image_turbo-Q4_K.gguf`, `ae.safetensors` and
/// `Qwen3-4B-Instruct-2507-Q4_K_M.gguf`.
#[tokio::test]
async fn engine_assets_live_z_image_draws_after_the_fallback() {
    let (Some(root), Some(models)) = (
        std::env::var_os("LMGW_LIVE_ENGINE_ASSETS"),
        std::env::var_os("LMGW_LIVE_IMAGE_DIR"),
    ) else {
        eprintln!("skipped: LMGW_LIVE_ENGINE_ASSETS and LMGW_LIVE_IMAGE_DIR are not set");
        return;
    };
    let mut engines = Engines::new(PathBuf::from(&root));
    if engines.pins().is_none() {
        eprintln!("skipped: this platform has no pinned engine builds");
        return;
    }
    if let Ok(v) = std::env::var("LMGW_LIVE_FORCE_VARIANT") {
        let v: Variant = serde_json::from_value(serde_json::json!(v)).unwrap();
        engines = engines.force_variant("sdcpp", v);
    }
    let models = PathBuf::from(models).display().to_string();
    let native = Arc::new(Native::with_engines(engines).unwrap());
    let reg = registry_over(native.clone());
    let data = tempfile::tempdir().unwrap();
    let obj = |v: serde_json::Value| v.as_object().cloned().unwrap();
    let rt = ModelRuntime {
        class: Class::Image,
        model_id: "z-image".into(),
        image: "unused-by-the-native-launcher".into(),
        extra_run_args: gpu_all(),
        idle_seconds: 0,
        warm_start: false,
        enabled: true,
        llama: None,
        audio: None,
        audio_settings: None,
        image_model: Some(ImageModel {
            id: 1,
            model_id: "z-image".into(),
            files: obj(serde_json::json!({
                "diffusion_model": "z_image_turbo-Q4_K.gguf",
                "vae": "ae.safetensors",
                "llm": "Qwen3-4B-Instruct-2507-Q4_K_M.gguf"
            })),
            args: obj(serde_json::json!({
                "offload_to_cpu": true, "diffusion_fa": true, "cfg_scale": 1.0, "steps": 4,
                "width": 512, "height": 512
            })),
            modes: vec!["img_gen".into()],
            edit: false,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            idle_seconds: 0,
            fallback: None,
            capabilities_override: None,
            peak_extra_bytes: None,
            peak_learned_at: None,
            idle_bytes: None,
            work_bytes: None,
            learned_key: None,
            prompt_style: None,
            preview_mode: None,
        }),
        sdcpp_caps: None,
        rung: None,
    };
    let name = container_name("lmgw", Class::Image, "z-image");
    let marks = || {
        std::fs::read_dir(PathBuf::from(&root).join("sdcpp"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| fallback::marked(&e.path()).map(|m| (e.file_name(), m)))
            .collect::<Vec<_>>()
    };
    let mut drawn = None;
    for attempt in 0..2 {
        let t0 = Instant::now();
        let guard = match reg
            .acquire(&spec(&rt, &models, data.path(), Duration::from_secs(900)))
            .await
        {
            Ok(g) => g,
            Err(e) => panic!(
                "{e}\n--- log ---\n{}",
                reg.logs_tail(&name, 80).await.unwrap_or_default()
            ),
        };
        let ready = t0.elapsed();
        let warnings = reg
            .list()
            .first()
            .map(|e| e.warnings.clone())
            .unwrap_or_default();
        let while_loading = warnings.iter().any(|w| w.contains("no kernel image"));
        eprintln!(
            "engine assets live: z-image attempt {attempt}: ready in {ready:?}, fallback while \
             loading: {while_loading}; warnings {warnings:?}; marks {:?}",
            marks()
        );
        let t = Instant::now();
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/images/generations", guard.endpoint()))
            .json(&serde_json::json!({
                "prompt": "a red bicycle leaning against a white wall, photo",
                "size": "512x512",
                "n": 1
            }))
            .timeout(Duration::from_secs(900))
            .send()
            .await;
        let (status, bytes) = match resp {
            Ok(r) => (
                r.status().as_u16(),
                r.bytes().await.map(|b| b.len()).unwrap_or(0),
            ),
            Err(e) => {
                eprintln!("engine assets live: z-image attempt {attempt}: request failed: {e}");
                (0, 0)
            }
        };
        let took = t.elapsed();
        if status != 200 {
            // An engine that died at its request ends on its own: waited for
            // (at most 30 s), so the stop below is not what ended it and the
            // exit is judged as the engine's own.
            use lmgw_core::runtime::launcher::Launcher;
            let t = Instant::now();
            let mut exit = None;
            while exit.is_none() && t.elapsed() < Duration::from_secs(30) {
                exit = native.exited(&name).await;
                if exit.is_none() {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
            eprintln!("engine assets live: z-image attempt {attempt} ended: {exit:?}");
        }
        let log = reg.logs_tail(&name, 400).await.unwrap_or_default();
        if let Some(line) = fallback::no_kernel_image_line(&log) {
            eprintln!("engine assets live: z-image attempt {attempt}: {line}");
        }
        let backend = log
            .lines()
            .find(|l| l.contains("Vulkan0") || l.contains("CUDA0") || l.contains("ggml_vulkan"))
            .unwrap_or_default()
            .to_string();
        drop(guard);
        reg.stop(Class::Image, "z-image", false).await.unwrap();
        eprintln!(
            "engine assets live: z-image attempt {attempt}: image {status} ({bytes} bytes) in \
             {took:?}; {backend}"
        );
        if status == 200 {
            drawn = Some((attempt, while_loading));
            break;
        }
        assert!(
            log.to_ascii_lowercase().contains(fallback::NO_KERNEL_IMAGE),
            "the image failed without a kernel-image error:\n{log}"
        );
    }
    eprintln!(
        "engine assets live: z-image drawn {drawn:?} (attempt, fallback while loading); marks {:?}",
        marks()
    );
    assert!(drawn.is_some(), "no image in two starts");
}

/// The release check against GitHub for the Windows pins (any platform):
/// prints, per project, the newest Windows release and whether it is newer
/// than the pin. Skips unless `LMGW_LIVE_RELEASE_CHECK` is set.
#[tokio::test]
async fn release_check_live_github_answers_for_every_pin() {
    if std::env::var_os("LMGW_LIVE_RELEASE_CHECK").is_none() {
        eprintln!("skipped: LMGW_LIVE_RELEASE_CHECK is not set");
        return;
    }
    use lmgw_core::runtime::engine_assets::{pins, releases};
    let w = pins::table().get("windows-x86_64").unwrap();
    let seen = releases::observe(releases::API, w, Duration::from_secs(60), "now").await;
    for (engine, o) in &seen {
        eprintln!(
            "release check live: {engine} {}: pinned {}, newest {:?} ({:?}), update {}, error {:?}",
            o.repo, o.pinned_tag, o.latest_tag, o.latest_published, o.update_available, o.error
        );
        assert!(o.error.is_none(), "{engine}: {:?}", o.error);
    }
    assert_eq!(seen.len(), 4);
}
