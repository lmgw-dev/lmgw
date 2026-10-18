//! Whether an image has a GPU backend (decision-models design §3.3, WP2b):
//! what the aux hoist asks before it turns `--n-gpu-layers 0` into
//! `backend: cpu` ([`crate::config::AuxModel::hoist_args`]).
//!
//! "No GPU backend" is the owner's definition (2026-10-12): no
//! `libggml-cuda`, `libggml-vulkan`, hip, sycl or metal library in the image.
//! Two sources, in order:
//! - **lmgw's own builds** carry `dev.lmgw.backend` (`cuda`, `vulkan`,
//!   `rocm`, `cpu`), the label the Images tab shows as the image's backend
//!   (`container_images`). It decides, and nothing runs.
//! - **Every other image** (`ghcr.io/ggml-org/llama.cpp:server`, a tag
//!   `build.sh` made) has no such label, so its files are read: one
//!   throwaway container of it, with no network, no GPU device and
//!   `--pull=never`, lists every `libggml*` library in it
//!   ([`Registry::run_throwaway`](crate::runtime::registry::Registry::run_throwaway),
//!   the read the `--help` vocabulary uses). [`from_files`] judges the list.
//!
//! GPU runtime libraries are no evidence either way: the CPU-only
//! `llama.cpp:server` image ships Mesa's `libvulkan` and an `libOpenCL`
//! (checked 2026-10-12, b11561), and a CUDA base image ships `libcudart`
//! whatever was built on it.
//!
//! An image that is not on this machine, a podman that cannot list it, and
//! one whose files do not tell are [`ImageGpu::Unknown`], with the reason,
//! and the hoist leaves such a row's flag alone.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use lmgw_api_types::builds::GpuBackend;

use super::{local_images, resolve};
use crate::backends::run::Podman;
use crate::config::ImageGpu;
use crate::state::SharedState;

/// The label lmgw's builds name their GPU backend in
/// (`backends::run`'s build labels).
const BACKEND_LABEL: &str = "dev.lmgw.backend";

/// How long listing one image's libraries may take before it counts as
/// unreadable. A listing takes well under a second (0.3–0.4 s on the CUDA
/// image); this only keeps a podman that hangs from holding up boot, and a
/// run cut short says so in the row's notice.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// `find`'s arguments: every `libggml*` file on the image's own filesystem.
const FIND_ARGS: [&str; 4] = ["/", "-xdev", "-name", "libggml*"];

/// The ggml libraries a build carries whatever its backend: the core split
/// (`libggml-base`), the CPU backend in its variants (`libggml-cpu-zen4`),
/// and the two that drive no device of their own (BLAS on the CPU, RPC to
/// another host). Every other `libggml-<name>` is a device backend.
const NOT_A_DEVICE: [&str; 4] = ["base", "cpu", "blas", "rpc"];

/// What each of `references` (an image as a row or class names it) carries,
/// keyed by the reference. Each image is read once, however many references
/// name it.
pub(crate) async fn image_gpu(
    state: &SharedState,
    references: &BTreeSet<String>,
) -> BTreeMap<String, ImageGpu> {
    let images = match local_images(&Podman::of(state)).await {
        Ok(images) => images,
        Err(e) => {
            let why = format!("podman could not list the local images: {e}");
            return references
                .iter()
                .map(|r| (r.clone(), ImageGpu::Unknown(why.clone())))
                .collect();
        }
    };
    let mut by_id: HashMap<String, ImageGpu> = HashMap::new();
    let mut out = BTreeMap::new();
    for reference in references {
        let Some((image, _)) = resolve(reference, &images) else {
            out.insert(
                reference.clone(),
                ImageGpu::Unknown(
                    "it is not on this machine; a start pulls it, and the next start reads it"
                        .into(),
                ),
            );
            continue;
        };
        let label = image
            .labels
            .get(BACKEND_LABEL)
            .map(|l| l.trim())
            .filter(|l| !l.is_empty());
        let gpu = match label {
            Some(label) => from_label(label),
            None => match by_id.get(&image.id) {
                Some(known) => known.clone(),
                None => {
                    let read = read_files(state, &image.id).await;
                    by_id.insert(image.id.clone(), read.clone());
                    read
                }
            },
        };
        out.insert(reference.clone(), gpu);
    }
    out
}

