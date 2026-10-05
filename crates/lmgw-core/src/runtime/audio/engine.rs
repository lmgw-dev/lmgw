//! What one audio row's engine runs with (the per-row CPU switch): the
//! class's `server.json` engine keys with the row's `backend` and `threads`
//! in effect, and the `podman run` args that go with them.
//!
//! **One function renders it** ([`engine_settings`]), and every consumer
//! reads its result rather than the class settings: the start's
//! `server.json` (the descriptor's `audio_settings`), boot adoption's
//! compare, and `local_model_get`'s `server_json`. A row that sets neither
//! field gets the class settings unchanged, so it renders the bytes it
//! rendered before the switch existed, and adoption keeps its container.
//!
//! **Threads** ([`threads_in_effect`]): the row's own count wins. Without
//! one, a row switched to the CPU uses this machine's physical cores — the
//! class's `threads` is a GPU engine's handful of host threads (default 1)
//! and would starve a CPU one — and every other row the class's figure. A
//! row that inherits a class backend of `cpu` keeps the class's figure too:
//! the class's backend and threads were set together, and a row that
//! changes neither must render what it rendered before. Nothing writes a
//! row's count by itself; with several CPU rows busy at once each runs its
//! own pool, so the owner splits the cores per row (or pins them with
//! `--cpuset-cpus` in its run args).
//!
//! **Run args** ([`run_args`]): a CPU row that inherits the class's
//! `extra_run_args` gets them without the GPU passthrough
//! ([`strip_gpu_devices`]: CDI and `--device` nodes, `--gpus`, GPU-node
//! volumes and mounts, the NVIDIA runtime, CDI annotations and
//! `NVIDIA_VISIBLE_DEVICES`) and with `NVIDIA_VISIBLE_DEVICES=void`
//! ([`cpu_run_args`]), which keeps the legacy NVIDIA OCI hook from handing
//! the GPU over anyway (the CUDA images set the variable to `all`). So its
//! zero VRAM is structural for the forms lmgw knows rather than resting on
//! audio.cpp never opening a CUDA context (it logs `ggml_cuda_init: found 1
//! CUDA devices` even on `backend: cpu`); a passthrough configured some
//! other way on the host is not seen here. The CUDA image serves on the CPU
//! without the device: the live check of 2026-10-02 transcribed on a dev
//! copy with `HostConfig.Devices = []` and no NVIDIA mounts, every
//! transcript exact and no GPU process for the container. Everything else
//! stays — `--security-opt label=disable` above all, without which the
//! container cannot read its model directory under SELinux. A row's own
//! override is used verbatim: lmgw never rewrites args the owner typed, and
//! says so when those args still pass a GPU ([`own_args_pass_gpu`]).

use serde::Serialize;

use crate::config::{AudioModel, AudioSettings};
use crate::host::HostCpu;
use crate::runtime::Placement;

/// The one backend a row may set for itself. Another GPU backend is a class
/// setting: it has to match the image the class runs.
pub const ROW_BACKENDS: &[&str] = &["cpu"];

/// Where a row's thread count comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadsSource {
    /// The row's own `threads`.
    Row,
    /// The audio class's `threads`.
    Class,
    /// This machine's physical cores ([`crate::host::cpu`]): a row switched
    /// to the CPU that names no count.
    Cores,
    /// [`Self::Cores`] where the core topology could not be read: the
    /// figure is the logical CPU count ([`crate::host::CoreSource`]).
    LogicalCpus,
}

impl ThreadsSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Row => "row",
            Self::Class => "class",
            Self::Cores => "cores",
            Self::LogicalCpus => "logical_cpus",
        }
    }
}

/// The row switched itself to the CPU (`backend: "cpu"` on the row, not
/// inherited from the class).
pub fn row_on_cpu(m: &AudioModel) -> bool {
    m.backend.as_deref().map(str::trim) == Some("cpu")
}

/// The backend `m` runs on: its own, else the class's.
pub fn backend_in_effect<'a>(m: &'a AudioModel, s: &'a AudioSettings) -> &'a str {
    m.backend
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .unwrap_or(&s.backend)
}

