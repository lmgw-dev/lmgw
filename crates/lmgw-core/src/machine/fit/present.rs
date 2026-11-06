//! What §1.4 and the settings switch read beyond the settings
//! (machine-check design §1.4, WP12, D135, D137, D139): whether an AMD card
//! lmgw can give models is on the bus and has its compute node, what an
//! engine image lmgw doesn't know by its name is built for, and which
//! models run now.
//!
//! - **The AMD card** ([`amd_card`]): bound to amdgpu with render nodes in
//!   `/dev/dri` is what a Vulkan engine needs; `/dev/kfd` is what ROCm needs
//!   besides. Read whatever card §1.1 judged, so a row deliberately on the
//!   AMD card of a computer whose NVIDIA card comes first is judged against
//!   it (D137).
//! - **Images** ([`Present::built`]): one of the three engines' published
//!   images is told by its tag ([`super::shipped::image_built`]); any other
//!   (an Engines-page build, a tag of the owner's) by what
//!   [`crate::backends::images::image_gpu`] reads of it: its
//!   `dev.lmgw.backend` label, or the `libggml-*` libraries in it, listed
//!   by one throwaway container with no network and no device. An image not
//!   on this machine, or one whose files do not say, stays unknown.
//! - **Running models** ([`running`]): the registry's entries, with the
//!   container each is ([`Running::generation`]), and those a confirmed
//!   switch could not stop ([`Present::kept`]), which still run with the
//!   settings from before it.

use std::collections::{BTreeMap, BTreeSet};

use lmgw_machine::pci::{PciDevice, Vendor};

use super::shipped::{image_built, Built};
use crate::config::{ImageGpu, Snapshot};
use crate::runtime::Class;
use crate::state::SharedState;

/// An AMD card lmgw can give models: bound to amdgpu, with render nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AmdCard {
    /// Its compute node `/dev/kfd` is there, so ROCm settings fit it too.
    pub kfd: bool,
}

/// A model's running container, as the registry holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Running {
    pub class: Class,
    pub model_id: String,
    /// Which container it is: a model stopped and started again is another.
    pub generation: u64,
}

/// What [`super::judge`] and [`super::switch::plan`] read besides the
/// settings (module doc).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Present {
    pub amd: Option<AmdCard>,
    /// What [`crate::backends::images::image_gpu`] read of each image the
    /// settings name that lmgw doesn't know by its name.
    pub images: BTreeMap<String, ImageGpu>,
    pub running: Vec<Running>,
    /// The running containers a confirmed switch could not stop, still the
    /// same containers.
    pub kept: Vec<Running>,
}

impl Present {
    /// What `reference` is built for, as far as lmgw can tell (module doc).
    pub fn built(&self, reference: &str) -> Option<Built> {
        image_built(reference).or_else(|| self.images.get(reference).and_then(from_gpu))
    }

    /// Whether `reference` is one of the engines' published images, which
    /// lmgw knows by its name and replaces with its own for the other card.
    pub fn shipped(reference: &str) -> bool {
        image_built(reference).is_some()
    }

    /// Whether an image built as `b` runs on card `fit` here: as
    /// [`Built::runs_on`] says, and ROCm only with the AMD card's compute
    /// node.
    pub fn runs_here(&self, b: Built, fit: lmgw_api_types::machine::SettingsFit) -> bool {
        b.runs_on(fit) && !(b == Built::Rocm && self.amd.is_some_and(|a| !a.kfd))
    }

    /// Whether the AMD card is here without its compute node.
    pub fn amd_without_kfd(&self) -> bool {
        self.amd.is_some_and(|a| !a.kfd)
    }
}

