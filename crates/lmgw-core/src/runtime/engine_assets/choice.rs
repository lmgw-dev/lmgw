//! Which build of an engine this machine runs (Windows design §4.2), per
//! engine: CUDA 13 on a driver that supports it and cards of compute
//! capability 7.5 or newer; the CUDA 12 build where its recorded coverage
//! reaches the cards; otherwise Vulkan, when a Vulkan loader is installed;
//! the CPU build when nothing else is usable.
//!
//! "The cards" are every NVIDIA card NVML lists: a build is only chosen when
//! it runs on all of them, because a model's card is chosen by its run args
//! later and one engine build serves every row of the engine.
//!
//! A CUDA build that ended a start with "no kernel image is available"
//! ([`super::fallback`]) is not chosen again: the engine falls back to its
//! Vulkan build.

use super::coverage::{reach, Reach, Version};
use super::pins::{BuildPin, EnginePins, Variant};

/// One NVIDIA card as the choice needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub index: u32,
    pub name: String,
    /// `None` when NVML could not say.
    pub cc: Option<Version>,
}

/// What the choice reads of the machine ([`super::facts::read`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpuFacts {
    /// NVML's cards; empty without an NVIDIA driver.
    pub cards: Vec<Card>,
    /// The driver's CUDA version (`nvmlSystemGetCudaDriverVersion`).
    pub driver_cuda: Option<Version>,
    /// Why NVML gave nothing, when it did not (no NVIDIA driver is the
    /// common one).
    pub nvml_error: Option<String>,
    /// A Vulkan loader is installed (Windows: `vulkan-1.dll` in the system
    /// directory, which every GPU driver with Vulkan puts there).
    pub vulkan_loader: bool,
}

/// The choice, with the sentence that says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub variant: Variant,
    /// "llama.cpp b12244 cuda12: GTX 970 (5.2) through compute_50 PTX …"
    pub why: String,
    /// The first start JIT-compiles the kernels (the build reaches a card
    /// only through PTX).
    pub jit: bool,
}

/// The build of `pins` for this machine; `fallen_back(variant)` says a CUDA
/// build of this tag ended a start without a kernel image here.
pub fn choose(
    pins: &EnginePins,
    facts: &GpuFacts,
    fallen_back: &dyn Fn(Variant) -> bool,
) -> Choice {
    let mut passed: Vec<String> = Vec::new();
    let any_fallback = pins
        .builds
        .iter()
        .any(|b| b.variant.is_cuda() && fallen_back(b.variant));
    for variant in Variant::PREFERENCE {
        let Some(build) = pins.build(variant) else {
            if variant.is_cuda() && !facts.cards.is_empty() {
                passed.push(format!("{variant}: not published for this release"));
            }
            continue;
        };
        match usable(build, facts, any_fallback) {
            Ok((why, jit)) => {
                let mut why = format!("{} {variant}: {why}", pins.tag);
                if !passed.is_empty() {
                    why.push_str(&format!(" (passed over: {})", passed.join("; ")));
                }
                return Choice { variant, why, jit };
            }
            Err(e) => passed.push(format!("{variant}: {e}")),
        }
    }
    // Every engine has a CPU build (the table's check), and the CPU is
    // always usable; this is only reached by a table without one.
    Choice {
        variant: Variant::Cpu,
        why: format!("{} cpu: {}", pins.tag, passed.join("; ")),
        jit: false,
    }
}

