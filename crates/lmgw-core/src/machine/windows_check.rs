//! The machine check on Windows (Windows design §4.4, WP6): the same six
//! items the Machine page shows, read from what a Windows machine has. No
//! root helper, no CDI spec, no package database: engines run as programs
//! lmgw starts (§4.1), so what decides whether local models can use the card
//! is the card, its driver and the processor, and whether there is room.
//!
//! - **§1.1 the card**: the display adapters' driver keys in the registry
//!   (`lmgw_machine::display`, vendor-neutral), and for an NVIDIA adapter
//!   NVML in a separate process (`lmgw --machine-probe nvml`, as on Linux):
//!   the driver's version, CUDA's, each card's name and memory.
//! - **§1.2 the card for models**: nothing to set up on Windows; said, for a
//!   card that works.
//! - **§1.3 running models**: the processor (`lmgw_machine::cpu`): its
//!   name, logical processors, and the instruction sets the engines' CPU
//!   builds choose their kernels by.
//! - **§1.4 the settings' fit**: not checked yet on Windows. It reads the
//!   rows' images, which a native start does not run, and the engines'
//!   builds for this machine are WP4's.
//! - **§1.5 room**: free space where the models dirs are, as on Linux, and
//!   where the engines are (`<local dir>/engines`), what Podman's image store
//!   is on Linux ("the software that runs models").
//! - **§1.6 download**: not checked yet; the engines' release builds are
//!   WP4's.
//!
//! [`judge`] is plain data in, the check out, so its tests run on any
//! system; [`gather`] reads this machine and is Windows'.

// On Linux only the tests reach it.
#![cfg_attr(not(windows), allow(dead_code))]

use lmgw_api_types::machine::{ItemState, MachineCause, MachineCheck, MachineItem, MachineItemId};
use lmgw_machine::cpu::Cpu;
use lmgw_machine::display::{Adapter, VENDOR_AMD, VENDOR_NVIDIA};

use super::card::Card;
use super::gather::Room;
use super::probe::NvmlAnswer;
use super::words::{item, not_checked};
use crate::hf::fmt_bytes;

/// What the Windows check read.
pub(super) struct WinFacts {
    /// The display adapters, or why they could not be read.
    pub adapters: Result<Vec<Adapter>, String>,
    /// NVML's answer, asked when an NVIDIA adapter is listed.
    pub nvml: Option<Result<NvmlAnswer, String>>,
    pub cpu: Cpu,
    pub rooms: Vec<Room>,
}

fn adapter_line(a: &Adapter) -> String {
    let mut line = format!("display adapter {}: {}", a.key, a.name);
    if let (Some(v), Some(d)) = (a.vendor, a.device) {
        line.push_str(&format!(" · PCI {v:04x}:{d:04x}"));
    }
    if let Some(m) = a.memory_bytes {
        line.push_str(&format!(" · {}", fmt_bytes(m)));
    }
    line.push_str(&format!(
        " · driver {} {}",
        a.provider.as_deref().unwrap_or("(no provider)"),
        a.driver_version.as_deref().unwrap_or("(no version)")
    ));
    line
}

fn of_vendor(adapters: &[Adapter], vendor: u16) -> Vec<&Adapter> {
    adapters
        .iter()
        .filter(|a| a.vendor == Some(vendor))
        .collect()
}

/// §1.1 on Windows, and what it concluded.
pub(super) fn card(w: &WinFacts) -> (MachineItem, Card) {
    let id = MachineItemId::GraphicsCard;
    let adapters = match &w.adapters {
        Ok(a) => a,
        Err(e) => return (
            item(
                id,
                ItemState::Problem,
                "lmgw couldn't read which graphics cards this computer has. The details say why.",
                vec![format!("reading the display adapters' driver keys: {e}")],
                None,
            ),
            Card::Problem,
        ),
    };
    let mut detail: Vec<String> = adapters.iter().map(adapter_line).collect();
    if adapters.is_empty() {
        detail.push("no display adapter has a driver key".into());
    }
    let nvidia = of_vendor(adapters, VENDOR_NVIDIA);
    if !nvidia.is_empty() {
        return by_nvml(w, detail);
    }
    let amd = of_vendor(adapters, VENDOR_AMD);
    if !amd.is_empty() {
        let names: Vec<String> = amd
            .iter()
            .map(|a| match a.memory_bytes {
                Some(m) => format!("{} · {}", a.name, fmt_bytes(m)),
                None => a.name.clone(),
            })
            .collect();
        detail.push(
            "AMD cards run models through the engines' Vulkan builds on Windows (ROCm builds \
             for Windows are not offered yet)"
                .into(),
        );
        let sentence = if names.len() == 1 {
            format!("Graphics card · {}", names[0])
        } else {
            format!("Graphics cards · {}", names.join(", "))
        };
        return (
            item(id, ItemState::Ok, &sentence, detail, None),
            Card::AmdOk,
        );
    }
    let others: Vec<String> = adapters
        .iter()
        .filter(|a| !a.is_basic())
        .map(|a| a.name.clone())
        .collect();
    (
        item(
            id,
            ItemState::Problem,
            "No graphics card found. Cloud models work; local models need a graphics card.",
            detail,
            Some(MachineCause::NoCard { others }),
        ),
        Card::None,
    )
}

