//! The model settings lmgw ships for each card (machine-check design §1.4,
//! D18, WP12): for NVIDIA cards, the class defaults
//! ([`crate::config::Settings::default`]); for AMD cards, the Vulkan images
//! and arguments docs/amd-arch.md §4 documents, and the image class's
//! (image-generation design §2.5). This is the one place lmgw's own switch
//! takes them from, and docs/amd-arch.md §4 lists them as they are here.
//!
//! The AMD arguments keep `--security-opt label=disable`, which
//! docs/amd-arch.md's Arch-only page used to leave out: lmgw mounts the
//! models dir without a relabel (`runtime::argv`'s `:/models:ro`), so on a
//! system with SELinux enforcing (Fedora) a container without it cannot
//! read its models.

use lmgw_api_types::machine::SettingsFit;

use crate::runtime::Class;

/// llama.cpp's Vulkan server image (chat and aux), as ghcr.io publishes it.
pub(crate) const LLAMA_VULKAN: &str = "ghcr.io/ggml-org/llama.cpp:server-vulkan";
/// llama.cpp's ROCm server image: documented for AMD cards
/// (docs/amd-arch.md §3), never written by the switch.
#[cfg(test)]
const LLAMA_ROCM: &str = "ghcr.io/ggml-org/llama.cpp:server-rocm";
/// audio.cpp's Vulkan image; it publishes no ROCm one.
pub(crate) const AUDIO_VULKAN: &str = "ghcr.io/0xshug0/audio.cpp:full-vulkan";
/// stable-diffusion.cpp's Vulkan image; it publishes no ROCm one.
pub(crate) const SDCPP_VULKAN: &str = "ghcr.io/leejet/stable-diffusion.cpp:master-vulkan";

/// What a Vulkan engine needs on an AMD card: the render nodes, the user's
/// groups (the render node is group `render`, and rootless Podman keeps
/// supplementary groups only when asked), and SELinux's label off (module
/// doc).
pub(crate) const AMD_VULKAN_ARGS: [&str; 6] = [
    "--device",
    "/dev/dri",
    "--group-add",
    "keep-groups",
    "--security-opt",
    "label=disable",
];

/// One class's settings for a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClassValues {
    pub image: String,
    pub args: Vec<String>,
    /// The audio class's backend, which must match its image; `None` for the
    /// other classes.
    pub backend: Option<String>,
}

/// Class `class`'s settings for the card `fit` names.
pub(crate) fn values(fit: SettingsFit, class: Class) -> ClassValues {
    match fit {
        SettingsFit::NvidiaCuda => {
            let s = crate::config::Settings::default();
            match class {
                Class::Chat => ClassValues {
                    image: s.router.image,
                    args: s.router.extra_run_args,
                    backend: None,
                },
                Class::Aux => ClassValues {
                    image: s.aux_router.image.clone(),
                    args: s.aux_router.extra_run_args.clone(),
                    backend: None,
                },
                Class::Audio => ClassValues {
                    image: s.audio.image,
                    args: s.audio.extra_run_args,
                    backend: Some(s.audio.backend),
                },
                Class::Image => ClassValues {
                    image: s.image.image,
                    args: s.image.extra_run_args,
                    backend: None,
                },
            }
        }
        SettingsFit::AmdVulkan => ClassValues {
            image: match class {
                Class::Chat | Class::Aux => LLAMA_VULKAN,
                Class::Audio => AUDIO_VULKAN,
                Class::Image => SDCPP_VULKAN,
            }
            .to_string(),
            args: AMD_VULKAN_ARGS.map(String::from).to_vec(),
            backend: (class == Class::Audio).then(|| "vulkan".to_string()),
        },
    }
}

/// The card whose settings `fit` are not: the one a switch to `fit` moves
/// away from.
pub(crate) fn other(fit: SettingsFit) -> SettingsFit {
    match fit {
        SettingsFit::AmdVulkan => SettingsFit::NvidiaCuda,
        SettingsFit::NvidiaCuda => SettingsFit::AmdVulkan,
    }
}

