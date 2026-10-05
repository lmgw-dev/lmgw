//! Engine presets (container-builds design §2.1, §3, §4, §14.2, §14.3): the
//! static knowledge of how each upstream repository's Dockerfile is built,
//! edited and verified, plus the pure functions that turn a build definition
//! and a Dockerfile's text into what the run passes to `podman build`.
//!
//! Two tables:
//!
//! - [`REPO_PRESETS`] — the four repositories the build editor offers
//!   (official llama.cpp, ik_llama.cpp, audio.cpp, stable-diffusion.cpp).
//! - [`PROFILES`] — one [`DockerfileProfile`] per `(engine, backend,
//!   Dockerfile path)`. Keyed by the *path*, not the repository, so a fork that
//!   keeps upstream's Dockerfile gets exactly upstream's treatment. The table
//!   order is the candidate order: for `(llama, cuda)` the official
//!   `.devops/cuda.Dockerfile` is tried before ik's
//!   `.devops/llama-server-cuda.Dockerfile`, and the first one present in the
//!   checkout wins.
//!
//! Only the four CUDA profiles were built and verified in the WP0 spike
//! (§14, `tested: true`). The Vulkan/ROCm/CPU profiles are read off the
//! upstream files — their edits are proven to *match* today's text by the
//! fixture tests, but no image was ever built from them.
//!
//! The edits are the spike's measured ones (from a local spike checkout)
//! with three changes: the ccache and npm cache ids and the ccache size are
//! templated (`{{ccache_id}}`, `{{npm_cache_id}}`, `{{ccache_max_size}}`,
//! plus `{{ccache_shared_mount}}`/`{{ccache_seed}}` and
//! `{{npm_shared_mount}}`/`{{npm_seed}}`, which let a build with extras read
//! the shared caches without writing them), edits that
//! did two jobs are split so each has one [`EditRole`] (turning ccache off
//! must not take the build-number injection with it), and `build.sh`'s zlib
//! edits are gone — they no longer match, and CMake gzips the UI itself
//! (§14.2).
//!
//! Nothing here runs a process except [`detect_cuda_arch`] and
//! [`detect_driver_cuda_version`], which go through the registry's
//! [`CommandRunner`] seam so tests hand them canned output.

use regex::Regex;
use serde::Serialize;

use super::model::{BuildEdit, BuildSpec, EditRole, Engine, Forge, GpuBackend};
use super::tags;
use super::validate;
use crate::runtime::registry::CommandRunner;

/// The CUDA toolkit every engine builds against unless the build says
/// otherwise (§14.2): 13.0.0 works for all four on driver 615.71, and one
/// version everywhere means one set of base images instead of three.
pub const DEFAULT_CUDA_VERSION: &str = "13.0.0";

/// The ignore file passed with `--ignorefile` (§14.2). It *replaces* the
/// repository's own `.dockerignore`. `**/.git` because sd.cpp's submodules
/// each carry a `.git` file; `.git` alone as well, for builders that do not
/// read `**` as "zero or more directories".
pub const IGNORE_FILE: &str = ".git\n**/.git\n.github\nbuild/\n.cache/\n";

/// The template variables an edit may use.
pub const TEMPLATE_KEYS: [&str; 7] = [
    "ccache_id",
    "npm_cache_id",
    "ccache_max_size",
    "ccache_shared_mount",
    "ccache_seed",
    "npm_shared_mount",
    "npm_seed",
];

/// Where a build with extras sees the engine's shared ccache, read-only
/// (§14.2 "PR builds read the shared cache").
pub const CCACHE_SHARED_TARGET: &str = "/ccache-shared";

/// Where a build with extras sees the engine's shared npm cache, read-only —
/// the same pattern as [`CCACHE_SHARED_TARGET`], applied to `npm ci`.
pub const NPM_CACHE_SHARED_TARGET: &str = "/npm-shared";

// ---------------------------------------------------------------------------
// Repositories
// ---------------------------------------------------------------------------

/// Which upstream a Dockerfile (and the image built from it) follows. Decides
/// the verify probes (§14.3): ik has no `--list-devices`, audio.cpp's image
/// runs a dispatcher, sd.cpp's entrypoint is the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Flavor {
    /// ggml-org/llama.cpp.
    Official,
    /// ikawrakow/ik_llama.cpp — the `llama` engine with its own Dockerfiles.
    Ik,
    /// 0xShug0/audio.cpp.
    Audio,
    /// leejet/stable-diffusion.cpp.
    Sdcpp,
}

impl Flavor {
    /// The flavor a build of `engine` is assumed to be when neither its
    /// Dockerfile nor its repository is a known one.
    pub fn default_for(engine: Engine) -> Self {
        match engine {
            Engine::Llama => Self::Official,
            Engine::Audio => Self::Audio,
            Engine::Sdcpp => Self::Sdcpp,
        }
    }
}

/// One entry of the build editor's repository picker (§9.1).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct RepoPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub engine: Engine,
    pub flavor: Flavor,
    pub repo_url: &'static str,
    pub forge: Forge,
    /// The default branch — what a new build's `ref` starts as.
    pub default_ref: &'static str,
}

pub const REPO_PRESETS: [RepoPreset; 4] = [
    RepoPreset {
        id: "official",
        name: "llama.cpp (official)",
        engine: Engine::Llama,
        flavor: Flavor::Official,
        repo_url: "https://github.com/ggml-org/llama.cpp",
        forge: Forge::Github,
        default_ref: "master",
    },
    RepoPreset {
        id: "ik",
        name: "ik_llama.cpp",
        engine: Engine::Llama,
        flavor: Flavor::Ik,
        repo_url: "https://github.com/ikawrakow/ik_llama.cpp",
        forge: Forge::Github,
        default_ref: "main",
    },
    RepoPreset {
        id: "audio",
        name: "audio.cpp",
        engine: Engine::Audio,
        flavor: Flavor::Audio,
        repo_url: "https://github.com/0xShug0/audio.cpp",
        forge: Forge::Github,
        default_ref: "main",
    },
    RepoPreset {
        id: "sdcpp",
        name: "stable-diffusion.cpp",
        engine: Engine::Sdcpp,
        flavor: Flavor::Sdcpp,
        repo_url: "https://github.com/leejet/stable-diffusion.cpp",
        forge: Forge::Github,
        default_ref: "master",
    },
];

pub fn repo_preset(id: &str) -> Option<&'static RepoPreset> {
    REPO_PRESETS.iter().find(|p| p.id == id)
}

/// The preset whose repository `url` names — case-insensitively, with or
/// without a trailing `/` or `.git`, and in its `git@github.com:` spelling.
pub fn repo_preset_for_url(url: &str) -> Option<&'static RepoPreset> {
    let want = web_url(url).to_ascii_lowercase();
    REPO_PRESETS
        .iter()
        .find(|p| p.repo_url.to_ascii_lowercase() == want)
}

/// The browsable `https://` form of a repository URL, for the image's
/// `IMAGE_URL`/`IMAGE_SOURCE` labels: `git@host:o/r.git` and
/// `ssh://git@host/o/r` become `https://host/o/r`, a trailing `.git` or `/`
/// goes. Anything else (`file://`) is returned as given.
pub fn web_url(url: &str) -> String {
    let url = url.trim();
    let rebuilt = if let Some(rest) = url.strip_prefix("git@") {
        rest.split_once(':')
            .map(|(host, path)| format!("https://{host}/{path}"))
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        let rest = rest.split_once('@').map_or(rest, |(_, r)| r);
        rest.split_once('/').map(|(authority, path)| {
            let host = authority.split(':').next().unwrap_or(authority);
            format!("https://{host}/{path}")
        })
    } else {
        // `https://user:token@host/…` → `https://host/…`: the web address has
        // no use for credentials, and this is what labels and `IMAGE_URL`
        // carry (validate refuses such a URL; this is the second line).
        ["https://", "http://"].iter().find_map(|scheme| {
            let rest = url.strip_prefix(scheme)?;
            let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
            let (_, host) = authority.rsplit_once('@')?;
            Some(format!("{scheme}{host}{}", &rest[authority.len()..]))
        })
    };
    let mut out = rebuilt.unwrap_or_else(|| url.to_string());
    while out.ends_with('/') {
        out.pop();
    }
    if let Some(stripped) = out.strip_suffix(".git") {
        out = stripped.to_string();
    }
    out
}

// ---------------------------------------------------------------------------
// Dockerfile profiles
// ---------------------------------------------------------------------------

/// One preset edit: a [`BuildEdit`] as static data.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PresetEdit {
    pub name: &'static str,
    pub role: EditRole,
    pub required: bool,
    pub find: &'static str,
    pub replace: &'static str,
}

impl PresetEdit {
    pub fn to_edit(&self) -> BuildEdit {
        BuildEdit {
            name: self.name.to_string(),
            role: self.role,
            find: self.find.to_string(),
            replace: self.replace.to_string(),
            required: self.required,
        }
    }
}