/// An NVIDIA adapter is listed: what NVML in a separate process said.
fn by_nvml(w: &WinFacts, mut detail: Vec<String>) -> (MachineItem, Card) {
    let id = MachineItemId::GraphicsCard;
    let problem = |detail, sentence: &str, cause, card| {
        (
            item(id, ItemState::Problem, sentence, detail, Some(cause)),
            card,
        )
    };
    match &w.nvml {
        Some(Ok(NvmlAnswer::Ok {
            driver_version,
            cuda_version,
            cards,
        })) if !cards.is_empty() => {
            detail.push(format!(
                "NVML in a separate process: driver {}{}, {} card(s)",
                driver_version.as_deref().unwrap_or("unknown"),
                cuda_version
                    .map(|v| format!(", CUDA {}.{}", v / 1000, (v % 1000) / 10))
                    .unwrap_or_default(),
                cards.len()
            ));
            for card in cards {
                detail.push(format!(
                    "card {}: {}{}",
                    card.index,
                    card.name,
                    card.pci_bus_id
                        .as_deref()
                        .map(|b| format!(" · PCI {b}"))
                        .unwrap_or_default()
                ));
            }
            let names: Vec<String> = cards
                .iter()
                .map(|k| match k.memory_total {
                    Some(b) => format!("{} · {}", k.name, fmt_bytes(b)),
                    None => k.name.clone(),
                })
                .collect();
            let sentence = if names.len() == 1 {
                format!("Graphics card · {}", names[0])
            } else {
                format!("Graphics cards · {}", names.join(", "))
            };
            (
                item(id, ItemState::Ok, &sentence, detail, None),
                Card::NvidiaOk,
            )
        }
        Some(Ok(NvmlAnswer::Ok { .. })) => {
            detail.push("NVML in a separate process: no card".into());
            problem(
                detail,
                "Your NVIDIA card needs NVIDIA's driver before it can run models.",
                MachineCause::NoNvidiaDriver { bound: None },
                Card::NeedsDriver,
            )
        }
        Some(Ok(NvmlAnswer::Failed { kind, error })) => {
            detail.push(format!(
                "NVML in a separate process failed ({kind}): {error}"
            ));
            if matches!(kind.as_str(), "library_not_found" | "driver_not_loaded") {
                problem(
                    detail,
                    "Your NVIDIA card needs NVIDIA's driver before it can run models.",
                    MachineCause::NoNvidiaDriver { bound: None },
                    Card::NeedsDriver,
                )
            } else {
                problem(
                    detail,
                    "lmgw couldn't talk to the graphics card through its driver.",
                    MachineCause::NvmlFailed {
                        error: format!("{kind}: {error}"),
                    },
                    Card::Problem,
                )
            }
        }
        Some(Err(e)) => {
            detail.push(format!("the NVML probe could not run: {e}"));
            problem(
                detail,
                "lmgw couldn't talk to the graphics card through its driver.",
                MachineCause::NvmlFailed { error: e.clone() },
                Card::Problem,
            )
        }
        None => {
            detail.push("NVML not asked".into());
            problem(
                detail,
                "lmgw couldn't talk to the graphics card through its driver.",
                MachineCause::NvmlFailed {
                    error: "not asked".into(),
                },
                Card::Problem,
            )
        }
    }
}

/// §1.2 on Windows: nothing to set up beside the driver.
pub(super) fn card_for_models(card: Card) -> MachineItem {
    let id = MachineItemId::CardForModels;
    match card {
        Card::NvidiaOk | Card::AmdOk => item(
            id,
            ItemState::Ok,
            "Models can use the graphics card.",
            vec![
                "on Windows each model runs as a program lmgw starts, which reaches the card \
                 through its driver: nothing else needs setting up"
                    .into(),
            ],
            None,
        ),
        Card::None => not_checked(id, "no graphics card"),
        _ => not_checked(id, "the graphics card comes first"),
    }
}

