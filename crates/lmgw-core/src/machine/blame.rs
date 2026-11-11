//! What a model start's failure is blamed on (machine-check design §2.5,
//! D64, D65, D95, D96): §1.1's and §1.2's problems, judged per row by what
//! its run arguments give its container, and the read of those two items
//! alone that a failed start takes when the machine changed since the last
//! check.
//!
//! - **Per row** ([`Blame::problem`], D64): a row given the NVIDIA card
//!   (`nvidia.com/gpu=…`, `--gpus`) is blamed on §1.1's NVIDIA causes and on
//!   §1.2; a row given AMD's compute node (`/dev/kfd`, ROCm) on §1.1's AMD
//!   cause; a row given the render nodes alone (`/dev/dri`, a Vulkan engine
//!   on any vendor's card) only when there are no render nodes; no card at
//!   all is every such row's. Another CDI kind, or no device, is nobody's
//!   to judge here.
//! - **The words** ([`Blamed::words`], D65): "could not start on the
//!   graphics card of the computer lmgw runs on." then the item's sentence,
//!   and "Open lmgw there to see what to do." only when the item has a plan
//!   or steps to show there; no card at all has words of its own.
//! - **Which causes the resolve-time swap trusts** ([`Blamed::swaps`],
//!   D95): only those a change to lmgw-machine's `Current` (the driver's two
//!   halves, the NVML library, `nvidia-ctk`, the rpm database, the
//!   `/dev/nvidia*` nodes, the boot) would clear.
//! - **The read** ([`Reads`], D96): §1.1 and §1.2 alone, one at a time — a
//!   failed start joins the read under way — with the NVML probe left out
//!   when the start's own write of lmgw's spec ran out of time on the card;
//!   when it ends, the full check runs in the background
//!   ([`super::check_in_background`]).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::{FutureExt, Shared};
use lmgw_api_types::machine::{ItemState, MachineCause, MachineCheck, MachineItem, MachineItemId};
use lmgw_machine::pci::Vendor;

use super::fit::Device;
use crate::runtime::cdi::CdiError;
use crate::state::SharedState;

/// Whose rows §1.1's problem concerns (D64).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Concerns {
    /// An NVIDIA card's driver: rows given the NVIDIA card.
    Nvidia,
    /// An AMD card's compute node: rows given `/dev/kfd`.
    Amd,
    /// No card at all: every row given a card.
    Every,
    /// A problem with no cause (a check whose reads stopped): no row's.
    Nobody,
}

impl Concerns {
    fn of(cause: Option<&MachineCause>) -> Self {
        match cause {
            Some(MachineCause::NoCard { .. }) => Self::Every,
            Some(MachineCause::AmdNoComputeNode) => Self::Amd,
            // The server image's container without the NVIDIA card falls
            // under `_`: rows given the NVIDIA card.
            Some(_) => Self::Nvidia,
            None => Self::Nobody,
        }
    }
}

/// What a check (or a failed start's read) found that a start on the card
/// can be blamed on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Blame {
    /// §1.1's item when it is a problem, and whose rows it concerns.
    card: Option<(MachineItem, Concerns)>,
    /// §1.2's item when it is a problem (an NVIDIA card's), and whether the
    /// check offers a plan with it (the button, or the steps and why).
    toolkit: Option<(MachineItem, bool)>,
    /// Whether `/dev/dri` has render nodes.
    render_nodes: bool,
}

impl Blame {
    /// From a check's items and plan, and whether it found render nodes.
    pub(super) fn of(check: &MachineCheck, render_nodes: bool) -> Self {
        let item = |id: MachineItemId| check.items.iter().find(|i| i.id == id);
        Self::from_parts(
            item(MachineItemId::GraphicsCard),
            item(MachineItemId::CardForModels),
            check.plan.is_some(),
            render_nodes,
        )
    }

    /// From §1.1's item (`None`: not judged), §1.2's, whether a plan comes
    /// with §1.2's, and whether there are render nodes.
    fn from_parts(
        card: Option<&MachineItem>,
        toolkit: Option<&MachineItem>,
        plan: bool,
        render_nodes: bool,
    ) -> Self {
        let problem =
            |i: Option<&MachineItem>| i.filter(|i| i.state == ItemState::Problem).cloned();
        Self {
            card: problem(card).map(|i| {
                let c = Concerns::of(i.cause.as_ref());
                (i, c)
            }),
            toolkit: problem(toolkit).map(|i| (i, plan)),
            render_nodes,
        }
    }

    /// Whether anything at all is a problem: the common case, none, is
    /// decided before a row is looked at.
    pub(super) fn any(&self) -> bool {
        self.card.is_some() || self.toolkit.is_some()
    }