/// What an engine image is built for: the backend its GPU libraries are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Built {
    /// CUDA: NVIDIA cards only.
    Cuda,
    /// Vulkan: an AMD card with the render nodes, or an NVIDIA card with
    /// NVIDIA's device (whose libraries it then gets).
    Vulkan,
    /// ROCm (HIP): AMD cards with the compute node only.
    Rocm,
}

impl Built {
    /// Whose card lmgw's settings with this image are: CUDA NVIDIA's,
    /// Vulkan and ROCm AMD's (lmgw ships Vulkan for AMD cards).
    pub(crate) fn card(self) -> SettingsFit {
        match self {
            Self::Cuda => SettingsFit::NvidiaCuda,
            Self::Vulkan | Self::Rocm => SettingsFit::AmdVulkan,
        }
    }

    /// Whether an image built so runs on card `fit` with that card's
    /// devices: CUDA only on NVIDIA's, ROCm only on AMD's, Vulkan on both.
    pub(crate) fn runs_on(self, fit: SettingsFit) -> bool {
        match self {
            Self::Vulkan => true,
            _ => self.card() == fit,
        }
    }

    /// The backend as a person reads it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Cuda => "CUDA",
            Self::Vulkan => "Vulkan",
            Self::Rocm => "ROCm",
        }
    }
}

/// The repositories whose tags name their backend: llama.cpp's
/// (`server-cuda`, `full-vulkan`, `light-rocm`, a build's `server-cuda-b7123`),
/// audio.cpp's (`full-cuda12`, `full-vulkan`) and stable-diffusion.cpp's
/// (`master-cuda`, `master-vulkan`), as ghcr.io publishes them.
const ENGINE_REPOSITORIES: [&str; 3] = [
    "ghcr.io/ggml-org/llama.cpp",
    "ghcr.io/0xshug0/audio.cpp",
    "ghcr.io/leejet/stable-diffusion.cpp",
];

/// What `reference` is built for, when it is one of the three engines'
/// published images ([`ENGINE_REPOSITORIES`]): a `-`-separated part of its
/// tag that is `cuda…` (`cuda12`), `vulkan`, `rocm` or `hip` says, in any
/// spelling podman takes (`docker://`, a digest pin after the tag, the
/// short `ggml-org/…` form is Docker Hub's and so not these). `None` for any
/// other image (a `localhost/` build, another registry, the CPU-only
/// `server` tag): lmgw cannot tell it by its name, and
/// [`super::present::Present::built`] asks the image itself.
pub(crate) fn image_built(reference: &str) -> Option<Built> {
    let r = reference.trim();
    let r = r.strip_prefix("docker://").unwrap_or(r);
    let r = r.split('@').next().unwrap_or(r);
    let canonical = crate::backends::pull::canonical_reference(r).ok()?;
    let (repository, tag) = canonical.rsplit_once(':')?;
    if !ENGINE_REPOSITORIES.contains(&repository) {
        return None;
    }
    let mut built = None;
    for part in tag.split('-') {
        let found = if part.starts_with("cuda") {
            Some(Built::Cuda)
        } else if part == "vulkan" {
            Some(Built::Vulkan)
        } else if part == "rocm" || part == "hip" {
            Some(Built::Rocm)
        } else {
            None
        };
        match (built, found) {
            (_, None) => {}
            (None, f) => built = f,
            // Two backends in one tag: it does not say.
            (Some(b), Some(f)) if b != f => return None,
            _ => {}
        }
    }
    built
}

/// The flags lmgw's settings for AMD cards carry that mean nothing on an
/// NVIDIA card, besides the device nodes: the user's groups, ROCm's seccomp
/// profile and its gfx override (docs/amd-arch.md §3, §4). A switch to
/// NVIDIA's settings takes them out with AMD's devices, so a round trip
/// ends where it began.
pub(crate) fn amd_only(flag: &str, value: &str) -> bool {
    (flag == "--group-add" && value == "keep-groups") || rocm_only(flag, value)
}

/// The flags of docs/amd-arch.md's ROCm settings that its Vulkan ones do
/// without: the seccomp profile and the gfx override. A switch to lmgw's
/// Vulkan settings for AMD cards takes them out with ROCm's `/dev/kfd`.
pub(crate) fn rocm_only(flag: &str, value: &str) -> bool {
    match flag {
        "--security-opt" => value == "seccomp=unconfined",
        "-e" | "--env" => value.starts_with("HSA_OVERRIDE_GFX_VERSION="),
        _ => false,
    }
}