/// §1.3 on Windows: the processor the models' programs run on, and what
/// they can use of it.
pub(super) fn running(cpu: &Cpu) -> MachineItem {
    let mut detail = vec![format!(
        "processor: {} · {} logical processors",
        cpu.brand.as_deref().unwrap_or("name not read"),
        cpu.logical
    )];
    if cpu.features.is_empty() {
        detail.push("instruction sets: not read on this kind of processor yet".into());
    } else {
        let list: Vec<String> = cpu
            .features
            .iter()
            .map(|(name, on)| {
                format!(
                    "{} {}",
                    name.to_ascii_uppercase(),
                    if *on { "yes" } else { "no" }
                )
            })
            .collect();
        detail.push(format!("instruction sets: {}", list.join(", ")));
    }
    let sentence = if cpu.has("avx2") == Some(false) {
        detail.push(
            "the engines' CPU builds choose their kernels by these: without AVX2 a model on \
             the processor runs on the slower SSE kernels"
                .into(),
        );
        "Models run as programs lmgw starts. This processor has no AVX2, so models on it run slower."
    } else {
        "Models run as programs lmgw starts."
    };
    item(
        MachineItemId::RunningModels,
        ItemState::Ok,
        sentence,
        detail,
        None,
    )
}

/// The six items and the verdict's inputs.
pub(super) fn judge(w: &WinFacts) -> (Vec<MachineItem>, Card) {
    let (card_item, card) = card(w);
    let items = vec![
        card_item,
        card_for_models(card),
        running(&w.cpu),
        not_checked(
            MachineItemId::SettingsFit,
            "on Windows the settings' fit to the card is not judged",
        ),
        super::running::room(&w.rooms, None, None),
        not_checked(
            MachineItemId::Download,
            "on Windows the engines' downloads are not looked at",
        ),
    ];
    (items, card)
}

/// The check from `w`, its verdict judged as on Linux (no toolkit, no
/// Podman, the settings' fit not judged).
pub(super) fn check(w: &WinFacts) -> (MachineCheck, Card) {
    let (items, card) = judge(w);
    let basis = super::Basis {
        card,
        toolkit: None,
        podman_problem: false,
        present: super::fit::present::Present {
            amd: None,
            images: Default::default(),
            running: Vec::new(),
            kept: Vec::new(),
        },
        container: false,
    };
    let (verdict, headline) = super::verdict(&items, &basis, super::fit::Outcome::Fits);
    (
        MachineCheck {
            checked_at: chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            verdict,
            headline,
            items,
            plan: None,
            failed_start: None,
        },
        card,
    )
}

