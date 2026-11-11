//! The Engines page's Windows column (`engine_releases`): the pins, the
//! choice on this machine, what is installed, and the release check's
//! stored observations. Reads files and NVML only; no download, no network.

use std::collections::BTreeMap;

use lmgw_api_types::engine_releases::{
    EngineBuild, EngineRelease, EngineReleasesResponse, MachineGpu, NoKernelImage, ReleaseCheck,
    SupportRelease,
};

use super::choice::GpuFacts;
use super::install::{Installer, Record};
use super::releases::ReleaseObservation;
use super::{engine_label, fallback, Engines, ESPEAK_DIR};

/// The response for `engines`, with `native` saying whether this lmgw starts
/// its engines from them and `observed` the release check's last answers.
pub fn response(
    engines: &Engines,
    facts: &GpuFacts,
    native: bool,
    observed: &BTreeMap<String, ReleaseObservation>,
    checked_at: Option<String>,
) -> EngineReleasesResponse {
    let mut out = EngineReleasesResponse {
        platform: super::pins::platform(),
        native,
        engines_dir: engines.root().display().to_string(),
        machine: MachineGpu {
            cards: facts
                .cards
                .iter()
                .map(|c| match c.cc {
                    Some(cc) => format!("{} ({cc})", c.name),
                    None => format!("{} (compute capability unknown)", c.name),
                })
                .collect(),
            driver_cuda: facts.driver_cuda.map(|v| v.to_string()),
            nvml_error: facts.nvml_error.clone(),
            vulkan_loader: facts.vulkan_loader,
        },
        checked_at,
        ..EngineReleasesResponse::default()
    };
    let Some(pins) = engines.pins() else {
        return out;
    };
    for (engine, p) in pins.engines() {
        let choice = engines.choose(engine, p, facts);
        let builds = p
            .builds
            .iter()
            .map(|b| {
                let dir = engines.build_dir(engine, p, b.variant);
                EngineBuild {
                    variant: b.variant.to_string(),
                    assets: b.assets().map(|a| a.name.clone()).collect(),
                    download_bytes: b.download_size(),
                    cuda_toolkit: b.cuda.as_ref().map(|c| c.toolkit.clone()),
                    cuda_coverage: b
                        .cuda
                        .iter()
                        .flat_map(|c| &c.files)
                        .map(|f| {
                            format!(
                                "{}: SASS {}; PTX {}",
                                f.file,
                                f.sass.join(", "),
                                if f.ptx.is_empty() {
                                    "none".to_string()
                                } else {
                                    f.ptx.join(", ")
                                }
                            )
                        })
                        .collect(),
                    installed: engines.is_installed(engine, p, b.variant),
                    no_kernel_image: fallback::marked(&dir).map(|m| NoKernelImage {
                        line: m.line,
                        exit: m.exit,
                        marked_at: m.marked_at,
                    }),
                    dir: dir.display().to_string(),
                }
            })
            .collect();
        out.engines.push(EngineRelease {
            engine: engine.to_string(),
            label: engine_label(engine).to_string(),
            repo: p.repo.clone(),
            tag: p.tag.clone(),
            published: p.published.clone(),
            chosen: choice.variant.to_string(),
            chosen_why: choice.why,
            chosen_jit: choice.jit,
            builds,
            release: observed.get(engine).map(check),
        });
    }
    let e = &pins.espeak;
    let dir = engines.root().join(ESPEAK_DIR).join(&e.tag);
    let record = Record {
        repo: e.repo.clone(),
        tag: e.tag.clone(),
        variant: None,
        assets: vec![e.asset.clone()],
        installed_at: String::new(),
    };
    out.espeak = Some(SupportRelease {
        label: "eSpeak NG".into(),
        repo: e.repo.clone(),
        tag: e.tag.clone(),
        published: e.published.clone(),
        asset: e.asset.name.clone(),
        download_bytes: e.asset.size,
        installed: Installer::is_installed(&dir, &record),
        dir: dir.display().to_string(),
        release: observed.get("espeak").map(check),
    });
    out
}

fn check(o: &ReleaseObservation) -> ReleaseCheck {
    ReleaseCheck {
        checked_at: o.checked_at.clone(),
        latest_tag: o.latest_tag.clone(),
        latest_published: o.latest_published.clone(),
        update_available: o.update_available,
        error: o.error.clone(),
    }
}