    /// What a start of a row whose container is given `row` is blamed on
    /// (module doc); `None` when nothing that row uses is a problem.
    pub(super) fn problem(&self, row: &Device) -> Option<Blamed> {
        let card = |concerned: &dyn Fn(Concerns) -> bool| {
            self.card
                .as_ref()
                .filter(|(_, c)| concerned(*c))
                .map(|(i, _)| Blamed::of(i, false))
        };
        match row {
            Device::Nvidia => card(&|c| matches!(c, Concerns::Nvidia | Concerns::Every))
                .or_else(|| self.toolkit.as_ref().map(|(i, plan)| Blamed::of(i, *plan))),
            Device::Amd => card(&|c| matches!(c, Concerns::Amd | Concerns::Every)),
            Device::Render if !self.render_nodes => card(&|c| c != Concerns::Nobody),
            Device::Render | Device::OtherCdi(_) | Device::None => None,
        }
    }
}

/// The words a model's own name comes before when there is no card at all.
const NO_CARD: &str = "could not start: no graphics card was found on the computer lmgw runs \
                       on. Cloud models still work.";

/// What one start is blamed on: the item's sentence and cause, and whether
/// it has a plan or steps to show in lmgw's window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blamed {
    sentence: String,
    cause: Option<MachineCause>,
    see_there: bool,
}

impl Blamed {
    fn of(item: &MachineItem, plan: bool) -> Self {
        Self {
            sentence: item.sentence.clone(),
            see_there: see_there(item.cause.as_ref(), plan),
            cause: item.cause.clone(),
        }
    }

    /// What `gpu_setup` says after the model's name (D65).
    pub fn words(&self) -> String {
        if matches!(self.cause, Some(MachineCause::NoCard { .. })) {
            return NO_CARD.to_string();
        }
        let mut w = format!(
            "could not start on the graphics card of the computer lmgw runs on. {}",
            self.sentence
        );
        if self.see_there {
            w.push_str(" Open lmgw there to see what to do.");
        }
        w
    }

    /// Whether the resolve-time swap may take this cause for as long as
    /// `Current` is unchanged (D95): the driver's causes, NVIDIA's toolkit
    /// missing, and a toolkit whose write failed (`nvidia-ctk` is in
    /// `Current`, so a toolkit updated or reinstalled reads as changed).
    /// Not no card at all, an AMD card's compute node, a folder that is not
    /// private, lmgw's own files or a write that ran out of time: nothing
    /// `Current` reads changes when they are fixed, and the start itself is
    /// what writes lmgw's spec again.
    pub(super) fn swaps(&self) -> bool {
        use MachineCause as C;
        matches!(
            self.cause,
            Some(
                C::NoNvidiaDriver { .. }
                    | C::DriverMismatch { .. }
                    | C::DriverNotRunning { .. }
                    | C::DriverLibrariesMissing
                    | C::CardNotPassedIn
                    | C::DriverUnreadable { .. }
                    | C::NvmlFailed { .. }
                    | C::ToolkitMissing
                    | C::ToolkitOtherProvider { .. }
                    | C::SpecNotWritten { .. }
            )
        )
    }
}

/// Whether an item with `cause` has a plan or steps to show in lmgw's
/// window (`plan`: §1.2's plan comes with it): the driver's install and its
/// missing part, the Secure Boot key enrolment's steps, the toolkit's plan,
/// and the folder or the files the details name. Not a restart, a wait, no
/// card, an AMD card's compute node, NVML or a driver lmgw could not read:
/// there is nothing to see there that the sentence does not say.
fn see_there(cause: Option<&MachineCause>, plan: bool) -> bool {
    use MachineCause as C;
    match cause {
        Some(C::NoNvidiaDriver { .. } | C::DriverLibrariesMissing | C::CardNotPassedIn) => true,
        Some(C::DriverNotRunning { secure_boot }) => *secure_boot,
        Some(
            C::ToolkitMissing
            | C::ToolkitOtherProvider { .. }
            | C::SpecNotWritten { .. }
            | C::ToolkitRetired { .. },
        ) => plan,
        Some(C::SpecDirNotPrivate { .. } | C::SpecFilesFailed { .. }) => true,
        _ => false,
    }
}

/// A read under way, shared by every failed start that joins it.
type Pending = Shared<Pin<Box<dyn Future<Output = Option<Arc<Blame>>> + Send>>>;

/// The failed starts' reads of §1.1 and §1.2 (module doc): one at a time.
#[derive(Default)]
pub(super) struct Reads {
    count: AtomicU64,
    pending: Mutex<Option<Pending>>,
}