/// Where `m`'s container computes, as its row is configured now.
pub fn placement(m: &AudioModel, s: &AudioSettings) -> Placement {
    Placement::of_backend(backend_in_effect(m, s))
}

/// Where `m`'s thread count comes from (module doc).
pub fn threads_source(m: &AudioModel) -> ThreadsSource {
    match m.threads {
        Some(_) => ThreadsSource::Row,
        None if row_on_cpu(m) => ThreadsSource::Cores,
        None => ThreadsSource::Class,
    }
}

/// `m`'s thread count and where it comes from (module doc). The host
/// figure is named [`ThreadsSource::LogicalCpus`] when it is the fallback.
pub fn threads_in_effect(m: &AudioModel, s: &AudioSettings, host: HostCpu) -> (i64, ThreadsSource) {
    let source = threads_source(m);
    let n = match (source, m.threads) {
        (ThreadsSource::Row, Some(n)) => n,
        (ThreadsSource::Cores, _) => i64::try_from(host.physical_cores).unwrap_or(i64::MAX),
        _ => s.threads,
    };
    let source = match (source, host.source) {
        (ThreadsSource::Cores, crate::host::CoreSource::Logical) => ThreadsSource::LogicalCpus,
        _ => source,
    };
    (n, source)
}

/// How a surface names where a thread count comes from: "set on the row",
/// "audio class", or the host figure's own source ("physical cores").
pub fn threads_source_label(source: ThreadsSource, host: HostCpu) -> &'static str {
    match source {
        ThreadsSource::Row => "set on the row",
        ThreadsSource::Class => "audio class",
        ThreadsSource::Cores | ThreadsSource::LogicalCpus => host.source.describe(),
    }
}

/// The one line a start of a CPU row logs: `engine` is the descriptor's
/// engine settings ([`engine_settings`]).
pub fn cpu_start_line(m: &AudioModel, engine: &AudioSettings, host: HostCpu) -> String {
    format!(
        "audio model '{}' runs on the CPU ({} threads, {}): nothing on the GPU, nothing learned \
         or charged",
        m.model_id,
        engine.threads,
        threads_source_label(threads_source(m), host)
    )
}

/// The class settings with `m`'s backend and threads in effect — what its
/// `server.json` renders from ([`super::render_single_model_config`]).
pub fn engine_settings(m: &AudioModel, s: &AudioSettings, host: HostCpu) -> AudioSettings {
    let mut out = s.clone();
    out.backend = backend_in_effect(m, s).to_string();
    out.threads = threads_in_effect(m, s, host).0;
    out
}

/// `m`'s effective `podman run` args: its own override verbatim, else the
/// class's — without the GPU passthrough when `m` runs on the CPU.
pub fn run_args(m: &AudioModel, s: &AudioSettings) -> Vec<String> {
    match &m.extra_run_args {
        Some(own) => own.clone(),
        None if placement(m, s) == Placement::Cpu => cpu_run_args(&s.extra_run_args),
        None => s.extra_run_args.clone(),
    }
}

/// `m` runs on the CPU, but its own run args — used verbatim — still hand
/// its container a GPU: its zero VRAM then rests on audio.cpp again. The
/// surfaces say so (`local_model_get`'s problems, the editor).
pub fn own_args_pass_gpu(m: &AudioModel, s: &AudioSettings) -> bool {
    placement(m, s) == Placement::Cpu && m.extra_run_args.as_deref().is_some_and(passes_gpu)
}

/// What a CPU row whose own args still pass a GPU is told.
pub const OWN_ARGS_PASS_GPU: &str = "this row runs on the CPU, but its own run args still pass \
     the GPU to its container (a GPU --device, --gpus, a /dev/nvidia* volume, the NVIDIA runtime \
     or NVIDIA_VISIBLE_DEVICES): lmgw uses them as written, so nothing keeps audio.cpp off the \
     card — remove those flags, or clear the override to inherit the class args without them";

pub use lmgw_api_types::audio_engine::{cpu_run_args, passes_gpu, strip_gpu_devices};

#[cfg(test)]
mod tests;