/// How one upstream Dockerfile is built (§3 "Engine preset").
#[derive(Debug, Clone, Copy, Serialize)]
pub struct DockerfileProfile {
    /// Stable identifier, for logs and the editor (`llama-official-cuda`).
    pub id: &'static str,
    pub engine: Engine,
    pub flavor: Flavor,
    pub backend: GpuBackend,
    /// Repository-relative path.
    pub dockerfile: &'static str,
    /// Built and verified end to end in the WP0 spike (§14). `false`: read off
    /// the upstream file, edits proven to match it, never built.
    pub tested: bool,
    /// Final-stage candidates, preferred first.
    pub targets: &'static [&'static str],
    /// The build arg the arch list goes into (`CUDA_DOCKER_ARCH`, sd.cpp's
    /// `CUDA_ARCHITECTURES`, `ROCM_DOCKER_ARCH`); `None` for backends with no
    /// architecture list (Vulkan, CPU).
    pub arch_arg: Option<&'static str>,
    /// Applied in order.
    pub edits: &'static [PresetEdit],
}

impl DockerfileProfile {
    /// The edits a build starts with (§4), minus the cache ones when the
    /// build has ccache off.
    pub fn edits_for(&self, ccache: bool) -> Vec<BuildEdit> {
        self.edits
            .iter()
            .filter(|e| ccache || !e.role.needs_ccache())
            .map(PresetEdit::to_edit)
            .collect()
    }

    pub fn verify(&self) -> VerifySpec {
        verify_spec(self.flavor, self.backend)
    }
}

/// The `RUN` prefix that mounts the build's ccache and zeroes its stats, so
/// the `ccache -s` at the end of the step reports this build alone.
///
/// A build with extras compiles into a cache of its own, so PR code never
/// writes where master builds read — but it starts from the shared one:
/// `{{ccache_shared_mount}}` adds the engine's plain cache read-only and
/// `{{ccache_seed}}` copies what the own cache lacks from it (reflinked where
/// the filesystem can). For a plain build both render empty, which keeps its
/// Containerfile the spike's byte for byte.
macro_rules! ccache_run {
    () => {
        "RUN --mount=type=cache,id={{ccache_id}},target=/ccache{{ccache_shared_mount}} export \
         CCACHE_DIR=/ccache CCACHE_MAXSIZE={{ccache_max_size}} && {{ccache_seed}}ccache -z && "
    };
}

/// ik_llama.cpp ignores `-DLLAMA_BUILD_NUMBER` (its `CMakeLists.txt:169` sets
/// it unconditionally), so the numbers are written into
/// `cmake/build-info.cmake` inside the compile step (§14.2, verified:
/// `version: 4960 (1aaf710)`).
macro_rules! ik_build_info_sed {
    () => {
        r#"sed -i "s/^set(BUILD_NUMBER 0)/set(BUILD_NUMBER ${LLAMA_BUILD_NUMBER})/; s/^set(BUILD_COMMIT \"unknown\")/set(BUILD_COMMIT \"${LLAMA_BUILD_COMMIT}\")/" cmake/build-info.cmake && "#
    };
}

/// audio.cpp forces `GGML_CCACHE OFF` and reads its commit with a `git` the
/// build image does not have, so both are wired by hand (§14.2).
macro_rules! audio_git_info_sed {
    () => {
        r#"sed -i "s/^set(AUDIOCPP_GIT_SHA \"unknown\")/set(AUDIOCPP_GIT_SHA \"${AUDIOCPP_GIT_SHA}\")/; s/^set(AUDIOCPP_GIT_DATE \"unknown\")/set(AUDIOCPP_GIT_DATE \"${AUDIOCPP_GIT_DATE}\")/" CMakeLists.txt && "#
    };
}

/// stable-diffusion.cpp's `CMakeLists.txt` asks `git describe` / `git
/// rev-parse --short HEAD` and falls back to `unknown` when there is no
/// `.git` (there is none in the context). A `-D` would lose to the empty
/// `execute_process` output, so the fallback itself is rewritten, the way
/// ik's is. Verified by the e2e build of 2f88688 (`stable-diffusion.cpp
/// version b920, commit 2f88688`); still not required, since the build info
/// is worth less than a build that runs when upstream rewords the fallback.
macro_rules! sd_build_info_sed {
    () => {
        r#"sed -i "s/set(SDCPP_BUILD_VERSION unknown)/set(SDCPP_BUILD_VERSION ${SDCPP_BUILD_VERSION})/; s/set(SDCPP_BUILD_COMMIT unknown)/set(SDCPP_BUILD_COMMIT ${SDCPP_BUILD_COMMIT})/" CMakeLists.txt && "#
    };
}

/// The web UI stage's npm cache mount, seeded from the shared one the same
/// way [`ccache_run!`] is: `{{npm_shared_mount}}` and `{{npm_seed}}` render
/// empty for a plain build, so its Containerfile stays the spike's byte for
/// byte (§14.2 "npm follows the same pattern").
const NPM_CACHE: PresetEdit = PresetEdit {
    name: "npm-cache",
    role: EditRole::Cache,
    required: false,
    find: "RUN npm ci",
    replace: "RUN --mount=type=cache,id={{npm_cache_id}},target=/root/.npm{{npm_shared_mount}} \
              {{npm_seed}}npm ci",
};

const OFFICIAL_BUILD_NUMBER_CMAKE_B: PresetEdit = PresetEdit {
    name: "build-number-cmake",
    role: EditRole::BuildInfo,
    required: true,
    find: "cmake -B build ",
    replace: "cmake -B build -DLLAMA_BUILD_NUMBER=${LLAMA_BUILD_NUMBER} \
              -DLLAMA_BUILD_COMMIT=${LLAMA_BUILD_COMMIT} ",
};

const OFFICIAL_BUILD_NUMBER_CMAKE_S: PresetEdit = PresetEdit {
    name: "build-number-cmake",
    role: EditRole::BuildInfo,
    required: true,
    find: "cmake -S . -B build ",
    replace: "cmake -S . -B build -DLLAMA_BUILD_NUMBER=${LLAMA_BUILD_NUMBER} \
              -DLLAMA_BUILD_COMMIT=${LLAMA_BUILD_COMMIT} ",
};

const OFFICIAL_CCACHE_STATS: PresetEdit = PresetEdit {
    name: "ccache-stats",
    role: EditRole::Ccache,
    required: false,
    find: "cmake --build build --config Release -j$(nproc)\n",
    replace: "cmake --build build --config Release -j$(nproc) && ccache -s\n",
};

const OFFICIAL_CUDA: &[PresetEdit] = &[
    NPM_CACHE,
    PresetEdit {
        name: "ccache-pkg",
        role: EditRole::Ccache,
        required: true,
        find: "build-essential cmake",
        replace: "build-essential cmake ccache",
    },
    PresetEdit {
        name: "ccache-mount",
        role: EditRole::Ccache,
        required: true,
        find: r#"RUN if [ "${CUDA_DOCKER_ARCH}" != "default" ]"#,
        replace: concat!(
            ccache_run!(),
            r#"if [ "${CUDA_DOCKER_ARCH}" != "default" ]"#
        ),
    },
    OFFICIAL_CCACHE_STATS,
    PresetEdit {
        name: "build-number-args",
        role: EditRole::BuildInfo,
        required: true,
        find: "ARG CUDA_DOCKER_ARCH=default\n",
        replace: "ARG CUDA_DOCKER_ARCH=default\n\
                  ARG LLAMA_BUILD_NUMBER=0\n\
                  ARG LLAMA_BUILD_COMMIT=unknown\n",
    },
    OFFICIAL_BUILD_NUMBER_CMAKE_B,
];

const OFFICIAL_VULKAN: &[PresetEdit] = &[
    NPM_CACHE,
    PresetEdit {
        name: "ccache-pkg",
        role: EditRole::Ccache,
        required: true,
        find: "build-essential cmake",
        replace: "build-essential cmake ccache",
    },
    PresetEdit {
        name: "ccache-mount",
        role: EditRole::Ccache,
        required: true,
        find: "RUN cmake -B build ",
        replace: concat!(ccache_run!(), "cmake -B build "),
    },
    OFFICIAL_CCACHE_STATS,
    PresetEdit {
        name: "build-number-args",
        role: EditRole::BuildInfo,
        required: true,
        find: "FROM docker.io/ubuntu:$UBUNTU_VERSION AS build\n",
        replace: concat!(
            "FROM docker.io/ubuntu:$UBUNTU_VERSION AS build\n",
            "ARG LLAMA_BUILD_NUMBER=0\nARG LLAMA_BUILD_COMMIT=unknown\n"
        ),
    },
    OFFICIAL_BUILD_NUMBER_CMAKE_B,
];