/// An lmgw build's `dev.lmgw.backend` label, read as the build spec's
/// [`GpuBackend`] it was written from. A value lmgw does not know says
/// nothing.
fn from_label(label: &str) -> ImageGpu {
    match GpuBackend::parse(label) {
        Some(GpuBackend::Cpu) => ImageGpu::CpuOnly(format!("its {BACKEND_LABEL} label says cpu")),
        Some(gpu) => ImageGpu::Gpu(gpu.as_str().to_string()),
        None => ImageGpu::Unknown(format!(
            "its {BACKEND_LABEL} label says '{label}', a backend this lmgw does not know"
        )),
    }
}

/// List the `libggml*` libraries in image `id` from a throwaway container
/// and judge them ([`from_files`]).
async fn read_files(state: &SharedState, id: &str) -> ImageGpu {
    let prefix = state.snapshot().settings.container_prefix.clone();
    let name = format!("{prefix}-libs-{}", crate::runtime::hash6(id));
    // A listing needs neither: no network, and no GPU even where the image
    // asks the NVIDIA hook for one (`NVIDIA_VISIBLE_DEVICES=all`).
    let mut run_args = vec!["--network=none".to_string()];
    run_args.extend(crate::runtime::aux::cpu_run_args(&[]));
    let args: Vec<String> = FIND_ARGS.iter().map(|a| a.to_string()).collect();
    let registry = state.runtime();
    let run = registry.run_throwaway(&name, id, "find", &run_args, &args);
    match tokio::time::timeout(LIST_TIMEOUT, run).await {
        Err(_) => {
            // `--rm` covers every run that exits; this one did not.
            let _ = registry.rm_force(&name).await;
            ImageGpu::Unknown(format!(
                "listing its files did not finish within {} s",
                LIST_TIMEOUT.as_secs()
            ))
        }
        Ok(Err(e)) => ImageGpu::Unknown(e),
        Ok(Ok(out)) => from_exit(out.status, &out.stdout, &out.stderr),
    }
}

/// `find`'s answer, by its exit status. 0 is the whole listing, judged by
/// [`from_files`]. 1 is a listing with some directory unreadable: a GPU
/// library in it still says GPU, but no part of a listing proves there is
/// none. Anything else — podman's 125–127, a killed run's -1 — says nothing.
pub(crate) fn from_exit(status: i32, stdout: &str, stderr: &str) -> ImageGpu {
    let said = stderr.lines().next().unwrap_or("").trim();
    match (status, from_files(stdout)) {
        (0, verdict) => verdict,
        (1, gpu @ ImageGpu::Gpu(_)) => gpu,
        (1, _) => ImageGpu::Unknown(format!(
            "listing its files left some of it unread (exit 1: {said}), and the part read shows \
             no GPU library"
        )),
        (status, _) => {
            ImageGpu::Unknown(format!("listing its files failed (exit {status}): {said}"))
        }
    }
}

