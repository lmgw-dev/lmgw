//! `engine_releases`: the Engines page's Windows column (Windows design
//! §4.2) — each engine's pinned upstream release build, the one this
//! machine runs, what is installed, and whether upstream has published a
//! newer release than the pin.

use serde::{Deserialize, Serialize};

/// `engine_releases` takes no arguments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EngineReleasesArgs {}

/// `engine_fallback_clear`: remove one build's "no kernel image" mark, so
/// the next start of its engine may choose it again.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EngineFallbackClearArgs {
    /// `llama`, `sdcpp`, `audio`.
    pub engine: String,
    /// The marked build's variant (`cuda13`, `cuda12`).
    pub variant: String,
}

/// The answer (also `engine_fallback_clear`'s, read after the change).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EngineReleasesResponse {
    /// The platform's key in lmgw's pin table (`windows-x86_64`).
    pub platform: String,
    /// lmgw starts its engines as native processes from these builds here
    /// (Windows). `false` where Podman runs them (Linux): `engines` is then
    /// empty.
    pub native: bool,
    /// Where the builds are installed.
    pub engines_dir: String,
    /// What the variant choice read of this machine.
    pub machine: MachineGpu,
    pub engines: Vec<EngineRelease>,
    /// eSpeak NG, which the phonemizing audio.cpp families load.
    pub espeak: Option<SupportRelease>,
    /// When the last release check ended; `None` before the first.
    pub checked_at: Option<String>,
}

/// The facts the choice reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineGpu {
    /// `NVIDIA GeForce GTX 970 (5.2)`, one per NVIDIA card.
    pub cards: Vec<String>,
    /// The driver's CUDA version (`12.6`).
    pub driver_cuda: Option<String>,
    /// Why NVML said nothing, when it did not.
    pub nvml_error: Option<String>,
    pub vulkan_loader: bool,
}

/// One engine's pinned release.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EngineRelease {
    /// `llama`, `sdcpp`, `audio`.
    pub engine: String,
    /// `llama.cpp`.
    pub label: String,
    /// `ggml-org/llama.cpp`.
    pub repo: String,
    /// The pinned tag (`b12244`).
    pub tag: String,
    /// The pinned release's day.
    pub published: String,
    /// The variant this machine runs (`cuda12`).
    pub chosen: String,
    /// Why.
    pub chosen_why: String,
    /// Its first start JIT-compiles the kernels from PTX.
    pub chosen_jit: bool,
    pub builds: Vec<EngineBuild>,
    /// The upstream release check.
    pub release: Option<ReleaseCheck>,
}

/// One pinned build of an engine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EngineBuild {
    /// `cuda13`, `cuda12`, `vulkan`, `cpu`.
    pub variant: String,
    /// The release asset, and its cudart companion for CUDA.
    pub assets: Vec<String>,
    /// Bytes to download.
    pub download_bytes: u64,
    /// The CUDA toolkit it is built with.
    pub cuda_toolkit: Option<String>,
    /// The architectures it carries: `SASS 86, 89; PTX 50, 61` (per file
    /// with device code).
    pub cuda_coverage: Vec<String>,
    /// Downloaded, verified and unpacked.
    pub installed: bool,
    /// A start of it ended with "no kernel image is available" here: the
    /// engine runs its Vulkan build instead, until the mark is cleared
    /// (`engine_fallback_clear`).
    pub no_kernel_image: Option<NoKernelImage>,
    /// Its directory.
    pub dir: String,
}

/// Why a build is marked: what its process said and how it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NoKernelImage {
    /// The engine's line (`CUDA error: no kernel image is available for
    /// execution on the device`).
    pub line: String,
    /// How the process ended (`exited with code 1`); empty for a mark that
    /// does not say.
    pub exit: String,
    /// When the mark was written, RFC 3339; empty when it does not say.
    pub marked_at: String,
}

/// A support library's pin (eSpeak NG).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SupportRelease {
    pub label: String,
    pub repo: String,
    pub tag: String,
    pub published: String,
    pub asset: String,
    pub download_bytes: u64,
    pub installed: bool,
    pub dir: String,
    pub release: Option<ReleaseCheck>,
}

/// What the release check saw of an upstream project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReleaseCheck {
    pub checked_at: String,
    /// The newest release with a Windows build.
    pub latest_tag: Option<String>,
    pub latest_published: Option<String>,
    /// Upstream has a newer release than the pin. lmgw runs its pinned
    /// builds only; the newer one comes with the lmgw release that pins it.
    pub update_available: bool,
    pub error: Option<String>,
}