const OFFICIAL_ROCM: &[PresetEdit] = &[
    NPM_CACHE,
    PresetEdit {
        name: "ccache-pkg",
        role: EditRole::Ccache,
        required: true,
        find: "    build-essential \\\n    cmake \\\n",
        replace: "    build-essential \\\n    cmake \\\n    ccache \\\n",
    },
    PresetEdit {
        name: "ccache-mount",
        role: EditRole::Ccache,
        required: true,
        find: "RUN HIPCXX=",
        replace: concat!(ccache_run!(), "HIPCXX="),
    },
    OFFICIAL_CCACHE_STATS,
    PresetEdit {
        name: "build-number-args",
        role: EditRole::BuildInfo,
        required: true,
        find: "FROM ${BASE_ROCM_DEV_CONTAINER} AS build\n",
        replace: concat!(
            "FROM ${BASE_ROCM_DEV_CONTAINER} AS build\n",
            "ARG LLAMA_BUILD_NUMBER=0\nARG LLAMA_BUILD_COMMIT=unknown\n"
        ),
    },
    OFFICIAL_BUILD_NUMBER_CMAKE_S,
];

const OFFICIAL_CPU: &[PresetEdit] = &[
    NPM_CACHE,
    PresetEdit {
        name: "ccache-pkg",
        role: EditRole::Ccache,
        required: true,
        find: "build-essential git cmake",
        replace: "build-essential git cmake ccache",
    },
    PresetEdit {
        name: "ccache-mount",
        role: EditRole::Ccache,
        required: true,
        find: r#"RUN if [ "$TARGETARCH" = "amd64" ]"#,
        replace: concat!(ccache_run!(), r#"if [ "$TARGETARCH" = "amd64" ]"#),
    },
    PresetEdit {
        name: "ccache-stats",
        role: EditRole::Ccache,
        required: false,
        find: "cmake --build build -j $(nproc)\n",
        replace: "cmake --build build -j $(nproc) && ccache -s\n",
    },
    PresetEdit {
        name: "build-number-args",
        role: EditRole::BuildInfo,
        required: true,
        find: "FROM docker.io/ubuntu:$UBUNTU_VERSION AS build\n",
        replace: concat!(
            "FROM docker.io/ubuntu:$UBUNTU_VERSION AS build\n",
            "ARG LLAMA_BUILD_NUMBER=0\nARG LLAMA_BUILD_COMMIT=unknown\n"
        ),
    },
    OFFICIAL_BUILD_NUMBER_CMAKE_S,
];

const IK_CUDA: &[PresetEdit] = &[
    PresetEdit {
        name: "qualify-base-images",
        role: EditRole::Qualify,
        required: false,
        find: "=nvidia/cuda:",
        replace: "=docker.io/nvidia/cuda:",
    },
    PresetEdit {
        name: "ccache-pkg",
        role: EditRole::Ccache,
        required: true,
        find: "apt-get install -y build-essential git",
        replace: "apt-get install -y build-essential ccache git",
    },
    // Anchored on the stage line rather than the spike's
    // `ARG CUDA_DOCKER_ARCH="86;90"`: that default is the line most likely to
    // change upstream (it lacks sm_89, §2.1).
    PresetEdit {
        name: "build-number-args",
        role: EditRole::BuildInfo,
        required: true,
        find: "FROM ${BASE_CUDA_DEV_CONTAINER} AS build\n",
        replace: concat!(
            "FROM ${BASE_CUDA_DEV_CONTAINER} AS build\n",
            "ARG LLAMA_BUILD_NUMBER=0\nARG LLAMA_BUILD_COMMIT=unknown\n"
        ),
    },
    PresetEdit {
        name: "ccache-mount",
        role: EditRole::Ccache,
        required: true,
        find: "RUN cmake -S . -B build -G Ninja",
        replace: concat!(ccache_run!(), "cmake -S . -B build -G Ninja"),
    },
    PresetEdit {
        name: "build-number",
        role: EditRole::BuildInfo,
        required: true,
        find: "cmake -S . -B build -G Ninja",
        replace: concat!(ik_build_info_sed!(), "cmake -S . -B build -G Ninja"),
    },
    PresetEdit {
        name: "ccache-stats",
        role: EditRole::Ccache,
        required: false,
        find: " && cmake --build build --target llama-server\n",
        replace: " && cmake --build build --target llama-server && ccache -s\n",
    },
];

const IK_VULKAN: &[PresetEdit] = &[
    PresetEdit {
        name: "qualify-base-images",
        role: EditRole::Qualify,
        required: false,
        find: "FROM ubuntu:",
        replace: "FROM docker.io/library/ubuntu:",
    },
    PresetEdit {
        name: "ccache-pkg",
        role: EditRole::Ccache,
        required: true,
        find: "build-essential cmake",
        replace: "build-essential cmake ccache",
    },
    PresetEdit {
        name: "build-number-args",
        role: EditRole::BuildInfo,
        required: true,
        find: " AS build\n",
        replace: " AS build\nARG LLAMA_BUILD_NUMBER=0\nARG LLAMA_BUILD_COMMIT=unknown\n",
    },
    PresetEdit {
        name: "ccache-mount",
        role: EditRole::Ccache,
        required: true,
        find: "RUN cmake -B build ",
        replace: concat!(ccache_run!(), "cmake -B build "),
    },
    PresetEdit {
        name: "build-number",
        role: EditRole::BuildInfo,
        required: true,
        find: "cmake -B build ",
        replace: concat!(ik_build_info_sed!(), "cmake -B build "),
    },
    PresetEdit {
        name: "ccache-stats",
        role: EditRole::Ccache,
        required: false,
        find: "cmake --build build --config Release --target llama-server\n",
        replace: "cmake --build build --config Release --target llama-server && ccache -s\n",
    },
];

const AUDIO_CCACHE_STATS: PresetEdit = PresetEdit {
    name: "ccache-stats",
    role: EditRole::Ccache,
    required: false,
    find: "        --target model_perf\n",
    replace: "        --target model_perf && ccache -s\n",
};

/// eSpeak NG in the runtime image. Kokoro (and the other phonemizing
/// families) link `libespeak-ng.so.1` at run time and fail to load without
/// it; upstream's runtime stages install `libgomp1 curl ffmpeg python3
/// ca-certificates` and nothing else, and ghcr's `full-cuda12` lacks it too.
/// Ubuntu 24.04 ships the library (1.51, found by soname) and its data on
/// the default path, about 10 MB of apt. The anchor is the tail every
/// runtime stage (cuda, vulkan, cpu) shares; the build stage's
/// `cmake ca-certificates` does not contain it. Not required: a build whose
/// upstream rewords the line still runs, it only lacks Kokoro. Rejected:
/// `-DAUDIOCPP_STATIC_ESPEAK=ON` (bypasses ccache, a second copy of the data,
/// writes under `$HOME/.cache` at run time).
const AUDIO_ESPEAK: PresetEdit = PresetEdit {
    name: "runtime-espeak",
    role: EditRole::Other,
    required: false,
    find: "curl ffmpeg python3 ca-certificates",
    replace: "curl ffmpeg python3 ca-certificates libespeak-ng1 espeak-ng-data",
};

const AUDIO_CUDA: &[PresetEdit] = &[
    PresetEdit {
        name: "ccache-pkg",
        role: EditRole::Ccache,
        required: true,
        find: "gcc-${GCC_VERSION} g++-${GCC_VERSION} cmake ca-certificates",
        replace: "gcc-${GCC_VERSION} g++-${GCC_VERSION} cmake ccache ca-certificates",
    },
    PresetEdit {
        name: "ccache-mount",
        role: EditRole::Ccache,
        required: true,
        find: r#"RUN if [ "${CUDA_DOCKER_ARCH}" != "default" ]"#,
        replace: concat!(
            ccache_run!(),
            r#"if [ "${CUDA_DOCKER_ARCH}" != "default" ]"#
        ),
    },
    PresetEdit {
        name: "git-info",
        role: EditRole::BuildInfo,
        required: true,
        find: r#"if [ "${CUDA_DOCKER_ARCH}" != "default" ]"#,
        replace: concat!(
            audio_git_info_sed!(),
            r#"if [ "${CUDA_DOCKER_ARCH}" != "default" ]"#
        ),
    },
    PresetEdit {
        name: "git-info-args",
        role: EditRole::BuildInfo,
        required: true,
        find: "ARG CUDA_DOCKER_ARCH=default\n",
        replace: "ARG CUDA_DOCKER_ARCH=default\n\
                  ARG AUDIOCPP_GIT_SHA=unknown\n\
                  ARG AUDIOCPP_GIT_DATE=unknown\n",
    },
    PresetEdit {
        name: "ccache-launchers",
        role: EditRole::Ccache,
        required: true,
        find: "-DCMAKE_BUILD_TYPE=Release \\\n",
        replace: "-DCMAKE_BUILD_TYPE=Release \\\n        -DCMAKE_C_COMPILER_LAUNCHER=ccache \
                  -DCMAKE_CXX_COMPILER_LAUNCHER=ccache -DCMAKE_CUDA_COMPILER_LAUNCHER=ccache \\\n",
    },
    AUDIO_CCACHE_STATS,
    AUDIO_ESPEAK,
];

/// audio.cpp's Vulkan and CPU Dockerfiles differ from the CUDA one only
/// where these anchors had to (no `CUDA_DOCKER_ARCH`, a plain `RUN cmake`).
const AUDIO_PLAIN: &[PresetEdit] = &[
    PresetEdit {
        name: "ccache-pkg",
        role: EditRole::Ccache,
        required: true,
        find: "make cmake libgomp1",
        replace: "make cmake ccache libgomp1",
    },
    PresetEdit {
        name: "ccache-mount",
        role: EditRole::Ccache,
        required: true,
        find: "RUN cmake -S . -B build",
        replace: concat!(ccache_run!(), "cmake -S . -B build"),
    },
    PresetEdit {
        name: "git-info",
        role: EditRole::BuildInfo,
        required: true,
        find: "cmake -S . -B build",
        replace: concat!(audio_git_info_sed!(), "cmake -S . -B build"),
    },
    PresetEdit {
        name: "git-info-args",
        role: EditRole::BuildInfo,
        required: true,
        find: "ARG AUDIOCPP_VERSION=dev\n",
        replace: "ARG AUDIOCPP_VERSION=dev\n\
                  ARG AUDIOCPP_GIT_SHA=unknown\n\
                  ARG AUDIOCPP_GIT_DATE=unknown\n",
    },
    PresetEdit {
        name: "ccache-launchers",
        role: EditRole::Ccache,
        required: true,
        find: "-DCMAKE_BUILD_TYPE=Release \\\n",
        replace: "-DCMAKE_BUILD_TYPE=Release \\\n        -DCMAKE_C_COMPILER_LAUNCHER=ccache \
                  -DCMAKE_CXX_COMPILER_LAUNCHER=ccache \\\n",
    },
    AUDIO_CCACHE_STATS,
    AUDIO_ESPEAK,
];

const SD_QUALIFY_UBUNTU: PresetEdit = PresetEdit {
    name: "qualify-base-images",
    role: EditRole::Qualify,
    required: false,
    find: "FROM ubuntu:",
    replace: "FROM docker.io/library/ubuntu:",
};

const SD_CCACHE_PKG: PresetEdit = PresetEdit {
    name: "ccache-pkg",
    role: EditRole::Ccache,
    required: true,
    find: "build-essential git cmake",
    replace: "build-essential git ccache cmake",
};

const SD_CCACHE_MOUNT: PresetEdit = PresetEdit {
    name: "ccache-mount",
    role: EditRole::Ccache,
    required: true,
    find: "RUN cmake --build ./build --config Release -j$(nproc)",
    replace: concat!(
        ccache_run!(),
        "cmake --build ./build --config Release -j$(nproc) && ccache -s"
    ),
};

const SD_BUILD_INFO: [PresetEdit; 2] = [
    PresetEdit {
        name: "build-info-args",
        role: EditRole::BuildInfo,
        required: false,
        find: "WORKDIR /sd.cpp\n",
        replace: "WORKDIR /sd.cpp\n\
                  ARG SDCPP_BUILD_VERSION=unknown\n\
                  ARG SDCPP_BUILD_COMMIT=unknown\n",
    },
    PresetEdit {
        name: "build-info",
        role: EditRole::BuildInfo,
        required: false,
        find: "cmake . -B ./build",
        replace: concat!(sd_build_info_sed!(), "cmake . -B ./build"),
    },
];

const SD_CUDA: &[PresetEdit] = &[
    PresetEdit {
        name: "qualify-base-images",
        role: EditRole::Qualify,
        required: false,
        find: "FROM nvidia/cuda:",
        replace: "FROM docker.io/nvidia/cuda:",
    },
    // sd.cpp needs no cuDNN; the plain images are the ones every other
    // engine's build already pulled (§14.2).
    PresetEdit {
        name: "drop-cudnn",
        role: EditRole::BaseImage,
        required: false,
        find: "-cudnn-",
        replace: "-",
    },
    SD_CCACHE_MOUNT,
    SD_BUILD_INFO[0],
    SD_BUILD_INFO[1],
];

const SD_VULKAN: &[PresetEdit] = &[
    SD_QUALIFY_UBUNTU,
    SD_CCACHE_PKG,
    SD_CCACHE_MOUNT,
    SD_BUILD_INFO[0],
    SD_BUILD_INFO[1],
];

const SD_CPU: &[PresetEdit] = &[
    SD_QUALIFY_UBUNTU,
    SD_CCACHE_PKG,
    PresetEdit {
        name: "ccache-mount",
        role: EditRole::Ccache,
        required: true,
        find: "RUN cmake --build ./build --config Release --parallel",
        replace: concat!(
            ccache_run!(),
            "cmake --build ./build --config Release --parallel && ccache -s"
        ),
    },
    SD_BUILD_INFO[0],
    SD_BUILD_INFO[1],
];

/// Every known Dockerfile, in candidate order per `(engine, backend)`.
///
/// Left out on purpose: ik_llama.cpp's `llama-server.Dockerfile` (CPU) and
/// `llama-server-rocm.Dockerfile` still run `make`, and ik has no Makefile
/// any more — they cannot build. A build that wants them sets `dockerfile`.
pub const PROFILES: [DockerfileProfile; 12] = [
    DockerfileProfile {
        id: "llama-official-cuda",
        engine: Engine::Llama,
        flavor: Flavor::Official,
        backend: GpuBackend::Cuda,
        dockerfile: ".devops/cuda.Dockerfile",
        tested: true,
        targets: &["server"],
        arch_arg: Some("CUDA_DOCKER_ARCH"),
        edits: OFFICIAL_CUDA,
    },
    DockerfileProfile {
        id: "llama-ik-cuda",
        engine: Engine::Llama,
        flavor: Flavor::Ik,
        backend: GpuBackend::Cuda,
        dockerfile: ".devops/llama-server-cuda.Dockerfile",
        tested: true,
        targets: &["runtime"],
        arch_arg: Some("CUDA_DOCKER_ARCH"),
        edits: IK_CUDA,
    },
    DockerfileProfile {
        id: "llama-official-vulkan",
        engine: Engine::Llama,
        flavor: Flavor::Official,
        backend: GpuBackend::Vulkan,
        dockerfile: ".devops/vulkan.Dockerfile",
        tested: false,
        targets: &["server"],
        arch_arg: None,
        edits: OFFICIAL_VULKAN,
    },
    DockerfileProfile {
        id: "llama-ik-vulkan",
        engine: Engine::Llama,
        flavor: Flavor::Ik,
        backend: GpuBackend::Vulkan,
        dockerfile: ".devops/llama-server-vulkan.Dockerfile",
        tested: false,
        // A single stage, and it is the image.
        targets: &["build"],
        arch_arg: None,
        edits: IK_VULKAN,
    },
    DockerfileProfile {
        id: "llama-official-rocm",
        engine: Engine::Llama,
        flavor: Flavor::Official,
        backend: GpuBackend::Rocm,
        dockerfile: ".devops/rocm.Dockerfile",
        tested: false,
        targets: &["server"],
        arch_arg: Some("ROCM_DOCKER_ARCH"),
        edits: OFFICIAL_ROCM,
    },
    DockerfileProfile {
        id: "llama-official-cpu",
        engine: Engine::Llama,
        flavor: Flavor::Official,
        backend: GpuBackend::Cpu,
        dockerfile: ".devops/cpu.Dockerfile",
        tested: false,
        targets: &["server"],
        arch_arg: None,
        edits: OFFICIAL_CPU,
    },
    DockerfileProfile {
        id: "audio-cuda",
        engine: Engine::Audio,
        flavor: Flavor::Audio,
        backend: GpuBackend::Cuda,
        dockerfile: ".devops/cuda.Dockerfile",
        tested: true,
        targets: &["full"],
        arch_arg: Some("CUDA_DOCKER_ARCH"),
        edits: AUDIO_CUDA,
    },
    DockerfileProfile {
        id: "audio-vulkan",
        engine: Engine::Audio,
        flavor: Flavor::Audio,
        backend: GpuBackend::Vulkan,
        dockerfile: ".devops/vulkan.Dockerfile",
        tested: false,
        targets: &["full"],
        arch_arg: None,
        edits: AUDIO_PLAIN,
    },
    DockerfileProfile {
        id: "audio-cpu",
        engine: Engine::Audio,
        flavor: Flavor::Audio,
        backend: GpuBackend::Cpu,
        dockerfile: ".devops/cpu.Dockerfile",
        tested: false,
        targets: &["full"],
        arch_arg: None,
        edits: AUDIO_PLAIN,
    },
    DockerfileProfile {
        id: "sdcpp-cuda",
        engine: Engine::Sdcpp,
        flavor: Flavor::Sdcpp,
        backend: GpuBackend::Cuda,
        dockerfile: "docker/Dockerfile.cuda",
        tested: true,
        targets: &["runtime"],
        arch_arg: Some("CUDA_ARCHITECTURES"),
        edits: SD_CUDA,
    },
    DockerfileProfile {
        id: "sdcpp-vulkan",
        engine: Engine::Sdcpp,
        flavor: Flavor::Sdcpp,
        backend: GpuBackend::Vulkan,
        dockerfile: "docker/Dockerfile.vulkan",
        tested: false,
        targets: &["runtime"],
        arch_arg: None,
        edits: SD_VULKAN,
    },
    DockerfileProfile {
        id: "sdcpp-cpu",
        engine: Engine::Sdcpp,
        flavor: Flavor::Sdcpp,
        backend: GpuBackend::Cpu,
        dockerfile: "docker/Dockerfile",
        tested: false,
        targets: &["runtime"],
        arch_arg: None,
        edits: SD_CPU,
    },
];

/// The profile for exactly this `(engine, backend, dockerfile)`, if any.
pub fn profile_for(
    engine: Engine,
    backend: GpuBackend,
    dockerfile: &str,
) -> Option<&'static DockerfileProfile> {
    PROFILES
        .iter()
        .find(|p| p.engine == engine && p.backend == backend && p.dockerfile == dockerfile)
}