/// Judge `find`'s list of the `libggml*` files in an image.
///
/// - A `libggml-<name>` whose name is not one of [`NOT_A_DEVICE`] is a
///   device backend, and the image has one (all such names, joined).
/// - Otherwise a `libggml-base` says the build splits its backends into
///   libraries of their own, as llama.cpp has since its backend registry, so
///   none of them is a GPU's.
/// - A lone `libggml.so` is a build that compiles its backends into that one
///   library (ik_llama.cpp's layout), and no library at all a static build:
///   either way the files do not say.
pub(crate) fn from_files(listing: &str) -> ImageGpu {
    let mut devices: BTreeSet<String> = BTreeSet::new();
    let (mut split, mut core) = (false, false);
    for line in listing.lines() {
        let file = line.trim().rsplit('/').next().unwrap_or("");
        let Some(rest) = file.strip_prefix("libggml") else {
            continue;
        };
        if rest.starts_with('.') {
            core = true;
            continue;
        }
        let Some(rest) = rest.strip_prefix('-') else {
            continue;
        };
        let name = rest.split(['.', '-']).next().unwrap_or("");
        if name == "base" {
            split = true;
        } else if !name.is_empty() && !NOT_A_DEVICE.contains(&name) {
            devices.insert(name.to_string());
        }
    }
    if !devices.is_empty() {
        let names: Vec<String> = devices.into_iter().collect();
        return ImageGpu::Gpu(names.join(", "));
    }
    if split {
        return ImageGpu::CpuOnly(
            "the ggml libraries in it are the CPU's alone, no libggml-cuda, -vulkan, -hip, \
             -sycl or -metal"
                .into(),
        );
    }
    ImageGpu::Unknown(if core {
        "its one libggml.so compiles its backends in, so its files do not say whether a GPU \
         backend is among them"
            .into()
    } else {
        "no ggml library was found in it, so its files do not say which backends it has".into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CPU-only `ghcr.io/ggml-org/llama.cpp:server` at b11561, listed
    /// 2026-10-12 (trimmed to one CPU variant).
    const CPU_IMAGE: &str = "/app/libggml-base.so\n/app/libggml-base.so.0\n\
        /app/libggml-base.so.0.26.0\n/app/libggml-cpu-alderlake.so\n/app/libggml-cpu-zen4.so\n\
        /app/libggml.so\n/app/libggml.so.0\n/app/libggml.so.0.26.0\n";

    #[test]
    fn the_cpu_image_has_no_gpu_backend() {
        assert!(matches!(from_files(CPU_IMAGE), ImageGpu::CpuOnly(_)));
    }

    #[test]
    fn a_device_library_is_a_gpu_backend_by_its_name() {
        let cuda = format!("{CPU_IMAGE}/app/libggml-cuda.so\n");
        assert_eq!(from_files(&cuda), ImageGpu::Gpu("cuda".into()));
        let both = format!("{CPU_IMAGE}/app/libggml-vulkan.so\n/app/libggml-hip.so.0\n");
        assert_eq!(from_files(&both), ImageGpu::Gpu("hip, vulkan".into()));
        // A backend this list has never heard of keeps the row where it is.
        let new = format!("{CPU_IMAGE}/usr/lib/libggml-newgpu.so\n");
        assert_eq!(from_files(&new), ImageGpu::Gpu("newgpu".into()));
        let rpc = format!("{CPU_IMAGE}/app/libggml-rpc.so\n/app/libggml-blas.so\n");
        assert!(matches!(from_files(&rpc), ImageGpu::CpuOnly(_)));
    }

    /// ik_llama.cpp's layout (`localhost/lmgw-llama-server:ik-main`, a CUDA
    /// build): one library, CUDA compiled in.
    #[test]
    fn a_monolithic_or_static_build_is_unknown() {
        assert!(matches!(
            from_files("/usr/local/lib/libggml.so\n"),
            ImageGpu::Unknown(_)
        ));
        assert!(matches!(from_files(""), ImageGpu::Unknown(_)));
    }

    #[test]
    fn an_lmgw_build_is_judged_by_its_label() {
        assert_eq!(from_label("cuda"), ImageGpu::Gpu("cuda".into()));
        assert_eq!(from_label("rocm"), ImageGpu::Gpu("rocm".into()));
        assert!(matches!(from_label("cpu"), ImageGpu::CpuOnly(_)));
        assert!(matches!(from_label("sycl"), ImageGpu::Unknown(_)));
    }

    /// Only a whole listing proves there is no GPU library; a partial one
    /// can still show one; any other exit says nothing.
    #[test]
    fn only_a_clean_exit_proves_no_gpu_backend() {
        let cuda = format!("{CPU_IMAGE}/app/libggml-cuda.so\n");
        assert!(matches!(from_exit(0, CPU_IMAGE, ""), ImageGpu::CpuOnly(_)));
        assert_eq!(from_exit(1, &cuda, "denied"), ImageGpu::Gpu("cuda".into()));
        assert!(matches!(
            from_exit(1, CPU_IMAGE, "denied"),
            ImageGpu::Unknown(_)
        ));
        for status in [-1, 125, 126, 127, 2] {
            assert!(
                matches!(from_exit(status, &cuda, "x"), ImageGpu::Unknown(_)),
                "{status}"
            );
        }
    }
}
