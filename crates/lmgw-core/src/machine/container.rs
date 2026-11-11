//! The check in the server image (headless server design §7): the same
//! reads, judged for the administrator of a server. Inside the image the
//! toolkit, the rpm database and the software sources are the image's, the
//! fix does not exist (ruling 1), and what is to be done is done on the
//! host: so the steps read "On the host: …", the plan is absent, and a
//! missing piece of the image means a broken image.
//!
//! - §1.1 ([`card`]): the card on the bus without its device nodes is a
//!   container started without the card; the host's driver not running,
//!   changed since the start, or passed in without its libraries is said as
//!   the host's to fix: a restart of the machine when the host's libraries
//!   are newer than its running module, else the host's CDI spec refreshed
//!   and the service restarted.
//! - §1.2: no plan, no button; a missing toolkit is an incomplete image.
//! - §1.3 ([`podman`]): Podman inside the lmgw container, rootful on the
//!   host or with its root mapped to a host uid, as `/proc/self/uid_map`
//!   shows it; no subuid hints for the inner root, and a map without a range
//!   says the service account needs one.
//! - §1.5 ([`room`]): each place named with the image's volume it lies on,
//!   as `/proc/self/mountinfo` shows it.
//! - §5: the metered probe is not run ([`METERED_NOT_CHECKED`]).
//!
//! The desktop never comes here: [`judge_card`] and [`judge_toolkit`] hand
//! the desktop to [`super::card`]'s and [`super::toolkit`]'s judges, and
//! [`super::running`] asks [`podman`] only in container mode.

use lmgw_api_types::machine::{ItemState, MachineCause, MachineItem, MachineItemId, MachinePlan};
use lmgw_api_types::RESTART_COMMAND;

use super::card::Card;
use super::gather::{CardFacts, ToolkitFacts};
use super::toolkit::{self, Toolkit12};
use super::words;

mod card;
pub(super) mod podman;
mod room;

pub(super) use podman::IdMaps;
pub(super) use room::{on_volume, read_mounts, Mounts};

/// §1.1's item for `deployment`: the server image's words, or the
/// desktop's.
pub(super) fn judge_card(
    c: &CardFacts,
    deployment: crate::config::deployment::Deployment,
) -> (MachineItem, Card) {
    if deployment.is_container() {
        card::card(c)
    } else {
        super::card::judge(c)
    }
}

/// §1.2's item for `deployment`: the server image's words without a plan,
/// or the desktop's.
pub(super) fn judge_toolkit(
    t: &ToolkitFacts,
    deployment: crate::config::deployment::Deployment,
) -> (MachineItem, Option<MachinePlan>, Toolkit12) {
    if deployment.is_container() {
        let (mut it, _plan, outcome) = toolkit::judge(t);
        let outcome = reword_toolkit(&mut it, outcome);
        (it, None, outcome)
    } else {
        toolkit::judge(t)
    }
}

/// §1.2's item while §1.1 is a problem: what comes first. In the server
/// image a container started without the card (NVIDIA's or AMD's) is the
/// card's to fix first, no driver's.
pub(super) fn card_first(card: &MachineItem, container: bool) -> MachineItem {
    let without_card = matches!(
        card.cause,
        Some(MachineCause::CardNotPassedIn | MachineCause::AmdNoComputeNode)
    );
    words::not_checked(
        MachineItemId::CardForModels,
        if container && without_card {
            "the graphics card comes first"
        } else {
            "the driver comes first"
        },
    )
}

/// The ready headline: the server's, or the computer's.
pub(super) fn ready(container: bool) -> String {
    let what = if container { "server" } else { "computer" };
    format!("This {what} is ready for local models.")
}

/// The NVIDIA control node every CDI spec of the card passes in: without
/// it, the container was started without the card.
pub(super) const NVIDIACTL: &str = "/dev/nvidiactl";

/// The image's volumes (§3.2), which the room item names each place by.
pub(super) const VOLUMES: [&str; 3] = ["/data", "/var/lib/containers", "/var/tmp"];