/// The Dockerfiles "auto" tries for `(engine, backend)`, in order (§4).
pub fn dockerfile_candidates(engine: Engine, backend: GpuBackend) -> Vec<&'static str> {
    PROFILES
        .iter()
        .filter(|p| p.engine == engine && p.backend == backend)
        .map(|p| p.dockerfile)
        .collect()
}

/// What a run tries, in order: the build's own `dockerfile` when set, else
/// [`dockerfile_candidates`]. The run reads each from the commit it builds
/// and takes the first that exists; [`no_dockerfile_error`] words the failure.
pub fn dockerfiles_to_try(spec: &BuildSpec) -> Vec<String> {
    match spec.dockerfile.as_deref() {
        Some(d) => vec![d.to_string()],
        None => dockerfile_candidates(spec.engine, spec.backend)
            .into_iter()
            .map(str::to_string)
            .collect(),
    }
}

/// The refusal when none of [`dockerfiles_to_try`] exists at `sha`.
pub fn no_dockerfile_error(spec: &BuildSpec, sha: &str) -> String {
    let at = format!("{} at {}", spec.repo_url, tags_short(sha));
    match spec.dockerfile.as_deref() {
        Some(d) => format!("the Dockerfile {d} does not exist in {at}"),
        None => {
            let tried = dockerfile_candidates(spec.engine, spec.backend);
            if tried.is_empty() {
                format!(
                    "there is no preset Dockerfile for {} with the {} backend — set dockerfile \
                     (Advanced) to the repository's Dockerfile",
                    spec.engine.as_str(),
                    spec.backend.as_str()
                )
            } else {
                format!(
                    "none of the preset Dockerfiles for {} with the {} backend ({}) exists in \
                     {at} — set dockerfile (Advanced) to the repository's Dockerfile",
                    spec.engine.as_str(),
                    spec.backend.as_str(),
                    tried.join(", ")
                )
            }
        }
    }
}