/// Read this machine (module doc): the adapters and the processor on a
/// blocking thread, NVML in its own process within the load timeout when an
/// NVIDIA adapter is there, and the room where each class's models dir and
/// the engines are.
#[cfg(windows)]
pub(super) async fn gather(
    state: &std::sync::Arc<crate::state::AppState>,
    f: std::sync::Arc<dyn lmgw_machine::Facts>,
) -> WinFacts {
    let load_timeout = crate::runtime::cdi::load_timeout(state);
    let snap = state.snapshot();
    let s = &snap.settings;
    let places: Vec<(&'static str, String)> = vec![
        ("chat models", s.router.models_dir.clone()),
        (
            "embedding, rerank and decision models",
            s.aux_router.models_dir.clone(),
        ),
        ("audio models", s.audio.models_dir.clone()),
        ("image models", s.image.models_dir.clone()),
        (
            super::running::SOFTWARE,
            crate::runtime::engine_assets::engines_root(&state.data_dir)
                .display()
                .to_string(),
        ),
    ];
    tokio::task::spawn_blocking(move || {
        let adapters = lmgw_machine::display::adapters().map_err(|e| e.to_string());
        let nvml = adapters
            .as_ref()
            .ok()
            .filter(|a| a.iter().any(|a| a.vendor == Some(VENDOR_NVIDIA)))
            .map(|_| super::probe::ask(&*f, load_timeout));
        let rooms = places
            .into_iter()
            .filter(|(_, p)| std::path::Path::new(p).is_absolute())
            .map(|(what, path)| Room {
                what,
                space: f
                    .space(std::path::Path::new(&path))
                    .map_err(|e| e.to_string()),
                path,
            })
            .collect();
        WinFacts {
            adapters,
            nvml,
            cpu: lmgw_machine::cpu::read(),
            rooms,
        }
    })
    .await
    .unwrap_or_else(|e| WinFacts {
        adapters: Err(format!("the reads stopped without an answer: {e}")),
        nvml: None,
        cpu: lmgw_machine::cpu::read(),
        rooms: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::probe::NvmlCard;
    use lmgw_api_types::machine::MachineVerdict;

    fn gtx970() -> Adapter {
        Adapter {
            key: "0001".into(),
            name: "NVIDIA GeForce GTX 970".into(),
            provider: Some("NVIDIA".into()),
            driver_version: Some("32.0.15.6081".into()),
            vendor: Some(VENDOR_NVIDIA),
            device: Some(0x13c2),
            memory_bytes: Some(4 << 30),
        }
    }

    fn basic() -> Adapter {
        Adapter {
            key: "0000".into(),
            name: "Microsoft Basic Display Adapter".into(),
            provider: Some("Microsoft".into()),
            driver_version: Some("10.0.26100.1".into()),
            ..Default::default()
        }
    }

    fn cpu(avx2: bool) -> Cpu {
        Cpu {
            brand: Some("Example CPU".into()),
            logical: 16,
            features: vec![("sse4.2", true), ("avx", true), ("avx2", avx2)],
        }
    }

    fn facts(adapters: Vec<Adapter>, nvml: Option<Result<NvmlAnswer, String>>) -> WinFacts {
        WinFacts {
            adapters: Ok(adapters),
            nvml,
            cpu: cpu(true),
            rooms: Vec::new(),
        }
    }

    fn nvml_ok() -> NvmlAnswer {
        NvmlAnswer::Ok {
            driver_version: Some("560.81".into()),
            cuda_version: Some(12060),
            cards: vec![NvmlCard {
                index: 0,
                name: "NVIDIA GeForce GTX 970".into(),
                uuid: None,
                memory_total: Some(4 << 30),
                pci_bus_id: Some("00000000:01:00.0".into()),
            }],
        }
    }

    #[test]
    fn an_nvidia_card_whose_driver_answers_is_ready() {
        let (c, card) = check(&facts(vec![basic(), gtx970()], Some(Ok(nvml_ok()))));
        assert_eq!(card, Card::NvidiaOk);
        assert_eq!(c.verdict, MachineVerdict::Ready, "{}", c.headline);
        assert_eq!(
            c.items[0].sentence,
            "Graphics card · NVIDIA GeForce GTX 970 · 4.0 GiB"
        );
        assert!(
            c.items[0]
                .detail
                .iter()
                .any(|d| d.contains("driver 560.81, CUDA 12.6")),
            "{:?}",
            c.items[0].detail
        );
        assert_eq!(c.items[1].state, ItemState::Ok);
        assert_eq!(c.items[3].state, ItemState::NotChecked);
    }

    #[test]
    fn an_nvidia_card_without_nvml_needs_the_driver() {
        let failed = NvmlAnswer::Failed {
            kind: "library_not_found".into(),
            error: "nvml.dll".into(),
        };
        let (c, card) = check(&facts(vec![gtx970()], Some(Ok(failed))));
        assert_eq!(card, Card::NeedsDriver);
        assert_eq!(c.verdict, MachineVerdict::NeedsDriver);
        let (c, card) = check(&facts(vec![gtx970()], Some(Err("no probe".into()))));
        assert_eq!(card, Card::Problem);
        assert_eq!(c.verdict, MachineVerdict::NeedsSteps);
    }

    #[test]
    fn only_a_basic_adapter_is_no_card_and_names_no_other() {
        let (c, card) = check(&facts(vec![basic()], None));
        assert_eq!(card, Card::None);
        assert_eq!(c.verdict, MachineVerdict::CloudOnly);
        assert_eq!(
            c.items[0].cause,
            Some(MachineCause::NoCard { others: Vec::new() })
        );
    }

    #[test]
    fn an_amd_card_is_used_through_vulkan() {
        let amd = Adapter {
            key: "0000".into(),
            name: "AMD Radeon RX 7900 XTX".into(),
            vendor: Some(VENDOR_AMD),
            memory_bytes: Some(24 << 30),
            ..Default::default()
        };
        let (c, card) = check(&facts(vec![amd], None));
        assert_eq!(card, Card::AmdOk);
        assert!(c.items[0].detail.iter().any(|d| d.contains("Vulkan")));
    }

    #[test]
    fn the_processor_is_named_and_a_missing_avx2_said() {
        let i = running(&cpu(true));
        assert_eq!(i.sentence, "Models run as programs lmgw starts.");
        assert!(i.detail[0].contains("Example CPU · 16 logical"));
        assert!(i.detail[1].contains("AVX2 yes"), "{:?}", i.detail);
        let i = running(&cpu(false));
        assert!(i.sentence.contains("no AVX2"), "{}", i.sentence);
    }

    /// The Machine page's rule: no sentence names the plumbing.
    #[test]
    fn no_sentence_names_the_plumbing() {
        for w in [
            facts(vec![gtx970()], Some(Ok(nvml_ok()))),
            facts(vec![basic()], None),
        ] {
            let (c, _) = check(&w);
            for i in &c.items {
                for word in ["container", "CDI", "Podman", "podman"] {
                    assert!(!i.sentence.contains(word), "{}", i.sentence);
                }
            }
        }
    }
}