/// Whether the audio class's `backend` is one of `fit`'s card's (what an
/// image for that card is built with): `cuda` for NVIDIA, `vulkan` or `hip`
/// for AMD. `cpu` is nobody's: a choice the switch keeps.
pub(crate) fn backend_is(fit: SettingsFit, backend: &str) -> bool {
    let b = backend.trim();
    match fit {
        SettingsFit::NvidiaCuda => b.eq_ignore_ascii_case("cuda"),
        SettingsFit::AmdVulkan => b.eq_ignore_ascii_case("vulkan") || b.eq_ignore_ascii_case("hip"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_engine_image_is_told_by_its_tag_in_any_spelling() {
        let cases = [
            ("ghcr.io/ggml-org/llama.cpp:server-cuda", Some(Built::Cuda)),
            (
                "ghcr.io/ggml-org/llama.cpp:server-cuda-b7123",
                Some(Built::Cuda),
            ),
            ("ghcr.io/ggml-org/llama.cpp:full-cuda", Some(Built::Cuda)),
            ("ghcr.io/ggml-org/llama.cpp:light-cuda", Some(Built::Cuda)),
            (
                "docker://ghcr.io/ggml-org/llama.cpp:server-vulkan",
                Some(Built::Vulkan),
            ),
            (
                "ghcr.io/ggml-org/llama.cpp:server-cuda@sha256:0123",
                Some(Built::Cuda),
            ),
            (
                "  ghcr.io/ggml-org/llama.cpp:server-rocm ",
                Some(Built::Rocm),
            ),
            ("ghcr.io/0xshug0/audio.cpp:full-cuda12", Some(Built::Cuda)),
            ("ghcr.io/0xshug0/audio.cpp:full-cuda13", Some(Built::Cuda)),
            ("ghcr.io/0xshug0/audio.cpp:full-vulkan", Some(Built::Vulkan)),
            (
                "ghcr.io/leejet/stable-diffusion.cpp:master-cuda",
                Some(Built::Cuda),
            ),
            (
                "ghcr.io/leejet/stable-diffusion.cpp:master-vulkan",
                Some(Built::Vulkan),
            ),
            // What its name does not say.
            ("ghcr.io/ggml-org/llama.cpp:server", None),
            ("ghcr.io/ggml-org/llama.cpp:server-intel", None),
            ("ghcr.io/0xshug0/audio.cpp:full-cpu", None),
            ("localhost/lmgw-build/llama:cuda", None),
            ("quay.io/someone/llama.cpp:server-cuda", None),
            ("docker-archive:/tmp/x.tar", None),
            ("", None),
        ];
        for (reference, want) in cases {
            assert_eq!(image_built(reference), want, "{reference}");
        }
    }

    #[test]
    fn each_cards_images_are_built_for_it_and_run_where_they_can() {
        for class in [Class::Chat, Class::Aux, Class::Audio, Class::Image] {
            let n = values(SettingsFit::NvidiaCuda, class);
            let a = values(SettingsFit::AmdVulkan, class);
            assert_eq!(image_built(&n.image), Some(Built::Cuda), "{class:?}");
            assert_eq!(image_built(&a.image), Some(Built::Vulkan), "{class:?}");
        }
        assert_eq!(image_built(LLAMA_ROCM), Some(Built::Rocm));
        assert!(Built::Vulkan.runs_on(SettingsFit::NvidiaCuda));
        assert!(Built::Vulkan.runs_on(SettingsFit::AmdVulkan));
        assert!(!Built::Cuda.runs_on(SettingsFit::AmdVulkan));
        assert!(!Built::Rocm.runs_on(SettingsFit::NvidiaCuda));
        assert!(backend_is(SettingsFit::NvidiaCuda, "cuda"));
        assert!(backend_is(SettingsFit::AmdVulkan, " Vulkan "));
        assert!(!backend_is(SettingsFit::AmdVulkan, "cpu"));
        assert!(!backend_is(SettingsFit::NvidiaCuda, "cpu"));
    }
}