fn tags_short(sha: &str) -> &str {
    sha.get(..tags::BASE_LEN).unwrap_or(sha)
}

/// Final-stage candidates for a Dockerfile no profile knows.
fn fallback_targets(engine: Engine) -> &'static [&'static str] {
    match engine {
        Engine::Llama => &["server", "runtime"],
        Engine::Audio => &["full"],
        Engine::Sdcpp => &["runtime"],
    }
}

/// The resolved CUDA version (§4): the build's own, else
/// [`DEFAULT_CUDA_VERSION`] — for the CUDA backend only.
pub fn resolve_cuda_version(spec: &BuildSpec) -> Option<String> {
    (spec.backend == GpuBackend::Cuda).then(|| {
        spec.cuda_version
            .clone()
            .unwrap_or_else(|| DEFAULT_CUDA_VERSION.to_string())
    })
}

/// The resolved arch list (§4). CUDA: the build's own, else the host GPUs'
/// compute capabilities ([`detect_cuda_arch`]) — a detection failure is an
/// error naming the field, never a silent "all archs". ROCm: the build's own,
/// else empty (the Dockerfile's fat default; nothing detects AMD targets yet).
/// Vulkan and CPU have no arch list; one set anyway is returned as given, and
/// [`build_args`] notes that it went nowhere.
pub async fn resolve_arch(
    backend: GpuBackend,
    spec_arch: Option<&[String]>,
    runner: &dyn CommandRunner,
) -> Result<Vec<String>, String> {
    if let Some(list) = spec_arch.filter(|l| !l.is_empty()) {
        return Ok(list.to_vec());
    }
    match backend {
        GpuBackend::Cuda => detect_cuda_arch(runner).await.map_err(|e| {
            format!("{e} — set arch (e.g. 89) on the build to build without detecting it")
        }),
        GpuBackend::Rocm | GpuBackend::Vulkan | GpuBackend::Cpu => Ok(Vec::new()),
    }
}

// ---------------------------------------------------------------------------
// Choosing target and edits for a Dockerfile
// ---------------------------------------------------------------------------

/// What "auto" became for one Dockerfile (§5 steps 2 and 4): the parts of
/// [`super::model::ResolvedInputs`] that come from the Dockerfile, plus how
/// to verify the image.
#[derive(Debug, Clone, Serialize)]
pub struct DockerfileChoice {
    pub dockerfile: String,
    pub profile: Option<&'static DockerfileProfile>,
    /// Empty only for a Dockerfile whose last stage has no name: build it
    /// without `--target`.
    pub target: String,
    /// Templated, not rendered: exactly what `ResolvedInputs::edits` records
    /// and the config hash covers.
    pub edits: Vec<BuildEdit>,
    pub verify: VerifySpec,
    /// Visible notes for the run log: fallbacks taken, untested profile, …
    pub notes: Vec<String>,
}

/// Resolve target, edits and verify probes for `dockerfile`, whose text (as
/// the run will build it) is `text`.
pub fn choose_dockerfile(
    spec: &BuildSpec,
    dockerfile: &str,
    text: &str,
) -> Result<DockerfileChoice, String> {
    let info = DockerfileInfo::parse(text);
    let profile = profile_for(spec.engine, spec.backend, dockerfile);
    let mut notes = Vec::new();
    if let Some(p) = profile {
        if !p.tested {
            notes.push(format!(
                "the {} preset for {dockerfile} has never been built by lmgw — its edits match \
                 the upstream file, but expect to adjust them",
                p.id
            ));
        }
    }

    let target = match spec.target.as_deref() {
        Some(t) => {
            if !info.has_stage(t) {
                return Err(format!(
                    "target '{t}' is not a stage of {dockerfile} (its stages: {})",
                    info.stage_list()
                ));
            }
            t.to_string()
        }
        None => {
            let preferred = profile.map_or(fallback_targets(spec.engine), |p| p.targets);
            match info.pick_target(preferred) {
                Some(t) => t,
                None => match info.stages.last() {
                    Some(Stage { name: Some(n), .. }) => {
                        notes.push(format!(
                            "none of the preferred stages ({}) is in {dockerfile}; building its \
                             last stage, {n}",
                            preferred.join(", ")
                        ));
                        n.clone()
                    }
                    Some(Stage { name: None, .. }) => {
                        notes.push(format!(
                            "{dockerfile} names no preferred stage ({}); building its last, \
                             unnamed stage",
                            preferred.join(", ")
                        ));
                        String::new()
                    }
                    None => return Err(format!("{dockerfile} has no FROM line")),
                },
            }
        }
    };

    let edits = match &spec.edits {
        Some(own) => own
            .iter()
            .filter(|e| spec.ccache || !e.role.needs_ccache())
            .cloned()
            .collect(),
        None => match profile {
            Some(p) => p.edits_for(spec.ccache),
            None => {
                notes.push(format!(
                    "no preset knows {dockerfile}, so no edits are applied — no ccache, no build \
                     number; add edits under Advanced if you want them"
                ));
                Vec::new()
            }
        },
    };

    let flavor = profile.map(|p| p.flavor).unwrap_or_else(|| {
        repo_preset_for_url(&spec.repo_url)
            .filter(|p| p.engine == spec.engine)
            .map_or(Flavor::default_for(spec.engine), |p| p.flavor)
    });

    Ok(DockerfileChoice {
        dockerfile: dockerfile.to_string(),
        profile,
        target,
        edits,
        verify: verify_spec(flavor, spec.backend),
        notes,
    })
}

// ---------------------------------------------------------------------------
// Applying edits
// ---------------------------------------------------------------------------