impl Reads {
    /// How many reads began.
    pub(super) fn count(&self) -> u64 {
        self.count.load(Ordering::SeqCst)
    }

    /// The read under way, if one is.
    pub(super) fn under_way(&self) -> Option<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .filter(|p| p.peek().is_none())
    }

    /// What a start on the card that failed just now is blamed on: the
    /// read under way, joined, or a new one in a task of its own (so a
    /// caller that goes away leaves it to the others), which begins the
    /// full check in the background when it ends. `None` when the read
    /// could not finish.
    pub(super) async fn read(&self, state: &SharedState) -> Option<Arc<Blame>> {
        let pending = {
            let mut slot = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            match slot.clone().filter(|p| p.peek().is_none()) {
                Some(p) => p,
                None => {
                    self.count.fetch_add(1, Ordering::SeqCst);
                    let st = state.clone();
                    let task = tokio::spawn(async move {
                        let blame = now(&st).await;
                        super::check_in_background(&st);
                        blame
                    });
                    let p: Pending = async move { task.await.ok().flatten().map(Arc::new) }
                        .boxed()
                        .shared();
                    *slot = Some(p.clone());
                    p
                }
            }
        };
        pending.await
    }
}

/// Why NVML is not asked in a failed start's read: the last write of
/// lmgw's spec ran out of time on the card (`nvidia-ctk` waits on NVML, so
/// the probe would wait as long again); `None` otherwise.
fn card_stopped_answering(state: &SharedState) -> Option<String> {
    match state.cdi().last()?.outcome {
        Err(e @ CdiError::TimedOut { .. }) => Some(e.to_string()),
        _ => None,
    }
}