/// What [`super::gather`] calls the scratch volume in a room.
pub(super) const SCRATCH: &str = "builds' scratch and temporary files";

/// The scratch volume's mount (`TMPDIR`, D21).
pub(super) const SCRATCH_DIR: &str = "/var/tmp";

/// Why the toolkit has no setup in the image: the detail's "no button for
/// the setup" line.
pub(super) const NO_SETUP: &str =
    "in the server image the toolkit comes with the image, so lmgw sets nothing up";

/// Why whether the connection is metered is not known in the server image
/// (§7), as the log's "is not known (…)" lines give the reason: the image
/// has no NetworkManager, and the container's view of the network is not
/// the host's.
pub const METERED_NOT_CHECKED: &str = "lmgw does not check it inside a container";

/// The step every item about a broken image ends with.
fn load_image() -> String {
    format!(
        "On the host: load a current lmgw server image (podman load -i \
         lmgw-server-<version>.oci.tar, then podman tag it as localhost/lmgw-server:current) and \
         restart the service ({RESTART_COMMAND})"
    )
}

/// The step after a change of the host's driver or spec.
fn refresh_spec() -> String {
    format!(
        "On the host: refresh the CDI spec (nvidia-ctk cdi generate \
         --output=/etc/cdi/nvidia.yaml, or the toolkit's nvidia-cdi-refresh units), then restart \
         the service ({RESTART_COMMAND})"
    )
}

/// The step when the host itself must restart: a new driver to load, or a
/// card that stopped answering.
fn restart_host() -> String {
    "On the host: restart the machine (sudo systemctl reboot); the lmgw service starts again \
     with it, and if the CDI spec is not refreshed at boot by the toolkit's nvidia-cdi-refresh \
     units, refresh it first (nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml)"
        .into()
}

/// The step when a fresh lmgw container is what helps.
fn restart_service() -> String {
    format!("On the host: restart the service ({RESTART_COMMAND}), which recreates the container")
}

// ---------------------------------------------------------------------------
// §1.2 Using the card for models
// ---------------------------------------------------------------------------

/// §1.2's item in the server image, reworded from the desktop's: never a
/// plan (the fix does not exist, ruling 1), and what the desktop offers to
/// set up is the image's to bring.
fn reword_toolkit(it: &mut MachineItem, outcome: Toolkit12) -> Toolkit12 {
    match it.cause.clone() {
        Some(MachineCause::ToolkitMissing | MachineCause::ToolkitOtherProvider { .. }) => {
            it.state = ItemState::Problem;
            it.sentence = "One piece is missing so models can use the graphics card: this \
                           server image is incomplete. On the host: load a current lmgw server \
                           image and restart the service."
                .into();
            it.detail.push(load_image());
            Toolkit12::Steps
        }
        Some(MachineCause::SpecNotWritten { .. }) => spec_not_written(it),
        Some(MachineCause::ToolkitRetired { .. }) if it.state == ItemState::Problem => {
            spec_not_written(it)
        }
        Some(MachineCause::ToolkitRetired { .. }) => {
            it.sentence = "Models can use the graphics card. One piece comes from a package \
                           Fedora no longer updates; a current lmgw server image replaces it."
                .into();
            it.detail.push(load_image());
            Toolkit12::Ok
        }
        Some(MachineCause::SpecWriteTimedOut { .. }) => {
            it.sentence = "Your graphics card stopped answering while lmgw prepared it for \
                           models. On the host: restart the machine; a restart of the lmgw \
                           service alone does not bring back a card that stopped answering."
                .into();
            it.detail.push(restart_host());
            Toolkit12::Restart
        }
        Some(MachineCause::SpecDirNotPrivate { .. }) => {
            it.sentence = "lmgw keeps a file about the graphics card in a folder only it may \
                           open, and that folder inside the lmgw container is open to others. On \
                           the host: restart the lmgw service, which gives the container a fresh \
                           one."
                .into();
            it.detail.push(restart_service());
            Toolkit12::Steps
        }
        Some(MachineCause::SpecFilesFailed { .. }) => {
            it.sentence = "lmgw couldn't save a file about the graphics card inside the lmgw \
                           container. The details say why. On the host: restart the lmgw \
                           service; if this stays, load a current lmgw server image."
                .into();
            it.detail.push(restart_service());
            it.detail.push(load_image());
            Toolkit12::Steps
        }
        // Nothing here has a button: what the desktop fixes behind one is
        // steps.
        _ if outcome == Toolkit12::Fixable => Toolkit12::Steps,
        _ => outcome,
    }
}