/// The per-run values the edit templates take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateVars {
    pub ccache_id: String,
    pub npm_cache_id: String,
    pub ccache_max_size: String,
    /// The shared cache a build with extras reads (read-only) and seeds its
    /// own from; `None` for a plain build, whose own cache *is* the shared one.
    pub ccache_shared_id: Option<String>,
    /// [`Self::ccache_shared_id`]'s counterpart for the npm cache.
    pub npm_shared_id: Option<String>,
}

impl TemplateVars {
    /// The values for a run: the ccache id of [`tags::ccache_id`], the npm
    /// cache id of [`tags::npm_cache_id`], the build's `CCACHE_MAXSIZE`, and —
    /// with extras — the plain build's ccache and npm cache ids to read from.
    pub fn new(
        engine: Engine,
        backend: GpuBackend,
        slug: &str,
        has_extras: bool,
        ccache_max_size: &str,
    ) -> Self {
        Self {
            ccache_id: tags::ccache_id(engine, backend, slug, has_extras),
            npm_cache_id: tags::npm_cache_id(engine, slug, has_extras),
            ccache_max_size: ccache_max_size.to_string(),
            ccache_shared_id: has_extras.then(|| tags::ccache_id(engine, backend, slug, false)),
            npm_shared_id: has_extras.then(|| tags::npm_cache_id(engine, slug, false)),
        }
    }

    /// `{{ccache_shared_mount}}`: a second, read-only cache mount of the
    /// shared cache (buildah honours `ro` on cache mounts: a write fails).
    fn ccache_shared_mount(&self) -> String {
        self.ccache_shared_id
            .as_deref()
            .map_or_else(String::new, |id| {
                format!(" --mount=type=cache,id={id},target={CCACHE_SHARED_TARGET},ro")
            })
    }

    /// `{{ccache_seed}}`: copy into the own cache every entry of the shared
    /// one it lacks or has an older copy of. Every run, not only the first:
    /// master builds keep adding to the shared cache, and a refresh with
    /// nothing new takes hundredths of a second. `--reflink=auto` makes the
    /// copy free on btrfs/XFS (and a real copy elsewhere); `-u` and `.` work
    /// in coreutils 8.32 (ik's ubuntu 22.04) as in 9.x.
    fn ccache_seed(&self) -> String {
        if self.ccache_shared_id.is_some() {
            format!("cp -a -u --reflink=auto {CCACHE_SHARED_TARGET}/. /ccache/ && ")
        } else {
            String::new()
        }
    }

    /// `{{npm_shared_mount}}`: [`Self::ccache_shared_mount`]'s counterpart for
    /// the npm cache.
    fn npm_shared_mount(&self) -> String {
        self.npm_shared_id
            .as_deref()
            .map_or_else(String::new, |id| {
                format!(" --mount=type=cache,id={id},target={NPM_CACHE_SHARED_TARGET},ro")
            })
    }

    /// `{{npm_seed}}`: [`Self::ccache_seed`]'s counterpart for the npm cache.
    fn npm_seed(&self) -> String {
        if self.npm_shared_id.is_some() {
            format!("cp -a -u --reflink=auto {NPM_CACHE_SHARED_TARGET}/. /root/.npm/ && ")
        } else {
            String::new()
        }
    }

    /// `s` with every `{{key}}` of [`TEMPLATE_KEYS`] filled in. Any other
    /// `{{…}}` is left as it is — a Dockerfile may legitimately contain Go
    /// template syntax.
    pub fn render(&self, s: &str) -> String {
        s.replace("{{ccache_id}}", &self.ccache_id)
            .replace("{{npm_cache_id}}", &self.npm_cache_id)
            .replace("{{ccache_max_size}}", &self.ccache_max_size)
            .replace("{{ccache_shared_mount}}", &self.ccache_shared_mount())
            .replace("{{ccache_seed}}", &self.ccache_seed())
            .replace("{{npm_shared_mount}}", &self.npm_shared_mount())
            .replace("{{npm_seed}}", &self.npm_seed())
    }
}

/// How one edit went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EditOutcome {
    /// Position in the list, from 0.
    pub index: usize,
    pub name: String,
    pub role: EditRole,
    pub required: bool,
    /// Occurrences replaced; 0 = not matched.
    pub matches: usize,
}

impl EditOutcome {
    pub fn applied(&self) -> bool {
        self.matches > 0
    }

    fn label(&self) -> String {
        if self.name.is_empty() {
            format!("edits[{}]", self.index)
        } else {
            format!("edits[{}] {}", self.index, self.name)
        }
    }

    /// The run-log line: `edit edits[1] ccache-pkg (ccache): applied (1 match)`.
    pub fn log_line(&self) -> String {
        let state = match self.matches {
            0 if self.required => "NOT MATCHED (required)".to_string(),
            0 => "not matched".to_string(),
            1 => "applied (1 match)".to_string(),
            n => format!("applied ({n} matches)"),
        };
        format!("edit {} ({}): {state}", self.label(), self.role.as_str())
    }
}

/// The edited text and every edit's outcome, before judging them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditReport {
    pub text: String,
    pub outcomes: Vec<EditOutcome>,
}

impl EditReport {
    pub fn log_lines(&self) -> Vec<String> {
        self.outcomes.iter().map(EditOutcome::log_line).collect()
    }

    /// `Err` naming every required edit that did not match.
    pub fn check(&self) -> Result<(), String> {
        let missed: Vec<String> = self
            .outcomes
            .iter()
            .filter(|o| o.required && !o.applied())
            .map(EditOutcome::label)
            .collect();
        if missed.is_empty() {
            return Ok(());
        }
        Err(format!(
            "required Dockerfile edit{} {} did not match — the Dockerfile has changed (upstream, \
             or in an extra). Customize edits (Advanced → edits): fix the find text to match the \
             new Dockerfile, or clear 'required' to build without {}",
            if missed.len() == 1 { "" } else { "s" },
            missed.join(", "),
            if missed.len() == 1 { "it" } else { "them" },
        ))
    }
}

/// Apply `edits` in order to a copy of `text`, each replacing **every**
/// occurrence of its (rendered) `find` with its (rendered) `replace`, and
/// report each. Never fails: judging the outcomes is [`EditReport::check`].
pub fn apply_edits_report(text: &str, edits: &[BuildEdit], vars: &TemplateVars) -> EditReport {
    let mut text = text.to_string();
    let mut outcomes = Vec::with_capacity(edits.len());
    for (index, e) in edits.iter().enumerate() {
        let find = vars.render(&e.find);
        let matches = if find.is_empty() {
            0
        } else {
            text.matches(find.as_str()).count()
        };
        if matches > 0 {
            text = text.replace(find.as_str(), &vars.render(&e.replace));
        }
        outcomes.push(EditOutcome {
            index,
            name: e.name.clone(),
            role: e.role,
            required: e.required,
            matches,
        });
    }
    EditReport { text, outcomes }
}

/// [`apply_edits_report`], failing when a required edit did not match.
pub fn apply_edits(
    text: &str,
    edits: &[BuildEdit],
    vars: &TemplateVars,
) -> Result<(String, Vec<EditOutcome>), String> {
    let report = apply_edits_report(text, edits, vars);
    report.check()?;
    Ok((report.text, report.outcomes))
}

// ---------------------------------------------------------------------------
// Reading a Dockerfile
// ---------------------------------------------------------------------------

/// One `FROM` of a Dockerfile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stage {
    /// The `AS` name, if any.
    pub name: Option<String>,
    /// The image or stage it starts from, as written.
    pub from: String,
}

/// What a run needs to know about a Dockerfile's text: the `ARG`s it
/// declares (global and per stage) and its stages.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DockerfileInfo {
    pub args: Vec<String>,
    pub stages: Vec<Stage>,
}

impl DockerfileInfo {
    /// Parse `text`: comment lines skipped (also inside a continuation, as
    /// the builder does), `\`-continued lines joined, instructions matched
    /// case-insensitively. Heredoc bodies are not understood — none of the
    /// preset Dockerfiles use one.
    pub fn parse(text: &str) -> Self {
        let mut info = Self::default();
        let mut logical = String::new();
        for raw in text.lines() {
            let trimmed = raw.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            let continued = trimmed.ends_with('\\');
            let piece = if continued {
                &trimmed[..trimmed.len() - 1]
            } else {
                trimmed
            };
            if !logical.is_empty() {
                logical.push(' ');
            }
            logical.push_str(piece);
            if !continued {
                info.take(&logical);
                logical.clear();
            }
        }
        if !logical.is_empty() {
            info.take(&logical);
        }
        info
    }

