//! §1.4, model settings fit the card: the device each class's run arguments
//! and each row's own pass, and the image each runs, against the cards on
//! this computer ([`present`]: the card §1.1 found, an AMD card beside it,
//! what an image is built for), and the switch to the settings lmgw ships
//! for §1.1's card (WP12): [`shipped`] holds them, [`switch`] works out what
//! the switch changes, and the item offers it as its action.

use lmgw_api_types::machine::{
    ItemState, MachineAction, MachineCause, MachineItem, MachineItemId, SettingsFit,
};
use lmgw_api_types::settings_labels as labels;

use super::card::Card;
use super::gather::{run_args, RunArgs};
use super::words::{item, not_checked};
use crate::config::Snapshot;
use crate::runtime::Class;

pub(crate) mod present;
pub(crate) mod shipped;
pub(crate) mod switch;

use present::Present;

/// What a set of run arguments gives a container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Device {
    Nvidia,
    /// AMD's compute node `/dev/kfd` (ROCm).
    Amd,
    /// The render nodes `/dev/dri` alone: what a Vulkan engine uses, on any
    /// vendor's card whose driver its image carries (AMD's Vulkan setting in
    /// docs/amd-arch.md, Intel's) — not one vendor's.
    Render,
    /// CDI devices of another kind (only the system's specs define them).
    OtherCdi(Vec<String>),
    /// No device: the processor.
    None,
}

/// The device `args` pass: NVIDIA's CDI device or `--gpus`, AMD's
/// `/dev/kfd`, the render nodes alone, another CDI kind, or none.
pub(super) fn device(args: &[String]) -> Device {
    let req = crate::runtime::cdi::requested(args);
    if req.nvidia {
        return Device::Nvidia;
    }
    let mut render = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let value = match a.split_once('=') {
            Some(("--device", v)) => Some(v.to_string()),
            None if a == "--device" => it.next().cloned(),
            _ => None,
        };
        match value {
            Some(v) if v.starts_with("/dev/kfd") => return Device::Amd,
            Some(v) if v.starts_with("/dev/dri") => render = true,
            _ => {}
        }
    }
    if render {
        return Device::Render;
    }
    if !req.other_kinds.is_empty() {
        return Device::OtherCdi(req.other_kinds);
    }
    Device::None
}

/// The class's models in plain words.
fn plain(class: Class) -> &'static str {
    match class {
        Class::Chat => "Chat models",
        Class::Aux => "Embedding, rerank and decision models",
        Class::Audio => "Speech and audio models",
        Class::Image => "Image models",
    }
}

/// Where `r`'s arguments are set, as the detail names it: the class's row in
/// Settings, or the model's own.
fn place(r: &RunArgs) -> String {
    match &r.model {
        None => labels::run_args_at(r.class.as_str()).to_string(),
        Some(model) => format!(
            "model {model} ({}), its own run arguments",
            r.class.as_str()
        ),
    }
}

/// How §1.4 came out, for the verdict (§1.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Fits,
    /// Settings for the other card that the switch makes fit, every one of
    /// them (D110): a button here, with no password.
    Switch,
    /// Settings that keep models from running which the switch alone does
    /// not make fit, or models still running with the settings from before
    /// a switch (D136): instructions.
    Steps,
}

/// How one class's or row's settings fit the cards here (D137).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Unit {
    Fits,
    /// Set for a card this computer does not have, or one its card cannot
    /// run (a CUDA image on an AMD card, D118; ROCm's compute node on an AMD
    /// card without it).
    Wrong,
    /// The render nodes alone on an NVIDIA card with no AMD card beside it
    /// (D51): a detail line.
    RenderOnNvidia,
    OtherCdi(Vec<String>),
    Processor,
}

/// Judge `r` against the card §1.1 found and the cards `present` (D137):
/// NVIDIA's device fits an NVIDIA card; AMD's compute node an AMD card that
/// has it, the render nodes any AMD card, with an image that is not CUDA's,
/// whichever card §1.1 judged first.
fn unit(card: Card, present: &Present, r: &RunArgs) -> Unit {
    let cuda = present.built(&r.image) == Some(shipped::Built::Cuda);
    match device(&r.args) {
        Device::Nvidia if card == Card::NvidiaOk => Unit::Fits,
        Device::Nvidia => Unit::Wrong,
        Device::Amd if present.amd.is_some_and(|a| a.kfd) && !cuda => Unit::Fits,
        Device::Amd => Unit::Wrong,
        Device::Render if present.amd.is_some() && !cuda => Unit::Fits,
        Device::Render if card == Card::AmdOk => Unit::Wrong,
        Device::Render => Unit::RenderOnNvidia,
        Device::OtherCdi(kinds) => Unit::OtherCdi(kinds),
        Device::None => Unit::Processor,
    }
}

