//! Command plumbing

use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

/// How the registry reaches `podman`.
///
/// Introduced as a deliberate duplicate of the shared-router manager's trait
/// of the same name, so the per-model path would not be left pointing at a
/// corpse when §7 deleted router mode; it is now the only one. The verbs this
/// module drives through it are `run`, `stop`, `wait`, `rm`, `logs`, the
/// throwaway `run --rm` of the probe/help path (§3.6) and — since
/// reconciliation (§3.4) — `ps` and `inspect`.
///
/// Those two are **not** trait methods, and deliberately: the trait's one
/// method already takes the argv, so a podman verb is data here rather than
/// API surface. Adding `ps`/`inspect` methods would force every existing fake
/// to grow implementations for verbs it does not care about, and would make
/// the *next* verb a breaking change to a trait three test files implement.
#[async_trait::async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput>;
}

/// The real runner: `podman` as a child process.
pub struct TokioRunner;

#[async_trait::async_trait]
impl CommandRunner for TokioRunner {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        let out = tokio::process::Command::new(program)
            .args(args)
            .kill_on_drop(true)
            .output()
            .await?;
        Ok(CmdOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// A runner that refuses every verb, with the reason it was installed.
///
/// The container-lifecycle sibling of [`crate::vram::nvml::NoTelemetry`], and
/// installed for the same reason: [`crate::state::AppState::init_for_tests`]
/// must not shell a real `podman` on the machine the suite runs on. A test
/// that reaches the lifecycle without meaning to fails with this message
/// instead of starting (or stopping) something on a live box.
pub struct NoRuntime(pub String);

#[async_trait::async_trait]
impl CommandRunner for NoRuntime {
    async fn run(&self, _program: &str, _args: &[String]) -> std::io::Result<CmdOutput> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            self.0.clone(),
        ))
    }
}

/// Where a start gets its host port (§3.5).
///
/// Production always uses [`ephemeral_port`]. The indirection exists so a
/// test can hand the registry the port of a `/health` mock — and so the
/// port-conflict retry (§10.4) can be driven with two *known-different*
/// ports instead of hoping the kernel does not hand the same one back twice.
pub type PortAllocator = Arc<dyn Fn() -> std::io::Result<u16> + Send + Sync>;

/// Ask the kernel for a free port and immediately give it back (§3.5).
///
/// There is no port range setting and no persisted port field: the ephemeral
/// range *is* the real limit, and inventing a smaller window would only add a
/// way to run out. The window between the listener closing and `podman run`
/// binding is genuinely racy, and that race is handled where it surfaces —
/// [`Registry::start_container`](crate::runtime::registry::Registry::start_container) retries once on a fresh port.
pub fn ephemeral_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// What [`Registry::image_facts`](crate::runtime::registry::Registry::image_facts) read about one image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageFacts {
    /// The image ID as podman prints it (full hex, no `sha256:`) — what the
    /// `--help` caches are keyed by.
    pub id: String,
    /// The executable of `Config.Entrypoint` (its first element), when the
    /// image sets one. Only the executable: a `["/bin/sh", "-c", …]`
    /// entrypoint names `sh`, which is exactly as much as it says about where
    /// the server is.
    pub entrypoint: Option<String>,
}

impl ImageFacts {
    /// Parse `{{.Id}} {{json .Config.Entrypoint}}`. `None` without an ID;
    /// an entrypoint that is `null`, empty or not the JSON list podman prints
    /// is simply absent — it only ever adds a candidate, so there is nothing
    /// for a misread to break.
    pub(super) fn parse(stdout: &str) -> Option<Self> {
        let line = stdout.lines().next()?.trim();
        let (id, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let id = id.trim_start_matches("sha256:");
        if id.is_empty() {
            return None;
        }
        let entrypoint = serde_json::from_str::<Option<Vec<String>>>(rest.trim())
            .ok()
            .flatten()
            .and_then(|argv| argv.into_iter().next())
            .filter(|bin| !bin.trim().is_empty());
        Some(Self {
            id: id.to_string(),
            entrypoint,
        })
    }
}

/// Where `llama-server` may be inside an image, in the order a `--help` read
/// tries them.
///
/// The image's own entrypoint comes first — but only when its basename *is*
/// `llama-server`. Then it is the binary a start of that image runs (llama
/// starts never override the entrypoint), so its vocabulary is the one that
/// matters, and ik_llama.cpp's `/llama-server` (off `PATH`) is found on the
/// first try. Anything else is ignored rather than tried: the owner's
/// nginx-fronted images have `/app/entrypoint.sh`, which starts llama-server
/// in the background and waits on nginx, so a `--help` handed to it would
/// start a server instead of printing a vocabulary.
///
/// After it, the known places: `PATH`, `/app/llama-server` (the upstream
/// `ggml-org/llama.cpp` server images and the nginx-fronted derivatives) and
/// `/llama-server` (ik_llama.cpp's images).
pub fn llama_server_candidates(entrypoint: Option<&str>) -> Vec<String> {
    const KNOWN: [&str; 3] = ["llama-server", "/app/llama-server", "/llama-server"];
    let mut out: Vec<String> = entrypoint
        .map(str::trim)
        .filter(|ep| ep.rsplit('/').next() == Some("llama-server"))
        .map(str::to_string)
        .into_iter()
        .collect();
    for known in KNOWN {
        if !out.iter().any(|c| c == known) {
            out.push(known.to_string());
        }
    }
    out
}

/// A class's (or model's) `podman run` args as a **throwaway** probe
/// ([`Registry::run_throwaway`](crate::runtime::registry::Registry::run_throwaway)) can take them: without what contradicts a
/// foreground, named, `--rm --replace`d container that prints and exits —
/// `-d`/`--detach` (the caller would get no output), `--restart` (a probe
/// that restarts never ends), `-p`/`--publish`/`-P`/`--publish-all` (a probe
/// needs no port, and would take one a running model has) and `--name` (the
/// probe names itself). Both `--flag value` and `--flag=value` forms.
pub fn throwaway_args(args: &[String]) -> Vec<String> {
    const WITH_VALUE: [&str; 4] = ["--restart", "-p", "--publish", "--name"];
    const ALONE: [&str; 4] = ["-d", "--detach", "-P", "--publish-all"];
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let flag = a.split_once('=').map_or(a.as_str(), |(f, _)| f);
        if ALONE.contains(&flag) {
            continue;
        }
        if WITH_VALUE.contains(&flag) {
            if !a.contains('=') {
                it.next(); // its value
            }
            continue;
        }
        // `-p8080:80`, the short form with the value attached.
        if a.len() > 2 && a.starts_with("-p") && !a.starts_with("--") {
            continue;
        }
        out.push(a.clone());
    }
    out
}

/// What hides every GPU from a probe that needs none: CUDA sees no device,
/// so the binary creates no CUDA context — hundreds of MB on the card that
/// the VRAM ledger never sees and the GPU hold never stops (measured
/// 2026-09-26: an arch probe on official llama-server held 390 MiB without
/// it, nothing with it). The device args stay: they are what mount the GPU
/// libraries, and ik_llama.cpp links `libcuda.so.1` directly, so without
/// them it cannot even print `--help`.
pub const HIDE_GPUS: [&str; 2] = ["-e", "CUDA_VISIBLE_DEVICES="];

/// `args` with [`HIDE_GPUS`] after them.
pub fn without_gpus(args: &[String]) -> Vec<String> {
    let mut out = args.to_vec();
    out.extend(HIDE_GPUS.iter().map(|s| s.to_string()));
    out
}