fn usable(
    build: &BuildPin,
    facts: &GpuFacts,
    cuda_fell_back: bool,
) -> Result<(String, bool), String> {
    match build.variant {
        Variant::Cpu => Ok(("no GPU build is usable".into(), false)),
        Variant::Vulkan => {
            if facts.vulkan_loader {
                Ok(("a Vulkan loader is installed".into(), false))
            } else {
                Err("no Vulkan loader is installed (vulkan-1.dll)".into())
            }
        }
        v => {
            if cuda_fell_back {
                return Err(
                    "a start of this release's CUDA build ended in \"no kernel image is \
                     available\" on this machine"
                        .into(),
                );
            }
            if facts.cards.is_empty() {
                return Err(match &facts.nvml_error {
                    Some(e) => format!("no NVIDIA card ({e})"),
                    None => "no NVIDIA card".into(),
                });
            }
            let major = v.cuda_major().unwrap_or(12);
            let driver = facts
                .driver_cuda
                .ok_or("the driver's CUDA version is unknown")?;
            let cuda = build.cuda.as_ref().ok_or("its coverage is not recorded")?;
            let mut parts = Vec::new();
            let mut jit = false;
            for card in &facts.cards {
                let cc = card
                    .cc
                    .ok_or_else(|| format!("{}: its compute capability is unknown", card.name))?;
                if v == Variant::Cuda13 && cc < Version::new(7, 5) {
                    return Err(format!(
                        "{} has compute capability {cc}, and CUDA 13 needs 7.5 or newer",
                        card.name
                    ));
                }
                match reach(cuda, major, cc, driver) {
                    Reach::Sass(s) => parts.push(format!("{} ({cc}) through {s}", card.name)),
                    Reach::Jit(p) => {
                        jit = true;
                        parts.push(format!(
                            "{} ({cc}) through {p} PTX, compiled on the first start",
                            card.name
                        ))
                    }
                    Reach::No(e) => return Err(format!("{}: {e}", card.name)),
                }
            }
            Ok((format!("{}, driver CUDA {driver}", parts.join(", ")), jit))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::engine_assets::pins;

    fn windows() -> &'static pins::PlatformPins {
        pins::table().get("windows-x86_64").unwrap()
    }

    fn gtx970() -> GpuFacts {
        GpuFacts {
            cards: vec![Card {
                index: 0,
                name: "NVIDIA GeForce GTX 970".into(),
                cc: Some(Version::new(5, 2)),
            }],
            driver_cuda: Some(Version::new(12, 6)),
            nvml_error: None,
            vulkan_loader: true,
        }
    }

    const NONE: &dyn Fn(Variant) -> bool = &|_| false;

    #[test]
    fn the_gtx_970_runs_llama_on_cuda12_and_the_others_on_vulkan() {
        let w = windows();
        let f = gtx970();
        let llama = choose(&w.llama, &f, NONE);
        assert_eq!(llama.variant, Variant::Cuda12, "{}", llama.why);
        assert!(llama.jit);
        assert!(llama.why.contains("compute_50"), "{}", llama.why);
        assert!(llama.why.contains("CUDA 13 needs 7.5"), "{}", llama.why);
        assert_eq!(choose(&w.sdcpp, &f, NONE).variant, Variant::Vulkan);
        let audio = choose(&w.audio, &f, NONE);
        assert_eq!(audio.variant, Variant::Vulkan);
        assert!(
            audio.why.contains("no kernels for compute capability 5.2"),
            "{}",
            audio.why
        );
    }

    #[test]
    fn a_new_card_on_a_new_driver_gets_cuda13_and_without_a_card_vulkan_or_cpu() {
        let w = windows();
        let mut f = gtx970();
        f.cards[0].cc = Some(Version::new(8, 9));
        f.driver_cuda = Some(Version::new(13, 5));
        assert_eq!(choose(&w.llama, &f, NONE).variant, Variant::Cuda13);
        // sd.cpp publishes no CUDA 13 build; its CUDA 12 one runs on 8.9.
        assert_eq!(choose(&w.sdcpp, &f, NONE).variant, Variant::Cuda12);
        f.cards.clear();
        assert_eq!(choose(&w.llama, &f, NONE).variant, Variant::Vulkan);
        f.vulkan_loader = false;
        assert_eq!(choose(&w.llama, &f, NONE).variant, Variant::Cpu);
    }

    #[test]
    fn mixed_cards_get_the_build_that_reaches_both() {
        // WP4.2: one build per engine, chosen for every card NVML lists.
        let w = windows();
        let mut f = gtx970();
        f.cards.push(Card {
            index: 1,
            name: "NVIDIA GeForce RTX 4080".into(),
            cc: Some(Version::new(8, 9)),
        });
        f.driver_cuda = Some(Version::new(13, 5));
        let llama = choose(&w.llama, &f, NONE);
        assert_eq!(llama.variant, Variant::Cuda12, "{}", llama.why);
        assert!(llama.jit, "the 970 through PTX");
        assert!(
            llama.why.contains("GTX 970 (5.2) through compute_50"),
            "{}",
            llama.why
        );
        assert!(
            llama.why.contains("RTX 4080 (8.9) through sm_89"),
            "{}",
            llama.why
        );
        assert!(llama.why.contains("CUDA 13 needs 7.5"), "{}", llama.why);
        // Neither of the others reaches 5.2: Vulkan, for both cards.
        assert_eq!(choose(&w.audio, &f, NONE).variant, Variant::Vulkan);
        assert_eq!(choose(&w.sdcpp, &f, NONE).variant, Variant::Vulkan);
    }

    #[test]
    fn a_cuda_build_without_a_kernel_image_falls_back_to_vulkan() {
        let w = windows();
        let mut f = gtx970();
        f.cards[0].cc = Some(Version::new(8, 6));
        f.driver_cuda = Some(Version::new(13, 5));
        let c = choose(&w.audio, &f, &|v| v == Variant::Cuda13);
        assert_eq!(c.variant, Variant::Vulkan, "{}", c.why);
        assert!(c.why.contains("no kernel image"), "{}", c.why);
    }
}