    fn take(&mut self, line: &str) {
        let words = split_words(line);
        let Some((instruction, rest)) = words.split_first() else {
            return;
        };
        if instruction.eq_ignore_ascii_case("ARG") {
            for w in rest {
                let name = w.split('=').next().unwrap_or(w);
                if !name.is_empty() && !self.args.iter().any(|a| a == name) {
                    self.args.push(name.to_string());
                }
            }
        } else if instruction.eq_ignore_ascii_case("FROM") {
            let operands: Vec<&String> = rest.iter().filter(|w| !w.starts_with("--")).collect();
            if let Some(from) = operands.first() {
                let name = match operands.get(1..3) {
                    Some([as_kw, name]) if as_kw.eq_ignore_ascii_case("AS") => {
                        Some(name.to_string())
                    }
                    _ => None,
                };
                self.stages.push(Stage {
                    name,
                    from: from.to_string(),
                });
            }
        }
    }

    pub fn declares_arg(&self, name: &str) -> bool {
        self.args.iter().any(|a| a == name)
    }

    /// Stage names are case-insensitive to the builder.
    pub fn has_stage(&self, name: &str) -> bool {
        self.stages.iter().any(|s| {
            s.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
    }

    /// The first of `preferred` that is a stage here.
    pub fn pick_target(&self, preferred: &[&str]) -> Option<String> {
        preferred
            .iter()
            .find(|t| self.has_stage(t))
            .map(|t| t.to_string())
    }

    fn stage_list(&self) -> String {
        let names: Vec<&str> = self
            .stages
            .iter()
            .filter_map(|s| s.name.as_deref())
            .collect();
        if names.is_empty() {
            "none named".into()
        } else {
            names.join(", ")
        }
    }
}

/// Whitespace-split, keeping quoted runs together (`ARG X="a b" Y`).
fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match quote {
            Some(q) if c == q => {
                quote = None;
                cur.push(c);
            }
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

// ---------------------------------------------------------------------------
// Build args
// ---------------------------------------------------------------------------

/// Facts about the base commit that the version build args are derived from
/// (§2.1, §14.1). The git layer supplies them: `Pool::build_number` and
/// `Pool::commit_date`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFacts {
    pub repo_url: String,
    /// Full SHA of the base commit.
    pub sha: String,
    /// `git rev-list --count <sha>` — for official llama.cpp, the `bNNNN` of
    /// its release tags.
    pub build_number: u64,
    /// The committer date, in the committer's own offset.
    pub commit_date: chrono::DateTime<chrono::FixedOffset>,
}

impl SourceFacts {
    /// 7 characters, as `LLAMA_BUILD_COMMIT` carries it (§14.2).
    pub fn short_sha(&self) -> &str {
        self.sha.get(..7).unwrap_or(&self.sha)
    }

    /// `b<build number>` (§14.1) — llama.cpp's own spelling, used for every
    /// engine: without the tags (the pool fetches `--no-tags`) there is no
    /// `git describe` to imitate.
    pub fn app_version(&self) -> String {
        format!("b{}", self.build_number)
    }

    /// `BUILD_DATE` (§2.1): the commit date in UTC, never the wall clock, so
    /// the runtime layers below it stay cacheable.
    pub fn build_date(&self) -> String {
        self.commit_date
            .with_timezone(&chrono::Utc)
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    /// `YYYY-MM-DD` in the committer's offset — git's `%cs`, which is what
    /// audio.cpp's `AUDIOCPP_GIT_DATE` reads when it has git.
    pub fn commit_day(&self) -> String {
        self.commit_date.format("%Y-%m-%d").to_string()
    }
}

/// The `--build-arg`s of a run, in argv order, with visible notes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BuildArgs {
    pub args: Vec<(String, String)>,
    pub notes: Vec<String>,
}

/// The arch arg a Dockerfile no profile knows is assumed to use, per backend.
fn arch_arg_candidates(backend: GpuBackend) -> &'static [&'static str] {
    match backend {
        GpuBackend::Cuda => &["CUDA_DOCKER_ARCH", "CUDA_ARCHITECTURES"],
        GpuBackend::Rocm => &["ROCM_DOCKER_ARCH"],
        GpuBackend::Vulkan | GpuBackend::Cpu => &[],
    }
}

/// Every build arg lmgw passes itself (§5 step 5), each only when the
/// **edited** Containerfile declares it — the build-info ARGs are declared by
/// the edits, and an undeclared arg would only earn a buildah warning — then
/// the build's own `build_args`. What lmgw wanted to pass and could not (a
/// CUDA version or an arch list with nowhere to go) is a note, not silence.
///
/// - `CUDA_VERSION`, the arch arg (`;`-joined, the way every preset
///   Dockerfile hands it to `CMAKE_CUDA_ARCHITECTURES`/`AMDGPU_TARGETS`)
/// - `APP_VERSION` (`b<N>`), `APP_REVISION` (full SHA), `BUILD_DATE`
///   (commit date), `IMAGE_URL`/`IMAGE_SOURCE` (the repository)
/// - `LLAMA_BUILD_NUMBER`/`LLAMA_BUILD_COMMIT`
/// - `AUDIOCPP_VERSION`/`AUDIOCPP_GIT_SHA`/`AUDIOCPP_GIT_DATE`
/// - `SDCPP_BUILD_VERSION`/`SDCPP_BUILD_COMMIT`
pub fn build_args(
    choice: &DockerfileChoice,
    backend: GpuBackend,
    containerfile: &DockerfileInfo,
    facts: &SourceFacts,
    cuda_version: Option<&str>,
    arch: &[String],
    spec_build_args: &str,
) -> Result<BuildArgs, String> {
    let mut out = BuildArgs::default();
    let pass = |out: &mut BuildArgs, k: &str, v: String| {
        if containerfile.declares_arg(k) {
            out.args.push((k.to_string(), v));
            true
        } else {
            false
        }
    };

    if let Some(v) = cuda_version {
        if !pass(&mut out, "CUDA_VERSION", v.to_string()) {
            out.notes.push(format!(
                "{} declares no CUDA_VERSION, so cuda_version {v} is not applied",
                choice.dockerfile
            ));
        }
    }
    if !arch.is_empty() {
        let joined = arch.join(";");
        let arg = match choice.profile.and_then(|p| p.arch_arg) {
            Some(a) => Some(a),
            None => arch_arg_candidates(backend)
                .iter()
                .copied()
                .find(|a| containerfile.declares_arg(a)),
        };
        let passed = arg.is_some_and(|a| pass(&mut out, a, joined.clone()));
        if !passed {
            out.notes.push(format!(
                "{} has no arch build arg{} for the {} backend, so arch {joined} is not applied",
                choice.dockerfile,
                arg.map(|a| format!(" ({a})")).unwrap_or_default(),
                backend.as_str()
            ));
        }
    }

    let short = facts.short_sha().to_string();
    let version = facts.app_version();
    let url = web_url(&facts.repo_url);
    for (k, v) in [
        ("APP_VERSION", version.clone()),
        ("APP_REVISION", facts.sha.clone()),
        ("BUILD_DATE", facts.build_date()),
        ("IMAGE_URL", url.clone()),
        ("IMAGE_SOURCE", url),
        ("LLAMA_BUILD_NUMBER", facts.build_number.to_string()),
        ("LLAMA_BUILD_COMMIT", short.clone()),
        ("AUDIOCPP_VERSION", version.clone()),
        ("AUDIOCPP_GIT_SHA", short.clone()),
        ("AUDIOCPP_GIT_DATE", facts.commit_day()),
        ("SDCPP_BUILD_VERSION", version),
        ("SDCPP_BUILD_COMMIT", short),
    ] {
        pass(&mut out, k, v);
    }

    out.args
        .extend(validate::parse_build_args(spec_build_args)?);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Verify (§14.3)
// ---------------------------------------------------------------------------

/// How to tell a working image from a broken one (§14.3). Exit codes are
/// useless here — official and audio exit 0 with no GPU, ik's `--help` exits
/// 1 — so both checks read the output, and both must run with the class's
/// GPU run args.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct VerifySpec {
    /// The `--entrypoint` both probes run (`Registry::run_throwaway` always
    /// sets one): the server binary, or audio.cpp's dispatcher — which is how
    /// lmgw's audio class starts the image too (`server --config …`).
    pub entrypoint: &'static str,
    pub help_args: &'static [&'static str],
    /// Found in a real `--help` output (regex).
    pub help_marker: &'static str,
    /// `None`: nothing to probe (the CPU backend has no device to find).
    pub device_args: Option<&'static [&'static str]>,
    pub devices: Option<DevicePattern>,
}

/// What proves a device of the build's backend loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DevicePattern {
    /// Matched against each output line; each match is a device found.
    pub device_line: &'static str,
    /// A line that must also be present when set (ik's `found N CUDA
    /// devices`), which counts as the device when no device line follows.
    pub requires: Option<&'static str>,
    /// Measured on the real card in the spike (CUDA). The Vulkan/ROCm
    /// patterns are read off the printing code.
    pub measured: bool,
}

const HELP: &[&str] = &["--help"];
const LIST_DEVICES: &[&str] = &["--list-devices"];