/// What `image_gpu`'s reading says the image is built for: one GPU backend
/// lmgw has settings for. Several, another (`sycl`, `metal`), none, or a
/// reading that does not tell: `None`.
fn from_gpu(g: &ImageGpu) -> Option<Built> {
    let ImageGpu::Gpu(names) = g else {
        return None;
    };
    let mut built = None;
    for name in names.split(',').map(str::trim) {
        let b = match name.to_ascii_lowercase().as_str() {
            "cuda" => Built::Cuda,
            "vulkan" => Built::Vulkan,
            "rocm" | "hip" => Built::Rocm,
            _ => return None,
        };
        if built.is_some_and(|was| was != b) {
            return None;
        }
        built = Some(b);
    }
    built
}

/// The AMD card on the bus that lmgw can give models (module doc).
pub(crate) fn amd_card(pci: &[PciDevice], kfd: bool, render_nodes: &[String]) -> Option<AmdCard> {
    let bound = pci
        .iter()
        .any(|d| d.vendor() == Vendor::Amd && d.driver.as_deref() == Some("amdgpu"));
    (bound && !render_nodes.is_empty()).then_some(AmdCard { kfd })
}

/// Every image the class settings and the rows name (a disabled row's
/// too) that lmgw doesn't know by its name.
fn unknown_images(snap: &Snapshot) -> BTreeSet<String> {
    let s = &snap.settings;
    let classes = [
        &s.router.image,
        &s.aux_router.image,
        &s.audio.image,
        &s.image.image,
    ];
    let rows = snap
        .local_models
        .iter()
        .filter_map(|m| m.image.as_ref())
        .chain(snap.aux_models.iter().filter_map(|m| m.image.as_ref()))
        .chain(snap.audio_models.iter().filter_map(|m| m.image.as_ref()))
        .chain(snap.image_models.iter().filter_map(|m| m.image.as_ref()));
    classes
        .into_iter()
        .chain(rows)
        .map(|i| i.trim())
        .filter(|i| !i.is_empty() && image_built(i).is_none())
        .map(str::to_string)
        .collect()
}

/// What each image of `snap` that lmgw doesn't know by its name is built
/// for, read from the image (module doc). Nothing is asked when every image
/// is known by its name.
pub(crate) async fn read_images(
    state: &SharedState,
    snap: &Snapshot,
) -> BTreeMap<String, ImageGpu> {
    let references = unknown_images(snap);
    if references.is_empty() {
        return BTreeMap::new();
    }
    crate::backends::images::image_gpu(state, &references).await
}

/// The models running now, by the registry.
pub(crate) fn running(state: &SharedState) -> Vec<Running> {
    state
        .runtime()
        .list()
        .into_iter()
        .map(|v| Running {
            class: v.class,
            model_id: v.model_id,
            generation: v.generation,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_images_own_reading_names_one_backend_or_says_nothing() {
        let gpu = |s: &str| from_gpu(&ImageGpu::Gpu(s.into()));
        assert_eq!(gpu("cuda"), Some(Built::Cuda));
        assert_eq!(gpu("vulkan"), Some(Built::Vulkan));
        assert_eq!(gpu("hip"), Some(Built::Rocm));
        assert_eq!(gpu("rocm"), Some(Built::Rocm));
        assert_eq!(gpu("hip, rocm"), Some(Built::Rocm));
        assert_eq!(gpu("cuda, vulkan"), None, "both run somewhere");
        assert_eq!(gpu("sycl"), None);
        assert_eq!(from_gpu(&ImageGpu::CpuOnly("label".into())), None);
        assert_eq!(from_gpu(&ImageGpu::Unknown("not here".into())), None);

        let p = Present {
            images: BTreeMap::from([(
                "localhost/sd-custom:cuda".to_string(),
                ImageGpu::Gpu("cuda".into()),
            )]),
            ..Present::default()
        };
        assert_eq!(p.built("localhost/sd-custom:cuda"), Some(Built::Cuda));
        assert!(!Present::shipped("localhost/sd-custom:cuda"));
        assert_eq!(
            p.built("ghcr.io/ggml-org/llama.cpp:server-vulkan"),
            Some(Built::Vulkan)
        );
        assert_eq!(p.built("localhost/other:latest"), None);
    }
}