/// A write of lmgw's spec the toolkit failed (§1.2), in the image.
fn spec_not_written(it: &mut MachineItem) -> Toolkit12 {
    it.state = ItemState::Problem;
    it.sentence = "lmgw couldn't write the description of your graphics card that models use. \
                   On the host: check that the host's CDI spec is current and restart the lmgw \
                   service; if this stays, load a current lmgw server image."
        .into();
    it.detail.push(refresh_spec());
    it.detail.push(load_image());
    Toolkit12::Steps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toolkit_item(cause: MachineCause) -> MachineItem {
        words::item(
            MachineItemId::CardForModels,
            ItemState::Problem,
            "the desktop's sentence",
            vec!["the last write failed: …".into()],
            Some(cause),
        )
    }

    /// The spec writes that fail inside the container: a fresh container
    /// for a folder or a file, the machine for a card that stopped
    /// answering; never the desktop's sentence.
    #[test]
    fn a_failed_write_is_the_hosts_to_act_on() {
        let mut it = toolkit_item(MachineCause::SpecFilesFailed {
            error: "No space left on device".into(),
        });
        assert_eq!(reword_toolkit(&mut it, Toolkit12::Steps), Toolkit12::Steps);
        assert_eq!(
            it.sentence,
            "lmgw couldn't save a file about the graphics card inside the lmgw container. The \
             details say why. On the host: restart the lmgw service; if this stays, load a \
             current lmgw server image."
        );
        assert!(it.detail.iter().any(|l| l.contains(RESTART_COMMAND)));

        let mut it = toolkit_item(MachineCause::SpecDirNotPrivate {
            error: "/run/user/0/lmgw-machine is mode 777".into(),
        });
        assert_eq!(reword_toolkit(&mut it, Toolkit12::Steps), Toolkit12::Steps);
        assert!(it
            .sentence
            .contains("inside the lmgw container is open to others"));
        assert!(it
            .sentence
            .contains("On the host: restart the lmgw service"));

        let mut it = toolkit_item(MachineCause::SpecWriteTimedOut {
            error: "timed out".into(),
        });
        assert_eq!(
            reword_toolkit(&mut it, Toolkit12::Restart),
            Toolkit12::Restart
        );
        assert_eq!(
            it.sentence,
            "Your graphics card stopped answering while lmgw prepared it for models. On the \
             host: restart the machine; a restart of the lmgw service alone does not bring back \
             a card that stopped answering."
        );
        assert!(it.detail.iter().any(|l| l.contains("restart the machine")));
    }

    #[test]
    fn a_container_without_the_card_has_the_card_first() {
        let item = |cause| {
            words::item(
                MachineItemId::GraphicsCard,
                ItemState::Problem,
                "",
                Vec::new(),
                Some(cause),
            )
        };
        for cause in [
            MachineCause::CardNotPassedIn,
            MachineCause::AmdNoComputeNode,
        ] {
            assert_eq!(
                card_first(&item(cause.clone()), true).sentence,
                "Not checked yet: the graphics card comes first."
            );
        }
        assert_eq!(
            card_first(&item(MachineCause::AmdNoComputeNode), false).sentence,
            "Not checked yet: the driver comes first.",
            "the desktop keeps its words"
        );
        assert_eq!(
            card_first(&item(MachineCause::DriverLibrariesMissing), true).sentence,
            "Not checked yet: the driver comes first."
        );
    }
}