/// The probes for images of `flavor` built for `backend`.
pub fn verify_spec(flavor: Flavor, backend: GpuBackend) -> VerifySpec {
    use GpuBackend::*;
    let (entrypoint, help_args, help_marker, device_args): (_, _, _, &'static [&'static str]) =
        match flavor {
            Flavor::Official => ("/app/llama-server", HELP, r"--port\b", LIST_DEVICES),
            // No --list-devices: a load of a model that does not exist runs
            // backend init and prints the device list, then fails.
            Flavor::Ik => (
                "/llama-server",
                HELP,
                r"--port\b",
                &["-m", "/nonexistent.gguf"],
            ),
            Flavor::Audio => (
                "/app/entrypoint.sh",
                &["server", "--help"],
                r"--port\b",
                &["server", "--list-devices"],
            ),
            Flavor::Sdcpp => ("/sd-server", HELP, r"--listen-port\b", LIST_DEVICES),
        };
    let pattern = |device_line, requires, measured| DevicePattern {
        device_line,
        requires,
        measured,
    };
    let devices = match (flavor, backend) {
        (_, Cpu) => None,
        (Flavor::Official, Cuda) => Some(pattern(r"^\s+CUDA\d+:", None, true)),
        (Flavor::Official, Vulkan) => Some(pattern(r"^\s+Vulkan\d+:", None, false)),
        (Flavor::Official, Rocm) => Some(pattern(r"^\s+ROCm\d+:", None, false)),
        (Flavor::Ik, Cuda) => Some(pattern(
            r"^\s+Device \d+: ",
            Some(r"ggml_cuda_init: found [1-9]\d* CUDA devices"),
            true,
        )),
        (Flavor::Ik, Rocm) => Some(pattern(
            r"^\s+Device \d+: ",
            Some(r"ggml_cuda_init: found [1-9]\d* ROCm devices"),
            false,
        )),
        (Flavor::Ik, Vulkan) => Some(pattern(r"^ggml_vulkan: \d+ = ", None, false)),
        (Flavor::Audio, Cuda) => Some(pattern(r"^CUDA:\d+ ", None, true)),
        (Flavor::Audio, Vulkan) => Some(pattern(r"^Vulkan:\d+ ", None, false)),
        (Flavor::Audio, Rocm) => Some(pattern(r"^ROCm:\d+ ", None, false)),
        (Flavor::Sdcpp, Cuda) => Some(pattern(r"^CUDA\d+\t", None, true)),
        (Flavor::Sdcpp, Vulkan) => Some(pattern(r"^Vulkan\d+\t", None, false)),
        (Flavor::Sdcpp, Rocm) => Some(pattern(r"^ROCm\d+\t", None, false)),
    };
    VerifySpec {
        entrypoint,
        help_args,
        help_marker,
        device_args: devices.map(|_| device_args),
        devices,
    }
}

fn regex(pattern: &str) -> Regex {
    // The patterns are static and covered by the tests; a bad one is a bug
    // in this file, not an input.
    Regex::new(pattern).unwrap_or_else(|e| panic!("preset regex {pattern:?}: {e}"))
}

/// Whether `output` (stdout and stderr together) of the help probe is a real
/// `--help`. The error says what was missing; the output itself belongs in
/// the run log, whole.
pub fn help_ok(spec: &VerifySpec, output: &str) -> Result<(), String> {
    if regex(spec.help_marker).is_match(output) {
        Ok(())
    } else {
        Err(format!(
            "`{} {}` printed no help (expected a match for `{}`) — the binary did not start; \
             see the probe output in the log",
            spec.entrypoint,
            spec.help_args.join(" "),
            spec.help_marker
        ))
    }
}

/// The devices the device probe's `output` (stdout and stderr together)
/// reports for the build's backend — each as its trimmed output line.
/// `Err` when there are none: the backend did not load, which is exactly the
/// "built, broken" case verify exists for (§5 step 6). `Ok(vec![])` when the
/// spec has no device probe (CPU).
pub fn devices_found(spec: &VerifySpec, output: &str) -> Result<Vec<String>, String> {
    let Some(p) = spec.devices else {
        return Ok(Vec::new());
    };
    let line_re = regex(p.device_line);
    let mut found: Vec<String> = output
        .lines()
        .filter(|l| line_re.is_match(l))
        .map(|l| l.trim().to_string())
        .collect();
    if let Some(req) = p.requires {
        let req_re = regex(req);
        match output.lines().find(|l| req_re.is_match(l)) {
            None => found.clear(),
            Some(line) if found.is_empty() => found.push(line.trim().to_string()),
            Some(_) => {}
        }
    }
    if found.is_empty() {
        let argv = spec.device_args.unwrap_or(&[]).join(" ");
        return Err(format!(
            "`{} {argv}` reported no device of the build's backend (expected a line matching \
             `{}`{}) — the backend did not load: a CUDA version newer than the driver, an arch \
             the card lacks, or a container started without the GPU; see the probe output in \
             the log",
            spec.entrypoint,
            p.device_line,
            p.requires
                .map(|r| format!(" after `{r}`"))
                .unwrap_or_default()
        ));
    }
    Ok(found)
}

// ---------------------------------------------------------------------------
// Host GPU facts
// ---------------------------------------------------------------------------

/// `nvidia-smi --query-gpu=compute_cap --format=csv,noheader` output → the
/// arch list: unique, dot removed, ascending (`8.9` → `89`). Ascending so a
/// multi-GPU box whose cards enumerate in another order after a reboot still
/// hashes to the same image.
pub fn parse_compute_caps(output: &str) -> Result<Vec<String>, String> {
    let mut caps: Vec<(u32, u32)> = Vec::new();
    for line in output.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let parsed = line
            .split_once('.')
            .and_then(|(ma, mi)| Some((ma.parse::<u32>().ok()?, mi.parse::<u32>().ok()?)));
        match parsed {
            Some(cap) => {
                if !caps.contains(&cap) {
                    caps.push(cap);
                }
            }
            None => {
                return Err(format!(
                    "nvidia-smi reported '{line}' as a compute capability"
                ))
            }
        }
    }
    if caps.is_empty() {
        return Err("nvidia-smi lists no GPU".into());
    }
    caps.sort_unstable();
    Ok(caps
        .into_iter()
        .map(|(ma, mi)| format!("{ma}{mi}"))
        .collect())
}

/// The host GPUs' arch list, from `nvidia-smi` (§4 "arch: auto").
pub async fn detect_cuda_arch(runner: &dyn CommandRunner) -> Result<Vec<String>, String> {
    let args = [
        "--query-gpu=compute_cap".to_string(),
        "--format=csv,noheader".to_string(),
    ];
    let out = runner
        .run("nvidia-smi", &args)
        .await
        .map_err(|e| format!("could not detect the GPU architecture: nvidia-smi: {e}"))?;
    if !out.ok() {
        return Err(format!(
            "could not detect the GPU architecture: nvidia-smi exited {}: {}",
            out.status,
            out.stderr.trim()
        ));
    }
    parse_compute_caps(&out.stdout)
        .map_err(|e| format!("could not detect the GPU architecture: {e}"))
}

/// The newest CUDA the driver supports, from plain `nvidia-smi`'s header:
/// `… CUDA Version: 13.1 |` up to the 61x drivers, `… CUDA UMD Version: 13.4
/// |` since 615.71.09.
pub fn parse_driver_cuda_version(output: &str) -> Option<String> {
    ["CUDA Version:", "CUDA UMD Version:"]
        .iter()
        .find_map(|label| {
            let rest = output.split(label).nth(1)?;
            let v = rest.split_whitespace().next()?;
            (v.split('.').count() >= 2 && v.split('.').all(|p| p.parse::<u32>().is_ok()))
                .then(|| v.to_string())
        })
}

pub async fn detect_driver_cuda_version(runner: &dyn CommandRunner) -> Result<String, String> {
    let out = runner
        .run("nvidia-smi", &[])
        .await
        .map_err(|e| format!("nvidia-smi: {e}"))?;
    if !out.ok() {
        return Err(format!(
            "nvidia-smi exited {}: {}",
            out.status,
            out.stderr.trim()
        ));
    }
    parse_driver_cuda_version(&out.stdout)
        .ok_or_else(|| "nvidia-smi printed no 'CUDA Version:'".to_string())
}

fn major_minor(v: &str) -> Option<(u32, u32)> {
    let mut parts = v.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// The build editor's warning (§4) when `requested` is newer than the
/// driver's `driver_max`: such a build compiles, and its CUDA backend then
/// fails to load — verify marks it broken.
pub fn cuda_driver_warning(requested: &str, driver_max: &str) -> Option<String> {
    let (req, max) = (major_minor(requested)?, major_minor(driver_max)?);
    (req > max).then(|| {
        format!(
            "CUDA {requested} is newer than the driver supports (CUDA {driver_max}): the image \
             would build, but its CUDA backend would not load and verify would mark the run \
             broken — pick {driver_max} or older, or update the driver"
        )
    })
}

#[cfg(test)]
mod tests;