/// The places of `snap` whose settings are [`Unit::Wrong`] here.
fn wrong_in(card: Card, present: &Present, snap: &Snapshot) -> Vec<String> {
    let (classes, rows) = run_args(snap);
    classes
        .iter()
        .chain(rows.iter())
        .filter(|r| unit(card, present, r) == Unit::Wrong)
        .map(place)
        .collect()
}

/// "a, b and c".
fn listed(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [first @ .., last] => format!("{} and {last}", first.join(", ")),
    }
}

/// §1.4's item and how it came out. The classes' settings and each enabled
/// row's own are judged against the cards here ([`unit`]). The switch to
/// this card's settings is worked out from `snap`, every row of it (a
/// disabled one too), and offered as the item's action whenever it changes
/// something: with the problem, or as information when nothing set for the
/// other card keeps models from running (lmgw's own Vulkan values on an
/// NVIDIA card, a disabled row, a row for the AMD card beside an NVIDIA
/// one). The switch is promised (`needs_fix`) only when the settings as it
/// leaves them fit (D110); models a confirmed switch could not stop are
/// steps until they stop (D136).
pub(super) fn judge(card: Card, present: &Present, snap: &Snapshot) -> (MachineItem, Outcome) {
    let id = MachineItemId::SettingsFit;
    let fit = match card {
        Card::NvidiaOk => SettingsFit::NvidiaCuda,
        Card::AmdOk => SettingsFit::AmdVulkan,
        Card::None => return (not_checked(id, "no graphics card"), Outcome::Fits),
        _ => {
            return (
                not_checked(id, "the graphics card comes first"),
                Outcome::Fits,
            )
        }
    };
    let (classes, rows) = run_args(snap);
    let mut detail = Vec::new();
    let mut wrong: Vec<String> = Vec::new();
    let mut render: Vec<String> = Vec::new();
    let mut on_cpu: Vec<&'static str> = Vec::new();
    let mut other: Vec<String> = Vec::new();
    for r in classes.iter().chain(rows.iter()) {
        detail.push(format!(
            "{}: {}{}",
            place(r),
            if r.args.is_empty() {
                "(none)".to_string()
            } else {
                switch::line(&r.args)
            },
            if r.model.is_none() {
                format!(" · {}: {}", labels::image_at(r.class.as_str()), r.image)
            } else {
                format!(" · image {}", r.image)
            }
        ));
        match unit(card, present, r) {
            Unit::Fits => {}
            Unit::Wrong => wrong.push(place(r)),
            Unit::RenderOnNvidia => render.push(place(r)),
            Unit::OtherCdi(kinds) => {
                for k in kinds {
                    if !other.contains(&k) {
                        other.push(k);
                    }
                }
            }
            Unit::Processor if r.model.is_none() => on_cpu.push(plain(r.class)),
            Unit::Processor => {}
        }
    }
    for place in &render {
        detail.push(format!(
            "{place} passes only the render nodes ({DRI}): a Vulkan engine reaches an NVIDIA \
             card only with NVIDIA's libraries, which nvidia.com/gpu=all brings",
            DRI = super::gather::DRI
        ));
    }
    if let Some(a) = present.amd.filter(|_| card == Card::NvidiaOk) {
        detail.push(format!(
            "an AMD card is here too ({}): settings for it fit",
            if a.kfd {
                "render nodes and /dev/kfd"
            } else {
                "render nodes, no /dev/kfd"
            }
        ));
    }
    let sw = switch::plan(snap, fit, present);
    let after = sw.apply(snap);
    let still_wrong = wrong_in(card, present, &after);
    let (to, from) = (
        switch::card_name(fit),
        switch::card_name(shipped::other(fit)),
    );
    let action = (!sw.is_empty()).then(|| {
        detail.push(format!("switching to the settings for {to} cards changes:"));
        detail.extend(sw.changes.iter().map(switch::change_line));
        detail.extend(sw.left.iter().map(switch::left_line));
        let running = switch::affected(snap, &after, &present.running);
        if !running.is_empty() {
            detail.push(format!(
                "running now, stopped after the switch unless answering a request: {}",
                running
                    .iter()
                    .map(|r| format!("{} ({})", r.model_id, r.class.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        MachineAction::SettingsFit {
            fit,
            label: format!("Switch to the settings for {to} cards"),
            changes: sw.changes.clone(),
            left: sw.left.clone(),
            running: switch::offered(&running),
        }
    });
    if action.is_none() {
        detail.extend(sw.left.iter().map(switch::left_line));
    }
    let cause = match fit {
        SettingsFit::AmdVulkan => MachineCause::SettingsForNvidia,
        SettingsFit::NvidiaCuda => MachineCause::SettingsForAmd,
    };
    let kept: Vec<String> = present.kept.iter().map(|k| k.model_id.clone()).collect();
    if !kept.is_empty() {
        detail.push(format!(
            "still running with the settings from before the switch: {}; they answer requests \
             now, and each picks up the new settings when it stops (Overview → stop once it is \
             done, or lmgw__container action=stop)",
            kept.join(", ")
        ));
    }
    if !wrong.is_empty() {
        detail.push(format!(
            "set for the other vendor's card: {}",
            wrong.join(", ")
        ));
        let (sentence, outcome) = if action.is_some() && still_wrong.is_empty() {
            (
                format!(
                    "lmgw is set up for {from} cards. It can switch to the settings for {to} \
                     cards."
                ),
                Outcome::Switch,
            )
        } else {
            detail.push(format!(
                "still set for another card after the switch: {}",
                still_wrong.join(", ")
            ));
            (
                format!(
                    "Some model settings are for {from} cards, and lmgw can't switch all of them \
                     to the settings for {to} cards. The details say which to change by hand."
                ),
                Outcome::Steps,
            )
        };
        let mut it = item(id, ItemState::Problem, &sentence, detail, Some(cause));
        it.action = action;
        return (it, outcome);
    }
    if !kept.is_empty() {
        let sentence = format!(
            "Some models still run with the settings from before the switch ({}). They use the \
             new settings once they stop.",
            listed(&kept)
        );
        let mut it = item(
            id,
            ItemState::Problem,
            &sentence,
            detail,
            Some(MachineCause::RunningOldSettings { models: kept }),
        );
        it.action = action;
        return (it, Outcome::Steps);
    }
    // Information: every sentence that applies, the switch's first.
    let mut sentences: Vec<String> = Vec::new();
    let mut causes: Vec<MachineCause> = Vec::new();
    if action.is_some() {
        sentences.push(format!(
            "Some model settings are for {from} cards. lmgw can switch them to the settings for \
             {to} cards."
        ));
        causes.push(cause);
    }
    if !on_cpu.is_empty() {
        let list = match on_cpu.as_slice() {
            [one] => one.to_string(),
            [first, rest @ .., last] => {
                let mut parts = vec![first.to_string()];
                parts.extend(rest.iter().map(|r| r.to_lowercase()));
                format!("{} and {}", parts.join(", "), last.to_lowercase())
            }
            [] => String::new(),
        };
        sentences.push(format!("{list} are set to run on the processor."));
        causes.push(MachineCause::RunsOnProcessor {
            classes: on_cpu.iter().map(|s| s.to_string()).collect(),
        });
    }
    if !other.is_empty() {
        detail.push(format!(
            "{} only a description in the system's spec dirs defines; those models read the \
             system's dirs before lmgw's own",
            other.join(", ")
        ));
        sentences.push(
            "Some model settings name a device only your system's own settings describe."
                .to_string(),
        );
        causes.push(MachineCause::OtherCdiKind { devices: other });
    }
    if sentences.is_empty() {
        return (
            item(
                id,
                ItemState::Ok,
                "Model settings fit this card.",
                detail,
                None,
            ),
            Outcome::Fits,
        );
    }
    let mut it = item(
        id,
        ItemState::Info,
        &sentences.join(" "),
        detail,
        causes.into_iter().next(),
    );
    it.action = action;
    (it, Outcome::Fits)
}