/// §1.1 and §1.2, read and judged now (module doc).
async fn now(state: &SharedState) -> Option<Blame> {
    let m = &state.machine;
    let skip = card_stopped_answering(state);
    let (card, toolkit) =
        match super::gather::for_blame(state, m.facts(), m.amd_reader(), skip.is_some()).await {
            Ok(read) => read,
            Err(why) => {
                tracing::warn!(
                    "a model's start failed, and reading the graphics card again did not \
                     finish ({why}); its start's own error stands"
                );
                return None;
            }
        };
    let render_nodes = !card.render_nodes.is_empty();
    let nvml_left_out = skip.is_some() && card.has(Vendor::Nvidia) && card.driver.nvml_decides();
    let card_item = if nvml_left_out {
        tracing::info!(
            "a model's start failed: NVML is not asked again now, since writing the graphics \
             card's description for it ran out of time on the card ({}); the check in the \
             background asks it",
            skip.as_deref().unwrap_or_default()
        );
        None
    } else {
        Some(super::container::judge_card(&card, state.deployment()).0)
    };
    let toolkit = toolkit.map(|t| super::container::judge_toolkit(&t, state.deployment()));
    Some(Blame::from_parts(
        card_item.as_ref(),
        toolkit.as_ref().map(|(i, ..)| i),
        toolkit.as_ref().is_some_and(|(_, plan, _)| plan.is_some()),
        render_nodes,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: MachineItemId, sentence: &str, cause: Option<MachineCause>) -> MachineItem {
        MachineItem {
            id,
            state: ItemState::Problem,
            sentence: sentence.into(),
            detail: Vec::new(),
            cause,
            action: None,
        }
    }

    fn render() -> Device {
        Device::Render
    }

    /// §1.1 by vendor, §1.2 only for the NVIDIA card's rows, the render
    /// nodes for a Vulkan row; no card is every card row's.
    #[test]
    fn each_row_is_blamed_only_on_what_it_uses() {
        let restart = item(
            MachineItemId::GraphicsCard,
            "Restart the computer to finish the graphics driver update.",
            Some(MachineCause::DriverMismatch {
                kernel: "600.10.01".into(),
                userspace: "600.20.03".into(),
            }),
        );
        let b = Blame::from_parts(Some(&restart), None, false, true);
        assert!(b.problem(&Device::Nvidia).is_some());
        assert_eq!(b.problem(&Device::Amd), None, "an NVIDIA cause");
        assert_eq!(b.problem(&render()), None, "render nodes are there");
        let b = Blame::from_parts(Some(&restart), None, false, false);
        assert!(b.problem(&render()).is_some(), "no render nodes");

        let amd = item(
            MachineItemId::GraphicsCard,
            "lmgw can't compute on this AMD card: its driver isn't giving models access to it.",
            Some(MachineCause::AmdNoComputeNode),
        );
        let b = Blame::from_parts(Some(&amd), None, false, true);
        assert!(b.problem(&Device::Amd).is_some());
        assert_eq!(b.problem(&Device::Nvidia), None);
        assert_eq!(b.problem(&render()), None);

        let none = item(
            MachineItemId::GraphicsCard,
            "No graphics card found. Cloud models work; local models need a graphics card.",
            Some(MachineCause::NoCard { others: vec![] }),
        );
        let b = Blame::from_parts(Some(&none), None, false, true);
        assert!(b.problem(&Device::Nvidia).is_some());
        assert!(b.problem(&Device::Amd).is_some());
        assert_eq!(b.problem(&render()), None, "an Intel card's render nodes");
        let b = Blame::from_parts(Some(&none), None, false, false);
        assert!(b.problem(&render()).is_some());
        assert_eq!(b.problem(&Device::None), None);
        assert_eq!(b.problem(&Device::OtherCdi(vec!["x.com/y".into()])), None);

        let toolkit = item(
            MachineItemId::CardForModels,
            "One piece is missing so models can use the graphics card. lmgw can set it up.",
            Some(MachineCause::ToolkitMissing),
        );
        let b = Blame::from_parts(None, Some(&toolkit), true, true);
        assert!(b.problem(&Device::Nvidia).is_some());
        assert_eq!(b.problem(&render()), None);
        assert_eq!(b.problem(&Device::Amd), None);

        let unfinished = item(MachineItemId::GraphicsCard, "lmgw couldn't finish.", None);
        let b = Blame::from_parts(Some(&unfinished), None, false, false);
        assert!(b.any());
        assert_eq!(b.problem(&Device::Nvidia), None, "nobody's");
        assert_eq!(b.problem(&render()), None);
    }

    /// The words: the computer lmgw runs on, the item's sentence, and what
    /// to do there only when there is something to see.
    #[test]
    fn the_words_point_to_lmgw_only_when_there_is_something_to_see() {
        let words = |sentence: &str, cause, plan| {
            Blamed::of(&item(MachineItemId::GraphicsCard, sentence, cause), plan).words()
        };
        assert_eq!(
            words(
                "Restart the computer to finish the graphics driver update.",
                Some(MachineCause::DriverMismatch {
                    kernel: "a".into(),
                    userspace: "b".into()
                }),
                false
            ),
            "could not start on the graphics card of the computer lmgw runs on. Restart the \
             computer to finish the graphics driver update."
        );
        assert_eq!(
            words(
                "One piece is missing so models can use the graphics card. lmgw can set it up.",
                Some(MachineCause::ToolkitMissing),
                true
            ),
            "could not start on the graphics card of the computer lmgw runs on. One piece is \
             missing so models can use the graphics card. lmgw can set it up. Open lmgw there \
             to see what to do."
        );
        assert_eq!(
            words(
                "No graphics card found. Cloud models work; local models need a graphics card.",
                Some(MachineCause::NoCard { others: vec![] }),
                false
            ),
            NO_CARD
        );
        for (cause, there) in [
            (MachineCause::DriverNotRunning { secure_boot: true }, true),
            (MachineCause::DriverNotRunning { secure_boot: false }, false),
            (MachineCause::AmdNoComputeNode, false),
            (MachineCause::SpecDirNotPrivate { error: "x".into() }, true),
            (MachineCause::SpecWriteTimedOut { error: "x".into() }, false),
            (MachineCause::NoNvidiaDriver { bound: None }, true),
            (MachineCause::DriverLibrariesMissing, true),
            (MachineCause::NvmlFailed { error: "x".into() }, false),
        ] {
            assert_eq!(
                words("S.", Some(cause.clone()), false).ends_with("what to do."),
                there,
                "{cause:?}"
            );
        }
    }

    /// The swap trusts only causes `Current` reads.
    #[test]
    fn the_swap_trusts_only_causes_current_reads() {
        let swaps =
            |cause| Blamed::of(&item(MachineItemId::GraphicsCard, "S.", Some(cause)), true).swaps();
        assert!(swaps(MachineCause::DriverMismatch {
            kernel: "a".into(),
            userspace: "b".into()
        }));
        assert!(swaps(MachineCause::ToolkitMissing));
        assert!(swaps(MachineCause::SpecNotWritten {
            error: "x".into(),
            owner: "y".into()
        }));
        for cause in [
            MachineCause::NoCard { others: vec![] },
            MachineCause::AmdNoComputeNode,
            MachineCause::SpecDirNotPrivate { error: "x".into() },
            MachineCause::SpecFilesFailed { error: "x".into() },
            MachineCause::SpecWriteTimedOut { error: "x".into() },
        ] {
            assert!(!swaps(cause.clone()), "{cause:?}");
        }
    }
}
